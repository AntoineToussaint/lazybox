/**
 * Handing work to the person's own tools (pure; `handoff.tsx` wires it):
 * the provider's CLI to continue a conversation in ("Continue in Claude
 * Code…"), and their own terminal app at a workspace's folder ("Open in
 * Terminal"). Lazybox is not a terminal emulator; these give the person the
 * one they already use, where Lazybox's mapping of a provider stops.
 */

/** A value quoted for a POSIX shell: as is when it is plainly safe, else in single quotes. */
export function shellQuote(value: string): string {
  if (/^[A-Za-z0-9_@%+=:,./-]+$/.test(value)) return value;
  return `'${value.replaceAll("'", `'\\''`)}'`;
}

// Characters a reader cannot see: C0 and C1 controls, DEL, zero-width and
// direction marks. Shown raw they hide or reorder what a command runs.
const INVISIBLE = /[\u0000-\u001f\u007f-\u009f\u00ad\u061c\u180e\u200b-\u200f\u2028-\u202e\u2060-\u206f\ufeff]/u;
const ESCAPES: Record<string, string> = { '\n': '\\n', '\t': '\\t', '\r': '\\r', '\u001b': '\\e', '\\': '\\\\', "'": "\\'" };

/**
 * An argument as the person reviews it before it runs: shellQuote, except
 * that one holding a character they could not see is written in ANSI-C
 * quoting (`$'…\e…'`), every such character as a visible escape. Pasted
 * into bash or zsh it is the same argument.
 */
export function reviewArgument(value: string): string {
  if (!INVISIBLE.test(value)) return shellQuote(value);
  let out = '';
  for (const char of value) {
    if (ESCAPES[char]) out += ESCAPES[char];
    else if (INVISIBLE.test(char)) {
      const code = char.codePointAt(0)!;
      out += code < 0x100 ? `\\x${code.toString(16).padStart(2, '0')}` : `\\u${code.toString(16).padStart(4, '0')}`;
    } else out += char;
  }
  return `$'${out}'`;
}

/** Where a command runs, said from its workspace: `lazybox/scripts`, or `workspace root`; the full directory when the root is unknown. */
export function workspaceFolder(root: string, cwd: string): string {
  const trim = (path: string) => path.replace(/\/+$/, '');
  const base = trim(root), dir = trim(cwd);
  if (root && dir === base) return 'workspace root';
  if (root && dir.startsWith(base + '/')) return dir.slice(base.length + 1);
  return cwd;
}

/** `cd <folder>`, ready to paste. */
export function cdCommand(folder: string): string { return `cd ${shellQuote(folder)}`; }

type ResumeSession = { provider?: string; providerReference?: string; workingDirectory?: string; profileId?: string; purpose?: string };
type ResumeProfile = { provider?: string; isolation?: string; label?: string };
/** The provider's own CLI, by provider id. */
const CLIS: Record<string, { name: string; resume(id: string): string }> = {
  claude: { name: 'Claude Code', resume: id => `claude --resume ${shellQuote(id)}` },
  codex: { name: 'Codex', resume: id => `codex resume ${shellQuote(id)}` },
};

export type ResumeCommand = { cli: string; command: string; unavailable?: undefined } | { cli: string; unavailable: string; command?: undefined };

/**
 * The command that continues a conversation in its provider's own CLI, in
 * its folder: `cd <folder> && claude --resume <id>` (`codex resume <id>`).
 * Undefined for an agent without such a CLI (ACP agents, the assistant).
 * Unavailable, with why, when the command would not find the conversation:
 * before the provider has named its session, or on a subscription Lazybox
 * keeps apart from the person's own CLI sign-in (its history lives in that
 * subscription's private folder, which the copied command could not name).
 */
export function resumeCommand(session: ResumeSession, profile?: ResumeProfile): ResumeCommand | undefined {
  const provider = session.provider || profile?.provider || '';
  const cli = CLIS[provider];
  if (!cli || (session.purpose && session.purpose !== 'coding')) return undefined;
  if (!session.providerReference) return { cli: cli.name, unavailable: `${cli.name} has not started this conversation yet` };
  if (profile?.isolation === 'environment') return { cli: cli.name, unavailable: `It runs on ${profile.label || 'a separate subscription'}, which Lazybox keeps apart from your own ${cli.name} sign-in` };
  const resume = cli.resume(session.providerReference);
  return { cli: cli.name, command: session.workingDirectory ? `${cdCommand(session.workingDirectory)} && ${resume}` : resume };
}

/** The terminals Lazybox opens at a folder, in the order it prefers them. */
export const TERMINAL_APPS = [
  { id: 'ghostty', title: 'Ghostty' },
  { id: 'iterm', title: 'iTerm' },
  { id: 'wezterm', title: 'WezTerm' },
  { id: 'warp', title: 'Warp' },
  { id: 'terminal', title: 'Terminal' },
] as const;
export type TerminalAppId = (typeof TERMINAL_APPS)[number]['id'];
/**
 * The person's choice of app: '' lets Lazybox pick the first installed one.
 * A custom command line is never part of it: the desktop host keeps that line
 * itself, and saves it only once the person allowed it in a native
 * confirmation, so nothing a page can write is ever run.
 */
export type TerminalChoice = { app: TerminalAppId | 'custom' | '' };
export const DEFAULT_TERMINAL: TerminalChoice = { app: '' };

export function parseTerminalChoice(raw: string | null | undefined): TerminalChoice {
  try {
    const value = JSON.parse(raw || 'null');
    const app = typeof value?.app === 'string' && (value.app === 'custom' || TERMINAL_APPS.some(t => t.id === value.app)) ? value.app : '';
    return { app };
  } catch { return DEFAULT_TERMINAL; }
}

/**
 * A command line an earlier Lazybox kept in the page's storage, offered once
 * in Settings to save through the host's confirmation; never run from there.
 */
export function legacyTerminalCommand(raw: string | null | undefined): string {
  try {
    const value = JSON.parse(raw || 'null');
    return typeof value?.command === 'string' ? value.command.trim().slice(0, 1000) : '';
  } catch { return ''; }
}

/**
 * The terminal a request goes to: the chosen one, else the first installed;
 * undefined when none is known. `custom` is the host's saved command line
 * ('' when it has none), which only names it here: the host runs its own.
 */
export function terminalFor(choice: TerminalChoice, installed: readonly string[], custom = ''): { app: TerminalAppId | 'custom' } | undefined {
  if (choice.app === 'custom') return custom.trim() ? { app: 'custom' } : undefined;
  if (choice.app) return { app: choice.app };
  const first = TERMINAL_APPS.find(t => installed.includes(t.id));
  return first ? { app: first.id } : undefined;
}

export function terminalTitle(app: string, command = ''): string {
  if (app === 'custom') return command.trim().split(/\s+/)[0]?.split('/').at(-1) || 'your terminal';
  return TERMINAL_APPS.find(t => t.id === app)?.title || 'Terminal';
}

/** The folder a terminal opens at for a path in a workspace: a folder itself, a file's folder. */
export function folderFor(root: string, path: string, directory: boolean): string {
  const base = root.replace(/\/+$/, '');
  const relative = path.replace(/^\.?\/+/, '').replace(/\/+$/, '');
  const full = relative ? `${base}/${relative}` : base;
  if (directory) return full;
  const slash = full.lastIndexOf('/');
  return slash > 0 ? full.slice(0, slash) : base;
}
