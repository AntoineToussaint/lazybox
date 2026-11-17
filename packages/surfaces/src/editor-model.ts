/** The editor area's arrangement, persisted per workspace: groups of tabs in a
 * split tree, which group has focus, and the recently opened files. Paths are
 * workspace-relative and include the repository folder ("api/src/main.go").
 * Everything here is pure, so the layout rules are unit-tested without a DOM. */
export type EditorGroup = { id: string; tabs: string[]; active: string; /** A tab opened by a single click, replaced by the next one until it is kept. */ preview?: string };
export type EditorNode = { type: 'group'; group: EditorGroup } | { type: 'split'; direction: 'row' | 'column'; ratio: number; children: [EditorNode, EditorNode] };
export type EditorLayout = { version: 1; root: EditorNode; focused: string; recent: string[] };

export const maxGroups = 4, maxTabs = 40, maxRecent = 30;
const idPattern = /^[a-zA-Z0-9_-]{1,64}$/;

export const emptyEditorLayout = (id = 'main'): EditorLayout => ({ version: 1, root: { type: 'group', group: { id, tabs: [], active: '' } }, focused: id, recent: [] });
export function editorGroups(node: EditorNode): EditorGroup[] { return node.type === 'group' ? [node.group] : node.children.flatMap(editorGroups); }
export const findGroup = (layout: EditorLayout, id: string) => editorGroups(layout.root).find(group => group.id === id);
export const focusedGroup = (layout: EditorLayout) => findGroup(layout, layout.focused) || editorGroups(layout.root)[0];
export const openPaths = (layout: EditorLayout) => new Set(editorGroups(layout.root).flatMap(group => group.tabs));
/** The file each group shows: what is on screen. */
export const visiblePaths = (layout: EditorLayout) => new Set(editorGroups(layout.root).map(group => group.active).filter(Boolean));
export const activePath = (layout: EditorLayout) => focusedGroup(layout)?.active || '';

function mapGroups(node: EditorNode, change: (group: EditorGroup) => EditorGroup): EditorNode {
 return node.type === 'group' ? { type: 'group', group: change(node.group) } : { ...node, children: [mapGroups(node.children[0], change), mapGroups(node.children[1], change)] };
}
const updateGroup = (layout: EditorLayout, id: string, change: (group: EditorGroup) => EditorGroup): EditorLayout => ({ ...layout, root: mapGroups(layout.root, group => group.id === id ? change(group) : group) });
const remember = (recent: string[], path: string) => [path, ...recent.filter(value => value !== path)].slice(0, maxRecent);

export type OpenOptions = { group?: string; preview?: boolean; /** Paths that must not be dropped to stay within the tab limit, such as unsaved files. */ keep?(path: string): boolean };
/** Open a file in a group (the focused one by default) and show it. A preview
 * open replaces the group's previous preview tab; opening a preview tab again
 * without `preview` keeps it. */
export function openTab(layout: EditorLayout, path: string, options: OpenOptions = {}): EditorLayout {
 const target = findGroup(layout, options.group || layout.focused) || editorGroups(layout.root)[0];
 const next = updateGroup(layout, target.id, group => {
  let tabs = group.tabs, preview = group.preview;
  if (tabs.includes(path)) { if (!options.preview && preview === path) preview = undefined; }
  else if (options.preview && preview && tabs.includes(preview) && !options.keep?.(preview)) { tabs = tabs.map(tab => tab === preview ? path : tab); preview = path; }
  else {
   const at = tabs.indexOf(group.active);
   tabs = at < 0 ? [...tabs, path] : [...tabs.slice(0, at + 1), path, ...tabs.slice(at + 1)];
   if (options.preview) preview = path;
  }
  // Past the limit, the oldest tabs that are neither shown nor unsaved go first.
  while (tabs.length > maxTabs) { const drop = tabs.find(tab => tab !== path && !options.keep?.(tab)); if (!drop) break; tabs = tabs.filter(tab => tab !== drop); if (preview === drop) preview = undefined; }
  return { ...group, tabs, active: path, preview };
 });
 return { ...next, focused: target.id, recent: remember(layout.recent, path) };
}
/** Keep a preview tab: it is no longer replaced by the next single-click open. */
export const pinTab = (layout: EditorLayout, groupId: string, path: string) => updateGroup(layout, groupId, group => group.preview === path ? { ...group, preview: undefined } : group);
export const pinPath = (layout: EditorLayout, path: string) => ({ ...layout, root: mapGroups(layout.root, group => group.preview === path ? { ...group, preview: undefined } : group) });
export function activateTab(layout: EditorLayout, groupId: string, path: string): EditorLayout {
 const group = findGroup(layout, groupId);
 if (!group || !group.tabs.includes(path)) return layout;
 return { ...updateGroup(layout, groupId, value => ({ ...value, active: path })), focused: groupId, recent: remember(layout.recent, path) };
}
export const focusGroup = (layout: EditorLayout, groupId: string): EditorLayout => findGroup(layout, groupId) && layout.focused !== groupId ? { ...layout, focused: groupId } : layout;

