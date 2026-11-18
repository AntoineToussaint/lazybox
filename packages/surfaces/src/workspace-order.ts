/**
 * The person's order of their workspaces in the sidebar: where a dragged row
 * lands, what ⌥⇧↑ / ⌥⇧↓ do, and how an order is shown before the Runner
 * answers. The Runner keeps the order (ArrangeWorkspaces) and lists in it
 * (ListCollections), so every window and a paired phone agree. Pure.
 */

/** The order with `id` moved to insertion slot `slot` (0…n, counted in the list as it is, the dragged row included). */
export function dropAt(ids: readonly string[], id: string, slot: number): string[] {
  const from = ids.indexOf(id);
  if (from < 0) return [...ids];
  const rest = ids.filter(value => value !== id);
  const to = Math.max(0, Math.min(rest.length, slot > from ? slot - 1 : slot));
  return [...rest.slice(0, to), id, ...rest.slice(to)];
}

/** Whether dropping `id` at `slot` leaves the order as it is: just above or just below itself. */
export function sameSlot(ids: readonly string[], id: string, slot: number): boolean {
  const from = ids.indexOf(id);
  return from < 0 || slot === from || slot === from + 1;
}

/** The order with `id` moved `delta` places (⌥⇧↑ is -1, ⌥⇧↓ is +1), held at either end. */
export function moveBy(ids: readonly string[], id: string, delta: number): string[] {
  const from = ids.indexOf(id);
  if (from < 0) return [...ids];
  const to = Math.max(0, Math.min(ids.length - 1, from + delta));
  return to === from ? [...ids] : dropAt(ids, id, to > from ? to + 1 : to);
}

/**
 * The insertion slot (0…n) for a pointer at `y` among rows, top to bottom:
 * before the first row whose middle is below the pointer.
 */
export function dropSlot(rows: readonly { top: number; bottom: number }[], y: number): number {
  const index = rows.findIndex(row => y < (row.top + row.bottom) / 2);
  return index < 0 ? rows.length : index;
}

/**
 * Items in a kept order, as the Runner lists them (ListCollections): those
 * the order does not name (created since) first, in their own order; then
 * the named ones in it. Shows a move at once, before the Runner answers.
 */
export function inOrder<T extends { id: string }>(items: readonly T[], order: readonly string[] | undefined): T[] {
  if (!order?.length) return [...items];
  const at = new Map(order.map((id, index) => [id, index]));
  const fresh = items.filter(item => !at.has(item.id));
  const placed = items.filter(item => at.has(item.id)).sort((a, b) => at.get(a.id)! - at.get(b.id)!);
  return [...fresh, ...placed];
}

/** Whether two orders are the same. */
export const sameOrder = (a: readonly string[], b: readonly string[]) => a.length === b.length && a.every((id, i) => id === b[i]);

/**
 * How fast a list scrolls while a row is dragged near one of its edges, in
 * pixels a frame: negative up, positive down, 0 away from the edges. Faster
 * the closer the pointer is.
 */
export function edgeScroll(y: number, top: number, bottom: number, zone = 40, max = 16): number {
  if (bottom - top < zone * 2) zone = (bottom - top) / 4;
  if (y < top + zone) return -Math.ceil(max * Math.min(1, (top + zone - y) / zone));
  if (y > bottom - zone) return Math.ceil(max * Math.min(1, (y - (bottom - zone)) / zone));
  return 0;
}

/** What a screen reader hears after a move: "Alpha feature moved to 2 of 4". */
export function movedWords(name: string, ids: readonly string[], id: string): string {
  return `${name} moved to ${ids.indexOf(id) + 1} of ${ids.length}`;
}
