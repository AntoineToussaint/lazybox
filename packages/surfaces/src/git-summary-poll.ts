import { Code, ConnectError, type RunnerClient } from '@lazybox/client-core';
import type { WorkspaceGitSummary } from '@lazybox/protocol';

/**
 * Every workspace's git state for the sidebar, polled gently: one request for
 * all workspaces every 20 seconds while the window is visible, at once when it
 * comes back into view, and shortly after something nudges it (an agent
 * finishing a turn, a commit). One poll per Runner client, however many
 * sidebars (the docked one, the phone navigator) show it.
 */
const INTERVAL_MS = 20_000;
const NUDGE_DELAY_MS = 1_200;

export type GitSummarySnapshot = { byWorkspace: ReadonlyMap<string, WorkspaceGitSummary>; unsupported: boolean; loaded: boolean };
const empty: GitSummarySnapshot = { byWorkspace: new Map(), unsupported: false, loaded: false };

export class GitSummaryPoll {
  snapshot: GitSummarySnapshot = empty;
  private listeners = new Set<() => void>();
  private timer: ReturnType<typeof setTimeout> | undefined;
  private nudgeTimer: ReturnType<typeof setTimeout> | undefined;
  /**
   * The request in flight, by its signal. The last sidebar going away aborts it;
   * one coming back (a remount, a layout change) asks again at once rather than
   * waiting on the aborted request, whose answer it would never get.
   */
  private inflight: AbortSignal | undefined;
  private control = new AbortController();
  constructor(private api: Pick<RunnerClient, 'listWorkspaceGitSummaries'>) {}

  subscribe = (listener: () => void) => {
    this.listeners.add(listener);
    if (this.listeners.size === 1) this.start();
    return () => { this.listeners.delete(listener); if (!this.listeners.size) this.stop(); };
  };
  private emit(next: GitSummarySnapshot) { this.snapshot = next; for (const listener of this.listeners) listener(); }
  private visible = () => typeof document === 'undefined' || document.visibilityState !== 'hidden';
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
  /** Ask again soon: something changed the worktrees. Several nudges in a row make one request. */
  nudge() { clearTimeout(this.nudgeTimer); this.nudgeTimer = setTimeout(() => void this.load(), NUDGE_DELAY_MS); }
  async load() {
    if ((this.inflight && !this.inflight.aborted) || this.snapshot.unsupported || !this.listeners.size) return;
    if (!this.visible()) { this.schedule(); return; }
    const signal = this.control.signal;
    this.inflight = signal;
    try {
      const answer = await this.api.listWorkspaceGitSummaries({}, { signal, timeoutMs: 15_000 });
      if (!signal.aborted) this.emit({ byWorkspace: new Map(answer.workspaces.map(w => [w.collectionId, w])), unsupported: false, loaded: true });
    } catch (error) {
      // An older Runner has no summaries: the rows simply show none.
      if (!signal.aborted && ConnectError.from(error).code === Code.Unimplemented) this.emit({ ...this.snapshot, unsupported: true, loaded: true });
    } finally {
      if (this.inflight === signal) this.inflight = undefined;
      if (!signal.aborted && !this.snapshot.unsupported) this.schedule();
    }
  }
}

const polls = new WeakMap<object, GitSummaryPoll>();
/** The one poll of a Runner client. */
export function pollFor(api: RunnerClient) {
  let poll = polls.get(api);
  if (!poll) { poll = new GitSummaryPoll(api); polls.set(api, poll); }
  return poll;
}
/** Ask every sidebar to refresh its git state (after a commit, push or merge). */
export function nudgeGitSummaries(api: RunnerClient) { polls.get(api)?.nudge(); }
