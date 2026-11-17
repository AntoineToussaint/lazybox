/**
 * The pure part of Lazybox's context menus: what an item is, how a list of
 * contributed items becomes one tidy menu, where a menu goes on screen, and
 * how the keyboard moves through it. No DOM here; `ContextMenu.tsx` renders it
 * and `scripts/context-menu.test.ts` pins it.
 */

/** One entry a feature contributes. Falsy values are skipped, so lists can use `cond && {…}`. */
export type MenuAction = {
  label: string;
  /** An `Icon` name. */
  icon?: string;
  /** A hint shown on the right, in `shortcut()` notation: 'Mod+C', 'Alt+W'. Display only. */
  shortcut?: string;
  /** `true`, or why not: the reason is the item's tooltip and accessible description. */
  disabled?: boolean | string;
  danger?: boolean;
  /** A choice that is on (a current dock side, the current permission). */
  checked?: boolean;
  run?(): void | Promise<void>;
  submenu?: MenuItem[];
  /** A word after the label when there is no shortcut ("Current"). */
  note?: string;
  /** The button's class where the same items also render as a ⋯ menu. */
  className?: string;
};
export type MenuItem = MenuAction | 'separator' | null | undefined | false | '' | 0;
export type MenuEntry = { kind: 'action'; action: MenuAction; submenu?: MenuEntry[] } | { kind: 'separator' };

/** Drop empty values and empty submenus; no separator at either end or twice in a row. */
export function normalizeMenu(items: readonly MenuItem[]): MenuEntry[] {
  const out: MenuEntry[] = [];
  for (const item of items) {
    if (!item) continue;
    if (item === 'separator') { if (out.length && out[out.length - 1].kind !== 'separator') out.push({ kind: 'separator' }); continue; }
    if (item.submenu) {
      const submenu = normalizeMenu(item.submenu);
      if (!submenu.length) continue;
      out.push({ kind: 'action', action: item, submenu });
    } else out.push({ kind: 'action', action: item });
  }
  while (out.length && out[out.length - 1].kind === 'separator') out.pop();
  return out;
}

/** Sections become one list with a separator between non-empty ones. */
export function joinSections(sections: readonly (readonly MenuItem[] | undefined)[]): MenuItem[] {
  const out: MenuItem[] = [];
  for (const section of sections) {
    if (!section?.some(Boolean)) continue;
    if (out.length) out.push('separator');
    out.push(...section);
  }
  return out;
}

/** The same label twice (a contributor repeating "Copy") shows once, the first wins. */
export function dedupeLabels(items: readonly MenuItem[]): MenuItem[] {
  const seen = new Set<string>();
  return items.filter(item => {
    if (!item || item === 'separator') return true;
    if (seen.has(item.label)) return false;
    seen.add(item.label); return true;
  });
}

export const isEnabled = (entry: MenuEntry | undefined) => entry?.kind === 'action' && !entry.action.disabled;
export const disabledReason = (action: MenuAction) => typeof action.disabled === 'string' ? action.disabled : undefined;

type Size = { width: number; height: number };
type Point = { x: number; y: number };
type Rect = { left: number; top: number; right: number; bottom: number };

/**
 * A menu opens down and right of the pointer, as on every desktop. Where it
 * would leave the viewport it opens up or left of the pointer instead, and a
 * menu larger than the space either way is pinned inside the margin. Either
 * way it keeps `gap` pixels clear of the pointer, so the press that opened it
 * is never on it (as macOS does).
 */
export function placeMenu(at: Point, size: Size, viewport: Size, margin = 8, gap = 2): { left: number; top: number } {
  const fit = (point: number, extent: number, limit: number) => {
    const after = point + gap, before = point - gap - extent;
    if (after + extent <= limit - margin) return Math.max(margin, after);
    if (before >= margin) return Math.min(before, limit - margin - extent);
    return Math.max(margin, limit - margin - extent);
  };
  return { left: Math.round(fit(at.x, size.width, viewport.width)), top: Math.round(fit(at.y, size.height, viewport.height)) };
}

/** How far (px) the pointer must travel from where a menu opened before a right-button release chooses an item. */
export const releaseSlop = 5;
/**
 * A right-button release on an item chooses it only when the pointer went
 * there from where the menu opened: press, drag onto the item, release, as on
 * macOS. A release where the menu opened (a slow right-click, a trackpad
 * click held a moment) chooses nothing, wherever the menu was placed.
 */
