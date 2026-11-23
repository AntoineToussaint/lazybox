/**
 * The agents a conversation can run on, as people know them: Claude and
 * Codex (native adapters), and any ACP agent (provider "acp:<id>", ACP.md)
 * such as Gemini CLI or Goose. Pure, so every surface names and draws an
 * agent the same way.
 */
import type { ProviderAvailability, ProviderStatus } from '@lazybox/protocol';

export const ACP_PREFIX = 'acp:';
export const isAcp = (provider?: string): boolean => !!provider && provider.startsWith(ACP_PREFIX) && provider.length > ACP_PREFIX.length;
export const acpId = (provider: string) => isAcp(provider) ? provider.slice(ACP_PREFIX.length) : '';

/** Built-in ACP agents by id: their names and icons, before the Runner says. */
const BUILTINS: Record<string, { title: string; icon: string }> = {
  antigravity: { title: 'Google Antigravity', icon: 'agent-gemini' },
  hermes: { title: 'Hermes Agent', icon: 'agent' },
  gemini: { title: 'Gemini CLI', icon: 'agent-gemini' },
  goose: { title: 'Goose', icon: 'agent-goose' },
  opencode: { title: 'OpenCode', icon: 'agent-opencode' },
  qwen: { title: 'Qwen Code', icon: 'agent-qwen' },
  copilot: { title: 'GitHub Copilot', icon: 'agent-copilot' },
  cursor: { title: 'Cursor Agent', icon: 'agent-cursor' },
  kiro: { title: 'Kiro CLI', icon: 'agent-kiro' },
  cline: { title: 'Cline', icon: 'agent-cline' },
  auggie: { title: 'Auggie', icon: 'agent-auggie' },
};

type ProfileLike = { provider: string; label?: string };

/**
 * An agent's name: "Claude", "Codex", "Gemini CLI", a custom agent's name
 * (its profile's label), else `fallback`.
 */
export function agentTitle(provider: string | undefined, profiles: readonly ProfileLike[] = [], fallback = 'Agent'): string {
  if (provider === 'claude') return 'Claude';
  if (provider === 'codex') return 'Codex';
  if (!provider || !isAcp(provider)) return fallback;
  const label = profiles.find(p => p.provider === provider && p.label)?.label;
  if (label) return label;
  const id = acpId(provider);
  return BUILTINS[id]?.title || id.split('-').map(w => w.charAt(0).toUpperCase() + w.slice(1)).join(' ');
}

/** The icon an agent is drawn with (Icons.tsx). */
export function agentIcon(provider: string | undefined, status?: Pick<ProviderStatus, 'acp'>): string {
  if (provider === 'claude') return 'agent-claude';
  if (provider === 'codex') return 'agent-codex';
  if (status?.acp?.icon) return status.acp.icon;
  return BUILTINS[acpId(provider || '')]?.icon || 'agent';
}

/** One agent the new-conversation picker offers. */
export type AgentChoice = {
  provider: string;
  title: string;
  icon: string;
  /** Can start now: installed, with a profile. */
  ready: boolean;
  /** "2.1.311", when known. */
  version: string;
  /** One quiet line: "ACP · not signed in yet", "Not installed". */
  detail: string;
  acp: boolean;
};

const STATE_WORDS: Record<string, string> = {
  ready: '', limited: 'At its limit', sign_in: 'Needs sign-in', billing: 'Billing problem', no_subscription: 'No account', not_installed: 'Not installed', unusable: 'Cannot run',
};

/**
 * What the picker lists: Claude and Codex always (their own forms say what is
 * missing), then every ACP agent the Runner can start (installed, or added by
 * the person). Agents that are not installed are left to Settings → Agents,
 * which says how to install them.
 */
export function agentChoices(profiles: readonly ProfileLike[], report?: Pick<ProviderAvailability, 'providers'>): AgentChoice[] {
  const statuses = new Map((report?.providers || []).map(p => [p.provider, p]));
  const native = ['claude', 'codex'].map(provider => {
    const status = statuses.get(provider);
    return { provider, title: agentTitle(provider), icon: agentIcon(provider), ready: profiles.some(p => p.provider === provider), version: status?.version || '', detail: status ? STATE_WORDS[status.state] ?? '' : '', acp: false };
  });
  const seen = new Set<string>();
  const acp: AgentChoice[] = [];
  for (const status of report?.providers || []) {
    if (!isAcp(status.provider) || !profiles.some(p => p.provider === status.provider)) continue;
    seen.add(status.provider);
    acp.push({ provider: status.provider, title: status.title || agentTitle(status.provider, profiles), icon: agentIcon(status.provider, status), ready: status.installed, version: status.version, detail: status.installed ? (STATE_WORDS[status.state] ?? '') : status.error || 'Not installed', acp: true });
  }
  // Without a report (an older Runner, or still loading), the profiles say which agents exist.
  for (const p of profiles) {
    if (!isAcp(p.provider) || seen.has(p.provider)) continue;
    seen.add(p.provider);
    acp.push({ provider: p.provider, title: agentTitle(p.provider, profiles), icon: agentIcon(p.provider), ready: true, version: '', detail: '', acp: true });
  }
  return [...native, ...acp];
}

