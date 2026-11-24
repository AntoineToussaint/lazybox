/**
 * A focus the app asks for once the layout it needs is shown (a conversation
 * opened, a panel docked). That can take a while: a task started on another
 * Runner shows once that Runner's page loads. If focus moves meanwhile, the
 * person has gone on, so the request is dropped rather than taking the
 * keyboard back from them.
 *
 * Each owner (a workspace view, the dock) keeps its own request, so one never
 * takes or replaces another's; `trackFocus` puts it on the page's list, which
 * focus moved elsewhere clears, and which a closing palette consults before
 * giving focus back.
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

const tracked = new Set<PendingFocus<unknown>>();
let listening = false;

/** Puts an owner's request on the page's list until the returned function takes it off (an unmounted view's request goes with it). */
export function trackFocus(focus: PendingFocus<unknown>): () => void {
  if (!listening && typeof document !== 'undefined') {
    listening = true;
    // Focus moved by anything else (the person, another part of the app) drops every waiting request.
    document.addEventListener('focusin', () => { for (const owner of tracked) owner.moved(); });
  }
  tracked.add(focus);
  return () => { tracked.delete(focus); };
}

/** Whether a focus the app asked for is waiting anywhere on the page. */
export function focusWaiting(): boolean {
  for (const owner of tracked) if (owner.waiting !== null) return true;
  return false;
}

/** Gives focus back to `element` (a palette or overlay closing), unless a focus the app asked for is waiting: that one wins. */
export function restoreFocus(element: HTMLElement | null | undefined) {
  if (!element?.isConnected || focusWaiting()) return;
  element.focus?.({ preventScroll: true });
}
