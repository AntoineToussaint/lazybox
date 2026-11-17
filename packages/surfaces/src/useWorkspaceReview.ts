import { useEffect, useMemo, useSyncExternalStore } from 'react';
import type { RunnerClient } from '@lazybox/client-core';
import type { AddReviewCommentRequest, WorkspaceReview } from '@lazybox/protocol';
import { useRunner } from './RunnerContext';
import { errorText } from './PullRequestFlow';

/** One workspace's review comments, shared by every panel of the window (the
 * diff and the tray read the same snapshot) and kept current with the Runner,
 * which holds them for every window: re-read every few seconds while the
 * window is visible, at once when it regains focus, and after each change. */
type Snapshot = { review?: WorkspaceReview; error: string; loaded: boolean };
const POLL_MS = 4000;

class ReviewSource {
  snapshot: Snapshot = { error: '', loaded: false };
  api?: RunnerClient;
  private listeners = new Set<() => void>();
  private timer?: ReturnType<typeof setInterval>;
  private loading = false;
  // A read that started before a change must not undo it on screen.
  private changes = 0;
  constructor(readonly workspaceId: string) {}
  private set(next: Partial<Snapshot>) { this.snapshot = { ...this.snapshot, ...next }; for (const listener of this.listeners) listener(); }
  private wake = () => { if (document.visibilityState === 'visible') void this.load(); };
  subscribe = (listener: () => void) => {
    this.listeners.add(listener);
    if (this.listeners.size === 1) {
      void this.load();
      this.timer = setInterval(() => { if (document.visibilityState === 'visible') void this.load(); }, POLL_MS);
      window.addEventListener('focus', this.wake); document.addEventListener('visibilitychange', this.wake);
    }
    return () => {
      this.listeners.delete(listener);
      if (!this.listeners.size) { clearInterval(this.timer); window.removeEventListener('focus', this.wake); document.removeEventListener('visibilitychange', this.wake); }
    };
  };
  get = () => this.snapshot;
  async load() {
    if (!this.api || this.loading || !this.workspaceId) return;
    this.loading = true;
    const before = this.changes;
    try { const review = await this.api.listReviewComments({ collectionId: this.workspaceId }, { timeoutMs: 15000 }); if (before === this.changes) this.set({ review, error: '', loaded: true }); }
    catch (error) { if (before === this.changes) this.set({ error: errorText(error), loaded: true }); }
    finally { this.loading = false; }
  }
  /** Runs a change; the reply is the whole review, so every panel updates at once. */
  async change(call: (api: RunnerClient) => Promise<WorkspaceReview>) {
    if (!this.api) throw new Error('Not connected to the Runner');
    const review = await call(this.api);
    this.changes++;
    this.set({ review, error: '', loaded: true });
    return review;
  }
}
const sources = new Map<string, ReviewSource>();
const sourceFor = (workspaceId: string) => { let source = sources.get(workspaceId); if (!source) { source = new ReviewSource(workspaceId); sources.set(workspaceId, source); } return source; };
/** Read a workspace's review again now, in every panel showing it (a review just started). */
export function refreshWorkspaceReview(workspaceId: string) { void sources.get(workspaceId)?.load(); }

export function useWorkspaceReview(workspaceId: string) {
  const { api } = useRunner();
  const source = useMemo(() => sourceFor(workspaceId), [workspaceId]);
  source.api = api;
  // A restarted Runner is a new client: read again from it.
  useEffect(() => { if (workspaceId) void source.load(); }, [source, api, workspaceId]);
  const snapshot = useSyncExternalStore(source.subscribe, source.get);
  const collectionId = workspaceId;
  return useMemo(() => ({
    ...snapshot,
    comments: snapshot.review?.comments || [],
    autoResolve: !!snapshot.review?.autoResolve,
    refresh: () => source.load(),
    add: (request: Omit<AddReviewCommentRequest, 'collectionId' | '$typeName' | '$unknown'>) => source.change(client => client.addReviewComment({ ...request, collectionId })),
    edit: (id: string, body: string) => source.change(client => client.editReviewComment({ collectionId, id, body })),
    remove: (id: string) => source.change(client => client.deleteReviewComment({ collectionId, id })),
    /** `note` is a dismissal's reason. */
    setDisposition: (ids: string[], disposition: string, to?: { sessionId: string; sessionTitle: string }, note = '') =>
      source.change(client => client.setReviewDisposition({ collectionId, ids, disposition, sessionId: to?.sessionId || '', sessionTitle: to?.sessionTitle || '', note })),
    /** A review's loop: pause, resume, stop, take_over, or loop (with its mode and rounds). */
    updateRun: (reviewId: string, action: string, loop = '', maxRounds = 0) => source.change(client => client.updateReviewRun({ collectionId, reviewId, action, loop, maxRounds })),
    setAutoResolve: (enabled: boolean) => source.change(client => client.setReviewAutoResolve({ collectionId, enabled })),
  }), [snapshot, source, collectionId]);
}
export type WorkspaceReviewState = ReturnType<typeof useWorkspaceReview>;
