import { createContext, memo, useContext, useMemo, useState, type ReactNode } from 'react';
import { AnsiText } from './ansi';
import type { FileLink } from './file-reference';
import type { OpenModifiers } from './file-links';
import { Icon } from './Icons';
import { CodeBlock } from './RichContent';
import { artifactOutcomes, briefItems, commandText, describeGroup, describeTool, describeToolBrief, groupTools, keptInQuiet, kindOf, shortPath, stepsOf, summarizeRun, summarizeTurn, unfolded, type ActivityCategory, type ActivityDetail, type BriefItem, type ToolEntry, type ToolItem, type ToolKind } from './tool-lines';
import { ArtifactCard, RetiredArtifactsCard } from './ArtifactCard';
import { useMessageScope } from './MessageArtifactActions';
import { titleWhenTruncated } from './truncated-title';
import './tool-lines.css';

/**
 * Tool calls as quiet lines of a conversation, each expandable in place to
 * what went in and came out. Reused wherever calls are listed (the chat, a
 * sub-agent's card): give it the entries, and a scope for paths and files.
 */
export type ToolLineScope = {
  /** The workspace folder: paths inside it are shown relative to it. */
  root: string;
  onOpenFile?(reference: FileLink, how: OpenModifiers): void;
  /** The conversation's latest plan: shown as a live checklist, earlier ones folded. */
  latestPlan?: string;
  /** How much activity to show (tool-lines.ts); every line when not given. */
  detail?: ActivityDetail;
};
export const ToolLineContext = createContext<ToolLineScope>({ root: '' });

/** Asks the Activity dialog showing this call to open on it. */
export const TOOL_DETAILS_EVENT = 'lazybox:tool-details';
export function openToolDetails(id: string) { window.dispatchEvent(new CustomEvent(TOOL_DETAILS_EVENT, { detail: { id } })); }

const ICONS: Record<ToolKind, string> = { read: 'file', search: 'search', list: 'folder', edit: 'pencil', command: 'terminal', web: 'external', browser: 'globe', agent: 'branch', plan: 'checklist', thinking: 'sparkle', mcp: 'activity', other: 'activity' };
const CATEGORY_ICONS: Record<ActivityCategory, string> = { explore: 'search', test: 'play', check: 'shield', build: 'processes', install: 'download', git: 'branch', web: 'external', browser: 'globe', command: 'terminal', tool: 'activity', plan: 'checklist', edit: 'pencil', thinking: 'sparkle' };
const stateLabel = (entry: ToolEntry) => entry.status === 'running' ? 'Running' : entry.status === 'failed' ? 'Failed' : entry.status === 'unknown' ? 'No result reported' : entry.status === 'stopped' ? 'Stopped' : entry.status === 'denied' ? 'Denied' : 'Done';
/** Not run: stopped or declined. Said in a word, quietly; the provider's own words are in the details. */
const notRun = (entry: ToolEntry) => entry.status === 'stopped' || entry.status === 'denied';
const itemKey = (item: ToolItem | BriefItem) => item.kind === 'call' ? item.entry.id : `${item.kind}:${item.entries[0].id}`;
const lastRunning = (entries: readonly ToolEntry[]) => { for (let i = entries.length - 1; i >= 0; i--) if (entries[i].status === 'running') return entries[i]; return undefined; };

/**
 * A run of tool calls, as the activity detail says: every call (reads and
 * edits grouped) in Verbose; in Smart one summary for what needs no attention;
 * in Quiet only edits, and `turn` (the whole turn's calls, given on its first
 * run) as the turn's one line.
 */
