/**
 * A focus the app asks for once the layout it needs is shown (a conversation
 * opened, a panel docked). That can take a while: a task started on another
 * Runner shows once that Runner's page loads. If focus moves meanwhile, the
 * person has gone on, so the request is dropped rather than taking the
 * keyboard back from them.
 */
export class PendingFocus<T> {
  private request: T | null = null;
  /** Asks for focus once the layout settles; a later ask replaces it. */
  ask(request: T) { this.request = request; }
  /** Focus moved: whatever was waiting is dropped. */
  moved() { this.request = null; }
  /** What is waiting, if anything. */
  get waiting(): T | null { return this.request; }
  /** Takes request to run it now: false when it was dropped or replaced since. */
  take(request: T): boolean {
    if (this.request !== request) return false;
    this.request = null;
    return true;
  }
}

/**
 * The page's one focus intent: every deferred focus (Workspace's focus after a
 * layout change, the dock's focus into a panel) waits here, so one rule
 * decides them all. Focus moved by anything else drops it; the app's own
 * "give focus back" (a closing palette) yields to it instead of cancelling it.
 */
export const focusIntent = new PendingFocus<unknown>();
if (typeof document !== 'undefined') document.addEventListener('focusin', () => focusIntent.moved());

/** Gives focus back to `element` (a palette or overlay closing), unless a focus the app asked for is waiting: that one wins. */
export function restoreFocus(element: HTMLElement | null | undefined) {
  if (!element?.isConnected || focusIntent.waiting) return;
  element.focus?.({ preventScroll: true });
}
