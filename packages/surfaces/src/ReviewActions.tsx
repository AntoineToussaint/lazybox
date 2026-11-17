import { useActions } from './actions';
import { showMenuNotice } from './ContextMenu';
import { useWorkConversation } from './PullRequestFlow';
import { useReviewLauncher } from './review-client';
import { useWorkspaceReview } from './useWorkspaceReview';

/**
 * ⌘K "Review this workspace's changes" (] ] ⇧R: the default reviewer), and
 * "Review changes with Codex" / "with Claude": the workspace's changes
 * since it started, reviewed by that agent, from anywhere in the window. The
 * work reviewed is the page's conversation's, or, when the page shows a
 * review conversation, the one it reviews.
 */
export function ReviewActions({ workspaceId, sessionId }: { workspaceId: string; sessionId: string }) {
  const conversation = useWorkConversation(workspaceId, sessionId);
  const review = useWorkspaceReview(workspaceId);
  const asReviewer = conversation && review.review?.runs.find(run => run.reviewerSessionId === conversation.id);
  const author = asReviewer ? { id: asReviewer.sourceSessionId, profileId: '', provider: asReviewer.sourceProvider } : conversation;
  const launcher = useReviewLauncher(workspaceId, author);
  const choice = (provider: string) => launcher.choices.find(c => c.provider === provider && !c.profileId);
  const handler = (provider: string) => ({ run: () => { const c = choice(provider); if (c) void launcher.start({ scope: 'changes' }, c); }, enabled: () => !launcher.unavailable && !launcher.busy && !!choice(provider) });
  useActions({
    // The default reviewer (Settings → Review by another agent): the other agent, by default.
    'review.start': { run: () => { const c = launcher.choices[0]; if (!c) return; showMenuNotice(`Starting ${c.label}’s review…`); void launcher.start({ scope: 'changes' }, c); }, enabled: () => !launcher.unavailable && !launcher.busy && launcher.choices.length > 0 },
    'review.codex': handler('codex'), 'review.claude': handler('claude'),
  });
  return launcher.error ? <div className="review-action-error" role="alert"><span>Could not start the review: {launcher.error}</span><button type="button" aria-label="Dismiss" onClick={() => launcher.setError('')}>×</button></div> : null;
}
