import { useEffect, useRef, type RefObject } from 'react';
import type { NativeWebViews, WebViewFrame } from '@lazybox/platform';

/**
 * Keeps each native web view over its panel. A native view is drawn above
 * the whole page, so it follows the panel's box on every layout change and
 * steps aside whenever something of the app's own would be under it: a menu,
 * a popover, a dialog, the dock preview while a panel is dragged, a
 * collapsed or hidden panel, another page. A view whose panel goes away is
 * hidden at once and closed a minute later, so visiting Settings or another
 * page and coming back keeps the page (and its scroll, its form) as it was.
 */
type Slot = { id: string; element: HTMLElement; views: NativeWebViews; url: string; zoom: number; shown: string; /** Shown once already: later shows only move it. */ placed: boolean; report(error: unknown): void; onShown(shown: boolean): void };
const slots = new Map<string, Slot>();
const closing = new Map<string, ReturnType<typeof setTimeout>>();
export const RELEASE_MS = 60_000;
let frame = 0;
let stop: (() => void) | undefined;

/** Elements of the app's own that a native view must never cover. */
function covers(): Element[] {
  // The dock's drop preview while a panel is dragged, and groups floating over the page.
  const fixed = '.dv-drop-target-anchor, .dv-drop-target-selection, .dv-resize-container, [data-covers-web-view]';
  try { return [...document.querySelectorAll(`:popover-open, dialog[open], ${fixed}`)]; }
  catch { return [...document.querySelectorAll(`dialog[open], ${fixed}`)]; }
}
/**
 * Where an overlay draws. A layer with no box of its own (the context menu's
 * top-layer holder is 0 × 0, its menu fixed inside it) draws where its children do.
 */
function drawn(element: Element, depth = 0): DOMRect[] {
  const rect = element.getBoundingClientRect();
  if (rect.width > 0 && rect.height > 0) return [rect];
  return depth < 3 ? [...element.children].flatMap(child => drawn(child, depth + 1)) : [];
}
export type Box = { left: number; top: number; right: number; bottom: number };
const overlaps = (a: DOMRect, b: Box) => a.width > 0 && a.height > 0 && a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom;
/** The boxes of the ancestors that clip `element` (a dock group, a scrolled container). */
function clipsOf(element: Element): Box[] {
  const clips: Box[] = [];
  // Not the body or the root: their overflow is the viewport's, which already clips.
  for (let node = element.parentElement; node && node !== document.body && node !== document.documentElement; node = node.parentElement) {
    const style = getComputedStyle(node);
    if (style.overflowX === 'visible' && style.overflowY === 'visible' && !/paint|strict|content/.test(style.contain)) continue;
    // Overflow does not apply to a box that is not there (display: contents) or inline.
    if (style.display === 'contents' || style.display === 'inline') continue;
    // Inside its borders and scroll bars, where its content shows.
    const box = node.getBoundingClientRect(), left = box.left + node.clientLeft, top = box.top + node.clientTop;
    clips.push({ left, top, right: left + node.clientWidth, bottom: top + node.clientHeight });
  }
  return clips;
}

/**
 * Where a view goes, in the app page's CSS pixels, or undefined when it must
 * be hidden. `rect` is the page area under the panel's address bar; the view
 * is clipped to what of it is on screen, the viewport and every ancestor that
 * clips it, so it never draws outside its panel. The edges stay fractional:
 * the host converts them to points (the app page may be zoomed) and snaps
 * them inward there, once (viewer.rs, `logical`).
 */
export function placement(rect: Box, viewport: { width: number; height: number }, blockers: DOMRect[], modal: boolean, clips: Box[] = []): Omit<WebViewFrame, 'viewportWidth' | 'viewportHeight'> | undefined {
  if (modal) return undefined;
  let x = Math.max(0, rect.left), y = Math.max(0, rect.top), right = Math.min(viewport.width, rect.right), bottom = Math.min(viewport.height, rect.bottom);
  for (const clip of clips) { x = Math.max(x, clip.left); y = Math.max(y, clip.top); right = Math.min(right, clip.right); bottom = Math.min(bottom, clip.bottom); }
  if (right - x < 8 || bottom - y < 8) return undefined;
  if (blockers.some(blocker => overlaps(blocker, { left: x, top: y, right, bottom }))) return undefined;
  return { x, y, width: right - x, height: bottom - y };
}
const frameKey = (f: WebViewFrame) => [f.x, f.y, f.width, f.height, f.viewportWidth, f.viewportHeight].map(n => n.toFixed(2)).join(',');

