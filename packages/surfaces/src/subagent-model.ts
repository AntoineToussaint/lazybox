import type { JournalEvent } from '@lazybox/protocol';
import type { ToolCall } from './tool-activity';
import { applyToolPayload, groupTools, type ToolEntry, type ToolItem } from './tool-lines';
import { toolLabel } from './live-status';
import { stoppedEntry, stopText } from './tool-stops';

/**
 * Sub-agents a conversation's agent spawned, folded from its journal: who
 * they are, what they were asked, what they are doing now, their own
 * messages and tool calls, and what they handed back.
 *
 * Attribution is the providers': the Claude worker names a sub-agent after
 * the tool call that spawned it (`claude-tool:<tool_use_id>`) and tags its
 * messages and tool calls with that id; Codex sub-threads are `codex:<thread>`.
 * Nothing here is inferred from text, but Claude's fixed words for a call it
 * did not run because it was stopped (tool-stops.ts).
 */
/**
 * `unreported`: it cannot still be running (the turn it belonged to ended)
 * but no end was ever reported: no outcome to show, and no clock running.
 */
export type SubagentStatus = 'starting' | 'running' | 'waiting' | 'done' | 'failed' | 'stopped' | 'unknown' | 'unreported';
export const activeSubagent = (status: SubagentStatus) => status === 'starting' || status === 'running' || status === 'waiting';

export type SubagentItem =
  | { kind: 'message'; id: string; text: string; itemId?: string }
  | { kind: 'tool'; id: string; call: ToolCall }
  /** A sub-agent this one spawned, at the point it spawned it. */
  | { kind: 'agent'; id: string; agentId: string };

export type Subagent = {
  id: string;
  /** The id the Runner knows it by (the rotation layer prefixes the provider's): what stopping it names. */
  runnerId: string;
  /** 'root' for one the main agent spawned. */
  parentId: string;
  depth: number;
  /** The provider's task id: what stopping it addresses. */
  nativeId: string;
  /** The transcript message its card sits at; '' when it sits in its parent's timeline. */
  anchor: string;
  /** The one-line task ("Find the flaky test"). */
  name: string;
  /** Its kind of agent ("Explore", "general-purpose"), when the provider says. */
  type: string;
  /** The full instructions it was given. */
  prompt: string;
  toolUseId: string;
  status: SubagentStatus;
  error: string;
  startedAt?: number;
  endedAt?: number;
  /** Its last own event: an end nobody reported is put here, never later. */
  lastAt?: number;
  /** The person asked the Runner to stop it (an accepted stopAgent). */
  stopRequested?: boolean;
  /** Its own step: working, thinking, responding, tool… and since when. */
  phase: string;
  phaseTool: string;
  phaseSince?: number;
  /** Tool calls observed in its timeline. */
  toolCount: number;
  /** Tool calls the provider counted (includes any not forwarded). */
  reportedTools: number;
  tokens?: number;
  /** The provider's own measure of how long it ran, once known. */
  durationMs?: number;
  /** The provider's one-line progress summary while it runs. */
  progress: string;
  /** What it handed back to its parent. */
  result: string;
  /** The provider's short summary of how it ended. */
  summary: string;
  backgrounded: boolean;
  /**
   * 'agent' for a sub-agent (Task/Agent, a Codex sub-thread); 'command' for a
   * background shell command the agent started (Claude's run_in_background):
   * it has no card in the conversation and is named by `commandTitle`.
   */
  kind: 'agent' | 'command';
  /** A background command's full script. */
  command: string;
  /** What the agent said a background command does ("Generate the protobuf code"), when it said. */
  description?: string;
  /** The latest output the agent read of a background command. */
  output: string;
  /** Where the provider keeps a background task's output. */
  outputFile: string;
  items: SubagentItem[];
  /** Older timeline items no longer retained. */
  dropped: number;
  children: string[];
  /** The id of the tool call it is running now. */
  runningTool: string;
};

export type Subagents = {
  byId: Readonly<Record<string, Subagent>>;
  /** In the order they started. */
  order: readonly string[];
  /** Spawning tool call id → sub-agent id. */
  spawns: Readonly<Record<string, string>>;
  /** Another id the provider used for a sub-agent already known by its spawning call. */
  aliases: Readonly<Record<string, string>>;
  /** Background shell calls (`run_in_background`) by tool call id: what a background command runs, and what the agent said it does. */
  commandCalls?: Readonly<Record<string, CommandCall>>;
  /** Skill calls by tool call id → the skill: names the run a forked one starts, which the provider may not name. */
  skillCalls?: Readonly<Record<string, string>>;
  /** Managed calls that older workers incorrectly emitted as a forked run. */
  managedCalls?: ReadonlySet<string>;
  truncated: boolean;
};
export type CommandCall = { command: string; description: string };
export const emptySubagents = (): Subagents => ({ byId: {}, order: [], spawns: {}, aliases: {}, commandCalls: {}, skillCalls: {}, truncated: false });
/** Background shell calls remembered for naming their commands: the newest. */
const COMMAND_CALLS = 256;
/** Skill calls remembered for naming a forked run: the newest. */
const SKILL_CALLS = 64;
/** A background command's description is kept whole, up to this. */
const DESCRIPTION = 400;

/**
 * What the spawning call says of a sub-agent. Claude's Task/Agent input is
 * `{description, prompt, subagent_type, name?, run_in_background?}`.
 */
function adoptInput(agent: Subagent, input: Record<string, any>) {
  agent.name ||= [text(input.description), text(input.name)].find(name => name.trim() && !genericName(name))?.trim() || '';
  agent.type ||= text(input.subagent_type);
  agent.prompt ||= text(input.prompt);
  agent.backgrounded ||= input.run_in_background === true;
}

/** Kinds of agent that tell a reader nothing ("general-purpose"): no badge for them. */
const GENERIC_TYPES = new Set(['', 'general-purpose', 'general', 'agent', 'local_agent', 'sub-agent', 'subagent', 'default', 'task']);
/** Names a provider or the Runner gives an agent it knows nothing about. */
const GENERIC_NAMES = new Set(['', 'agent', 'sub-agent', 'subagent', 'sub agent', 'task']);
export const genericType = (type: string) => GENERIC_TYPES.has(type.trim().toLowerCase());
export const genericName = (name: string) => GENERIC_NAMES.has(name.trim().toLowerCase());
const MANAGED_TOOLS = new Set(['mcp__lazybox__command', 'lazybox_command']);
const SPAWN_TOOLS = new Set(['Task', 'Agent']);
/** Tools the agent reads a background command's output with. */
const READ_TOOLS = new Set(['BashOutput', 'TaskOutput']);
/** Provider task types that are background commands, not sub-agents. */
const COMMAND_TASKS = new Set(['local_bash', 'shell', 'bash']);
/** A sub-agent's own events that change its card or timeline. */
const CHILD_TYPES = new Set(['tool_activity', 'message_delta', 'activity', 'approval', 'question', 'request_resolved', 'turn_started', 'turn_ended', 'failure']);
/** Sub-agents and background commands a conversation keeps; past it, the oldest finished ones make room. */
export const SUBAGENT_LIMIT = 256;
const MAX_AGENTS = SUBAGENT_LIMIT, MAX_ITEMS = 400, MAX_MESSAGE = 100_000, MAX_RESULT = 64_000, MAX_PAYLOAD = 8_000;

