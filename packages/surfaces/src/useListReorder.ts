import { useEffect, useMemo, useRef, useState, type KeyboardEvent as ReactKeyboardEvent, type MouseEvent as ReactMouseEvent, type PointerEvent as ReactPointerEvent } from 'react';
import { holdWorkspaceHover } from './WorkspaceHover';
import { dropAt, dropSlot, edgeScroll, inOrder, moveBy, movedWords, sameOrder, sameSlot } from './workspace-order';
import './list-reorder.css';

/** How far the pointer moves before a press becomes a drag (a click stays a click). */
const THRESHOLD_PX = 4;
/** After the Runner took an order, how long a move is shown before the list must say it. */
const SETTLE_MS = 5000;

type Options<T> = {
  /** The rows in the Runner's order. */
  items: readonly T[];
  name(item: T): string;
  /** Keeps the new order (ArrangeWorkspaces). */
  arrange?(ids: string[]): Promise<unknown>;
  onError?(message: string): void;
  /** What a drag starts from, and what ⌥⇧↑ / ⌥⇧↓ act on: the row's main button. */
  grip: string;
};

type Drag = { id: string; slot: number };

/**
 * Drag-to-reorder for one list (the workspaces of one Runner): a press on a
 * row's grip that moves a few pixels picks the row up; a line shows where it
 * lands; the list scrolls near its edges; Esc puts it back. ⌥⇧↑ and ⌥⇧↓ move
 * a focused row one place. The new order shows at once and is kept by
 * `arrange`; a refusal puts the Runner's back. Dragging never selects text:
 * the press's default is prevented, a selection is cleared, and the page
 * cannot select while the row moves (`html.is-reordering`).
 */