export const ToolLines = memo(function ToolLines({ entries, turn }: { entries: readonly ToolEntry[]; turn?: readonly ToolEntry[] }) {
  const { detail = 'verbose', latestPlan = '' } = useContext(ToolLineContext);
  const items = useMemo<(ToolItem | BriefItem)[]>(() => detail === 'verbose' ? groupTools(entries) : briefItems(entries, latestPlan, detail === 'quiet'), [entries, detail, latestPlan]);
  const showTurn = detail === 'quiet' && !!turn && (stepsOf(turn).length > 0 || turn.some(e => e.status === 'running'));
  if (!items.length && !showTurn) return <PublishedCards entries={entries}/>;
  return <>
    <div className="tool-lines" role="list" aria-label="Tool calls" data-detail={detail}>
      {showTurn && <TurnLine entries={turn!} latestPlan={latestPlan}/>}
      {items.map(item => <ToolItemLine key={itemKey(item)} item={item}/>)}
    </div>
    <PublishedCards entries={entries}/>
  </>;
});

/**
 * What the agent did with `lazybox artifact put` and `retire`, as cards under
 * the calls, at every activity detail: what it published, what it merged
 * (with Undo), and what it retired (with Undo).
 */
function PublishedCards({ entries }: { entries: readonly ToolEntry[] }) {
  const scope = useMessageScope();
  const outcomes = useMemo(() => entries.map(entry => artifactOutcomes(entry)), [entries]);
  if (!scope?.collectionId || !outcomes.some(o => o.published.length || o.merges.length || o.retired.length)) return null;
  const collectionId = scope.collectionId;
  return <div className="artifact-cards">
    {outcomes.flatMap((o, i) => [
      ...o.merges.map(m => <RetiredArtifactsCard key={`${i}:merge:${m.into.id}@${m.into.version}`} collectionId={collectionId} retired={m.retired} text={m.text} into={m.into}/>),
      ...o.published.map(p => <ArtifactCard key={`${i}:${p.id}@${p.version}`} collectionId={collectionId} artifactId={p.id} title={p.title} kind={p.kind} version={p.version} how={p.version > 1 ? 'updated by the agent' : 'published by the agent'}/>),
      ...(o.retired.length ? [<RetiredArtifactsCard key={`${i}:retired:${o.retired.map(r => r.id).join(',')}`} collectionId={collectionId} retired={o.retired}
        text={o.retired.length === 1 ? `Retired “${o.retired[0].title}”` : `Retired ${o.retired.length} artifacts`}/>] : []),
    ])}
  </div>;
}

/** One line of a run: a call, a group of reads or edits, or (Smart) a run's summary. */
export function ToolItemLine({ item }: { item: ToolItem | BriefItem }) {
  if (item.kind === 'call') return <ToolLine entry={item.entry}/>;
  if (item.kind === 'run') return <ToolRun entries={item.entries}/>;
  return <ToolGroup item={item}/>;
}

const OPEN_EDITS = 4;
function ToolGroup({ item }: { item: Exclude<ToolItem, { kind: 'call' }> }) {
  // In Verbose a few edits stay open, each with its diff; reads fold away.
  const { root, detail = 'verbose' } = useContext(ToolLineContext);
  const [open, setOpen] = useState(detail === 'verbose' && item.kind === 'edits' && item.entries.length <= OPEN_EDITS);
  const { text, meta, running } = describeGroup(item, root);
  return <div className="tool-line tool-group" role="listitem" data-state={running ? 'running' : 'completed'}>
    <button type="button" className="tool-line-row" aria-expanded={open} onClick={() => setOpen(!open)}>
      <Chevron open={open}/><StateIcon kind={item.kind === 'edits' ? 'edit' : 'read'} state={running ? 'running' : 'completed'}/>
      <span className="tool-line-title" onPointerEnter={titleWhenTruncated(text)}><span className="tool-verb">{text}</span></span>
      <Meta meta={meta}/>
    </button>
    {open && <div className="tool-group-calls">{item.entries.map(entry => <ToolLine key={entry.id} entry={entry}/>)}</div>}
  </div>;
}

/**
 * Smart: a run of calls as one line ("Explored the codebase · 8 commands,
 * 3 files read · 58s"), opened in place to its calls. While the agent works
 * the call running now shows under it.
 */