const decoder = new TextDecoder();
function payload(bytes: Uint8Array): Record<string, any> | undefined {
  if (!bytes.length) return;
  try { const value = JSON.parse(decoder.decode(bytes)); return value && typeof value === 'object' && !Array.isArray(value) ? value : undefined; } catch { return; }
}
/** The head of a payload, to recognise a spawning call without parsing a large result. */
const peek = (bytes: Uint8Array, size = 384) => decoder.decode(bytes.subarray(0, size));
const time = (capturedAt: string) => { const at = Date.parse(capturedAt); return Number.isFinite(at) ? at : undefined; };
const text = (value: unknown) => typeof value === 'string' ? value : '';
const number = (value: unknown) => typeof value === 'number' && Number.isFinite(value) && value >= 0 ? value : undefined;

/** Keeps a display preview; the Runner journal keeps the original. */
function bounded(value: unknown): unknown {
  if (value === undefined || value === null) return value ?? undefined;
  if (typeof value === 'string') return value.length > MAX_PAYLOAD ? value.slice(0, MAX_PAYLOAD) + '\n[Preview truncated]' : value;
  const json = JSON.stringify(value);
  return json && json.length > MAX_PAYLOAD * 2 ? json.slice(0, MAX_PAYLOAD * 2) + '\n[Preview truncated]' : value;
}

/** A tool result as text: Claude sends a string or text blocks. */
export function resultText(value: unknown): string {
  if (typeof value === 'string') return value;
  if (Array.isArray(value)) return value.map(part => typeof part === 'string' ? part : part && typeof part === 'object' && typeof part.text === 'string' ? part.text : '').filter(Boolean).join('\n\n');
  if (value && typeof value === 'object' && 'content' in value) return resultText((value as { content: unknown }).content);
  return value === undefined || value === null ? '' : JSON.stringify(value, null, 2);
}
// Claude's sub-agent result ends with a resume handle for the model, not the
// reader; a forked Skill's starts with a line saying it was one.
const cleanResult = (value: string) => value.replace(/\n*agentId: [\w-]+[^\n]*\s*$/, '').replace(/^Skill "[^"\n]*" completed \(forked execution\)\.\s*(?:Result:\s*)?/, '').trim().slice(0, MAX_RESULT);

function mapStatus(status: string, current: SubagentStatus): SubagentStatus | undefined {
  switch (status) {
    case 'starting': return 'starting';
    case 'running': return 'running';
    case 'waiting_input': case 'waiting_approval': return 'waiting';
    case 'done': case 'completed': return 'done';
    case 'failed': return 'failed';
    case 'stopped': case 'interrupted': return 'stopped';
    // An idle sub-thread that worked has finished.
    case 'ready': return current === 'running' || current === 'waiting' ? 'done' : undefined;
    case 'unknown': return activeSubagent(current) ? 'unknown' : undefined;
    // The view's word for one the Runner said was not running when asked to stop it.
    case 'unreported': return activeSubagent(current) ? 'unreported' : undefined;
  }
  return;
}

/**
 * Folds one batch of records into the sub-agent index. Like the transcript
 * fold, the input state is never mutated: objects created in this batch are
 * the only ones extended in place, so a card re-renders only when its own
 * sub-agent changed.
 */
export class SubagentFold {
  private byId?: Record<string, Subagent>;
  private order?: string[];
  private spawns?: Record<string, string>;
  private aliases?: Record<string, string>;
  private commandCalls?: Record<string, CommandCall>;
  private skillCalls?: Record<string, string>;
  private managedCalls?: Set<string>;
  private truncated: boolean;
  private owned = new Set<object>();
  private cards: string[] = [];
  constructor(private state: Subagents) { this.truncated = state.truncated; }

  get(id: string): Subagent | undefined { return (this.byId ?? this.state.byId)[id]; }
  private spawnOf(callId: string) { return (this.spawns ?? this.state.spawns)[callId]; }
  /**
   * The sub-agent an event's id names. A card created from its spawning call
   * is keyed by that call; the Runner may know it under a prefixed id
   * (`attempt-2:claude-tool:<call>`), which becomes an alias of the card.
   */
  private resolve(id: string) {
    const known = (this.aliases ?? this.state.aliases)[id];
    if (known) return known;
    const call = /(?:^|:)claude-tool:(.+)$/.exec(id)?.[1];
    const spawned = call ? this.spawnOf(call) : undefined;
    if (spawned && spawned !== id && this.get(spawned)) {
      (this.aliases ??= { ...this.state.aliases })[id] = spawned;
      const agent = this.edit(spawned); agent.runnerId = id;
      return spawned;
    }
    return id;
  }
  /** Top-level sub-agents that started in the records folded so far; each gets a card there. */
  takeCards(): string[] { const cards = this.cards; this.cards = []; return cards; }

  private edit(id: string): Subagent {
    const map = this.byId ??= { ...this.state.byId };
    let agent = map[id];
    if (!this.owned.has(agent)) { agent = { ...agent }; map[id] = agent; this.owned.add(agent); }
    return agent;
  }
  private items(agent: Subagent): SubagentItem[] {
    if (!this.owned.has(agent.items)) { agent.items = [...agent.items]; this.owned.add(agent.items); }
    return agent.items;
  }
  private push(agent: Subagent, item: SubagentItem) {
    const items = this.items(agent);
    items.push(item);
    if (items.length > MAX_ITEMS) {
      // A nested sub-agent's place in the timeline is kept; old tool calls and prose go first.
      const index = items.findIndex(entry => entry.kind !== 'agent');
      if (index >= 0) { items.splice(index, 1); agent.dropped++; }
    }
  }

  private create(id: string, parentId: string, at: number | undefined, kind: 'agent' | 'command' = 'agent'): Subagent | undefined {
    const existing = this.get(id);
    if (existing) return existing;
    const count = (this.order ?? this.state.order).length;
    if (count >= MAX_AGENTS && !this.evict()) { this.truncated = true; return; }
    const parent = parentId && parentId !== 'root' ? this.get(this.resolve(parentId)) : undefined;
    const agent: Subagent = {
      id, runnerId: id, parentId: parent ? parent.id : 'root', depth: parent ? parent.depth + 1 : 0, nativeId: '', anchor: parent ? '' : `subagent:${id}`,
      name: '', type: '', prompt: '', toolUseId: '', status: 'starting', error: '', startedAt: at, phase: 'starting', phaseTool: '', phaseSince: at,
      toolCount: 0, reportedTools: 0, progress: '', result: '', summary: '', backgrounded: false, items: [], dropped: 0, children: [], runningTool: '',
      kind, command: '', output: '', outputFile: '',
    };
    if (kind === 'command') agent.anchor = '';
    (this.byId ??= { ...this.state.byId })[id] = agent; this.owned.add(agent); this.owned.add(agent.items); this.owned.add(agent.children);
    (this.order ??= [...this.state.order]).push(id);
    if (kind === 'command') return agent;
    if (parent) {
      const owner = this.edit(parent.id);
      if (!this.owned.has(owner.children)) { owner.children = [...owner.children]; this.owned.add(owner.children); }
      owner.children.push(id);
      this.push(owner, { kind: 'agent', id: `agent:${id}`, agentId: id });
    } else this.cards.push(id);
    return agent;
  }

