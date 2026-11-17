import { memo, useEffect, useId, useLayoutEffect, useMemo, useRef, useState, type KeyboardEvent, type RefObject } from 'react';
import { Icon } from './Icons';
import { RichContent } from './RichContent';
import { ToolItemLine, ToolLineContext } from './ToolLine';
import { titleWhenTruncated } from './truncated-title';
import { useNow } from './LiveIndicator';
import { elapsedLabel } from './live-status';
import { activeSubagent, agentElapsed, currentAction, mergedStatus, resultPreview, statusWord, stepRows, subagentLabel, tokenLabel, toolTotal, type Subagent, type SubagentStatus } from './subagent-model';
import { usePresentation, useSubagent, useSubagentView, type SubagentView } from './subagent-view';
import './subagents.css';
import { contextMenu } from './ContextMenu';
import { subagentItems, type SubagentMenuDetail } from './subagent-context-menu';

const TIMELINE_STEP = 40;
/** Steps an open card shows: the latest few; the panel and "Show all" show the rest. */
const CARD_STEPS = 5;

/** A sub-agent's state at a glance: a turning ring while it works, a check, a cross, a square. */
export function SubagentMark({ status, size = 14 }: { status: SubagentStatus; size?: number }) {
  return <span className="subagent-mark" data-status={status} role="img" aria-label={statusWord[status]} style={{ ['--mark-size' as string]: `${size}px` }}>
    {status === 'done' ? <Icon name="check" size={size - 2}/> : status === 'failed' ? <Icon name="close" size={size - 3}/> : status === 'stopped' ? <span className="subagent-mark-square"/> : status === 'unknown' ? '?' : null}
  </span>;
}

/** The status a card shows: the journal's, unless the Runner knows it ended. */
export function useAgentStatus(view: SubagentView | null, agent: Subagent | undefined): SubagentStatus {
  usePresentation(view);
  return agent ? mergedStatus(agent, view?.runner) : 'unknown';
}

/** Its kind (when the kind says something) and its task; see `subagentLabel`. */
export const subagentLabelOf = (agent: Subagent, view?: SubagentView | null) => subagentLabel(agent, view?.names[agent.runnerId] || view?.names[agent.id] || '');
export const subagentTitle = (agent: Subagent, view?: SubagentView | null) => subagentLabelOf(agent, view).title;

/** "Explore · Map the transcript pipeline": the kind, muted, then the task; one line, the whole on hover only when cut. */
export function SubagentName({ agent, view, className = 'subagent-who' }: { agent: Subagent; view?: SubagentView | null; className?: string }) {
  const { kind, title } = subagentLabelOf(agent, view);
  return <span className={className} onPointerEnter={titleWhenTruncated(kind ? `${kind} · ${title}` : title)}>
    {kind && <><span className="subagent-type">{kind}</span><span className="subagent-sep" aria-hidden="true"> · </span></>}
    <span className="subagent-name">{title}</span>
  </span>;
}

/**
 * "12m 04s · 14 tools · 48k tokens". The one elapsed time a sub-agent shows,
 * wherever it shows: since it started, or how long it ran.
 */
export function SubagentStats({ agent, status, now }: { agent: Subagent; status: SubagentStatus; now: number }) {
  const elapsed = agentElapsed({ ...agent, status }, now), tools = toolTotal(agent);
  const parts = [
    elapsed !== undefined && <time key="time" title={`${activeSubagent(status) ? 'Running for' : 'Ran for'} ${elapsedLabel(elapsed)}`}>{elapsedLabel(elapsed)}</time>,
    tools > 0 && <span key="tools" className="subagent-stat-tools" title={`${tools.toLocaleString()} tool ${tools === 1 ? 'call' : 'calls'}`}>{tools} {tools === 1 ? 'tool' : 'tools'}</span>,
    agent.tokens !== undefined && agent.tokens > 0 && <span key="tokens" className="subagent-stat-tokens" title={`${agent.tokens.toLocaleString()} tokens`}>{tokenLabel(agent.tokens)}</span>,
  ].filter(Boolean);
  return <span className="subagent-stats">{parts.flatMap((part, i) => i ? [<span key={i} className="subagent-sep" aria-hidden="true"> · </span>, part] : [part])}</span>;
}

