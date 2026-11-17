import { memo, useEffect, useLayoutEffect, useMemo, useRef, useState, type CSSProperties, type KeyboardEvent, type MouseEvent, type ReactNode, type RefObject } from 'react';
import { segmentStyle } from './ansi';
import { readLine, SourceColours, sliceSegments, tailLines, type LinkToken, type ListSpan, type LogLine, type LogRow } from './log-lines';
import type { FileLink } from './file-reference';
import type { OpenModifiers } from './file-links';
import { isMac } from './workbench';
import { Icon } from './Icons';
import '@fontsource-variable/jetbrains-mono/wght.css';
import './log-view.css';

/**
 * Command output laid out as a log (log-lines.ts reads it): a dim time
 * gutter, an aligned source column in each source's colour, level badges,
 * milestone rows, and links to pages, local ports and files. Long lists fold
 * to a count and repeated lines to ×N; both open in place. Copy never reads
 * this: it takes the output as received.
 */

export type LogActions = {
  /** A web page or local port; `browser` for ⌘/Ctrl-click. */
  openLink(url: string, browser: boolean): void;
  openFile?(link: FileLink, how: OpenModifiers): void;
  /** Opens or folds a long list on a line, by the list's start. */
  toggleList?(lineId: number, start: number): void;
  /** Shows a fold line by line, or folds it again (by its first line's id). */
  toggleFold?(firstId: number): void;
};
export type LogLayout = {
  hasTime: boolean;
  /** Characters of the widest source name; 0 for no source column. */
  sourceWidth: number;
  colour(name: string): number | undefined;
  /** Highlighted in the text. */
  search: string;
};

const browserKey = isMac ? '⌘' : 'Ctrl', altKey = isMac ? '⌥' : 'Alt';

/** The grid columns every row of one log shares, as custom properties on the list. */
export function layoutStyle(layout: Pick<LogLayout, 'hasTime' | 'sourceWidth'>): CSSProperties {
  return { '--log-time-w': layout.hasTime ? 'calc(8ch + 1.25ch)' : '0px', '--log-source-w': layout.sourceWidth ? `calc(${Math.min(layout.sourceWidth, 28) + 2}ch + 1.25ch)` : '0px' } as CSSProperties;
}

function Highlighted({ text, needle }: { text: string; needle: string }) {
  if (!needle) return <>{text}</>;
  const lower = text.toLocaleLowerCase(), out: ReactNode[] = [];
  let at = 0;
  for (let i = lower.indexOf(needle); i >= 0; i = lower.indexOf(needle, i + needle.length)) {
    if (i > at) out.push(text.slice(at, i));
    out.push(<mark key={i} className="log-match">{text.slice(i, i + needle.length)}</mark>);
    at = i + needle.length;
  }
  if (at < text.length) out.push(text.slice(at));
  return <>{out}</>;
}

/** Part of a line, in its terminal colours. */
function Styled({ line, start, end, needle }: { line: LogLine; start: number; end: number; needle: string }) {
  return <>{sliceSegments(line.segments, start, end).map((segment, i) => {
    const style = segmentStyle(segment.style);
    return style ? <span key={i} style={style}><Highlighted text={segment.text} needle={needle}/></span> : <Highlighted key={i} text={segment.text} needle={needle}/>;
  })}</>;
}

type Deco = { start: number; end: number; node: (children: ReactNode) => ReactNode; replace?: boolean };

function linkNode(line: LogLine, link: LinkToken, actions: LogActions): Deco['node'] {
  if (link.kind === 'file') {
    if (!actions.openFile) return children => <span key={`l${link.start}`}>{children}</span>;
    const open = (how: OpenModifiers) => actions.openFile!({ path: link.path, line: link.line }, how);
    const where = `${link.path}${link.line ? `:${link.line}` : ''}`;
    return children => <a key={`l${link.start}`} className="log-link log-file" role="link" tabIndex={0} title={`Open ${where} in the editor · ${altKey}-click: a quick look`}
      onClick={(event: MouseEvent) => { event.preventDefault(); event.stopPropagation(); open(event); }}
      onKeyDown={(event: KeyboardEvent) => { if (event.key === 'Enter') { event.preventDefault(); open(event); } }}>{children}</a>;
  }
  const title = link.kind === 'port' ? `Open localhost:${link.port} in the Web panel · ${browserKey}-click: your browser` : `Open in the Web panel · ${browserKey}-click: your browser`;
  return children => <a key={`l${link.start}`} className="log-link" href={link.url} title={title} data-port={link.kind === 'port' ? link.port : undefined}
    onClick={(event: MouseEvent) => { event.preventDefault(); event.stopPropagation(); actions.openLink(link.url, event.metaKey || event.ctrlKey); }}>{children}</a>;
}

