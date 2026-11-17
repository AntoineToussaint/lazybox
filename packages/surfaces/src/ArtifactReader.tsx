import { Children, isValidElement, memo, useCallback, useEffect, useMemo, useState, type MouseEvent, type ReactNode, type RefObject } from 'react';
import Markdown, { type Components, type ExtraProps } from 'react-markdown';
import remarkGfm from 'remark-gfm';
import { CodeBlock } from './RichContent';
import { MermaidDiagram } from './MermaidDiagram';
import { Icon } from './Icons';
import { lineDiff } from './artifact-model';
import { blockDiff, calloutLabel, cellTone, parseCallout, type BlockChange, type CalloutKind, type TocEntry } from './artifact-reader';
import '@fontsource-variable/inter/opsz.css';
import '@fontsource-variable/newsreader/opsz.css';
import '@fontsource-variable/newsreader/opsz-italic.css';
import './artifact-reader.css';

/** The few hast shapes the callout plugin reads and rewrites. */
type HastNode = { type: string; tagName?: string; value?: string; properties?: Record<string, unknown>; children?: HastNode[] };
type HastNodeProp = ExtraProps['node'];
const textOf = (node: HastNode | HastNodeProp | undefined): string => !node ? '' : node.type === 'text' ? (node as HastNode).value || '' : ((node as HastNode).children || []).map(child => textOf(child)).join('');

/**
 * Blockquotes that open with `[!NOTE]` or a bold label (`**Warning**`,
 * `**Decision:**`) become callouts: the label leaves the text and becomes
 * `data-callout` (and `data-callout-title`) on the blockquote.
 */
function rehypeCallouts() {
  const visit = (node: HastNode) => {
    for (const child of node.children || []) visit(child);
    if (node.tagName !== 'blockquote') return;
    const paragraph = node.children?.find(child => child.type === 'element');
    if (paragraph?.tagName !== 'p' || !paragraph.children?.length) return;
    const [first, second] = paragraph.children;
    const start = first.type === 'text' ? first.value || '' : first.tagName === 'strong' ? `**${textOf(first)}**${second?.type === 'text' ? second.value : ''}` : '';
    const callout = parseCallout(start);
    if (!callout) return;
    if (first.type === 'text') first.value = callout.rest;
    else {
      paragraph.children.shift();
      if (second?.type === 'text') second.value = (second.value || '').replace(/^\s*:?\s*/, '');
    }
    // A label alone on its first line leaves a line break or nothing behind.
    while (paragraph.children.length && ((paragraph.children[0].type === 'text' && !paragraph.children[0].value?.trim()) || paragraph.children[0].tagName === 'br')) paragraph.children.shift();
    if (!paragraph.children.length) node.children = node.children!.filter(child => child !== paragraph);
    node.properties = { ...node.properties, dataCallout: callout.kind, dataCalloutTitle: callout.title };
  };
  return (tree: HastNode) => visit(tree);
}

const calloutIcon: Record<CalloutKind, string> = { note: 'notes', tip: 'sparkle', important: 'alert', warning: 'alert', caution: 'alert', decision: 'check' };

function scrollToId(container: HTMLElement | null, id: string) {
  const target = container?.querySelector<HTMLElement>(`#${CSS.escape(id)}`);
  if (!target) return;
  const smooth = document.documentElement.dataset.motion !== 'reduce';
  target.scrollIntoView({ behavior: smooth ? 'smooth' : 'auto', block: 'start' });
  if (/^H[1-6]$/.test(target.tagName)) target.focus({ preventScroll: true });
}

type BodyProps = {
  markdown: string;
  /** Heading ids by source line, from extractToc of the same text. */
  headings?: TocEntry[];
  /** The scroll container, for in-document links (footnotes, anchors). */
  scroller?: RefObject<HTMLElement | null>;
  /** Prefix for footnote ids, so two rendered versions side by side keep theirs apart. */
  idPrefix?: string;
  /** Source lines (1-based) whose row, item or paragraph a comparison marks as changed. */
  marked?: ReadonlySet<number>;
};

