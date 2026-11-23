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

/** A focus scheduled for a later frame still applies only if focus is where it was when scheduled. */
export function focusUnmoved(before: Element | null, now: Element | null): boolean {
  return before === now;
}
