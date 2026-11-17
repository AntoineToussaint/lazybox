import { useEffect, useRef, useState, type KeyboardEvent } from 'react';
import type { ReviewComment } from '@lazybox/protocol';
import { RichContent } from './RichContent';
import { errorText } from './PullRequestFlow';
import { authorLabel, diffLabel, dispositionLabel, freshnessLabel, isFinding, isPending, isProposed, linesLabel, severityLabel, short } from './review-model';
import { askReviewer, FindingActions } from './ReviewTray';
import type { WorkspaceReviewState } from './useWorkspaceReview';
import { isMac } from './workbench';

const submitKey = isMac ? '⌘↩' : 'Ctrl+Enter';

/** Writing a comment: Markdown, ⌘↩ to save, Esc to cancel. */
export function ReviewComposer({ label, initial = '', submitLabel, onSubmit, onCancel, onDraft }: { label: string; initial?: string; submitLabel: string; onSubmit(text: string): Promise<unknown>; onCancel(): void; onDraft?(text: string): void }) {
  const [text, setText] = useState(initial), [busy, setBusy] = useState(false), [error, setError] = useState('');
  const field = useRef<HTMLTextAreaElement>(null);
  useEffect(() => { const el = field.current; if (el) { el.focus(); el.setSelectionRange(el.value.length, el.value.length); } }, []);
  const submit = async () => {
    if (!text.trim() || busy) return;
    setBusy(true); setError('');
    try { await onSubmit(text); } catch (e) { setError(errorText(e)); setBusy(false); }
  };
  const keys = (event: KeyboardEvent) => {
    if (event.key === 'Enter' && (event.metaKey || event.ctrlKey)) { event.preventDefault(); void submit(); }
    if (event.key === 'Escape') { event.preventDefault(); event.stopPropagation(); onCancel(); }
  };
  return <form className="review-composer" aria-label={label} onSubmit={event => { event.preventDefault(); void submit(); }}>
    <textarea ref={field} aria-label="Comment" value={text} rows={Math.min(12, Math.max(3, text.split('\n').length + 1))} placeholder="What should the agent change here? Markdown works." onChange={event => { setText(event.target.value); onDraft?.(event.target.value); }} onKeyDown={keys}/>
    {error && <p className="review-error" role="alert">{error}</p>}
    <footer><small>{submitKey} to save · Esc to cancel</small><button type="button" onClick={onCancel}>Cancel</button><button type="submit" className="primary" disabled={!text.trim() || busy}>{busy ? 'Saving…' : submitLabel}</button></footer>
  </form>;
}

/** The lines a comment quotes, numbered as they were. */
export function QuotedLines({ comment }: { comment: ReviewComment }) {
  return <pre className="review-quote" aria-label={`Quoted ${comment.side === 'old' ? 'removed ' : ''}lines`}>{comment.snippet.map((line, index) =>
    <div key={index}><span className="review-quote-number">{comment.startLine + index}</span><code>{comment.side === 'old' ? '-' : ' '}{line || ' '}</code></div>)}</pre>;
}

/** One comment as a thread card in the diff: where and on what it was made,
 * whether its lines are still there, its text, and what became of it. */
export function ReviewThread({ workspaceId = '', comment, review, quote = false, focused = false, focusNonce = 0 }: { workspaceId?: string; comment: ReviewComment; review: WorkspaceReviewState; quote?: boolean; focused?: boolean; focusNonce?: number }) {
  const [editing, setEditing] = useState(false), [confirmDelete, setConfirmDelete] = useState(false), [error, setError] = useState('');
  const ref = useRef<HTMLElement>(null);
  useEffect(() => { if (focused) ref.current?.scrollIntoView({ block: 'center', behavior: 'smooth' }); }, [focused, focusNonce]);
  const act = (run: () => Promise<unknown>) => { setError(''); void run().catch(e => setError(errorText(e))); };
  const fresh = freshnessLabel(comment), finding = isFinding(comment);
  const label = finding ? `${severityLabel(comment.severity)} finding on ${linesLabel(comment.side, comment.startLine, comment.endLine).toLowerCase()}` : `Comment on ${linesLabel(comment.side, comment.startLine, comment.endLine).toLowerCase()}`;
  return <article ref={ref} id={`review-thread-${comment.id}`} className={`review-thread is-${comment.disposition || 'open'} fresh-${comment.freshness || 'unknown'}${focused ? ' is-focused' : ''}${finding ? ` is-finding sev-${comment.severity}` : ''}`} aria-label={label}>
    {finding && <div className="review-finding-head">
      <span className={`review-severity sev-${comment.severity}`}>{severityLabel(comment.severity)}</span>
      <strong>{comment.title}</strong>
      <span className="review-chip is-author">{authorLabel(comment)}</span>
    </div>}
    <header>
      <strong>{linesLabel(comment.side, comment.startLine, comment.endLine)}</strong>
      <small title={`Made on ${comment.head || 'a repository without commits'}`}>{diffLabel(comment)}</small>
      {fresh && <span className={`review-chip fresh-${comment.freshness}`} title={comment.freshnessReason}>{fresh}</span>}
      <span className={`review-chip is-${comment.disposition || 'open'}`} title={comment.disposition === 'sent' && comment.sentHead ? `Put in the message box when ${comment.member} was at ${short(comment.sentHead)}` : undefined}>{dispositionLabel(comment)}</span>
    </header>
    {(quote || comment.freshness === 'outdated') && <QuotedLines comment={comment}/>}
    {comment.freshness === 'outdated' && comment.freshnessReason && <p className="review-muted">{comment.freshnessReason}</p>}
    {editing
      ? <ReviewComposer label="Edit comment" initial={comment.body} submitLabel="Save" onCancel={() => setEditing(false)} onSubmit={async text => { await review.edit(comment.id, text); setEditing(false); }}/>
      : <RichContent text={comment.body}/>}
    {comment.note && <p className="review-muted">{comment.disposition === 'dismissed' ? `Dismissed: ${comment.note}` : comment.note}</p>}
    {error && <p className="review-error" role="alert">{error}</p>}
    {!editing && finding && (isProposed(comment) || isPending(comment)) && <FindingActions comment={comment} review={review} onAsk={c => askReviewer(workspaceId, c)}/>}
    {!editing && <footer>
      {confirmDelete
        ? <span role="group" aria-label="Confirm delete">Delete this comment?<button className="danger" autoFocus onClick={() => act(() => review.remove(comment.id))}>Delete</button><button onClick={() => setConfirmDelete(false)}>Keep</button></span>
        : <>
          <button onClick={() => setEditing(true)}>Edit</button>
          <button onClick={() => setConfirmDelete(true)}>Delete</button>
          <span className="review-spacer"/>
          {(isPending(comment) || comment.disposition === 'sent') && <>
            <button onClick={() => act(() => review.setDisposition([comment.id], 'wont_fix'))}>Won’t fix</button>
            <button onClick={() => act(() => review.setDisposition([comment.id], 'resolved'))}>Resolve</button>
          </>}
          {!isPending(comment) && !isProposed(comment) && <button onClick={() => act(() => review.setDisposition([comment.id], 'open'))} title={comment.disposition === 'sent' ? 'Back to pending, to send again' : 'Back to pending'}>Reopen</button>}
        </>}
    </footer>}
  </article>;
}
