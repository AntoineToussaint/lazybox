import { useCallback, useEffect, useId, useLayoutEffect, useRef, useState, useSyncExternalStore, type KeyboardEvent as ReactKeyboardEvent, type PointerEvent as ReactPointerEvent } from 'react';
import { createPortal } from 'react-dom';
import type { RunnerClient } from '@lazybox/client-core';
import type { Collection, ItemLink, ProcessInventory, Session, TelemetryReport, WorkspaceGitSummary, WorkspaceRuntime } from '@lazybox/protocol';
import { Icon } from './Icons';
import { LiveIndicator } from './LiveIndicator';
import { useRunner } from './RunnerContext';
import { runAction } from './actions';
import { contextMenuPlatform, copyText } from './ContextMenu';
import { openInTerminal } from './handoff';
import { titleWhenTruncated } from './truncated-title';
import { aheadBehind, agentStateWords, ageLine, costLine, hoverAgents, hoverOrigin, hoverPulls, hoverRepositories, hoverRuns, placeHoverCard, type AgentState, type HoverPlacement, type HoverRepository } from './workspace-hover-model';
import type { LiveTone } from './live-status';
import './workspace-hover.css';

/**
 * The workspace hover card: rest the pointer on a workspace row for a moment
 * (or press ⌥↓ on it) and a card beside the row says what the workspace is —
 * its task, its repositories with branch and changes, its pull requests, its
 * agents, what waits for you, what serves, and what it cost — with a few
 * quick actions. It never covers the row, flips to the other side (or below)
 * near the window's edge, and stays open while the pointer crosses into it,
 * so its links and buttons can be clicked. Esc closes it. The pure half is
 * workspace-hover-model.ts.
 *
 * What the sidebar already holds shows at once; line counts, running
 * commands and usage are read when the card opens (cached briefly), with a
 * placeholder until they answer. Nothing waits on the network to appear.
 */

const OPEN_DELAY_MS = 400;
/** How long the card stays after the pointer leaves the row or the card, so it can cross the gap. */
const GRACE_MS = 220;
/** With a card open, how long the pointer rests on another row before the card moves to it. */
const SWITCH_DELAY_MS = 180;

type Open = { id: string; anchor: HTMLElement; via: 'pointer' | 'keyboard' };
let current: Open | null = null;
let openTimer: ReturnType<typeof setTimeout> | undefined, closeTimer: ReturnType<typeof setTimeout> | undefined;
/** A row just clicked does not reopen its card until the pointer leaves it. */
let suppressed = '';
const listeners = new Set<() => void>();
const emit = () => { for (const listener of listeners) listener(); };
const subscribe = (listener: () => void) => { listeners.add(listener); return () => listeners.delete(listener); };
const set = (next: Open | null) => { clearTimeout(openTimer); clearTimeout(closeTimer); if (next?.id === current?.id && next?.anchor === current?.anchor && next?.via === current?.via) return; current = next; emit(); };

/** Closes whichever card is open. */
function closeWorkspaceHover() { set(null); }
const keep = () => clearTimeout(closeTimer);
const leave = () => { clearTimeout(openTimer); clearTimeout(closeTimer); if (current) closeTimer = setTimeout(() => set(null), GRACE_MS); };

/**
 * The row's half: the props that open the card on rest and on ⌥↓ (on the
 * row's link), the ref of the row the card sits beside, and whether this
 * row's card is the one open.
 */
