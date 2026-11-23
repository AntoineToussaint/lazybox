/**
 * A workspace's saved pages and how each one arranges its panels.
 *
 * Persisted personal composition contains references, never credentials or
 * operations. A page is a dock: a tree of splits whose leaves are tab groups,
 * plus groups floating over the page, groups popped out into their own
 * windows (same window process), and chats open in their own native window.
 * Sizes are fractions, so a page looks the same in any window size; the
 * Workspaces sidebar alone has one fixed width in pixels, kept outside the
 * page (see DockLayout).
 *
 * Version 1 was a binary tree of single panels; `parseLayout` migrates it.
 */
export type ViewTarget = { collectionId: string; sessionId: string; commandId: string };
export type Binding = { mode: 'follow' } | { mode: 'pinned'; target: ViewTarget };
/**
 * One panel of a page. A chat is a conversation tab: `preview` marks its
 * group's preview tab (see "Conversations" below), shown in italics, which the
 * next conversation chosen with a single click replaces.
 */
export type Panel = { id: string; kind: string; binding: Binding; detached: boolean; origin?: PanelOrigin; preview?: boolean };
/**
 * Where a panel sat before it went to a window of its own, so putting it back
 * returns it there: as a tab of the group it left (at its place among the
 * tabs), or, when that group went with it, beside the groups that were next
 * to it, on the same side and with the same share.
 */
export type PanelOrigin = { group: string; index: number; beside?: string[]; side?: DockSide; share?: number; /** The group beside it was minimized (command output below a chat): it is again. */ minimized?: boolean };
/** Panels shown as tabs in one place; `active` is the one in front. A minimized group shows only its tabs. */
export type Group = { type: 'group'; id: string; panels: Panel[]; active: string; minimized?: boolean };
/** Children side by side ('row') or stacked ('column'); `sizes` are their shares, summing to 1. */
export type Split = { type: 'split'; direction: 'row' | 'column'; sizes: number[]; children: LayoutNode[] };
export type LayoutNode = Group | Split;
/** A floating group's place over the page, in pixels from one corner. */
export type Box = { left?: number; top?: number; right?: number; bottom?: number; width: number; height: number };
export type FloatingGroup = { group: Group; box: Box };
export type WorkspacePage = {
  id: string;
  name: string;
  selection: ViewTarget;
  /** The docked arrangement; null when every panel was closed. */
  layout: LayoutNode | null;
  floating: FloatingGroup[];
  /** Groups in their own window. They come back docked when the page is loaded again. */
  popouts: Group[];
  /** Chats open in their own native window that left no card behind. */
  detached: Panel[];
};
export type WorkspaceLayout = { version: 2; selectedPage: string; pages: WorkspacePage[] };
export type DockSide = 'left' | 'right' | 'top' | 'bottom';

export const emptyTarget = (): ViewTarget => ({ collectionId: '', sessionId: '', commandId: '' });
export const dockSides: readonly DockSide[] = ['left', 'right', 'top', 'bottom'];
export const isDockSide = (value: unknown): value is DockSide => dockSides.includes(value as DockSide);
/** At most this many panels on one page, wherever they are. */
export const PANEL_LIMIT = 16;
/** At most this many pages per workspace. */
export const PAGE_LIMIT = 8;
const SIZE_LIMIT = 192 << 10, DEPTH_LIMIT = 10;

const idPattern = /^[a-zA-Z0-9_-]{1,128}$/;
const kindPattern = /^[a-z][a-z0-9-]{0,63}$/;
const object = (value: unknown): value is Record<string, unknown> => !!value && typeof value === 'object' && !Array.isArray(value);
const keys = (value: Record<string, unknown>, allowed: string[]) => Object.keys(value).every(key => allowed.includes(key));
const id = (value: unknown, optional = false): value is string => typeof value === 'string' && ((optional && value === '') || idPattern.test(value));
const finite = (value: unknown): value is number => typeof value === 'number' && Number.isFinite(value);

export function validTarget(value: unknown): value is ViewTarget {
  return object(value) && keys(value, ['collectionId', 'sessionId', 'commandId']) && id(value.collectionId, true) && id(value.sessionId, true) && id(value.commandId, true) && (!value.commandId || !!value.sessionId);
}
export function resolveTarget(panel: Panel, selection: ViewTarget): ViewTarget { return panel.binding.mode === 'pinned' ? panel.binding.target : selection; }
export function pinnedTarget(panel: Panel): { collectionId: string; sessionId: string } {
  return panel.binding.mode === 'pinned' ? { collectionId: panel.binding.target.collectionId, sessionId: panel.binding.target.sessionId } : { collectionId: '', sessionId: '' };
}

// ---------------------------------------------------------------------------
// Reading a tree.

export function groups(node: LayoutNode | null): Group[] { return !node ? [] : node.type === 'group' ? [node] : node.children.flatMap(groups); }
/** The docked panels, in reading order. */
export function panels(node: LayoutNode | null): Panel[] { return groups(node).flatMap(group => group.panels); }
/** Every group of a page: docked, floating and popped out. */
export function pageGroups(page: WorkspacePage): Group[] { return [...groups(page.layout), ...page.floating.map(value => value.group), ...page.popouts]; }
/** Every panel of a page, including chats in their own window. */
export function allPanels(page: WorkspacePage): Panel[] { return [...pageGroups(page).flatMap(group => group.panels), ...page.detached]; }
/** Panels a person can see on this page (not those in other native windows). */
export function shownPanels(page: WorkspacePage): Panel[] { return pageGroups(page).flatMap(group => group.panels); }
export function groupOf(page: WorkspacePage, panelID: string): Group | undefined { return pageGroups(page).find(group => group.panels.some(panel => panel.id === panelID)); }

// ---------------------------------------------------------------------------
// Changing a tree. Every function returns a normalized tree: no empty groups,
// no split with one child, no split nested in one going the same way, and
// sizes that sum to 1.

const MIN_SHARE = 0.02;
const round = (value: number) => Math.round(value * 10000) / 10000;
function shares(sizes: number[]): number[] {
  const clean = sizes.map(value => finite(value) && value > 0 ? value : MIN_SHARE), total = clean.reduce((sum, value) => sum + value, 0);
  return clean.map(value => round(Math.max(MIN_SHARE, value / total)));
}
export function normalize(node: LayoutNode | null): LayoutNode | null {
  if (!node) return null;
  if (node.type === 'group') {
    if (!node.panels.length) return null;
    return node.panels.some(panel => panel.id === node.active) ? node : { ...node, active: node.panels[0].id };
  }
  const children: LayoutNode[] = [], sizes: number[] = [];
  node.children.forEach((child, index) => {
    const next = normalize(child), size = node.sizes[index] ?? 1 / node.children.length;
    if (!next) return;
    // A split inside a split going the same way is the same split.
    if (next.type === 'split' && next.direction === node.direction) next.children.forEach((inner, at) => { children.push(inner); sizes.push(size * next.sizes[at]); });
    else { children.push(next); sizes.push(size); }
  });
  if (!children.length) return null;
  if (children.length === 1) return children[0];
  return { type: 'split', direction: node.direction, sizes: shares(sizes), children };
}
export function mapPanels(node: LayoutNode, change: (panel: Panel) => Panel): LayoutNode;
export function mapPanels(node: LayoutNode | null, change: (panel: Panel) => Panel): LayoutNode | null;
export function mapPanels(node: LayoutNode | null, change: (panel: Panel) => Panel): LayoutNode | null {
  if (!node) return null;
  if (node.type === 'group') { const next = node.panels.map(change); return { ...node, panels: next, active: next[node.panels.findIndex(panel => panel.id === node.active)]?.id ?? next[0]?.id }; }
  return { ...node, children: node.children.map(child => mapPanels(child, change)) };
}
export function mapGroups(node: LayoutNode | null, change: (group: Group) => Group): LayoutNode | null {
  if (!node) return null;
  return node.type === 'group' ? change(node) : { ...node, children: node.children.map(child => mapGroups(child, change)!) };
}
/** The page with one change to every group, wherever it is. */
export function mapPageGroups(page: WorkspacePage, change: (group: Group) => Group): WorkspacePage {
  return { ...page, layout: mapGroups(page.layout, change), floating: page.floating.map(value => ({ ...value, group: change(value.group) })), popouts: page.popouts.map(change) };
}
export function mapPagePanels(page: WorkspacePage, change: (panel: Panel) => Panel): WorkspacePage {
  const group = (value: Group): Group => ({ ...value, panels: value.panels.map(change) });
  return { ...mapPageGroups(page, group), detached: page.detached.map(change) };
}
export function removePanel(node: LayoutNode | null, panelID: string): LayoutNode | null {
  return normalize(mapGroups(node, group => group.panels.some(panel => panel.id === panelID) ? { ...group, panels: group.panels.filter(panel => panel.id !== panelID) } : group));
}
/** The page without this panel, wherever it was. */
export function removeFromPage(page: WorkspacePage, panelID: string): WorkspacePage {
  const without = (group: Group): Group | null => normalize({ ...group, panels: group.panels.filter(panel => panel.id !== panelID) }) as Group | null;
  return {
    ...page,
    layout: removePanel(page.layout, panelID),
    floating: page.floating.flatMap(value => { const group = without(value.group); return group ? [{ ...value, group }] : []; }),
    popouts: page.popouts.flatMap(value => { const group = without(value); return group ? [group] : []; }),
    detached: page.detached.filter(panel => panel.id !== panelID),
  };
}
/** Bring a panel to the front of its group, restoring a minimized group. */
export function activatePanel(page: WorkspacePage, panelID: string): WorkspacePage {
  return mapPageGroups(page, group => group.panels.some(panel => panel.id === panelID) ? { ...group, active: panelID, minimized: false } : group);
}

