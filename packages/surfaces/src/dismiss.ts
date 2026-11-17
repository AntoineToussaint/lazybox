import { useEffect, useRef, type RefObject } from 'react';

/**
 * One dismissal rule for every dialog, sheet, popover and picker:
 *
 * - **Escape** closes the surface in front.
 * - **A click outside** closes it too. Over a modal dialog's backdrop the
 *   click is swallowed (the page under it is inert); beside a popover it goes
 *   on to what it landed on. Only a press *and* release on the backdrop count:
 *   a text selection dragged out of the dialog never closes it.
 * - **Work is never lost to a stray click.** A dialog holding something the
 *   person typed or changed stays open on a click outside and nudges its
 *   footer (the macOS sheet's shake); Escape, Cancel and × still close it.
 *   A dialog that keeps its draft when it closes (New task) says so and
 *   closes like any other.
 * - A busy dialog ignores the click the way its `cancel` handler already
 *   ignores Escape: the click asks through the same `cancel` event.
 */

/** Pointer events a popover already used to close itself: a modal under it does not close on the same press. */
const consumed = new WeakSet<Event>();
/** Presses that closed a context menu: they close nothing else, as a native menu's dismissal does not. */
const menuPresses = new WeakSet<Event>();

/**
 * A context menu (ContextMenu.tsx) closed on this press outside it: the
 * dialog or popover under the menu stays open.
 */
export function claimPress(event: Event) { consumed.add(event); menuPresses.add(event); }

/** A Lazybox context menu is on screen: Escape and a press outside are its own until it closes. */
const contextMenuShown = () => !!document.querySelector('.context-menu-layer');

/** Open popovers and pickers, innermost last: Escape closes only the last. */
const stack: symbol[] = [];

const inside = (rect: DOMRect, event: MouseEvent) => event.clientX >= rect.left && event.clientX <= rect.right && event.clientY >= rect.top && event.clientY <= rect.bottom;

/**
 * True when a press on a modal `<dialog>` is on its backdrop: a click there
 * targets the dialog element itself, and so does a click on its own padding,
 * which is inside its box.
 */
export function onBackdrop(dialog: HTMLDialogElement, event: MouseEvent): boolean {
  return event.target === dialog && dialog.open && !inside(dialog.getBoundingClientRect(), event);
}

/** A light-dismiss popover open over the page: a press that closes it closes nothing else. */
const nativePopoverOpen = () => { try { return !!document.querySelector('[popover="auto"]:popover-open, [popover=""]:popover-open, .context-menu-layer:popover-open'); } catch { return false; } };

/**
 * Asks a modal dialog to close the way Escape does: a cancelable `cancel`
 * event (its `onCancel` decides, and may refuse while busy), then `close()`.
 */
export function requestDialogClose(dialog: HTMLDialogElement) {
  if (!dialog.open) return;
  if (dialog.dispatchEvent(new Event('cancel', { cancelable: true })) && dialog.open) dialog.close();
}

/**
 * Wires the backdrop of a modal `<dialog>`: a press and a release both on it
 * call `dismiss`. A press that starts in the dialog (a text selection, a
 * drag) and ends outside does not, and neither does the press that closes a
 * popover open above it. Returns the cleanup.
 */
export function bindBackdropDismiss(dialog: HTMLDialogElement, dismiss: (event: MouseEvent) => void, isBackdrop: (event: MouseEvent) => boolean = event => onBackdrop(dialog, event)): () => void {
  let armed = false;
  const down = (event: PointerEvent) => {
    armed = event.isPrimary && event.button === 0 && dialog.open && isBackdrop(event) && !consumed.has(event) && !nativePopoverOpen();
  };
  // The click, not the pointerup, closes: it is the last event of the press,
  // so nothing under the dialog receives a click once it is gone.
  const click = (event: MouseEvent) => {
    const go = armed && dialog.open && isBackdrop(event);
    armed = false;
    if (go) dismiss(event);
  };
  const cancel = () => { armed = false; };
  dialog.addEventListener('pointerdown', down);
  dialog.addEventListener('click', click);
  dialog.addEventListener('pointercancel', cancel);
  return () => { dialog.removeEventListener('pointerdown', down); dialog.removeEventListener('click', click); dialog.removeEventListener('pointercancel', cancel); };
}

/**
 * For a modal `<dialog>` of its own (not `DraggableDialog`, which does this
 * already): a click on the backdrop asks it to close, as Escape does.
 * `isBackdrop` widens the backdrop, for a full-screen dialog whose empty
 * area is its own. `holding` names what would be lost (a note being typed):
 * while it returns an element, a click outside nudges that element instead
 * of closing.
 */
export function useBackdropDismiss(dialog: RefObject<HTMLDialogElement | null>, isBackdrop?: (event: MouseEvent) => boolean, holding?: () => HTMLElement | null | undefined) {
  const latest = useRef({ isBackdrop, holding }); latest.current = { isBackdrop, holding };
  useEffect(() => {
    const node = dialog.current;
    if (!node) return;
    return bindBackdropDismiss(node, () => {
      const held = latest.current.holding?.();
      if (held) { nudge(held); return; }
      requestDialogClose(node);
    }, event => latest.current.isBackdrop ? latest.current.isBackdrop(event) : onBackdrop(node, event));
  }, [dialog]);
}