export const rightReleaseChooses = (origin: Point, at: Point, slop = releaseSlop) => Math.hypot(at.x - origin.x, at.y - origin.y) > slop;

/**
 * Ctrl+click on a Mac is the context-menu gesture, not a click: WebKit sends
 * the `contextmenu` event and then a `click` with `ctrlKey`, which must not
 * open, follow or press what is under the pointer.
 */
export const isMenuClick = (event: { button: number; ctrlKey: boolean }, mac: boolean) => mac && event.button === 0 && event.ctrlKey;

/**
 * A tooltip under its element, centred on it; above when there is no room below; and always whole
 * inside the viewport, however close to an edge the element is.
 */
export function placeTooltip(anchor: Rect, size: Size, viewport: Size, gap = 6, margin = 8): { left: number; top: number } {
  const centre = (anchor.left + anchor.right) / 2 - size.width / 2;
  const left = Math.max(margin, Math.min(centre, viewport.width - margin - size.width));
  const below = anchor.bottom + gap, above = anchor.top - gap - size.height;
  const top = below + size.height <= viewport.height - margin ? below : above >= margin ? above : Math.max(margin, viewport.height - margin - size.height);
  return { left: Math.round(left), top: Math.round(top) };
}

/** A submenu beside its item: right of the menu, or left when there is no room; aligned with the item. */
export function placeSubmenu(item: Rect, size: Size, viewport: Size, margin = 8): { left: number; top: number } {
  const right = item.right - 2, left = item.left - size.width + 2;
  const x = right + size.width <= viewport.width - margin ? right : left >= margin ? left : Math.max(margin, viewport.width - margin - size.width);
  const y = Math.max(margin, Math.min(item.top - 5, viewport.height - margin - size.height));
  return { left: Math.round(x), top: Math.round(y) };
}

/** The next enabled item from `from` (-1: none yet), wrapping around. -1 when nothing is enabled. */
export function stepFocus(entries: readonly MenuEntry[], from: number, step: 1 | -1): number {
  const count = entries.length;
  for (let i = 1; i <= count; i++) {
    const index = ((from < 0 ? (step > 0 ? -1 : count) : from) + step * i + count * 2) % count;
    if (isEnabled(entries[index])) return index;
  }
  return -1;
}
export const firstEnabled = (entries: readonly MenuEntry[]) => stepFocus(entries, -1, 1);
export const lastEnabled = (entries: readonly MenuEntry[]) => stepFocus(entries, -1, -1);

/** Typing jumps to the next enabled item whose label starts with what was typed, after the current one. */
export function typeahead(entries: readonly MenuEntry[], query: string, from: number): number {
  const needle = query.toLocaleLowerCase();
  if (!needle) return -1;
  const count = entries.length;
  // Repeating one letter cycles through the items that start with it.
  const repeated = needle.length > 1 && [...needle].every(letter => letter === needle[0]);
  const start = needle.length === 1 || repeated ? from + 1 : Math.max(from, 0);
  for (let i = 0; i < count; i++) {
    const index = (start + i) % count, entry = entries[index];
    if (entry.kind !== 'action' || entry.action.disabled) continue;
    if (entry.action.label.toLocaleLowerCase().startsWith(repeated ? needle[0] : needle)) return index;
  }
  return -1;
}

/** Text quoted into a reply: Markdown quote lines, then a blank line to type under. */
export function quoteText(text: string): string {
  const lines = text.replace(/\s+$/, '').split('\n');
  return lines.map(line => line ? `> ${line}` : '>').join('\n') + '\n\n';
}

/** A fenced Markdown block whose fence outlasts any fence inside the code. */
export function fencedCode(text: string, language = ''): string {
  const longest = Math.max(2, ...[...text.matchAll(/`{3,}/g)].map(match => match[0].length));
  const fence = '`'.repeat(longest + 1);
  return `${fence}${language}\n${text.replace(/\n$/, '')}\n${fence}`;
}

/** A workspace path made absolute against the workspace root ('.' is the root itself). */
export function absolutePath(root: string, path: string): string {
  const base = root.replace(/\/+$/, '');
  if (!base || path.startsWith('/')) return path;
  return path === '.' || !path ? base : `${base}/${path.replace(/^\.\//, '')}`;
}
