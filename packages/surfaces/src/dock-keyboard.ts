/**
 * The dock from the keyboard, the pure part: where each group is on screen,
 * which group is the neighbour in a direction, how far one press of a resize
 * key moves an edge, and what moving or swapping a panel does to the saved
 * arrangement. No DOM: `scripts/dock-keyboard.test.ts` pins it, DockLayout and
 * Workspace run it (see the `layout.focus-*`, `layout.move-*`,
 * `layout.resize-*` and `layout.tab.*` actions).
 *
 * Everything works on the saved arrangement (workspace-model), which is the
 * dock's source of truth, so a neighbour is found the same way whether or not
 * a group is maximized (the others are hidden then and have no size on screen).
 */
import type { LayoutDirection } from './actions';
import { childPixels, needsEmptyContent } from './dock-bridge';
import { addToGroup, groupOf, groups, isSidebar, normalize, removeFromPage, splitBeside, type DockSide, type Group, type LayoutNode, type Panel, type Split, type WorkspacePage } from './workspace-model';

export type Rect = { x: number; y: number; width: number; height: number };
export type DockGeometry = { width: number; height: number; sidebarWidth: number; header: number };

/** Each docked group's rectangle in a dock of `geometry`'s size, laid out as dockview lays it out (toDockview). */
export function groupRects(layout: LayoutNode | null, geometry: DockGeometry): Map<string, Rect> {
  const rects = new Map<string, Rect>();
  if (!layout) return rects;
  const whole: Rect = { x: 0, y: 0, width: geometry.width, height: geometry.height };
  if (layout.type === 'group') {
    // Alone, the sidebar keeps its width beside the empty content area.
    rects.set(layout.id, needsEmptyContent({ layout }) ? { ...whole, width: Math.min(geometry.sidebarWidth, geometry.width) } : whole);
    return rects;
  }
  const visit = (node: LayoutNode, rect: Rect, root: boolean) => {
    if (node.type === 'group') { rects.set(node.id, rect); return; }
    const row = node.direction === 'row';
    const pixels = childPixels(node, row ? rect.width : rect.height, root, geometry);
    let offset = 0;
    node.children.forEach((child, index) => {
      visit(child, row ? { x: rect.x + offset, y: rect.y, width: pixels[index], height: rect.height } : { x: rect.x, y: rect.y + offset, width: rect.width, height: pixels[index] }, false);
      offset += pixels[index];
    });
  };
  visit(layout, whole, true);
  return rects;
}

const EDGE = 2;
const overlap = (a0: number, a1: number, b0: number, b1: number) => Math.min(a1, b1) - Math.max(a0, b0);
/**
 * The group beside `from` in `direction`: the nearest one across that edge
 * that shares some of its span. Among several, the one in line with the middle
 * of `from` wins, then the one sharing most of it. `prefer` (the group the
 * last move came from) wins any tie, so ← then → comes back to where it began.
 * Undefined at the edge of the page: there is no wrapping around.
 */
export function neighbourIn(rects: ReadonlyMap<string, Rect>, from: string, direction: LayoutDirection, prefer?: string): string | undefined {
  const a = rects.get(from);
  if (!a) return undefined;
  const horizontal = direction === 'left' || direction === 'right';
  type Scored = { id: string; gap: number; centred: boolean; shared: number };
  const scored: Scored[] = [];
  for (const [id, b] of rects) {
    if (id === from) continue;
    const gap = direction === 'right' ? b.x - (a.x + a.width) : direction === 'left' ? a.x - (b.x + b.width) : direction === 'down' ? b.y - (a.y + a.height) : a.y - (b.y + b.height);
    if (gap < -EDGE) continue;
    const shared = horizontal ? overlap(a.y, a.y + a.height, b.y, b.y + b.height) : overlap(a.x, a.x + a.width, b.x, b.x + b.width);
    if (shared <= EDGE) continue;
    const middle = horizontal ? a.y + a.height / 2 : a.x + a.width / 2;
    const centred = horizontal ? b.y <= middle && middle <= b.y + b.height : b.x <= middle && middle <= b.x + b.width;
    scored.push({ id, gap: Math.max(0, gap), centred, shared });
  }
  if (!scored.length) return undefined;
  const nearest = Math.min(...scored.map(value => value.gap));
  const adjacent = scored.filter(value => value.gap <= nearest + EDGE);
  if (prefer && adjacent.some(value => value.id === prefer)) return prefer;
  adjacent.sort((x, y) => Number(y.centred) - Number(x.centred) || y.shared - x.shared);
  return adjacent[0].id;
}

