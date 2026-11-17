import type { Extension } from '@lazybox/protocol';
import { actionDefinitions } from './actions';

/**
 * What each grant lets an extension do, as the approval sheet says it
 * (EXTENSIONS.md, "Grants"). `tone` is how much it matters: reading is
 * quiet, acting is marked, sending as the person is the strongest.
 */
export type GrantTone = 'read' | 'act' | 'send';
export type GrantText = { grant: string; icon: string; title: string; detail: string; tone: GrantTone };

const fixed: Record<string, Omit<GrantText, 'grant'>> = {
  'read:workspaces': { icon: 'collections', title: 'See your workspaces', detail: 'Their names and repositories', tone: 'read' },
  'read:sessions': { icon: 'history', title: 'See your conversations', detail: 'Titles, workspace, provider, model and status — not their messages', tone: 'read' },
  'read:attention': { icon: 'inbox', title: 'See what needs you', detail: 'The Inbox: which conversations wait for you, with the question or the agent’s last line', tone: 'read' },
  'read:commands': { icon: 'output', title: 'See a conversation’s commands', detail: 'What ran and how it ended — not the output', tone: 'read' },
  'read:missions': { icon: 'target', title: 'See your missions', detail: 'Missions, their tasks and status', tone: 'read' },
  'read:usage': { icon: 'chart', title: 'See token use and cost', detail: 'Tokens and dollars by workspace, conversation and model, hour by hour', tone: 'read' },
  'show:conversations': { icon: 'eye', title: 'Show a conversation', detail: 'Bring a conversation or a workspace forward in the window', tone: 'act' },
  'send:messages': { icon: 'send', title: 'Send messages as you', detail: 'Write to any of your conversations; the agent acts on them as if you had', tone: 'send' },
  // Account policy grants (EXTENSIONS.md, "Account policy"): the Runner acts on them, no panel does.
  'switch:claude': { icon: 'card', title: 'Move Claude conversations to another of your Claude accounts', detail: 'When one reaches its subscription’s limit, it continues on another account you signed in', tone: 'act' },
  'switch:codex': { icon: 'card', title: 'Move Codex conversations to another of your Codex accounts', detail: 'When one reaches its subscription’s limit, it continues on another account you signed in', tone: 'act' },
};

/** Anthropic's terms for Claude Code and the Agent SDK (CLAUDE_COMPLIANCE.md). */
export const CLAUDE_TERMS_URL = 'https://code.claude.com/docs/en/legal-and-compliance';

/** The providers an extension's account policy would switch, from what it asks for. */
export const switchedProviders = (ext: Pick<Extension, 'grants'>) => ext.grants.filter(g => g.startsWith('switch:')).map(g => g.slice('switch:'.length));

/** The warning shown wherever an account policy covers Claude, in one sentence. */
export const CLAUDE_SWITCHING_WARNING = 'Anthropic’s terms may not allow moving between Claude subscriptions to keep working past a usage limit. Read them before allowing this for Claude.';

/** One grant in plain words. */
export function describeGrant(grant: string): GrantText {
  if (fixed[grant]) return { grant, ...fixed[grant] };
  if (grant.startsWith('do:')) {
    const id = grant.slice(3), action = actionDefinitions.find(a => a.id === id);
    return { grant, icon: action?.icon || 'play', title: `Run “${action?.title || id}”`, detail: 'An action that only shows, focuses or arranges the window', tone: 'act' };
  }
  return { grant, icon: 'alert', title: grant, detail: 'Not a grant this Lazybox knows', tone: 'send' };
}

/** A network origin in plain words. */
export function describeHost(host: string): { host: string; title: string; detail: string } {
  const url = (() => { try { return new URL(host.replace('*.', 'wildcard.')); } catch { return undefined; } })();
  const local = url && (url.hostname === 'localhost' || url.hostname === '127.0.0.1');
  if (host.includes('*.')) return { host, title: host.replace(/^https:\/\/\*\./, ''), detail: 'Any subdomain of it, over https' };
  return { host, title: url ? url.host : host, detail: local ? 'A service on this computer' : 'Over https' };
}

/** What no extension can ever do, whatever it asks for. */
export const extensionNever = [
  'Read your files, terminals or a conversation’s messages',
  'Run commands, start or stop agents, or approve anything',
  'Change a setting or see your credentials',
  'Reach any site you did not allow, or leave its panel',
];

/** What an update asks for beyond what was approved. */
export function widening(ext: Pick<Extension, 'grants' | 'hosts' | 'approvedGrants' | 'approvedHosts'>) {
  return { grants: ext.grants.filter(g => !ext.approvedGrants.includes(g)), hosts: ext.hosts.filter(h => !ext.approvedHosts.includes(h)) };
}

export const extensionStateLabel: Record<string, string> = { new: 'Needs your approval', enabled: 'On', disabled: 'Off', review: 'Asks for more', broken: 'Does not load' };
