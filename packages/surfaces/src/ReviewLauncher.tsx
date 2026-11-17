import { useMemo } from 'react';
import type { ReviewRun } from '@lazybox/protocol';
import { ActionMenu } from './ActionMenu';
import { ActionMenuItems } from './ActionMenuItems';
import { Icon } from './Icons';
import { LiveIndicator } from './LiveIndicator';
import type { MenuItem } from './context-menu-model';
import { showConversation, showReview, type ReviewLauncher, type ReviewScope } from './review-client';
import { countsSummary, isPending, isProposed, loopLine, loopNeedsYou, reviewMessage, runSummary } from './review-model';
import { composeInConversation } from './compose';
import { errorText } from './PullRequestFlow';
import { showMenuNotice } from './ContextMenu';
import { useWorkspaceReview } from './useWorkspaceReview';
import type { ReviewNote } from './transcript-model';
import './review.css';
import './review-agents.css';

/** "Review … with Codex", then "… with Claude", as menu items: a turn's ⋯ menu, the file list's context menu. */
export function reviewMenuItems(launcher: ReviewLauncher, scope: ReviewScope, what: string): MenuItem[] {
  if (launcher.unavailable || !launcher.choices.length) return [];
  // The default reviewer, then each other agent; a specific subscription is
  // the Changes page's ▾ menu's, not every menu's.
  const [first, ...rest] = launcher.choices;
  return [first, ...rest.filter(choice => !choice.profileId && choice.provider !== first.provider)].map(choice =>
    ({ label: `Review ${what} with ${choice.label}`, icon: 'review', disabled: !!launcher.busy && 'A review is starting', run: () => void launcher.start(scope, choice) }));
}

/**
 * "Review with Codex ▾": one click reviews with the default reviewer (the
 * other provider than the author's, or the setting's subscription); the ▾
 * lists every reviewer that is signed in.
 */
export function ReviewWithButton({ launcher, scope, label = 'Review with', className = '' }: { launcher: ReviewLauncher; scope: ReviewScope; label?: string; className?: string }) {
  const [first, ...rest] = launcher.choices;
  if (!first) return null;
  const busy = !!launcher.busy;
  const items: MenuItem[] = launcher.choices.map((choice, index) => ({ label: choice.label, icon: 'review', note: index === 0 ? 'Default' : undefined, run: () => void launcher.start(scope, choice) }));
  return <span className={`review-with ${className}`} role="group" aria-label="Review with another agent">
    <button type="button" disabled={busy || !!launcher.unavailable} title={launcher.unavailable || `${first.label} reviews it in a new read-only conversation; its findings land in the Review tray`} onClick={() => void launcher.start(scope, first)}>
      <Icon name="review" size={14}/>{busy ? `Starting ${launcher.choices.find(c => c.key === launcher.busy)?.label || 'the review'}…` : `${label} ${first.label}`}
    </button>
    {rest.length > 0 && !launcher.unavailable && <ActionMenu className="review-with-more" label="Choose the reviewer" summary={<Icon name="chevron" size={12}/>}>
      <ActionMenuItems icons items={items}/>
    </ActionMenu>}
  </span>;
}

/** The reviews of a workspace's runs by id, from its review (polled). */
export function useReviewRun(workspaceId: string, reviewId: string): ReviewRun | undefined {
  const review = useWorkspaceReview(workspaceId);
  return useMemo(() => review.review?.runs.find(run => run.id === reviewId), [review.review, reviewId]);
}

export type ReviewCardData = ReviewNote;

/** One line of the two agents talking: "Codex asked Claude: …", "Claude answered Codex: …". */
export function ReviewExchange({ text, id }: { text: string; id: string }) {
  const [, who = '', said = text] = /^(.+? (?:asked|answered) [^:]+): ([\s\S]*)$/.exec(text) || [];
  return <div className="review-exchange" role="note" data-message-id={id}>
    <Icon name="ask" size={12}/>
    {who ? <><b>{who}:</b><span title={said}>{said}</span></> : <span>{text}</span>}
  </div>;
}

/** A review loop that stopped to ask the person, with the way to its findings. */
export function ReviewAlert({ text, id, note }: { text: string; id: string; note?: ReviewNote }) {
  return <div className="review-alert" role="note" data-message-id={id}>
    <Icon name="bell" size={13}/><span><b>Review needs you.</b> {text}</span>
    {note?.workspace && <button type="button" onClick={() => showReview({ collectionId: note.workspace, reviewId: note.review })}>Open review</button>}
  </div>;
}

