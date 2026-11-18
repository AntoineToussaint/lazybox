/**
 * Typing a command into a shell of the Terminal panel for the person: when
 * the shell can take it, what to send, and which open shell to reuse. Pure:
 * `home-shell.ts` drives it against the Runner, `scripts/shell-input.test.ts`
 * pins it.
 *
 * A shell that is still starting has not switched the terminal to its line
 * editor yet: the terminal echoes what arrives, and the editor shows it again
 * at the prompt, so the command appears twice. Nothing is written before the
 * shell is ready:
 *
 * - zsh with Lazybox's integration says so (OSC 133;B from its line-init
 *   hook, counted by the Runner as `promptsReady`);
 * - a shell without the marks (bash, fish) is ready once its output settles;
 * - zsh whose line-init hook was replaced by the person's setup marked its
 *   precmd (`commandEnds`) but never the prompt: its output settling, longer,
 *   stands for it;
 * - and after a while the command is typed anyway, rather than never.
 */
import type { TerminalStatus } from '@lazybox/protocol';
import { placeholdersIn } from './code-run';

/** What the window saw of a shell at one moment. */
export type ShellSample = { at: number; offset: bigint; promptsReady: bigint; commandEnds: bigint; promptMarks: boolean; done: boolean };
export const sample = (status: TerminalStatus, at: number): ShellSample => ({ at, offset: status.offset, promptsReady: status.promptsReady, commandEnds: status.commandEnds, promptMarks: status.promptMarks, done: status.done });

/** A shell without marks has drawn its prompt once its output stops this long. */
export const SETTLE_MS = 700;
/** A zsh that marks its precmd but not its prompt: the person's setup replaced the line-init hook. */
export const HOOKLESS_SETTLE_MS = 1500;
/** The longest wait for a shell that says when it is ready, and for one that does not. */
export const MARKED_TIMEOUT_MS = 15_000;
export const UNMARKED_TIMEOUT_MS = 8_000;

export type Readiness = 'wait' | 'ready' | 'settled' | 'timeout' | 'gone';
/**
 * Whether a starting shell can take input, from what the window saw of it
 * since `since` (oldest first). `ready`: it said so; `settled`: its output
 * stopped; `timeout`: waited long enough; `gone`: it exited.
 */
export function shellReadiness(samples: readonly ShellSample[], since: number, now: number): Readiness {
  const last = samples.at(-1);
  if (!last) return now - since >= UNMARKED_TIMEOUT_MS ? 'timeout' : 'wait';
  if (last.done) return 'gone';
  if (last.promptsReady > 0n) return 'ready';
  // Quiet since the first sample showing the output as it is now.
  let quietFrom = last.at;
  for (let i = samples.length - 1; i >= 0 && samples[i].offset === last.offset; i--) quietFrom = samples[i].at;
  const quiet = now - quietFrom;
  if (last.offset > 0n) {
    if (!last.promptMarks && quiet >= SETTLE_MS) return 'settled';
    if (last.promptMarks && last.commandEnds > 0n && quiet >= HOOKLESS_SETTLE_MS) return 'settled';
  }
  return now - since >= (last.promptMarks ? MARKED_TIMEOUT_MS : UNMARKED_TIMEOUT_MS) ? 'timeout' : 'wait';
}

/**
 * Whether Run runs a command at once: one line with nothing left to fill in.
 * A script of several lines, or one with a placeholder (`<your-project>`),
 * is typed for the person to review, edit and run with Return.
 */
export const runsAtOnce = (command: string) => !command.includes('\n') && placeholdersIn(command).length === 0;

/** Ctrl+U: clears what is on the prompt's line before typing into a shell that was already open. */
const CLEAR_LINE = '\x15';
/**
 * What to write: the command and Return when it runs, else the command alone
 * (several lines as one bracketed paste, which the shell holds at the prompt).
 * Into a reused shell, the prompt's line is cleared first.
 */
export function shellInput(command: string, run: boolean, reused: boolean): string {
  const text = run ? command + '\r' : command.includes('\n') ? `\x1b[200~${command}\x1b[201~` : command;
  return reused ? CLEAR_LINE + text : text;
}

/** Where a shell starts: a repository's home checkout, the home folder, or a folder of the workspace (relative to its root; '' is the root). */
export type ShellPlace = { repositoryId: string; label: string } | { userHome: true; label: string } | { directory: string; label: string };

/** Whether a terminal was started in `place`. */
export function startedIn(terminal: TerminalStatus, place: ShellPlace): boolean {
  if ('repositoryId' in place) return terminal.place === 'repository-home' && terminal.repositoryId === place.repositoryId;
  if ('userHome' in place) return terminal.place === 'user-home';
  return !terminal.place && terminal.directory === place.directory;
}

/**
 * An open shell to run a command in instead of starting another: started in
 * `place`, at a prompt there with nothing running (the Runner's `idle`), and
 * not already taken by another command the window is typing or holding at
 * its prompt. The most recent one; undefined when every shell there is busy.
 */
export function reusableTerminal(terminals: readonly TerminalStatus[], place: ShellPlace, taken: ReadonlySet<string>): TerminalStatus | undefined {
  return terminals.filter(terminal => terminal.idle && !terminal.done && !taken.has(terminal.id) && startedIn(terminal, place)).at(-1);
}
