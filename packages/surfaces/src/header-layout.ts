/**
 * The chat header's second line: what fits beside the status, by priority,
 * and what folds into its "⋯" details. Pure, so the order and the folding are
 * tested without a browser; ChatHeader measures and renders.
 *
 * Nothing on the line is ever cut: an item is shown whole or folded whole.
 * The numbers' widths are reserved (a count that ticks from 99 to 100 never
 * refolds the line), so what folds depends on the panel's width and on texts
 * that rarely change (the branch, the model, the subscription).
 */
export type HeaderItemId = 'status' | 'branch' | 'model' | 'account' | 'usage' | 'background' | 'artifacts' | 'tools' | 'prompts';

/** Most important first. The status never folds. */
export const HEADER_PRIORITY: readonly HeaderItemId[] = ['status', 'branch', 'model', 'usage', 'background', 'artifacts', 'account', 'tools', 'prompts'];

/** The order on the line: who and how on the left, the numbers on the right. */
export const HEADER_ORDER: readonly HeaderItemId[] = ['status', 'branch', 'model', 'account', 'prompts', 'artifacts', 'background', 'usage', 'tools'];

/** Whether an item is one of the numbers (a chip on the right) or about the conversation (left). */
export const isStat = (id: HeaderItemId) => id === 'usage' || id === 'background' || id === 'tools' || id === 'prompts' || id === 'artifacts';

/**
 * Reserved widths of the number chips at text scale 1, in CSS pixels: an icon,
 * three or four digits and the chip's padding (the usage chip: its bar, the
 * percentage and the cost; tools: the count and "N failed"; artifacts: "12 artifacts",
 * the one chip that keeps its noun, since a bare count would not say what it is).
 */
export const STAT_WIDTH: Record<'usage' | 'background' | 'tools' | 'prompts' | 'artifacts', number> = { prompts: 50, background: 50, usage: 138, tools: 112, artifacts: 96 };

/** The gap between items, and the "⋯" button's width, at text scale 1. */
export const HEADER_GAP = 16, MORE_WIDTH = 30;

/** `urgent`: live news (background work running) that comes right after the status. */
export type HeaderItem = { id: HeaderItemId; width: number; urgent?: boolean };
export type HeaderPlan = { inline: HeaderItemId[]; folded: HeaderItemId[] };

/**
 * Which items show on a line `available` pixels wide. Everything fits, or the
 * "⋯" button takes its room and items are kept by priority while they fit; a
 * long item that folds does not take the smaller ones after it along. The
 * status always shows, and urgent items come right after it. Both lists come
 * back in line order.
 */
export function fitHeader(items: readonly HeaderItem[], available: number, gap = HEADER_GAP, more = MORE_WIDTH): HeaderPlan {
  const byOrder = (a: HeaderItemId, b: HeaderItemId) => HEADER_ORDER.indexOf(a) - HEADER_ORDER.indexOf(b);
  const total = items.reduce((sum, item) => sum + item.width + gap, 0);
  if (total <= available) return { inline: items.map(item => item.id).sort(byOrder), folded: [] };
  const rank = (item: HeaderItem) => item.id === 'status' ? -2 : item.urgent ? -1 : HEADER_PRIORITY.indexOf(item.id);
  const ranked = [...items].sort((a, b) => rank(a) - rank(b));
  let used = more;
  const inline: HeaderItemId[] = [], folded: HeaderItemId[] = [];
  for (const item of ranked) {
    const need = item.width + gap;
    if (item.id === 'status' || used + need <= available) { inline.push(item.id); used += need; }
    else folded.push(item.id);
  }
  return { inline: inline.sort(byOrder), folded: folded.sort(byOrder) };
}

/**
 * Rounds a measured text width up to a step, so a status whose text changes
 * by a character or two keeps its slot (and the line its plan).
 */
export function reserve(width: number, step = 24): number {
  return Math.ceil(Math.max(0, width) / step) * step;
}