function removeGroup(node: EditorNode, id: string): EditorNode | null {
 if (node.type === 'group') return node.group.id === id ? null : node;
 const first = removeGroup(node.children[0], id), second = removeGroup(node.children[1], id);
 return first && second ? { ...node, children: [first, second] } : first || second;
}
/** Close a group; its neighbour takes focus. The last group stays, emptied. */
export function closeGroup(layout: EditorLayout, groupId: string): EditorLayout {
 const all = editorGroups(layout.root), index = all.findIndex(group => group.id === groupId);
 if (index < 0) return layout;
 const root = removeGroup(layout.root, groupId);
 if (!root) return updateGroup(layout, groupId, group => ({ ...group, tabs: [], active: '', preview: undefined }));
 const neighbour = all[index - 1] || all[index + 1];
 return { ...layout, root, focused: layout.focused === groupId ? neighbour.id : layout.focused };
}
/** Close a tab; the tab to its right (else left) is shown. A group left empty
 * closes, unless it is the only one. */
export function closeTab(layout: EditorLayout, groupId: string, path: string): EditorLayout {
 const group = findGroup(layout, groupId);
 if (!group || !group.tabs.includes(path)) return layout;
 const index = group.tabs.indexOf(path), tabs = group.tabs.filter(tab => tab !== path);
 if (!tabs.length && editorGroups(layout.root).length > 1) return closeGroup(layout, groupId);
 const active = group.active === path ? tabs[Math.min(index, tabs.length - 1)] || '' : group.active;
 return updateGroup(layout, groupId, value => ({ ...value, tabs, active, preview: value.preview === path ? undefined : value.preview }));
}
/** Close every tab of a path, in every group (the file was deleted or is a folder). */
export function closePath(layout: EditorLayout, path: string): EditorLayout {
 let next = layout;
 for (const group of editorGroups(layout.root)) if (group.tabs.includes(path)) next = closeTab(next, group.id, path);
 return next;
}
/** Reorder within a group: move a tab to `index` (clamped). */
export function moveTab(layout: EditorLayout, groupId: string, path: string, index: number): EditorLayout {
 return updateGroup(layout, groupId, group => {
  if (!group.tabs.includes(path)) return group;
  const rest = group.tabs.filter(tab => tab !== path), at = Math.max(0, Math.min(rest.length, index));
  return { ...group, tabs: [...rest.slice(0, at), path, ...rest.slice(at)] };
 });
}
/** Move a tab one place left (-1) or right (+1). */
export function shiftTab(layout: EditorLayout, groupId: string, path: string, step: -1 | 1): EditorLayout {
 const group = findGroup(layout, groupId);
 return group ? moveTab(layout, groupId, path, group.tabs.indexOf(path) + step) : layout;
}
/** Move a tab into another group at `index` (the end by default), shown there. */
export function moveTabToGroup(layout: EditorLayout, fromId: string, path: string, toId: string, index?: number): EditorLayout {
 if (fromId === toId) return index === undefined ? layout : moveTab(layout, fromId, path, index);
 const from = findGroup(layout, fromId), to = findGroup(layout, toId);
 if (!from || !to || !from.tabs.includes(path)) return layout;
 const pinned = from.preview !== path;
 let next = updateGroup(layout, toId, group => {
  const rest = group.tabs.filter(tab => tab !== path), at = Math.max(0, Math.min(rest.length, index ?? rest.length));
  return { ...group, tabs: [...rest.slice(0, at), path, ...rest.slice(at)], active: path, preview: pinned && group.preview === path ? undefined : group.preview };
 });
 next = closeTab(next, fromId, path);
 return { ...next, focused: toId };
}
/** The tab `step` places away from the shown one, wrapping around. */
export function adjacentTab(layout: EditorLayout, groupId: string, step: number): string {
 const group = findGroup(layout, groupId);
 if (!group || !group.tabs.length) return '';
 const index = Math.max(0, group.tabs.indexOf(group.active));
 return group.tabs[(index + step + group.tabs.length * 4) % group.tabs.length];
}
function replaceGroup(node: EditorNode, id: string, change: (node: EditorNode) => EditorNode): EditorNode {
 if (node.type === 'group') return node.group.id === id ? change(node) : node;
 return { ...node, children: [replaceGroup(node.children[0], id, change), replaceGroup(node.children[1], id, change)] };
}
export const canSplit = (layout: EditorLayout) => editorGroups(layout.root).length < maxGroups;
/** Split a group right ('row') or down ('column'): the new group shows `path`,
 * by default the same file, so the two halves can show two places in it. It
 * takes focus. */
