import './drag-gesture.css';

/**
 * One drag of a handle, by the pointer: a divider, an edge, a header, a tab.
 * Every drag in the app goes through `beginDrag`, so none of them selects text.
 *
 * A held button is a text selection to the browser: unless the press is
 * refused, the `mousedown` anchors one and every move extends it over whatever
 * the pointer crosses. WebKit (the desktop app's engine) does that even from a
 * handle that is `user-select: none`, as dockview's dividers are; Chromium does
 * not. Refusing the press keeps a new selection from starting, but leaves the
 * one made before it: that stays highlighted through the drag, or is hidden
 * only while `user-select: none` is on and comes back on the release.
 *
 * So on the press, `beginDrag`:
 * - clears the selection (the focus stays where it is);
 * - marks the page `html.is-dragging`: no text selectable anywhere, the drag's
 *   cursor everywhere, and frames and web views do not take the pointer;
 * - refuses `selectstart`, the `mousedown` default and native drags;
 * and undoes all of it when the drag ends: on the release, a cancelled
 * pointer, the window losing focus, Escape, or the handle losing the pointer.
 *
 * Call it from the handle's `pointerdown` (the `mousedown` comes after it), and
 * give the handle `onMouseDown={preventMouseDown}` too. Keyboard resizing is
 * not a drag and does not come here.
 */

/** What the drag does: resizes along x (`col-resize`) or y (`row-resize`), or moves something (`grabbing`). */
export type DragAxis = 'x' | 'y' | 'move';
/** Why a drag ended: released, the pointer cancelled, Escape, the window lost focus, the handle lost the pointer, another drag began, or `end()` was called. */
export type DragEnd = 'release' | 'cancel' | 'escape' | 'blur' | 'lost' | 'replaced' | 'ended';

export type DragOptions = {
  /** The pointer this drag follows; other pointers are ignored. */
  pointerId?: number;
  /** The element that captures the pointer for the drag: losing the capture ends it. */
  capture?: Element | null;
  /** A cursor for the whole page instead of the axis's own (the computed cursor of a library's handle). */
  cursor?: string;
  /**
   * Whether Escape ends the drag (the default; it does nothing else meanwhile).
   * False for a library's own drag, which Escape would not stop.
   */
  escape?: boolean;
  /** Each move of the drag's pointer. */
  onMove?(event: PointerEvent): void;
  /** Once, when the drag ends, however it ends. `event` is the pointer event that ended it, if one did. */
  onEnd?(reason: DragEnd, event?: PointerEvent): void;
};

const CURSORS: Record<DragAxis, string> = { x: 'col-resize', y: 'row-resize', move: 'grabbing' };

/** The drag under way: one at a time. */
let current: ((reason: DragEnd) => void) | undefined;

/** Whether a drag is under way. */
export function isDragging(): boolean { return current !== undefined; }

/** For a handle's `onMouseDown`: the press is a drag, not the start of a text selection. */
export function preventMouseDown(event: { button: number; preventDefault(): void }): void { if (event.button === 0) event.preventDefault(); }

/**
 * Clears the page's text selection, keeping the focus where it is. The
 * selection inside the focused text field (or editable) is that field's own
 * and is left alone: a drag elsewhere does not extend it, and the person may
 * want it back.
 */
export function clearSelection(doc: Document = document): void {
  const selection = doc.getSelection();
  if (!selection || !selection.rangeCount || selection.isCollapsed) return;
  const focused = doc.activeElement;
  const field = focused instanceof HTMLInputElement || focused instanceof HTMLTextAreaElement || (focused instanceof HTMLElement && focused.isContentEditable);
  if (field && selection.getRangeAt(0).intersectsNode(focused)) return;
  selection.removeAllRanges();
  if (focused instanceof HTMLElement && focused.isConnected && doc.activeElement !== focused) focused.focus({ preventScroll: true });
}

/** Starts a drag. Returns `end()`, which ends it (once; later calls do nothing). */
export function beginDrag(axis: DragAxis, options: DragOptions = {}): () => void {
  current?.('replaced');
  const { pointerId, capture, onMove, onEnd } = options;
  const doc = capture?.ownerDocument ?? document, view = doc.defaultView ?? window, root = doc.documentElement;
  clearSelection(doc);
  root.classList.add('is-dragging');
  root.dataset.dragAxis = axis;
  root.style.setProperty('--drag-cursor', options.cursor || CURSORS[axis]);

  const ours = (event: PointerEvent) => pointerId === undefined || event.pointerId === pointerId;
  const refuse = (event: Event) => event.preventDefault();
  const move = (event: PointerEvent) => { if (ours(event)) onMove?.(event); };
  const release = (event: PointerEvent) => { if (ours(event)) finish(event.type === 'pointerup' ? 'release' : 'cancel', event); };
  const key = (event: KeyboardEvent) => { if (event.key !== 'Escape' || options.escape === false) return; event.preventDefault(); event.stopPropagation(); finish('escape'); };
  const blur = () => finish('blur');
  const lost = (event: Event) => { if (ours(event as PointerEvent)) finish('lost'); };

  let done = false;
  function finish(reason: DragEnd, event?: PointerEvent) {
    if (done) return;
    done = true;
    if (current === stop) current = undefined;
    for (const type of ['selectstart', 'mousedown', 'dragstart']) doc.removeEventListener(type, refuse, true);
    doc.removeEventListener('keydown', key, true);
    view.removeEventListener('pointermove', move, true);
    view.removeEventListener('pointerup', release, true);
    view.removeEventListener('pointercancel', release, true);
    view.removeEventListener('blur', blur);
    doc.removeEventListener('lostpointercapture', lost, true);
    if (capture && pointerId !== undefined) { try { if (capture.hasPointerCapture(pointerId)) capture.releasePointerCapture(pointerId); } catch { /* the pointer is gone */ } }
    // Another drag that began in the meantime keeps the page marked.
    if (!current) { root.classList.remove('is-dragging'); delete root.dataset.dragAxis; root.style.removeProperty('--drag-cursor'); }
    onEnd?.(reason, event);
  }
  const stop = (reason: DragEnd) => finish(reason);
  current = stop;

  for (const type of ['selectstart', 'mousedown', 'dragstart']) doc.addEventListener(type, refuse, true);
  doc.addEventListener('keydown', key, true);
  view.addEventListener('pointermove', move, true);
  view.addEventListener('pointerup', release, true);
  view.addEventListener('pointercancel', release, true);
  view.addEventListener('blur', blur);
  if (capture && pointerId !== undefined) {
    try { capture.setPointerCapture(pointerId); } catch { /* a pointer that is not down (a synthetic one) has nothing to capture */ }
    // At the handle, or at the document when the handle was removed mid-drag.
    doc.addEventListener('lostpointercapture', lost, true);
  }
  return () => finish('ended');
}