/** A group's id derived from the panel it was made for, unique on the page. */
export function groupID(panelID: string, taken: Iterable<string> = []): string {
  const used = new Set(taken);
  let candidate = `g-${panelID}`.slice(0, 128), n = 1;
  while (used.has(candidate)) candidate = `g-${panelID}`.slice(0, 120) + `-${++n}`;
  return candidate;
}
const groupFor = (panel: Panel, taken: Iterable<string> = []): Group => ({ type: 'group', id: groupID(panel.id, taken), panels: [panel], active: panel.id });

/** Panels that make up the page's sidebar: the workspace list above the agents list, both always in view. */
export const sidebarKinds: ReadonlySet<string> = new Set(['collections', 'agents']);
/**
 * Sidebar panels at the left or right edge of the page are its sidebar, with
 * one fixed width: a group of them, or a column of such groups stacked
 * (Workspaces above Agents).
 */
export const isSidebar = (node: LayoutNode): boolean => node.type === 'group'
  ? node.panels.every(panel => sidebarKinds.has(panel.kind))
  : node.direction === 'column' && node.children.every(isSidebar);
/** The ids of the groups that make up the page's sidebar, top to bottom; none when it has no sidebar. */
export function sidebarGroupIDs(node: LayoutNode | null): string[] {
  const at = sidebarAt(node);
  if (!at || node?.type !== 'split') return [];
  return groups(node.children[at === 'first' ? 0 : node.children.length - 1]).map(group => group.id);
}
/** The Agents list's share of the sidebar's height, under Workspaces. */
export const AGENTS_SHARE = 0.45;
/**
 * Workspaces and Agents kept as tabs of one group (as every page had them
 * before) become a column: Workspaces on top, Agents below, both in view.
 * Everything else about the page stays as it was.
 */
export function stackSidebar(node: LayoutNode | null): LayoutNode | null {
  if (!node) return node;
  const taken = groups(node).map(group => group.id);
  const visit = (current: LayoutNode): LayoutNode => {
    if (current.type === 'split') return { ...current, children: current.children.map(visit) };
    const list = current.panels.filter(panel => panel.kind === 'collections'), agents = current.panels.filter(panel => panel.kind === 'agents');
    if (!list.length || !agents.length || !current.panels.every(panel => sidebarKinds.has(panel.kind))) return current;
    const { minimized: _, ...rest } = current;
    const top: Group = { ...rest, panels: list, active: list[0].id };
    const below: Group = { type: 'group', id: groupID(agents[0].id, taken), panels: agents, active: agents[0].id };
    taken.push(below.id);
    return { type: 'split', direction: 'column', sizes: [1 - AGENTS_SHARE, AGENTS_SHARE], children: [top, below] };
  };
  const next = visit(node);
  return JSON.stringify(next) === JSON.stringify(node) ? node : normalize(next);
}
/**
 * The Agents list under Workspaces is one thing for the whole window, not a
 * panel of each page: closed, it is off every page of every workspace; folded,
 * it is folded on all of them; until the person brings it back. Undefined is a
 * window that never chose (saved pages are left as they are).
 */
export type SidebarAgents = 'shown' | 'folded' | 'closed';
export const isSidebarAgents = (value: unknown): value is SidebarAgents => value === 'shown' || value === 'folded' || value === 'closed';
/** The sidebar's Agents group, when it is stacked under Workspaces (so it can fold). */
export function sidebarAgentsGroup(node: LayoutNode | null): Group | undefined {
  const at = sidebarAt(node);
  if (!at || node?.type !== 'split') return undefined;
  const side = node.children[at === 'first' ? 0 : node.children.length - 1];
  if (side.type !== 'split') return undefined;
  return groups(side).find(group => group.panels.every(panel => panel.kind === 'agents'));
}
/**
 * The page as the window's choice has it. Closed: every Agents panel leaves
 * the page, and Workspaces takes the sidebar's height (its width is the
 * window's, kept outside the page). Shown or folded: a page whose sidebar has
 * Workspaces gets Agents under them if it has none, folded or not. Unchanged
 * (the same object) when the page already agrees.
 */
export function fitSidebarAgents(page: WorkspacePage, choice: SidebarAgents | undefined): WorkspacePage {
  if (!choice) return page;
  if (choice === 'closed') return allPanels(page).filter(panel => panel.kind === 'agents').reduce((result, panel) => removeFromPage(result, panel.id), page);
  let layout = page.layout;
  const at = sidebarAt(layout);
  if (at && layout?.type === 'split' && !allPanels(page).some(panel => panel.kind === 'agents')) {
    const index = at === 'first' ? 0 : layout.children.length - 1, side = layout.children[index];
    const list = groups(side).flatMap(group => group.panels).find(panel => panel.kind === 'collections');
    if (list) {
      const taken = new Set(allPanels(page).map(panel => panel.id));
      let panelID = `${list.id}-agents`.slice(0, 128);
      for (let n = 2; taken.has(panelID); n++) panelID = `${list.id}-agents`.slice(0, 120) + `-${n}`;
      const below = leaf(followPanel(panelID, 'agents'));
      const stacked = edge(side, { ...below, id: groupID(panelID, pageGroups(page).map(group => group.id)) }, 'bottom', AGENTS_SHARE);
      layout = normalize({ ...layout, children: layout.children.map((child, at) => at === index ? stacked : child) });
    }
  }
  const agents = sidebarAgentsGroup(layout), folded = choice === 'folded';
  if (agents && !!agents.minimized !== folded) layout = mapGroups(layout, group => group.id !== agents.id ? group : folded ? { ...group, minimized: true } : (({ minimized: _, ...rest }) => rest)(group));
  return layout === page.layout ? page : { ...page, layout };
}
/** Every page of a workspace as the window's Agents choice has it (see fitSidebarAgents); the same object when none changes. */
export function fitAgentsEverywhere(layout: WorkspaceLayout, choice: SidebarAgents | undefined): WorkspaceLayout {
  const pages = layout.pages.map(page => fitSidebarAgents(page, choice));
  return pages.every((page, index) => page === layout.pages[index]) ? layout : { ...layout, pages };
}
/** Where the sidebar is in a page's root row: 'first', 'last', or not there. */
export function sidebarAt(node: LayoutNode | null): 'first' | 'last' | undefined {
  if (!node || node.type !== 'split' || node.direction !== 'row') return undefined;
  if (isSidebar(node.children[0])) return 'first';
  if (isSidebar(node.children[node.children.length - 1])) return 'last';
  return undefined;
}

/**
 * The narrowest a panel of this kind stays usable, in pixels: a conversation
 * needs room for its messages and composer, Files for its names and find box.
 * A page narrower than its panels' minimums puts panels together as tabs
 * (fitWidth) rather than crushing them.
 */
const MIN_WIDTH: Readonly<Record<string, number>> = { chat: 360, assistant: 360, editor: 320, changes: 320, web: 320, preview: 320, terminal: 280, console: 240, files: 200 };
export const minWidthOf = (kind: string) => MIN_WIDTH[kind] ?? 220;
/** The width an arrangement needs: side-by-side panels add up, stacked ones take the widest; a sidebar takes its fixed width. */
export function minimumWidth(node: LayoutNode | null, sidebarWidth: number): number {
  if (!node) return 0;
  if (node.type === 'group') return isSidebar(node) ? sidebarWidth : Math.max(...node.panels.map(panel => minWidthOf(panel.kind)));
  const widths = node.children.map(child => minimumWidth(child, sidebarWidth));
  return node.direction === 'row' ? widths.reduce((sum, value) => sum + value, 0) : Math.max(...widths);
}
/**
 * The arrangement fitted to `available` pixels: while its panels' minimum
 * widths do not fit, the rightmost two side-by-side panels (never the
 * sidebar) become tabs of one group, showing the one that was on the right
 * (usually the one just opened). Unchanged when it already fits.
 */
