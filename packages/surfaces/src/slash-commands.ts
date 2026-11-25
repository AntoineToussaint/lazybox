import { fuzzyMatch } from './fuzzy';
import { modelName } from './conversation-model';
import { agentTitle, isAcp } from './providers';

/**
 * Slash commands in the message box: `/` at the very start of the box opens a
 * menu of what can be run — Lazybox's own commands (they do the real thing,
 * never send text), the agent's own slash commands (sent as the prompt, the
 * provider executes them) and the snippets (inserted). This module is the pure
 * part: parsing what is typed, filtering and ranking, argument choices and the
 * rule for what Enter does. SlashMenu.tsx draws it; useSlashCommands.tsx runs it.
 */
export type SlashGroup = 'lazybox' | 'provider' | 'snippet';
export const SLASH_GROUPS: readonly SlashGroup[] = ['lazybox', 'provider', 'snippet'];

/** One value an argument can take (a model, a permission mode), as it is listed. */
export type SlashChoice = { value: string; label: string; detail?: string; current?: boolean; disabled?: string; keywords?: string; effort?: string };
export type SlashCommand = {
  /** Unique across groups: `lazybox:model`, `provider:review-pr`, `snippet:review`. */
  key: string;
  /** Without the slash. */
  name: string;
  group: SlashGroup;
  description: string;
  /** What follows the name, as the provider or Lazybox writes it: `<name>`, `quiet|verbose`. */
  argumentHint?: string;
  icon: string;
  aliases?: string[];
  /** Why it cannot run now. It is still listed, so the person learns it exists. */
  disabled?: string;
  /**
   * Lazybox's command does nothing for this provider (`/compact` for an ACP
   * agent): the agent's own command of the same name, if it has one, takes its
   * place in the menu and on Enter.
   */
  unsupported?: boolean;
  /** The argument is picked from these as it is typed. */
  choices?: SlashChoice[];
  /** A free argument is required (`/rename <title>`): choosing it completes `/name ` first. */
  needsArgument?: boolean;
  /** A free argument is accepted besides the choices (a model id not in the list). */
  freeArgument?: boolean;
  /** A snippet's body, inserted when chosen. */
  body?: string;
  /** A short tag after the name: "Next" for a snippet the last one chains to. */
  badge?: string;
  /** Extra words the filter matches (the action registry's keywords). */
  keywords?: string;
};

// ---- What is typed ------------------------------------------------------

export type SlashInput = { name: string; args: string; /** Something (a space) follows the name. */ completed: boolean };
/**
 * The command at the start of the message box: `/name` and what follows it.
 * Null when the box does not start with one — including a path such as
 * `/usr/local/bin`, whose first word holds another slash.
 */
export function parseSlash(text: string): SlashInput | null {
  const match = /^\/([^\s/]*)(?:(\s)([\s\S]*))?$/.exec(text);
  if (!match) return null;
  return { name: match[1], args: match[3] ?? '', completed: match[2] !== undefined };
}
/** A first word that reads as a command, not a path or a stray slash: `/compact`, `/review-pr`, `/plugin:tool`. */
export const COMMAND_NAME = /^[A-Za-z][\w:.-]*$/;

/** Every name a command answers to, lowercased. */
const names = (command: SlashCommand) => [command.name, ...(command.aliases || [])].map(name => name.toLowerCase());
/** The command a name means: Lazybox's own first, then the agent's, then a snippet. */
export function findCommand(commands: readonly SlashCommand[], name: string): SlashCommand | undefined {
  const lower = name.toLowerCase();
  for (const group of SLASH_GROUPS) {
    const found = commands.find(command => command.group === group && names(command).includes(lower));
    if (found) return found;
  }
  return undefined;
}

// ---- Filtering and ranking ----------------------------------------------

export type RankedCommand = { command: SlashCommand; score: number; positions: number[] };
/**
 * The commands a typed name matches, grouped (Lazybox · the agent · snippets)
 * and best first within each group. Frequently used name prefixes win, then an exact name, a
 * prefix, then letters in order; aliases count almost as much as the name,
 * and the description or keywords much less (`/tokens` finds `/cost`).
 * `best` is the index of the strongest match overall: the one Enter takes.
 */
