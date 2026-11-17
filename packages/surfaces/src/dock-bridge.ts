/**
 * Between a page's saved arrangement (workspace-model) and dockview's
 * serialized form. Pure: no DOM, so the round trip is unit-tested.
 *
 * Saved sizes are shares; dockview's are pixels. Two kinds of group are sized
 * in pixels whatever their share: the Workspaces sidebar at the edge of the
 * page's root row takes the one sidebar width every workspace shares, and a
 * minimized group in a column takes its tab bar's height. Their saved share
 * is kept so a minimized group restores to the size it had.
 */
import type { GroupviewPanelState, SerializedDockview, SerializedGridObject } from 'dockview';

type GroupPanelViewState = NonNullable<NonNullable<SerializedDockview['floatingGroups']>[number]['data']>;
import { groups, isSidebar, normalize, type Binding, type Box, type Group, type LayoutNode, type Panel, type WorkspacePage } from './workspace-model';

export const PANEL_COMPONENT = 'panel';
/**
 * The content area of a page whose docked panels are only the sidebar (every
 * other panel closed, floating or in a window): an empty group beside the
 * sidebar, so the sidebar keeps its width instead of filling the page. It is
 * dockview's only (no params): reading the layout back drops it, so it is never saved.
 */
export const EMPTY_CONTENT = 'lazybox-empty-content';
/** Whether a page's docked arrangement is the sidebar alone, which then needs `EMPTY_CONTENT` beside it. */
export const needsEmptyContent = (page: Pick<WorkspacePage, 'layout'>) => !!page.layout && isSidebar(page.layout);
export const TAB_COMPONENT = 'tab';
/** What dockview keeps for each panel: enough to rebuild the saved panel. */
export type PanelParams = { kind: string; binding: Binding; detached?: boolean; /** A chat group's preview tab. */ preview?: boolean };
export type Geometry = {
  width: number;
  height: number;
  /** The Workspaces sidebar's width, shared by every workspace. */
  sidebarWidth: number;
  /** A tab bar's height: a minimized group's size. */
  header: number;
  title(panel: Panel): string;
};
type Arranged = Pick<WorkspacePage, 'layout' | 'floating' | 'popouts'>;

const MIN_PX = 48;
const orientationOf = (direction: 'row' | 'column') => direction === 'row' ? 'HORIZONTAL' : 'VERTICAL';
// A chat always says whether it is the preview tab: dockview merges parameters, so a mark left out would stay.
export const paramsOf = (panel: Panel): PanelParams => ({ kind: panel.kind, binding: panel.binding, ...(panel.detached ? { detached: true } : {}), ...(panel.kind === 'chat' ? { preview: !!panel.preview } : {}) });

/** Pixel sizes for a split's children along its direction. */
export function childPixels(node: Extract<LayoutNode, { type: 'split' }>, extent: number, root: boolean, geometry: Pick<Geometry, 'sidebarWidth' | 'header'>): number[] {
  const fixed = node.children.map((child, index) => {
    if (root && node.direction === 'row' && (index === 0 || index === node.children.length - 1) && isSidebar(child)) return geometry.sidebarWidth;
    if (node.direction === 'column' && child.type === 'group' && child.minimized) return geometry.header;
    return undefined;
  });
  const flexible = node.sizes.reduce((sum, size, index) => fixed[index] === undefined ? sum + size : sum, 0) || 1;
  const remaining = Math.max(0, extent - fixed.reduce<number>((sum, value) => sum + (value ?? 0), 0));
  const pixels = node.sizes.map((size, index) => fixed[index] ?? Math.max(MIN_PX, Math.floor(remaining * size / flexible)));
  // Rounding leftovers go to the last flexible child so the children fill the split.
  const last = fixed.lastIndexOf(undefined);
  if (last >= 0) pixels[last] = Math.max(MIN_PX, pixels[last] + extent - pixels.reduce((sum, value) => sum + value, 0));
  return pixels;
}

function groupState(group: Group): GroupPanelViewState {
  return { id: group.id, views: group.panels.map(panel => panel.id), activeView: group.active };
}
function panelState(panel: Panel, geometry: Pick<Geometry, 'title'>): GroupviewPanelState {
  return { id: panel.id, contentComponent: PANEL_COMPONENT, tabComponent: TAB_COMPONENT, title: geometry.title(panel), params: paramsOf(panel) };
}

