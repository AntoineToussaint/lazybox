import { useEffect, useRef } from 'react';
import type { Session } from '@lazybox/protocol';
import { ActionMenu } from './ActionMenu';
import { ActionMenuItems } from './ActionMenuItems';
import { runAction } from './actions';
import { requestReviewTray } from './ChangesPanel';
import { Icon } from './Icons';
import { LiveIndicator } from './LiveIndicator';
import type { MenuItem } from './context-menu-model';
import { showConversation, useReviewLauncher, type ReviewLauncher } from './review-client';
import { reviewButtonTitle, type ReviewerChoice } from './review-model';
import { showMenuNotice } from './ContextMenu';
import { useWorkspaceReview } from './useWorkspaceReview';
import './review-agents.css';

/**
 * Review from the chat (GITHUB.md, "Agent review"): the conversation header's
 * Review button, its ⋯ item and its chevron start the same review that
 * Changes → Review starts — the workspace's changes since it started,
 * committed, uncommitted and untracked — with the default reviewer (the
 * other agent, by default) or one picked for this run. The review shows in
 * the conversation (its card) and as a background tab beside it.
 */
export function useChatReview({ session, sessionID, onError }: { session?: Session; sessionID: string; onError(message: string): void }) {
  const workspaceId = session?.collectionId || '';
  const runs = useWorkspaceReview(workspaceId).review?.runs;
  // A review conversation is never itself reviewed (its `/review` reviews the
  // work it reviews); an assistant has no work to review.
  const asReviewer = runs?.find(run => run.reviewerSessionId === sessionID);
  const isReviewer = !!asReviewer;
  const coding = !!session && (!session.purpose || session.purpose === 'coding');
  // One launcher per chat: the header's button, its ⋯ item and `/review`
  // (useSlashCommands.tsx) share it, and so its busy state: never two reviews at once.
  const launcher = useReviewLauncher(workspaceId, asReviewer ? { id: asReviewer.sourceSessionId, profileId: '', provider: asReviewer.sourceProvider } : coding ? session : undefined);
  const latest = useRef(onError); latest.current = onError;
  useEffect(() => { if (launcher.error) { latest.current(`Could not start the review: ${launcher.error}`); launcher.setError(''); } }, [launcher.error]);
  const shown = !!workspaceId && coding && !isReviewer && launcher.choices.length > 0;
  // This conversation's review under way, newest first: the button follows it.
  const running = runs?.find(run => run.sourceSessionId === sessionID && run.state === 'running');
  const start = (choice?: ReviewerChoice, focus = '') => {
    if (launcher.unavailable) { showMenuNotice(launcher.unavailable); return; }
    const reviewer = choice || launcher.choices[0];
    if (!reviewer) return;
    showMenuNotice(`Starting ${reviewer.label}’s review…`);
    void launcher.start({ scope: 'changes' }, reviewer, focus);
  };
  /** The ⋯ menu's item (the button folds into it in a slim panel). */
  const menuItems: MenuItem[] = shown ? [{ label: `Review with ${launcher.choices[0].label}`, icon: 'review', disabled: !!launcher.busy && 'A review is starting', run: () => start() }] : [];
  return { launcher, shown, running, start, menuItems, workspaceId };
}
export type ChatReview = ReturnType<typeof useChatReview>;

/**
 * The header's Review button and its chevron: one click reviews with the
 * default reviewer; the chevron picks another for this run, or opens the
 * tray. While this conversation's review runs, the button says so and shows
 * the reviewer's tab.
 */
export function ReviewChatButton({ review, keys }: { review: ChatReview; keys: string }) {
  const { launcher, shown, running, start, workspaceId } = review;
  if (!shown) return null;
  const [first] = launcher.choices;
  const busy = !!launcher.busy;
  const items: MenuItem[] = [
    ...launcher.choices.map(choice => ({ label: `Review with ${choice.label}`, icon: 'review', note: choice.isDefault ? 'Default' : undefined, disabled: busy && 'A review is starting', run: () => start(choice) })),
    'separator',
    { label: 'Open the review tray', icon: 'eye', run: () => { requestReviewTray(workspaceId); if (!runAction('changes.show')) showMenuNotice('Open the Changes panel to see the review tray'); } },
  ];
  return <span className="chat-review review-with" role="group" aria-label="Review this conversation's work">
    {running
      ? <button type="button" className="chat-review-button" title={`${running.reviewerLabel} is reviewing ${running.scopeLabel.toLowerCase()}: show its conversation`} onClick={() => showConversation({ collectionId: workspaceId, sessionId: running.reviewerSessionId })}>
        <LiveIndicator tone="busy"/><span>Reviewing</span>
      </button>
      : <button type="button" className="chat-review-button" disabled={busy} title={reviewButtonTitle(first, keys)} aria-label={`Review with ${first.label}`} onClick={() => start()}>
        <Icon name="review" size={14}/><span>{busy ? 'Starting…' : 'Review'}</span>
      </button>}
    <ActionMenu className="review-with-more chat-review-more" label="Choose the reviewer" title="Choose the reviewer for this run" summary={<Icon name="chevron" size={12}/>}>
      <ActionMenuItems icons items={items}/>
    </ActionMenu>
  </span>;
}