function ToolRun({ entries }: { entries: ToolEntry[] }) {
  const [open, setOpen] = useState(false);
  const summary = summarizeRun(entries);
  const live = !open && summary.running ? lastRunning(entries) : undefined;
  return <Summary className="tool-run" open={open} onToggle={() => setOpen(!open)} running={summary.running} icon={CATEGORY_ICONS[summary.category]} text={summary.text} meta={summary.meta}
    calls={open ? unfolded(entries) : undefined} live={live}/>;
}

/** Quiet: a turn's work as one line ("Worked for 2m 10s · 14 steps · 1 failed"), opened in place to its calls. */
function TurnLine({ entries, latestPlan }: { entries: readonly ToolEntry[]; latestPlan: string }) {
  const [open, setOpen] = useState(false);
  const summary = summarizeTurn(entries);
  // Edits and the plan show beside it already; the step running now shows where the agent is.
  const calls = useMemo(() => open ? unfolded(entries.filter(e => !keptInQuiet(e, latestPlan))) : undefined, [open, entries, latestPlan]);
  return <Summary className="tool-turn" open={open} onToggle={() => setOpen(!open)} running={summary.running} icon={summary.failed ? 'alert' : 'activity'} failed={summary.failed > 0} text={summary.text} meta={summary.meta}
    calls={calls}/>;
}

function Summary({ className, open, onToggle, running, icon, failed, text, meta, calls, live }: { className: string; open: boolean; onToggle(): void; running: boolean; icon: string; failed?: boolean; text: string; meta: string[]; calls?: ToolEntry[]; live?: ToolEntry }) {
  return <div className={`tool-line tool-summary ${className}`} role="listitem" data-state={running ? 'running' : failed ? 'failed' : 'completed'}>
    <button type="button" className="tool-line-row" aria-expanded={open} aria-label={`${text}${running ? '…' : ''}${meta.length ? ' · ' + meta.join(' · ') : ''}`} onClick={onToggle}>
      <Chevron open={open}/><span className="tool-icon" data-state={failed ? 'failed' : undefined} aria-hidden="true"><Icon name={icon} size={13}/></span>
      <span className="tool-line-title" onPointerEnter={titleWhenTruncated(text)}><span className="tool-verb">{text}{running ? '…' : ''}</span></span>
      <Meta meta={meta}/>
    </button>
    {calls && <div className="tool-group-calls" role="list" aria-label="Steps">{calls.map(entry => <ToolLine key={entry.id} entry={entry}/>)}</div>}
    {live && <div className="tool-summary-live" role="list" aria-label="Now">{<ToolLine entry={live}/>}</div>}
  </div>;
}