/** "gemini --acp": how an ACP agent is started, for Settings. */
export function commandLine(status: Pick<ProviderStatus, 'acp'>): string {
  const a = status.acp;
  if (!a) return '';
  return [a.command, ...a.args].map(part => /[\s"'$`\\]/.test(part) ? JSON.stringify(part) : part).join(' ');
}

/** Splits a command line typed in Settings into the command and its arguments; quotes group words. */
export function parseCommandLine(line: string): { command: string; args: string[] } {
  const words: string[] = [];
  let current = '', quote = '', escaped = false, started = false;
  for (const ch of line.trim()) {
    if (escaped) { current += ch; escaped = false; continue; }
    if (ch === '\\' && quote !== "'") { escaped = true; started = true; continue; }
    if (quote) { if (ch === quote) quote = ''; else current += ch; continue; }
    if (ch === '"' || ch === "'") { quote = ch; started = true; continue; }
    if (/\s/.test(ch)) { if (started) { words.push(current); current = ''; started = false; } continue; }
    current += ch; started = true;
  }
  if (started) words.push(current);
  return { command: words[0] || '', args: words.slice(1) };
}

/** "MY_KEY=value" lines as environment entries; blank lines and comments are skipped. */
export function parseEnvLines(text: string): { entries: { name: string; value: string }[]; error: string } {
  const entries: { name: string; value: string }[] = [];
  for (const [index, raw] of text.split('\n').entries()) {
    const line = raw.trim();
    if (!line || line.startsWith('#')) continue;
    const eq = line.indexOf('=');
    const name = (eq < 0 ? line : line.slice(0, eq)).trim();
    if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(name)) return { entries, error: `Line ${index + 1}: “${name}” is not an environment variable name.` };
    entries.push({ name, value: eq < 0 ? '' : line.slice(eq + 1) });
  }
  return { entries, error: '' };
}

/** What people call the account an agent signs in with, by the agent that owns it. */
const ACCOUNT_FAMILIES: Record<string, string> = { gemini: 'Google' };

export type SharedCredentialGroup = { title: string; source: ProviderStatus; agents: ProviderStatus[] };

/**
 * Settings → Subscriptions: each agent other agents share credentials with
 * (Antigravity uses Gemini CLI's API key), with those agents, shown when any
 * of them is on the Runner.
 */
export function sharedCredentialGroups(providers: ProviderStatus[]): SharedCredentialGroup[] {
  const present = (p: ProviderStatus) => p.installed || !!p.acp?.custom;
  const groups: SharedCredentialGroup[] = [];
  for (const source of providers) {
    const id = source.acp?.id;
    if (!id) continue;
    const sharers = providers.filter(p => p.acp?.sharesCredentialsWith === id);
    const agents = [source, ...sharers];
    if (sharers.length && agents.some(present)) groups.push({ title: ACCOUNT_FAMILIES[id] || source.title, source, agents });
  }
  return groups;
}

/** The names a group's agents share (GEMINI_API_KEY): the only names that are its key. */
export function sharedKeyNames(group: SharedCredentialGroup): string[] {
  return [...new Set(group.agents.flatMap(a => a.acp?.sharedEnvNames || []))].sort();
}

/** "Uses Gemini CLI’s GEMINI_API_KEY": where an agent's API key comes from, by name only. */
export function keySource(agent: ProviderStatus, source: ProviderStatus, keys: string[]): string {
  const inherited = (agent.acp?.inheritedEnvNames || []).filter(name => keys.includes(name));
  if (inherited.length) return `Uses ${source.title}’s ${inherited.join(', ')}`;
  const own = (agent.acp?.envNames || []).filter(name => keys.includes(name));
  if (own.length) return `Its own ${own.join(', ')}`;
  return agent === source ? 'No API key set: signs in with its own Google account' : `No API key: set one on ${source.title}, or sign in with its own Google account`;
}