const nounOf = (agent: Subagent) => agent.kind === 'command' ? 'background command' : 'sub-agent';

/** Delete or Backspace, alone, on a sub-agent's card, chip or row: stop it. */
export function stopKeys(view: SubagentView, agent: Subagent, status: SubagentStatus) {
  return (event: KeyboardEvent) => {
    if ((event.key !== 'Delete' && event.key !== 'Backspace') || event.metaKey || event.ctrlKey || event.altKey || event.shiftKey || event.repeat) return;
    if (!view.canStopAgent(agent, status) || view.isStopping(agent)) return;
    event.preventDefault(); event.stopPropagation();
    void view.stop(agent);
  };
}

/**
 * ■ Stop: ends one sub-agent (or background command); the main agent keeps
 * working. Offered while it runs and the Runner can address it; it reads
 * Stopping… until its end is reported. `compact` is the square alone (a chip, a row).
 */
export function StopSubagent({ view, agent, status, compact = false }: { view: SubagentView; agent: Subagent; status: SubagentStatus; compact?: boolean }) {
  if (!view.canStopAgent(agent, status)) return null;
  const stopping = view.isStopping(agent), noun = nounOf(agent), title = subagentTitle(agent, view);
  return <button type="button" className="subagent-stop" data-compact={compact || undefined} disabled={stopping} aria-keyshortcuts="Delete Backspace"
    aria-label={stopping ? `Stopping ${noun} ${title}` : `Stop ${noun} ${title}`}
    title={stopping ? `Stopping this ${noun}…` : `Stop this ${noun}; the main agent continues`}
    onClick={event => { event.stopPropagation(); void view.stop(agent); }}>
    <span className="subagent-stop-square" aria-hidden="true"/>{(!compact || stopping) && <span>{stopping ? 'Stopping…' : 'Stop'}</span>}
  </button>;
}

/** Keeps a scroller at its end while the reader is there, as the main chat does. */
function useFollow(ref: RefObject<HTMLElement | null>, change: unknown) {
  const following = useRef(true);
  useLayoutEffect(() => { const node = ref.current; if (node && following.current) node.scrollTop = node.scrollHeight; }, [ref, change]);
  return () => { const node = ref.current; if (node) following.current = node.scrollHeight - node.scrollTop - node.clientHeight < 32; };
}

/**
 * A sub-agent's own steps: its prose, nested sub-agents, and its tool calls
 * as the chat shows them (ToolLine: one dense line each, reads and edits
 * grouped, a failure standing out). In a card only the latest few show
 * (`latest`), the rest on request; in the panel all of them, mounted from the
 * end. While it runs, what it is doing now stays at the bottom: the running
 * call's line, or what it is otherwise busy with.
 */