/** One call: icon, what it did, short facts; click to see input and output. */
export const ToolLine = memo(function ToolLine({ entry }: { entry: ToolEntry }) {
  const { root, onOpenFile, latestPlan, detail = 'verbose' } = useContext(ToolLineContext);
  const [open, setOpen] = useState(false);
  const brief = detail !== 'verbose';
  const title: ReturnType<typeof describeToolBrief> = brief ? describeToolBrief(entry, root) : describeTool(entry, root);
  const kind = kindOf(entry);
  const label = `${title.verb} ${title.target}`.trim();
  // Thinking has nothing to open: what the agent thought stays private.
  if (kind === 'thinking') return <div className="tool-line tool-thinking" role="listitem" data-state={entry.status} data-kind={kind} data-tool-id={entry.id}>
    <span className="tool-line-row" role="status" aria-live="off"><span className="tool-chevron" aria-hidden="true"/><StateIcon kind={kind} state={entry.status === 'running' ? 'thinking' : entry.status}/><span className="tool-line-title"><span className="tool-verb">{title.verb}</span></span></span>
  </div>;
  return <div className="tool-line" role="listitem" data-state={entry.status} data-kind={kind} data-tool-id={entry.id}>
    {/* No tooltip repeating the line: it would cover the next one. The whole line on hover only when it is cut. */}
    <button type="button" className="tool-line-row" aria-expanded={open} aria-label={`${label}${title.meta.length ? ' · ' + title.meta.join(' · ') : ''} · ${stateLabel(entry)}`} onClick={() => setOpen(!open)}>
      <Chevron open={open}/><StateIcon kind={kind} state={entry.status}/>
      <span className="tool-line-title" onPointerEnter={titleWhenTruncated(label)}><span className="tool-verb">{title.verb}</span>{title.target && <> {title.code ? <code className={title.aside ? 'tool-target tool-aside' : 'tool-target'}>{title.target}</code> : <span className="tool-target">{title.target}</span>}</>}</span>
      <Meta meta={title.meta} failed={entry.status === 'failed'}/>
      {notRun(entry) && <span className="tool-line-stopped">{stateLabel(entry)}</span>}
    </button>
    {title.problem && !open && <p className="tool-line-problem" role="note">{title.problem}</p>}
    {!open && kind === 'plan' && entry.id === latestPlan && <div className="tool-inline"><PlanList input={entry.input && typeof entry.input === 'object' ? entry.input as Record<string, any> : undefined}/></div>}
    {!open && !brief && kind === 'edit' && entry.patch && entry.status !== 'failed' && !notRun(entry) && <InlineDiff patch={entry.patch} onMore={() => setOpen(true)}/>}
    {open && <ToolDetails entry={entry} kind={kind} root={root} file={title.file} onOpenFile={onOpenFile}/>}
  </div>;
});

const INLINE_DIFF_LINES = 8;
/** An edit's changed lines under its line, the start of them: the rest on open. */
function InlineDiff({ patch, onMore }: { patch: string; onMore(): void }) {
  const lines = useMemo(() => patch.split('\n').filter(l => (l.startsWith('+') || l.startsWith('-')) && !l.startsWith('+++') && !l.startsWith('---')), [patch]);
  if (!lines.length) return null;
  const shown = lines.slice(0, INLINE_DIFF_LINES);
  return <div className="tool-inline tool-inline-diff" aria-label="Changed lines">
    <pre>{shown.map((l, i) => <span key={i} data-sign={l[0] === '+' ? 'add' : 'del'}>{l}{'\n'}</span>)}</pre>
    {lines.length > shown.length && <button type="button" className="tool-output-more" onClick={onMore}>{lines.length - shown.length} more changed lines</button>}
  </div>;
}

function Chevron({ open }: { open: boolean }) {
  return <span className="tool-chevron" data-open={open || undefined} aria-hidden="true"><Icon name="chevronRight" size={12}/></span>;
}
function StateIcon({ kind, state }: { kind: ToolKind; state: string }) {
  return <span className="tool-icon" data-state={state} aria-hidden="true">{state === 'running' ? <span className="tool-spinner"/> : state === 'thinking' ? <span className="tool-thinking-dot"><Icon name="sparkle" size={13}/></span> : state === 'stopped' || state === 'denied' ? <span className="tool-stop-square"/> : <Icon name={state === 'failed' ? 'alert' : ICONS[kind]} size={13}/>}</span>;
}
function Meta({ meta, failed }: { meta: string[]; failed?: boolean }) {
  if (!meta.length) return null;
  return <span className="tool-meta">{meta.map(m => <span key={m} data-tone={/^exit [1-9]/.test(m) || /^\d+ failed$/.test(m) || (failed && m.startsWith('exit')) ? 'failed' : /^\+\d/.test(m) ? 'diff' : undefined}>{diffMeta(m)}</span>)}</span>;
}
/** "+12 −4" with each side coloured. */
function diffMeta(m: string): ReactNode {
  const match = /^\+(\d+) −(\d+)$/.exec(m);
  return match ? <>{match[1] !== '0' || match[2] === '0' ? <ins>+{match[1]}</ins> : null}{match[1] !== '0' && match[2] !== '0' ? ' ' : null}{match[2] !== '0' ? <del>−{match[2]}</del> : null}</> : m;
}

