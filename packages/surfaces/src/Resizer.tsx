import { createContext, useContext, useEffect, useLayoutEffect, useRef, type KeyboardEvent as ReactKeyboardEvent, type PointerEvent as ReactPointerEvent } from 'react';
import type { ViewStateStore } from '@lazybox/platform';
import { Icon } from './Icons';
import { DEFAULT_SPLIT, dragSplit, keySplit, readSplit, splitCss, splitMax, writeSplit, type SplitSpec, type SplitState } from './resizable-model';
import './resizer.css';

/** The panel's store, for the dividers inside it (WorkspacePanel provides it). */
export const SplitStoreContext = createContext<ViewStateStore | undefined>(undefined);

/** A phone keeps its stacked and tabbed layouts: no dividers there. */
const PHONE = '(max-width: 479px)';

/**
 * The divider inside a panel. Placed right after the element it resizes
 * (`side: 'start'`) or right before it (`'end'`), in the same container, it
 * sits on the edge between that element and the content beside it whatever
 * the layout (grid or flex), and hides itself when the two are not side by
 * side (a stacked narrow layout, a phone, an overlay).
 *
 * The size is the CSS variable `--split-<name>` on the container, which the
 * panel's CSS reads with its own default as the fallback: a drag writes the
 * variable, never React state, so nothing re-renders while it moves.
 *
 * Drag it, double-click it to go back to the default, or focus it: the
 * arrows move it 16 px (Shift: 64), Home and End go to the minimum and the
 * maximum, Enter folds a collapsible side to a thin rail and back. The size
 * is remembered for this window and kind of divider, in every workspace.
 */
