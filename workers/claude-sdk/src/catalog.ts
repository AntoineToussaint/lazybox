/**
 * The agent's own slash commands and models, as Lazybox's composer lists
 * them. Claude reports them three ways: the initialize response (commands with
 * descriptions and argument hints, and the models), each turn's `init`
 * message (command names only) and `commands_changed` (a full new list). This
 * keeps the best list known and says it once per change, as one
 * `command_catalog` event the Runner journals.
 */
export type CatalogCommand = { name: string; description: string; argumentHint: string; aliases?: string[]; builtin?: boolean };
export type CatalogModel = { value: string; displayName: string; description: string; effortLevels?: string[] };
export type CatalogEvent = { type: 'command_catalog'; data: { commands: CatalogCommand[]; models: CatalogModel[]; terminal?: string[] } };

const MAX_COMMANDS = 400, MAX_MODELS = 64, NAME = 80, DESCRIPTION = 300, HINT = 120;
const text = (value: unknown, limit: number) => typeof value === 'string' ? value.replace(/\s+/g, ' ').trim().slice(0, limit) : '';
/** A command name as typed after the slash: no slash, no spaces. */
const commandName = (value: unknown) => {
  const name = text(value, NAME).replace(/^\/+/, '');
  return /^[\w][\w:.-]*$/.test(name) ? name : '';
};

function commands(list: unknown): CatalogCommand[] {
  if (!Array.isArray(list)) return [];
  const seen = new Set<string>(), out: CatalogCommand[] = [];
  for (const row of list) {
    const raw = typeof row === 'string' ? { name: row } : row && typeof row === 'object' ? row as Record<string, unknown> : undefined;
    const name = commandName(raw?.name);
    // The first row of a name is the one /name runs (a builtin is listed first by Claude).
    if (!name || seen.has(name)) continue;
    seen.add(name);
    const aliases = Array.isArray(raw?.aliases) ? raw.aliases.map(commandName).filter(Boolean).slice(0, 8) : [];
    out.push({ name, description: text(raw?.description, DESCRIPTION), argumentHint: text(raw?.argumentHint, HINT), ...(aliases.length ? { aliases } : {}), ...(raw?.builtin === true ? { builtin: true } : {}) });
    if (out.length >= MAX_COMMANDS) break;
  }
  return out;
}
function models(list: unknown): CatalogModel[] {
  if (!Array.isArray(list)) return [];
  const out: CatalogModel[] = [];
  for (const row of list) {
    if (!row || typeof row !== 'object') continue;
    const value = text((row as Record<string, unknown>).value, NAME);
    if (!value || /\s/.test(value) || out.some(m => m.value === value)) continue;
    const levels = (row as Record<string, unknown>).supportedEffortLevels;
    const effortLevels = Array.isArray(levels) ? levels.filter((level): level is string => typeof level === 'string' && /^(low|medium|high|xhigh|max)$/.test(level)) : [];
    out.push({ ...(effortLevels.length ? { effortLevels } : {}), value, displayName: text((row as Record<string, unknown>).displayName, NAME) || value, description: text((row as Record<string, unknown>).description, DESCRIPTION) });
    if (out.length >= MAX_MODELS) break;
  }
  return out;
}
const names = (list: unknown) => Array.isArray(list) ? list.map(commandName).filter(Boolean).slice(0, MAX_COMMANDS) : [];

export class CommandCatalog {
  private commands: CatalogCommand[] = [];
  private models: CatalogModel[] = [];
  private terminal: string[] = [];
  /** The commands came with descriptions: names alone never replace them. */
  private detailed = false;
  private said = '';

  /** The initialize response: commands with their descriptions, and the models. */
  fromInitialize(init: { commands?: unknown; models?: unknown } | undefined): CatalogEvent | undefined {
    if (!init) return undefined;
    const listed = commands(init.commands);
    if (listed.length) { this.commands = listed; this.detailed = true; }
    const offered = models(init.models);
    if (offered.length) this.models = offered;
    return this.say();
  }
  /** A turn's `init` message: names only, and which commands only make sense in a terminal. */
  fromInitMessage(message: { slash_commands?: unknown; terminal_slash_commands?: unknown }): CatalogEvent | undefined {
    this.terminal = names(message.terminal_slash_commands);
    if (!this.detailed) this.commands = commands(names(message.slash_commands));
    return this.say();
  }
  /** `commands_changed`: the full new list replaces the old one. */
  fromChanged(list: unknown): CatalogEvent | undefined {
    if (!Array.isArray(list)) return undefined;
    this.commands = commands(list);
    this.detailed = true;
    return this.say();
  }
  private say(): CatalogEvent | undefined {
    if (!this.commands.length && !this.models.length) return undefined;
    const data = { commands: this.commands, models: this.models, ...(this.terminal.length ? { terminal: this.terminal } : {}) };
    const key = JSON.stringify(data);
    if (key === this.said) return undefined;
    this.said = key;
    return { type: 'command_catalog', data };
  }
}