const OUTPUT_LINES = 14;
/** A diff's file headers, relative to the workspace. */
const relativePatch = (patch: string, root: string) => root ? patch.replace(/^(---|\+\+\+) (.+)$/gm, (_, mark, path) => `${mark} ${shortPath(path, root)}`) : patch;

function ToolDetails({ entry, kind, root, file, onOpenFile }: { entry: ToolEntry; kind: ToolKind; root: string; file?: string; onOpenFile?: ToolLineScope['onOpenFile'] }) {
  const command = kind === 'command' ? commandText(entry) : '';
  const input = entry.input && typeof entry.input === 'object' ? entry.input as Record<string, any> : undefined;
  const showInput = !command && !entry.patch && kind !== 'read' && kind !== 'plan' && input && Object.keys(input).length > 0;
  return <div className="tool-details">
    {command && <CodeBlock text={command} language="bash" label={entry.cwd ? `Command · in ${shortPath(entry.cwd, '')}` : 'Command'}/>}
    {entry.patch && <CodeBlock text={relativePatch(entry.patch, root)} language="diff" label="Changes"/>}
    {kind === 'plan' && <PlanList input={input}/>}
    {showInput && <CodeBlock text={JSON.stringify(input, null, 2)} language="json" label="Input"/>}
    {entry.output !== undefined && entry.output !== '' && kind !== 'plan' && !(kind === 'edit' && entry.status !== 'failed' && !notRun(entry)) && <ToolOutput text={entry.output} lines={entry.outputLines || 0} failed={entry.status === 'failed'}/>}
    {entry.outputImages ? <small className="tool-note">{entry.outputImages} image{entry.outputImages === 1 ? '' : 's'} returned</small> : null}
    {entry.output === undefined && entry.status === 'running' && <small className="tool-note">Waiting for the result…</small>}
    {entry.output === undefined && entry.status === 'unknown' && <small className="tool-note">The agent never reported a result for this call.</small>}
    <div className="tool-details-actions">
      {file && onOpenFile && <button type="button" onClick={event => onOpenFile({ path: file }, event)}><Icon name="file" size={12}/>Open {shortPath(file, root).split('/').pop()}</button>}
      <button type="button" onClick={() => openToolDetails(entry.id)}><Icon name="external" size={12}/>Open full details</button>
    </div>
  </div>;
}

/** Output with terminal colours, the last lines first shown, all on request. */
function ToolOutput({ text, lines, failed }: { text: string; lines: number; failed: boolean }) {
  const [all, setAll] = useState(false);
  const split = useMemo(() => text.split('\n'), [text]);
  // A failure's cause is usually at the end; a read's content at the start.
  const shown = all || split.length <= OUTPUT_LINES ? text : failed ? split.slice(-OUTPUT_LINES).join('\n') : split.slice(0, OUTPUT_LINES).join('\n');
  return <div className="tool-output" data-failed={failed || undefined}>
    <pre aria-label="Output"><AnsiText text={shown}/></pre>
    {split.length > OUTPUT_LINES && <button type="button" className="tool-output-more" onClick={() => setAll(!all)}>{all ? 'Show less' : `Show all ${(lines || split.length).toLocaleString()} lines`}</button>}
  </div>;
}

function PlanList({ input }: { input?: Record<string, any> }) {
  const todos: any[] = Array.isArray(input?.todos) ? input!.todos : Array.isArray(input?.plan) ? input!.plan : [];
  if (!todos.length) return null;
  return <ul className="tool-plan" aria-label="Plan">{todos.map((t, i) => {
    const status = t?.status === 'completed' ? 'completed' : t?.status === 'in_progress' || t?.status === 'inProgress' ? 'in_progress' : 'pending';
    return <li key={i} data-status={status}><span aria-hidden="true">{status === 'completed' ? '✓' : status === 'in_progress' ? '◐' : '○'}</span>{String(t?.content ?? t?.step ?? t?.text ?? '')}</li>;
  })}</ul>;
}
