/**
 * Opens the shell panel in a home checkout (or the home folder, or a folder of
 * the workspace) and, when asked, runs or types a command there. Lazybox is
 * not a terminal emulator: this is the workspace's own Terminal panel, a real
 * PTY, so a login can ask for a password or open a browser sign-in. The shell
 * started outside the workspace (`OpenTerminal` with `repositoryHome` or
 * `userHome`) still belongs to it: `lazybox log` typed there feeds its console.
 *
 * Run runs a one-line command (Return included); a script of several lines,
 * a command with a placeholder, or Paste is typed at the prompt for the
 * person to review and run with Return. Either way nothing is written before
 * the shell is ready (`shell-input.ts`), and a shell already open there, idle
 * at its prompt, is used rather than a new one.
 *
 * The shell marks where each command line ends (OSC 133;D;<status>,
 * agentshell's zsh integration); the Runner counts those marks on the
 * terminal (`commandEnds`, `lastCommandStatus`), which says when the command
 * finished, and how. Reading that status needs no stream of its own: a
 * window's connections to the Runner are few.
 */
import type { RunnerClient } from '@lazybox/client-core';
import type { TerminalStatus } from '@lazybox/protocol';
import { runAction } from './actions';
import { showMenuNotice } from './ContextMenu';
import { requestTerminal } from './TerminalPanel';
import { reusableTerminal, runsAtOnce, sample, shellInput, shellReadiness, type ShellPlace, type ShellSample } from './shell-input';

export type { ShellPlace } from './shell-input';

const encoder = new TextEncoder();
const reason = (error: unknown) => String(error instanceof Error ? error.message : error).replace(/^\[\w+\]\s*/, '');
const pause = (ms: number) => new Promise(resolve => setTimeout(resolve, ms));
/** Shells this window is typing into, or holds a typed command at the prompt of: never reused for another. */
const taken = new Set<string>();
/** The notice that says a command waits at the prompt. */
export const TYPED_NOTICE = 'Typed in the terminal: review and press Return';

async function statusOf(api: RunnerClient, workspaceId: string, terminalId: string): Promise<TerminalStatus | undefined> {
  const { terminals } = await api.listTerminals({ collectionId: workspaceId });
  return terminals.find(value => value.id === terminalId);
}

/** Waits until a new shell can take input (see `shell-input.ts`); undefined when it exited first. */
async function whenReady(api: RunnerClient, workspaceId: string, terminalId: string): Promise<TerminalStatus | undefined> {
  const since = Date.now(), samples: ShellSample[] = [];
  let status: TerminalStatus | undefined;
  for (;;) {
    status = await statusOf(api, workspaceId, terminalId).catch(() => status);
    if (!status && Date.now() - since > 3000) return undefined;
    if (status) samples.push(sample(status, Date.now()));
    const readiness = shellReadiness(samples, since, Date.now());
    if (readiness === 'gone') return undefined;
    if (readiness !== 'wait' && status) return status;
    await pause(120);
  }
}

/** Writes `text` into a shell just opened, once it can take it; false when the shell exited first. */
export async function writeWhenReady(api: RunnerClient, workspaceId: string, terminalId: string, text: string): Promise<boolean> {
  if (!await whenReady(api, workspaceId, terminalId)) return false;
  await api.writeTerminal({ terminalId, data: encoder.encode(text) });
  return true;
}

/** How a command went into the shell: run (Return sent) or typed for the person. */
export type ShellTyped = 'run' | 'typed';
export type ShellCommand = {
  /** Run sends Return after a one-line command; Paste never does. */
  mode?: 'run' | 'paste';
  /** Show a specific shell in the caller’s own terminal panel. */
  showTerminal?(terminalId: string): void;
  /** The command is in the shell: running, or waiting at the prompt. */
  onTyped?(how: ShellTyped): void;
  /** The command line the person or Run started there ended, with its status, when the shell says. */
  onEnd?(status: number): void;
};

/**
 * Writes `command` into the shell once it can take it, then waits for the
 * next command-end mark: the command line ran and ended. Gives up when the
 * shell goes.
 */
async function typeAndWatch(api: RunnerClient, workspaceId: string, terminalId: string, command: string, label: string, reused: boolean, how: ShellCommand) {
  taken.add(terminalId);
  try {
    const status = reused ? await statusOf(api, workspaceId, terminalId).catch(() => undefined) : await whenReady(api, workspaceId, terminalId);
    if (!status || status.done) return;
    const base = status.commandEnds;
    const run = how.mode !== 'paste' && runsAtOnce(command);
    await api.writeTerminal({ terminalId, data: encoder.encode(shellInput(command, run, reused)) });
    how.onTyped?.(run ? 'run' : 'typed');
    showMenuNotice(run ? `Running in ${label}` : TYPED_NOTICE);
    // The shell stays taken until the command line ends: the Runner may still call it idle a moment after Return.
    // A quick command ends in a moment, a login takes a minute, a dev server hours: look often at first, then calmly, for half a day at most.
    const started = Date.now();
    while (Date.now() - started < 12 * 3600_000) {
      const elapsed = Date.now() - started;
      await pause(elapsed < 10_000 ? 250 : elapsed < 10 * 60_000 ? 1000 : 10_000);
      const now = await statusOf(api, workspaceId, terminalId).catch(() => undefined);
      if (!now || now.done) return;
      if (now.commandEnds > base) { how.onEnd?.(now.lastCommandStatus); return; }
    }
  } finally { taken.delete(terminalId); }
}

/**
 * A shell in `place`, shown and focused in the workspace's Terminal panel.
 * With `command`, it goes into a shell already open there and idle at its
 * prompt, or else a new one, once that shell is ready: run at once when it
 * is one line with nothing to fill in (mode `run`), otherwise typed at the
 * prompt for the person to review and run with Return. `onEnd` hears the
 * status of the command line that runs there, when the shell says.
 */
export async function openHomeShell(api: RunnerClient, workspaceId: string, place: ShellPlace, command = '', how: ShellCommand = {}): Promise<{ terminalId?: string; reused?: boolean; error: string }> {
  if (!workspaceId) return { error: 'Open a workspace first: the shell runs in its Terminal panel.' };
  try {
    const open = command ? await api.listTerminals({ collectionId: workspaceId }).then(value => value.terminals, () => []) : [];
    const idle = command ? reusableTerminal(open, place, taken) : undefined;
    const terminal = idle || await api.openTerminal({ collectionId: workspaceId, ...('repositoryId' in place ? { repositoryHome: place.repositoryId } : 'userHome' in place ? { userHome: true } : { directory: place.directory }), cols: 100, rows: 30, requestKey: crypto.randomUUID() });
    // The specific shell first, so showing the panel does not start another.
    if (how.showTerminal) how.showTerminal(terminal.id);
    else { requestTerminal(workspaceId, terminal.id); runAction('terminal.open'); }
    if (!command) { if (!how.showTerminal) showMenuNotice(`Shell in ${place.label}`); return { terminalId: terminal.id, error: '' }; }
    if (!idle) showMenuNotice(`Starting a shell in ${place.label}…`);
    taken.add(terminal.id);
    void typeAndWatch(api, workspaceId, terminal.id, command, place.label, !!idle, how).catch(() => {});
    return { terminalId: terminal.id, reused: !!idle, error: '' };
  } catch (error) {
    const why = reason(error);
    showMenuNotice(`Could not open a shell in ${place.label}: ${why}`);
    return { error: why };
  }
}
