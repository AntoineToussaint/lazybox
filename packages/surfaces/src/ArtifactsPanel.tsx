import { peekProps } from './peek-model';
import { useCallback, useContext, useEffect, useMemo, useRef, useState, type KeyboardEvent as ReactKeyboardEvent } from 'react';
import type { Artifact, Collection } from '@lazybox/protocol';
import type { ViewStateStore } from '@lazybox/platform';
import { ArtifactHostContext, ArtifactPage } from './ArtifactPage';
import { ContextMenu, menuPoint, type ContextMenuState } from './ContextMenu';
import { DeleteArtifactDialog } from './ArtifactDialogs';
import { ago, asKind, authorShort, fromConversation, groupByWorkspace, kindIcon, kindLabel, updatedLine, updatedSinceSeen, type ConversationRef } from './artifact-model';
import { useArtifactSelection, type OpenArtifactRequest } from './artifact-events';
import { useArtifacts, useArtifactsOf } from './useArtifacts';
import { errorText, restoreArtifacts, retireArtifact } from './artifact-client';
import { useRunner } from './RunnerContext';
import { useWorkConversation } from './PullRequestFlow';
import { Icon } from './Icons';
import type { ViewTarget } from './workspace-model';
import { Resizer } from './Resizer';
import { SPLITS } from './resizable-model';
import './artifacts.css';

/**
 * The latest version the person saw of each artifact in a workspace, per
 * device: it puts the "updated since you looked" dot on a row.
 */
function useSeenVersions(viewState: ViewStateStore | undefined, workspaceId: string) {
  const key = `${viewState?.key || 'lazybox'}:artifact-seen:${workspaceId}`;
  const read = useCallback((): Record<string, string> => { try { return JSON.parse((viewState ? viewState.getItem(key) : localStorage.getItem(key)) || '{}') || {}; } catch { return {}; } }, [viewState, key]);
  const [seen, setSeen] = useState(read);
  useEffect(() => setSeen(read()), [read]);
  const markSeen = useCallback((artifact: Artifact) => {
    const at = artifact.versions.at(-1)?.createdAtUnixMillis;
    if (at === undefined) return;
    setSeen(current => {
      if (current[artifact.id] === String(at)) return current;
      const next = { ...current, [artifact.id]: String(at) };
      try { const text = JSON.stringify(next); if (viewState) viewState.setItem(key, text); else localStorage.setItem(key, text); } catch { /* a convenience */ }
      return next;
    });
  }, [viewState, key]);
  return { seen, markSeen };
}

type Scope = 'workspace' | 'all';
const plural = (n: number, noun: string) => `${n} ${noun}${n === 1 ? '' : 's'}`;
const matches = (artifact: Artifact, q: string) => !q || artifact.title.toLowerCase().includes(q) || artifact.kind.includes(q) || artifact.trackedPath.toLowerCase().includes(q);

/**
 * The Artifacts panel: a workspace's plans, reports, specs and notes, or
 * every workspace's ("All workspaces"), grouped by workspace. The list and
 * the open page sit side by side when the panel is wide, and one at a time
 * when it is narrow. The chat header's chip opens it on its conversation's
 * artifacts.
 */