/**
 * A document's Markdown, rendered for reading: headings with ids and anchors,
 * callouts, tables in their own scroll frame with status pills, task lists,
 * code with its language and copy, Mermaid diagrams, footnotes. Raw HTML is
 * never interpreted and external images are not loaded.
 */
export const DocumentBody = memo(function DocumentBody({ markdown, headings, scroller, idPrefix = 'doc-', marked }: BodyProps) {
  const byLine = useMemo(() => new Map((headings || []).map(entry => [entry.line, entry])), [headings]);
  const components = useMemo<Components>(() => {
    // A row, item or paragraph that starts on a changed line is marked; its own children are not.
    const isMarked = (node?: HastNodeProp) => !!marked && !!node?.position && marked.has(node.position.start.line);
    const markable = (Tag: 'tr' | 'li' | 'p') => function Markable({ node, children, className }: { node?: HastNodeProp; children?: ReactNode; className?: string }) {
      const classes = [className, isMarked(node) ? 'is-marked' : ''].filter(Boolean).join(' ') || undefined;
      return Tag === 'tr' ? <tr className={classes}>{children}</tr> : Tag === 'li' ? <li className={classes}>{children}</li> : <p className={classes}>{children}</p>;
    };
    const heading = (Tag: 'h1' | 'h2' | 'h3' | 'h4' | 'h5' | 'h6') => function Heading({ node, children, className }: { node?: HastNodeProp; children?: ReactNode; className?: string }) {
      const entry = node?.position ? byLine.get(node.position.start.line) : undefined;
      if (!entry) return <Tag className={className}>{children}</Tag>;
      return <Tag id={entry.id} tabIndex={-1} className={`doc-heading${className ? ' ' + className : ''}`}>
        <a className="doc-anchor" href={`#${entry.id}`} aria-hidden="true" tabIndex={-1} onClick={event => { event.preventDefault(); scrollToId(scroller?.current || null, entry.id); }}>#</a>{children}
      </Tag>;
    };
    return {
      ...(marked ? { tr: markable('tr'), li: markable('li'), p: markable('p') } : {}),
      h1: heading('h1'), h2: heading('h2'), h3: heading('h3'), h4: heading('h4'), h5: heading('h5'), h6: heading('h6'),
      blockquote: ({ node, children }) => {
        const kind = node?.properties?.dataCallout as CalloutKind | undefined;
        if (!kind) return <blockquote>{children}</blockquote>;
        const title = String(node?.properties?.dataCalloutTitle || '');
        return <aside className={`doc-callout is-${kind}`} role="note" aria-label={title ? `${calloutLabel[kind]}: ${title}` : calloutLabel[kind]}>
          <div className="doc-callout-head"><span className="doc-callout-icon"><Icon name={calloutIcon[kind]} size={12}/></span><strong>{calloutLabel[kind]}</strong>{title && <span className="doc-callout-title">{title}</span>}</div>
          <div className="doc-callout-body">{children}</div>
        </aside>;
      },
      table: ({ children }) => <div className="doc-table" role="region" aria-label="Table" tabIndex={0}><table>{children}</table></div>,
      td: ({ node, children, style }) => {
        const tone = cellTone(textOf(node));
        return <td style={style}>{tone ? <span className={`doc-pill tone-${tone}`}>{children}</span> : children}</td>;
      },
      pre: ({ children }) => {
        const child = Children.toArray(children)[0];
        if (!isValidElement<{ className?: string; children?: ReactNode }>(child)) return <pre>{children}</pre>;
        const language = /language-([^\s]+)/.exec(child.props.className || '')?.[1]?.toLowerCase() || '';
        const text = flatText(child.props.children).replace(/\n$/, '');
        return language === 'mermaid' ? <MermaidDiagram eager text={text} sourceView={source => <CodeBlock text={source} language="mermaid"/>}/> : <CodeBlock text={text} language={language} label={languageName(language)}/>;
      },
      a: ({ href, children, id, className }) => {
        if (href?.startsWith('#')) {
          const target = decodeURIComponent(href.slice(1));
          return <a href={href} id={id} className={className} onClick={(event: MouseEvent) => { event.preventDefault(); scrollToId(scroller?.current || null, target); }}>{children}</a>;
        }
        return href && /^https?:\/\//i.test(href) ? <a href={href} target="_blank" rel="noopener noreferrer">{children}</a> : <span className="content-reference">{children}</span>;
      },
      // Do not load external images from an agent's document just by reading it.
      img: ({ alt }) => <span className="content-reference">{alt || 'Image reference'}</span>,
      section: ({ node, children, className }) => (className || '').includes('footnotes') || node?.properties?.dataFootnotes !== undefined ? <section className="doc-footnotes" aria-label="Footnotes">{children}</section> : <section className={className}>{children}</section>,
    };
  }, [byLine, scroller, marked]);
  return <div className="doc-body">
    <Markdown remarkPlugins={[remarkGfm]} remarkRehypeOptions={{ clobberPrefix: idPrefix }} rehypePlugins={[rehypeCallouts]} components={components}>{markdown}</Markdown>
  </div>;
});

