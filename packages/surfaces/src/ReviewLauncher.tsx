import { closeReviewersOnSend } from './review-close';
import { useEffect, useMemo, useState, type KeyboardEvent as ReactKeyboardEvent } from 'react';
import type { ReviewRun } from '@lazybox/protocol';
import { ActionMenu } from './ActionMenu';
import { ActionMenuItems } from './ActionMenuItems';
import { Icon } from './Icons';
import { LiveIndicator } from './LiveIndicator';
import type { MenuItem } from './context-menu-model';
import { showConversation, showReview, useReviewLauncher, type ReviewLauncher, type ReviewScope } from './review-client';
import { countsSummary, endedByHand, isPending, isProposed, loopLine, loopNeedsYou, reviewAgain, reviewInProgress, runSummary } from './review-model';
import { openArtifact } from './artifact-events';
import { useCancelReview } from './ReviewCancel';
import { humanError } from './human-error';
import { showMenuNotice } from './ContextMenu';
import { useWorkspaceReview } from './useWorkspaceReview';
import type { ReviewNote } from './transcript-model';
import './review.css';
import './review-agents.css';

/** "Review … with Codex", then "… with Claude", as menu items: a turn's ⋯ menu, the file list's context menu. */
export function reviewMenuItems(launcher: ReviewLauncher, scope: ReviewScope, what: string): MenuItem[] {
  const active = launcher.activeFor(scope);
  if (active) return [{label: 'View review in progress', icon: 'review', run: () => showReview({collectionId: active.collectionId, reviewId: active.id})}];
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
  const active = launcher.activeFor(scope);
  if (active) return <button type="button" onClick={() => showReview({collectionId: active.collectionId, reviewId: active.id})}>View review in progress</button>;
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
  const [sending, setSending] = useState(false);
  const review = useWorkspaceReview(data.workspace);
  const live = useMemo(() => review.review?.runs.find(run => run.id === data.review), [review.review, data.review]);
  const run = live || { state: data.state || (data.finished ? 'done' : 'running'), reviewerLabel: data.reviewerLabel, scopeLabel: data.scopeLabel, blockers: data.blockers || 0, majors: data.majors || 0, minors: data.minors || 0, nits: data.nits || 0, detail: '', loop: '', phase: '', round: 1, maxRounds: 1, paused: false, stopReason: '', sourceLabel: '', sourceSessionId: '', fallback: '', focus: '' };
  const loop = loopLine(run), needsYou = loopNeedsYou(run);
  const running = run.state === 'running', failed = run.state === 'failed', ended = endedByHand(run);
  // Under way, it can be cancelled (Cancel review, or Esc on the card); once cancelled or stopped, started again.
  const canCancel = !!live && reviewInProgress(live);
  const cancelling = useCancelReview(data.workspace);
  const again = live && (ended || (!!live.templateId && !running)) ? reviewAgain(live) : undefined;
  const launcher = useReviewLauncher(data.workspace, again && live ? { id: live.sourceSessionId, profileId: '', provider: live.sourceProvider } : undefined);
  useEffect(() => { if (launcher.error) { showMenuNotice(`Could not start the review: ${launcher.error}`); launcher.setError(''); } }, [launcher.error]);
  const onKeyDown = (event: ReactKeyboardEvent) => {
    // Only from the card itself or its buttons, never a dialog it opened.
    if (event.key !== 'Escape' || !canCancel || !live || event.defaultPrevented || !event.currentTarget.contains(event.target as Node)) return;
    event.preventDefault(); event.stopPropagation();
    cancelling.request(live);
  };
  const findings = run.blockers + run.majors + run.minors + run.nits;
  const unsent = review.comments.filter(c => c.reviewId === data.review && (isProposed(c) || isPending(c)));
  const author = run.sourceLabel || 'the author';
  const send = async () => {
    if (sending || !run.sourceSessionId || !unsent.length) return;
    setSending(true);
    try { await review.send(unsent, run.sourceSessionId, closeReviewersOnSend()); }
    catch (error) { showMenuNotice(humanError(error)); }
    finally { setSending(false); }
  };
  const canSend = !running && !failed && unsent.length > 0 && !!run.sourceSessionId;
  const stoppedAt = live?.stoppedAtUnix ? new Date(Number(live.stoppedAtUnix) * 1000).toLocaleString([], { hour: '2-digit', minute: '2-digit', month: 'short', day: 'numeric' }) : '';
  // Focusable while it can be cancelled, so Esc on it cancels it.
  return <><div className={`review-card is-${run.state}${run.blockers ? ' has-blockers' : ''}${needsYou ? ' needs-you' : ''}`} role="group" aria-label={`${run.reviewerLabel} review`} data-review={data.review}
    tabIndex={canCancel ? 0 : undefined} aria-keyshortcuts={canCancel ? 'Escape' : undefined} onKeyDown={onKeyDown}
    title={ended && live?.stoppedBy ? `${live.stoppedBy} ${run.state === 'cancelled' ? 'cancelled' : 'stopped'} it${stoppedAt ? ` · ${stoppedAt}` : ''}` : undefined}>
    <span className="review-card-icon" aria-hidden="true">{running ? <LiveIndicator tone="busy"/> : <Icon name={ended ? 'stop' : failed ? 'alert' : run.blockers ? 'alert' : 'check'} size={14}/>}</span>
    <span className="review-card-text" role="status" aria-live="polite">{runSummary(run)}{loop && <small className="review-card-loop">{loop}</small>}
      {(run.fallback || run.focus) && <small className="review-card-note">{[run.fallback, run.focus && `Focus: ${run.focus}`].filter(Boolean).join(' · ')}</small>}</span>
    {!running && !failed && findings > 0 && <span className="review-card-counts" aria-hidden="true">
      {run.blockers > 0 && <span className="review-severity sev-blocker">{run.blockers}</span>}
      {run.majors > 0 && <span className="review-severity sev-major">{run.majors}</span>}
      {run.minors > 0 && <span className="review-severity sev-minor">{run.minors}</span>}
      {run.nits > 0 && <span className="review-severity sev-nit">{run.nits}</span>}
    </span>}
    <span className="review-card-actions">
      {live?.resultArtifactId && <button type="button" onClick={() => openArtifact({ collectionId: data.workspace, artifactId: live.resultArtifactId, version: 1 })}>Open findings artifact</button>}
      {again && <button type="button" className="primary" disabled={!!launcher.busy || !!launcher.unavailable} title={launcher.unavailable || `${run.reviewerLabel} reviews ${run.scopeLabel.toLowerCase()} again, in a new read-only conversation`}
        onClick={() => void launcher.start(again.scope, again.choice, live?.focus || '', live?.templateId)}><Icon name="review" size={12}/>{launcher.busy ? 'Starting…' : 'Review again'}</button>}
      {canSend && <button type="button" className="primary" disabled={sending} title={`Send the findings directly to ${author}`} onClick={() => void send()}>Send {unsent.length} finding{unsent.length === 1 ? '' : 's'} to {author}</button>}
      <button type="button" className={!running && findings && !canSend && !again ? 'primary' : ''} title={findings ? `Triage ${countsSummary(run)} in the Review tray` : 'Open the Review tray'} onClick={() => showReview({ collectionId: data.workspace, reviewId: data.review })}>Open review</button>
      <button type="button" title={`Open ${run.reviewerLabel}'s review conversation: ask it about a finding`} onClick={() => showConversation({ collectionId: data.workspace, sessionId: data.reviewerSession })}>Open conversation</button>
      {canCancel && live && <button type="button" className="review-card-cancel" aria-keyshortcuts="Escape" title={`Cancel review (Esc): ${run.reviewerLabel} stops now and no further round starts; its findings are kept`}
        onClick={() => cancelling.request(live)}><Icon name="stop" size={12}/>Cancel review</button>}
    </span>
  </div>{cancelling.dialog}</>;
}