export function SubagentTimeline({ agent, status, scroller, latest }: { agent: Subagent; status: SubagentStatus; scroller?: RefObject<HTMLElement | null>; latest?: number }) {
  const view = useSubagentView();
  usePresentation(view);
  const [shown, setShown] = useState(TIMELINE_STEP);
  const [all, setAll] = useState(false);
  const active = activeSubagent(status);
  const rows = useMemo(() => stepRows(agent.items), [agent.items]);
  const root = view?.root ?? '', openFile = view?.actions.openFile;
  const scope = useMemo(() => ({ root, onOpenFile: openFile }), [root, openFile]);
  const compact = !!latest && !all;
  const hidden = Math.max(0, rows.length - (compact ? latest : shown));
  const action = active && !agent.runningTool ? currentAction({ ...agent, status }) : undefined;
  const keepPlace = () => {
    // Earlier steps are added above what the reader is looking at: keep it in place.
    const node = scroller?.current, before = node?.scrollHeight ?? 0;
    setShown(count => count + TIMELINE_STEP * 2);
    if (node) requestAnimationFrame(() => { node.scrollTop += node.scrollHeight - before; });
  };
  const steps = rows.length + agent.dropped;
  return <ToolLineContext.Provider value={scope}><ol className="subagent-timeline" data-compact={compact || undefined} aria-label={`Steps of ${subagentTitle(agent, view)}`}>
    {compact && hidden > 0 && <li className="subagent-earlier">
      <button type="button" className="subagent-link" onClick={() => setAll(true)}>Show all {steps.toLocaleString()} steps</button>
    </li>}
    {!compact && (hidden > 0 || agent.dropped > 0 || (latest && rows.length > latest)) && <li className="subagent-earlier">
      {hidden > 0 && <button type="button" className="show-earlier" onClick={keepPlace}>Show earlier steps <small>{hidden.toLocaleString()} more</small></button>}
      {agent.dropped > 0 && hidden === 0 && <small>{agent.dropped.toLocaleString()} earlier {agent.dropped === 1 ? 'step is' : 'steps are'} not kept here; the Activity panel lists every tool call.</small>}
      {!!latest && rows.length > latest && <button type="button" className="subagent-link" onClick={() => setAll(false)}>Show only the latest {latest}</button>}
    </li>}
    {rows.slice(hidden).map(row => {
      const running = active && row.kind === 'tool' && !!agent.runningTool && row.ids.includes(agent.runningTool);
      return <li key={row.id} className={'subagent-step ' + row.kind} data-current={running || undefined}>
        {row.kind === 'message' ? <article className="message message_delta subagent-message"><RichContent text={row.text} onOpenFile={openFile}/></article>
          : row.kind === 'tool' ? <div className="tool-lines" role="list"><ToolItemLine item={row.item}/></div>
          : <SubagentCard id={row.agentId}/>}
      </li>;
    })}
    {action && <li className="subagent-step now" data-current>
      <div className="tool-line" data-state="running"><span className="tool-line-row" role="status" aria-live="off">
        <span className="tool-chevron" aria-hidden="true"/><span className="tool-icon" data-state="running" aria-hidden="true"><span className="tool-spinner"/></span>
        <span className="tool-line-title subagent-now-text">{action.text}</span>
      </span></div>
    </li>}
    {!rows.length && !action && <li className="subagent-empty">{active ? 'Waiting for its first step…' : 'It reported no steps.'}</li>}
  </ol></ToolLineContext.Provider>;
}

/** The instructions it was given: one line, the whole of it on request. */
export function SubagentPrompt({ agent, open = false }: { agent: Subagent; open?: boolean }) {
  if (!agent.prompt) return null;
  return <details className="subagent-prompt" open={open}>
    <summary><span>Instructions</span><small>{agent.prompt.replace(/\s+/g, ' ').slice(0, 160)}</small></summary>
    <div className="subagent-prompt-text"><RichContent text={agent.prompt}/></div>
  </details>;
}

/** How it ended, in a line, and what it handed back. */
export function SubagentOutcome({ view, agent, status }: { view: SubagentView; agent: Subagent; status: SubagentStatus }) {
  const full = view.results.has(agent.id);
  const preview = agent.result ? resultPreview(agent.result) : '';
  const word = status === 'stopped' ? (view.isStopping(agent) ? 'Stopped by you' : 'Stopped') : status === 'done' ? 'Done' : statusWord[status];
  const line = status === 'failed' ? agent.error || agent.summary || preview
    : status === 'unreported' ? preview || agent.summary || (view.gone.has(agent.id) ? 'It was no longer running when you asked to stop it; how it ended was never reported.' : 'Its turn ended, and how it ended was never reported.')
    : preview || agent.summary;
  return <div className="subagent-outcome" data-status={status}>
    <p><strong>{word}</strong>{line && <span className="subagent-outcome-text">{line}</span>}</p>
    {agent.result && <button type="button" className="subagent-link" aria-expanded={full} onClick={() => view.toggleResult(agent.id)}>{full ? 'Hide full result' : 'Show full result'}</button>}
    {full && agent.result && <div className="subagent-result"><RichContent text={agent.result} onOpenFile={view.actions.openFile}/></div>}
  </div>;
}