  /**
   * Makes room for a new sub-agent or command past the limit: the oldest
   * finished one with nothing kept under it goes, from the index and from
   * its parent's timeline (`truncated` says the list is no longer whole).
   * False when everything kept is still at work: the new one is dropped.
   */
  private evict(): boolean {
    const order = this.order ?? this.state.order;
    const index = order.findIndex(id => { const agent = this.get(id); return !!agent && !activeSubagent(agent.status) && !agent.children.length; });
    if (index < 0) return false;
    const id = order[index], agent = this.get(id)!;
    (this.order ??= [...this.state.order]).splice(index, 1);
    delete (this.byId ??= { ...this.state.byId })[id];
    const spawns = this.spawns ?? this.state.spawns, aliases = this.aliases ?? this.state.aliases;
    for (const [call, target] of Object.entries(spawns)) if (target === id) delete (this.spawns ??= { ...this.state.spawns })[call];
    for (const [alias, target] of Object.entries(aliases)) if (target === id) delete (this.aliases ??= { ...this.state.aliases })[alias];
    if (agent.parentId !== 'root' && this.get(agent.parentId)) {
      const owner = this.edit(agent.parentId);
      owner.children = owner.children.filter(child => child !== id); this.owned.add(owner.children);
      owner.items = owner.items.filter(item => !(item.kind === 'agent' && item.agentId === id)); this.owned.add(owner.items);
    }
    this.truncated = true;
    return true;
  }

  private phase(agent: Subagent, phase: string, tool: string, at: number | undefined) {
    if (agent.phase !== phase || agent.phaseTool !== tool) { agent.phase = phase; agent.phaseTool = tool; agent.phaseSince = at; }
  }

  /** Its own event at `at`: the latest it is known to have done anything. */
  private touch(agent: Subagent, at: number | undefined) {
    if (at !== undefined && (agent.lastAt === undefined || at > agent.lastAt)) agent.lastAt = at;
  }

  private settle(agent: Subagent, status: SubagentStatus, at: number | undefined) {
    agent.status = status;
    if (activeSubagent(status)) { agent.endedAt = undefined; return; }
    agent.endedAt ??= at;
    agent.phase = ''; agent.phaseTool = ''; agent.runningTool = '';
    // A missing result is never inferred to be success.
    const items = agent.items;
    if (items.some(item => item.kind === 'tool' && item.call.status === 'Running')) {
      const owned = this.items(agent);
      for (let i = 0; i < owned.length; i++) {
        const item = owned[i];
        if (item.kind === 'tool' && item.call.status === 'Running') owned[i] = { ...item, call: { ...item.call, status: 'Result not reported' } };
      }
    }
  }

  /** The agent read a background command's output: keep the latest read with the command. */
  private readCall(data: Record<string, any>, bytes: Uint8Array) {
    const known = this.spawnOf(data.id);
    if (!known) {
      const input = data.input && typeof data.input === 'object' ? data.input : {};
      const target = text(input.bash_id) || text(input.task_id) || text(input.shell_id);
      if (!target || data.status !== 'running') return;
      const order = this.order ?? this.state.order;
      for (let i = order.length - 1; i >= 0; i--) {
        const agent = this.get(order[i]);
        if (agent?.kind === 'command' && agent.nativeId === target) { (this.spawns ??= { ...this.state.spawns })[data.id] = agent.id; return; }
      }
      return;
    }
    if (data.status !== 'completed') return;
    const output = resultText(data.output !== undefined ? data.output : payload(bytes)?.output);
    if (output) this.edit(known).output = output.length > 16_000 ? '[Earlier output not shown]\n' + output.slice(-16_000) : output;
  }

  /** The main agent (or a sub-agent) called a tool that spawns one. */
  private spawnCall(owner: string, data: Record<string, any>, at: number | undefined, bytes: Uint8Array) {
    if (data.type !== 'tool_call' || typeof data.id !== 'string') return;
    const known = this.spawnOf(data.id);
    if (READ_TOOLS.has(data.name) || (known && this.get(known)?.kind === 'command')) { this.readCall(data, bytes); return; }
    if (!known) {
      if (data.status !== 'running' || !SPAWN_TOOLS.has(data.name)) return;
      const id = `claude-tool:${data.id}`;
      // Its own events may come first, under the Runner's attempt prefix: that card is this one.
      const heard = this.get(id) ? id : (this.order ?? this.state.order).find(known => known.endsWith(`:${id}`) && this.get(known)?.kind === 'agent');
      const created = heard ? this.get(heard) : this.create(this.resolve(id), owner, at);
      if (!created) return;
      (this.spawns ??= { ...this.state.spawns })[data.id] = created.id;
      const agent = this.edit(created.id), input = data.input && typeof data.input === 'object' ? data.input : {};
      agent.toolUseId = data.id;
      adoptInput(agent, input);
      return;
    }
    // A later report of the same call may be the first to carry its input.
    if (data.status === 'running') {
      const input = data.input && typeof data.input === 'object' ? data.input : undefined;
      const current = this.get(known);
      if (input && current && (!current.name || !current.type || !current.prompt)) adoptInput(this.edit(known), input);
      return;
    }
    if (data.status !== 'completed' && data.status !== 'failed') return;
    const current = this.get(known);
    if (!current) return;
    const agent = this.edit(known);
    // A background sub-agent's call returns at once; its result comes when it settles.
    if (agent.backgrounded) return;
    this.touch(agent, at);
    const output = data.output !== undefined ? data.output : payload(bytes)?.output;
    const text = resultText(output);
    // Its call was stopped (an interrupt, Send now): it was stopped, with all it started; nothing failed.
    if (data.status === 'failed' && stopText(text)) { this.stopTree(agent.id, at); return; }
    const result = cleanResult(text);
    if (result) agent.result = result;
    if (activeSubagent(agent.status)) this.settle(agent, data.status === 'failed' ? 'failed' : 'done', at);
    if (data.status === 'failed' && !agent.error) agent.error = result.split('\n')[0].slice(0, 300);
  }

  /** A sub-agent stopped under its call, and every one it started that still runs, each at its last event. */
  private stopTree(id: string, at: number | undefined, depth = 0) {
    const agent = this.get(id);
    if (!agent || depth > 16) return;
    if (activeSubagent(agent.status)) { const owned = this.edit(id); this.settle(owned, 'stopped', depth ? owned.lastAt ?? at : at); }
    for (const child of agent.children) { const nested = this.get(child); if (nested && !nested.backgrounded) this.stopTree(child, at, depth + 1); }
  }

