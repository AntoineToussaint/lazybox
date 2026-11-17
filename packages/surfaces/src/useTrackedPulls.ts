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
/** The Inbox rows this window dismissed (a new failure or review brings a row back). */
export function useDismissedPulls() {
  const [dismissed, setDismissed] = useState<ReadonlySet<string>>(() => { try { return new Set(parseDismissed(localStorage.getItem(DISMISSED))); } catch { return new Set(); } });
  const dismiss = useCallback((key: string) => setDismissed(previous => {
    const next = new Set(previous); next.add(key);
    try { localStorage.setItem(DISMISSED, JSON.stringify([...next].slice(-500))); } catch { /* per-viewer convenience only */ }
    return next;
  }), []);
  return { dismissed, dismiss };
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
