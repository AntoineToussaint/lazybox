import type { PermissionResult, PermissionUpdate } from '@anthropic-ai/claude-agent-sdk';
import { randomUUID } from 'node:crypto';

/**
 * "Always allow" for Claude's approvals.
 *
 * Where Claude suggests permission updates (`suggestions` in canUseTool), the
 * choices are those updates, at the scope the person picks: this conversation
 * (destination `session`) or this workspace (`localSettings`, the worktree's
 * .claude/settings.local.json). Claude then enforces them itself.
 *
 * Lazybox's "ask" permission adds a tool-wide `ask` rule for every command
 * tool (COMMAND_TOOL_NAMES: Bash, Monitor, PowerShell and the managed command
 * tool), so every command asks. That rule outranks allow rules and Claude
 * offers no suggestions under it (observed with the CLI 2.1.325: no
 * `suggestions`, and a session allow rule did not stop the next ask). For
 * those tools the worker keeps the grants itself, and matches them
 * conservatively: an exact command, or a prefix of plain words on a command
 * with no shell operators (PowerShell: exact commands only). Anything else
 * asks again.
 *
 * Claude's Monitor tool runs a shell command too. Since SDK 0.3.325 Claude
 * suggests rules for Bash and Monitor only when saving them would allow that
 * command, so either can ask with no suggestions: the worker then offers the
 * exact command and its plain prefix, never a tool-wide grant, which would
 * allow every command.
 *
 * Every choice for this conversation is a grant the worker holds, never a
 * rule handed to Claude's session: Claude forgets its session rules when its
 * process ends, and has no way to take one back. The worker reports each
 * grant (`permission_grant`); the Runner keeps it with the conversation,
 * opens the next worker with it (a restart, a resume) and asks this one to
 * drop it when the person removes it. A grant made of Claude's suggestions
 * covers a later request whose own suggestions it already holds: Claude
 * would propose nothing new, so nothing new is allowed.
 */
export type Scope = 'conversation' | 'workspace';
/** What the client renders (the request's `always` list) and sends back by id. */
export type AlwaysChoice = { id: string; scope: Scope | 'prefix'; label: string; detail?: string };
/** What a grant covers, for one tool: an exact command, its leading words, Claude's suggested updates, or every request. */
export type GrantRule = { tool: string; exact?: string; prefix?: string[]; updates?: PermissionUpdate[]; all?: true };
/** A grant the worker holds for this conversation, as the Runner keeps it. */
export type Grant = GrantRule & { id: string; label: string };
type Choice = AlwaysChoice & { updates?: PermissionUpdate[]; grant?: GrantRule };

