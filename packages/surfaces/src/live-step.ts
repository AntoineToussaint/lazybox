import type { CommandStatus, JournalEvent } from '@lazybox/protocol';
import { actionLabel } from './subagent-model';
import { serverLike, serverLikeArgv } from './code-run';
import { plainLazybox } from './lazybox-cli';

/** The main agent's tool call in progress, from its journal. */
export type RootStep = { id: string; name: string; input: unknown; startedAt?: number };

const decoder = new TextDecoder();
const parsed = new WeakMap<object, Record<string, any> | null>();
function payload(event: NonNullable<JournalEvent['event']>): Record<string, any> | undefined {
  let value = parsed.get(event);
  if (value === undefined) {
    try { const raw = JSON.parse(decoder.decode(event.providerDataJson)); value = raw && typeof raw === 'object' && !Array.isArray(raw) ? raw : null; } catch { value = null; }
    parsed.set(event, value ?? null);
  }
  return value ?? undefined;
}

/**
 * The tool call the main agent is in, read backwards from the newest event:
 * none once the turn ended, or once the agent moved on to thinking or
 * writing. A call whose result was recorded is over.
 */
export function rootStep(events: readonly JournalEvent[]): RootStep | undefined {
  const finished = new Set<string>();
  for (let i = events.length - 1; i >= 0 && i >= events.length - 400; i--) {
    const { event: e, capturedAt } = events[i];
    if (!e || (e.agentId && e.agentId !== 'root')) continue;
    if (['turn_ended', 'turn_started', 'session_closed', 'failure', 'operation_accepted'].includes(e.type)) return;
    if (e.type === 'message_delta' || (e.type === 'activity' && e.status !== 'tool' && e.status !== 'working')) return;
    if (e.type !== 'tool_activity') continue;
    const data = payload(e);
    if (!data || data.type !== 'tool_call' || typeof data.id !== 'string') continue;
    if (data.status === 'completed' || data.status === 'failed') { finished.add(data.id); continue; }
    // A sub-agent's call is shown by its chip; the step is what the agent itself runs.
    if (data.status !== 'running' || finished.has(data.id) || data.name === 'Task' || data.name === 'Agent') continue;
    const at = Date.parse(capturedAt);
    return { id: data.id, name: typeof data.name === 'string' ? data.name : 'Tool', input: data.input, startedAt: Number.isFinite(at) ? at : undefined };
  }
  return;
}

const SHELL = new Set(['Bash', 'Shell command', 'command', 'shell', 'exec_command', 'local_shell']);
const MANAGED = new Set(['mcp__lazybox__command', 'lazybox_command']);
export const isManagedCommandTool = (name: string) => MANAGED.has(name);

/**
 * A step as the live bar says it: the command text where there is one
 * (`pnpm run check`, run in `services/front-door`), or the action in words.
 */
export function stepDetail(step: RootStep): { code?: string; where?: string; full?: string; words?: string } {
  const input = step.input && typeof step.input === 'object' && !Array.isArray(step.input) ? step.input as Record<string, any> : {};
  let script = '';
  if (SHELL.has(step.name)) script = Array.isArray(step.input) ? step.input.join(' ') : typeof step.input === 'string' ? step.input : Array.isArray(input.command) ? input.command.join(' ') : typeof input.command === 'string' ? input.command : '';
  else if (MANAGED.has(step.name) && input.action === 'start' && Array.isArray(input.argv)) script = input.argv.join(' ');
  if (!script) return MANAGED.has(step.name) ? { words: input.action === 'wait' ? 'Waiting for its command' : 'Reading command output' } : step.name === 'Task' || step.name === 'Agent' ? {} : { words: actionLabel(step.name, step.input) };
  return { ...commandParts(script), full: script };
}

/** "cd services/front-door && pnpm run check" → the command, and where it runs. */
export function commandParts(script: string): { code: string; where?: string } {
  const lines = script.split('\n').map(line => line.trim()).filter(line => line && !line.startsWith('#'));
  let first = lines[0] || script.trim(), where: string | undefined;
  const cd = /^cd\s+("[^"]+"|'[^']+'|\S+)\s*(?:&&|;)\s*(.+)$/.exec(first);
  if (cd) { where = cd[1].replace(/^["']|["']$/g, ''); first = cd[2]; }
  const code = first.length > 120 ? first.slice(0, 119) + '…' : first;
  return { code: lines.length > 1 ? code + ' …' : code, where };
}

/**
 * The Lazybox-managed command of this conversation that runs now, if any: the
 * newest one still running. One that went on in the background (a server
 * `lazybox run` moved there) is not the turn's step any more.
 */
export function runningCommand(commands: readonly CommandStatus[], sessionId: string): CommandStatus | undefined {
  for (let i = commands.length - 1; i >= 0; i--) {
    const command = commands[i];
    if (!command.done && command.sessionId === sessionId && !command.origin && !command.background) return command;
  }
  return;
}

/** How long a foreground command may stay quiet before the bar offers to move it to the background. */
export const QUIET_MS = 20_000;
/** How long a shell step may run before the bar warns it may never end. */
export const LONG_STEP_MS = 60_000;

/**
 * Whether the bar offers Move to background for a `lazybox run` in the
 * foreground: its CLI must follow the Runner (canBackground), and it must look
 * like it will not end: it listens on a port, it looks like a server or a
 * watcher, or its output has not changed for QUIET_MS.
 */
export function offersBackground(command: Pick<CommandStatus, 'canBackground' | 'background' | 'done' | 'ports' | 'argv' | 'title'>, quietForMs: number): boolean {
  if (!command.canBackground || command.background || command.done) return false;
  return command.ports.length > 0 || quietForMs >= QUIET_MS || serverLikeArgv(command.argv).server || serverLike(command.title).server;
}

/**
 * Whether a shell step the agent runs itself (not through `lazybox run`,
 * however it calls lazybox)
 * may never end: it looks like a server or a watcher, or it has run long.
 */
export function mayNeverEnd(script: string | undefined, runningForMs: number): boolean {
  if (!script || /\blazybox\s+run\b/.test(plainLazybox(script))) return false;
  return serverLike(script).server || runningForMs >= LONG_STEP_MS;
}

/** Terminal output as lines a person can read: no colour codes, carriage-return redraws resolved. */
export function outputLines(text: string, count: number): string[] {
  // eslint-disable-next-line no-control-regex
  const clean = text.replace(/\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07]*(?:\x07|\x1b\\)/g, '');
  const lines = clean.split('\n').map(line => { const parts = line.split('\r'); return parts.filter(Boolean).at(-1) ?? ''; });
  while (lines.length && !lines.at(-1)!.trim()) lines.pop();
  return lines.slice(-count);
}