export function useWorkspaceHover(id: string) {
  // Only the row whose card opens or closes renders again.
  const state = useSyncExternalStore(subscribe, () => current?.id === id ? current : null);
  // The same workspace can be listed twice (the sidebar, and the navigator over it): the card is the row's it was opened from.
  const row = useRef<HTMLDivElement>(null);
  const open = state?.id === id && state.anchor === row.current ? state : null;
  const props = {
    'aria-keyshortcuts': 'Alt+ArrowDown',
    onPointerEnter(event: ReactPointerEvent<HTMLElement>) {
      if (event.pointerType !== 'mouse' || suppressed === id) return;
      clearTimeout(openTimer);
      if (current?.id === id) { keep(); return; }
      const anchor = rowOf(event.currentTarget);
      openTimer = setTimeout(() => { if (anchor.isConnected) set({ id, anchor, via: 'pointer' }); }, current ? SWITCH_DELAY_MS : OPEN_DELAY_MS);
    },
    onPointerLeave() { if (suppressed === id) suppressed = ''; leave(); },
    onPointerDown() { suppressed = id; set(null); },
    onKeyDown(event: ReactKeyboardEvent<HTMLElement>) {
      if (event.altKey && event.key === 'ArrowDown' && !event.metaKey && !event.ctrlKey) {
        event.preventDefault(); event.stopPropagation();
        set({ id, anchor: rowOf(event.currentTarget), via: 'keyboard' });
        return;
      }
      if (event.key === 'Escape' && current?.id === id) { event.preventDefault(); event.stopPropagation(); set(null); return; }
      // Moving on (j/k, Space to peek, Enter): the card of the row left behind goes.
      if (current?.id === id && current.via === 'pointer' && !['Shift', 'Alt', 'Meta', 'Control'].includes(event.key)) set(null);
    },
  };
  return { open, props, row };
}
const rowOf = (element: HTMLElement) => element.closest<HTMLElement>('.workspace-row') || element;

// Data read when a card opens: kept 30 seconds per Runner client, shown at once on the next open and refreshed.
const FRESH_MS = 30_000;
type Part<T> = { value?: T | null; at: number; pending?: Promise<void> };
type Details = { counted: Part<WorkspaceGitSummary>; runtime: Part<WorkspaceRuntime>; telemetry: Part<TelemetryReport> };
const caches = new WeakMap<object, { workspaces: Map<string, Details>; processes: Part<ProcessInventory> }>();
const cacheOf = (api: RunnerClient) => { let cache = caches.get(api); if (!cache) { cache = { workspaces: new Map(), processes: { at: 0 } }; caches.set(api, cache); } return cache; };
function refresh<T>(part: Part<T>, read: () => Promise<T | null>, changed: () => void) {
  if (part.pending || Date.now() - part.at < FRESH_MS) return;
  // An answer that fails is "none": the card leaves that part out rather than show an error on a hover.
  part.pending = read().catch(() => null).then(value => { part.value = value; part.at = Date.now(); part.pending = undefined; changed(); });
}

/** What the card reads when it opens: line counts, running actions and processes, usage. undefined until each answers. */
export function useWorkspaceDetails(workspaceId: string) {
  const { api } = useRunner();
  const [, setVersion] = useState(0);
  const cache = cacheOf(api);
  let details = cache.workspaces.get(workspaceId);
  if (!details) { details = { counted: { at: 0 }, runtime: { at: 0 }, telemetry: { at: 0 } }; cache.workspaces.set(workspaceId, details); }
  useEffect(() => {
    let live = true;
    const changed = () => { if (live) setVersion(v => v + 1); };
    const d = cache.workspaces.get(workspaceId)!;
    const options = { timeoutMs: 10_000 };
    refresh(d.counted, () => api.listWorkspaceGitSummaries({ collectionIds: [workspaceId], changes: true }, options).then(answer => answer.workspaces.find(w => w.collectionId === workspaceId) || null), changed);
    refresh(d.runtime, () => api.getWorkspaceRuntime({ collectionId: workspaceId }, options), changed);
    refresh(d.telemetry, () => api.getTelemetry({ workspace: workspaceId }, options), changed);
    refresh(cache.processes, () => api.listProcesses({}, options), changed);
    // Answers already on their way when the card opened again.
    for (const part of [d.counted, d.runtime, d.telemetry, cache.processes]) void part.pending?.then(changed);
    return () => { live = false; };
  }, [api, cache, workspaceId]);
  return { counted: details.counted.value, runtime: details.runtime.value, telemetry: details.telemetry.value, processes: cache.processes.value };
}

export type WorkspaceHoverProps = {
  open: Open;
  workspace: Collection;
  /** The sidebar's labels of its repositories, in its members' order. */
  repositoryNames: readonly string[];
  sessions: readonly Session[];
  summary?: WorkspaceGitSummary;
  links: readonly ItemLink[];
  onOpen(): void;
  onNewConversation(): void;
  onOpenConversation(sessionId: string): void;
};

