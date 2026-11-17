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
/** The person's choice: '' lets Lazybox pick the first installed one. */
export type TerminalChoice = { app: TerminalAppId | 'custom' | ''; command: string };
export const DEFAULT_TERMINAL: TerminalChoice = { app: '', command: '' };

export function parseTerminalChoice(raw: string | null | undefined): TerminalChoice {
  try {
    const value = JSON.parse(raw || 'null');
    const app = typeof value?.app === 'string' && (value.app === 'custom' || TERMINAL_APPS.some(t => t.id === value.app)) ? value.app : '';
    const command = typeof value?.command === 'string' ? value.command.slice(0, 1000) : '';
    return { app, command };
  } catch { return DEFAULT_TERMINAL; }
}

/** The terminal a request goes to: the chosen one, else the first installed; undefined when none is known. */
export function terminalFor(choice: TerminalChoice, installed: readonly string[]): { app: TerminalAppId | 'custom'; command: string } | undefined {
  if (choice.app === 'custom') return choice.command.trim() ? { app: 'custom', command: choice.command.trim() } : undefined;
  if (choice.app) return { app: choice.app, command: '' };
  const first = TERMINAL_APPS.find(t => installed.includes(t.id));
  return first ? { app: first.id, command: '' } : undefined;
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
