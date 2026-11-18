import { Fragment, useEffect, useMemo, useRef, useState, type PointerEvent as ReactPointerEvent } from 'react';
import hljs from 'highlight.js/lib/core';
import type { ReviewComment } from '@lazybox/protocol';
import './RichContent';
import { ReviewComposer, ReviewThread } from './ReviewThread';
import { isLine, languageOf, linesLabel, parseDiff, placeComment, selectionTarget, type DiffRow, type ReviewView } from './review-model';
import { contextFolds, escapeHtml, expandGap, fileLines, foldedRows, hunkGaps, markRanges, splitIndexes, splitRows, trailingGap, wordHighlights, type DiffMode, type Fold } from './diff-layout';
import { SplitRowView, type LineCells } from './SplitDiff';
import { useWorkspaceReview } from './useWorkspaceReview';
import './review.css';
import { contextMenu, copyText } from './ContextMenu';
import { beginDrag, preventMouseDown } from './drag-gesture';
import './diff-view.css';

type Range = { anchor: number; end: number };
const HIGHLIGHT_LIMIT = 6000;
const GIT_HEADER = /^(diff --git |index |--- |\+\+\+ )/;
const plural = (n: number) => `${n} unchanged line${n === 1 ? '' : 's'}`;

/** A file's diff you can comment on: click + beside a line (or drag, or
 * Shift-click to extend) to comment on one line or a range; saved comments
 * show as threads under their lines. Comments whose lines are not in this
 * diff (outdated, or outside its hunks) are listed above it with their quote.
 * Unified or side by side (`mode`), with the words that changed marked, long
 * unchanged runs folded, and, given `loadFile` (the new side's text), the
 * unchanged lines between hunks one click away. */