const SHELL_OPERATORS = /[;&|<>`$()\n\\]/;
/**
 * Tools whose request runs a command: a grant for every request would allow
 * any command, and Lazybox's "ask" permission makes each of them ask
 * (options.ts). PowerShell is Claude's built-in shell tool on Windows (SDK
 * 0.3.330 lists it among its built-in tools).
 */
export const COMMAND_TOOL_NAMES = ['Bash', 'Monitor', 'PowerShell', 'mcp__lazybox__command'] as const;
const COMMAND_TOOLS = new Set<string>(COMMAND_TOOL_NAMES);
const words = (command: string) => command.trim().split(/\s+/).filter(Boolean);
/**
 * Whether a prefix grant is safe for the tool's commands. SHELL_OPERATORS is
 * a POSIX shell's; PowerShell has a grammar of its own, so it gets exact
 * commands only.
 */
const prefixes = (tool: string) => tool !== 'PowerShell';

/** The command a request runs, as one line, for Bash, Monitor, PowerShell and the managed command tool. */
export function commandOf(tool: string, input: Record<string, unknown>): string {
  if ((tool === 'Bash' || tool === 'Monitor' || tool === 'PowerShell') && typeof input.command === 'string') return input.command.trim();
  if (tool === 'mcp__lazybox__command' && input.action === 'start' && Array.isArray(input.argv) && input.argv.every(a => typeof a === 'string')) return (input.argv as string[]).join(' ');
  return '';
}

/** The leading words a prefix grant would keep: `npm run`, `git status`, `ls`. */
export function commandPrefix(command: string): string[] {
  if (!command || SHELL_OPERATORS.test(command)) return [];
  const [first, second] = words(command);
  if (!first || /[*?'"=]/.test(first)) return [];
  return second && /^[a-z][\w:.-]*$/i.test(second) ? [first, second] : [first];
}

/** The allow-type updates of Claude's suggestions: what an "always" would add. */
const allowing = (suggestions: readonly PermissionUpdate[] = []) => suggestions.filter(u => !(u.type === 'addRules' || u.type === 'replaceRules') || u.behavior === 'allow');

/** Each thing a set of updates allows, as one comparable word. */
function atoms(updates: readonly PermissionUpdate[]): string[] {
  const out: string[] = [];
  for (const u of updates) {
    if (u.type === 'addRules' || u.type === 'replaceRules') out.push(...u.rules.map(r => `rule\0${u.behavior}\0${r.toolName}\0${r.ruleContent ?? ''}`));
    else if (u.type === 'addDirectories') out.push(...u.directories.map(d => `dir\0${d}`));
    else if (u.type === 'setMode') out.push(`mode\0${u.mode}`);
    else out.push(`other\0${JSON.stringify({ ...u, destination: undefined })}`);
  }
  return out;
}

/** Whether a grant the worker holds covers this request; `suggestions` are Claude's for it. */
export function covers(grant: GrantRule, tool: string, input: Record<string, unknown>, suggestions?: readonly PermissionUpdate[]): boolean {
  if (grant.tool !== tool) return false;
  if (grant.all) return !COMMAND_TOOLS.has(tool);
  if (grant.updates) {
    const want = atoms(allowing(suggestions));
    const have = new Set(atoms(grant.updates));
    return want.length > 0 && want.every(atom => have.has(atom));
  }
  const command = commandOf(tool, input);
  if (!command) return false;
  if (grant.exact !== undefined) return grant.exact === command;
  if (!grant.prefix?.length || !prefixes(tool) || SHELL_OPERATORS.test(command)) return false;
  const have = words(command);
  return grant.prefix.every((word, i) => have[i] === word);
}

const ruleText = (rule: { toolName: string; ruleContent?: string }) => {
  const content = rule.ruleContent?.trim();
  if (!content) return rule.toolName.startsWith('mcp__') ? `every ${rule.toolName.replace(/^mcp__(.+?)__/, '$1 · ')} call` : `every ${rule.toolName} request`;
  if (rule.toolName === 'Bash') {
    const prefix = content.replace(/(:\*|\s\*)$/, '');
    return prefix === content ? `\`${content}\`` : `commands starting with \`${prefix}\``;
  }
  if (rule.toolName === 'WebFetch' && content.startsWith('domain:')) return `fetches from ${content.slice(7)}`;
  if (['Read', 'Edit', 'Write'].includes(rule.toolName)) return `${rule.toolName === 'Read' ? 'reading' : 'editing'} ${content.replace(/^\/\//, '/').replace(/\/\*\*$/, '/')}`;
  return `${rule.toolName}(${content})`;
};

/** What a set of permission updates stops asking for, in words. */
export function describeUpdates(updates: readonly PermissionUpdate[]): string {
  const parts: string[] = [];
  for (const update of updates) {
    if ((update.type === 'addRules' || update.type === 'replaceRules') && update.behavior === 'allow') parts.push(...update.rules.map(ruleText));
    else if (update.type === 'addDirectories') parts.push(`access to ${update.directories.join(', ')}`);
    else if (update.type === 'setMode' && update.mode === 'acceptEdits') parts.push('file edits');
    else if (update.type === 'setMode') parts.push(`${update.mode} mode`);
  }
  return [...new Set(parts)].join(' and ');
}

const persistable = (u: PermissionUpdate) => u.type === 'addRules' || u.type === 'replaceRules' || u.type === 'addDirectories';
const at = (updates: readonly PermissionUpdate[], destination: PermissionUpdate['destination']) => updates.map(u => ({ ...u, destination }));

/**
 * The "always allow" choices for a request, narrowest first. None for the
 * settings tool (every change is confirmed), when Claude says the rule would
 * grant more than this ask, or where there is nothing safe to remember.
 */