type CommandUsage = Readonly<Record<string, { count: number }>>;
export function rankCommands(query: string, commands: readonly SlashCommand[], usage: CommandUsage = {}): { items: RankedCommand[]; best: number } {
  const q = query.toLowerCase();
  const scored: RankedCommand[] = [];
  commands.forEach((command, order) => {
    if (!q) { scored.push({ command, score: -order, positions: [] }); return; }
    const main = fuzzyMatch(q, command.name);
    let score = main ? main.score + (command.name.toLowerCase() === q ? 5000 : command.name.toLowerCase().startsWith(q) ? 1000 : 0) : -Infinity;
    for (const alias of command.aliases || []) {
      const match = fuzzyMatch(q, alias);
      if (match) score = Math.max(score, match.score * 0.9 + (alias.toLowerCase() === q ? 4500 : 0));
    }
    // Words of the description or keywords: only a whole word start, so a stray letter run does not match.
    if (score === -Infinity && q.length >= 3) {
      const words = `${command.description} ${command.keywords || ''}`.toLowerCase().split(/[^a-z0-9]+/);
      if (words.some(word => word.startsWith(q))) score = 100 - command.name.length;
    }
    if (score > -Infinity) scored.push({ command, score, positions: main && score >= main.score ? main.positions : [] });
  });
  const groupIndex = (command: SlashCommand) => SLASH_GROUPS.indexOf(command.group);
  const frequency = (item: RankedCommand) => q && item.command.name.toLowerCase().startsWith(q) ? usage[item.command.key]?.count || 0 : 0;
  const compare = (a: RankedCommand, b: RankedCommand) => frequency(b) - frequency(a) || b.score - a.score;
  const items = scored.sort((a, b) => groupIndex(a.command) - groupIndex(b.command) || compare(a, b));
  let best = items.length ? 0 : -1;
  if (q) items.forEach((item, i) => { if (!item.command.disabled && (best < 0 || items[best].command.disabled || compare(item, items[best]) < 0)) best = i; });
  else best = items.findIndex(item => !item.command.disabled);
  return { items, best };
}

/** The choices an argument matches, the current one first when nothing is typed. */
export function rankChoices(query: string, choices: readonly SlashChoice[]): { items: SlashChoice[]; best: number } {
  const q = query.trim().toLowerCase();
  if (!q) {
    const best = choices.findIndex(choice => !choice.disabled && !choice.current);
    return { items: [...choices], best: best >= 0 ? best : choices.findIndex(choice => !choice.disabled) };
  }
  const scored = choices.flatMap(choice => {
    const exact = choice.value.toLowerCase() === q || choice.label.toLowerCase() === q;
    const match = fuzzyMatch(q, choice.label) || fuzzyMatch(q, choice.value);
    const extra = !match && choice.keywords && choice.keywords.toLowerCase().split(/\s+/).some(word => word.startsWith(q));
    return match || extra ? [{ choice, score: (match?.score ?? 50) + (exact ? 5000 : 0) }] : [];
  }).sort((a, b) => b.score - a.score);
  const items = scored.map(item => item.choice);
  return { items, best: items.findIndex(choice => !choice.disabled) };
}

// ---- The menu -----------------------------------------------------------

export type SlashMenuState =
  /** Typing a name: the commands it matches. */
  | { kind: 'commands'; query: string; items: RankedCommand[]; best: number }
  /** After a command with choices: the choices the argument matches. */
  | { kind: 'choices'; command: SlashCommand; query: string; items: SlashChoice[]; best: number }
  /** After any other command: what it takes. */
  | { kind: 'hint'; command: SlashCommand; args: string };

const modelOrder = new Intl.Collator('en', { numeric: true, sensitivity: 'base' });

/** Keep the current model visible and sort the other names independently of catalog order. */
export function orderedModelChoices(choices: readonly SlashChoice[]): SlashChoice[] {
  return [...choices].sort((a, b) =>
    Number(!!b.current) - Number(!!a.current) || Number(b.value === DEFAULT_MODEL) - Number(a.value === DEFAULT_MODEL) || modelOrder.compare(a.label, b.label) ||
    modelOrder.compare(a.value, b.value) || (a.value < b.value ? -1 : a.value > b.value ? 1 : 0),
  );
}

/**
 * What the menu shows for the message box's text, or null for no menu: the
 * box does not start with a command, holds more than one line, the person
 * closed the menu (Esc), or nothing matches — a message that merely starts
 * with a slash is never held up by an empty menu.
 */
export function slashMenu(text: string, commands: readonly SlashCommand[], dismissed = false, usage: CommandUsage = {}): SlashMenuState | null {
  if (dismissed || text.includes('\n')) return null;
  const input = parseSlash(text);
  if (!input) return null;
  if (!input.completed) {
    if (input.name && !/^[\w:.-]+$/.test(input.name)) return null;
    const { items, best } = rankCommands(input.name, commands, usage);
    return items.length ? { kind: 'commands', query: input.name, items, best } : null;
  }
  const command = findCommand(commands, input.name);
  if (!command || command.group === 'snippet') return null;
  if (command.choices?.length) {
    const choices = command.key === 'lazybox:model' ? orderedModelChoices(command.choices) : command.choices;
    const { items, best } = rankChoices(input.args, choices);
    if (items.length) return { kind: 'choices', command, query: input.args, items, best };
  }
  return command.argumentHint ? { kind: 'hint', command, args: input.args } : null;
}

