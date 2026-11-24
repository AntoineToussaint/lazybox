/**
 * The person's config files, as the app reads and writes them (POWER.md,
 * "Config files"): keys.yaml, settings.yaml and layouts/*.yaml beside the
 * actions file. The Runner parses them (internal/power) and hands the app what
 * each declares as JSON; the app changes them by edits at paths (the Runner
 * keeps the comments) or, for a layout, by its whole YAML.
 *
 * Pure: what a file means for the app (a keymap, the ]] menu, Vim editing,
 * notification preferences, layouts), and the edits that make a file say what
 * the app now holds.
 */
import type { ConfigFile } from '@lazybox/protocol';

/** One change to a config file: the value (JSON) at a path of keys; empty json removes the key. */
export type ConfigEdit = { path: string[]; json: string };
import { actionDefinition, actionDefinitions, bindings, canonicalBinding, isMac, joinBindings, resolveMod } from './actions';
import { keyboardPreset, validateKeymap, withFixedKeys, type Keymap, type MenuEntry, type VimPreferences } from './keyboard';
import { builtinPresets, restoreTemplate, type LayoutPreset, type TemplateNode } from './layout-presets';
import { defaultNotificationPreferences, type NotificationPreferences } from './attention';
import { DEFAULT_PREFERENCES, parsePreferences, type AppearancePreferences, type Typography } from './appearance/preferences';

export type Issue = { path: string; line: number; message: string; severity: string };
export type KeysValue = {
  preset: string;
  bindings: { action: string; keys: string[]; section: string; line: number }[];
  menu: (MenuEntry & { line: number })[];
  vim: { messageBox: boolean; fileEditor: boolean };
};
export type SettingsValue = { notifications: Partial<NotificationPreferences>; appearance?: AppearanceFile };
export type LayoutValue = { name: string; description: string; layout: TemplateNode };

/** A config file with what it declares, parsed. */
export type ConfigDoc<T = unknown> = { name: string; kind: string; path: string; exists: boolean; error: string; stale: boolean; schemaPath: string; value?: T; issues: Issue[]; revision: number; loadedAt: number };

export function parseDoc<T>(file: ConfigFile): ConfigDoc<T> {
  let value: T | undefined;
  try { value = file.json ? JSON.parse(file.json) as T : undefined; } catch { value = undefined; }
  return { name: file.name, kind: file.kind, path: file.path, exists: file.exists, error: file.error, stale: file.stale, schemaPath: file.schemaPath, value,
    issues: file.issues.map(i => ({ path: i.path, line: i.line, message: i.message, severity: i.severity })), revision: Number(file.revision), loadedAt: Number(file.loadedAtUnixMillis) };
}

/** The file's name as a person reads it: keys.yaml, layouts/review.yaml. */
export const fileLabel = (name: string) => `${name}.yaml`;
/** Problems that matter: the file does not load, or an entry is left out. */
export function errorCount(doc?: ConfigDoc): number { return doc ? (doc.error ? 1 : 0) + doc.issues.filter(i => i.severity === 'error').length : 0; }
/** One line about a doc's first problem: "line 12: there is no action …". */
export function firstProblem(doc: ConfigDoc): string {
  const issue = doc.issues.find(i => i.severity === 'error') || doc.issues[0];
  if (doc.error && !issue) return doc.error;
  return issue ? `${issue.line ? `line ${issue.line}: ` : ''}${issue.message}` : '';
}

// ---- keys.yaml ------------------------------------------------------------

/** One binding as written in the file, resolved for this platform. */
const resolve = (value: string, mac: boolean) => resolveMod(value, mac);

/** The keymap a keys.yaml declares: its preset with its bindings over it. */
export function keymapFromFile(keys: KeysValue, mac = isMac): Keymap {
  const map = keyboardPreset(keys.preset || 'vim', mac);
  for (const binding of keys.bindings) {
    if (!actionDefinition(binding.action)) continue;
    map[binding.action as keyof Keymap] = joinBindings(binding.keys.flatMap(value => bindings(resolve(value, mac))));
  }
  return withFixedKeys(map, mac);
}
export const menuFromFile = (keys: KeysValue): MenuEntry[] => keys.menu.map(({ key, label, snippet, mode }) => ({ key, label, snippet, mode: mode === 'insert' ? 'insert' : 'send' }));
export const vimFromFile = (keys: KeysValue): VimPreferences => ({ composer: !!keys.vim?.messageBox, editor: !!keys.vim?.fileEditor });