/** The first field in `root` with text the person typed (not a search): what a stray click must not throw away. */
export function typedField(root: Element | null | undefined): HTMLElement | null {
  if (!root) return null;
  for (const field of root.querySelectorAll<HTMLInputElement | HTMLTextAreaElement>('textarea, input:not([type=checkbox]):not([type=radio])')) {
    if (isEditField(field) && !field.disabled && field.value.trim()) return field;
  }
  return null;
}

/**
 * Plays the "not like this" nudge on a dialog that refused a click outside:
 * its footer (Cancel / Save) shakes, or flashes with reduced motion.
 */
export function nudge(dialog: HTMLElement) {
  const footers = [...dialog.querySelectorAll<HTMLElement>('footer')].filter(footer => footer.querySelector('button') && footer.closest('dialog') === dialog);
  const target = footers.at(-1) || dialog;
  target.classList.remove('dismiss-nudge');
  // Restart the animation when the person clicks again before it ended.
  void target.offsetWidth;
  target.classList.add('dismiss-nudge');
  window.setTimeout(() => target.classList.remove('dismiss-nudge'), 450);
}

/** Fields whose value is a query, not work: typing in them never guards a dialog. */
export function isEditField(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement) || !target.matches('input, textarea, select, [contenteditable="true"], [contenteditable=""]')) return false;
  if (target.matches('input[type="search"], [role="combobox"], [role="searchbox"], [data-dismiss-query]')) return false;
  return true;
}

const focusable = (node: Element | null): node is HTMLElement => node instanceof HTMLElement && node.isConnected && node !== document.body && !node.closest('[inert]') && !(node as HTMLButtonElement).disabled;

/**
 * Dismissal for a popover, picker or menu that is not a modal dialog: a
 * press outside every `within` element closes it (the press goes on to what
 * it landed on), and Escape closes it while it is the innermost one open.
 * The first of `within` is the popover itself.
 *
 * Focus goes back where it was when it opened — the message box, say — when
 * it closes and the focus would otherwise be lost: after Escape or a choice,
 * and after a press outside that landed on nothing focusable and selected no
 * text. A trigger that should give the focus back keeps it on press
 * (`onMouseDown={event => event.preventDefault()}`).
 *
 * A trigger that toggles the popover belongs in `within`, or its own click
 * would reopen what the press just closed.
 */
export function useDismiss(open: boolean, onDismiss: (how: 'outside' | 'escape') => void, within: ReadonlyArray<RefObject<Element | null>>, options: { escape?: boolean; restoreFocus?: boolean } = {}) {
  const latest = useRef({ onDismiss, within, options }); latest.current = { onDismiss, within, options };
  useEffect(() => {
    if (!open) return;
    const id = Symbol('dismissable');
    // One that leaves Escape to its owner is not in line for it.
    if (latest.current.options.escape !== false) stack.push(id);
    const returnTo = document.activeElement;
    let pressedOutside = false;
    const restore = () => {
      if (latest.current.options.restoreFocus === false || !focusable(returnTo)) return;
      const now = document.activeElement;
      if (now && now !== document.body && now.isConnected) return;
      // A press that selected text keeps its selection: focusing a field would take it.
      if (pressedOutside && !(document.getSelection()?.isCollapsed ?? true)) return;
      returnTo.focus({ preventScroll: true });
    };
    const contains = (target: EventTarget | null) => target instanceof Node && latest.current.within.some(ref => ref.current?.contains(target));
    // A press or a key in a modal dialog opened above this popover belongs to that dialog.
    const elsewhere = (target: EventTarget | null) => {
      const panel = latest.current.within[0]?.current;
      const layer = target instanceof Element ? target.closest('dialog[open]') : null;
      return !!layer && !(panel && layer.contains(panel));
    };
    const press = (event: PointerEvent) => {
      // A press in a context menu opened over the popover is the menu's; one that closed a menu closes nothing else.
      if (menuPresses.has(event) || (event.target instanceof Element && event.target.closest('.context-menu-layer')) || contains(event.target) || elsewhere(event.target)) return;
      consumed.add(event);
      pressedOutside = true;
      latest.current.onDismiss('outside');
      // The focus settles once the press ends.
      const settle = () => { window.removeEventListener('pointerup', settle, true); window.removeEventListener('pointercancel', settle, true); requestAnimationFrame(restore); };
      window.addEventListener('pointerup', settle, true);
      window.addEventListener('pointercancel', settle, true);
    };
    // Capture: the innermost popover takes Escape before whatever holds the focus inside it.
    const key = (event: KeyboardEvent) => {
      // A context menu over the popover takes Escape first (its own handler closes it).
      if (event.key !== 'Escape' || event.defaultPrevented || event.isComposing || latest.current.options.escape === false || stack.at(-1) !== id || elsewhere(event.target) || contextMenuShown()) return;
      event.preventDefault(); event.stopPropagation();
      latest.current.onDismiss('escape');
    };
    window.addEventListener('pointerdown', press, true);
    window.addEventListener('keydown', key, true);
    return () => {
      window.removeEventListener('pointerdown', press, true);
      window.removeEventListener('keydown', key, true);
      if (stack.includes(id)) stack.splice(stack.indexOf(id), 1);
      // Closed by a key or a choice: the focus was in the popover, which is gone.
      if (!pressedOutside) requestAnimationFrame(restore);
    };
  }, [open]);
}