/** A page as dockview's serialized layout, sized for a container of `geometry.width` × `geometry.height`. */
export function toDockview(page: Arranged, geometry: Geometry): SerializedDockview {
  const states: Record<string, GroupviewPanelState> = {};
  for (const group of [...groups(page.layout), ...page.floating.map(value => value.group), ...page.popouts]) for (const panel of group.panels) states[panel.id] = panelState(panel, geometry);
  const visit = (node: LayoutNode, along: number, across: number, size: number, root: boolean): SerializedGridObject<GroupPanelViewState> => {
    if (node.type === 'group') return { type: 'leaf', data: groupState(node), size };
    // `along` is this split's extent in its own direction; children take shares of it.
    const pixels = childPixels(node, along, root, geometry);
    return { type: 'branch', size, data: node.children.map((child, index) => visit(child, across, pixels[index], pixels[index], false)) };
  };
  const width = Math.max(1, Math.round(geometry.width)), height = Math.max(1, Math.round(geometry.height));
  let grid: SerializedDockview['grid'];
  if (!page.layout) grid = { root: { type: 'branch', data: [], size: height }, width, height, orientation: 'HORIZONTAL' as never };
  else if (needsEmptyContent(page)) {
    // The sidebar at its width, and the empty content area taking the rest.
    const sidebar = Math.min(geometry.sidebarWidth, Math.max(MIN_PX, width - MIN_PX));
    states[EMPTY_CONTENT] = { id: EMPTY_CONTENT, contentComponent: PANEL_COMPONENT, tabComponent: TAB_COMPONENT, title: '' };
    // The sidebar is a group, or Workspaces stacked over Agents.
    const side = page.layout.type === 'group' ? { type: 'leaf' as const, data: groupState(page.layout), size: sidebar } : visit(page.layout, height, sidebar, sidebar, false);
    grid = { root: { type: 'branch', size: height, data: [side, { type: 'leaf', data: { id: `g-${EMPTY_CONTENT}`, views: [EMPTY_CONTENT], activeView: EMPTY_CONTENT }, size: width - sidebar }] }, width, height, orientation: 'HORIZONTAL' as never };
  }
  else if (page.layout.type === 'group') grid = { root: { type: 'branch', size: height, data: [{ type: 'leaf', data: groupState(page.layout), size: width }] }, width, height, orientation: 'HORIZONTAL' as never };
  else {
    const row = page.layout.direction === 'row';
    grid = { root: visit(page.layout, row ? width : height, row ? height : width, row ? height : width, true), width, height, orientation: orientationOf(page.layout.direction) as never };
  }
  return {
    grid,
    panels: states,
    ...(page.floating.length ? { floatingGroups: page.floating.map(value => ({ data: groupState(value.group), position: value.box as never })) } : {}),
    ...(page.popouts.length ? { popoutGroups: page.popouts.map(group => ({ data: groupState(group), position: null })) } : {}),
  };
}

/** How the bridge reads what dockview does not know: which groups are minimized, and their share to restore. */
export type Overlay = { minimized: ReadonlyMap<string, number> };