export function alwaysChoices(tool: string, input: Record<string, unknown>, context: { suggestions?: PermissionUpdate[]; suppressAlwaysAllowRule?: boolean; settingsTool?: boolean }): Choice[] {
  if (context.settingsTool || context.suppressAlwaysAllowRule) return [];
  const suggestions = allowing(context.suggestions);
  if (suggestions.length) {
    const what = describeUpdates(suggestions);
    if (!what) return [];
    const out: Choice[] = [{ id: 'conversation', scope: 'conversation', label: what, detail: 'Claude stops asking for the rest of this conversation, also after it restarts.', grant: { tool, updates: suggestions.map(({ destination: _, ...u }) => u as PermissionUpdate) } }];
    // A mode is never written to settings; rules and directories may be.
    const lasting = suggestions.filter(persistable);
    if (lasting.length) out.push({ id: 'workspace', scope: 'workspace', label: describeUpdates(lasting), detail: 'Saved to this workspace’s .claude/settings.local.json; later conversations here follow it too.', updates: at(lasting, 'localSettings') });
    return out;
  }
  const command = commandOf(tool, input);
  if (command) {
    const out: Choice[] = [];
    const prefix = prefixes(tool) ? commandPrefix(command) : [];
    if (prefix.length && prefix.join(' ') !== command) out.push({ id: 'prefix', scope: 'prefix', label: `commands starting with \`${prefix.join(' ')}\``, detail: 'For the rest of this conversation. Commands that chain, pipe or redirect still ask.', grant: { tool, prefix } });
    out.push({ id: 'command', scope: 'conversation', label: `\`${command.length > 80 ? command.slice(0, 79) + '…' : command}\``, detail: 'This exact command, for the rest of this conversation.', grant: { tool, exact: command } });
    return out;
  }
  // A command tool without suggestions is covered above (a Monitor of a
  // WebSocket has no command); a tool-wide rule for it would allow every command.
  if (COMMAND_TOOLS.has(tool)) return [];
  return [{ id: 'conversation', scope: 'conversation', label: ruleText({ toolName: tool }), detail: 'Claude stops asking for the rest of this conversation, also after it restarts.', grant: { tool, all: true } }];
}

/** The choices as the client sees them, without the native updates. */
export const offered = (choices: readonly Choice[]): AlwaysChoice[] => choices.map(({ id, scope, label, detail }) => ({ id, scope, label, detail }));

export type Decision = { decision: string; always?: string; reason?: string };

/**
 * Claude's result for the person's decision, and a grant for the worker to
 * keep when the choice is for this conversation.
 */
export function decide(tool: string, input: Record<string, unknown>, choices: readonly Choice[], d: Decision, newId: () => string = randomUUID): { result: PermissionResult; grant?: Grant } {
  if (d.decision !== 'accept') {
    const reason = d.reason?.trim();
    return { result: { behavior: 'deny', message: reason ? `The user declined this ${tool} request and said: ${reason}` : 'Declined by operator', decisionClassification: 'user_reject' } };
  }
  if (!d.always) return { result: { behavior: 'allow', updatedInput: input, decisionClassification: 'user_temporary' } };
  const choice = choices.find(c => c.id === d.always);
  if (!choice) throw new Error(`this request does not offer "${d.always}"`);
  return { result: { behavior: 'allow', updatedInput: input, ...(choice.updates ? { updatedPermissions: choice.updates } : {}), decisionClassification: 'user_permanent' }, grant: choice.grant && { id: newId(), label: choice.label, ...choice.grant } };
}

/** A grant the Runner hands back to a new worker, if it reads as one; anything else is dropped. */
export function restoreGrant(value: unknown): Grant | undefined {
  if (!value || typeof value !== 'object') return undefined;
  const g = value as Record<string, unknown>;
  if (typeof g.id !== 'string' || !g.id || typeof g.label !== 'string' || typeof g.tool !== 'string' || !g.tool) return undefined;
  const base = { id: g.id, label: g.label, tool: g.tool };
  if (typeof g.exact === 'string' && g.exact) return { ...base, exact: g.exact };
  if (Array.isArray(g.prefix) && g.prefix.length && g.prefix.every(w => typeof w === 'string' && w)) return { ...base, prefix: g.prefix as string[] };
  if (Array.isArray(g.updates) && g.updates.length && g.updates.every(u => u && typeof u === 'object' && typeof (u as { type?: unknown }).type === 'string')) return { ...base, updates: g.updates as PermissionUpdate[] };
  // A tool-wide grant for a command tool (one an earlier worker offered for a
  // Monitor without suggestions) is dropped: the next such request asks.
  if (g.all === true && !COMMAND_TOOLS.has(g.tool)) return { ...base, all: true };
  return undefined;
}

export function runnerApprovesTerminalAction(name: string, action: unknown): boolean {
  return name === 'mcp__lazybox__command' &&
    (action === 'interactive' || action === 'interactive_read' || action === 'interactive_write');
}

/**
 * The grants the Runner opened this worker with that it holds, and the ids of
 * those it dropped. The worker reports each drop (`permission_grant_dropped`),
 * so the Runner stops keeping and listing a grant nothing enforces.
 */
export function restoreGrants(values: unknown): { grants: Grant[]; dropped: string[] } {
  const grants: Grant[] = [], dropped: string[] = [];
  for (const value of Array.isArray(values) ? values : []) {
    const grant = restoreGrant(value);
    if (grant) grants.push(grant);
    else if (value && typeof value === 'object' && typeof (value as { id?: unknown }).id === 'string' && (value as { id: string }).id) dropped.push((value as { id: string }).id);
  }
  return { grants, dropped };
}