export function fitWidth(node: LayoutNode | null, available: number, sidebarWidth: number): LayoutNode | null {
  let current = node;
  for (let step = 0; step < PANEL_LIMIT && current && available > 0 && minimumWidth(current, sidebarWidth) > available; step++) {
    const next = mergeRightmost(current);
    if (next === current) break;
    current = next;
  }
  return current;
}
function mergeRightmost(root: LayoutNode): LayoutNode {
  let done = false;
  const visit = (node: LayoutNode): LayoutNode => {
    if (node.type === 'group') return node;
    const children = [...node.children];
    // Deepest first: a row inside the last column is merged before the row it sits in.
    for (let index = children.length - 1; index >= 0 && !done; index--) children[index] = visit(children[index]);
    if (done || node.direction !== 'row') return { ...node, children };
    const open = children.map((child, index) => ({ child, index })).filter(item => !isSidebar(item.child));
    if (open.length < 2) return { ...node, children };
    const last = open[open.length - 1], previous = open[open.length - 2];
    const moved = groups(last.child), into = groups(previous.child), target = into.reduce((best, group) => group.panels.length >= best.panels.length ? group : best, into[0]);
    const merged: Group = { ...target, panels: [...target.panels, ...moved.flatMap(group => group.panels)], active: moved[moved.length - 1]?.active || target.active, minimized: false };
    const replaced = mapGroups(previous.child, group => group.id === target.id ? merged : group)!;
    const sizes = [...node.sizes];
    sizes[previous.index] += sizes[last.index];
    done = true;
    return normalize({ ...node, children: children.map((child, index) => index === previous.index ? replaced : child).filter((_, index) => index !== last.index), sizes: sizes.filter((_, index) => index !== last.index) })!;
  };
  return visit(root);
}

/** Where a panel of this kind goes the first time it is shown. */
export const defaultDock = (kind: string): DockSide => kind === 'console' || kind === 'actions' ? 'bottom' : kind === 'sessions' || sidebarKinds.has(kind) ? 'left' : 'right';
/** The share of the content area a docked panel takes: a terminal wants columns, the rest stay secondary. */
export const dockShare = (kind: string, side: DockSide) => {
  const across = side === 'left' || side === 'right';
  // An editor beside the conversation gets the larger half: code wants columns.
  if (kind === 'editor') return across ? 0.55 : 0.5;
  if (kind === 'files') return across ? 0.24 : 0.3;
  // A page wants width; the action list wants room for a log.
  if (kind === 'preview' || kind === 'web' || kind === 'browser') return across ? 0.45 : 0.5;
  if (kind === 'actions') return across ? 0.35 : 0.38;
  // Command output opens tall enough that its output, not its tools, fills it.
  if (kind === 'console' && !across) return 0.38;
  return kind === 'terminal' && across ? 0.4 : 0.3;
};
const along = (side: DockSide): 'row' | 'column' => side === 'left' || side === 'right' ? 'row' : 'column';
const before = (side: DockSide) => side === 'left' || side === 'top';
/** `node` with `leaf` added at one edge, taking `share` of it. */
function edge(node: LayoutNode | null, leaf: LayoutNode, side: DockSide, share: number): LayoutNode {
  if (!node) return leaf;
  share = Math.min(0.85, Math.max(0.1, share));
  if (node.type === 'split' && node.direction === along(side)) {
    const rest = node.sizes.map(size => size * (1 - share));
    return normalize({ ...node, children: before(side) ? [leaf, ...node.children] : [...node.children, leaf], sizes: before(side) ? [share, ...rest] : [...rest, share] })!;
  }
  return normalize({ type: 'split', direction: along(side), sizes: before(side) ? [share, 1 - share] : [1 - share, share], children: before(side) ? [leaf, node] : [node, leaf] })!;
}
/**
 * Put `leaf` at one edge of the page's content area, taking `share` of it: a
 * sidebar at the page edge stays there, and the panel docks beside it.
 */
export function dockAt(root: LayoutNode | null, leaf: LayoutNode, side: DockSide, share: number, movingKind = ''): LayoutNode {
  const at = sidebarKinds.has(movingKind) ? undefined : sidebarAt(root);
  if (!at || !root || root.type !== 'split') return edge(root, leaf, side, share);
  const index = at === 'first' ? 0 : root.children.length - 1;
  const sidebar = root.children[index], sidebarSize = root.sizes[index];
  const contentChildren = root.children.filter((_, at) => at !== index), contentSizes = root.sizes.filter((_, at) => at !== index);
  const content = normalize({ type: 'split', direction: 'row', sizes: contentSizes, children: contentChildren });
  const docked = edge(content, leaf, side, share);
  const children = at === 'first' ? [sidebar, docked] : [docked, sidebar], sizes = at === 'first' ? [sidebarSize, 1 - sidebarSize] : [1 - sidebarSize, sidebarSize];
  return normalize({ type: 'split', direction: 'row', sizes, children })!;
}
/** Put a new panel at one edge of the content area, in a group of its own. */
export function insertDocked(node: LayoutNode | null, panel: Panel, side: DockSide, share = dockShare(panel.kind, side)): LayoutNode {
  return dockAt(node, groupFor(panel, groups(node).map(group => group.id)), side, share, panel.kind);
}
/** Move a docked panel to an edge of the content area, in a group of its own. The only panel stays put. */
export function dockPanel(node: LayoutNode | null, panelID: string, side: DockSide, share?: number): LayoutNode | null {
  const panel = panels(node).find(value => value.id === panelID), rest = removePanel(node, panelID);
  if (!panel || !rest) return node;
  return insertDocked(rest, panel, side, share);
}
/** Add a panel as a tab of an existing group (in front). */
export function addToGroup(page: WorkspacePage, groupId: string, panel: Panel): WorkspacePage {
  return mapPageGroups(page, group => group.id === groupId ? { ...group, panels: [...group.panels, panel], active: panel.id, minimized: false } : group);
}
/** Split beside a group: `panel` (already removed elsewhere) goes in a new group on `side` of it, sharing its space. */
export function splitBeside(node: LayoutNode | null, groupId: string, panel: Panel, side: DockSide): LayoutNode | null {
  const leaf = groupFor(panel, groups(node).map(group => group.id));
  const visit = (current: LayoutNode): LayoutNode => {
    if (current.type === 'group') return current.id === groupId ? edge(current, leaf, side, 0.5) : current;
    return { ...current, children: current.children.map(visit) };
  };
  return node ? normalize(visit(node)) : leaf;
}

const shape = (node: LayoutNode | null): unknown => !node ? null : node.type === 'group' ? node.panels.map(panel => panel.id) : [node.direction, node.children.map(shape)];
/** The edge a panel is already docked at, so the menu does not offer a move that changes nothing. */
export function dockedSide(node: LayoutNode | null, panelID: string): DockSide | undefined {
  const current = JSON.stringify(shape(node));
  if (panels(node).length < 2) return undefined;
  return dockSides.find(side => JSON.stringify(shape(dockPanel(node, panelID, side))) === current);
}

/** Show the page's panel of this kind (in front, restored) or dock a new one; never a second copy. */
export function showPanel(page: WorkspacePage, kind: string, side: DockSide, newID: () => string): { page: WorkspacePage; id: string; added: boolean } {
  const existing = shownPanels(page).find(panel => panel.kind === kind && !panel.detached);
  if (existing) return { page: activatePanel(page, existing.id), id: existing.id, added: false };
  const id = newID();
  return { page: { ...page, layout: insertDocked(page.layout, { id, kind, binding: { mode: 'follow' }, detached: false }, side) }, id, added: true };
}
/**
 * Files and the editor work as a pair: showing one while the other is docked
 * and alone in its group puts the new one beside it (Files on the left, the
 * editor on the right). Otherwise this is showPanel.
 */
export function showCodePanel(page: WorkspacePage, kind: 'files' | 'editor', side: DockSide, newID: () => string): { page: WorkspacePage; id: string; added: boolean } {
  if (shownPanels(page).some(panel => panel.kind === kind && !panel.detached)) return showPanel(page, kind, side, newID);
  const partner = groups(page.layout).find(group => group.panels.some(panel => panel.kind === (kind === 'files' ? 'editor' : 'files')));
  if (!partner) return showPanel(page, kind, side, newID);
  const id = newID(), panel: Panel = { id, kind, binding: { mode: 'follow' }, detached: false };
  const pair = (node: Group) => edge(node, groupFor(panel, groups(page.layout).map(group => group.id)), kind === 'files' ? 'left' : 'right', kind === 'files' ? 0.3 : 0.7);
  // The pair takes room enough for code: at least 62% of the split it is in.
  const visit = (node: LayoutNode): LayoutNode => {
    if (node.type === 'group') return node.id === partner.id ? pair(node) : node;
    const index = node.children.findIndex(child => child.type === 'group' && child.id === partner.id), share = node.sizes[index];
    const sizes = index >= 0 && node.direction === 'row' && share < 0.55 ? node.sizes.map((size, at) => at === index ? 0.62 : size * 0.38 / (1 - share)) : node.sizes;
    return { ...node, sizes, children: node.children.map(visit) };
  };
  return { page: { ...page, layout: normalize(visit(page.layout!)) }, id, added: true };
}

// ---------------------------------------------------------------------------
// Conversations.

// Every chat panel is a tab of one conversation, as editors are tabs of files
// in an IDE. A chat group's **preview tab** (`preview`, its title in italics)
// is where a single click shows a conversation: the next single click
// replaces it. A **regular tab** stays on its conversation until it is closed:
// a double-click, typing in it, ⌘-click (in the background) or Open in new
// tab make one. Every chat keeps its own conversation (a `pinned` binding); a
// chat following the page (`follow`, from older pages and templates) becomes
// the preview tab of what it shows (`settleChats`). The page's selection is
// the conversation of the tab in front: bringing a chat tab forward selects
// its conversation, so the sidebar, command output and Changes follow it.

