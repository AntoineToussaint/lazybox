import type { JournalEvent, Session } from '@lazybox/protocol';
import { awaitingFirstMessage, runningChild, statusLabel } from './agent-status';
import { limitStatus } from './subscription-model';

/**
 * What a conversation is doing right now, in one short phrase that moves while
 * the agent works: "Thinking · 4s", "Running · Bash · 12s", "Waiting for approval".
 *
 * - `busy`: the agent is on a step; the indicator animates.
 * - `waiting`: the agent is blocked on the person; the indicator pulses.
 * - `idle`, `failed`, `saved`: nothing moves.
 */
export type LiveTone = 'busy' | 'waiting' | 'idle' | 'failed' | 'saved';
/** `label` is stable while a step lasts (for assistive technology); `text` adds the moving elapsed time. */
export type LiveStatus = { tone: LiveTone; label: string; text: string; since?: number; toolPhase?: boolean };
const fixed = (tone: LiveTone, label: string): LiveStatus => ({ tone, label, text: label });

const millis = (value: bigint | number | undefined) => { const n = Number(value || 0); return Number.isFinite(n) && n > 0 ? n : 0; };

/** "12s", "3m 04s", "1h 02m": short enough for a sidebar row. */
export function elapsedLabel(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000));
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${String(s % 60).padStart(2, '0')}s`;
  return `${Math.floor(m / 60)}h ${String(m % 60).padStart(2, '0')}m`;
}

const toolWords: Record<string, string> = {
  bash: 'Running a command', shell: 'Running a command', exec_command: 'Running a command', command: 'Running a command', lazybox_command: 'Running a command', local_shell: 'Running a command',
  read: 'Reading files', read_file: 'Reading files', view: 'Reading files', notebookread: 'Reading a notebook',
  edit: 'Editing files', multiedit: 'Editing files', write: 'Writing files', apply_patch: 'Editing files', notebookedit: 'Editing a notebook',
  grep: 'Searching the code', glob: 'Finding files', ls: 'Listing files', search: 'Searching',
  webfetch: 'Reading a web page', websearch: 'Searching the web', web_search: 'Searching the web',
  task: 'Running a sub-agent', agent: 'Running a sub-agent', todowrite: 'Updating its plan', update_plan: 'Updating its plan',
  update_settings: 'Proposing a settings change', read_settings: 'Reading settings',
};
/**
 * A tool as a person says it: "Running a command", not "mcp__lazybox__command".
 * MCP tools (mcp__server__tool) use the tool's words, else "Using <server>".
 */
export function toolLabel(tool: string): string {
  const mcp = /^mcp__(.+?)__(.+)$/.exec(tool);
  const name = (mcp ? mcp[2] : tool).toLowerCase();
  if (toolWords[name]) return toolWords[name];
  if (mcp) return `Using ${mcp[1].replace(/^lazybox_?/, 'Lazybox ').replaceAll('_', ' ').trim()} · ${mcp[2].replaceAll('_', ' ')}`;
  return `Using ${tool.replaceAll('_', ' ')}`;
}

/** A step's words, without timing. Unknown phases read as working. */
export function phaseLabel(phase: string, tool = ''): string {
  switch (phase) {
    case 'tool': return tool ? toolLabel(tool) : 'Running a tool';
    case 'thinking': return 'Thinking';
    case 'responding': return 'Writing';
    case 'requesting': return 'Waiting for the model';
    case 'compacting': return 'Compacting context';
    case 'starting': return 'Starting';
    case 'switching': return 'Switching subscription';
    case 'background': return 'Background agents working';
    default: return 'Working';
  }
}

/** A step and when it began; `at` is undefined when no time was observed. */
export type Phase = { phase: string; tool: string; at?: number };

/**
 * The main agent's current step, from the conversation's own journal: the
 * freshest evidence a chat has. Empty when the last turn ended.
 */
export function phaseFromEvents(events: readonly JournalEvent[]): Phase | undefined {
  let found: Phase | undefined;
  for (let i = events.length - 1; i >= 0; i--) {
    const { event: e, capturedAt } = events[i];
    if (!e || (e.agentId && e.agentId !== 'root')) continue;
    const parsed = Date.parse(capturedAt), at = Number.isFinite(parsed) ? parsed : undefined;
    let step: Phase | undefined;
    if (['turn_ended', 'session_closed', 'failure', 'question', 'approval'].includes(e.type)) return found;
    if (e.type === 'activity') step = { phase: e.status || 'working', tool: e.status === 'tool' ? e.name : '', at };
    else if (e.type === 'message_delta') step = { phase: 'responding', tool: '', at };
    else if (e.type === 'turn_started' || e.type === 'operation_accepted' || e.type === 'request_resolved') step = { phase: 'working', tool: '', at };
    if (!step) continue;
    // The step began at the earliest consecutive report of the same step.
    if (!found) found = step;
    else if (step.phase === found.phase && step.tool === found.tool) found = { ...found, at: step.at ?? found.at };
    else return found;
  }
  return found;
}

/** The journal's latest word on the main agent's turn is that it ended. */
export function turnEndedInJournal(events: readonly JournalEvent[]): boolean {
  for (let i = events.length - 1; i >= 0; i--) {
    const e = events[i].event;
    if (!e || (e.agentId && e.agentId !== 'root')) continue;
    if (['turn_ended', 'session_closed'].includes(e.type)) return true;
    if (['turn_started', 'operation_accepted', 'activity', 'message_delta', 'request_resolved'].includes(e.type)) return false;
  }
  return false;
}

/**
 * The live status of a conversation. `events` (a chat's journal) refines the
 * step when supplied; rows pass only the session the Runner lists.
 */
/** `subagentsShown`: the place showing this status lists the sub-agents itself (the Agents list's nested row), so it does not count them. */
export function liveStatus(session: Session | undefined, now: number, events?: readonly JournalEvent[], { subagentsShown = false }: { subagentsShown?: boolean } = {}): LiveStatus {
  if (!session) return fixed('busy', 'Connecting…');
  if (session.fault === 'native_exit_unconfirmed') return fixed('failed', 'Status unknown');
  if (session.historical && session.status === 'interrupted') return fixed('failed', 'Interrupted');
  if (session.closed) return fixed('saved', 'Saved');
  if (session.pending.length || session.status === 'waiting_input' || session.status === 'waiting_approval') {
    const question = session.pending[0] ? session.pending[0].type === 'question' : session.status === 'waiting_input';
    const more = session.pending.length > 1 ? ` · ${session.pending.length} waiting` : '';
    if (session.pending[0]?.type === 'terminal_handoff') return fixed('waiting', 'Waiting for terminal approval' + more);
    if (session.pending[0]?.type === 'repository_request') return fixed('waiting', 'Waiting for a repository' + more);
    if (session.pending[0]?.type === 'browser_request') return fixed('waiting', 'Waiting for you' + more);
    if (session.pending[0]?.type === 'conversation_request') return fixed('waiting', 'Waiting to start a conversation' + more);
    return fixed('waiting', (question ? 'Waiting for your answer' : 'Waiting for approval') + more);
  }
  // A subscription limit is a decision for the person, not a failure.
  const limit = limitStatus(session, now);
  if (limit) return session.status === 'switching_profile' ? timed(limit, millis(session.agents.find(a => a.id === 'root')?.phaseSinceUnixMillis), now) : fixed('waiting', limit);
  if (session.fault || session.status === 'failed') return fixed('failed', 'Needs attention');
  if (session.status === 'switching_profile') return timed('Switching subscription', millis(session.agents.find(a => a.id === 'root')?.phaseSinceUnixMillis), now);
  if (awaitingFirstMessage(session)) return fixed('idle', 'Ready');
  const root = session.agents.find(agent => agent.id === 'root');
  // A background shell command (Claude's run_in_background) is not a sub-agent.
  const working = session.agents.filter(runningChild);
  const commands = working.filter(agent => agent.kind === 'local_bash').length, children = working.length - commands;
  // The journal is fresher than the polled session: a turn it saw end is over.
  const ended = !!events && turnEndedInJournal(events);
  const rootWorking = !ended && (session.busy || root?.status === 'running' || (!root && session.status === 'running'));
  if (rootWorking) {
    const journal = events ? phaseFromEvents(events) : undefined;
    const step: Phase = journal || { phase: root?.phase || (session.status === 'starting' ? 'starting' : 'working'), tool: root?.tool || '', at: millis(root?.phaseSinceUnixMillis) || millis(root?.turnStartedAtUnixMillis) || undefined };
    const status = timed(phaseLabel(step.phase, step.tool), step.at, now);
    return step.phase === 'tool' ? { ...status, toolPhase: true } : status;
  }
  if (children || commands) return fixed('busy', ['Running', !subagentsShown && children && `${children} ${children === 1 ? 'sub-agent' : 'sub-agents'}`, commands && `${commands} background ${commands === 1 ? 'command' : 'commands'}`].filter(Boolean).join(' · '));
  if (session.status === 'starting') return fixed('busy', 'Starting…');
  if (session.status === 'running' && !ended) return fixed('busy', 'Working…');
  if (ended && ['running', 'starting'].includes(session.status)) return fixed('idle', 'Turn done');
  return fixed(session.status === 'interrupted' ? 'failed' : 'idle', statusLabel(session.status || 'ready'));
}

function timed(text: string, at: number | undefined, now: number): LiveStatus {
  // A step that started after the shared clock's last tick is still a timed step ("0s"): whether a status
  // is timed must not depend on the instant it is read at, or the ticker that reads it turns itself off and on.
  return at ? { tone: 'busy', label: text, text: `${text} · ${elapsedLabel(Math.max(0, now - at))}`, since: at } : { tone: 'busy', label: text, text: `${text}…` };
}
