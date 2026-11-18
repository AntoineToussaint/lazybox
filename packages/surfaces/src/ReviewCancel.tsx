import { useState } from 'react';
import type { ReviewRun } from '@lazybox/protocol';
import { ConfirmDialog } from './ConfirmDialog';
import { showMenuNotice } from './ContextMenu';
import { errorText } from './PullRequestFlow';
import { findingsKept } from './review-model';
import { useWorkspaceReview } from './useWorkspaceReview';

/**
 * Cancelling a review (GITHUB.md, "Cancelling a review"), one way from
 * every place that offers it: the review's card (Cancel review, Esc), the
 * reviewer conversation's ⋯ menu and ⌘K. The Runner interrupts the
 * reviewer's turn, starts no further round and ends its session; its
 * conversation and findings are kept. It asks first only when the reviewer
 * has already filed findings: with none, there is nothing to weigh.
 */
export function useCancelReview(workspaceId: string) {
  const review = useWorkspaceReview(workspaceId);
  const [asking, setAsking] = useState<ReviewRun>();
  const filed = (run: ReviewRun) => review.comments.filter(c => c.reviewId === run.id).length;
  const cancel = async (run: ReviewRun) => {
    const tray = await review.cancelRun(run.id);
    const after = tray.runs.find(r => r.id === run.id);
    showMenuNotice(`Review cancelled · ${after ? findingsKept(after) : 'its findings kept'}`);
  };
  /** Cancel run: at once, or after asking when it has findings. */
  const request = (run: ReviewRun) => {
    if (filed(run) > 0) { setAsking(run); return; }
    void cancel(run).catch(e => showMenuNotice(`Could not cancel the review: ${errorText(e)}`));
  };
  const n = asking ? filed(asking) : 0;
  const dialog = asking && <ConfirmDialog title="Cancel this review?" label="Cancel review" closeLabel="Keep reviewing" subject={asking.reviewerTitle || `${asking.reviewerLabel} review`}
    subjectMeta={`${n} finding${n === 1 ? '' : 's'} filed`} confirm="Cancel review" cancelLabel="Keep reviewing" working="Cancelling…"
    kept={`The ${n === 1 ? 'finding' : `${n} findings`} ${asking.reviewerLabel} filed stay${n === 1 ? 's' : ''} in the Review tray, and its conversation stays.`}
    onClose={() => setAsking(undefined)} onConfirm={async () => {
      try { await cancel(asking); } catch (e) { throw new Error(`Could not cancel the review: ${errorText(e)}`); }
      setAsking(undefined);
    }}>
    {asking.reviewerLabel} stops now: its turn is interrupted and its session ends. No further round starts.
  </ConfirmDialog>;
  return { request, dialog: dialog || null };
}