function sync() {
  frame = 0;
  const found = covers();
  // While a panel is dragged the drop preview can land anywhere: every view steps aside.
  const modal = document.documentElement.classList.contains('dock-dragging') || found.some(element => { try { return element.matches('dialog:modal'); } catch { return false; } });
  const boxes = found.map(element => ({ element, rects: drawn(element) }));
  // Clipped to what is on screen (without scroll bars), measured against the
  // whole viewport: that is the host's app webview, in points.
  const viewport = { width: document.documentElement.clientWidth, height: document.documentElement.clientHeight };
  const measured = { viewportWidth: window.innerWidth, viewportHeight: window.innerHeight };
  for (const slot of slots.values()) {
    const element = slot.element;
    // A panel moved into a window of its own is not where this window's view can be.
    const visible = element.isConnected && element.ownerDocument === document && document.visibilityState === 'visible' && (element.checkVisibility?.({ visibilityProperty: true } as CheckVisibilityOptions) ?? true);
    if (visible) watch?.(element);
    // What covers a view is anything of the app's own over it, except the floating group it is in.
    const blockers = boxes.filter(box => !box.element.contains(element)).flatMap(box => box.rects);
    const place = visible ? placement(element.getBoundingClientRect(), viewport, blockers, modal, clipsOf(element)) : undefined;
    const target: WebViewFrame | undefined = place && { ...place, ...measured };
    const key = target ? frameKey(target) : 'hidden';
    if (key === slot.shown) continue;
    const wasShown = slot.shown !== 'hidden' && slot.shown !== '';
    slot.shown = key;
    if (target) { const url = slot.placed ? '' : slot.url; slot.placed = true; void slot.views.show(slot.id, url, target, slot.zoom).catch(slot.report); }
    else void slot.views.hide(slot.id).catch(slot.report);
    if (wasShown !== !!target) slot.onShown(!!target);
  }
}
const schedule = () => { if (!frame) frame = requestAnimationFrame(sync); };

function observe() {
  if (stop) return;
  // A panel moves without changing size when something around it does (the
  // sidebar, a split, the Agents panel closing): its ancestors are watched too.
  const resize = new ResizeObserver(schedule);
  const watched = new WeakSet<Element>();
  watch = element => { for (let node: Element | null = element; node; node = node.parentElement) if (!watched.has(node)) { watched.add(node); resize.observe(node); } };
  for (const slot of slots.values()) watch(slot.element);
  const mutations = new MutationObserver(schedule);
  mutations.observe(document.body, { subtree: true, childList: true, attributes: true, attributeFilter: ['open', 'class', 'style', 'hidden', 'popover'] });
  // The appearance (text size, density, theme) is set on the root.
  mutations.observe(document.documentElement, { attributes: true, attributeFilter: ['class', 'style', 'data-theme'] });
  // A popover opening is not a mutation: measure as it toggles, not a frame later.
  const toggled = () => sync();
  window.addEventListener('resize', schedule);
  document.addEventListener('scroll', schedule, true);
  document.addEventListener('toggle', toggled, true);
  document.addEventListener('visibilitychange', schedule);
  // A display of another scale, or the app page zoomed (⌘+): the pixel ratio changes.
  let ratio: MediaQueryList | undefined;
  const rescaled = () => { ratio?.removeEventListener('change', rescaled); ratio = matchMedia(`(resolution: ${window.devicePixelRatio}dppx)`); ratio.addEventListener('change', rescaled); schedule(); };
  rescaled();
  stop = () => {
    resize.disconnect(); mutations.disconnect();
    window.removeEventListener('resize', schedule); document.removeEventListener('scroll', schedule, true);
    document.removeEventListener('toggle', toggled, true); document.removeEventListener('visibilitychange', schedule);
    ratio?.removeEventListener('change', rescaled);
    watch = undefined; stop = undefined;
  };
}
let watch: ((element: Element) => void) | undefined;

function acquire(slot: Slot) {
  const timer = closing.get(slot.id);
  if (timer) { clearTimeout(timer); closing.delete(slot.id); }
  slots.set(slot.id, slot);
  observe(); watch?.(slot.element);
  schedule();
}
function release(slot: Slot) {
  if (slots.get(slot.id) !== slot) return;
  slots.delete(slot.id);
  void slot.views.hide(slot.id).catch(() => {});
  closing.set(slot.id, setTimeout(() => { closing.delete(slot.id); void slot.views.action(slot.id, 'close').catch(() => {}); }, RELEASE_MS));
  if (!slots.size) stop?.();
}
/** Closes a view now: its panel was closed for good. */
export function closeNativeWebView(views: NativeWebViews, id: string) {
  const timer = closing.get(id);
  if (timer) { clearTimeout(timer); closing.delete(id); }
  const slot = slots.get(id);
  if (slot) { slots.delete(id); if (!slots.size) stop?.(); }
  void views.action(id, 'close').catch(() => {});
}
/** Measure again now, for a change the observers cannot see. */
export const refreshNativeWebViews = schedule;

/**
 * Shows the native view `id` over `surface` while the component is mounted.
 * `url` is what a new view loads first; later navigation goes through the
 * views' own `navigate`. `onShown` says whether it is on screen.
 */
export function useNativeWebView(views: NativeWebViews | undefined, id: string, surface: RefObject<HTMLElement | null>, url: string, zoom: number, report: (error: unknown) => void, onShown: (shown: boolean) => void) {
  const latest = useRef({ url, zoom, report, onShown });
  latest.current = { url, zoom, report, onShown };
  const active = !!views && !!url && !!id;
  useEffect(() => {
    const element = surface.current;
    if (!views || !active || !element) return;
    const slot: Slot = { id, element, views, url: latest.current.url, zoom: latest.current.zoom, shown: '', placed: false, report: error => latest.current.report(error), onShown: shown => latest.current.onShown(shown) };
    acquire(slot);
    return () => release(slot);
  }, [views, id, active]);
  useEffect(() => {
    const slot = slots.get(id);
    if (!slot || !views) return;
    slot.url = url;
    if (slot.zoom !== zoom) { slot.zoom = zoom; void views.zoom(id, zoom).catch(report); }
  }, [views, id, url, zoom]);
}