/** A tap moves an edge by 5 % of what it divides; held down, each repeat moves it 1 %, speeding up to 3 %. */
export const RESIZE_TAP = 0.05;
export function resizeStep(repeat: boolean, streak = 0): number {
  if (!repeat) return RESIZE_TAP;
  return 0.01 * (1 + Math.min(2, Math.max(0, streak) / 10));
}
/** No group is resized below this share of its split (dockview's pixel minimums apply on top). */
const MIN_SHARE = 0.05;

export type Resized =
  /** The arrangement with the edge moved. */
  | { kind: 'layout'; layout: LayoutNode; /** The share the active group's side now has. */ share: number; axis: 'width' | 'height' }
  /** The edge is the Workspaces sidebar's: its one shared width changes instead, by `sign` × the step in pixels. */
  | { kind: 'sidebar'; sign: 1 | -1 }
  /** Nothing to move: the group fills the page that way, or that edge is at its limit. */
  | { kind: 'none' };

type Step = { split: Split; index: number; path: number[] };
/** The splits between the root and a group, outermost first, with the child index taken in each. */
function pathTo(root: LayoutNode, groupID: string): Step[] | undefined {
  const walk = (node: LayoutNode, path: number[], trail: Step[]): Step[] | undefined => {
    if (node.type === 'group') return node.id === groupID ? trail : undefined;
    for (let index = 0; index < node.children.length; index++) {
      const found = walk(node.children[index], [...path, index], [...trail, { split: node, index, path }]);
      if (found) return found;
    }
    return undefined;
  };
  return walk(root, [], []);
}
function replaceAt(node: LayoutNode, path: number[], change: (split: Split) => Split): LayoutNode {
  if (node.type !== 'split') return node;
  if (!path.length) return change(node);
  const [head, ...rest] = path;
  return { ...node, children: node.children.map((child, index) => index === head ? replaceAt(child, rest, change) : child) };
}
/** A child whose size a divider cannot change: the sidebar (pixels) or a minimized group (its tab bar). */
const pinned = (split: Split, index: number, root: boolean) => {
  const child = split.children[index];
  return (split.direction === 'column' && child.type === 'group' && !!child.minimized) || (root && split.direction === 'row' && (index === 0 || index === split.children.length - 1) && isSidebar(child));
};

/**
 * One press of a resize key, as in tmux: the active group's edge on the arrow's
 * side moves that way (it grows); with no edge there, its opposite edge moves
 * that way (it shrinks). The nearest such edge in the tree is the one moved,
 * and only the two sides of it change. The sidebar's edge changes the sidebar's
 * pixel width instead (`sidebar`), so it keeps one width on every page.
 */
export function resizeEdge(layout: LayoutNode | null, groupID: string, direction: LayoutDirection, step: number): Resized {
  if (!layout || layout.type === 'group') return { kind: 'none' };
  const trail = pathTo(layout, groupID);
  if (!trail) return { kind: 'none' };
  const axis = direction === 'left' || direction === 'right' ? 'row' : 'column';
  const forward = direction === 'right' || direction === 'down';
  const candidates = [...trail].reverse().filter(value => value.split.direction === axis);
  const edgeOf = (wantForward: boolean) => {
    for (const value of candidates) {
      const other = wantForward ? value.index + 1 : value.index - 1;
      if (other >= 0 && other < value.split.children.length) return { ...value, other };
    }
    return undefined;
  };
  const found = edgeOf(forward) || edgeOf(!forward);
  if (!found) return { kind: 'none' };
  const { split, index, other, path } = found;
  const root = path.length === 0;
  const [first, second] = index < other ? [index, other] : [other, index];
  // The divider moves forward (right or down) when the arrow does.
  const sign = forward ? 1 : -1;
  const sidebar = (at: number) => root && split.direction === 'row' && (at === 0 || at === split.children.length - 1) && isSidebar(split.children[at]);
  if (sidebar(first)) return { kind: 'sidebar', sign: sign as 1 | -1 };
  if (sidebar(second)) return { kind: 'sidebar', sign: (-sign) as 1 | -1 };
  if (pinned(split, first, root) || pinned(split, second, root)) return { kind: 'none' };
  const flexible = split.sizes.reduce((sum, size, at) => pinned(split, at, root) ? sum : sum + size, 0) || 1;
  const floor = MIN_SHARE * flexible, pair = split.sizes[first] + split.sizes[second];
  const moved = Math.max(floor, Math.min(pair - floor, split.sizes[first] + sign * step * flexible));
  if (Math.abs(moved - split.sizes[first]) < 1e-6) return { kind: 'none' };
  const sizes = split.sizes.map((size, at) => at === first ? moved : at === second ? pair - moved : size);
  const share = (index === first ? moved : pair - moved) / flexible;
  return { kind: 'layout', layout: replaceAt(layout, path, value => ({ ...value, sizes })), share, axis: axis === 'row' ? 'width' : 'height' };
}

