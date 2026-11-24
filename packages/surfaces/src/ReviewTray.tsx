import { openPeek } from './Peek';
import { conversationName } from './conversation-model';
import { useEffect, useMemo, useRef, useState, type KeyboardEvent } from 'react';
import type { ReviewComment, ReviewRun, Session } from '@lazybox/protocol';
import { useRunner } from './RunnerContext';
import { errorText } from './PullRequestFlow';
import { Icon } from './Icons';
import { LiveIndicator } from './LiveIndicator';
import { composeInConversation } from './compose';
import { showConversation } from './review-client';
import { openRelatedConversation } from './conversation-requests';
import { endedByHand, loopActive, loopLine, loopModes, loopNeedsYou } from './review-model';
import { authorLabel, dispositionLabel, freshnessLabel, groupByFile, isFinding, isPending, isProposed, linesLabel, location, matchesFilter, matchesQuery, reviewAuthors, reviewExcerpt, reviewMessage, runSummary, severities, severityLabel, trayFilters, type ReviewQuery, type TrayFilter } from './review-model';
import type { WorkspaceReviewState } from './useWorkspaceReview';
import { useCloseReviewersOnSend } from './review-close';
import './review.css';
import './review-agents.css';

const plural = (n: number, word: string) => `${n} ${word}${n === 1 ? '' : 's'}`;
const excerpt = reviewExcerpt;

/** The workspace's open coding conversations, the current one first; reviewer conversations are not where a review goes. */
function useConversations(workspaceId: string, current: Session | undefined, reviewers: ReadonlySet<string>) {
  const { api } = useRunner();
  const [sessions, setSessions] = useState<Session[]>([]);
  useEffect(() => {
    const control = new AbortController();
    void api.listHistory({ collectionId: workspaceId, limit: 30 }, { signal: control.signal, timeoutMs: 10000 }).then(history => {
      if (!control.signal.aborted) setSessions(history.sessions.filter(s => (!s.purpose || s.purpose === 'coding') && !s.closed));
    }, () => {});
    return () => control.abort();
  }, [api, workspaceId]);
  return useMemo(() => {
    const list = sessions.filter(s => s.id !== current?.id && !reviewers.has(s.id));
    return current && !reviewers.has(current.id) ? [current, ...list] : list;
  }, [sessions, current, reviewers]);
}

/** Why a finding is dismissed, asked inline: Enter dismisses, Esc keeps it. */
function DismissReason({ onDismiss, onCancel }: { onDismiss(reason: string): Promise<unknown>; onCancel(): void }) {
  const [reason, setReason] = useState(''), [busy, setBusy] = useState(false);
  const field = useRef<HTMLInputElement>(null);
  useEffect(() => field.current?.focus(), []);
  const submit = async () => { if (busy) return; setBusy(true); try { await onDismiss(reason); } finally { setBusy(false); } };
  const keys = (event: KeyboardEvent) => {
    if (event.key === 'Enter') { event.preventDefault(); void submit(); }
    if (event.key === 'Escape') { event.preventDefault(); event.stopPropagation(); onCancel(); }
  };
  return <span className="review-dismiss" role="group" aria-label="Dismiss finding">
    <input ref={field} aria-label="Why dismiss it" placeholder="Why? (optional, kept with the finding)" value={reason} maxLength={2000} onChange={event => setReason(event.target.value)} onKeyDown={keys}/>
    <button type="button" className="danger" disabled={busy} onClick={() => void submit()}>Dismiss</button>
    <button type="button" onClick={onCancel}>Keep</button>
  </span>;
}