/**
 * A review in the reviewed conversation: "Codex is reviewing turn 3", live
 * while it runs, then "Codex reviewed turn 3 · 2 blockers, 3 minor", with
 * the review (its findings in the tray) and the reviewer's conversation one
 * click away.
 */
export function ReviewCard({ data }: { data: ReviewCardData }) {
  const review = useWorkspaceReview(data.workspace);
  const live = useMemo(() => review.review?.runs.find(run => run.id === data.review), [review.review, data.review]);
  const run = live || { state: data.finished ? 'done' : 'running', reviewerLabel: data.reviewerLabel, scopeLabel: data.scopeLabel, blockers: data.blockers || 0, majors: data.majors || 0, minors: data.minors || 0, nits: data.nits || 0, detail: '', loop: '', phase: '', round: 1, maxRounds: 1, paused: false, stopReason: '', sourceLabel: '', sourceSessionId: '', fallback: '', focus: '' };
  const loop = loopLine(run), needsYou = loopNeedsYou(run);
  const running = run.state === 'running', failed = run.state === 'failed';
  const findings = run.blockers + run.majors + run.minors + run.nits;
  // Findings still to hand to the author (to triage or accepted, not sent): one click puts them
  // in its message box, exactly as filed, as the tray's Send does. Nothing is sent for the person.
  const unsent = review.comments.filter(c => c.reviewId === data.review && (isProposed(c) || isPending(c)));
  const author = run.sourceLabel || 'the author';
  const send = async () => {
    if (!run.sourceSessionId || !unsent.length) return;
    composeInConversation({ sessionId: run.sourceSessionId, text: reviewMessage(unsent) });
    try { await review.setDisposition(unsent.map(c => c.id), 'sent', { sessionId: run.sourceSessionId, sessionTitle: '' }); }
    catch (e) { showMenuNotice(`Could not mark the findings sent: ${errorText(e)}`); }
  };
  const canSend = !running && !failed && unsent.length > 0 && !!run.sourceSessionId;
  return <div className={`review-card is-${run.state}${run.blockers ? ' has-blockers' : ''}${needsYou ? ' needs-you' : ''}`} role="group" aria-label={`${run.reviewerLabel} review`} data-review={data.review}>
    <span className="review-card-icon" aria-hidden="true">{running ? <LiveIndicator tone="busy"/> : <Icon name={failed ? 'alert' : run.blockers ? 'alert' : 'check'} size={14}/>}</span>
    <span className="review-card-text" role="status" aria-live="polite">{runSummary(run)}{loop && <small className="review-card-loop">{loop}</small>}
      {(run.fallback || run.focus) && <small className="review-card-note">{[run.fallback, run.focus && `Focus: ${run.focus}`].filter(Boolean).join(' · ')}</small>}</span>
    {!running && !failed && findings > 0 && <span className="review-card-counts" aria-hidden="true">
      {run.blockers > 0 && <span className="review-severity sev-blocker">{run.blockers}</span>}
      {run.majors > 0 && <span className="review-severity sev-major">{run.majors}</span>}
      {run.minors > 0 && <span className="review-severity sev-minor">{run.minors}</span>}
      {run.nits > 0 && <span className="review-severity sev-nit">{run.nits}</span>}
    </span>}
    <span className="review-card-actions">
      {canSend && <button type="button" className="primary" title={`Put ${unsent.length === 1 ? 'the finding' : `the ${unsent.length} findings`} in ${author}'s message box, exactly as filed; you send it`} onClick={() => void send()}>Send {unsent.length} finding{unsent.length === 1 ? '' : 's'} to {author}</button>}
      <button type="button" className={!running && findings && !canSend ? 'primary' : ''} title={findings ? `Triage ${countsSummary(run)} in the Review tray` : 'Open the Review tray'} onClick={() => showReview({ collectionId: data.workspace, reviewId: data.review })}>Open review</button>
      <button type="button" title={`Open ${run.reviewerLabel}'s review conversation: ask it about a finding`} onClick={() => showConversation({ collectionId: data.workspace, sessionId: data.reviewerSession })}>Open conversation</button>
    </span>
  </div>;
}