export function Resizer({ split, label, store: given }: { split: SplitSpec; /** What it resizes: "file list". */ label: string; store?: ViewStateStore }) {
  const context = useContext(SplitStoreContext);
  const store = given ?? context;
  const handle = useRef<HTMLDivElement>(null);
  const state = useRef<SplitState>(DEFAULT_SPLIT);
  const axis = split.axis ?? 'x';
  const name = `--split-${split.name}`;

  /** The divider, its container, the side it resizes and the content beside it. */
  const parts = () => {
    const node = handle.current, container = node?.parentElement;
    if (!node || !container) return null;
    const side = split.side === 'start' ? node.previousElementSibling : node.nextElementSibling;
    const other = split.side === 'start' ? node.nextElementSibling : node.previousElementSibling;
    return { node, container, side: side instanceof HTMLElement ? side : null, other: other instanceof HTMLElement ? other : null };
  };
  const extent = (rect: DOMRect) => axis === 'x' ? rect.width : rect.height;

  /** Applies the size, then puts the divider on the edge (or hides it). */
  const layout = () => {
    const found = parts();
    if (!found) return;
    const { node, container, side, other } = found;
    const phone = typeof matchMedia === 'function' && matchMedia(PHONE).matches;
    const inFlow = (element: HTMLElement) => !['absolute', 'fixed'].includes(getComputedStyle(element).position);
    let s = side?.getBoundingClientRect(), o = other?.getBoundingClientRect();
    const beside = !!(side && other && s && o && extent(s) > 0 && extent(o) > 0 && inFlow(side) && inFlow(other) && (axis === 'x'
      ? (split.side === 'start' ? o.left >= s.right - 1 : s.left >= o.right - 1) && o.top < s.bottom && s.top < o.bottom
      : (split.side === 'start' ? o.top >= s.bottom - 1 : s.top >= o.bottom - 1) && o.left < s.right && s.left < o.right));
    const available = beside ? extent(s!) + extent(o!) : undefined;
    const value = phone ? null : splitCss(split, state.current, available);
    if ((container.style.getPropertyValue(name) || null) !== value) {
      if (value === null) container.style.removeProperty(name); else container.style.setProperty(name, value);
      s = side?.getBoundingClientRect(); o = other?.getBoundingClientRect();
    }
    const collapsed = !!split.collapsible && state.current.collapsed;
    node.toggleAttribute('data-collapsed', collapsed);
    if (phone || !beside || !s || !o) { node.setAttribute('data-off', ''); return; }
    node.removeAttribute('data-off');
    // Placed in its offset parent's coordinates (which may scroll).
    const parent = (node.offsetParent as HTMLElement | null) ?? container, p = parent.getBoundingClientRect();
    const left = p.left + parent.clientLeft - parent.scrollLeft, top = p.top + parent.clientTop - parent.scrollTop;
    if (axis === 'x') {
      const edge = collapsed ? s.left + s.width / 2 : split.side === 'start' ? (s.right + o.left) / 2 : (o.right + s.left) / 2;
      const from = Math.min(s.top, o.top), to = Math.max(s.bottom, o.bottom);
      Object.assign(node.style, { left: `${edge - left}px`, top: `${from - top}px`, height: `${to - from}px`, width: '' });
    } else {
      const edge = collapsed ? s.top + s.height / 2 : split.side === 'start' ? (s.bottom + o.top) / 2 : (o.bottom + s.top) / 2;
      const from = Math.min(s.left, o.left), to = Math.max(s.right, o.right);
      Object.assign(node.style, { top: `${edge - top}px`, left: `${from - left}px`, width: `${to - from}px`, height: '' });
    }
    node.setAttribute('aria-valuenow', String(collapsed ? 0 : Math.round(extent(s))));
    node.setAttribute('aria-valuemax', String(splitMax(split, available)));
    node.title = collapsed ? `Show the ${label} (Enter)` : `Drag to resize the ${label} · double-click for its default size`;
  };
  const measured = () => {
    const found = parts(), s = found?.side?.getBoundingClientRect(), o = found?.other?.getBoundingClientRect();
    return { size: s ? Math.round(extent(s)) : 0, available: s && o ? extent(s) + extent(o) : undefined };
  };
  const commit = (next: SplitState) => { state.current = next; layout(); writeSplit(store, split, next); };

  // The remembered size, again when the store changes (another window's panel moved here).
  useLayoutEffect(() => { state.current = readSplit(store, split); layout(); }, [store, split.name]);
  // Laid out again after every render of the panel, and whenever anything around it resizes.
  useLayoutEffect(layout);
  useEffect(() => {
    const container = handle.current?.parentElement;
    if (!container) return;
    let frame = 0;
    const soon = () => { cancelAnimationFrame(frame); frame = requestAnimationFrame(layout); };
    const sizes = new ResizeObserver(soon);
    const watch = () => { sizes.disconnect(); sizes.observe(container); for (const child of container.children) sizes.observe(child); };
    watch();
    const children = new MutationObserver(() => { watch(); soon(); });
    children.observe(container, { childList: true });
    const phone = matchMedia(PHONE);
    phone.addEventListener('change', soon);
    window.addEventListener('resize', soon);
    return () => {
      cancelAnimationFrame(frame); sizes.disconnect(); children.disconnect();
      phone.removeEventListener('change', soon); window.removeEventListener('resize', soon);
      container.style.removeProperty(name);
    };
  }, [name]);

  const down = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (event.button !== 0) return;
    event.preventDefault();
    const node = event.currentTarget, from = state.current, start = from.collapsed ? 0 : measured().size, available = measured().available;
    const origin = axis === 'x' ? event.clientX : event.clientY;
    let moved = false;
    node.focus({ preventScroll: true });
    node.setPointerCapture(event.pointerId);
    node.setAttribute('data-dragging', '');
    const root = document.documentElement;
    root.classList.add('is-resizing'); root.dataset.resizeAxis = axis;
    const move = (next: PointerEvent) => {
      const delta = (axis === 'x' ? next.clientX : next.clientY) - origin;
      if (!moved && Math.abs(delta) < 3) return;
      moved = true;
      state.current = dragSplit(split, from, start, delta, available);
      layout();
    };
    const up = () => {
      node.removeEventListener('pointermove', move); node.removeEventListener('pointerup', up); node.removeEventListener('pointercancel', up);
      node.removeAttribute('data-dragging');
      root.classList.remove('is-resizing'); delete root.dataset.resizeAxis;
      // A click on the rail unfolds it; a drag keeps where it ended.
      if (!moved) { if (from.collapsed) commit({ ...from, collapsed: false }); return; }
      commit(state.current);
    };
    node.addEventListener('pointermove', move); node.addEventListener('pointerup', up); node.addEventListener('pointercancel', up);
  };
  const keys = (event: ReactKeyboardEvent<HTMLDivElement>) => {
    if (event.altKey || event.metaKey || event.ctrlKey) return;
    const { size, available } = measured();
    const next = keySplit(split, state.current, size, event.key, event.shiftKey, available);
    if (!next) return;
    event.preventDefault(); event.stopPropagation();
    commit(next);
  };
  return <div ref={handle} className="resizer" role="separator" tabIndex={0} data-side={split.side} data-axis={axis} data-off=""
    aria-orientation={axis === 'x' ? 'vertical' : 'horizontal'} aria-label={`Resize the ${label}`} aria-valuemin={split.min}
    onPointerDown={down} onDoubleClick={() => commit({ ...DEFAULT_SPLIT })} onKeyDown={keys}>
    <Icon name="chevronRight" size={12}/>
  </div>;
}