/** A reviewer's finding's triage: Accept (pending, to send), Dismiss (with a reason), Ask the reviewer. */
export function FindingActions({ comment, review, onAsk, compact = false }: { comment: ReviewComment; review: WorkspaceReviewState; onAsk(comment: ReviewComment): void; compact?: boolean }) {
  const [dismissing, setDismissing] = useState(false), [error, setError] = useState('');
  const act = (run: () => Promise<unknown>) => { setError(''); return run().catch(e => setError(errorText(e))); };
  if (dismissing) return <DismissReason onCancel={() => setDismissing(false)} onDismiss={reason => act(() => review.setDisposition([comment.id], 'dismissed', undefined, reason)).then(() => setDismissing(false))}/>;
  return <span className={`review-finding-actions${compact ? ' is-compact' : ''}`} role="group" aria-label={`Triage “${comment.title}”`}>
    {isProposed(comment) && <button type="button" className="primary" title="Keep it: it goes to the agent with the next review you send" onClick={() => void act(() => review.setDisposition([comment.id], 'open'))}><Icon name="check" size={13}/>Accept</button>}
    {(isProposed(comment) || isPending(comment)) && <button type="button" title="Turn it down; say why for the record" onClick={() => setDismissing(true)}><Icon name="close" size={13}/>Dismiss</button>}
    {comment.authorSessionId && <button type="button" title={`Ask ${comment.authorLabel || 'the reviewer'} about this finding in its conversation`} onClick={() => onAsk(comment)}><Icon name="ask" size={13}/>Ask {comment.authorLabel || 'reviewer'}</button>}
    {error && <span className="review-error" role="alert">{error}</span>}
  </span>;
}

/** The draft a follow-up question to a reviewer starts with. */
export function askReviewerDraft(c: ReviewComment) {
  return `About your finding ${c.id} (${severityLabel(c.severity).toLowerCase() || 'finding'} at ${location(c)}:${c.startLine === c.endLine ? c.startLine : `${c.startLine}–${c.endLine}`}, “${c.title}”): `;
}
export function askReviewer(workspaceId: string, c: ReviewComment) {
  composeInConversation({ sessionId: c.authorSessionId, text: askReviewerDraft(c) });
  showConversation({ collectionId: workspaceId, sessionId: c.authorSessionId });
}

/** The workspace's agent reviews, newest first: who is reviewing what, live. */
const clock = (unix: bigint) => new Date(Number(unix) * 1000).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });

/** One review's loop: where it is, the person's controls, and its timeline. */
function ReviewLoop({ run, workspaceId, review }: { run: ReviewRun; workspaceId: string; review: WorkspaceReviewState }) {
  const [error, setError] = useState('');
  const act = (action: string, loop = '', maxRounds = 0) => { setError(''); void review.updateRun(run.id, action, loop, maxRounds).catch(e => setError(errorText(e))); };
  const line = loopLine(run), active = loopActive(run);
  if (!line && !run.timeline.length) return null;
  return <div className={`review-loop${loopNeedsYou(run) ? ' needs-you' : ''}`}>
    {line && <span className="review-loop-line" role="status">{loopNeedsYou(run) && <Icon name="bell" size={12}/>}{line}</span>}
    {active && <span className="review-loop-controls" role="group" aria-label={`${run.reviewerLabel} review loop`}>
      <select aria-label="Between rounds" value={run.loop} onChange={event => act('loop', event.target.value, run.maxRounds)}>{loopModes.map(m => <option key={m.value} value={m.value}>{m.label}</option>)}</select>
      {run.paused
        ? <button type="button" title="Re-review and send again when the author changes files" onClick={() => act('resume')}><Icon name="play" size={12}/>Resume</button>
        : <button type="button" title="Hold the loop: no re-review or automatic send until you resume" onClick={() => act('pause')}><Icon name="pending" size={12}/>Pause</button>}
      <button type="button" title="End the loop here; findings stay as they are" onClick={() => act('stop')}><Icon name="stop" size={12}/>Stop loop</button>
      <button type="button" title={`End the loop and continue in ${run.sourceLabel || 'the author'}'s conversation yourself`} onClick={() => { act('take_over'); showConversation({ collectionId: workspaceId, sessionId: run.sourceSessionId }); }}>Take over</button>
    </span>}
    {run.timeline.length > 0 && <details className="review-timeline">
      <summary>Timeline · {run.rounds.length === 1 ? '1 round' : `${run.rounds.length} rounds`}</summary>
      <ol>{run.timeline.map((entry, index) => <li key={index} className={`is-${entry.kind}`}><time>{clock(entry.atUnix)}</time><b>{entry.actor}</b><span>{entry.text}</span></li>)}</ol>
    </details>}
    {error && <p className="review-error" role="alert">{error}</p>}
  </div>;
}