/** The message of a line, from `from`: badges, links and folded lists over its styled text. */
function Message({ line, from, open, actions, needle }: { line: LogLine; from: number; open?: ReadonlySet<number>; actions: LogActions; needle: string }) {
  const decos: Deco[] = [];
  const level = line.levelSpan;
  if (level && level.start >= from) decos.push({ start: level.start, end: level.end, replace: true, node: () => <span key="level" className="log-level" data-level={line.level}>{level.label}</span> });
  for (const list of line.lists) {
    if (list.start < from) continue;
    if (open?.has(list.start)) {
      decos.push({ start: list.end, end: list.end, replace: true, node: () => <button key={`f${list.start}`} type="button" className="log-fold" data-open="" aria-expanded="true" title="Fold this list again" onClick={() => actions.toggleList?.(line.id, list.start)}>less</button> });
      continue;
    }
    decos.push({ start: list.start, end: list.end, replace: true, node: () => <ListFold key={`f${list.start}`} line={line} list={list} actions={actions}/> });
  }
  for (const link of line.links) if (link.start >= from && !decos.some(d => d.replace && d.end > d.start && link.start < d.end && link.end > d.start)) decos.push({ start: link.start, end: link.end, node: linkNode(line, link, actions) });
  decos.sort((a, b) => a.start - b.start || a.end - b.end);
  const out: ReactNode[] = [];
  let at = from;
  for (const deco of decos) {
    if (deco.start < at) continue;
    if (deco.start > at) out.push(<Styled key={`t${at}`} line={line} start={at} end={deco.start} needle={needle}/>);
    out.push(deco.replace ? deco.node(null) : deco.node(<Styled line={line} start={deco.start} end={deco.end} needle={needle}/>));
    at = deco.end;
  }
  if (at < line.text.length) out.push(<Styled key={`t${at}`} line={line} start={at} end={line.text.length} needle={needle}/>);
  return <>{out}</>;
}

function ListFold({ line, list, actions }: { line: LogLine; list: ListSpan; actions: LogActions }) {
  const inner = line.text.slice(list.start, list.end);
  return <>{list.bracketed && <span className="log-bracket">[</span>}<button type="button" className="log-fold" aria-expanded="false" title={`${inner.length > 400 ? inner.slice(0, 400) + '…' : inner}\nClick to show all ${list.label}`}
    onClick={() => actions.toggleList?.(line.id, list.start)}>… {list.label}</button>{list.bracketed && <span className="log-bracket">]</span>}</>;
}

type RowProps = { row: LogRow; layout: LogLayout; actions: LogActions; open?: ReadonlySet<number>; first: boolean };