/** The share of its split's flexible space each ancestor in `direction`'s axis gives a group, innermost first (what a resize announces). */
export function shareOf(layout: LayoutNode | null, groupID: string, axis: 'width' | 'height'): number | undefined {
  if (!layout || layout.type === 'group') return undefined;
  const trail = pathTo(layout, groupID);
  const step = trail && [...trail].reverse().find(value => value.split.direction === (axis === 'width' ? 'row' : 'column'));
  if (!step) return undefined;
  const root = step.path.length === 0;
  const flexible = step.split.sizes.reduce((sum, size, at) => pinned(step.split, at, root) ? sum : sum + size, 0) || 1;
  return step.split.sizes[step.index] / flexible;
}

const sideOf: Record<LayoutDirection, DockSide> = { left: 'left', right: 'right', up: 'top', down: 'bottom' };
export type Moved = { page: WorkspacePage; how: 'joined' | 'swapped' | 'split' };
/**
 * The active panel moved one step in a direction, the keyboard's drag:
 * - into the neighbouring group, as its front tab, when it shares its own
 *   group with other tabs;
 * - swapped with the neighbouring group, when it is alone in its group (the
 *   arrangement keeps its shape; the two trade places);
 * - with no neighbour that way, split out of its group to that side (when it
 *   has tabs beside it).
 * The sidebar never takes part (its width is the page's, its panels are the
 * workspace list): undefined then, and when nothing can move.
 */
export function movePanel(page: WorkspacePage, panelID: string, direction: LayoutDirection, neighbour: string | undefined): Moved | undefined {
  const source = groupOf(page, panelID);
  const panel = source?.panels.find(value => value.id === panelID);
  // Only a docked group has neighbours; a floating one is moved with the mouse or its menu.
  if (!source || !panel || isSidebar(source) || !groups(page.layout).some(group => group.id === source.id)) return undefined;
  const target = neighbour ? groups(page.layout).find(group => group.id === neighbour) : undefined;
  if (target && isSidebar(target)) return source.panels.length > 1 ? split(page, source, panel, direction) : undefined;
  if (target && source.panels.length > 1) return { page: addToGroup(removeFromPage(page, panelID), target.id, panel), how: 'joined' };
  if (target) return { page: { ...page, layout: swapGroups(page.layout, source.id, target.id) }, how: 'swapped' };
  return source.panels.length > 1 ? split(page, source, panel, direction) : undefined;
}
function split(page: WorkspacePage, source: Group, panel: Panel, direction: LayoutDirection): Moved {
  const without = removeFromPage(page, panel.id);
  return { page: { ...without, layout: splitBeside(without.layout, source.id, panel, sideOf[direction]) }, how: 'split' };
}
/** Two groups trade places; each keeps its tabs. A minimized one is opened (it would otherwise move as a sliver). */
export function swapGroups(layout: LayoutNode | null, a: string, b: string): LayoutNode | null {
  const all = groups(layout), first = all.find(group => group.id === a), second = all.find(group => group.id === b);
  if (!first || !second) return layout;
  const open = ({ minimized: _, ...group }: Group): Group => group;
  const visit = (node: LayoutNode): LayoutNode => {
    if (node.type === 'group') return node.id === a ? open(second) : node.id === b ? open(first) : node;
    return { ...node, children: node.children.map(visit) };
  };
  return layout && normalize(visit(layout));
}

/** The tab a number key selects: 1–8 by place, 9 the last one (as in a browser). Undefined past the last. */
export function tabAt(count: number, slot: number): number | undefined {
  if (count <= 0) return undefined;
  if (slot >= 9) return count - 1;
  return slot >= 1 && slot <= count ? slot - 1 : undefined;
}