/** A chat shown on the page (not a card for one in its own window). */
export const isChatTab = (panel: Panel) => panel.kind === 'chat' && !panel.detached;
const visibleChats = (page: WorkspacePage) => shownPanels(page).filter(isChatTab);
/** Two targets are the same conversation: the same session, or, with none, the same workspace's start. */
export const sameConversation = (a: ViewTarget, b: ViewTarget) => a.sessionId ? a.sessionId === b.sessionId : !b.sessionId && a.collectionId === b.collectionId;
const conversationKey = (target: ViewTarget) => target.sessionId ? `s:${target.sessionId}` : `w:${target.collectionId}`;
/** A chat tab on a conversation; `preview` makes it its group's preview tab. */
export const chatTab = (panelID: string, target: ViewTarget, preview = false): Panel => ({ id: panelID, kind: 'chat', binding: { mode: 'pinned', target: { ...target } }, detached: false, ...(preview ? { preview: true } : {}) });
const withoutPreview = ({ preview: _, ...panel }: Panel): Panel => panel;
/** The chat tab showing a conversation on this page, if any: a regular tab before a preview (dedupeChats keeps the regular one). */
export function tabFor(page: WorkspacePage, target: ViewTarget): Panel | undefined {
  const showing = visibleChats(page).filter(panel => sameConversation(resolveTarget(panel, page.selection), target));
  return showing.find(panel => !panel.preview && panel.binding.mode === 'pinned') || showing[0];
}
/** The page selecting a conversation: its command stays chosen while it is the same conversation. */
export function withSelection(page: WorkspacePage, target: ViewTarget): WorkspacePage {
  const commandId = target.commandId || (target.sessionId && target.sessionId === page.selection.sessionId ? page.selection.commandId : '');
  return { ...page, selection: { collectionId: target.collectionId, sessionId: target.sessionId, commandId } };
}

/**
 * Chats as tabs: a chat following the page becomes a tab of the conversation
 * it shows. With preview tabs (`previews`, the person's choice) it is the
 * group's preview tab and a group keeps one (the one in front, else its
 * first); without, every chat is a kept tab, so a conversation already open
 * is never replaced. The same page when nothing changes.
 */
export function settleChats(page: WorkspacePage, previews = true): WorkspacePage {
  const settle = (group: Group): Group => {
    if (!group.panels.some(panel => panel.preview || (isChatTab(panel) && panel.binding.mode === 'follow'))) return group;
    const converted = group.panels.map(panel => isChatTab(panel) && panel.binding.mode === 'follow' ? chatTab(panel.id, page.selection, previews) : !previews && panel.preview ? withoutPreview(panel) : panel);
    const marked = converted.filter(panel => isChatTab(panel) && panel.preview);
    const keep = (marked.find(panel => panel.id === group.active) || marked[0])?.id;
    const panels = converted.map(panel => panel.preview && (panel.id !== keep || !isChatTab(panel)) ? withoutPreview(panel) : panel);
    return panels.every((panel, index) => panel === group.panels[index]) ? group : { ...group, panels };
  };
  const before = pageGroups(page), next = mapPageGroups(page, settle);
  return pageGroups(next).every((group, index) => group === before[index]) ? page : next;
}
/** Every page of a layout with its chats as tabs (settleChats); the same layout when none changes. */
export function settleLayout(layout: WorkspaceLayout, previews = true): WorkspaceLayout {
  const pages = layout.pages.map(page => settleChats(page, previews));
  return pages.every((page, index) => page === layout.pages[index]) ? layout : { ...layout, pages };
}

/**
 * One visible chat per conversation: of two tabs on the same conversation the
 * regular one stays and a preview goes (else the first stays). Never drops
 * the last chat.
 */
export function dedupeChats(page: WorkspacePage, selection: ViewTarget = page.selection): WorkspacePage {
  const chats = visibleChats(page), shown = new Set<string>();
  const preview = (panel: Panel) => !!panel.preview || panel.binding.mode === 'follow';
  let result = page;
  for (const panel of [...chats.filter(value => !preview(value)), ...chats.filter(preview)]) {
    const key = conversationKey(resolveTarget(panel, selection));
    if (!shown.has(key)) { shown.add(key); continue; }
    result = removeFromPage(result, panel.id);
  }
  return result;
}

/**
 * The group a conversation opens in: the one showing the selected
 * conversation, else the one with a preview tab, else the first with a chat.
 */
export function chatGroup(page: WorkspacePage): Group | undefined {
  const all = pageGroups(page), chats = visibleChats(page);
  const holding = (panel?: Panel) => panel && all.find(group => group.panels.includes(panel));
  return holding(chats.find(panel => sameConversation(resolveTarget(panel, page.selection), page.selection)))
    || holding(chats.find(panel => panel.preview))
    || holding(chats[0]);
}
/** A new tab in a group: right after `after`, at `index`, or right after the tab in front; `front` brings it forward. */
export function insertTab(page: WorkspacePage, groupId: string, panel: Panel, place: { after?: string; index?: number; front: boolean }): WorkspacePage {
  return mapPageGroups(page, group => {
    if (group.id !== groupId) return group;
    const anchor = place.after ? group.panels.findIndex(value => value.id === place.after) : -1;
    const at = anchor >= 0 ? anchor + 1 : place.index !== undefined ? Math.max(0, Math.min(group.panels.length, place.index)) : group.panels.findIndex(value => value.id === group.active) + 1;
    const panels = [...group.panels.slice(0, at), panel, ...group.panels.slice(at)];
    const { minimized: _, ...open } = group;
    return place.front ? { ...open, panels, active: panel.id } : { ...group, panels };
  });
}

/**
 * How a conversation is opened: `preview` (a single click: the chat group's
 * preview tab shows it), `tab` (a regular tab, in front: a double-click, Open
 * in new tab, a new conversation) or `background` (a regular tab behind the
 * one in front: ⌘-click, middle-click, a related conversation).
 */
export type OpenMode = 'preview' | 'tab' | 'background';
export type OpenOptions = {
  mode: OpenMode;
  /** The group it opens in (a group's +, a closed tab reopened); else the chat group. */
  group?: string;
  /** Right after this tab: a related conversation beside its parent. */
  after?: string;
  /** At this place among the group's tabs: a closed tab reopened where it was. */
  index?: number;
};
/** `full`: the page already has PANEL_LIMIT panels, so nothing was opened. */
export type Opened = { page: WorkspacePage; id: string; added: boolean; full?: boolean };

/**
 * Open a conversation as a tab. A conversation already in a tab is brought
 * forward there (a background open leaves the page as it is), never shown
 * twice. A foreground open uses the empty workspace starter or, in preview
 * mode, the chat group's preview. Otherwise it gets a new tab if there is room. Opening it in front selects it.
 */
export function openConversation(page: WorkspacePage, target: ViewTarget, options: OpenOptions, newID: () => string): Opened {
  const settled = settleChats(page), existing = tabFor(settled, target);
  if (existing) {
    if (options.mode === 'background') return { page: settled, id: existing.id, added: false };
    const kept = options.mode === 'tab' ? keepTab(settled, existing.id) : settled;
    return { page: dedupeChats(withSelection(activatePanel(kept, existing.id), target)), id: existing.id, added: false };
  }
  const all = pageGroups(settled);
  const group = (options.group ? all.find(value => value.id === options.group) : undefined) || chatGroup(settled);
  const starter = target.sessionId && group?.panels.find(panel => isChatTab(panel) && !resolveTarget(panel, settled.selection).sessionId);
  const preview = options.mode === 'preview' && group?.panels.find(panel => isChatTab(panel) && panel.preview);
  const replace = preview || starter;
  if (replace && options.mode !== 'background') {
    const shown = mapPagePanels(settled, panel => panel.id === replace.id ? chatTab(panel.id, target, options.mode === 'preview') : panel);
    return { page: dedupeChats(withSelection(activatePanel(shown, replace.id), target)), id: replace.id, added: false };
  }
  if (allPanels(settled).length >= PANEL_LIMIT) return { page: settled, id: '', added: false, full: true };
  const panel = chatTab(newID(), target, options.mode === 'preview'), front = options.mode !== 'background';
  const placed = group ? insertTab(settled, group.id, panel, { after: options.after, index: options.index, front }) : { ...settled, layout: insertDocked(settled.layout, panel, 'right', 0.5) };
  return { page: front ? withSelection(placed, target) : placed, id: panel.id, added: true };
}
/** The chat panel that shows a selected conversation: its tab, else the preview tab (see openConversation). */
export function routeChat(page: WorkspacePage, selection: ViewTarget, newID: () => string): Opened {
  return openConversation(page, selection, { mode: 'preview' }, newID);
}
/**
 * A conversation opened from another one (a review of it, a child an
 * orchestrator started): a tab right after its parent's, behind it by default,
 * so the conversation in front is not replaced.
 */
