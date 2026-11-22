import { useCallback, useSyncExternalStore } from 'react';
import { ConnectError } from '@lazybox/client-core';
import type { ItemLink } from '@lazybox/protocol';
import { useRunner } from './RunnerContext';
import { nudgeTrackedItems, trackedPollFor } from './tracked-items-poll';
import { nudgeGitSummaries } from './useGitSummaries';
import { pullReactions } from './pull-reaction-state';
import { pullReactionKey, type TrackedPull } from './pull-requests-model';

/** Every tracked pull request of the window's Runner (tracked-items-poll.ts). */
export function useTrackedItems() {
  const { api } = useRunner();
  const poll = trackedPollFor(api);
  return useSyncExternalStore(poll.subscribe, () => poll.snapshot);
}

export function useDismissedPulls() {
  const state = useSyncExternalStore(pullReactions.subscribe, pullReactions.snapshot);
  return { ...state, dismiss: pullReactions.dismiss };
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
  const { states } = useDismissedPulls();
  const run = useCallback(async (pull: TrackedPull, action: PullAction): Promise<PullActionResult> => pullReactions.run(pullReactionKey(pull), async () => {
    const link: ItemLink = pull.link;
    const ref = { provider: link.item?.ref?.provider || 'github', key: link.item?.ref?.key || '' };
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
      nudgeTrackedItems(api);
    }
  }), [api]);
  return { run, states };
}