export function ArtifactsPanel({ workspaceId, collections, viewState, target, onShowSession, onCurrentChange }: { workspaceId: string; collections: Collection[]; viewState?: ViewStateStore; target: ViewTarget; onShowSession?(target: ViewTarget): void; /** The artifact open in the panel ('' for the list), as it changes. */ onCurrentChange?(collectionId: string, artifactId: string): void }) {
  const key = viewState ? `${viewState.key}:artifact:${workspaceId}` : '';
  const scopeKey = viewState ? `${viewState.key}:artifact-scope` : '';
  const retiredKey = viewState ? `${viewState.key}:artifact-retired` : '';
  // Retired artifacts are out of the list until asked for ("Show retired (N)"); the choice is kept per panel.
  const [showRetired, setShowRetiredState] = useState(() => { try { return !!retiredKey && viewState!.getItem(retiredKey) === 'shown'; } catch { return false; } });
  const setShowRetired = (next: boolean) => { setShowRetiredState(next); try { if (retiredKey) viewState!.setItem(retiredKey, next ? 'shown' : 'hidden'); } catch { /* a convenience */ } };
  const [selected, setSelectedState] = useState(() => { try { return key ? viewState!.getItem(key) || '' : ''; } catch { return ''; } });
  const setSelected = (id: string) => { setElsewhere(undefined); setSelectedState(id); try { if (key) viewState!.setItem(key, id); } catch { /* a convenience */ } };
  const [history, setHistory] = useState<{ collectionId: string; id: string; version: number }[]>([]);
  const [scope, setScopeState] = useState<Scope>(() => { try { return scopeKey && viewState!.getItem(scopeKey) === 'all' ? 'all' : 'workspace'; } catch { return 'workspace'; } });
  const setScope = (next: Scope) => { setScopeState(next); if (next === 'all') setFromConversation(undefined); else setElsewhere(undefined); try { if (scopeKey) viewState!.setItem(scopeKey, next); } catch { /* a convenience */ } };
  // Only this conversation's artifacts, when the chat header's chip asked for them.
  const [fromChat, setFromConversation] = useState<ConversationRef & { title?: string }>();
  const [focus, setFocus] = useState<{ artifactId: string; version?: number; pinned?: boolean; compareWith?: number; at: number }>();
  const { artifacts, retired, loaded, error } = useArtifacts(workspaceId);
  const { api } = useRunner();
  const [actionError, setActionError] = useState('');
  const [elsewhere, setElsewhere] = useState<{ collectionId: string; id: string }>();
  // Every workspace is read when it is asked for, and to say where artifacts are when this workspace has none.
  const several = collections.filter(c => c.id).length > 1;
  const wantAll = several && (scope === 'all' || (loaded && !artifacts.length));
  const everywhere = useArtifactsOf(wantAll ? collections : []);
  const others = useMemo(() => everywhere.items.filter(item => item.collectionId !== workspaceId), [everywhere.items, workspaceId]);
  const [query, setQuery] = useState('');
  const [menu, setMenu] = useState<ContextMenuState>(), [deleting, setDeleting] = useState<Artifact>();
  const host = useContext(ArtifactHostContext);
  const list = useRef<HTMLDivElement>(null);
  useArtifactSelection(workspaceId, (request: OpenArtifactRequest) => {
    if (request.conversation || !request.artifactId) {
      setScope('workspace');
      setFromConversation(request.conversation);
      setQuery('');
    }
    setHistory([]);
    setSelected(request.artifactId);
    setFocus(request.artifactId && (request.version || request.compareWith) ? { artifactId: request.artifactId, version: request.version, compareWith: request.compareWith, at: Date.now() } : undefined);
  });
  const conversation = useWorkConversation(workspaceId, target.collectionId === workspaceId ? target.sessionId : '');
  const collection = collections.find(value => value.id === workspaceId);
  const q = query.trim().toLowerCase();
  const all = scope === 'all' && several;
  const here = useMemo(() => artifacts.filter(a => matches(a, q) && (!fromChat || fromConversation(a, fromChat))), [artifacts, q, fromChat]);
  const hereRetired = useMemo(() => showRetired ? retired.filter(a => matches(a, q) && (!fromChat || fromConversation(a, fromChat))) : [], [showRetired, retired, q, fromChat]);
  const groups = useMemo(() => all ? groupByWorkspace(everywhere.items.filter(item => matches(item.artifact, q))) : [], [all, everywhere.items, q]);
  const total = all ? everywhere.items.length : artifacts.length;
  const other = elsewhere ? others.find(o => o.collectionId === elsewhere.collectionId && o.artifact.id === elsewhere.id) : undefined;
  // A retired artifact stays readable: chosen from "Show retired", a chat card or a banner, it opens with its banner.
  const current = other ? other.artifact : artifacts.find(a => a.id === selected) || retired.find(a => a.id === selected);
  const currentCollectionId = other ? other.collectionId : workspaceId;
  const { seen, markSeen } = useSeenVersions(viewState, workspaceId);
  // Opening an artifact, or reading it while a new version arrives, is looking at it.
  useEffect(() => { if (current && !other) markSeen(current); }, [current, other, markSeen]);
  useEffect(() => { onCurrentChange?.(currentCollectionId, current?.id || ''); }, [currentCollectionId, current?.id]);
  if (!workspaceId) return <p className="module-empty">Choose a workspace to see its artifacts.</p>;
  const focusRow = (id: string) => requestAnimationFrame(() => list.current?.querySelector<HTMLElement>(`[data-artifact-id="${CSS.escape(id)}"]`)?.focus());
  const navigate = (collectionId: string, id: string, version?: number) => {
    if (collectionId === workspaceId) setSelected(id);
    else setElsewhere({ collectionId, id });
    setFocus(version ? { artifactId: id, version, pinned: true, at: Date.now() } : undefined);
  };
  const choose = (collectionId: string, id: string) => { setHistory([]); navigate(collectionId, id); };
  const back = () => {
    const previous = history.at(-1);
    if (previous) {
      setHistory(items => items.slice(0, -1));
      navigate(previous.collectionId, previous.id, previous.version);
    } else {
      if (other) setElsewhere(undefined); else setSelected('');
      if (current) focusRow(current.id);
    }
  };
  // Retire and Restore are reversible and keep every version, so neither asks first.
  const setRetired = (artifact: Artifact, retire: boolean) => {
    setActionError('');
    void (retire ? retireArtifact(api, artifact.collectionId, artifact.id) : restoreArtifacts(api, artifact.collectionId, [artifact.id])).catch(e => setActionError(errorText(e)));
  };
  const rowMenu = (artifact: Artifact, point: { x: number; y: number }, returnFocus: HTMLElement | null) => setMenu({ ...point, label: `${artifact.title} actions`, returnFocus, items: [
    { label: 'Open', icon: 'artifact', run: () => choose(artifact.collectionId, artifact.id) },
    ...(host.openArtifact ? [{ label: 'Open in its own window', icon: 'external', run: () => void host.openArtifact!({ collectionId: artifact.collectionId, artifactId: artifact.id }) }] : []),
    ...(artifact.collectionId === workspaceId ? [
      artifact.retired ? { label: 'Restore', icon: 'history', separatorBefore: true, run: () => setRetired(artifact, false) } : { label: 'Retire', icon: 'archive', separatorBefore: true, run: () => setRetired(artifact, true) },
      { label: 'Delete…', icon: 'cross', danger: true, run: () => setDeleting(artifact) },
    ] : []),
  ] });
  // ↑/↓ (or j/k) move through every row shown, across workspaces.
  const moveFocus = (event: ReactKeyboardEvent) => {
    const step = event.key === 'ArrowDown' || event.key === 'j' ? 1 : event.key === 'ArrowUp' || event.key === 'k' ? -1 : 0;
    const rows = [...list.current?.querySelectorAll<HTMLElement>('[role=option]') || []];
    if (!step || !rows.length) return;
    event.preventDefault();
    const index = rows.indexOf(document.activeElement as HTMLElement);
    rows[Math.max(0, Math.min(rows.length - 1, index + step))].focus();
  };
  const isActive = (artifact: Artifact) => artifact.collectionId === workspaceId ? !other && artifact.id === selected : elsewhere?.collectionId === artifact.collectionId && elsewhere.id === artifact.id;
  // One row is in the Tab order: the open one when it is listed, else the first (a filter, or the scope, can hide the open one).
  const activeListed = (all ? groups.flatMap(group => group.items.map(item => item.artifact)) : [...here, ...hereRetired]).some(isActive);
  const row = (artifact: Artifact, first: boolean) => {
    const kind = asKind(artifact.kind), mine = artifact.collectionId === workspaceId;
    const active = isActive(artifact);
    const unread = mine && !active && !artifact.retired && updatedSinceSeen(artifact, seen[artifact.id]);
    const successor = artifact.retired?.supersededBy ? [...artifacts, ...retired].find(a => a.id === artifact.retired!.supersededBy) : undefined;
    return <li key={artifact.collectionId + '/' + artifact.id} role="option" aria-selected={active} data-artifact-id={artifact.id} {...peekProps({ kind: 'artifact', workspaceId: artifact.collectionId, artifactId: artifact.id, title: artifact.title })} tabIndex={active || (!activeListed && first) ? 0 : -1}
      className={`artifact-row${unread ? ' is-unread' : ''}${artifact.retired ? ' is-retired' : ''}`} onClick={() => choose(artifact.collectionId, artifact.id)}
      onKeyDown={event => { if (event.key === 'Enter' || event.key === ' ') { event.preventDefault(); choose(artifact.collectionId, artifact.id); } if (mine && (event.key === 'Delete' || event.key === 'Backspace')) { event.preventDefault(); setDeleting(artifact); } if (event.key === 'ContextMenu' || (event.shiftKey && event.key === 'F10')) { event.preventDefault(); const r = event.currentTarget.getBoundingClientRect(); rowMenu(artifact, { x: r.left + 24, y: r.bottom - 4 }, event.currentTarget); } }}
      onContextMenu={event => { if (event.shiftKey) return; event.preventDefault(); rowMenu(artifact, menuPoint(event), event.currentTarget); }}>
      <span className={`artifact-kind-icon kind-${kind}`} title={kindLabel[kind]}><Icon name={kindIcon[kind]} size={16}/></span>
      <span className="artifact-row-text">
        <strong>{artifact.title}</strong>
        <small title={updatedLine(artifact)}>{kindLabel[kind]} · v{artifact.versions.at(-1)?.number ?? 1} · {authorShort(artifact.versions.at(-1))}</small>
        {artifact.retired && <small className="artifact-row-retired"><Icon name="archive" size={11}/>{successor ? `Retired · superseded by “${successor.title}”` : 'Retired'}</small>}
        {(artifact.trackedPath || artifact.trackingProblem) && <small className={artifact.trackingProblem ? 'artifact-row-problem' : 'artifact-row-file'}>{artifact.trackingProblem ? <><Icon name="alert" size={11}/>{artifact.trackingProblem}</> : <><Icon name="refresh" size={11}/>{artifact.trackedPath}</>}</small>}
      </span>
      <span className="artifact-row-aside">
        <time dateTime={new Date(Number(artifact.updatedAtUnixMillis)).toISOString()}>{ago(artifact.updatedAtUnixMillis)}</time>
        {unread && <span className="artifact-unread" role="img" aria-label="Updated since you last looked" title="Updated since you last looked"/>}
      </span>
    </li>;
  };
  const shownCount = all ? groups.reduce((n, g) => n + g.items.length, 0) : here.length;
  const nothingAnywhere = loaded && !artifacts.length && !retired.length && (!several || (everywhere.loaded && !others.length));
  const onlyRetired = loaded && !artifacts.length && retired.length > 0;
  const heading = !loaded ? 'Loading…' : all ? `${plural(total, 'artifact')}${groups.length > 1 ? ` in ${groups.length} workspaces` : ''}` : fromChat ? `${plural(here.length, 'artifact')} from this conversation` : plural(artifacts.length, 'artifact');
  return <div className={`artifacts-panel${current ? ' has-selection' : ''}`}>
    <section className="artifacts-list" aria-label="Artifacts">
      <header className="artifacts-list-header">
        <strong>{heading}</strong>
        {(total > 4 || query) && <label className="artifacts-search"><Icon name="search" size={13}/><input type="search" placeholder="Filter" aria-label="Filter artifacts" value={query} onChange={event => setQuery(event.target.value)}/></label>}
        {several && <div className="artifacts-scope" role="radiogroup" aria-label="Show artifacts of" onKeyDown={event => { if (event.key === 'ArrowLeft' || event.key === 'ArrowRight') { event.preventDefault(); const next = scope === 'all' ? 'workspace' : 'all'; setScope(next); (event.currentTarget.querySelector(`[data-scope="${next}"]`) as HTMLElement | null)?.focus(); } }}>
          {(['workspace', 'all'] as const).map(value => <button key={value} type="button" role="radio" data-scope={value} aria-checked={scope === value} tabIndex={scope === value ? 0 : -1} onClick={() => setScope(value)}>{value === 'all' ? 'All workspaces' : 'This workspace'}</button>)}
        </div>}
      </header>
      {fromChat && !all && <div className="artifacts-conversation-filter" role="status">
        <Icon name="chat" size={12}/><span>From {fromChat.title ? `“${fromChat.title}”` : 'this conversation'}</span>
        <button type="button" className="icon-button" aria-label="Show every artifact of this workspace" title="Show every artifact of this workspace" onClick={() => setFromConversation(undefined)}><Icon name="cross" size={12}/></button>
      </div>}
      {(error || actionError) && <p className="artifact-error" role="alert">{error || actionError}</p>}
      {nothingAnywhere && !error && <div className="artifacts-empty">
        <Icon name="artifact" size={26}/>
        <strong>No artifacts yet</strong>
        <p>Plans and reports show up here when an agent publishes one, or when you right-click a message or a Markdown file → <em>Track as artifact</em>.</p>
      </div>}
      {!all && onlyRetired && !showRetired && !error && <div className="artifacts-empty">
        <Icon name="archive" size={26}/>
        <strong>Every artifact here is retired</strong>
        <p>Retired artifacts keep their versions and can be restored.</p>
      </div>}
      {!all && loaded && !artifacts.length && !retired.length && !error && others.length > 0 && <div className="artifacts-empty">
        <Icon name="artifact" size={26}/>
        <strong>None in this workspace</strong>
        <p>{plural(others.length, 'artifact')} in other workspaces.</p>
        <button type="button" onClick={() => setScope('all')}>Show all workspaces</button>
      </div>}
      <div ref={list} role="listbox" aria-label={all ? 'Artifacts of every workspace' : 'Artifacts'} className="artifacts-rows" onKeyDown={moveFocus}>
        {all
          ? groups.map((group, g) => <ul key={group.collectionId} role="group" aria-label={group.workspace} className="artifacts-group">
            <li role="presentation" className="artifacts-group-name"><Icon name="collections" size={11}/>{group.workspace}{group.collectionId === workspaceId && <small>This workspace</small>}</li>
            {group.items.map((item, i) => row(item.artifact, g === 0 && i === 0))}
          </ul>)
          : <>
            <ul role="presentation" className="artifacts-group">{here.map((artifact, i) => row(artifact, i === 0))}</ul>
            {hereRetired.length > 0 && <ul role="group" aria-label="Retired" className="artifacts-group artifacts-retired">
              <li role="presentation" className="artifacts-group-name"><Icon name="archive" size={11}/>Retired</li>
              {hereRetired.map((artifact, i) => row(artifact, !here.length && i === 0))}
            </ul>}
          </>}
        {(q || fromChat) && !shownCount && !hereRetired.length && total > 0 && <p className="artifacts-none">{q ? `No artifact matches “${query}”.` : 'None from this conversation.'}</p>}
      </div>
      {!all && retired.length > 0 && <button type="button" className="link-button artifacts-retired-toggle" aria-pressed={showRetired} onClick={() => setShowRetired(!showRetired)}>
        <Icon name="archive" size={12}/>{showRetired ? `Hide retired (${retired.length})` : `Show retired (${retired.length})`}
      </button>}
    </section>
    <Resizer split={SPLITS.artifactsList} label="artifact list"/>
    {current ? <ArtifactPage key={currentCollectionId + '/' + current.id} collectionId={currentCollectionId} artifactId={current.id} summary={current} collection={other ? collections.find(c => c.id === other.collectionId) : collection}
      onNavigateRelated={(next, fromVersion) => {
        setHistory(items => [...items, { collectionId: currentCollectionId, id: current.id, version: fromVersion }]);
        navigate(currentCollectionId, next.id, next.version);
      }}
      onBack={back} onShowSession={onShowSession}
      focus={focus?.artifactId === current.id ? focus : undefined}
      conversationId={other ? undefined : conversation?.id} conversationTitle={other ? undefined : conversation?.title} onDeleted={back}/>
      : (all ? everywhere.items.length > 0 : artifacts.length > 0) && <div className="artifact-page is-placeholder"><p className="module-empty">Choose an artifact to read it.</p></div>}
    {menu && <ContextMenu menu={menu} onClose={() => setMenu(undefined)}/>}
    {deleting && <DeleteArtifactDialog artifact={deleting} onClose={() => setDeleting(undefined)} onDeleted={() => { if (deleting.id === selected) setSelected(''); setDeleting(undefined); }}/>}
  </div>;
}