export function openRelated(page: WorkspacePage, parent: ViewTarget, target: ViewTarget, options: { background: boolean }, newID: () => string): Opened {
  const loose = settleChats(page), found = parent.sessionId ? tabFor(loose, parent) : undefined;
  // The parent stays: a preview parent is kept open first, so nothing opened later replaces it.
  const settled = found?.preview ? keepTab(loose, found.id) : loose, home = found && tabFor(settled, parent), group = home && groupOf(settled, home.id);
  return openConversation(settled, target, { mode: options.background ? 'background' : 'tab', ...(home && group ? { group: group.id, after: home.id } : {}) }, newID);
}
/** A preview tab kept open: it becomes a regular tab. */
export function keepTab(page: WorkspacePage, panelID: string): WorkspacePage {
  return shownPanels(page).some(value => value.id === panelID && value.preview) ? mapPagePanels(page, value => value.id === panelID ? withoutPreview(value) : value) : page;
}
/**
 * A regular tab made its group's preview tab ("Use as preview tab"): it keeps
 * its conversation until the next single click replaces it; the group's
 * other preview tab, if any, becomes a regular tab.
 */
export function previewTab(page: WorkspacePage, panelID: string): WorkspacePage {
  const settled = settleChats(page), group = groupOf(settled, panelID), panel = group?.panels.find(value => value.id === panelID);
  if (!group || !panel || !isChatTab(panel) || panel.preview) return page;
  return mapPagePanels(settled, value => value.id === panelID ? { ...value, preview: true } : value.preview && group.panels.some(member => member.id === value.id) ? withoutPreview(value) : value);
}
/**
 * A panel closed. A chat tab in front hands the front to its neighbour (the
 * tab to its right, else to its left), and when it showed the selected
 * conversation, the neighbour's conversation is selected.
 */
export function closeTab(page: WorkspacePage, panelID: string): WorkspacePage {
  const group = groupOf(page, panelID), panel = group?.panels.find(value => value.id === panelID), next = removeFromPage(page, panelID);
  if (!group || !panel || panel.kind !== 'chat') return next;
  const index = group.panels.indexOf(panel);
  const neighbour = group.active === panelID ? group.panels[index + 1] || group.panels[index - 1] : undefined;
  const fronted = neighbour ? mapPageGroups(next, value => value.id === group.id ? { ...value, active: neighbour.id } : value) : next;
  if (!sameConversation(resolveTarget(panel, page.selection), page.selection)) return fronted;
  const other = neighbour && isChatTab(neighbour) ? neighbour : visibleChats(fronted).find(candidate => groupOf(fronted, candidate.id)?.active === candidate.id);
  return withSelection(fronted, other ? resolveTarget(other, page.selection) : { ...page.selection, sessionId: '', commandId: '' });
}
/** What "Close other tabs" (or "Close tabs to the right") closes: tabs of the panel's group, never the sidebar's lists nor a card for a window. */
export function tabsToClose(page: WorkspacePage, panelID: string, which: 'others' | 'right'): Panel[] {
  const group = groupOf(page, panelID);
  if (!group) return [];
  const index = group.panels.findIndex(panel => panel.id === panelID);
  return group.panels.filter((panel, at) => panel.id !== panelID && (which === 'others' || at > index) && !sidebarKinds.has(panel.kind) && !panel.detached);
}
/**
 * After the person rearranged the dock (a tab clicked or dragged, ⌃1, ⌃⇥):
 * a chat tab brought to the front of its group selects its conversation.
 */
export function followFrontTab(before: WorkspacePage, after: WorkspacePage): WorkspacePage {
  const was = new Map(pageGroups(before).map(group => [group.id, group.active]));
  for (const group of pageGroups(after)) {
    const front = group.panels.find(panel => panel.id === group.active);
    if (!front || !isChatTab(front) || was.get(group.id) === front.id) continue;
    const target = resolveTarget(front, after.selection);
    return sameConversation(target, after.selection) ? after : withSelection(after, target);
  }
  return after;
}
/**
 * A workspace's conversations as tabs ("Open a workspace's conversations as
 * tabs"): those given (open ones, most recent first) that have no tab yet get
 * a kept tab each, after the chat group's tabs and behind the one in front,
 * until the page has `cap` conversation tabs (or no room). A tab still on the
 * workspace's start (no conversation yet) takes the first and selects it. A
 * page without a chat is left as it is; so is one that already has them all.
 */
export function showWorkspaceTabs(page: WorkspacePage, conversations: readonly ViewTarget[], cap: number, newID: () => string): WorkspacePage {
  const chats = visibleChats(page);
  if (!chats.length) return page;
  const missing = conversations.filter(target => target.sessionId && !tabFor(page, target));
  if (!missing.length) return page;
  let next = page;
  const start = chats.find(panel => !resolveTarget(panel, page.selection).sessionId);
  if (start) {
    const first = missing.shift()!;
    next = withSelection(mapPagePanels(next, panel => panel.id === start.id ? chatTab(panel.id, first) : panel), first);
  }
  const group = chatGroup(next);
  if (!group) return next;
  for (const target of missing) {
    if (visibleChats(next).length >= cap || allPanels(next).length >= PANEL_LIMIT) break;
    const last = groupOf(next, group.panels[0].id)?.panels.at(-1);
    next = insertTab(next, group.id, chatTab(newID(), target), { after: last?.id, front: false });
  }
  return next;
}
/**
 * A conversation that is gone (deleted): its tabs leave every page, and a
 * card for it in its own window too. The same layout when it had none.
 */
export function forgetConversation(layout: WorkspaceLayout, sessionId: string): WorkspaceLayout {
  if (!sessionId) return layout;
  const shows = (page: WorkspacePage, panel: Panel) => panel.kind === 'chat' && resolveTarget(panel, page.selection).sessionId === sessionId;
  const pages = layout.pages.map(page => {
    const gone = allPanels(page).filter(panel => shows(page, panel));
    return gone.length ? gone.reduce((result, panel) => closeTab(result, panel.id), page) : page;
  });
  return pages.every((page, index) => page === layout.pages[index]) ? layout : { ...layout, pages };
}
/** The preview tab a full page can close to make room: the first one, in reading order. */
export function oldestPreview(page: WorkspacePage): Panel | undefined { return visibleChats(page).find(panel => panel.preview); }
/** A page name from a conversation's title: one line, at most 64 characters. */
export function pageName(title: string): string {
  const clean = title.replace(/[\u0000-\u001f\u007f]+/g, ' ').replace(/\s+/g, ' ').trim();
  return (clean.length > 64 ? `${clean.slice(0, 63)}…` : clean) || 'Conversation';
}
/**
 * A conversation moved to a page of its own (named after it, in front): its
 * tab leaves the page it was on, or a new tab is made for it.
 */
export function moveToNewPage(layout: WorkspaceLayout, fromPage: string, target: ViewTarget, title: string, newID: () => string): { layout: WorkspaceLayout; page: string } | { error: string } {
  if (layout.pages.length >= PAGE_LIMIT) return { error: `A workspace has at most ${PAGE_LIMIT} pages; remove one to move this conversation to a page of its own.` };
  const from = layout.pages.find(page => page.id === fromPage), existing = from && tabFor(from, target);
  const panel = existing ? { ...withoutPreview(existing), binding: { mode: 'pinned' as const, target: { ...resolveTarget(existing, from!.selection) } } } : chatTab(newID(), target);
  const id = newID(), created = emptyPage(id, pageName(title), { ...target, commandId: '' }, groupFor(panel));
  const pages = layout.pages.map(page => page.id === fromPage && existing ? closeTab(page, existing.id) : page);
  return { layout: { ...layout, pages: [...pages, created], selectedPage: id }, page: id };
}
/**
 * A review beside the work it reviews: `openRelated` in the background, right
 * after the tab of the conversation reviewed (`beside`), else in the chat
 * group; one already open is kept as it is. Undefined when the page has no
 * chat to join or is full (the caller says so).
 */
export function openBesideChat(page: WorkspacePage, target: ViewTarget, beside: string, newID: () => string): { page: WorkspacePage; id: string; added: boolean } | undefined {
  if (!visibleChats(page).length) return undefined;
  const opened = openRelated(page, { collectionId: target.collectionId, sessionId: beside, commandId: '' }, target, { background: true }, newID);
  return opened.full ? undefined : { page: opened.page, id: opened.id, added: opened.added };
}
/** The page whose visible chat shows a conversation: the selected page first. */
export function pageShowing(layout: WorkspaceLayout, sessionId: string): WorkspacePage | undefined {
  const ordered = [...layout.pages.filter(page => page.id === layout.selectedPage), ...layout.pages.filter(page => page.id !== layout.selectedPage)];
  return ordered.find(page => visibleChats(page).some(panel => resolveTarget(panel, page.selection).sessionId === sessionId));
}
/** Content panels: what the page shows besides the workspace list and command output. */
const content = (panel: Panel) => !sidebarKinds.has(panel.kind) && panel.kind !== 'console';
/**
 * A chat moving to its own window: it leaves the page, unless it is the last
 * content panel, which stays as a card so the workspace list does not fill the
 * page. It remembers where it was (`originOf`), so putting it back returns it there.
 */