/** Reads dockview's serialized layout back into the saved form. Unknown panels (no params) are dropped. */
export function fromDockview(json: SerializedDockview, overlay: Overlay = { minimized: new Map() }): Arranged {
  const panel = (viewID: string): Panel | undefined => {
    const params = json.panels[viewID]?.params as PanelParams | undefined;
    if (!params || typeof params.kind !== 'string' || !params.binding) return undefined;
    return { id: viewID, kind: params.kind, binding: params.binding, detached: !!params.detached, ...(params.preview ? { preview: true } : {}) };
  };
  const group = (data: GroupPanelViewState): Group | null => {
    const members = data.views.map(panel).filter((value): value is Panel => !!value);
    if (!members.length) return null;
    return { type: 'group', id: data.id, panels: members, active: members.some(value => value.id === data.activeView) ? data.activeView! : members[0].id };
  };
  const visit = (node: SerializedGridObject<GroupPanelViewState>, orientation: 'HORIZONTAL' | 'VERTICAL'): LayoutNode | null => {
    if (node.type === 'leaf') return group(node.data as GroupPanelViewState);
    const direction = orientation === 'HORIZONTAL' ? 'row' : 'column';
    const items = (node.data as SerializedGridObject<GroupPanelViewState>[]).map(child => ({ child, node: visit(child, orientation === 'HORIZONTAL' ? 'VERTICAL' : 'HORIZONTAL') }));
    const kept = items.filter(item => item.node);
    // A minimized group keeps the share it restores to; the others share the rest by their pixels.
    const minimized = kept.map(item => direction === 'column' && item.node!.type === 'group' && overlay.minimized.has(item.node!.id));
    const reserved = kept.reduce((sum, item, index) => minimized[index] ? sum + overlay.minimized.get((item.node as Group).id)! : sum, 0);
    const pixels = kept.reduce((sum, item, index) => minimized[index] ? sum : sum + Math.max(1, item.child.size ?? 1), 0) || 1;
    const sizes = kept.map((item, index) => minimized[index] ? overlay.minimized.get((item.node as Group).id)! : Math.max(1, item.child.size ?? 1) / pixels * Math.max(0.05, 1 - reserved));
    const children = kept.map((item, index) => minimized[index] ? { ...(item.node as Group), minimized: true } : item.node!);
    return { type: 'split', direction, sizes, children };
  };
  const nested = (value: { data?: GroupPanelViewState; grid?: { root: SerializedGridObject<GroupPanelViewState> } }): Group | null => {
    if (value.data) return group(value.data);
    // A floating or popped-out window holding a grid of its own comes back as one group of tabs.
    const inner = value.grid ? groups(visit(value.grid.root, 'HORIZONTAL')) : [];
    if (!inner.length) return null;
    return { ...inner[0], panels: inner.flatMap(item => item.panels) };
  };
  const layout = normalize(visit(json.grid.root, json.grid.orientation as 'HORIZONTAL' | 'VERTICAL'));
  const floating = (json.floatingGroups || []).flatMap(value => { const g = nested(value); return g ? [{ group: g, box: boxOf(value.position as Record<string, unknown>) }] : []; });
  const popouts = (json.popoutGroups || []).flatMap(value => { const g = nested(value); return g ? [g] : []; });
  return { layout, floating, popouts };
}
function boxOf(value: Record<string, unknown>): Box {
  const box: Box = { width: Math.max(40, Math.round(Number(value.width) || 300)), height: Math.max(40, Math.round(Number(value.height) || 200)) };
  for (const side of ['left', 'top', 'right', 'bottom'] as const) if (typeof value[side] === 'number' && Number.isFinite(value[side])) box[side] = Math.round(value[side] as number);
  if (box.left === undefined && box.right === undefined) box.left = 0;
  if (box.top === undefined && box.bottom === undefined) box.top = 0;
  return box;
}

/**
 * The arrangement without sizes: which groups exist, where, and what they
 * hold. Two pages with the same key differ only in sizes, front tabs or panel
 * targets, which dockview can change in place.
 */
export function structureKey(page: Arranged): string {
  const node = (value: LayoutNode | null): unknown => !value ? null : value.type === 'group' ? [value.id, value.panels.map(panel => panel.id)] : [value.direction, value.children.map(node)];
  const group = (value: Group) => [value.id, value.panels.map(panel => panel.id)];
  return JSON.stringify([node(page.layout), page.floating.map(value => group(value.group)), page.popouts.map(group)]);
}
/** Same sizes, within `tolerance` of a share; the sidebar's share is not compared (its width is fixed in pixels). */
export function sameSizes(a: LayoutNode | null, b: LayoutNode | null, tolerance = 0.01, root = true): boolean {
  if (!a || !b) return a === b;
  if (a.type === 'group' || b.type === 'group') return a.type === b.type;
  if (a.children.length !== b.children.length) return false;
  const skip = (index: number) => root && a.direction === 'row' && (index === 0 || index === a.children.length - 1) && isSidebar(a.children[index]);
  // The others are compared as shares of what the sidebar leaves.
  const rest = (sizes: number[]) => { const total = sizes.reduce((sum, size, index) => skip(index) ? sum : sum + size, 0) || 1; return sizes.map(size => size / total); };
  const left = rest(a.sizes), right = rest(b.sizes);
  return left.every((size, index) => skip(index) || Math.abs(size - right[index]) <= tolerance) && a.children.every((child, index) => sameSizes(child, b.children[index], tolerance, false));
}
/** Front tab and panel targets of every group, for in-place updates. */
export function groupStates(page: Arranged): Map<string, Group> {
  return new Map([...groups(page.layout), ...page.floating.map(value => value.group), ...page.popouts].map(group => [group.id, group]));
}