/** The card, beside its row, in the window's top layer (inside the row's dialog, when it is in one, so a modal does not make it inert). */
export function WorkspaceHoverCard({ open, workspace, repositoryNames, sessions, summary, links, onOpen, onNewConversation, onOpenConversation }: WorkspaceHoverProps) {
  const { info } = useRunner();
  const card = useRef<HTMLDivElement>(null);
  const titleId = useId();
  const [placement, setPlacement] = useState<HoverPlacement | null>(null);
  const details = useWorkspaceDetails(workspace.id);
  const now = Date.now();

  const place = useCallback(() => {
    const node = card.current;
    if (!node || !open.anchor.isConnected) return;
    const row = open.anchor.getBoundingClientRect();
    setPlacement(placeHoverCard(row, { width: node.offsetWidth, height: node.scrollHeight }, { width: window.innerWidth, height: window.innerHeight }));
  }, [open.anchor]);
  useLayoutEffect(() => {
    const node = card.current;
    if (!node) return;
    try { if (!node.matches(':popover-open')) node.showPopover(); } catch { /* not in the document yet */ }
    place();
    // Line counts and usage arrive after it opened: it stays in place, whole inside the window.
    const observer = new ResizeObserver(place);
    observer.observe(node);
    return () => observer.disconnect();
  }, [place]);
  // Opened from the keyboard, the card takes focus once it is placed (a hidden one cannot).
  const placed = !!placement;
  useEffect(() => {
    if (open.via === 'keyboard' && placed) card.current?.focus({ preventScroll: true });
  }, [open.via, placed]);
  useEffect(() => {
    const outside = (event: Event) => { const target = event.target as Node | null; if (target && !card.current?.contains(target) && !open.anchor.contains(target)) closeWorkspaceHover(); };
    const scrolled = (event: Event) => { const target = event.target; if (target instanceof Node && !card.current?.contains(target) && target.contains(open.anchor)) closeWorkspaceHover(); };
    // Esc closes the card first, as it would a menu: whatever is under it (a navigator, a dialog) stays.
    const key = (event: KeyboardEvent) => { if (event.key === 'Escape' && !event.defaultPrevented && !card.current?.contains(event.target as Node)) { event.preventDefault(); event.stopPropagation(); closeWorkspaceHover(); } };
    document.addEventListener('pointerdown', outside, true);
    document.addEventListener('contextmenu', closeWorkspaceHover, true);
    window.addEventListener('keydown', key, true);
    window.addEventListener('scroll', scrolled, true);
    window.addEventListener('resize', place);
    window.addEventListener('blur', closeWorkspaceHover);
    return () => {
      document.removeEventListener('pointerdown', outside, true);
      document.removeEventListener('contextmenu', closeWorkspaceHover, true);
      window.removeEventListener('keydown', key, true);
      window.removeEventListener('scroll', scrolled, true);
      window.removeEventListener('resize', place);
      window.removeEventListener('blur', closeWorkspaceHover);
    };
  }, [open.anchor, open.via, place]);

  const back = () => { closeWorkspaceHover(); open.anchor.querySelector<HTMLElement>('.workspace-link')?.focus(); };
  const act = (run: () => void) => () => { closeWorkspaceHover(); run(); };
  const origin = hoverOrigin(workspace, links);
  const age = ageLine(workspace, sessions, now);
  const repositories = hoverRepositories(workspace.members.map(m => m.spec?.name || ''), repositoryNames, summary, details.counted);
  const pulls = hoverPulls(workspace.id, links, details.counted || summary, now);
  const agents = hoverAgents(sessions, info?.profiles || []);
  const runs = details.runtime === undefined || details.processes === undefined ? undefined : hoverRuns(workspace.id, details.runtime?.actions, details.processes?.processes);
  const cost = costLine(details.telemetry || undefined);
  const ready = workspace.state === 'ready' && !workspace.ownershipUncertain;
  const reveal = contextMenuPlatform().revealInFinder;
  const isMac = typeof navigator !== 'undefined' && /Mac/.test(navigator.platform);

  return createPortal(<div
    ref={card}
    popover="manual"
    className="workspace-hover"
    role="dialog"
    aria-labelledby={titleId}
    tabIndex={-1}
    data-side={placement?.side}
    style={placement ? { left: placement.left, top: placement.top, maxHeight: placement.maxHeight } : { visibility: 'hidden' }}
    onPointerEnter={keep}
    onPointerLeave={event => { if (event.pointerType === 'mouse' && open.via === 'pointer') leave(); }}
    onKeyDown={event => {
      if (event.key === 'Escape') { event.preventDefault(); event.stopPropagation(); back(); return; }
      // Tab goes round the card's links and buttons, in order, whatever the engine makes of a popover's focus order.
      if (event.key === 'Tab' && !event.altKey && !event.metaKey && !event.ctrlKey) {
        const items = [...(card.current?.querySelectorAll<HTMLElement>('a[href], button:not(:disabled)') || [])];
        if (!items.length) return;
        event.preventDefault();
        const at = items.indexOf(document.activeElement as HTMLElement);
        items[at < 0 ? (event.shiftKey ? items.length - 1 : 0) : (at + (event.shiftKey ? -1 : 1) + items.length) % items.length].focus();
      }
    }}
    onBlur={event => { const next = event.relatedTarget as Node | null; if (next && !card.current?.contains(next) && !open.anchor.contains(next)) closeWorkspaceHover(); }}
  >
    <header className="workspace-hover-header">
      <h3 id={titleId} onPointerEnter={titleWhenTruncated(workspace.name)}>{workspace.name}</h3>
      {origin && (origin.url
        ? <a className="workspace-hover-origin" href={origin.url} target="_blank" rel="noreferrer" title={origin.title}><Icon name={origin.icon} size={12}/><span>{origin.text}</span><Icon name="external" size={10}/></a>
        : <span className="workspace-hover-origin" title={origin.title}><Icon name={origin.icon} size={12}/><span>{origin.text}</span></span>)}
      {age && <p className="workspace-hover-age">{age}</p>}
    </header>

    {agents.waiting > 0 && (
      <button type="button" className="workspace-hover-waiting" onClick={act(() => runAction('inbox.open'))} aria-label={`${agents.waiting} waiting for you: open the Inbox`}>
        <LiveIndicator tone="waiting"/>
        <span>{agents.waiting} waiting for you</span>
        <span className="workspace-hover-go">Inbox<Icon name="chevronRight" size={11}/></span>
      </button>
    )}

    <section className="workspace-hover-section" aria-label="Repositories">
      <h4>Repositories</h4>
      {repositories.length ? <ul className="workspace-hover-repos">{repositories.map(repo => <RepositoryLine key={repo.member} repo={repo}/>)}</ul>
        : <p className="workspace-hover-none">No repositories yet</p>}
    </section>

    {pulls.length > 0 && (
      <section className="workspace-hover-section" aria-label="Pull requests">
        <h4>Pull requests</h4>
        <ul className="workspace-hover-pulls">{pulls.slice(0, 4).map(pull => (
          <li key={pull.key}>
            <a href={pull.url || undefined} target="_blank" rel="noreferrer" aria-label={pull.label}>
              <span className={`pr-dot is-${pull.tone}`} aria-hidden="true"/>
              <span className="workspace-hover-number">#{pull.number}</span>
              <span className="workspace-hover-pull-title" onPointerEnter={titleWhenTruncated(pull.title)}>{pull.title || pull.repository}</span>
            </a>
            <span className="workspace-hover-pills" aria-hidden="true">{pull.pills.map(pill => <span key={pill.text} className={`workspace-hover-pill is-${pill.tone}`}>{pill.icon && <Icon name={pill.icon} size={10}/>}{pill.text}</span>)}</span>
          </li>
        ))}</ul>
        {pulls.length > 4 && <button type="button" className="workspace-hover-more" onClick={act(() => runAction('pulls.open'))}>+{pulls.length - 4} more</button>}
      </section>
    )}

    <section className="workspace-hover-section" aria-label="Agents">
      <h4>Agents</h4>
      {agents.shown.length ? <ul className="workspace-hover-agents">{agents.shown.map(agent => (
        <li key={agent.id}>
          <button type="button" onClick={act(() => onOpenConversation(agent.id))} aria-label={`${agent.title}, ${agentStateWords[agent.state]}${agent.detail ? `, ${agent.detail}` : ''}: open it`}>
            <LiveIndicator tone={agentTone[agent.state]}/>
            <span className="workspace-hover-agent-text">
              <span className="workspace-hover-agent-title" onPointerEnter={titleWhenTruncated(agent.title)}>{agent.title}</span>
              {agent.detail && <small>{agent.detail}</small>}
            </span>
            <span className="workspace-hover-state" data-state={agent.state}>{agentStateWords[agent.state]}</span>
          </button>
        </li>
      ))}</ul> : <p className="workspace-hover-none">No conversation yet</p>}
      {agents.more > 0 && <button type="button" className="workspace-hover-more" onClick={act(onOpen)}>+{agents.more} more</button>}
    </section>

    {(runs === undefined || runs.length > 0) && (
      <section className="workspace-hover-section" aria-label="Running">
        <h4>Running</h4>
        {runs === undefined ? <span className="workspace-hover-skeleton is-wide" aria-label="Reading what runs"/> : <ul className="workspace-hover-runs">{runs.map(run => {
          const text = <><Icon name="play" size={10}/><span className="workspace-hover-run-name">{run.name}</span>{run.ports.map(port => <span key={port} className="workspace-hover-port">:{port}</span>)}</>;
          return <li key={run.key}>{run.url ? <a href={run.url} target="_blank" rel="noreferrer" aria-label={`${run.label}: open it`}>{text}</a> : <span aria-label={run.label}>{text}</span>}</li>;
        })}</ul>}
      </section>
    )}

    <footer className="workspace-hover-actions">
      <button type="button" className="workspace-hover-open" aria-label={`Open ${workspace.name}`} onClick={act(onOpen)}>Open</button>
      <button type="button" className="icon-button" aria-label="New conversation" title="New conversation" disabled={!ready} onClick={act(onNewConversation)}><Icon name="plus" size={14}/></button>
      <button type="button" className="icon-button" aria-label="Open in Terminal" title="Open in Terminal" disabled={!workspace.root} onClick={act(() => void openInTerminal(workspace.root))}><Icon name="terminal" size={14}/></button>
      {reveal && <button type="button" className="icon-button" aria-label={isMac ? 'Reveal in Finder' : 'Show in file manager'} title={isMac ? 'Reveal in Finder' : 'Show in file manager'} disabled={!workspace.root} onClick={act(() => void reveal(workspace.root))}><Icon name="folder" size={14}/></button>}
      <button type="button" className="icon-button" aria-label="Copy path" title={workspace.root ? `Copy path\n${workspace.root}` : 'Copy path'} disabled={!workspace.root} onClick={act(() => void copyText(workspace.root, 'Copied path'))}><Icon name="copy" size={14}/></button>
      {cost && <span className="workspace-hover-cost" title={cost.label}>{cost.text}</span>}
    </footer>
  </div>, open.anchor.closest('dialog') || document.body);
}