function flatText(node: ReactNode): string { if (typeof node === 'string') return node; if (Array.isArray(node)) return node.map(flatText).join(''); return ''; }
const languageNames: Record<string, string> = { ts: 'TypeScript', typescript: 'TypeScript', tsx: 'TSX', js: 'JavaScript', javascript: 'JavaScript', jsx: 'JSX', go: 'Go', rust: 'Rust', rs: 'Rust', py: 'Python', python: 'Python', sh: 'Shell', bash: 'Bash', zsh: 'Shell', shell: 'Shell', json: 'JSON', yaml: 'YAML', yml: 'YAML', sql: 'SQL', diff: 'Diff', md: 'Markdown', markdown: 'Markdown', proto: 'Protobuf', toml: 'TOML', html: 'HTML', css: 'CSS' };
const languageName = (language: string) => languageNames[language] || (language ? language[0].toUpperCase() + language.slice(1) : 'Text');

/**
 * The heading the reader is in: the last one whose top has passed a line a
 * little below the top of the scroll container.
 */
export function useScrollSpy(scroller: RefObject<HTMLElement | null>, ids: string[]) {
  const [active, setActive] = useState(ids[0] || '');
  const key = ids.join('\n');
  useEffect(() => {
    const node = scroller.current;
    if (!node || !ids.length) return;
    let frame = 0;
    const measure = () => {
      frame = 0;
      const line = node.getBoundingClientRect().top + Math.min(120, node.clientHeight * 0.25);
      let current = ids[0];
      for (const id of ids) {
        const heading = node.querySelector<HTMLElement>(`#${CSS.escape(id)}`);
        if (heading && heading.getBoundingClientRect().top <= line) current = id;
      }
      // At the very end, the last heading is the one being read.
      if (node.scrollTop + node.clientHeight >= node.scrollHeight - 4) current = ids.findLast(id => { const h = node.querySelector<HTMLElement>(`#${CSS.escape(id)}`); return h && h.getBoundingClientRect().top < node.getBoundingClientRect().bottom; }) || current;
      setActive(current);
    };
    const schedule = () => { if (!frame) frame = requestAnimationFrame(measure); };
    measure();
    node.addEventListener('scroll', schedule, { passive: true });
    const resize = new ResizeObserver(schedule); resize.observe(node);
    return () => { node.removeEventListener('scroll', schedule); resize.disconnect(); cancelAnimationFrame(frame); };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [scroller, key]);
  return active;
}

/** "On this page": the outline, following the reader, each entry a jump. */
export function TableOfContents({ entries, active, scroller, onPick }: { entries: TocEntry[]; active: string; scroller: RefObject<HTMLElement | null>; onPick?(): void }) {
  const top = Math.min(...entries.map(e => e.level));
  return <nav className="doc-toc" aria-label="On this page">
    <p className="doc-toc-title">On this page</p>
    <ol>{entries.map(entry => <li key={entry.id} className={entry.level > top ? 'is-sub' : undefined}>
      <a href={`#${entry.id}`} aria-current={entry.id === active ? 'location' : undefined} onClick={event => { event.preventDefault(); scrollToId(scroller.current, entry.id); onPick?.(); }}>{entry.text}</a>
    </li>)}</ol>
  </nav>;
}

/** The 1-based lines of one side of a replaced block that differ from the other side. */
function changedLines(before: string, after: string, side: 'removed' | 'added'): Set<number> {
  const out = new Set<number>();
  for (const line of lineDiff(before, after)) {
    if (side === 'removed' && line.kind === 'remove' && line.before) out.add(line.before);
    if (side === 'added' && line.kind === 'add' && line.after) out.add(line.after);
  }
  return out;
}

/**
 * Two versions side by side, rendered: unchanged blocks once on each side
 * (long runs folded), replaced blocks paired, removed and added ones marked.
 * Narrow, the sides stack.
 */
export function RenderedDiff({ before, after, beforeLabel, afterLabel }: { before: string; after: string; beforeLabel: ReactNode; afterLabel: ReactNode }) {
  const changes = useMemo(() => blockDiff(before, after), [before, after]);
  const [opened, setOpened] = useState(() => new Set<number>());
  const open = useCallback((index: number) => setOpened(current => new Set(current).add(index)), []);
  if (!changes.some(change => change.kind === 'changed')) return <p className="artifact-diff-empty">These versions read the same.</p>;
  // Runs of unchanged blocks keep one block of context on each side of a change.
  const rows: ({ fold: number; start: number } | { change: BlockChange; index: number })[] = [];
  for (let i = 0; i < changes.length;) {
    if (changes[i].kind === 'changed') { rows.push({ change: changes[i], index: i }); i++; continue; }
    let end = i; while (end < changes.length && changes[end].kind === 'same') end++;
    const keepStart = i === 0 ? 0 : 1, keepEnd = end === changes.length ? 0 : 1;
    if (end - i > keepStart + keepEnd + 1 && !opened.has(i)) {
      for (let k = i; k < i + keepStart; k++) rows.push({ change: changes[k], index: k });
      rows.push({ fold: end - i - keepStart - keepEnd, start: i });
      for (let k = end - keepEnd; k < end; k++) rows.push({ change: changes[k], index: k });
    } else for (let k = i; k < end; k++) rows.push({ change: changes[k], index: k });
    i = end;
  }
  const side = (change: Extract<BlockChange, { kind: 'changed' }>, kind: 'removed' | 'added', index: number) => {
    const blocks = kind === 'removed' ? change.before : change.after;
    // A block replaced by one block: mark the lines that changed inside it (table rows, items).
    const lines = change.before.length === 1 && change.after.length === 1 ? changedLines(change.before[0], change.after[0], kind) : undefined;
    return blocks.length
      ? blocks.map((block, k) => <div key={k} className={`doc-diff-block is-${kind}`}><span className="visually-hidden">{kind === 'removed' ? 'Removed:' : 'Added:'}</span><DocumentBody markdown={block} marked={lines} idPrefix={`diff-${kind}-${index}-${k}-`}/></div>)
      : <div className="doc-diff-block is-empty" aria-hidden="true"/>;
  };
  return <div className="doc-diff" role="region" aria-label="Rendered changes">
    <div className="doc-diff-head"><div>{beforeLabel}</div><div>{afterLabel}</div></div>
    {rows.map(row => 'fold' in row
      ? <button key={`fold-${row.start}`} type="button" className="doc-diff-fold" onClick={() => open(row.start)}><Icon name="expand" size={12}/>{row.fold} unchanged block{row.fold === 1 ? '' : 's'}</button>
      : row.change.kind === 'same'
        ? <div key={row.index} className="doc-diff-row is-same"><div className="doc-diff-block"><DocumentBody markdown={row.change.before} idPrefix={`diff-a-${row.index}-`}/></div><div className="doc-diff-block"><DocumentBody markdown={row.change.after} idPrefix={`diff-b-${row.index}-`}/></div></div>
        : <div key={row.index} className="doc-diff-row is-changed"><div className="doc-diff-side">{side(row.change, 'removed', row.index)}</div><div className="doc-diff-side">{side(row.change, 'added', row.index)}</div></div>)}
  </div>;
}