/** What keys.yaml means for the app, or why it cannot apply (a keymap the app refuses). */
export function keysFromFile(keys: KeysValue, mac = isMac): { map: Keymap; menu: MenuEntry[]; vim: VimPreferences; problem: string } {
  const map = keymapFromFile(keys, mac);
  return { map, menu: menuFromFile(keys), vim: vimFromFile(keys), problem: validateKeymap(map) };
}

const sameKeys = (a: string, b: string) => bindings(a).map(canonicalBinding).join(' or ') === bindings(b).map(canonicalBinding).join(' or ');
/** A binding as the file keeps it: ⌘ written Mod+ on a Mac, so the file reads the same on Linux. */
function portable(binding: string, mac: boolean): string { return mac ? binding.replace(/(^|\s|\+)Meta\+/g, '$1Mod+') : binding; }
const sameMenu = (a: MenuEntry[], b: MenuEntry[]) => JSON.stringify(a.map(e => [e.key, e.label, e.snippet, e.mode])) === JSON.stringify(b.map(e => [e.key, e.label, e.snippet, e.mode]));

/**
 * The edits that make keys.yaml say this keymap, ]] menu and Vim choice: an
 * action whose keys are its preset's has no line (one that had is removed),
 * any other gets its keys. `preset` changes the file's preset (Settings'
 * "Start from a preset").
 */
export function keysEdits(next: { map: Keymap; menu: MenuEntry[]; vim: VimPreferences; preset?: string }, file: KeysValue | undefined, mac = isMac): ConfigEdit[] {
  const edits: ConfigEdit[] = [];
  const edit = (path: string[], value?: unknown) => edits.push({ path, json: value === undefined ? '' : JSON.stringify(value) });
  const preset = next.preset || file?.preset || 'vim';
  if (preset !== (file?.preset || 'vim') || (!file && next.preset)) edit(['preset'], preset);
  const base = keyboardPreset(preset, mac);
  const written = new Map((file?.bindings || []).map(b => [b.action, b]));
  for (const definition of actionDefinitions) {
    if (definition.fixed) continue;
    const want = next.map[definition.id] ?? '', had = written.get(definition.id);
    const path = ['keys', definition.scope, definition.id];
    // A line in another section is replaced where it belongs.
    if (had && had.section !== definition.scope) edit(['keys', had.section, definition.id]);
    if (sameKeys(want, base[definition.id])) { if (had && had.section === definition.scope) edit(path); continue; }
    if (had && had.section === definition.scope && sameKeys(want, joinBindings(had.keys.flatMap(value => bindings(resolve(value, mac)))))) continue;
    const values = bindings(want).map(value => portable(value, mac));
    edit(path, values.length === 1 ? values[0] : values);
  }
  if (!sameMenu(next.menu, file ? menuFromFile(file) : [])) edit(['leader-menu'], next.menu.length ? next.menu.map(({ key, label, snippet, mode }) => ({ key, label, snippet, mode })) : undefined);
  const vim = file ? vimFromFile(file) : { composer: false, editor: false };
  if (next.vim.composer !== vim.composer) edit(['vim', 'message-box'], next.vim.composer);
  if (next.vim.editor !== vim.editor) edit(['vim', 'file-editor'], next.vim.editor);
  return edits;
}

/** Whether the app's keys are all their defaults (nothing worth a file). */
export function defaultKeys(state: { map: Keymap; menu: MenuEntry[]; vim: VimPreferences }, mac = isMac): boolean {
  const base = keyboardPreset('vim', mac);
  return actionDefinitions.every(d => sameKeys(state.map[d.id] ?? '', base[d.id])) && !state.menu.length && !state.vim.composer && !state.vim.editor;
}

// ---- settings.yaml --------------------------------------------------------