const agentTone: Record<AgentState, LiveTone> = { 'needs-you': 'waiting', failed: 'failed', working: 'busy', done: 'idle' };

/** One repository: its name and how far it is from the default branch; its branch and what it changes. */
function RepositoryLine({ repo }: { repo: HoverRepository }) {
  const level = aheadBehind(repo);
  const changes = repo.changes;
  return <li>
    <div className="workspace-hover-repo-line">
      <Icon name="repository" size={12}/>
      <span className="workspace-hover-repo-name" onPointerEnter={titleWhenTruncated(repo.name)}>{repo.owner && <span className="workspace-hover-owner">{repo.owner}/</span>}{repo.short}</span>
      {level.text && <span className="workspace-hover-level" title={level.label} aria-label={level.label}>{level.text}</span>}
    </div>
    <div className="workspace-hover-repo-line is-detail">
      {repo.error ? <span className="workspace-hover-problem"><Icon name="alert" size={11}/>{repo.error}</span> : <>
        <Icon name="branch" size={11}/>
        <span className="workspace-hover-branch" onPointerEnter={titleWhenTruncated(repo.branch)}>{repo.branch || 'Reading…'}</span>
        {changes === undefined
          ? <span className="workspace-hover-skeleton" aria-label="Counting changes"/>
          : changes
            ? <span className={`workspace-hover-changes${changes.clean ? ' is-clean' : ''}`} aria-label={changes.label}>{changes.clean ? 'clean' : <>
              <span className="is-add">+{changes.additions}{changes.floor ? '+' : ''}</span> <span className="is-del">−{changes.deletions}{changes.floor ? '+' : ''}</span>
              <span className="workspace-hover-files"> · {changes.files}{changes.floor ? '+' : ''} {changes.files === 1 ? 'file' : 'files'}</span></>}</span>
            // An older Runner counts no lines: what the sidebar knows, in words.
            : <span className={`workspace-hover-changes${repo.dirty ? '' : ' is-clean'}`}>{repo.dirty ? `${repo.dirty} uncommitted` : 'clean'}</span>}
      </>}
    </div>
  </li>;
}