export function detachChat(page: WorkspacePage, panelID: string, target: ViewTarget): WorkspacePage {
  const panel = shownPanels(page).find(value => value.id === panelID);
  if (!panel) return page;
  const moved: Panel = { ...withoutPreview(panel), binding: { mode: 'pinned', target: { ...target } }, detached: true };
  const others = shownPanels(page).filter(value => value.id !== panelID && content(value) && !value.detached);
  if (!others.length) return mapPagePanels(page, value => value.id === panelID ? moved : value);
  const origin = originOf(page.layout, panelID);
  return { ...removeFromPage(page, panelID), detached: [...page.detached, { ...moved, ...(origin ? { origin } : {}) }] };
}
/** A chat brought back from its window rejoins the page where it was (`placeBack`), unless another panel already shows that conversation. */
export function returnChat(page: WorkspacePage, panelID: string, selection: ViewTarget): WorkspacePage {
  const panel = allPanels(page).find(value => value.id === panelID);
  if (!panel) return page;
  const session = resolveTarget(panel, selection).sessionId;
  if (visibleChats(page).some(other => other.id !== panelID && resolveTarget(other, selection).sessionId === session)) return removeFromPage(page, panelID);
  const { origin, ...rest } = panel, back: Panel = { ...rest, detached: false };
  if (!page.detached.some(value => value.id === panelID)) return mapPagePanels(page, value => value.id === panelID ? back : value);
  return placeBack({ ...page, detached: page.detached.filter(value => value.id !== panelID) }, back, origin);
}
/**
 * Where a docked panel sits, to put it back there later: its group and place
 * among the tabs and, for a panel alone in its group (the group leaves with
 * it), the groups beside it in its split, which side of them it was on and
 * its share of the space the two took.
 */
export function originOf(root: LayoutNode | null, panelID: string): PanelOrigin | undefined {
  // (A minimized neighbour in a column loses its minimized state once alone in a row; it is remembered here.)
  const group = groups(root).find(value => value.panels.some(panel => panel.id === panelID));
  if (!group) return undefined;
  const origin: PanelOrigin = { group: group.id, index: group.panels.findIndex(panel => panel.id === panelID) };
  if (group.panels.length > 1) return origin;
  let found: Pick<PanelOrigin, 'beside' | 'side' | 'share'> | undefined;
  const visit = (node: LayoutNode | null) => {
    if (found || node?.type !== 'split') return;
    const index = node.children.findIndex(child => child.type === 'group' && child.id === group.id);
    if (index < 0) { node.children.forEach(visit); return; }
    const row = node.direction === 'row', previous = node.children[index - 1], next = node.children[index + 1];
    // Prefer the neighbour that is not the sidebar: the panel belongs to the content area.
    const sibling = previous && (!next || !isSidebar(previous))
      ? { node: previous, size: node.sizes[index - 1], side: (row ? 'right' : 'bottom') as DockSide }
      : next ? { node: next, size: node.sizes[index + 1], side: (row ? 'left' : 'top') as DockSide } : undefined;
    if (!sibling) return;
    const share = node.sizes[index] / (node.sizes[index] + sibling.size);
    found = { beside: groups(sibling.node).map(value => value.id), side: sibling.side, share: round(Math.min(0.9, Math.max(0.1, share))), ...(sibling.node.type === 'group' && sibling.node.minimized ? { minimized: true } : {}) };
  };
  visit(root);
  return { ...origin, ...found };
}
/** The deepest node that holds every group of `ids` still on the page. */
function holderOf(root: LayoutNode | null, ids: ReadonlySet<string>): LayoutNode | undefined {
  const present = groups(root).filter(group => ids.has(group.id)).map(group => group.id);
  if (!root || !present.length) return undefined;
  const holds = (node: LayoutNode) => { const inside = new Set(groups(node).map(group => group.id)); return present.every(id => inside.has(id)); };
  let node: LayoutNode = root;
  for (;;) {
    if (node.type === 'group') return node;
    const child: LayoutNode | undefined = node.children.find(holds);
    if (!child) return node;
    node = child;
  }
}
/** The group with the most room in the content area (never the sidebar, nor a minimized group), by shares. */
export function largestGroup(root: LayoutNode | null): Group | undefined {
  let best: { group: Group; area: number } | undefined;
  const visit = (node: LayoutNode, area: number) => {
    if (node.type === 'group') { if (!isSidebar(node) && !node.minimized && (!best || area > best.area)) best = { group: node, area }; return; }
    node.children.forEach((child, index) => visit(child, area * node.sizes[index]));
  };
  if (root) visit(root, 1);
  return best?.group;
}
/**
 * A panel coming back from its own window: into the group it left, at its
 * place among the tabs; else beside the groups it was next to, on the same
 * side and with its share; else beside the largest group; on an empty page,
 * the page.
 */
export function placeBack(page: WorkspacePage, panel: Panel, origin?: PanelOrigin): WorkspacePage {
  const { origin: _, ...plain } = panel, back: Panel = { ...plain, detached: false };
  const home = origin && pageGroups(page).find(group => group.id === origin.group);
  if (origin && home) return mapPageGroups(page, group => group.id === home.id ? { ...group, panels: [...group.panels.slice(0, origin.index), back, ...group.panels.slice(origin.index)], active: back.id, minimized: false } : group);
  const leaf = groupFor(back, pageGroups(page).map(group => group.id));
  const beside = origin?.beside && origin.side ? holderOf(page.layout, new Set(origin.beside)) : undefined;
  if (page.layout && beside && origin?.side) {
    const placed = replaceNode(page.layout, beside, edge(beside, leaf, origin.side, origin.share ?? 0.5));
    const column = origin.side === 'top' || origin.side === 'bottom';
    return { ...page, layout: origin.minimized && column && beside.type === 'group' ? mapGroups(placed, group => group.id === beside.id ? { ...group, minimized: true } : group) : placed };
  }
  const largest = largestGroup(page.layout);
  if (page.layout && largest) return { ...page, layout: replaceNode(page.layout, largest, edge(largest, leaf, 'right', 0.5)) };
  return { ...page, layout: insertDocked(page.layout, back, 'right', 0.5) };
}
function replaceNode(root: LayoutNode, from: LayoutNode, to: LayoutNode): LayoutNode {
  const visit = (node: LayoutNode): LayoutNode => node === from ? to : node.type === 'group' ? node : { ...node, children: node.children.map(visit) };
  return normalize(visit(root))!;
}
/**
 * Put a chat back from its own window, in whichever page of `layout` holds
 * it: the layout with the chat docked again and the page it is on, or
 * undefined when no page has it in a window (it was put back already).
 */
export function returnDetached(layout: WorkspaceLayout, panelID: string): { layout: WorkspaceLayout; page: string } | undefined {
  const page = layout.pages.find(value => allPanels(value).some(panel => panel.id === panelID && panel.detached));
  if (!page) return undefined;
  const next = returnChat(page, panelID, page.selection);
  return { layout: { ...layout, pages: layout.pages.map(value => value.id === page.id ? next : value) }, page: page.id };
}
/** Every chat of `layout` that is in a window of its own, with the page it belongs to. */
export function detachedChats(layout: WorkspaceLayout): { page: WorkspacePage; panel: Panel }[] {
  return layout.pages.flatMap(page => allPanels(page).filter(panel => panel.detached).map(panel => ({ page, panel })));
}
/** A detached panel already showing this conversation, so selecting it focuses that window. */
export function detachedFor(page: WorkspacePage, selection: ViewTarget): Panel | undefined {
  return allPanels(page).find(panel => panel.detached && panel.kind === 'chat' && panel.binding.mode === 'pinned' && !!selection.sessionId && panel.binding.target.sessionId === selection.sessionId);
}

// ---------------------------------------------------------------------------
// Pages.