/** One row: time, source and message in the log's shared columns; the message wraps under itself. */
export const LogRowView = memo(function LogRowView({ row, layout, actions, open, first }: RowProps) {
  const { line, count } = row;
  const needle = layout.search.trim().toLocaleLowerCase();
  const wide = !line.time && !line.source && !line.continuation && !line.marker;
  const colour = line.source ? layout.colour(line.source.name) : undefined;
  const marker = line.marker && <span className="log-marker" aria-hidden="true">{line.text.slice(line.marker.start, line.marker.end)}</span>;
  return <div className="log-row" data-level={line.level} data-milestone={line.milestone || undefined} data-wide={wide || undefined} data-continuation={line.continuation || undefined}>
    {/* A newline between rows, for a selection's copy and the text of the list; it takes no room. */}
    {!first && <span className="log-nl">{'\n'}</span>}
    {!wide && layout.hasTime && <span className="log-time" title={line.time ? line.text.slice(line.time.start, line.time.end) : undefined}>{line.time ? line.time.label : marker}</span>}
    {!wide && layout.sourceWidth > 0 && <span className="log-source" style={colour ? { '--log-source': `var(--log-source-${colour})` } as CSSProperties : undefined} title={line.source?.name}>
      {/* `[codefly]` keeps its brackets around the name; a runner's separator (`>`, `|`, `:`) lines up at the column's edge. */}
      {line.source && (line.source.open
        ? <span className="log-source-name"><span className="log-bracket">{line.source.open}</span>{line.source.name}<span className="log-bracket">{line.source.sep}</span></span>
        : <><span className="log-source-name">{line.source.name}</span><span className="log-sep">{line.source.sep}</span></>)}
    </span>}
    <span className="log-msg">{!layout.hasTime && marker}{!layout.hasTime && marker && ' '}<Message line={line} from={wide ? 0 : line.message} open={open} actions={actions} needle={needle}/>
      {count > 1 && <button type="button" className="log-repeat" aria-expanded={!!row.expanded} title={`${row.expanded ? 'Fold' : 'Show'} the ${count} times this line came${row.first.time ? `, first at ${row.first.time.label}` : ''}`} onClick={() => actions.toggleFold?.(row.first.id)}>×{count}</button>}
    </span>
  </div>;
}, (a, b) => a.row.line === b.row.line && a.row.count === b.row.count && a.row.expanded === b.row.expanded && a.row.first === b.row.first && a.layout === b.layout && a.actions === b.actions && a.open === b.open && a.first === b.first);

/** A row's lasting identity: a fold's first line, which stays while the fold grows; a line shown on its own, itself. */
const rowId = (row: LogRow) => row.expanded ? row.line.id : row.first.id;
const BLOCK = 128;
const OVERSCAN = 900;
type Block = { id: number; rows: LogRow[] };
const blockSignature = (block: Block) => `${block.rows.length}:${block.rows[0].line.id}:${block.rows.at(-1)!.line.id}:${block.rows.at(-1)!.count}`;

const LogBlock = memo(function LogBlock({ block, layout, actions, openLists, first, measure }: { block: Block; layout: LogLayout; actions: LogActions; openLists: ReadonlyMap<number, ReadonlySet<number>>; first: boolean; measure(id: number, node: HTMLDivElement | null): void }) {
  return <div className="log-block" data-block={block.id} ref={node => measure(block.id, node)}>
    {block.rows.map((row, i) => <LogRowView key={rowId(row)} row={row} layout={layout} actions={actions} open={openLists.get(row.line.id)} first={first && i === 0}/>)}
  </div>;
}, (a, b) => a.layout === b.layout && a.actions === b.actions && a.openLists === b.openLists && a.first === b.first && a.measure === b.measure && a.block.id === b.block.id
  && a.block.rows.length === b.block.rows.length && a.block.rows.every((row, i) => { const other = b.block.rows[i]; return row.line === other.line && row.count === other.count && row.expanded === other.expanded; }));

/**
 * The rows of a log, windowed: rows are grouped into blocks of consecutive
 * lines, and only the blocks near the visible part of `scroller` are in the
 * page; the others keep their measured (or estimated) height. A block that
 * changes height above what is being read does not move it.
 */