/** The file's names for the notification preferences, in its order. */
export const notificationFields: [string, keyof NotificationPreferences][] = [['needs-input', 'needsInput'], ['failed', 'failed'], ['review', 'review'], ['reminders', 'reminders'], ['dock-badge', 'badge'], ['sound-needs-you', 'soundNeedsYou'], ['sound-done', 'soundDone']];
export function notificationsFromFile(settings: SettingsValue): NotificationPreferences {
  return { ...defaultNotificationPreferences, ...Object.fromEntries(Object.entries(settings.notifications || {}).filter(([, v]) => typeof v === 'boolean')) };
}
/** The edits that make settings.yaml say these preferences; a value the file does not hold is written only when it is not the default. */
export function settingsEdits(next: NotificationPreferences, file: SettingsValue | undefined): ConfigEdit[] {
  const current = notificationsFromFile(file || { notifications: {} });
  return notificationFields.filter(([, key]) => next[key] !== current[key]).map(([name, key]) => ({ path: ['notifications', name], json: JSON.stringify(next[key]) }));
}
export const defaultNotifications = (p: NotificationPreferences) => notificationFields.every(([, key]) => p[key] === defaultNotificationPreferences[key]);

// ---- settings.yaml: appearance -------------------------------------------

type TopField = 'mode' | 'darkTheme' | 'lightTheme' | 'accent' | 'minContrast' | 'motion';
/** The appearance section as the Runner hands it over: the app's names, only what the file sets. */
export type AppearanceFile = Partial<Pick<AppearancePreferences, TopField> & Typography & { zoom: number }>;
/**
 * The file's names for the appearance preferences, in its order
 * (internal/power/settings.go, AppearanceFields). `top`: a field of the
 * preferences with a default; `type`: a typography field, absent meaning "the
 * theme's"; `zoom`: the desktop app's zoom.
 */
export const appearanceFields: [string, keyof AppearanceFile, 'top' | 'type' | 'zoom'][] = [
  ['mode', 'mode', 'top'], ['dark-theme', 'darkTheme', 'top'], ['light-theme', 'lightTheme', 'top'], ['accent', 'accent', 'top'], ['min-contrast', 'minContrast', 'top'], ['motion', 'motion', 'top'],
  ['density', 'density', 'type'], ['interface-font', 'uiFont', 'type'], ['interface-font-size', 'uiFontSize', 'type'], ['code-font', 'codeFont', 'type'], ['code-font-size', 'codeFontSize', 'type'], ['ligatures', 'ligatures', 'type'],
  ['chat-font-size', 'chatFontSize', 'type'], ['chat-line-height', 'chatLineHeight', 'type'], ['chat-paragraph-gap', 'chatParagraphGap', 'type'], ['chat-line-width', 'chatMaxWidth', 'type'],
  ['zoom', 'zoom', 'zoom'],
];
const zoomLevel = (value: unknown) => typeof value === 'number' && value >= 0.5 && value <= 2 ? value : undefined;

/** What the file would say for these preferences (and zoom): every value that is not its default. */
export function appearanceToFile(preferences: AppearancePreferences, zoom?: number): AppearanceFile {
  const out: Record<string, unknown> = {};
  for (const [, key, scope] of appearanceFields) {
    const value = scope === 'type' ? preferences.typography[key as keyof Typography] : scope === 'zoom' ? zoom : preferences[key as TopField];
    if (value === undefined || (scope === 'top' && value === DEFAULT_PREFERENCES[key as TopField]) || (scope === 'zoom' && value === 1)) continue;
    out[key] = value;
  }
  return out as AppearanceFile;
}

/**
 * The preferences a file's appearance section means, over the device's own
 * colour overrides and cached user themes (which stay on the device). Values
 * the app would refuse are dropped, as a stored preference would be.
 */
export function appearanceFromFile(file: AppearanceFile | undefined, current: AppearancePreferences): { preferences: AppearancePreferences; zoom: number } {
  const value = file || {};
  const typography: Record<string, unknown> = {};
  for (const [, key, scope] of appearanceFields) if (scope === 'type' && value[key] !== undefined) typography[key] = value[key];
  const top = Object.fromEntries(appearanceFields.filter(([, key, scope]) => scope === 'top' && value[key] !== undefined).map(([, key]) => [key, value[key]]));
  const preferences = parsePreferences(JSON.stringify({ ...top, typography }));
  if (current.tokens) preferences.tokens = current.tokens;
  if (current.userThemes) preferences.userThemes = current.userThemes;
  return { preferences, zoom: zoomLevel(value.zoom) ?? 1 };
}

/** Whether two appearance sections say the same. */
export const sameAppearance = (a: AppearanceFile, b: AppearanceFile) => JSON.stringify(Object.entries(a).sort()) === JSON.stringify(Object.entries(b).sort());

/**
 * The edits that make settings.yaml's appearance section say `next`: a value
 * whose meaning changed is written, one back at its default is removed. A
 * value the file states that still means the same (mode: system) is left as
 * the person wrote it.
 */
