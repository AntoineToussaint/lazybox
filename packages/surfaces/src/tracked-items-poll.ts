import { Code, ConnectError, type RunnerClient } from '@lazybox/client-core';
import type { ItemEvent, ItemLink } from '@lazybox/protocol';

/**
 * Every tracked pull request across workspaces (ListLinks), polled gently:
 * every 20 seconds while the window is visible, at once when it comes back,
 * and shortly after a reaction or a merge. One poll per Runner client,
 * however many places show them (the header, the sidebar, the panel, the
 * Inbox). The request says whether this window has focus: the Runner reads
 * open pull requests about every minute then, every five minutes otherwise.
 * The answer is the Runner's store: no request waits on GitHub.
 */
const INTERVAL_MS = 20_000;
const NUDGE_DELAY_MS = 800;

export type TrackedItems = { links: readonly ItemLink[]; events: readonly ItemEvent[]; trackingError: string; pollSeconds: number; loaded: boolean; unsupported: boolean };
const empty: TrackedItems = { links: [], events: [], trackingError: '', pollSeconds: 0, loaded: false, unsupported: false };

export class TrackedItemsPoll {
  snapshot: TrackedItems = empty;
  private listeners = new Set<() => void>();
  private timer: ReturnType<typeof setTimeout> | undefined;
  private nudgeTimer: ReturnType<typeof setTimeout> | undefined;
  private inflight: AbortSignal | undefined;
  private control = new AbortController();
  constructor(private api: Pick<RunnerClient, 'listLinks'>) {}

  subscribe = (listener: () => void) => {
    this.listeners.add(listener);
    if (this.listeners.size === 1) this.start();
    return () => { this.listeners.delete(listener); if (!this.listeners.size) this.stop(); };
  };
  private emit(next: TrackedItems) { this.snapshot = next; for (const listener of this.listeners) listener(); }
  private visible = () => typeof document === 'undefined' || document.visibilityState !== 'hidden';
  private focused = () => typeof document !== 'undefined' && document.visibilityState === 'visible' && document.hasFocus();
  private onVisible = () => { if (this.visible()) void this.load(); };
  private start() {
    this.control = new AbortController();
    globalThis.document?.addEventListener('visibilitychange', this.onVisible);
    globalThis.window?.addEventListener('focus', this.onVisible);
    void this.load();
  }
  private stop() {
    this.control.abort(); clearTimeout(this.timer); clearTimeout(this.nudgeTimer);
    globalThis.document?.removeEventListener('visibilitychange', this.onVisible);
    globalThis.window?.removeEventListener('focus', this.onVisible);
  }
  private schedule() { clearTimeout(this.timer); this.timer = setTimeout(() => void this.load(), INTERVAL_MS); }
  /** Ask again soon: a reaction, a merge or a link changed something. */
  nudge() { clearTimeout(this.nudgeTimer); this.nudgeTimer = setTimeout(() => void this.load(), NUDGE_DELAY_MS); }
  async load() {
    if ((this.inflight && !this.inflight.aborted) || this.snapshot.unsupported || !this.listeners.size) return;
    if (!this.visible()) { this.schedule(); return; }
    const signal = this.control.signal;
    this.inflight = signal;
    try {
      const answer = await this.api.listLinks({ all: true, focused: this.focused() }, { signal, timeoutMs: 15_000 });
      if (!signal.aborted) this.emit({ links: answer.links, events: answer.events, trackingError: answer.trackingError, pollSeconds: answer.pollSeconds, loaded: true, unsupported: false });
    } catch (error) {
      // An older Runner tracks nothing: every place simply shows none.
      if (!signal.aborted && ConnectError.from(error).code === Code.Unimplemented) this.emit({ ...empty, loaded: true, unsupported: true });
    } finally {
      if (this.inflight === signal) this.inflight = undefined;
      if (!signal.aborted && !this.snapshot.unsupported) this.schedule();
    }
  }
}

const polls = new WeakMap<object, TrackedItemsPoll>();
/** The one poll of a Runner client. */
export function trackedPollFor(api: RunnerClient) {
  let poll = polls.get(api);
  if (!poll) { poll = new TrackedItemsPoll(api); polls.set(api, poll); }
  return poll;
}
/** Ask every view of tracked pull requests to refresh (after a reaction or a merge). */
export function nudgeTrackedItems(api: RunnerClient) { polls.get(api)?.nudge(); }