export function LogList({ rows, layout, actions, openLists, scroller, follow, lineHeight }: {
  rows: LogRow[]; layout: LogLayout; actions: LogActions; openLists: ReadonlyMap<number, ReadonlySet<number>>;
  scroller: RefObject<HTMLElement | null>; follow: boolean; lineHeight: number;
}) {
  const blocks = useMemo(() => {
    const out: Block[] = [];
    for (const row of rows) {
      const id = Math.floor(rowId(row) / BLOCK), last = out.at(-1);
      if (last && last.id === id) last.rows.push(row); else out.push({ id, rows: [row] });
    }
    return out;
  }, [rows]);
  const heights = useRef(new Map<number, { signature: string; height: number }>());
  const nodes = useRef(new Map<number, HTMLDivElement>());
  const [view, setView] = useState({ top: 0, height: 800 });
  const [, setMeasured] = useState(0);
  const anchor = useRef<{ id: number; offset: number } | null>(null);
  const observer = useRef<ResizeObserver | null>(null);
  const blocksRef = useRef(blocks); blocksRef.current = blocks;

  const record = () => {
    const pane = scroller.current; if (!pane) return;
    const top = pane.getBoundingClientRect().top;
    for (const [id, node] of nodes.current) {
      const rect = node.getBoundingClientRect();
      if (rect.bottom > top) { if (!anchor.current || anchor.current.id !== id || Math.abs(anchor.current.offset - (rect.top - top)) > 0.5) anchor.current = { id, offset: rect.top - top }; return; }
    }
  };
  useEffect(() => {
    const pane = scroller.current; if (!pane) return;
    let frame = 0;
    const update = () => { frame = 0; setView(current => Math.abs(current.top - pane.scrollTop) < 40 && current.height === pane.clientHeight ? current : { top: pane.scrollTop, height: pane.clientHeight }); };
    // The anchor follows the reader at once, so a render before the next frame never pulls them back.
    const onScroll = () => { record(); if (!frame) frame = requestAnimationFrame(update); };
    pane.addEventListener('scroll', onScroll, { passive: true });
    let width = pane.clientWidth;
    const resize = new ResizeObserver(() => { if (pane.clientWidth !== width) { width = pane.clientWidth; heights.current.clear(); } onScroll(); });
    resize.observe(pane);
    update();
    return () => { pane.removeEventListener('scroll', onScroll); resize.disconnect(); if (frame) cancelAnimationFrame(frame); };
  }, [scroller]);
  // A new text size changes every row's height.
  useEffect(() => { heights.current.clear(); }, [lineHeight]);
  useEffect(() => {
    observer.current = new ResizeObserver(entries => {
      let changed = false;
      for (const entry of entries) {
        const node = entry.target as HTMLDivElement, id = Number(node.dataset.block);
        const block = blocksRef.current.find(value => value.id === id); if (!block) continue;
        const height = node.offsetHeight, known = heights.current.get(id), signature = blockSignature(block);
        if (!known || known.height !== height || known.signature !== signature) { heights.current.set(id, { signature, height }); changed = true; }
      }
      if (changed) setMeasured(n => n + 1);
    });
    for (const node of nodes.current.values()) observer.current.observe(node);
    return () => observer.current?.disconnect();
  }, []);
  const measure = useMemo(() => (id: number, node: HTMLDivElement | null) => {
    const previous = nodes.current.get(id);
    if (previous && previous !== node) { observer.current?.unobserve(previous); nodes.current.delete(id); }
    if (node && previous !== node) { nodes.current.set(id, node); observer.current?.observe(node); }
  }, []);

  // Measured heights where known, else an estimate from the rows measured so far.
  let measuredHeight = 0, measuredRows = 0;
  for (const block of blocks) { const known = heights.current.get(block.id); if (known && known.signature === blockSignature(block)) { measuredHeight += known.height; measuredRows += block.rows.length; } }
  const perRow = measuredRows ? measuredHeight / measuredRows : lineHeight;
  const sizes = blocks.map(block => { const known = heights.current.get(block.id); return known && known.signature === blockSignature(block) ? known.height : block.rows.length * perRow; });
  let first = 0, last = blocks.length - 1;
  if (blocks.length > 3) {
    // Following, the window is the end, wherever the last scroll left it.
    const total = sizes.reduce((sum, size) => sum + size, 0);
    const top = follow ? Math.max(0, total - view.height) : view.top;
    const from = top - OVERSCAN, to = top + view.height + OVERSCAN;
    let offset = 0; first = -1; last = -1;
    sizes.forEach((size, i) => { if (offset + size >= from && offset <= to) { if (first < 0) first = i; last = i; } offset += size; });
    if (first < 0) { first = Math.max(0, blocks.length - 2); last = blocks.length - 1; }
    // Following: the end is always there, so the newest line is never a gap.
    if (follow) last = blocks.length - 1;
    if (follow && first > last - 1) first = Math.max(0, last - 1);
    // The block being read stays while its neighbours come and go.
    const anchored = anchor.current ? blocks.findIndex(block => block.id === anchor.current!.id) : -1;
    if (!follow && anchored >= first - 2 && anchored <= last + 2) { first = Math.min(first, anchored); last = Math.max(last, anchored); }
  }
  const before = sizes.slice(0, first).reduce((sum, size) => sum + size, 0);
  const after = sizes.slice(last + 1).reduce((sum, size) => sum + size, 0);

  // Keep what is being read in place when blocks above it change height (WebKit has no scroll anchoring).
  useLayoutEffect(() => {
    const pane = scroller.current; if (!pane || follow || !anchor.current) return;
    const node = nodes.current.get(anchor.current.id); if (!node) return;
    const offset = node.getBoundingClientRect().top - pane.getBoundingClientRect().top;
    const delta = offset - anchor.current.offset;
    if (Math.abs(delta) > 1) pane.scrollTop += delta;
  });
  useEffect(() => { if (follow) anchor.current = null; }, [follow]);

  return <div className="log-list" style={layoutStyle(layout)}>
    {before > 0 && <div className="log-spacer" style={{ height: before }} aria-hidden="true"/>}
    {blocks.slice(first, last + 1).map((block, i) => <LogBlock key={block.id} block={block} layout={layout} actions={actions} openLists={openLists} first={first + i === 0} measure={measure}/>)}
    {after > 0 && <div className="log-spacer" style={{ height: after }} aria-hidden="true"/>}
  </div>;
}