// ---- What Enter does ----------------------------------------------------

export type SlashDecision =
  /** An ordinary message: send it. */
  | { kind: 'text' }
  /** Lazybox's own command: run it, send nothing. */
  | { kind: 'lazybox'; command: SlashCommand; args: string }
  /** The agent's own command: send the text as the prompt, the provider runs it. */
  | { kind: 'provider'; command: SlashCommand; args: string }
  /** A snippet's name: put its body in the box. */
  | { kind: 'snippet'; command: SlashCommand };
/** Run known commands locally or through the provider. Other input follows the normal message path. */
export function decideSend(text: string, commands: readonly SlashCommand[]): SlashDecision {
  const input = parseSlash(text.trimEnd());
  if (!input || !COMMAND_NAME.test(input.name)) return { kind: 'text' };
  const command = findCommand(commands, input.name);
  if (!command || command.unsupported) return { kind: 'text' };
  if (command.group === 'snippet') return { kind: 'snippet', command };
  return { kind: command.group, command, args: input.args.trim() };
}

// ---- The commands -------------------------------------------------------

/** The agent's catalog, as the worker reports it (`command_catalog`). */
export type ProviderCommand = { name: string; description: string; argumentHint: string; aliases?: string[]; builtin?: boolean };
export type ProviderModel = { value: string; displayName: string; description: string; effortLevels?: readonly string[] };
export type CommandCatalog = { commands: ProviderCommand[]; models: ProviderModel[]; terminal: string[] };
export const emptyCatalog = (): CommandCatalog => ({ commands: [], models: [], terminal: [] });
const text = (value: unknown, limit: number) => typeof value === 'string' ? value.slice(0, limit) : '';
/** A `command_catalog` payload folded into what was known: a part it lacks (models, say) is kept. */
export function mergeCatalog(previous: CommandCatalog, payload: Record<string, unknown>): CommandCatalog {
  const next = { ...previous };
  if (Array.isArray(payload.commands)) next.commands = payload.commands.flatMap((raw): ProviderCommand[] => {
    const c = raw as Record<string, unknown>;
    const name = text(c?.name, 80).replace(/^\//, '');
    if (!COMMAND_NAME.test(name)) return [];
    const aliases = Array.isArray(c.aliases) ? c.aliases.map(alias => text(alias, 80).replace(/^\//, '')).filter(alias => COMMAND_NAME.test(alias)) : undefined;
    return [{ name, description: text(c.description, 300), argumentHint: text(c.argumentHint, 120), ...(aliases?.length ? { aliases } : {}), ...(c.builtin ? { builtin: true } : {}) }];
  }).slice(0, 400);
  if (Array.isArray(payload.models)) next.models = payload.models.flatMap((raw): ProviderModel[] => {
    const m = raw as Record<string, unknown>;
    const value = text(m?.value, 200);
    const effortLevels = Array.isArray(m?.effortLevels) ? m.effortLevels.filter((level): level is string => typeof level === 'string' && /^(none|minimal|low|medium|high|xhigh|max|ultra)$/.test(level)) : [];
    return value ? [{ ...(effortLevels.length ? { effortLevels } : {}), value, displayName: text(m.displayName, 120) || value, description: text(m.description, 300) }] : [];
  }).slice(0, 100);
  if (Array.isArray(payload.terminal)) next.terminal = payload.terminal.map(name => text(name, 80).replace(/^\//, '')).filter(Boolean);
  return next;
}

export type PermissionMode = 'ask' | 'read-only' | 'yolo';
export const PERMISSION_LABELS: Record<PermissionMode, string> = { ask: 'Ask for approval', 'read-only': 'Read only', yolo: 'Full access' };
const PERMISSION_DETAILS: Record<PermissionMode, string> = { ask: 'Asks before commands and edits', 'read-only': 'Reads and plans, changes nothing', yolo: 'Runs agent tools and ordinary Lazybox commands without approval' };
/**
 * Why an agent cannot take a permission mode now (YOLO in plan mode), or ''.
 * The mode it is in is never blocked: every list shows it as the current,
 * selected choice, and choosing it again changes nothing.
 */
export const permissionBlocked = (workflow: string | undefined, mode: string) => mode === 'yolo' && workflow === 'plan' ? 'Full access is unavailable in plan mode' : '';
/** The model choices the ⋯ menu shows before "More": the first of the provider's list, and the current one wherever it is. */
export const MODEL_CHOICES_SHOWN = 4;
export function shownModelChoices<T extends { current?: boolean }>(choices: readonly T[], expanded: boolean): { shown: T[]; more: number } {
  // One hidden model is not worth a "More" button.
  if (expanded || choices.length <= MODEL_CHOICES_SHOWN + 1) return { shown: [...choices], more: 0 };
  const current = choices.findIndex(choice => choice.current);
  const shown = choices.filter((_, i) => i < MODEL_CHOICES_SHOWN || i === current);
  return { shown, more: choices.length - shown.length };
}
export const DETAIL_CHOICES: SlashChoice[] = [
  { value: 'quiet', label: 'Quiet', detail: 'Only what the agent says, its decisions and edits' },
  { value: 'verbose', label: 'Verbose', detail: 'Every tool call on its own line' },
];

/** What Lazybox's commands need to know about the conversation in front. */
export type SlashContext = {
  /** "claude", "codex" or an ACP agent ("acp:gemini", providers.ts). */
  provider: string;
  /** The agent's name: "Claude", "Gemini CLI". */
  agentName?: string;
  /** There is a live conversation (not saved, not still connecting). */
  live: boolean;
  busy: boolean;
  /** A coding conversation (assistants take no permissions or Run). */
  coding: boolean;
  /** In a workspace: review, artifacts and missions belong to one. */
  workspace: boolean;
  permission: string;
  planWorkflow: boolean;
  livePermission: boolean;
  model: string;
  effort?: string;
  liveModel: boolean;
  /** The provider can resume: a restart (model, permissions) keeps the conversation. */
  resumable: boolean;
  /** The models the running agent listed (its `command_catalog`): its subscription's. */
  models: ProviderModel[];
  /** The Runner's list for this provider (GetProviderAvailability): what the program lists, else built in. */
  providerModels?: readonly ProviderModel[];
  /** The agent has said something that can be tracked. */
  agentMessage: boolean;
  /** The agent's latest shell block, to run. */
  shellBlock: boolean;
  /** The agent's catalog has /compact. */
  compact: boolean;
  detail: string;
  /** More than one subscription of this provider: switching means something. */
  subscriptions: number;
  /** Who can review this conversation's work, the default first (review-model.ts reviewerChoices). */
  reviewers?: readonly { key: string; label: string }[];
  /** Whether the agent has the agent browser (Session.browserAccess): has, can_give, off, or "" where it does not apply. */
  browser?: string;
};

const unless = (...reasons: (string | false | undefined)[]) => reasons.find(Boolean) || undefined;
/** Lazybox's own commands, each with why it cannot run now (when it cannot). */
export function lazyboxCommands(ctx: SlashContext): SlashCommand[] {
  const notLive = !ctx.live && 'The conversation is not running';
  const agentName = ctx.agentName || agentTitle(ctx.provider);
  const restart = (live: boolean) => live ? '' : ctx.resumable ? ' (restarts the agent; the conversation continues)' : '';
  const catalog = mergeModels(ctx.providerModels || [], ctx.models);
  const models = modelChoices(catalog, ctx.model);
  const current = models.find(choice => choice.current);
  const levels = catalog.find(model => model.value === current?.value)?.effortLevels || [];
  const order = ['none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max', 'ultra'];
  const priority = (level: string) => order.includes(level) ? order.indexOf(level) : order.length;
  const efforts = [...new Set(levels)].sort((a, b) => priority(a) - priority(b) || modelOrder.compare(a, b))
    .map(level => ({ value: level, label: level, effort: level, current: level === ctx.effort, detail: current?.label }));
  const permissions: SlashChoice[] = (['ask', 'read-only', 'yolo'] as PermissionMode[]).map(mode => ({ value: mode, label: PERMISSION_LABELS[mode], detail: PERMISSION_DETAILS[mode] + restart(ctx.livePermission), current: ctx.permission === mode, disabled: permissionBlocked(ctx.planWorkflow ? 'plan' : '', mode) || undefined, keywords: mode === 'yolo' ? 'yolo bypass dangerous skip' : mode === 'ask' ? 'approve default' : 'plan readonly' }));
  return [
    { key: 'lazybox:model', name: 'model', group: 'lazybox', icon: 'sparkle', argumentHint: '<name> [effort]', description: ctx.liveModel ? `Switch ${agentName}'s model` : `Switch ${agentName}'s model${restart(false)}`, choices: models, freeArgument: true, disabled: unless(notLive), keywords: 'opus sonnet haiku gpt' },
    { key: 'lazybox:permissions', name: 'permissions', group: 'lazybox', icon: 'shield', argumentHint: 'ask|read-only|yolo', aliases: ['permission', 'yolo'], description: 'Change agent and Lazybox command permissions', choices: permissions, disabled: unless(notLive, !ctx.coding && 'Assistants are read-only') },
    { key: 'lazybox:subscription', name: 'subscription', group: 'lazybox', icon: 'card', description: 'Switch subscription…', aliases: ['account'], disabled: unless(notLive), keywords: 'account login profile limit' },
    { key: 'lazybox:browser', name: 'browser', group: 'lazybox', icon: 'globe', description: `Give ${agentName} the browser${ctx.browser === 'can_give' ? ' (restarts the agent; the conversation continues)' : ''}`,
      disabled: unless(notLive, ctx.browser === 'has' && `${agentName} has the browser`, ctx.browser === 'off' && 'The agent browser is off in Settings → Browser', !ctx.browser && 'The agent browser is not available in this conversation'),
      unsupported: !ctx.browser, keywords: 'chromium web page preview playwright' },
    { key: 'lazybox:compact', name: 'compact', group: 'lazybox', icon: 'collapse', description: 'Compact the context: summarize the conversation so far', disabled: unless(notLive, ctx.busy && 'Wait for the turn to finish', !ctx.compact && (isAcp(ctx.provider) ? `${agentName} has no /compact; see its own commands` : 'Not available until the agent reports its commands')), unsupported: !ctx.compact, keywords: 'summarize context shrink tokens' },
    { key: 'lazybox:clear', name: 'clear', group: 'lazybox', icon: 'plus', description: 'New conversation in this workspace (this one is kept)', aliases: ['new'], disabled: unless(!ctx.workspace && 'Only in a workspace'), keywords: 'reset fresh start' },
    { key: 'lazybox:pr-cost', name: 'pr-cost', group: 'lazybox', icon: 'activity', description: 'Show workspace and pull request cost estimates', keywords: 'dashboard spend' },
    { key: 'lazybox:cost', name: 'cost', group: 'lazybox', icon: 'activity', description: 'Token cost of this conversation', aliases: ['usage'], keywords: 'tokens price money spend' },
    ...((ctx.provider === 'codex' || ctx.provider === 'claude') && ctx.live ? [{ key: 'lazybox:mcp', name: 'mcp', group: 'lazybox' as const, icon: 'puzzle', description: 'List MCP servers in this conversation', disabled: unless(notLive), keywords: 'tools integrations servers' }] : []),
    ...(ctx.provider === 'codex' && ctx.live ? (['skills', 'apps', 'plugins'] as const).map(name => ({
      key: `lazybox:${name}`, name, group: 'lazybox' as const, icon: 'puzzle', description: `List Codex ${name}`,
    })) : []),
    { key: 'lazybox:diff', name: 'diff', group: 'lazybox', icon: 'branch', description: 'Open this workspace’s changes', disabled: unless(!ctx.workspace && 'Only in a workspace') },
    { key: 'lazybox:status', name: 'status', group: 'lazybox', icon: 'activity', description: 'Show conversation status and context usage' },
    { key: 'lazybox:copy', name: 'copy', group: 'lazybox', icon: 'copy', description: 'Copy the latest agent reply', freeArgument: true, disabled: unless(!ctx.agentMessage && 'No reply to copy yet') },
    ...(efforts.length || ctx.provider === 'claude' || ctx.provider === 'codex' ? [
      { key: 'lazybox:effort', name: 'effort', group: 'lazybox' as const, icon: 'sliders', description: 'Show or change the current model’s reasoning effort', choices: efforts, freeArgument: true, disabled: unless(notLive) },
    ] : []),
    { key: 'lazybox:context', name: 'context', group: 'lazybox', icon: 'activity', description: 'How full the context window is', keywords: 'tokens window memory' },
    // `/review` has the default reviewer review this conversation's changes; `/review claude` names one; `/review <focus>`
    // (or `/review codex <focus>`) adds what to look at to the brief; `/review tray` opens the tray.
    { key: 'lazybox:review', name: 'review', group: 'lazybox', icon: 'review', argumentHint: reviewHint(ctx), description: ctx.reviewers?.length ? `${ctx.reviewers[0].label} reviews this conversation’s changes, read-only` : 'Open the review tray of this workspace', freeArgument: true,
      choices: [...(ctx.reviewers || []).map((reviewer, index) => ({ value: reviewer.key, label: `Review with ${reviewer.label}`, detail: index === 0 ? 'Default reviewer · a read-only conversation; findings land in the Review tray' : 'A read-only conversation; findings land in the Review tray', keywords: reviewer.label.toLowerCase() })),
        { value: 'tray', label: 'Open the review tray', detail: 'Comments and findings of this workspace', keywords: 'comments findings diff' }],
      disabled: unless(!ctx.workspace && 'Only in a workspace'), keywords: 'diff changes comments codex claude second opinion findings' },
    { key: 'lazybox:artifact', name: 'artifact', group: 'lazybox', icon: 'artifact', description: `Track ${agentName}'s last message as an artifact`, disabled: unless(!ctx.workspace && 'Only in a workspace', !ctx.agentMessage && `${agentName} has not said anything yet`), keywords: 'document report save' },
    { key: 'lazybox:mission', name: 'mission', group: 'lazybox', icon: 'target', description: 'Open missions', aliases: ['missions'], keywords: 'tasks goals orchestrate' },
    { key: 'lazybox:orchestrate', name: 'orchestrate', group: 'lazybox', icon: 'subagent', description: `Let ${agentName} start and drive agents for a mission`, aliases: ['orchestrator'], disabled: unless(notLive, !ctx.coding && 'Assistants cannot run agents', !ctx.workspace && 'Only in a workspace'), keywords: 'spawn agents team grant mission' },
    { key: 'lazybox:run', name: 'run', group: 'lazybox', icon: 'play', description: `Open the last command block ${agentName} wrote in the terminal, ready to run`, disabled: unless(!ctx.coding && 'Assistants have no commands to run', !ctx.shellBlock && 'No command block to run yet'), keywords: 'shell bash script execute' },
    { key: 'lazybox:stop', name: 'stop', group: 'lazybox', icon: 'stop', description: `Interrupt ${agentName}`, aliases: ['interrupt'], disabled: unless(notLive, !ctx.busy && `${agentName} is not working`), keywords: 'cancel abort halt' },
    { key: 'lazybox:detail', name: 'detail', group: 'lazybox', icon: 'activity', argumentHint: 'quiet|verbose', description: 'How much tool activity this conversation shows', choices: DETAIL_CHOICES.map(choice => ({ ...choice, current: choice.value === ctx.detail })), keywords: 'activity tools noise' },
    { key: 'lazybox:rename', name: 'rename', group: 'lazybox', icon: 'pencil', argumentHint: '<title>', needsArgument: true, description: 'Rename this conversation', aliases: ['title'], disabled: unless(!ctx.live && !ctx.resumable && 'The conversation is still connecting') },
    { key: 'lazybox:help', name: 'help', group: 'lazybox', icon: 'keyboard', description: 'Keyboard shortcuts', aliases: ['shortcuts', 'keys'] },
  ];
}
function reviewHint(ctx: SlashContext): string {
  const names = [...new Set((ctx.reviewers || []).filter(r => !r.key.startsWith('profile:')).map(r => r.key))];
  return names.length ? `[${[...names, 'tray'].join('|')}] [focus…]` : '';
}
/**
 * What `/review <args>` asks for: the tray, or a review by the reviewer its
 * first word names (else the default) with the rest as its focus — so
 * `/review the retry loop` is the default reviewer, focused on the retry loop.
 */
export function reviewRequest<T extends { key: string; label: string }>(reviewers: readonly T[], args: string): { tray: true } | { tray?: false; reviewer: T | undefined; focus: string } {
  const text = args.trim(), [first = '', ...rest] = text.split(/\s+/);
  if (!text) return { reviewer: reviewers[0], focus: '' };
  if (!rest.length && (first.toLowerCase() === 'tray' || first.toLowerCase() === 'comments')) return { tray: true };
  const word = first.toLowerCase();
  const named = reviewers.find(r => r.key.toLowerCase() === word || r.label.toLowerCase() === word || `profile:${word}` === r.key.toLowerCase())
    // One word that starts a reviewer's name (`/review cla`): that reviewer.
    || (!rest.length ? reviewers.find(r => !r.key.startsWith('profile:') && r.label.toLowerCase().startsWith(word)) : undefined);
  return named ? { reviewer: named, focus: rest.join(' ') } : { reviewer: reviewers[0], focus: text };
}
/**
 * A listed model is the session's: by id, by name, or by the name its id
 * says — the agent reports "claude-opus-5-5" and lists "opus" as "Opus 5.5".
 */
export function sameModel(model: ProviderModel, current: string): boolean {
  if (!current) return false;
  const c = current.trim().toLowerCase(), said = modelName(current).toLowerCase();
  const value = model.value.toLowerCase(), name = model.displayName.toLowerCase();
  if (value === c || name === c || name === said || modelName(model.value).toLowerCase() === said) return true;
  // An alias of the current model ("opus", listed as "Opus"): resolveModel would
  // take it for this model. A name with its own version ("Fable 5.1") must match it.
  return ALIAS.test(value) && said.startsWith(`${name} `);
}
/** A family alias Claude Code takes: one word, no version ("opus", "sonnet"). */
const ALIAS = /^[a-z]+$/;

/**
 * The models `/model` and the picker list: the Runner's list for the
 * provider first (Claude's current models by id, or what Codex lists), then
 * whatever else the running agent listed — Claude Code's older models and
 * aliases, a subscription's own. A model both list (by id or by name) is
 * listed once, the first time.
 */
export function mergeModels(provider: readonly ProviderModel[], listed: readonly ProviderModel[]): ProviderModel[] {
  const out: ProviderModel[] = [];
  const seen = new Map<string, ProviderModel>();
  for (const model of [...provider, ...listed]) {
    if (!model.value) continue;
    const keys = [model.value.toLowerCase(), `name:${(model.displayName || model.value).toLowerCase()}`];
    const existing = keys.map(key => seen.get(key)).find(Boolean);
    if (existing) {
      if (model.effortLevels?.length) existing.effortLevels = model.effortLevels;
      continue;
    }
    const entry = { ...model, displayName: model.displayName || model.value, description: model.description || '' };
    keys.forEach(key => seen.set(key, entry));
    out.push(entry);
  }
  return out;
}

/** The choice standing for "the provider's default model", when the conversation names none. */
export const DEFAULT_MODEL = 'default';
const DEFAULT_MODEL_CHOICE: SlashChoice = { value: DEFAULT_MODEL, label: 'Default', detail: 'The agent\'s own default model · its name shows once the agent reports it', current: true, keywords: 'default' };

/**
 * The choices of `/model`: each model by its name, its id and description
 * after, the current one marked. A current model the list lacks (a custom
 * id) is listed first, so the list always says what runs now — but choosing
 * the current model changes nothing (useSlashCommands' switchModel).
 */
export function modelChoices(models: readonly ProviderModel[], current: string): SlashChoice[] {
  // No model named yet: the conversation runs on the provider's own default
  // (no id was asked for, and the agent has not said which). That is the
  // current choice, so the list never reads as "none of these": the agent's
  // own Default entry where it lists one (Claude Code's `default`), else one.
  if (!current.trim()) {
    const listed = models.findIndex(model => model.value.toLowerCase() === DEFAULT_MODEL);
    const choices = models.map((model, i): SlashChoice => ({ value: model.value, label: model.displayName, detail: [model.value !== model.displayName ? model.value : '', model.description].filter(Boolean).join(' · '), current: i === listed, keywords: model.value }));
    return listed < 0 ? [DEFAULT_MODEL_CHOICE, ...choices] : choices;
  }
  // Every entry that is the current model is marked (an id and its alias), so
  // none of them is offered first as a switch (rankChoices skips the current).
  let same = models.map(model => sameModel(model, current));
  const c = current.trim().toLowerCase();
  // The current model is an alias ("sonnet", after a switch by alias): the first,
  // newest model of that name, as resolveModel takes a prefix.
  if (!same.some(Boolean) && ALIAS.test(c)) {
    const first = models.findIndex(model => model.displayName.toLowerCase().startsWith(`${c} `));
    same = models.map((_, i) => i === first);
  }
  const marked = same.some(Boolean);
  const choices: SlashChoice[] = models.map((model, i) => ({ value: model.value, label: model.displayName, detail: [model.value !== model.displayName ? model.value : '', model.description].filter(Boolean).join(' · '), current: same[i], keywords: model.value }));
  if (current && !marked) {
    const label = modelName(current);
    choices.unshift({ value: current, label, detail: label === current ? 'Current model' : `${current} · Current model`, current: true, keywords: current });
  }
  return choices;
}
/**
 * What the card asks before a model switch that restarts the agent, in one
 * sentence: why it restarts (Claude: an older Lazybox started its worker;
 * another agent cannot switch live at all), when, and that the conversation
 * continues.
 */
export function modelRestartText(provider: string, agentName: string, label: string, busy: boolean, queued: boolean): string {
  const when = busy ? ' when this turn ends' : '';
  if (queued) return `Switching to ${label} when this turn ends: ${agentName} restarts and your conversation continues.`;
  return provider === 'claude'
    ? `Switch to ${label}? An older Lazybox started this conversation's agent, so switching restarts it${when} and your conversation continues.`
    : `Switch to ${label}? ${agentName} cannot change models mid-conversation, so switching restarts it${when} and your conversation continues.`;
}

/** The model a typed argument names: a listed one by id or name, else the text itself (an id the list lacks). */
export function resolveModel(args: string, models: readonly ProviderModel[]): string {
  const q = args.trim();
  if (!q) return '';
  const lower = q.toLowerCase();
  const byWord = models.filter(model => model.value.toLowerCase().split(/[-_.]/).includes(lower));
  return (models.find(m => m.value.toLowerCase() === lower || m.displayName.toLowerCase() === lower) || models.find(m => m.displayName.toLowerCase().startsWith(lower) || m.value.toLowerCase().startsWith(lower)) || (byWord.length === 1 ? byWord[0] : undefined))?.value || q;
}

/**
 * The agent's own commands, as listed by its catalog: Lazybox's command of
 * the same name stands in for it (Lazybox's `/model` switches live; the
 * agent's would wait for a terminal), unless Lazybox's does nothing for this
 * agent (`unsupported`: an ACP agent's own `/compact`), and terminal-only
 * ones are left out.
 */
export function providerCommands(catalog: CommandCatalog, lazybox: readonly SlashCommand[], disabled?: string): SlashCommand[] {
  const taken = new Set(lazybox.filter(command => !command.unsupported).flatMap(names));
  const terminal = new Set(catalog.terminal.map(name => name.toLowerCase()));
  const seen = new Set<string>();
  return catalog.commands.flatMap(command => {
    const lower = command.name.toLowerCase();
    if (taken.has(lower) || terminal.has(lower) || seen.has(lower)) return [];
    seen.add(lower);
    return [{ key: `provider:${command.name}`, name: command.name, group: 'provider' as const, icon: command.builtin ? 'sparkle' : 'notes', description: command.description || (command.builtin ? 'Built-in command' : 'Custom command'), argumentHint: command.argumentHint || undefined, aliases: command.aliases?.filter(alias => !taken.has(alias.toLowerCase())), disabled }];
  });
}

/**
 * What ⌘K does with one of Lazybox's commands: `/review` from the palette is
 * its title, "Open the review tray" (it starts no review); one that takes a
 * choice or an argument borrows the message box to ask for it; any other runs.
 */
export function paletteCommand(command: Pick<SlashCommand, 'name' | 'choices' | 'needsArgument'>): { borrow: string } | { args: string } {
  if (command.name === 'review') return { args: 'tray' };
  if (command.choices?.length || command.needsArgument) return { borrow: `/${command.name} ` };
  return { args: '' };
}
/**
 * The message box once a command has run: typed there (or chosen from its
 * menu), the `/command` gives way to the draft it set aside (empty without
 * one); from the palette, the box is the person's draft and stays as it is,
 * unless the palette had borrowed it for a command's choices. Undefined:
 * leave it.
 */
export function boxAfterCommand(from: 'box' | 'palette', setAside: string | null): string | undefined {
  if (from === 'palette' && setAside === null) return undefined;
  return setAside ?? '';
}

/**
 * Every command the menu lists: Lazybox's, the agent's, the snippets. A
 * Lazybox command that does nothing for this agent gives way to the agent's
 * own command of the same name.
 */
export function slashCommandList(lazybox: readonly SlashCommand[], provider: readonly SlashCommand[], snippets: readonly SlashCommand[]): SlashCommand[] {
  const own = new Set(provider.flatMap(names));
  return [...lazybox.filter(command => !command.unsupported || !own.has(command.name.toLowerCase())), ...provider, ...snippets];
}

/** Snippets by name; those the last one sent chains to (`next:`) come first, marked Next. */
export function snippetCommands(snippets: readonly { key: string; description: string; category: string; body: string }[], next: readonly string[] = []): SlashCommand[] {
  const rows = snippets.filter(snippet => COMMAND_NAME.test(snippet.key) || /^[\w:.-]+$/.test(snippet.key)).map(snippet => ({
    key: `snippet:${snippet.key}`, name: snippet.key, group: 'snippet' as const, icon: 'snippets', description: snippet.description || snippet.category || 'Snippet', body: snippet.body, badge: next.includes(snippet.key) ? 'Next' : undefined, keywords: snippet.category,
  }));
  return [...rows.filter(row => row.badge), ...rows.filter(row => !row.badge)];
}

// ---- The conversation's record ------------------------------------------

/** 156000 → "156k", 1200000 → "1.2M". */
export function shortTokens(n: number): string {
  if (!Number.isFinite(n) || n < 0) return '?';
  if (n < 1000) return String(Math.round(n));
  if (n < 1_000_000) return `${Math.round(n / 1000)}k`;
  return `${(n / 1_000_000).toFixed(1).replace(/\.0$/, '')}M`;
}
/**
 * The one line a command leaves in the conversation, from the event the
 * Runner or the agent recorded: "Switched model to claude-opus-5-5",
 * "Compacted context: 156k → 61k tokens". Empty for anything else.
 */
export function commandLine(type: string, text: string, status: string, data: Record<string, unknown>): string {
  if (type === 'mcp_servers' || type === 'provider_catalog') return text;
  if (type === 'model_changed' && text) return `Switched model to ${text}${status === 'restart' ? ' · the agent restarted' : status === 'next_turn' ? ' · from the next turn' : ''}`;
  if (type === 'permission_changed' && text) return `Permissions: ${PERMISSION_LABELS[text as PermissionMode] || text}${status === 'restart' ? ' · the agent restarted' : ''}`;
  if (type === 'compacted') {
    const before = Number(data.preTokens), after = Number(data.postTokens);
    const how = data.trigger === 'auto' ? 'Context compacted automatically' : 'Compacted context';
    if (Number.isFinite(before) && before > 0 && Number.isFinite(after) && after > 0) return `${how}: ${shortTokens(before)} → ${shortTokens(after)} tokens`;
    if (Number.isFinite(before) && before > 0) return `${how} from ${shortTokens(before)} tokens`;
    return how;
  }
  return '';
}