/** The workspace's agent reviews, newest first: who is reviewing what, live, and each loop. */
function ReviewRuns({ runs, workspaceId, review, focus, onFocus }: { runs: readonly ReviewRun[]; workspaceId: string; review: WorkspaceReviewState; focus: string; onFocus(run: ReviewRun): void }) {
  if (!runs.length) return null;
  // What needs you first, then what is under way, then the rest; newest first within each.
  const rank = (run: ReviewRun) => loopNeedsYou(run) ? 0 : run.state === 'running' || loopActive(run) ? 1 : 2;
  return <ul className="review-runs" aria-label="Agent reviews">
    {[...runs].sort((a, b) => rank(a) - rank(b)).slice(0, 4).map(run => <li key={run.id} className={`is-${run.state}${focus === run.id ? ' is-focused' : ''}${loopNeedsYou(run) ? ' needs-you' : ''}`} aria-label={`${run.reviewerLabel} review of ${run.scopeLabel}`} data-review={run.id}>
      <div className="review-run-head">
        <span className="review-run-mark" aria-hidden="true">{run.state === 'running' ? <LiveIndicator tone="busy"/> : <Icon name={endedByHand(run) ? 'stop' : run.state === 'failed' || run.blockers || loopNeedsYou(run) ? 'alert' : 'check'} size={13}/>}</span>
        <button type="button" className="review-run-summary" title="Show only this review's findings" onClick={() => onFocus(run)}>{runSummary(run)}</button>
        <button type="button" className="review-icon" aria-label={`Open ${run.reviewerLabel}'s review conversation`} title="Open the review conversation" onClick={() => showConversation({ collectionId: workspaceId, sessionId: run.reviewerSessionId })}><Icon name="chat" size={13}/></button>
      </div>
      {run.verification.length > 0 && <details className="review-verification">
        <summary>Acceptance checks · {run.verification.length}</summary>
        {run.verification.map(check => <section key={check.name} aria-label={`Acceptance check ${check.name}`}>
          <strong>{check.name}</strong><p>{check.expected}</p>
          {check.runs.map(result => <div key={result.round}>
            <span>Round {result.round} · {result.state}</span>
            <p>Base {result.base.slice(0, 10)} · exit {result.baseExit ?? 'pending'}<br/>Candidate {result.candidate.slice(0, 10)} · exit {result.candidateExit ?? 'pending'}</p>
            {result.detail && <p>{result.detail}</p>}
            {[['Base output', result.baseCommandId], ['Candidate output', result.candidateCommandId]].map(([label, id]) => id && <button key={id} type="button" onClick={() => openPeek({kind: 'command', commandId: id, title: `${check.name} · ${label}`})}>{label}</button>)}
          </div>)}
          {check.regression && <p>Regression test proposal — {check.regression}</p>}
        </section>)}
      </details>}
      <ReviewLoop run={run} workspaceId={workspaceId} review={review}/>
    </li>)}
  </ul>;
}

/** Every comment and finding on the workspace's changes, by file; agent
 * findings to triage; and the one action that matters: send the selected comments
 * to the author's conversation as one message. */