/**
 * A sub-agent where its parent started it: what it was asked, what it is
 * doing now and for how long, how it ended and what it found. Opening it
 * shows its own timeline.
 */
export const SubagentCard = memo(function SubagentCard({ id }: { id: string }) {
  const view = useSubagentView();
  const agent = useSubagent(view, id);
  const status = useAgentStatus(view, agent);
  const parent = useSubagent(view, agent?.parentId || '');
  const parentStatus = useAgentStatus(view, parent);
  const active = activeSubagent(status);
  const now = useNow(active);
  const bodyId = useId();
  const card = useRef<HTMLElement>(null), scroller = useRef<HTMLDivElement>(null);
  const flash = view?.flash?.agentId === id ? view.flash.nonce : 0;
  const follow = useFollow(scroller, agent?.items);
  useEffect(() => {
    if (!flash || !card.current) return;
    const node = card.current;
    requestAnimationFrame(() => { node.scrollIntoView({ block: 'nearest', behavior: matchMedia('(prefers-reduced-motion: reduce)').matches ? 'auto' : 'smooth' }); node.classList.remove('subagent-flash'); void node.offsetWidth; node.classList.add('subagent-flash'); });
  }, [flash]);
  if (!view || !agent) return null;
  const expanded = view.expanded.has(id);
  // Open, the timeline keeps what it is doing now at its bottom; closed, one line says it.
  const action = active && !expanded ? currentAction({ ...agent, status }) : undefined;
  const title = subagentTitle(agent, view);
  const toggle = () => view.toggle(id);
  const keys = (event: KeyboardEvent) => { if (event.key === 'Escape' && expanded) { event.stopPropagation(); view.toggle(id, false); } };
  return <section ref={card} className="subagent-card" data-status={status} data-nested={agent.depth > 0 || undefined} data-agent-id={id} aria-label={`Sub-agent: ${title}`} onKeyDown={keys}
    onContextMenu={contextMenu<SubagentMenuDetail>('subagent', () => subagentItems(view, agent, title), { agentId: id, kind: agent.kind }, `${title} actions`)}>
    <div className="subagent-head" onKeyDown={stopKeys(view, agent, status)}>
      <button type="button" className="subagent-toggle" aria-expanded={expanded} aria-controls={bodyId} aria-keyshortcuts={view.canStopAgent(agent, status) ? 'Alt+Click Space Delete Backspace' : 'Alt+Click Space'}
        onClick={event => { if (event.altKey) { event.preventDefault(); view.open(id, 'float'); return; } toggle(); }}
        onKeyDown={event => { if (event.key === ' ' && !event.repeat) { event.preventDefault(); view.open(id, 'float'); } }}>
        <SubagentMark status={status}/>
        <SubagentName agent={agent} view={view}/>
        <span className="subagent-chevron" aria-hidden="true"><Icon name="chevronRight" size={14}/></span>
      </button>
      <span className="subagent-side">
        <SubagentStats agent={agent} status={status} now={now}/>
        <span className="subagent-actions">
          <StopSubagent view={view} agent={agent} status={status}/>
          <button type="button" className="icon-button" aria-label={`Open ${title} in a panel`} title="Open in a panel" onClick={() => view.open(id)}><Icon name="dock-right" size={14}/></button>
        </span>
      </span>
    </div>
    {action && <div className="subagent-now" aria-live="off">
      <span className="subagent-now-text" onPointerEnter={titleWhenTruncated(action.text)}>{action.text}</span>
    </div>}
    {active && agent.progress && <p className="subagent-progress" title="The provider's summary of its progress">{agent.progress}</p>}
    {/* Inside a parent's timeline whose own end went unreported too, the parent's outcome says it once. */}
    {!active && !(status === 'unreported' && parentStatus === 'unreported' && !agent.result && !agent.summary) && <SubagentOutcome view={view} agent={agent} status={status}/>}
    {expanded && <div id={bodyId} className="subagent-body" ref={scroller} onScroll={follow}>
      <SubagentPrompt agent={agent}/>
      <SubagentTimeline agent={agent} status={status} scroller={scroller} latest={CARD_STEPS}/>
    </div>}
  </section>;
});
