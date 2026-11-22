import { useCallback, useState, useSyncExternalStore } from 'react';
import { ConnectError } from '@lazybox/client-core';
import type { ItemLink } from '@lazybox/protocol';
import { useRunner } from './RunnerContext';
import { nudgeTrackedItems, trackedPollFor } from './tracked-items-poll';
import { nudgeGitSummaries } from './useGitSummaries';
import { parseDismissed, type TrackedPull } from './pull-requests-model';

/** Every tracked pull request of the window's Runner (tracked-items-poll.ts). */
export function useTrackedItems() {
  const { api } = useRunner();
  const poll = trackedPollFor(api);
  return useSyncExternalStore(poll.subscribe, () => poll.snapshot);
}

const DISMISSED = 'lazybox:pull-inbox-dismissed';
const listeners = new Set<() => void>();
const completed = new Map<string, { text: string; until: number; timer: number }>();
const readDismissed = () => {
  try { return new Set(parseDismissed(localStorage.getItem(DISMISSED))); }
  catch { return new Set<string>(); }
};
let dismissed = readDismissed();
let snapshot = { dismissed, completed: new Map(completed) };
function publish() {
  snapshot = { dismissed, completed: new Map(completed) };
  listeners.forEach(listener => listener());
}
function subscribe(listener: () => void) {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
}
function remember(key: string) {
  const keys = new Set([...readDismissed(), ...dismissed]);
  keys.add(key);
  dismissed = new Set([...keys].slice(-500));
  try { localStorage.setItem(DISMISSED, JSON.stringify([...dismissed])); } catch { /* keep in memory */ }
}
function dismiss(key: string) {
  window.clearTimeout(completed.get(key)?.timer);
  completed.delete(key);
  remember(key);
  publish();
}
function complete(key: string, text: string) {
  if (dismissed.has(key)) return;
  remember(key);
  const until = Date.now() + 10_000;
  const timer = window.setTimeout(() => { completed.delete(key); publish(); }, 10_000);
  completed.set(key, { text, until, timer });
  publish();
}
/** All Inbox surfaces and their badges share the same dismissal and receipt. */
export function useDismissedPulls() {
  const state = useSyncExternalStore(subscribe, () => snapshot);
  return { ...state, dismiss, complete };
}

export type PullAction = 'send' | 'review' | 'merge';
export type PullActionResult = { ok: boolean; text: string };

/**
 * The one-click reactions to a tracked pull request: send its failing
 * checks or its review to the conversation that delivered it, or merge it.
 * Each is the person's; the Runner queues a message behind a turn in
 * progress and says so.
 */
export function usePullActions() {
  const { api } = useRunner();
  const [busy, setBusy] = useState('');
  const run = useCallback(async (pull: TrackedPull, action: PullAction): Promise<PullActionResult> => {
    const link: ItemLink = pull.link;
    const ref = { provider: link.item?.ref?.provider || 'github', key: link.item?.ref?.key || '' };
    setBusy(pull.key + ':' + action);
    try {
      if (action === 'merge') {
        let method = 'squash';
        try { method = localStorage.getItem('lazybox:merge-method') || 'squash'; } catch { /* the default */ }
        await api.mergePullRequest({ collectionId: link.collectionId, member: link.member, number: BigInt(pull.number), method, expectedHead: link.item?.pullRequest?.headSha || '', item: ref }, { timeoutMs: 60_000 });
        nudgeGitSummaries(api);
        return { ok: true, text: `Merged #${pull.number}` };
      }
      const request = { collectionId: link.collectionId, ref, sessionId: '' };
      const answer = action === 'send' ? await api.sendPullRequestFailures(request, { timeoutMs: 120_000 }) : await api.importPullRequestReview(request, { timeoutMs: 120_000 });
      const notes = answer.notes.length ? ` · ${answer.notes.join('; ')}` : '';
      return { ok: true, text: `${answer.summary}${answer.queued ? ' (queued until its turn ends)' : ''}${notes}` };
    } catch (error) {
      return { ok: false, text: ConnectError.from(error).rawMessage || String(error) };
    } finally {
      setBusy('');
      nudgeTrackedItems(api);
    }
  }, [api]);
  return { run, busy };
}