export function splitGroup(layout: EditorLayout, groupId: string, direction: 'row' | 'column', newId: string, path?: string): EditorLayout {
 const group = findGroup(layout, groupId);
 if (!group || !canSplit(layout) || findGroup(layout, newId)) return layout;
 const shown = path ?? group.active;
 const created: EditorNode = { type: 'group', group: { id: newId, tabs: shown ? [shown] : [], active: shown } };
 return { ...layout, root: replaceGroup(layout.root, groupId, node => ({ type: 'split', direction, ratio: 0.5, children: [node, created] })), focused: newId };
}
/** Set the ratio of the split at `path` (child indexes from the root). */
export function resizeSplit(layout: EditorLayout, path: number[], ratio: number): EditorLayout {
 const clamp = Math.round(Math.min(0.85, Math.max(0.15, ratio)) * 100) / 100;
 const update = (node: EditorNode, rest: number[]): EditorNode => {
  if (node.type !== 'split') return node;
  if (!rest.length) return { ...node, ratio: clamp };
  return { ...node, children: [rest[0] === 0 ? update(node.children[0], rest.slice(1)) : node.children[0], rest[0] === 1 ? update(node.children[1], rest.slice(1)) : node.children[1]] };
 };
 return { ...layout, root: update(layout.root, path) };
}
export function forgetRecent(layout: EditorLayout, path: string): EditorLayout { return layout.recent.includes(path) ? { ...layout, recent: layout.recent.filter(value => value !== path) } : layout; }

const object = (value: unknown): value is Record<string, unknown> => !!value && typeof value === 'object' && !Array.isArray(value);
const validPath = (value: unknown): value is string => typeof value === 'string' && value.length > 0 && value.length <= 4096 && !value.includes('\0');
/** A saved layout, checked: anything malformed is rejected whole rather than half-restored. */
export function parseEditorLayout(text: string): EditorLayout {
 if (text.length > 256 << 10) throw new Error('Editor layout exceeds 256 KiB');
 const value: unknown = JSON.parse(text), ids = new Set<string>();
 const node = (value: unknown, depth: number): boolean => {
  if (!object(value) || depth > maxGroups) return false;
  if (value.type === 'split') return ['row', 'column'].includes(value.direction as string) && typeof value.ratio === 'number' && value.ratio >= 0.15 && value.ratio <= 0.85 && Array.isArray(value.children) && value.children.length === 2 && value.children.every(child => node(child, depth + 1));
  const group = value.group;
  if (value.type !== 'group' || !object(group) || typeof group.id !== 'string' || !idPattern.test(group.id) || ids.has(group.id)) return false;
  ids.add(group.id);
  return Array.isArray(group.tabs) && group.tabs.length <= maxTabs && group.tabs.every(validPath) && new Set(group.tabs).size === group.tabs.length && typeof group.active === 'string' && (group.active === '' ? !group.tabs.length : group.tabs.includes(group.active)) && (group.preview === undefined || (typeof group.preview === 'string' && group.tabs.includes(group.preview)));
 };
 if (!object(value) || value.version !== 1 || !node(value.root, 0) || ids.size > maxGroups || typeof value.focused !== 'string' || !ids.has(value.focused) || !Array.isArray(value.recent) || !value.recent.every(validPath)) throw new Error('Invalid editor layout');
 return { version: 1, root: value.root as EditorNode, focused: value.focused, recent: (value.recent as string[]).slice(0, maxRecent) };
}

/** The shortest trailing part of each path that tells same-named tabs apart:
 * two README.md tabs read "api" and "web" beside their names. */
export function tabHints(paths: string[]): Record<string, string> {
 const hints: Record<string, string> = {};
 const byName = new Map<string, string[]>();
 for (const path of paths) { const name = path.split('/').pop() || path; byName.set(name, [...(byName.get(name) || []), path]); }
 for (const same of byName.values()) {
  if (same.length < 2) continue;
  const parts = same.map(path => path.split('/').slice(0, -1));
  for (let depth = 1; depth <= Math.max(...parts.map(value => value.length)); depth++) {
   const labels = parts.map(value => value.slice(-depth).join('/'));
   if (new Set(labels).size === labels.length || depth === Math.max(...parts.map(value => value.length))) { same.forEach((path, i) => { hints[path] = labels[i] || '/'; }); break; }
  }
 }
 return hints;
}

/** Go to file: "routes.ts:42" opens at line 42; the line is not part of the search. */
export function parseQuery(value: string): { term: string; line?: number } {
 const match = /^(.*?)(?::(\d+))?(?::\d+)?\s*$/.exec(value.trim());
 const term = (match?.[1] ?? value).trim(), line = match?.[2] ? Number(match[2]) : undefined;
 return { term, line: line && Number.isSafeInteger(line) ? line : undefined };
}