const leaf = (panel: Panel, minimized = false): Group => ({ type: 'group', id: groupID(panel.id), panels: [panel], active: panel.id, ...(minimized ? { minimized } : {}) });
const followPanel = (panelID: string, kind: string): Panel => ({ id: panelID, kind, binding: { mode: 'follow' }, detached: false });
export const emptyPage = (pageID: string, name: string, selection: ViewTarget, layout: LayoutNode | null): WorkspacePage => ({ id: pageID, name, selection: { ...selection }, layout, floating: [], popouts: [], detached: [] });
/** The Work page: the workspace list above the agents list (unless the window's choice closed or folded it), the conversation, and command output below it, minimized until a run starts. */
export function defaultLayout(selection = emptyTarget(), agents?: SidebarAgents): WorkspaceLayout {
  const panel = (panelID: string, kind: string) => followPanel(selection.collectionId ? `${selection.collectionId}-${panelID}` : panelID, kind);
  const layout: LayoutNode = { type: 'split', direction: 'row', sizes: [0.2, 0.8], children: [
    { type: 'split', direction: 'column', sizes: [1 - AGENTS_SHARE, AGENTS_SHARE], children: [leaf(panel('work-collections', 'collections')), leaf(panel('work-agents', 'agents'))] },
    // The conversation is the page's preview tab: choosing another one in the sidebar shows it there.
    { type: 'split', direction: 'column', sizes: [0.62, 0.38], children: [leaf(chatTab(panel('work-chat', 'chat').id, selection, true)), leaf(panel('work-console', 'console'), true)] },
  ] };
  return { version: 2, selectedPage: 'work', pages: [fitSidebarAgents(emptyPage('work', 'Work', selection, layout), agents)] };
}
export function clonePage(page: WorkspacePage, newID: () => string): WorkspacePage {
  const ids = new Map<string, string>();
  const copy = (panel: Panel): Panel => { const next = newID(); ids.set(panel.id, next); return { ...panel, id: next, detached: false, binding: panel.binding.mode === 'follow' ? { mode: 'follow' } : { mode: 'pinned', target: { ...panel.binding.target } } }; };
  const group = (value: Group): Group => { const panelsCopy = value.panels.filter(panel => !page.detached.includes(panel)).map(copy); return { ...value, id: groupID(panelsCopy[0]?.id || newID()), panels: panelsCopy, active: ids.get(value.active) || panelsCopy[0]?.id }; };
  // A copy shows everything in place: nothing in another window.
  let layout = mapGroups(page.layout, group);
  for (const popped of page.popouts) layout = dockAt(layout, group(popped), 'right', 0.3);
  return { ...emptyPage(newID(), `${page.name.slice(0, 59)} copy`, page.selection, layout), floating: page.floating.map(value => ({ ...value, group: group(value.group) })) };
}
/** Remove the retired built-in Monitor page without resetting other saved pages. */
export function retireMonitor(layout: WorkspaceLayout): WorkspaceLayout {
  const pages = layout.pages.filter(page => !(page.id === 'monitor' && page.name === 'Monitor'));
  if (pages.length === layout.pages.length) return layout;
  if (!pages.length) return defaultLayout(layout.pages[0]?.selection);
  return { ...layout, pages, selectedPage: pages.some(page => page.id === layout.selectedPage) ? layout.selectedPage : pages[0].id };
}
/** Groups popped out into windows of a window that has since closed come back docked on the right. */
export function redockPopouts(page: WorkspacePage): WorkspacePage {
  if (!page.popouts.length) return page;
  let layout = page.layout;
  for (const group of page.popouts) layout = dockAt(layout, group, 'right', 0.3, group.panels[0]?.kind);
  return { ...page, layout, popouts: [] };
}
/** Retire the old workspace Missions surface, including popped-out and floating copies. */
function retireMissionPanels(layout: WorkspaceLayout, agents?: SidebarAgents): WorkspaceLayout {
  const pages = layout.pages.flatMap(page => {
    const obsolete = allPanels(page).filter(panel => panel.kind === 'missions');
    if (!obsolete.length) return [page];
    const next = obsolete.reduce((rest, panel) => removeFromPage(rest, panel.id), page);
    return allPanels(next).length ? [next] : [];
  });
  if (!pages.length) return defaultLayout(layout.pages.find(page => page.id === layout.selectedPage)?.selection, agents);
  if (pages.length === layout.pages.length && pages.every((page, index) => page === layout.pages[index])) return layout;
  return { ...layout, pages, selectedPage: pages.some(page => page.id === layout.selectedPage) ? layout.selectedPage : pages[0].id };
}

/** What loading a saved page fixes: windows that are gone, chats that follow the page (they become its preview tab), duplicate chats, Agents hidden behind Workspaces, and Agents as the window has it (`agents`, see fitSidebarAgents). */
export function repairLayout(layout: WorkspaceLayout, agents?: SidebarAgents, previews = true): WorkspaceLayout {
  layout = retireMissionPanels(layout, agents);
  const pages = layout.pages.map(page => {
    const docked = redockPopouts(page), stacked = stackSidebar(docked.layout);
    const next = fitSidebarAgents(dedupeChats(settleChats(stacked === docked.layout ? docked : { ...docked, layout: stacked }, previews), page.selection), agents);
    return next.layout === page.layout && next.floating === page.floating && next.popouts === page.popouts && next.detached === page.detached ? page : next;
  });
  return pages.every((page, index) => page === layout.pages[index]) ? layout : { ...layout, pages };
}

/**
 * Switching workspace never moves a divider: the target keeps its own pages
 * and panels, but takes the current shares wherever its arrangement has the
 * same shape (the sidebar has one fixed width everywhere).
 */
export function adoptGeometry(to: WorkspaceLayout, from: WorkspaceLayout): WorkspaceLayout {
  const same = (a: LayoutNode | null, b: LayoutNode | null): LayoutNode | null => {
    // A stacked sidebar has the shape of a content column: never one for the other.
    if (a && b && isSidebar(a) !== isSidebar(b)) return a;
    if (a?.type === 'split' && b?.type === 'split' && a.direction === b.direction && a.children.length === b.children.length) return { ...a, sizes: [...b.sizes], children: a.children.map((child, index) => same(child, b.children[index])!) };
    if (a?.type === 'group' && b?.type === 'group' && !!a.minimized !== !!b.minimized) return { ...a, minimized: b.minimized };
    return a;
  };
  return { ...to, pages: to.pages.map(page => {
    const match = from.pages.find(value => value.id === page.id) || (page.id === to.selectedPage ? from.pages.find(value => value.id === from.selectedPage) : undefined);
    return match ? { ...page, layout: same(page.layout, match.layout) } : page;
  }) };
}

// ---------------------------------------------------------------------------
// Saved form.

function parseGroup(value: unknown, panelIDs: Set<string>, groupIDs: Set<string>): Group {
  if (!object(value) || value.type !== 'group' || !keys(value, ['type', 'id', 'panels', 'active', 'minimized']) || !id(value.id) || groupIDs.has(value.id) || !Array.isArray(value.panels) || !value.panels.length || value.panels.length > PANEL_LIMIT || (value.minimized !== undefined && typeof value.minimized !== 'boolean')) throw new Error('Invalid panel group');
  groupIDs.add(value.id);
  const parsed = value.panels.map(panel => parsePanel(panel, panelIDs));
  if (typeof value.active !== 'string' || !parsed.some(panel => panel.id === value.active)) throw new Error('Invalid active panel');
  return { type: 'group', id: value.id, panels: parsed, active: value.active, ...(value.minimized ? { minimized: true } : {}) };
}
function parsePanel(p: unknown, panelIDs: Set<string>): Panel {
  if (!object(p) || !keys(p, ['id', 'kind', 'binding', 'detached', 'origin', 'preview']) || !id(p.id) || panelIDs.has(p.id) || typeof p.kind !== 'string' || !kindPattern.test(p.kind) || typeof p.detached !== 'boolean' || !object(p.binding) || (p.preview !== undefined && typeof p.preview !== 'boolean')) throw new Error('Invalid panel');
  panelIDs.add(p.id);
  const bindingValid = p.binding.mode === 'follow' ? keys(p.binding, ['mode']) : p.binding.mode === 'pinned' && keys(p.binding, ['mode', 'target']) && validTarget(p.binding.target);
  if (!bindingValid) throw new Error('Invalid panel binding');
  if (p.detached && !(p.kind === 'chat' && p.binding.mode === 'pinned' && !!(p.binding.target as ViewTarget).sessionId)) throw new Error('Only a chat pinned to a conversation opens in its own window');
  const origin = p.origin === undefined ? undefined : parseOrigin(p.origin);
  // Only a chat on the page is a preview tab.
  const preview = p.preview === true && p.kind === 'chat' && !p.detached;
  return { id: p.id, kind: p.kind, binding: p.binding as Binding, detached: p.detached, ...(origin && p.detached ? { origin } : {}), ...(preview ? { preview } : {}) };
}
function parseOrigin(value: unknown): PanelOrigin {
  if (!object(value) || !keys(value, ['group', 'index', 'beside', 'side', 'share', 'minimized']) || (value.minimized !== undefined && typeof value.minimized !== 'boolean') || !id(value.group) || !Number.isInteger(value.index) || (value.index as number) < 0 || (value.index as number) > PANEL_LIMIT
    || (value.beside !== undefined && !(Array.isArray(value.beside) && value.beside.length <= PANEL_LIMIT && value.beside.every(item => id(item))))
    || (value.side !== undefined && !isDockSide(value.side)) || (value.share !== undefined && !(finite(value.share) && value.share > 0 && value.share < 1))) throw new Error('Invalid panel origin');
  return { group: value.group as string, index: value.index as number, ...(value.beside ? { beside: value.beside as string[] } : {}), ...(value.side ? { side: value.side as DockSide } : {}), ...(value.share !== undefined ? { share: value.share as number } : {}), ...(value.minimized ? { minimized: true } : {}) };
}
function parseNode(value: unknown, depth: number, panelIDs: Set<string>, groupIDs: Set<string>): LayoutNode {
  if (depth > DEPTH_LIMIT) throw new Error('Page layout is nested too deeply');
  if (object(value) && value.type === 'split') {
    if (!keys(value, ['type', 'direction', 'sizes', 'children']) || !['row', 'column'].includes(value.direction as string) || !Array.isArray(value.children) || !value.children.length || value.children.length > PANEL_LIMIT || !Array.isArray(value.sizes) || value.sizes.length !== value.children.length || !value.sizes.every(size => finite(size) && size > 0 && size <= 1)) throw new Error('Invalid split');
    return { type: 'split', direction: value.direction as 'row' | 'column', sizes: value.sizes as number[], children: value.children.map(child => parseNode(child, depth + 1, panelIDs, groupIDs)) };
  }
  return parseGroup(value, panelIDs, groupIDs);
}
function parseBox(value: unknown): Box {
  if (!object(value) || !keys(value, ['left', 'top', 'right', 'bottom', 'width', 'height']) || !finite(value.width) || !finite(value.height) || value.width < 40 || value.height < 40) throw new Error('Invalid floating position');
  for (const side of ['left', 'top', 'right', 'bottom']) if (value[side] !== undefined && !finite(value[side])) throw new Error('Invalid floating position');
  return value as Box;
}
function parsePage(page: unknown, pageIDs: Set<string>): WorkspacePage {
  if (!object(page) || !keys(page, ['id', 'name', 'selection', 'layout', 'floating', 'popouts', 'detached']) || !id(page.id) || pageIDs.has(page.id) || typeof page.name !== 'string' || !page.name.trim() || page.name.length > 64 || /[\u0000-\u001f]/.test(page.name) || !validTarget(page.selection)) throw new Error('Invalid page or panel configuration');
  pageIDs.add(page.id);
  const panelIDs = new Set<string>(), groupIDs = new Set<string>();
  const layout = page.layout === null ? null : parseNode(page.layout, 0, panelIDs, groupIDs);
  const list = (value: unknown) => value === undefined ? [] : Array.isArray(value) && value.length <= PANEL_LIMIT ? value : (() => { throw new Error('Invalid page or panel configuration'); })();
  const floating = list(page.floating).map(value => { if (!object(value) || !keys(value, ['group', 'box'])) throw new Error('Invalid floating group'); return { group: parseGroup(value.group, panelIDs, groupIDs), box: parseBox(value.box) }; });
  const popouts = list(page.popouts).map(value => parseGroup(value, panelIDs, groupIDs));
  const detached = list(page.detached).map(value => { const panel = parsePanel(value, panelIDs); if (!panel.detached) throw new Error('A panel kept for its own window must be detached'); return panel; });
  if (panelIDs.size > PANEL_LIMIT) throw new Error(`At most ${PANEL_LIMIT} panels per page`);
  return { id: page.id, name: page.name, selection: page.selection, layout: normalize(layout), floating, popouts, detached };
}