/**
 * The last lines of a command's output, read the same way as the console:
 * the live bar's tail. `count` lines at most; no windowing, no filters.
 */
export function LogTail({ text, count, actions }: { text: string; count: number; actions: LogActions }) {
  const { rows, layout } = useMemo(() => {
    const colours = new SourceColours();
    let previous: LogLine | undefined;
    const lines = tailLines(text, count).map((raw, i) => (previous = readLine(raw, i, previous)));
    for (const line of lines) if (line.source) colours.colour(line.source.name);
    const layout: LogLayout = { hasTime: lines.some(line => line.time), sourceWidth: Math.max(0, ...lines.map(line => line.source ? Math.min(line.source.name.length, 20) : 0)), colour: name => colours.get(name), search: '' };
    return { rows: lines.map(line => ({ line, count: 1, first: line })), layout };
  }, [text, count]);
  return <div className="log-list log-tail" style={layoutStyle(layout)}>{rows.map((row, i) => <LogRowView key={i} row={row} layout={layout} actions={actions} first={i === 0}/>)}</div>;
}

/**
 * One chip per source, to show or hide its lines: a click toggles it, the
 * target (or ⌥-click) shows only that source, and Show all brings every
 * source back. "Other" stands for lines no source wrote.
 */
export function SourceFilter({ sources, counts, hidden, colour, onChange }: { sources: readonly string[]; counts: ReadonlyMap<string, number>; hidden: ReadonlySet<string>; colour(name: string): number | undefined; onChange(next: ReadonlySet<string>): void }) {
  const names = counts.get('') ? [...sources, ''] : [...sources];
  const toggle = (name: string) => { const next = new Set(hidden); if (next.has(name)) next.delete(name); else next.add(name); onChange(next); };
  const only = (name: string) => onChange(new Set(names.filter(other => other !== name)));
  return <div className="log-sources" role="group" aria-label="Sources">
    {names.map(name => {
      const label = name || 'Other', shown = !hidden.has(name), n = colour(name);
      return <span key={name || '\u0000other'} className="log-chip" data-off={shown ? undefined : ''} style={n ? { '--log-source': `var(--log-source-${n})` } as CSSProperties : undefined}>
        <button type="button" className="log-chip-toggle" aria-pressed={shown} title={`${shown ? 'Hide' : 'Show'} ${name ? `${name}’s lines` : 'lines with no source'} · ${altKey}-click: only these`}
          onClick={event => { if (event.altKey) only(name); else toggle(name); }}>
          <span className="log-chip-dot" aria-hidden="true"/><span className="log-chip-name">{label}</span><span className="log-chip-count">{counts.get(name) || 0}</span>
        </button>
        <button type="button" className="log-chip-only" aria-label={`Only ${label}`} title={`Show only ${name ? `${name}’s lines` : 'lines with no source'}`} onClick={() => only(name)}><Icon name="target" size={11}/></button>
      </span>;
    })}
    {hidden.size > 0 && <button type="button" className="log-sources-reset" onClick={() => onChange(new Set())}>Show all</button>}
  </div>;
}