export function appearanceEdits(next: AppearanceFile, file: AppearanceFile | undefined): ConfigEdit[] {
  const had = file || {};
  const effective = (value: AppearanceFile, key: keyof AppearanceFile, scope: string) => value[key] ?? (scope === 'top' ? DEFAULT_PREFERENCES[key as TopField] : scope === 'zoom' ? 1 : undefined);
  const edits: ConfigEdit[] = [];
  for (const [name, key, scope] of appearanceFields) {
    if (effective(next, key, scope) === effective(had, key, scope)) continue;
    edits.push({ path: ['appearance', name], json: next[key] === undefined ? '' : JSON.stringify(next[key]) });
  }
  return edits;
}
/** Whether nothing in these preferences (and zoom) is worth a line in the file. */
export const defaultAppearance = (preferences: AppearancePreferences, zoom?: number) => !Object.keys(appearanceToFile(preferences, zoom)).length;

// ---- layouts/*.yaml -------------------------------------------------------

export type FileLayout = LayoutPreset & { file: string };
/** The layouts the files declare, in file order, as presets the Layout menu applies. */
export function layoutsFromFiles(docs: ConfigDoc[]): FileLayout[] {
  return docs.filter(d => d.kind === 'layout' && d.value).map(d => {
    const value = d.value as LayoutValue;
    return { id: `file:${d.name.slice('layouts/'.length)}`, name: value.name, description: value.description || '', layout: restoreTemplate(value.layout), file: d.name };
  });
}

/** A layout file's name from a layout's name: "Review wide" → review-wide, made unique. */
export function layoutFileName(name: string, taken: Iterable<string>): string {
  const base = name.toLowerCase().normalize('NFKD').replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '').slice(0, 48) || 'layout';
  const used = new Set(taken);
  let id = base;
  for (let n = 2; used.has(`layouts/${id}`); n++) id = `${base}-${n}`;
  return `layouts/${id}`;
}

const plain = /^[A-Za-z0-9][A-Za-z0-9 _.,()&'+/-]*$/;
const reserved = /^(?:true|false|yes|no|on|off|null|~|[-+]?[0-9.]+)$/i;
/** A string as YAML: plain when it reads the same, else double-quoted. */
export function yamlString(value: string): string { return plain.test(value) && !reserved.test(value) && !/\s$/.test(value) ? value : JSON.stringify(value); }
const share = (size: number) => String(Math.round(size * 1000) / 10);

/** A layout as its file: what "Copy the layout as YAML" puts on the clipboard and Save writes. */
export function layoutYaml(layout: { name: string; description?: string; layout: TemplateNode }): string {
  const lines = ['# yaml-language-server: $schema=../layout.schema.json', `name: ${yamlString(layout.name)}`];
  if (layout.description) lines.push(`description: ${yamlString(layout.description)}`);
  lines.push('layout:');
  const node = (n: TemplateNode, indent: string, first: string) => {
    // `first` is the prefix of the node's first line ("- " in a list); the rest align with it.
    const rest = indent + ' '.repeat(first.length);
    if (n.type === 'group') {
      lines.push(`${indent}${first}panels: [${n.kinds.join(', ')}]`);
      if (n.minimized) lines.push(`${rest}minimized: true`);
      return;
    }
    lines.push(`${indent}${first}split: ${n.direction}`, `${rest}sizes: [${n.sizes.map(share).join(', ')}]`, `${rest}children:`);
    for (const child of n.children) node(child, `${rest}  `, '- ');
  };
  node(layout.layout, '  ', '');
  return lines.join('\n') + '\n';
}

/** Whether pasted text looks like a layout file (the palette offers to import it). */
export function looksLikeLayoutYaml(text: string): boolean {
  return /(^|\n)\s*layout\s*:/.test(text) && /(^|\n)\s*-?\s*(panels|split)\s*:/.test(text) && text.includes('\n');
}
/** The name a layout's YAML gives itself, for its file name before the Runner reads it. */
export function layoutNameOf(text: string): string {
  const match = /^name:\s*(.+?)\s*$/m.exec(text);
  return match ? match[1].replace(/^(["'])(.*)\1$/, '$2') : 'Imported layout';
}

/** Every layout the cycle key steps through: the presets, then the files'. */
export function cycleLayouts(files: FileLayout[]): LayoutPreset[] { return [...builtinPresets, ...files]; }