export type ReviewFocus = { id: string; nonce: number };
export function ReviewDiff({ workspaceId, member, path, text, view, base, focus, mode = 'unified', loadFile }: { workspaceId: string; member: string; path: string; text: string; view: ReviewView; base: string; focus?: ReviewFocus; mode?: DiffMode; loadFile?(): Promise<string> }) {
  const review = useWorkspaceReview(workspaceId);
  // The diff as shown: the Runner's, plus any gaps the person expanded.
  const [expanded, setExpanded] = useState<{ from: string; text: string }>();
  const shown = expanded?.from === text ? expanded.text : text;
  const rows = useMemo(() => parseDiff(shown), [shown]);
  const language = languageOf(path);
  const highlighted = useMemo(() => {
    const known = language && hljs.getLanguage(language);
    if (!known || rows.length > HIGHLIGHT_LIMIT) return undefined;
    return rows.map(row => isLine(row) ? hljs.highlight(row.text, { language, ignoreIllegals: true }).value : '');
  }, [rows, language]);
  const words = useMemo(() => wordHighlights(rows), [rows]);
  const [range, setRange] = useState<Range>(), [composing, setComposing] = useState(false);
  const dragging = useRef(false);
  // What was typed survives changing the range, which moves the composer.
  const draft = useRef('');
  const [openFolds, setOpenFolds] = useState<ReadonlySet<string>>(new Set());
  const file = useRef<string[]>(undefined);
  const [expanding, setExpanding] = useState(false), [expandError, setExpandError] = useState('');
  // A different file or diff starts afresh.
  useEffect(() => { setRange(undefined); setComposing(false); draft.current = ''; setOpenFolds(new Set()); file.current = undefined; setExpandError(''); }, [member, path, text]);
  useEffect(() => { const stop = () => { if (dragging.current) { dragging.current = false; setComposing(true); } }; window.addEventListener('pointerup', stop); return () => window.removeEventListener('pointerup', stop); }, []);

  const { placed, unplaced, marked } = useMemo(() => {
    const comments = review.comments.filter(c => c.member === member && c.path === path);
    const placed = new Map<number, ReviewComment[]>(), unplaced: ReviewComment[] = [], marked = new Set<number>();
    for (const comment of comments) {
      const row = placeComment(rows, comment);
      if (row === undefined) { unplaced.push(comment); continue; }
      placed.set(row, [...(placed.get(row) || []), comment]);
      // Mark every line the comment covers, walking back along its side.
      let left = comment.snippet.length;
      for (let i = row; i >= 0 && left > 0; i--) { const r = rows[i]; if (isLine(r) && (comment.side === 'old' ? r.type !== 'add' : r.type !== 'del')) { marked.add(i); left--; } }
    }
    return { placed, unplaced, marked };
  }, [rows, review.comments, member, path]);
  const target = range ? selectionTarget(rows, range.anchor, range.end) : undefined;
  const selected = useMemo(() => {
    const out = new Set<number>();
    if (!range || !target) return out;
    const from = Math.min(range.anchor, range.end), to = target.lastRow;
    for (let i = from; i <= to; i++) if (isLine(rows[i]) && rows[i].hunk === rows[range.anchor].hunk) out.add(i);
    return out;
  }, [range, target, rows]);
  // Long unchanged runs of the Runner's diff fold; a commented or chosen line never hides.
  const folds = useMemo(() => {
    const keep = new Set<number>();
    for (const index of [...marked, ...placed.keys(), ...selected]) { const row = rows[index]; if (isLine(row) && row.newLine) keep.add(row.newLine); }
    const original = parseDiff(text);
    return contextFolds(original, 3, 4, new Set(original.flatMap((row, i) => isLine(row) && row.type === 'context' && keep.has(row.newLine!) ? [i] : [])));
  }, [text, rows, marked, placed, selected]);
  const { hidden, starts } = useMemo(() => foldedRows(rows, folds, openFolds), [rows, folds, openFolds]);
  const gaps = useMemo(() => hunkGaps(shown), [shown]);
  // Lines after the last hunk: unknown (-1) until the file is read, none when the diff says it ends there.
  const trailing = loadFile && rows.some(row => row.kind === 'hunk') && !rows.some(row => row.kind === 'note')
    ? (file.current ? trailingGap(shown, file.current.length) : -1) : 0;

  const expand = async (gap: number) => {
    if (!loadFile || expanding) return;
    setExpanding(true); setExpandError('');
    try {
      file.current ??= fileLines(await loadFile());
      setExpanded({ from: text, text: expandGap(shown, file.current, gap) });
    } catch (error) { setExpandError(error instanceof Error ? error.message : String(error)); }
    finally { setExpanding(false); }
  };

  const start = (index: number, event: ReactPointerEvent) => {
    if (event.button !== 0) return;
    event.preventDefault();
    if (event.shiftKey && range) { setRange({ anchor: range.anchor, end: index }); setComposing(true); return; }
    dragging.current = true; setComposing(false); setRange({ anchor: index, end: index });
    // Dragged over the lines to choose several: a drag gesture, so the code under it is not selected. Escape drops the choice.
    beginDrag('y', { pointerId: event.pointerId, cursor: 'pointer', onEnd: reason => { if (reason === 'escape') { dragging.current = false; cancel(); } } });
  };
  const extend = (index: number) => { if (dragging.current && range && rows[index]?.hunk === rows[range.anchor].hunk) setRange({ anchor: range.anchor, end: index }); };
  const keyboard = (index: number, shift: boolean) => { setRange(shift && range ? { anchor: range.anchor, end: index } : { anchor: index, end: index }); setComposing(true); };
  const cancel = () => { setComposing(false); setRange(undefined); draft.current = ''; };
  const save = async (body: string) => {
    if (!target) return;
    await review.add({ member, path, side: target.side, startLine: target.startLine, snippet: target.snippet, body, view, base });
    setComposing(false); setRange(undefined); draft.current = '';
  };

  const code = (index: number) => {
    const row = rows[index];
    if (!isLine(row)) return null;
    const marks = words.get(index);
    const html = highlighted?.[index] || (marks ? escapeHtml(row.text) : undefined);
    return <span role="gridcell" className="review-code"><span className="review-sign">{row.type === 'add' ? '+' : row.type === 'del' ? '-' : ' '}</span>{html ? <span dangerouslySetInnerHTML={{ __html: (marks ? markRanges(html, marks) : html) || ' ' }}/> : row.text || ' '}</span>;
  };
  const addButton = (index: number) => {
    const row = rows[index];
    if (!isLine(row)) return null;
    const number = row.type === 'del' ? row.oldLine : row.newLine, side = row.type === 'del' ? 'removed line' : 'line';
    return <button type="button" className="review-add" aria-label={`Comment on ${side} ${number}`} title="Comment on this line · drag or Shift-click to choose several"
      onPointerDown={event => start(index, event)} onMouseDown={preventMouseDown} onKeyDown={event => { if (event.key === 'Enter' || event.key === ' ') { event.preventDefault(); keyboard(index, event.shiftKey); } }}>+</button>;
  };
  // A line's context menu, the same in both views: comment on it (or the chosen range), copy it.
  const lineMenu = (index: number) => {
    const row = rows[index];
    if (!isLine(row)) return undefined;
    return contextMenu('diff-line', () => [
      selected.size > 1 && selected.has(index) && target
        ? { label: `Comment on ${linesLabel(target.side, target.startLine, target.endLine).toLowerCase()}`, icon: 'chat', run: () => setComposing(true) }
        : { label: 'Add review comment', icon: 'chat', run: () => keyboard(index, false) },
      { label: 'Copy line', icon: 'copy', run: () => copyText(row.text, 'Copied line') },
    ], { member, path, line: (row.type === 'del' ? row.oldLine : row.newLine) ?? 0, side: row.type === 'del' ? 'old' : 'new' });
  };
  const lineClass = (index: number) => { const row = rows[index]; return `${isLine(row) ? `is-${row.type}` : ''}${selected.has(index) ? ' is-selected' : ''}${marked.has(index) ? ' is-commented' : ''}`; };
  const cells: LineCells = {
    className: lineClass, selected: index => selected.has(index), extend, menu: lineMenu,
    cells(index, side) {
      const row = rows[index];
      if (!isLine(row)) return null;
      // Removed lines are commented on from the left, everything else from the right.
      const commentable = side === 'old' ? row.type === 'del' : row.type !== 'del';
      return <>
        <span role="gridcell" className="review-number">{(side === 'old' ? row.oldLine : row.newLine) ?? ''}</span>
        <span role="gridcell" className="review-gutter">{commentable && addButton(index)}</span>
        {code(index)}
      </>;
    },
  };
  /** The composer and saved threads under a line, spanning the whole diff. */
  const after = (index: number) => {
    const threads = placed.get(index);
    const composerHere = composing && target && target.lastRow === index;
    return <>
      {composerHere && <div className="review-inline"><ReviewComposer initial={draft.current} onDraft={text => { draft.current = text; }} label={`Comment on ${linesLabel(target.side, target.startLine, target.endLine).toLowerCase()}`} submitLabel="Add comment" onCancel={cancel} onSubmit={save}/>
        <small className="review-hint">{linesLabel(target.side, target.startLine, target.endLine)} · Shift-click another line to change the range</small></div>}
      {threads && <div className="review-inline">{threads.map(comment => <ReviewThread key={comment.id} workspaceId={workspaceId} comment={comment} review={review} focused={focus?.id === comment.id} focusNonce={focus?.nonce}/>)}</div>}
    </>;
  };
  const foldRow = (fold: Fold) => <div role="row" className="review-row is-fold">
    <span role="gridcell"><button type="button" className="diff-expand" aria-label={`Show ${plural(fold.count)}, lines ${fold.startLine}–${fold.endLine}`} onClick={() => setOpenFolds(open => new Set([...open, fold.id]))}>
      <span aria-hidden="true">⋯</span> {plural(fold.count)}</button></span>
  </div>;
  const gapRow = (count: number, gap: number, where: string) => <div role="row" className="review-row is-gap">
    <span role="gridcell">{loadFile
      ? <button type="button" className="diff-expand" disabled={expanding} aria-label={count < 0 ? 'Show the rest of the file' : `Show ${plural(count)} ${where}`} title="Show the unchanged lines between these changes" onClick={() => void expand(gap)}>
          <span aria-hidden="true">↕</span> {count < 0 ? 'Show the rest of the file' : `${plural(count)} ${where}`}</button>
      : <span className="diff-gap-count">{plural(count)} {where}</span>}</span>
  </div>;
  /** A header row (hunk, git metadata, note), with the hidden lines before a hunk. */
  const header = (row: DiffRow, key: string) => {
    if (row.kind === 'meta' && GIT_HEADER.test(row.text)) return null;
    const gap = row.kind === 'hunk' ? gaps.get(row.hunk) : undefined;
    return <Fragment key={key}>
      {gap !== undefined && gapRow(gap, row.hunk, row.hunk === 1 ? 'above' : 'between changes')}
      <div role="row" className={`review-row is-${row.kind}`} {...(row.kind === 'hunk' ? { 'data-diff-hunk': row.hunk, tabIndex: -1 } : {})}><span role="gridcell" className="review-meta">{row.text}</span></div>
    </Fragment>;
  };
  const lastHunk = rows.reduce((n, row) => Math.max(n, row.hunk), 0);
  const tail = trailing !== 0 && <Fragment key="tail">{gapRow(trailing, lastHunk + 1, 'below')}</Fragment>;

  return <div className="review-diff-wrap">
    {review.error && review.loaded && !review.review && <p className="review-muted" role="status">Review comments unavailable: {review.error}</p>}
    {unplaced.length > 0 && <section className="review-elsewhere" aria-label="Comments not on lines in this diff">
      <h4>{unplaced.length === 1 ? '1 comment' : `${unplaced.length} comments`} on lines not in this diff</h4>
      {unplaced.map(comment => <ReviewThread key={comment.id} workspaceId={workspaceId} comment={comment} review={review} quote focused={focus?.id === comment.id} focusNonce={focus?.nonce}/>)}
    </section>}
    {expandError && <p className="review-error" role="alert">{expandError}</p>}
    <div className={`review-diff is-${mode}`} role="grid" aria-label="File diff" aria-multiselectable="true">
      {mode === 'split'
        ? splitRows(rows).map(row => {
          if (row.kind === 'full') return header(rows[row.row], `h${row.row}`);
          const key = `${row.old ?? ''}:${row.new ?? ''}`;
          const context = row.old !== undefined && row.old === row.new;
          if (context && hidden.has(row.old!)) { const fold = starts.get(row.old!); return fold ? <Fragment key={key}>{foldRow(fold)}</Fragment> : null; }
          return <Fragment key={key}>
            <SplitRowView row={row} line={cells}/>
            {splitIndexes(row).map(index => <Fragment key={index}>{after(index)}</Fragment>)}
          </Fragment>;
        })
        : rows.map((row, index) => {
          if (!isLine(row)) return header(row, `h${index}`);
          if (hidden.has(index)) { const fold = starts.get(index); return fold ? <Fragment key={index}>{foldRow(fold)}</Fragment> : null; }
          return <Fragment key={index}>
            <div role="row" aria-selected={selected.has(index)} className={`review-row ${lineClass(index)}`} onPointerEnter={() => extend(index)} onContextMenu={lineMenu(index)}>
              <span role="gridcell" className="review-number">{row.oldLine ?? ''}</span>
              <span role="gridcell" className="review-number">{row.newLine ?? ''}</span>
              <span role="gridcell" className="review-gutter">{addButton(index)}</span>
              {code(index)}
            </div>
            {after(index)}
          </Fragment>;
        })}
      {tail}
    </div>
  </div>;
}