export function ReviewTray({ id, workspaceId, review, conversation, focusReview = '', onJump, onSend, onClose }: {
  id?: string; workspaceId: string; review: WorkspaceReviewState; conversation?: Session; focusReview?: string;
  onJump(comment: ReviewComment): void; onSend(sessionId: string): void; onClose(): void;
}) {
  const runs = review.review?.runs || [];
  const reviewers = useMemo(() => new Set(runs.map(run => run.reviewerSessionId)), [runs]);
  const conversations = useConversations(workspaceId, conversation, reviewers);
  const [closeReviewers, setCloseReviewers] = useCloseReviewersOnSend();
  const proposed = review.comments.filter(isProposed).length;
  const [query, setQuery] = useState<ReviewQuery>(() => ({ filter: proposed ? 'proposed' : 'pending', author: '', severity: '' }));
  const [focusRun, setFocusRun] = useState(focusReview);
  // A review asked for (its card's "Open review") shows its findings to triage.
  useEffect(() => {
    if (!focusReview) return;
    setFocusRun(focusReview);
    const run = runs.find(r => r.id === focusReview);
    if (run) setQuery(q => ({ ...q, author: run.reviewerLabel, filter: review.comments.some(c => c.reviewId === run.id && isProposed(c)) ? 'proposed' : 'all' }));
  }, [focusReview, runs.length]);
  // The first findings to arrive bring the tray to them.
  const hadProposed = useRef(proposed > 0);
  useEffect(() => { if (proposed && !hadProposed.current && query.filter === 'pending' && !review.comments.some(isPending)) setQuery(q => ({ ...q, filter: 'proposed' })); hadProposed.current = proposed > 0; }, [proposed]);
  const [excluded, setExcluded] = useState<Set<string>>(new Set());
  const { api } = useRunner();
  const handingOff = useRef(false);
  const [target, setTarget] = useState(''), [busy, setBusy] = useState(false), [error, setError] = useState('');
  const scoped = useMemo(() => focusRun ? review.comments.filter(c => c.reviewId === focusRun) : review.comments, [focusRun, review.comments]);
  const counts = useMemo(() => Object.fromEntries(trayFilters.map(f => [f.value, scoped.filter(c => matchesFilter(c, f.value)).length])) as Record<TrayFilter, number>, [scoped]);
  const authors = useMemo(() => reviewAuthors(review.comments), [review.comments]);
  const hasFindings = review.comments.some(isFinding);
  const shown = groupByFile(scoped.filter(c => matchesQuery(c, query)));
  const selectable = shown.flatMap(group => group.comments).filter(c => isPending(c) || isProposed(c));
  const chosen = selectable.filter(c => !excluded.has(c.id));
  // Findings go back to the conversation whose work was reviewed.
  const author = runs.find(run => chosen.some(c => c.reviewId === run.id))?.sourceSessionId || (conversation && reviewers.has(conversation.id) ? runs.find(run => run.reviewerSessionId === conversation.id)?.sourceSessionId : '');
  const to = conversations.find(s => s.id === target) || conversations.find(s => s.id === author) || conversations[0];
  const toggle = (id: string) => setExcluded(previous => { const next = new Set(previous); if (next.has(id)) next.delete(id); else next.add(id); return next; });
  const send = async () => {
    if (!to || !chosen.length || busy) return;
    setBusy(true); setError('');
    try {
      onSend(await review.send(chosen, to.id, closeReviewers && chosen.some(c => !!c.reviewId)));
    } catch (e) { setError(errorText(e)); } finally { setBusy(false); }
  };
  const authorSession = to;
  const startFixConversation = async () => {
    if (!authorSession || !chosen.length || handingOff.current || busy) return;
    handingOff.current = true;
    setBusy(true);
    setError('');
    try {
      const opened = await api.openSession({ collectionId: workspaceId, profileId: authorSession.profileId, options: { permission: 'ask' } });
      if (!opened.session) throw new Error('Runner did not return a conversation');
      const sessionId = opened.session.id;
      const draft = [
        'Verify these selected findings against the current workspace, then address the issues that still apply.',
        ...runs.filter(run => run.resultArtifactId && chosen.some(finding => finding.reviewId === run.id)).map(run => `Source: findings artifact ${run.resultArtifactId} v1.`),
        `Finding IDs: ${chosen.map(finding => finding.id).join(', ')}.`,
        reviewMessage(chosen),
      ].join('\n\n');
      composeInConversation({ sessionId, text: draft, api, reviews: [{text: reviewMessage(chosen), ids: chosen.map(c => c.id)}] });
      const openedBeside = openRelatedConversation({ collectionId: workspaceId, sessionId: authorSession.id, commandId: '' },
        { collectionId: workspaceId, sessionId, commandId: '' }, { background: false });
      if (!openedBeside) showConversation({ collectionId: workspaceId, sessionId });
    } catch (e) {
      setError(errorText(e));
    } finally {
      handingOff.current = false;
      setBusy(false);
    }
  };
  const summary = counts.all ? [counts.proposed && `${counts.proposed} to triage`, counts.pending && `${counts.pending} pending`, counts.sent && `${counts.sent} sent`, counts.settled && `${counts.settled} done`].filter(Boolean).join(' · ') : runs.some(r => r.state === 'running') ? 'Waiting for findings…' : 'No comments yet';
  const empty = () => {
    if (query.filter === 'proposed') return runs.some(r => r.state === 'running') ? 'Findings appear here as the reviewer files them.' : 'Nothing to triage.';
    if (query.filter === 'pending') return counts.sent ? `Nothing pending. ${plural(counts.sent, 'comment')} sent to the agent, under Sent.` : counts.proposed ? `Nothing pending. Accept findings under To triage to send them.` : 'Nothing pending. New comments show up here.';
    return 'Nothing here.';
  };
  // Escape closes the tray; a field's own Escape comes first.
  const closeOnEscape = (event: KeyboardEvent<HTMLElement>) => { if (event.key === 'Escape' && !event.defaultPrevented && !(event.target as Element).closest('select,textarea')) { event.preventDefault(); onClose(); } };
  // Findings scroll between the review controls and the sending controls.
  return <aside className="review-tray" id={id} aria-label="Review" onKeyDown={closeOnEscape}>
    <div className="review-tray-top">
    <header>
      <h3><Icon name="chat" size={14}/>Review</h3>
      <small>{summary}</small>
      <button className="review-icon" aria-label="Close review" title="Close review" onClick={onClose}><Icon name="close" size={14}/></button>
    </header>
    <ReviewRuns runs={runs} workspaceId={workspaceId} review={review} focus={focusRun} onFocus={run => { setFocusRun(run.id); setQuery(q => ({ ...q, author: run.reviewerLabel, filter: 'all' })); }}/>
    </div>
    <div className="review-filters" role="group" aria-label="Show">
      {trayFilters.filter(f => f.value !== 'proposed' || counts.proposed || hasFindings).map(f => <button key={f.value} aria-pressed={query.filter === f.value} onClick={() => setQuery(q => ({ ...q, filter: f.value }))}>{f.label}<span>{counts[f.value]}</span></button>)}
    </div>
    <div className="review-tray-scroll">
      {hasFindings && <div className="review-facets">
        {focusRun && <button type="button" onClick={() => { setFocusRun(''); setQuery(q => ({ ...q, author: '' })); }}>Show all reviews</button>}
        <label>By<select aria-label="Author" value={query.author} onChange={event => setQuery(q => ({ ...q, author: event.target.value }))}>{authors.map(a => <option key={a.value} value={a.value}>{a.label}</option>)}</select></label>
        <label>Severity<select aria-label="Severity" value={query.severity} onChange={event => setQuery(q => ({ ...q, severity: event.target.value }))}><option value="">Any</option>{severities.map(s => <option key={s.value} value={s.value}>{s.label}</option>)}</select></label>
      </div>}
      <div className="review-tray-list">
        {!counts.all && !runs.length && <p className="review-empty">Hover a line in the diff and click <b>+</b> to comment on it. Drag or Shift-click to comment on several lines. Or have another agent review: <b>Review with…</b> above. Comments collect here until you send them to the agent.</p>}
        {(counts.all > 0 || runs.length > 0) && !shown.length && <p className="review-empty">{empty()}</p>}
        {shown.map(group => <section key={group.file}>
          <h4 title={group.file}>{group.file}</h4>
          <ul>{group.comments.map(c => {
            const fresh = freshnessLabel(c), finding = isFinding(c);
            return <li key={c.id} className={`is-${c.disposition || 'open'}${finding ? ' is-finding' : ''}`}>
              {(isPending(c) || isProposed(c)) && <input type="checkbox" checked={!excluded.has(c.id)} onChange={() => toggle(c.id)} aria-label={`Include the comment on ${group.file} ${linesLabel(c.side, c.startLine, c.endLine).toLowerCase()}`}/>}
              <div className="review-tray-item">
                <button className="review-tray-jump" onClick={() => onJump(c)} title="Show in the diff">
                  <span className="review-tray-where">
                    {finding && <span className={`review-severity sev-${c.severity}`}>{severityLabel(c.severity)}</span>}
                    {linesLabel(c.side, c.startLine, c.endLine)}
                    {finding && <span className="review-chip is-author">{authorLabel(c)}</span>}
                    {fresh && <span className={`review-chip fresh-${c.freshness}`}>{fresh}</span>}
                    {!isPending(c) && <span className={`review-chip is-${c.disposition}`}>{dispositionLabel(c)}</span>}
                  </span>
                  {finding && <span className="review-tray-title">{c.title}</span>}
                  <span className="review-tray-body">{excerpt(c.body)}</span>
                  {c.disposition === 'dismissed' && c.note && <span className="review-muted">Dismissed: {c.note}</span>}
                </button>
                {finding && (isProposed(c) || isPending(c)) && <FindingActions comment={c} review={review} compact onAsk={comment => askReviewer(workspaceId, comment)}/>}
              </div>
            </li>;
          })}</ul>
        </section>)}
      </div>
    </div>
    <footer>
      {error && <p className="review-error" role="alert">{error}</p>}
      {!!selectable.length && <button type="button" onClick={() => setExcluded(previous => {
        const next = new Set(previous);
        for (const c of selectable) { if (chosen.length === selectable.length) next.add(c.id); else next.delete(c.id); }
        return next;
      })}>{chosen.length === selectable.length ? 'Clear selection' : 'Select all'}</button>}
      {conversations.length > 1 && <label className="review-target">Send to<select value={to?.id || ''} onChange={event => setTarget(event.target.value)} aria-label="Conversation">
        {conversations.map(s => <option key={s.id} value={s.id}>{conversationName(s, 'Untitled conversation')}{s.id === author ? ' (author)' : s.id === conversation?.id ? ' (current)' : ''}</option>)}
      </select></label>}
      {authorSession && <button type="button" className="review-send" disabled={!chosen.length || busy} onClick={() => void startFixConversation()}>
        <Icon name="chat" size={14}/>Fix in new conversation
      </button>}
      <button className="primary review-send" disabled={!chosen.length || !to || busy} onClick={() => void send()}>
        <Icon name="send" size={14}/>{chosen.some(isFinding) ? `Fix ${plural(chosen.length, 'finding')} in ${to?.id === conversation?.id ? 'this conversation' : (to ? conversationName(to, 'conversation') : 'conversation')}` : chosen.length ? `Send ${plural(chosen.length, 'comment')} to the agent` : 'Send review to the agent'}
      </button>
      <small className="review-muted">{!to ? 'No conversation in this workspace yet. Start one to send it comments.' : `Sends the selected comments directly to “${conversationName(to, 'the conversation')}” as one message.`}</small>
      {runs.length > 0 && <label className="review-auto check-row"><input type="checkbox" checked={closeReviewers} onChange={event => setCloseReviewers(event.target.checked)}/><span>Close the reviewer after sending</span></label>}
      <label className="review-auto check-row"><input type="checkbox" checked={review.autoResolve} onChange={event => { const enabled = event.target.checked; void review.setAutoResolve(enabled).catch(e => setError(errorText(e))); }}/><span>Resolve sent comments when a commit changes their lines</span></label>
    </footer>
  </aside>;
}