  record(record: JournalEvent): void {
    const e = record.event;
    if (!e) return;
    // Legacy journals may include synthetic children for an interactive MCP
    // command. Keep the parent tool call and ignore its duplicate child.
    if (e.type === 'tool_activity' && /"name"\s*:\s*"(?:mcp__lazybox__command|lazybox_command)"/.test(peek(e.providerDataJson))) {
      const call = payload(e.providerDataJson);
      if (call && MANAGED_TOOLS.has(call.name) && typeof call.id === 'string') {
        const known = this.managedCalls ??= new Set(this.state.managedCalls);
        known.add(call.id);
        if (known.size > COMMAND_CALLS) known.delete(known.values().next().value!);
      }
    }
    const childCall = /(?:^|:)claude-tool:(.+)$/.exec(e.agentId)?.[1];
    if (MANAGED_TOOLS.has(e.agentKind) || (childCall && (this.managedCalls ?? this.state.managedCalls)?.has(childCall))) return;
    const at = time(record.capturedAt);
    if (!e.agentId || e.agentId === 'root') {
      switch (e.type) {
        case 'tool_activity': {
          // Recognise a spawn from the head of the payload: results can be large.
          const head = peek(e.providerDataJson);
          const pending = (this.spawns ?? this.state.spawns);
          let data: Record<string, any> | undefined;
          if (/"name"\s*:\s*"(Task|Agent|BashOutput|TaskOutput)"/.test(head)) data = payload(e.providerDataJson);
          else if (/"name"\s*:\s*"Bash"/.test(head) && /"status"\s*:\s*"running"/.test(head)) {
            // A shell call's input names the background command it may start.
            const call = payload(e.providerDataJson);
            if (call) this.commandCall(call);
            return;
          } else if (/"name"\s*:\s*"Skill"/.test(head) && /"status"\s*:\s*"running"/.test(head)) {
            // A skill's call names the run it may fork.
            const call = payload(e.providerDataJson);
            if (call) this.skillCall(call);
            return;
          } else {
            const id = /"id"\s*:\s*"([^"]+)"/.exec(head)?.[1];
            if (id && pending[id]) data = payload(e.providerDataJson);
          }
          if (data) this.spawnCall('root', data, at, e.providerDataJson);
          return;
        }
        case 'session_closed': case 'profile_switching': this.endAll('stopped', at); return;
        case 'process_exit': this.endAll('unknown', at); return;
        case 'turn_ended': this.endTurn(at); return;
      }
      return;
    }
    const id = this.resolve(e.agentId);
    if (e.type === 'agent_status') { this.status(id, e, at); return; }
    // The only operation that names a sub-agent: the person asked to stop it.
    if (e.type === 'operation_accepted') { if (this.get(id)) this.edit(id).stopRequested = true; return; }
    if (!CHILD_TYPES.has(e.type)) return;
    if (!this.get(id)) {
      if (!this.create(id, e.parentAgentId || 'root', at)) return;
      this.adoptCall(id);
    }
    let agent = this.edit(id);
    this.touch(agent, at);
    switch (e.type) {
      case 'tool_activity': {
        const data = payload(e.providerDataJson);
        if (!data || data.type !== 'tool_call' || typeof data.id !== 'string') return;
        if (data.name === 'Skill' && data.status === 'running') this.skillCall(data);
        if (SPAWN_TOOLS.has(data.name) || READ_TOOLS.has(data.name) || this.spawnOf(data.id)) { this.spawnCall(id, data, at, e.providerDataJson); if (!READ_TOOLS.has(data.name) && !(this.spawnOf(data.id) && this.get(this.spawnOf(data.id)!)?.kind === 'command')) return; }
        agent = this.edit(id);
        if (agent.status === 'starting') agent.status = 'running';
        const status = ({ running: 'Running', completed: 'Completed', failed: 'Failed' } as Record<string, string>)[data.status] || 'Result not reported';
        const items = agent.items;
        let index = -1;
        for (let i = items.length - 1; i >= 0 && i >= items.length - 64; i--) { const item = items[i]; if (item.kind === 'tool' && item.call.id === data.id) { index = i; break; } }
        if (index < 0) {
          const call: ToolCall = { id: data.id, agent: id, name: text(data.name) || 'Tool', status, input: bounded(data.input), output: bounded(data.output), startedAt: status === 'Running' ? at : undefined, endedAt: status === 'Running' ? undefined : at };
          this.push(agent, { kind: 'tool', id: `tool:${data.id}`, call });
          agent.toolCount++;
          if (status === 'Running') { agent.runningTool = data.id; this.phase(agent, 'tool', call.name, at); }
        } else {
          const owned = this.items(agent), item = owned[index] as Extract<SubagentItem, { kind: 'tool' }>;
          const old = item.call;
          // A result already recorded is not undone by a late start.
          const next = ['Completed', 'Failed'].includes(old.status) && status === 'Running' ? old.status : status;
          owned[index] = { ...item, call: { ...old, name: text(data.name) || old.name, status: next, input: data.input !== undefined ? bounded(data.input) : old.input, output: data.output !== undefined ? bounded(data.output) : old.output, endedAt: old.endedAt ?? (next === 'Running' ? undefined : at) } };
          if (next !== 'Running' && agent.runningTool === data.id) { agent.runningTool = ''; this.phase(agent, 'working', '', at); }
        }
        return;
      }
      case 'message_delta': {
        if (!e.text) return;
        if (agent.status === 'starting') agent.status = 'running';
        this.phase(agent, 'responding', '', at);
        const items = agent.items, last = items.at(-1);
        const itemId = text(payload(e.providerDataJson)?.itemId) || undefined;
        if (last?.kind === 'message' && last.itemId === itemId) {
          if (last.text.length >= MAX_MESSAGE) return;
          const owned = this.items(agent);
          owned[owned.length - 1] = { ...last, text: last.text + e.text };
        } else this.push(agent, { kind: 'message', id: `message:${record.cursor}`, text: e.text, ...(itemId ? { itemId } : {}) });
        return;
      }
      case 'activity':
        if (!activeSubagent(agent.status)) return;
        if (agent.status === 'starting') agent.status = 'running';
        if (!agent.runningTool || e.status !== 'tool') this.phase(agent, e.status || 'working', e.status === 'tool' ? e.name : '', at);
        return;
      case 'approval': case 'question':
        agent.status = 'waiting'; return;
      case 'request_resolved':
        if (agent.status === 'waiting') agent.status = 'running';
        return;
      case 'turn_started':
        if (agent.status !== 'waiting') { agent.status = 'running'; agent.endedAt = undefined; }
        return;
      case 'turn_ended': {
        const status = e.status === 'failed' ? 'failed' : e.status === 'interrupted' ? 'stopped' : 'done';
        this.settle(agent, status, at);
        return;
      }
      case 'failure':
        agent.error = e.text || e.status || 'Failed';
        this.settle(agent, 'failed', at);
        return;
    }
  }

  /**
   * A shell call that runs in the background: its command and description,
   * for naming the background command it starts (whose own report names it
   * by the description alone, or by the whole script).
   */
  private commandCall(data: Record<string, any>) {
    const input = data.input && typeof data.input === 'object' ? data.input : undefined;
    if (data.type !== 'tool_call' || typeof data.id !== 'string' || input?.run_in_background !== true || !text(input.command).trim()) return;
    const calls = this.commandCalls ??= { ...(this.state.commandCalls ?? {}) };
    calls[data.id] = { command: text(input.command), description: oneLine(text(input.description), DESCRIPTION) };
    const ids = Object.keys(calls);
    for (const old of ids.slice(0, Math.max(0, ids.length - COMMAND_CALLS))) delete calls[old];
    const known = this.spawnOf(data.id), agent = known ? this.get(known) : undefined;
    if (agent?.kind === 'command') this.nameCommand(this.edit(known!), agent.description || agent.command);
  }

  /** A Skill call: the skill names the run it forks, if it forks one. */
  private skillCall(data: Record<string, any>) {
    const skill = data.input && typeof data.input === 'object' ? text(data.input.skill).trim() : '';
    if (data.type !== 'tool_call' || typeof data.id !== 'string' || !skill) return;
    const calls = this.skillCalls ??= { ...(this.state.skillCalls ?? {}) };
    calls[data.id] = skill;
    const ids = Object.keys(calls);
    for (const old of ids.slice(0, Math.max(0, ids.length - SKILL_CALLS))) delete calls[old];
  }

  /**
   * A sub-agent first heard of by its own events, under the call that runs it
   * (`claude-tool:<call>`): that call's result is its end (a forked Skill
   * reports no other), and a Skill call names it.
   */
  private adoptCall(id: string) {
    const call = /(?:^|:)claude-tool:(.+)$/.exec(id)?.[1];
    if (!call || this.spawnOf(call)) return;
    (this.spawns ??= { ...this.state.spawns })[call] = id;
    const agent = this.edit(id);
    agent.toolUseId ||= call;
    const skill = (this.skillCalls ?? this.state.skillCalls ?? {})[call];
    if (skill) { agent.name ||= skill; agent.type ||= 'skill'; }
  }

  /**
   * A background command's name: what the agent said it does, whole, else
   * what runs ("pytest tests/test_mcp.py"), never a description cut as if
   * it were a command. `reported` is the provider's name for the task.
   */
  private nameCommand(agent: Subagent, reported: string) {
    const call = agent.toolUseId ? (this.commandCalls ?? this.state.commandCalls ?? {})[agent.toolUseId] : undefined;
    if (call) {
      agent.command = call.command;
      const said = oneLine(reported, DESCRIPTION);
      agent.description = call.description || (said && said !== oneLine(call.command, DESCRIPTION) ? said : '');
    } else agent.command = reported;
    agent.name = agent.description || commandTitle(agent.command);
  }

  private status(raw: string, e: NonNullable<JournalEvent['event']>, at: number | undefined) {
    const data = payload(e.providerDataJson) || {};
    let id = raw;
    const toolUseId = text(data.toolUseId);
    if (toolUseId) {
      const known = this.spawnOf(toolUseId);
      if (known && known !== id) { (this.aliases ??= { ...this.state.aliases })[id] = known; if (this.get(known)) this.edit(known).runnerId = id; id = known; }
      else if (!known && this.get(id)) (this.spawns ??= { ...this.state.spawns })[toolUseId] = id;
    }
    if (!this.get(id)) {
      if (e.status && !['running', 'starting'].includes(e.status) && !e.name) return;
      const command = COMMAND_TASKS.has(e.agentKind) || COMMAND_TASKS.has(text(data.taskType));
      if (!this.create(id, e.parentAgentId || 'root', at, command ? 'command' : 'agent')) return;
      if (toolUseId) (this.spawns ??= { ...this.state.spawns })[toolUseId] = id;
    }
    const agent = this.edit(id);
    this.touch(agent, at);
    if (toolUseId) agent.toolUseId ||= toolUseId;
    if (e.name) {
      if (agent.kind === 'command') this.nameCommand(agent, e.name);
      // A placeholder ("Sub-agent") never replaces the task it was given.
      else if (!genericName(e.name)) agent.name = e.name;
    }
    if (text(data.outputFile)) agent.outputFile = data.outputFile;
    if (e.nativeId) agent.nativeId = e.nativeId;
    const type = text(data.subagentType);
    if (type) agent.type = type;
    else if (!agent.type && e.agentKind && !['agent', 'local_agent'].includes(e.agentKind)) agent.type = e.agentKind;
    if (text(data.prompt)) agent.prompt = data.prompt;
    if (data.backgrounded === true) agent.backgrounded = true;
    const usage = data.usage && typeof data.usage === 'object' ? data.usage : undefined;
    if (usage) {
      const tokens = number(usage.totalTokens), tools = number(usage.toolUses), duration = number(usage.durationMs);
      if (tokens !== undefined) agent.tokens = tokens;
      if (tools !== undefined) agent.reportedTools = tools;
      if (duration !== undefined) agent.durationMs = duration;
    }
    const lastTool = text(data.lastTool);
    if (lastTool && !agent.runningTool && activeSubagent(agent.status)) this.phase(agent, 'tool', lastTool, at);
    if (text(data.error)) agent.error = data.error;
    if (text(data.message) && !agent.result) agent.progress = data.message;
    // The worker's word that the turn it belonged to ended without its end reported.
    const status = e.status === 'unknown' && data.reason === 'turn_ended' ? (activeSubagent(agent.status) ? 'unreported' : undefined) : mapStatus(e.status, agent.status);
    if (status) {
      if (activeSubagent(status)) { agent.status = status; agent.endedAt = undefined; if (status === 'running' && agent.phase === 'starting') this.phase(agent, 'working', '', at); }
      else this.settle(agent, status, at);
    }
    const summary = text(data.summary);
    if (summary) { if (activeSubagent(agent.status)) agent.progress = summary; else agent.summary = summary; }
    if (!activeSubagent(agent.status)) agent.progress = '';
  }

  /** The session ended under them: each ended no later than its last own event. */
  private endAll(status: SubagentStatus, at: number | undefined) {
    for (const id of this.order ?? this.state.order) {
      const agent = this.get(id);
      if (agent && activeSubagent(agent.status)) this.settle(this.edit(id), status, agent.lastAt ?? at);
    }
  }

  /**
   * The main agent's turn ended: a sub-agent the turn waited on cannot still
   * run. One whose end was never reported (an older worker, a forked run, an
   * interrupted turn) ended unreported, at its last own event. Background
   * work outlives the turn and is left alone.
   */
  private endTurn(at: number | undefined) {
    for (const id of this.order ?? this.state.order) {
      const agent = this.get(id);
      if (agent && activeSubagent(agent.status) && endsWithTurn(agent, this.byId ?? this.state.byId)) this.settle(this.edit(id), 'unreported', agent.lastAt ?? at);
    }
  }

  result(): Subagents {
    if (!this.byId && !this.order && !this.spawns && !this.aliases && !this.commandCalls && !this.skillCalls && !this.managedCalls && this.truncated === this.state.truncated) return this.state;
    return { byId: this.byId ?? this.state.byId, order: this.order ?? this.state.order, spawns: this.spawns ?? this.state.spawns, aliases: this.aliases ?? this.state.aliases, commandCalls: this.commandCalls ?? this.state.commandCalls ?? {}, skillCalls: this.skillCalls ?? this.state.skillCalls ?? {}, managedCalls: this.managedCalls ?? this.state.managedCalls, truncated: this.truncated };
  }
}