export function useListReorder<T extends { id: string }>({ items, name, arrange, onError, grip }: Options<T>) {
  const [pending, setPending] = useState<string[]>();
  const [settled, setSettled] = useState(0);
  const [drag, setDrag] = useState<Drag | null>(null);
  const [announcement, setAnnouncement] = useState('');
  const list = useRef<HTMLElement | null>(null);
  const queue = useRef<Promise<unknown>>(Promise.resolve());
  const shown = useMemo(() => inOrder(items, pending), [items, pending]);
  const ids = useMemo(() => shown.map(item => item.id), [shown]);
  const live = useRef({ ids, shown, name, arrange, onError });
  live.current = { ids, shown, name, arrange, onError };

  // The move shows until the Runner lists in it (or, after it took it, a little longer at most).
  useEffect(() => {
    if (pending && sameOrder(items.map(item => item.id), inOrder(items, pending).map(item => item.id))) setPending(undefined);
  }, [items, pending]);
  useEffect(() => {
    if (!settled) return;
    const timer = setTimeout(() => setPending(undefined), SETTLE_MS);
    return () => clearTimeout(timer);
  }, [settled]);

  const commit = (next: string[], id: string) => {
    const { arrange: keep, onError: fail, shown: rows, name: nameOf } = live.current;
    if (!keep) return;
    setPending(next); setSettled(0);
    const row = rows.find(item => item.id === id);
    if (row) setAnnouncement(movedWords(nameOf(row), next, id));
    // In order: the last move is the one the Runner keeps.
    const done = queue.current.then(() => keep(next));
    queue.current = done.catch(() => {});
    done.then(() => setSettled(Date.now()), error => { setPending(undefined); fail?.(`The workspaces could not be reordered: ${error instanceof Error ? error.message : String(error)}`); });
  };
  const focusGrip = (id: string) => requestAnimationFrame(() => list.current?.querySelector<HTMLElement>(`[data-reorder-id="${CSS.escape(id)}"] ${grip}`)?.focus({ preventScroll: false }));

  const begin = (id: string, start: PointerEvent | ReactPointerEvent) => {
    const startY = start.clientY;
    let lastY = startY, active: Drag | null = null, frame = 0;
    const rows = () => [...(list.current?.querySelectorAll<HTMLElement>(':scope [data-reorder-id]') || [])].filter(row => row.closest('[data-reorder-list]') === list.current);
    const scroller = (() => { for (let node = list.current?.parentElement; node; node = node.parentElement) { const style = getComputedStyle(node); if (/(auto|scroll)/.test(style.overflowY) && node.scrollHeight > node.clientHeight) return node; } return document.scrollingElement as HTMLElement | null; })();
    const place = () => {
      const slot = dropSlot(rows().map(row => row.getBoundingClientRect()), lastY);
      if (active && active.slot !== slot) { active = { id, slot }; setDrag(active); }
    };
    const blockSelect = (event: Event) => event.preventDefault();
    const tick = () => {
      frame = 0;
      if (!active || !scroller) return;
      const box = scroller === document.scrollingElement ? { top: 0, bottom: window.innerHeight } : scroller.getBoundingClientRect();
      const speed = edgeScroll(lastY, box.top, box.bottom);
      if (!speed) return;
      const before = scroller.scrollTop;
      scroller.scrollTop += speed;
      if (scroller.scrollTop !== before) place();
      frame = requestAnimationFrame(tick);
    };
    const finish = (keep: boolean) => {
      window.removeEventListener('pointermove', move, true);
      window.removeEventListener('pointerup', up, true);
      window.removeEventListener('pointercancel', cancel, true);
      window.removeEventListener('keydown', key, true);
      window.removeEventListener('blur', cancel);
      document.removeEventListener('selectstart', blockSelect, true);
      cancelAnimationFrame(frame);
      if (!active) return;
      document.documentElement.classList.remove('is-reordering');
      holdWorkspaceHover(false);
      const { ids: order } = live.current;
      if (keep && !sameSlot(order, id, active.slot)) commit(dropAt(order, id, active.slot), id);
      active = null; setDrag(null);
      // The click a drop ends with is not a click on the row.
      const swallow = (event: MouseEvent) => { event.preventDefault(); event.stopPropagation(); };
      window.addEventListener('click', swallow, { capture: true, once: true });
      setTimeout(() => window.removeEventListener('click', swallow, true), 0);
    };
    const move = (event: PointerEvent) => {
      lastY = event.clientY;
      if (!active) {
        if (Math.abs(lastY - startY) < THRESHOLD_PX) return;
        active = { id, slot: -1 };
        window.getSelection()?.removeAllRanges();
        document.documentElement.classList.add('is-reordering');
        // No hover card opens on a row the dragged one passes over.
        holdWorkspaceHover(true);
      }
      event.preventDefault();
      place();
      if (!frame) frame = requestAnimationFrame(tick);
    };
    const up = () => finish(true);
    const cancel = () => finish(false);
    const key = (event: KeyboardEvent) => { if (event.key === 'Escape' && active) { event.preventDefault(); event.stopPropagation(); finish(false); } };
    window.addEventListener('pointermove', move, true);
    window.addEventListener('pointerup', up, true);
    window.addEventListener('pointercancel', cancel, true);
    window.addEventListener('keydown', key, true);
    window.addEventListener('blur', cancel);
    document.addEventListener('selectstart', blockSelect, true);
  };

  /** Props for a row of the list (its whole group, with what is nested under it). */
  const itemProps = (id: string) => ({
    'data-reorder-id': id,
    'data-dragging': drag?.id === id || undefined,
    'data-drop': drag && drag.slot >= 0 && !sameSlot(ids, drag.id, drag.slot) ? (ids[drag.slot] === id ? 'before' : drag.slot === ids.length && ids.at(-1) === id ? 'after' : undefined) : undefined,
    onMouseDown(event: ReactMouseEvent<HTMLElement>) {
      // A press on the grip never starts a text selection. (WebKit never focuses a
      // clicked button anyway, so nothing else changes in the desktop app.)
      const target = event.target as HTMLElement;
      if (!arrange || event.button !== 0 || !target.closest(grip) || event.altKey || event.metaKey || event.ctrlKey || event.shiftKey) return;
      event.preventDefault();
    },
    onPointerDown(event: ReactPointerEvent<HTMLElement>) {
      const target = event.target as HTMLElement;
      // Touch scrolls the list; ⌥-click peeks; a modified click is the row's own.
      if (!arrange || event.button !== 0 || event.pointerType === 'touch' || !target.closest(grip) || event.altKey || event.metaKey || event.ctrlKey || event.shiftKey) return;
      begin(id, event);
    },
    onKeyDown(event: ReactKeyboardEvent<HTMLElement>) {
      if (!arrange || !event.altKey || !event.shiftKey || event.metaKey || event.ctrlKey || (event.key !== 'ArrowUp' && event.key !== 'ArrowDown')) return;
      if (!(event.target as HTMLElement).closest(grip)) return;
      event.preventDefault(); event.stopPropagation();
      const next = moveBy(ids, id, event.key === 'ArrowUp' ? -1 : 1);
      if (sameOrder(next, ids)) return;
      commit(next, id);
      focusGrip(id);
    },
  });

  /** Props for the list itself. */
  const listProps = {
    ref: (node: HTMLElement | null) => { list.current = node; },
    'data-reorder-list': '',
    'data-reordering': drag ? '' : undefined,
  };
  return { items: shown, itemProps, listProps, announcement, dragging: !!drag };
}