/** Reads saved pages in any version, migrating older ones (with the window's Agents choice); throws rather than guessing. */
export function parseLayout(text: string, agents?: SidebarAgents): WorkspaceLayout {
  if (text.length > SIZE_LIMIT) throw new Error('Page layout exceeds 192 KiB');
  const value: unknown = JSON.parse(text);
  if (object(value) && value.version === 1) return migrateV1(value, agents);
  if (!object(value) || !keys(value, ['version', 'selectedPage', 'pages']) || value.version !== 2 || !id(value.selectedPage) || !Array.isArray(value.pages) || value.pages.length < 1 || value.pages.length > PAGE_LIMIT) throw new Error('Unsupported page layout');
  const pageIDs = new Set<string>();
  const pages = value.pages.map(page => parsePage(page, pageIDs));
  if (!pageIDs.has(value.selectedPage)) throw new Error('Selected page does not exist');
  return { version: 2, selectedPage: value.selectedPage, pages };
}

// ---------------------------------------------------------------------------
// Version 1: a binary tree of single panels, each optionally collapsed or in
// its own window. Validated as strictly as it always was, then converted.

type V1Panel = { id: string; kind: string; binding: Binding; detached: boolean; collapsed?: boolean };
type V1Node = { type: 'panel'; panel: V1Panel } | { type: 'split'; direction: 'row' | 'column'; ratio: number; children: [V1Node, V1Node] };
function validV1(value: Record<string, unknown>) {
  const pageIDs = new Set<string>(), panelIDs = new Set<string>();
  const node = (value: unknown, depth: number): boolean => {
    if (!object(value) || depth > 12) return false;
    if (value.type === 'split') return keys(value, ['type', 'direction', 'ratio', 'children']) && ['row', 'column'].includes(value.direction as string) && finite(value.ratio) && value.ratio >= 0.15 && value.ratio <= 0.85 && Array.isArray(value.children) && value.children.length === 2 && value.children.every(child => node(child, depth + 1));
    if (value.type !== 'panel' || !keys(value, ['type', 'panel']) || !object(value.panel)) return false;
    const p = value.panel;
    if (!keys(p, ['id', 'kind', 'binding', 'detached', 'collapsed']) || !id(p.id) || panelIDs.has(p.id) || typeof p.kind !== 'string' || !kindPattern.test(p.kind) || typeof p.detached !== 'boolean' || (p.collapsed !== undefined && typeof p.collapsed !== 'boolean') || !object(p.binding)) return false;
    panelIDs.add(p.id);
    const bindingValid = p.binding.mode === 'follow' ? keys(p.binding, ['mode']) : p.binding.mode === 'pinned' && keys(p.binding, ['mode', 'target']) && validTarget(p.binding.target);
    return bindingValid && (!p.detached || (p.kind === 'chat' && p.binding.mode === 'pinned' && validTarget(p.binding.target) && !!p.binding.target.sessionId));
  };
  const v1Panels = (n: V1Node): V1Panel[] => n.type === 'panel' ? [n.panel] : n.children.flatMap(v1Panels);
  if (!keys(value, ['version', 'selectedPage', 'pages']) || !id(value.selectedPage) || !Array.isArray(value.pages) || value.pages.length < 1 || value.pages.length > PAGE_LIMIT) throw new Error('Unsupported page layout');
  for (const page of value.pages) {
    if (!object(page) || !keys(page, ['id', 'name', 'selection', 'layout']) || !id(page.id) || pageIDs.has(page.id) || typeof page.name !== 'string' || !page.name.trim() || page.name.length > 64 || /[\u0000-\u001f]/.test(page.name) || !validTarget(page.selection) || !node(page.layout, 0)) throw new Error('Invalid page or panel configuration');
    pageIDs.add(page.id);
    if (v1Panels(page.layout as V1Node).length > 12) throw new Error('At most 12 panels per page');
  }
  if (!pageIDs.has(value.selectedPage as string)) throw new Error('Selected page does not exist');
}
/** Converts a version 1 layout: every panel becomes a group of its own, in the same place and size. Agents joins the sidebar unless the window closed it. */
export function migrateV1(value: Record<string, unknown>, agents?: SidebarAgents): WorkspaceLayout {
  validV1(value);
  const old = value as { selectedPage: string; pages: { id: string; name: string; selection: ViewTarget; layout: V1Node }[] };
  return { version: 2, selectedPage: old.selectedPage, pages: old.pages.map(page => {
    const all: V1Panel[] = [];
    const collect = (n: V1Node) => { if (n.type === 'panel') all.push(n.panel); else n.children.forEach(collect); };
    collect(page.layout);
    // As before: chats in their own window leave no gap, unless every content panel is away (then the first stays as a card).
    const isContent = (panel: V1Panel) => panel.kind !== 'collections' && panel.kind !== 'console';
    const card = all.some(panel => !panel.detached && isContent(panel)) ? undefined : all.find(panel => panel.detached && isContent(panel));
    const away = all.filter(panel => panel.detached && panel !== card);
    const convert = (n: V1Node, parent?: 'row' | 'column'): LayoutNode | null => {
      if (n.type === 'panel') {
        if (away.includes(n.panel)) return null;
        const { collapsed, ...panel } = n.panel;
        return leaf(panel, !!collapsed && parent === 'column');
      }
      return { type: 'split', direction: n.direction, sizes: [n.ratio, 1 - n.ratio], children: [convert(n.children[0], n.direction), convert(n.children[1], n.direction)].filter(Boolean) as LayoutNode[] };
    };
    // Normalizing drops the gaps and merges splits that go the same way.
    let layout = normalize(convert(page.layout));
    // The workspace list gains the agents list under it, as new pages have it.
    const list = groups(layout).find(group => group.panels.length === 1 && group.panels[0].kind === 'collections');
    const agentsID = list ? `${list.panels[0].id}-agents`.slice(0, 128) : '';
    if (list && agents !== 'closed' && !all.some(panel => panel.id === agentsID || panel.kind === 'agents')) layout = stackSidebar(mapGroups(layout, group => group === list ? { ...group, panels: [...group.panels, followPanel(agentsID, 'agents')] } : group));
    return fitSidebarAgents({ ...emptyPage(page.id, page.name, page.selection, layout), detached: away.map(({ collapsed: _, ...panel }) => panel) }, agents);
  }) };
}