/** Whether a record can change the sub-agent index; checked before any parsing. */
export function subagentRelevant(event: JournalEvent['event']): boolean {
  if (!event) return false;
  if (event.agentId && event.agentId !== 'root') return true;
  return event.type === 'tool_activity' || event.type === 'session_closed' || event.type === 'process_exit' || event.type === 'profile_switching' || event.type === 'turn_ended';
}

const shortPath = (path: string) => {
  const clean = path.replace(/^\.\//, '');
  if (clean.length <= 56) return clean;
  const parts = clean.split('/');
  let out = parts.pop() || '';
  while (parts.length && out.length + parts[parts.length - 1].length < 48) out = parts.pop() + '/' + out;
  return '…/' + out;
};
const oneLine = (value: string, max = 72) => { const line = value.replace(/\s+/g, ' ').trim(); return line.length > max ? line.slice(0, max - 1) + '…' : line; };
const quoted = (value: string, max = 48) => `“${oneLine(value, max)}”`;
const host = (url: string) => { try { const u = new URL(url); return oneLine(u.host + (u.pathname === '/' ? '' : u.pathname), 60); } catch { return oneLine(url, 60); } };

/** What a tool call is doing, as a person would say it: "Reading src/main.go", "Running go test ./…". */
export function actionLabel(name: string, input: unknown): string {
  const i = input && typeof input === 'object' && !Array.isArray(input) ? input as Record<string, any> : {};
  const path = text(i.file_path) || text(i.path) || text(i.notebook_path);
  const command = Array.isArray(input) ? input.join(' ') : typeof input === 'string' ? input : Array.isArray(i.command) ? i.command.join(' ') : text(i.command) || (Array.isArray(i.argv) ? i.argv.join(' ') : '');
  switch (name) {
    case 'Read': case 'NotebookRead': case 'View image': case 'view_image': return path ? `Reading ${shortPath(path)}` : 'Reading a file';
    case 'Edit': case 'MultiEdit': case 'NotebookEdit': return path ? `Editing ${shortPath(path)}` : 'Editing a file';
    case 'Write': return path ? `Writing ${shortPath(path)}` : 'Writing a file';
    case 'Bash': case 'Shell command': case 'command': case 'BashOutput':
      return command ? `Running ${oneLine(command, 64)}` : text(i.description) ? oneLine(i.description) : 'Running a command';
    case 'Grep': return text(i.pattern) ? `Searching for ${quoted(i.pattern)}${path ? ` in ${shortPath(path)}` : ''}` : 'Searching the code';
    case 'Glob': return text(i.pattern) ? `Finding ${oneLine(i.pattern, 56)}` : 'Finding files';
    case 'LS': return path ? `Listing ${shortPath(path)}` : 'Listing files';
    case 'WebFetch': return text(i.url) ? `Fetching ${host(i.url)}` : 'Fetching a page';
    case 'WebSearch': case 'Web search': return text(i.query) || typeof input === 'string' ? `Searching the web for ${quoted(text(i.query) || String(input))}` : 'Searching the web';
    case 'TodoWrite': return 'Updating its plan';
    case 'Task': case 'Agent': return text(i.description) ? `Delegating · ${oneLine(i.description, 60)}` : 'Starting a sub-agent';
    case 'File changes': return 'Editing files';
    case 'mcp__lazybox__command': case 'lazybox_command':
      return i.action === 'start' && command ? `Running ${oneLine(command, 64)}` : i.action === 'wait' ? 'Waiting for a command' : 'Reading command output';
  }
  if (text(i.description)) return oneLine(i.description);
  return toolLabel(name);
}

/** The phrase for a phase when no tool is running. */
export function phaseAction(phase: string, tool: string): string {
  switch (phase) {
    case 'thinking': return 'Thinking';
    case 'responding': return 'Writing';
    case 'requesting': return 'Waiting for the model';
    case 'compacting': return 'Compacting context';
    case 'starting': return 'Starting';
    case 'tool': return tool ? toolLabel(tool) : 'Running a tool';
    default: return 'Working';
  }
}

/** What the sub-agent is doing right now, and since when; undefined once it ended. */
export function currentAction(agent: Subagent): { text: string; since?: number } | undefined {
  if (!activeSubagent(agent.status)) return;
  if (agent.status === 'waiting') return { text: 'Waiting for approval' };
  if (agent.runningTool) {
    for (let i = agent.items.length - 1; i >= 0; i--) {
      const item = agent.items[i];
      if (item.kind === 'tool' && item.call.id === agent.runningTool) return { text: actionLabel(item.call.name, item.call.input), since: item.call.startedAt };
    }
  }
  if (agent.status === 'starting') return { text: 'Starting', since: agent.startedAt };
  return { text: phaseAction(agent.phase, agent.phaseTool), since: agent.phaseSince };
}

/** The first lines of a result, without Markdown chrome, for a finished card. */
export function resultPreview(value: string, max = 220): string {
  const lines = value.split('\n').map(line => line.replace(/^#{1,6}\s+|^\s*[-*]\s+|^>\s?/, '').replace(/\*\*|__|`/g, '').trim()).filter(Boolean);
  const joined = lines.slice(0, 4).join(' · ');
  return joined.length > max ? joined.slice(0, max - 1).trimEnd() + '…' : joined;
}

/** Tool calls to show: the provider's count when it is ahead of what was forwarded. */
export const toolTotal = (agent: Subagent) => Math.max(agent.toolCount, agent.reportedTools);

/** How long it ran (or has been running): the provider's measure when it ended. */
export function agentElapsed(agent: Subagent, now: number): number | undefined {
  if (!activeSubagent(agent.status) && agent.durationMs !== undefined) return agent.durationMs;
  if (agent.startedAt === undefined) return;
  // Ended without a word of when (the Runner says so, the journal never did): at its last own event.
  const end = activeSubagent(agent.status) ? now : agent.endedAt ?? agent.lastAt;
  return end !== undefined && end >= agent.startedAt ? end - agent.startedAt : undefined;
}

export function tokenLabel(tokens: number): string {
  if (tokens < 1000) return `${tokens} tokens`;
  if (tokens < 1_000_000) return `${(tokens / 1000).toFixed(tokens < 10_000 ? 1 : 0).replace(/\.0$/, '')}k tokens`;
  return `${(tokens / 1_000_000).toFixed(1).replace(/\.0$/, '')}M tokens`;
}

/**
 * What a person calls a sub-agent: its kind, when the kind says something
 * ("Explore", never "general-purpose"), and its task. The task is the
 * spawning call's description, else a name the Runner has for it, else the
 * first line of its instructions; a placeholder name is never a title.
 */
export function subagentLabel(agent: Subagent, runnerName = ''): { kind: string; title: string } {
  if (agent.kind === 'command') return { kind: '', title: agent.name || 'Background command' };
  const kind = genericType(agent.type) ? '' : agent.type.trim();
  const named = [agent.name, runnerName].find(name => name && !genericName(name) && name !== agent.type);
  const brief = agent.prompt.split('\n').map(line => line.replace(/^#+\s*/, '').trim()).find(Boolean);
  const title = named?.trim() || (brief ? oneLine(brief, 120) : '');
  if (title) return { kind, title };
  return kind ? { kind: '', title: `${kind} sub-agent` } : { kind: '', title: 'Sub-agent' };
}

/**
 * One line of a sub-agent's timeline: prose, a nested sub-agent, or a tool
 * line as the chat shows it (a call, or a run of reads or edits).
 */
export type StepRow =
  | { kind: 'message'; id: string; text: string }
  | { kind: 'agent'; id: string; agentId: string }
  | { kind: 'tool'; id: string; item: ToolItem; ids: readonly string[] };

const entries = new WeakMap<ToolCall, ToolEntry>();
const STATUS: Record<string, ToolEntry['status']> = { Running: 'running', Completed: 'completed', Failed: 'failed' };
/** A sub-agent's call as the chat's tool lines read it; computed once per version of the call. */
export function toolEntry(call: ToolCall): ToolEntry {
  let entry = entries.get(call);
  if (entry) return entry;
  const status = STATUS[call.status] || 'unknown';
  entry = applyToolPayload(undefined, { type: 'tool_call', id: call.id, name: call.name, status, input: call.input, output: call.output }, call.startedAt ?? call.endedAt) || { id: call.id, name: call.name, status };
  // The journal's own times: when the call was seen to start, and to end.
  entry.status = status; entry.startedAt = call.startedAt; entry.endedAt = call.endedAt;
  entry = stoppedEntry(entry, undefined);
  entries.set(call, entry);
  return entry;
}

const rowCache = new WeakMap<readonly SubagentItem[], StepRow[]>();
/**
 * The timeline as rows, its tool calls grouped as the chat groups them:
 * consecutive reads and searches one line, edits one line, a file read
 * again and again one line with a count; a failed call always its own.
 */
export function stepRows(items: readonly SubagentItem[]): StepRow[] {
  const cached = rowCache.get(items);
  if (cached) return cached;
  const rows: StepRow[] = [];
  let run: ToolEntry[] = [];
  const flush = () => {
    for (const item of groupTools(run)) {
      const group = item.kind === 'call' ? [item.entry] : item.entries;
      rows.push({ kind: 'tool', id: `tool:${group[0].id}`, item, ids: group.map(entry => entry.id) });
    }
    run = [];
  };
  for (const item of items) {
    if (item.kind === 'tool') { run.push(toolEntry(item.call)); continue; }
    flush();
    rows.push(item.kind === 'message' ? { kind: 'message', id: item.id, text: item.text } : { kind: 'agent', id: item.id, agentId: item.agentId });
  }
  flush();
  rowCache.set(items, rows);
  return rows;
}

export const statusWord: Record<SubagentStatus, string> = { starting: 'Starting', running: 'Running', waiting: 'Waiting', done: 'Done', failed: 'Failed', stopped: 'Stopped', unknown: 'Status unknown', unreported: 'Ended without a report' };

/**
 * Whether a sub-agent ends with the main agent's turn. A Claude sub-agent the
 * spawning call waits on (`claude-tool:<call>`, not run in the background,
 * nor inside one that was) cannot outlive the turn. Background commands,
 * background sub-agents, tasks the provider found running on its own
 * (`claude-task:`) and other providers' sub-threads (Codex keeps them across
 * turns) can.
 */
export function endsWithTurn(agent: Subagent, byId: Readonly<Record<string, Subagent>>): boolean {
  for (let node: Subagent | undefined = agent, depth = 0; node && depth < 32; node = byId[node.parentId], depth++) {
    if (node.kind !== 'agent' || node.backgrounded || !/(?:^|:)claude-tool:/.test(node.runnerId || node.id)) return false;
    if (node.parentId === 'root') return true;
  }
  return false;
}

/**
 * Whether a Stop is offered for it: the provider can stop one sub-agent, it
 * is running, and the Runner knows it by the provider's id (what a stop
 * addresses: a forked Skill has none, and only interrupting the turn ends it).
 */
export function canStopSubagent(agent: Subagent, status: SubagentStatus, session: { canStop: boolean; native: ReadonlySet<string> }): boolean {
  return session.canStop && activeSubagent(status) && (session.native.has(agent.runnerId) || session.native.has(agent.id));
}

/**
 * The Runner's view of a sub-agent is fresher than the journal for a
 * process that ended without a word (a stopped session, a lost worker):
 * a journal that still says running defers to it.
 */
export function mergedStatus(agent: Subagent, runner?: Readonly<Record<string, string>>): SubagentStatus {
  return mergedWith(agent, runner?.[agent.runnerId] ?? runner?.[agent.id]);
}
function mergedWith(agent: Subagent, runner?: string): SubagentStatus {
  if (!runner || !activeSubagent(agent.status)) return agent.status;
  const mapped = mapStatus(runner, agent.status);
  return mapped && !activeSubagent(mapped) ? mapped : agent.status;
}

/**
 * Whether Background work leaves it out: the person cleared it, and neither
 * it nor anything it started runs. Running work is never hidden.
 */
export function hiddenFromWork(agent: Subagent, cleared: ReadonlySet<string>, status: (agent: Subagent) => SubagentStatus, byId: Readonly<Record<string, Subagent>>): boolean {
  if (!cleared.has(agent.id)) return false;
  const running = (node: Subagent, depth: number): boolean => activeSubagent(status(node)) || (depth < 16 && node.children.some(id => { const child = byId[id]; return !!child && running(child, depth + 1); }));
  return !running(agent, 0);
}

/** What Clear finished clears: every listed item that ended (done, failed, stopped or otherwise). */
export function finishedWork(agents: readonly Subagent[], status: (agent: Subagent) => SubagentStatus): string[] {
  return agents.filter(agent => !activeSubagent(status(agent))).map(agent => agent.id);
}

/** Background commands, running first, newest first. */
export function backgroundCommands(state: Subagents, runner?: Readonly<Record<string, string>>): Subagent[] {
  const out = state.order.map(id => state.byId[id]).filter((agent): agent is Subagent => agent?.kind === 'command').reverse();
  return out.sort((a, b) => Number(activeSubagent(mergedStatus(b, runner))) - Number(activeSubagent(mergedStatus(a, runner))));
}

/** Sub-agents in tree order: each followed by the ones it spawned. Background commands are not sub-agents. */
export function subagentTree(state: Subagents): { agent: Subagent; depth: number }[] {
  const out: { agent: Subagent; depth: number }[] = [];
  const visit = (id: string, depth: number) => {
    const agent = state.byId[id];
    if (!agent || agent.kind === 'command' || depth > 16) return;
    out.push({ agent, depth });
    for (const child of agent.children) visit(child, depth + 1);
  };
  for (const id of state.order) { const agent = state.byId[id]; if (agent && (agent.parentId === 'root' || !state.byId[agent.parentId])) visit(id, 0); }
  return out;
}

/** Sub-agents in tree order, the newest top-level one first; each still followed by the ones it spawned. */
export function newestFirstTree(state: Subagents): { agent: Subagent; depth: number }[] {
  const tree = subagentTree(state), roots: { agent: Subagent; depth: number }[][] = [];
  for (const node of tree) if (node.depth === 0) roots.push([node]); else roots.at(-1)?.push(node);
  return roots.reverse().flat();
}

/** What runs in a conversation's background, counted apart: never "19 sub-agents and commands". */
export type WorkCounts = { agents: number; commands: number; agentsRunning: number; commandsRunning: number; running: number; agentsFailed: number; commandsFailed: number; failed: number };
export function workCounts(agents: readonly Subagent[], commands: readonly Subagent[], status: (agent: Subagent) => SubagentStatus): WorkCounts {
  const count = (list: readonly Subagent[], test: (s: SubagentStatus) => boolean) => list.reduce((n, agent) => n + (test(status(agent)) ? 1 : 0), 0);
  const agentsRunning = count(agents, activeSubagent), commandsRunning = count(commands, activeSubagent);
  const agentsFailed = count(agents, s => s === 'failed'), commandsFailed = count(commands, s => s === 'failed');
  return { agents: agents.length, commands: commands.length, agentsRunning, commandsRunning, running: agentsRunning + commandsRunning, agentsFailed, commandsFailed, failed: agentsFailed + commandsFailed };
}
const counted = (n: number, one: string, many: string) => `${n} ${n === 1 ? one : many}`;
/** "2 sub-agents · 17 commands": what there is, each kind once, none of a kind left out. */
export function workTotals(c: WorkCounts): string {
  return [c.agents && counted(c.agents, 'sub-agent', 'sub-agents'), c.commands && counted(c.commands, 'command', 'commands')].filter(Boolean).join(' · ') || 'Nothing yet';
}
/** "2 sub-agents · 17 commands · 0 running · 1 failed": the totals, then their state. */
export function workSummary(c: WorkCounts): string {
  return [workTotals(c), `${c.running} running`, c.failed ? `${c.failed} failed` : ''].filter(Boolean).join(' · ');
}
/** "3 sub-agents · 1 command running": what runs now, by kind. */
export function runningSummary(c: WorkCounts): string {
  const kinds = [c.agentsRunning && counted(c.agentsRunning, 'sub-agent', 'sub-agents'), c.commandsRunning && counted(c.commandsRunning, 'command', 'commands')].filter(Boolean).join(' · ');
  return kinds ? `${kinds} running` : 'Nothing running';
}

/**
 * A long list, newest first, with its older finished items folded away: the
 * first `keep` items, and after them only what still needs a look (running
 * or failed). `earlier` counts what is folded.
 */
export function recentWork<T>(items: readonly T[], notable: (item: T) => boolean, keep = 5): { shown: T[]; earlier: number } {
  const shown = items.filter((item, index) => index < keep || notable(item));
  return { shown, earlier: items.length - shown.length };
}

/** The chain from the top-level sub-agent down to this one. */
export function ancestry(state: Subagents, id: string): string[] {
  const chain: string[] = [];
  let current: Subagent | undefined = state.byId[id];
  while (current && chain.length < 17) { chain.unshift(current.id); current = current.parentId !== 'root' ? state.byId[current.parentId] : undefined; }
  return chain;
}

const LAUNCHERS = new Set(['sudo', 'time', 'env', 'exec', 'nohup', 'command', 'timeout', 'nice']);
const SETUP = new Set(['cd', 'pushd', 'export', 'source', '.', 'echo', 'printf', 'sleep', 'mkdir', 'true', 'set', 'trap', 'unset', 'wait']);
/** Programs whose first two words say what runs: "go test ./…", "npm run build". */
const TWO_WORDS = new Set(['npm', 'pnpm', 'yarn', 'bun', 'go', 'cargo', 'git', 'docker', 'kubectl', 'make', 'gradle', 'mvn', 'dotnet', 'swift', 'deno', 'gh', 'terraform', 'helm']);
const unquote = (token: string) => token.replace(/^(["'])(.*)\1$/, '$2');

/**
 * A background command as a person names it: the command and its target
 * ("pytest tests/test_mcp.py", "go test ./internal/..."), never the whole
 * script. Setup lines (cd, export, comments) are skipped.
 */
export function commandTitle(script: string): string {
  const clip = (value: string) => value.length > 56 ? value.slice(0, 55) + '…' : value;
  const lines = script.split('\n').map(line => line.trim()).filter(line => line && !line.startsWith('#'));
  for (const line of lines) {
    for (const segment of line.split(/\s*(?:&&|\|\||;|\|)\s*/)) {
      const tokens = segment.match(/"[^"]*"|'[^']*'|\S+/g) || [];
      let i = 0;
      while (i < tokens.length && (/^[A-Za-z_][A-Za-z0-9_]*=/.test(tokens[i]) || LAUNCHERS.has(tokens[i]) || (/^\d+$/.test(tokens[i]) && tokens[i - 1] === 'timeout'))) i++;
      if (i >= tokens.length) continue;
      let program = unquote(tokens[i]).replace(/^.*\//, '');
      if (SETUP.has(program) || program === '(' || program === '{') continue;
      const args: string[] = [];
      for (let j = i + 1; j < tokens.length; j++) {
        const token = unquote(tokens[j]);
        // A redirection and its target are not what runs.
        if (/^\d*[<>]+&?\d*$/.test(token)) { j++; continue; }
        if (token.startsWith('-') || /^\d*[<>]/.test(token) || token === '&') continue;
        args.push(token);
      }
      // Runners name what they run: "uv run pytest x" is pytest x.
      if (['npx', 'bunx', 'pipx'].includes(program) && args.length) program = args.shift()!.replace(/^.*\//, '');
      else if (['uv', 'poetry', 'pipenv', 'bundle', 'pdm', 'hatch'].includes(program) && ['run', 'exec'].includes(args[0]) && args.length > 1) { args.shift(); program = args.shift()!.replace(/^.*\//, ''); }
      else if (/^python[\d.]*$/.test(program) && tokens.slice(i + 1).includes('-m') && args.length) program = args.shift()!;
      return clip([program, ...args.slice(0, TWO_WORDS.has(program) ? 2 : 1)].join(' '));
    }
  }
  return clip(lines[0] || script.trim() || 'Background command');
}
