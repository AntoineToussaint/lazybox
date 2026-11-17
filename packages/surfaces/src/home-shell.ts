/**
 * Opens the shell panel in a home checkout (or the home folder, or a folder of
 * the workspace) and, when asked, types a command at its prompt. Lazybox is
 * not a terminal emulator: this is the workspace's own Terminal panel, a real
 * PTY, so a login can ask for a password or open a browser sign-in. The shell
 * started outside the workspace (`OpenTerminal` with `repositoryHome` or
 * `userHome`) still belongs to it: `lazybox log` typed there feeds its console.
 *
 * Nothing runs by itself: the command waits at the prompt for the person to
 * read, edit and run with Return. The shell marks where each command line
 * ends (OSC 133;D;<status>, agentshell's zsh integration); the Runner counts
 * those marks on the terminal (`commandEnds`, `lastCommandStatus`), which says
 * when the command the person ran finished, and how. Reading that status
 * needs no stream of its own: a window's connections to the Runner are few.
 */
import type { RunnerClient } from '@lazybox/client-core';
import type { TerminalStatus } from '@lazybox/protocol';
import { runAction } from './actions';
import { showMenuNotice } from './ContextMenu';
import { requestTerminal } from './TerminalPanel';

/** Where a shell starts: a repository's home checkout, the home folder, or a folder of the workspace (relative to its root; '' is the root). */
export type ShellPlace = { repositoryId: string; label: string } | { userHome: true; label: string } | { directory: string; label: string };

const encoder = new TextEncoder();
const reason = (error: unknown) => String(error instanceof Error ? error.message : error).replace(/^\[\w+\]\s*/, '');
const pause = (ms: number) => new Promise(resolve => setTimeout(resolve, ms));

async function statusOf(api: RunnerClient, workspaceId: string, terminalId: string): Promise<TerminalStatus | undefined> {
  const { terminals } = await api.listTerminals({ collectionId: workspaceId });
  return terminals.find(value => value.id === terminalId);
}

/**
 * Types `command` once the shell's first prompt is up (its first command-end
 * mark, or a few seconds for a shell without them), then waits for the next
 * mark: the command line the person ran ended. Gives up when the shell goes.
 */
async function typeAndWatch(api: RunnerClient, workspaceId: string, terminalId: string, command: string, label: string, ended: (status: number) => void) {
  let status: TerminalStatus | undefined;
  for (const until = Date.now() + 3000; Date.now() < until; await pause(150)) {
    status = await statusOf(api, workspaceId, terminalId).catch(() => undefined);
    if (!status || status.done || status.commandEnds > 0n) break;
  }
  if (!status || status.done) return;
  const base = status.commandEnds;
  // A script of several lines goes as one paste, which the shell holds at the prompt.
  const text = command.includes('\n') ? `\x1b[200~${command}\x1b[201~` : command;
  await api.writeTerminal({ terminalId, data: encoder.encode(text) });
  showMenuNotice(`Ready in ${label}: check the command, then press Return`);
  // A login takes a minute, a dev server hours: look often at first, then calmly, for half a day at most.
  const started = Date.now();
  while (Date.now() - started < 12 * 3600_000) {
    await pause(Date.now() - started < 10 * 60_000 ? 1000 : 10_000);
    const now = await statusOf(api, workspaceId, terminalId).catch(() => undefined);
    if (!now || now.done) return;
    if (now.commandEnds > base) { ended(now.lastCommandStatus); return; }
  }
}

/**
 * A new shell in `place`, shown and focused in the workspace's Terminal
 * panel. With `command`, it is typed at the prompt, not run: the person
 * reads it, edits it if they like, and presses Return. `onEnd` hears the
 * status of the command line they run there, when the shell says.
 */
export async function openHomeShell(api: RunnerClient, workspaceId: string, place: ShellPlace, command = '', onEnd?: (status: number) => void): Promise<{ terminalId?: string; error: string }> {
  if (!workspaceId) return { error: 'Open a workspace first: the shell runs in its Terminal panel.' };
  try {
    const terminal = await api.openTerminal({ collectionId: workspaceId, ...('repositoryId' in place ? { repositoryHome: place.repositoryId } : 'userHome' in place ? { userHome: true } : { directory: place.directory }), cols: 100, rows: 30, requestKey: crypto.randomUUID() });
    // The specific shell first, so showing the panel does not start another.
    requestTerminal(workspaceId, terminal.id);
    runAction('terminal.open');
    if (!command) { showMenuNotice(`Shell in ${place.label}`); return { terminalId: terminal.id, error: '' }; }
    void typeAndWatch(api, workspaceId, terminal.id, command, place.label, status => onEnd?.(status)).catch(() => {});
    return { terminalId: terminal.id, error: '' };
  } catch (error) {
    const why = reason(error);
    showMenuNotice(`Could not open a shell in ${place.label}: ${why}`);
    return { error: why };
  }
}
