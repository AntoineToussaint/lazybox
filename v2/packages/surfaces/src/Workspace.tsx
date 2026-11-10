import { ActionMenu } from './ActionMenu';
import { WorkspaceNavigator } from './WorkspaceNavigator';
import {useKeyboardNavigation} from './keyboard';
import {RepositoryHostContext} from './RepositoryBrowser';
import {conversationReader} from './transcript-cache';
import { WorkspaceRepositories } from './WorkspaceRepositories';
import { ChangesPanel } from './ChangesPanel';
import { TelemetryPanel } from './TelemetryPanel';
import { DraggableDialog, DialogHeader } from './DraggableDialog';
import { useEffect, useMemo, useRef, useState, type ComponentType, type ReactNode } from 'react';
import type { Collection } from '@lazybox/protocol';
import type { ViewStateStore, WorkspaceHost } from '@lazybox/platform';
import { RunnerProvider, useRunner, type RunnerConnection } from './RunnerContext';
import { ChatPanel } from './AgentWindow';
import { ConversationList } from './ConversationList';
import { CollectionPanel } from './CollectionPanel';
import { CommandPanel } from './CommandPanel';
import { mergeConversations } from './conversation-model';
import { retireMonitor, clonePage, defaultLayout, emptyTarget, mapPanels, movePanel, panels, parseLayout, removePanel, resolveTarget, visibleLayout, type LayoutNode, type Panel, type ViewTarget, type WorkspaceLayout, type WorkspacePage } from './workspace-model';
import './workspace.css';
import './workspace-shell.css';
import { Icon } from './Icons';
import { sessionStatus } from './agent-status';
import { SettingsPage } from './SettingsPage';

export type ModuleProps = { workspaceId: string; chooseRepositoryFolder?(): Promise<string | null>; target: ViewTarget; onTarget(target: ViewTarget): void; collections: Collection[]; error: string; refresh(): void; viewState?: ViewStateStore; onOpenViewer?(sessionId: string, commandId: string): Promise<void>; onOpenChat?(): void; onNewConversation?(workspaceId: string): void };
export type PanelModule = { title: string; Component: ComponentType<ModuleProps> };
function ChatModule({ target, onTarget, viewState, collections, onOpenChat }: ModuleProps) {
  const {api}=useRunner();
  const collection = collections.find(value => value.id === target.collectionId);
  return <ChatPanel key={JSON.stringify([target.collectionId, conversationReader(api,target.sessionId).identity])} embedded showCommands={false} onOpenChat={onOpenChat} collectionId={target.collectionId} initialSessionId={target.sessionId} allowStart={!!collection && collection.state === 'ready' && !collection.ownershipUncertain} viewState={viewState} onSessionChange={sessionId => onTarget({ ...target, sessionId, commandId: '' })}/>;
}
function AssistantModule({ viewState,workspaceId }: ModuleProps) {
 const key=`${viewState?.key}:assistant-session`;
 const [sessionId,setSessionId]=useState(()=>viewState?.getItem(key)||'');
 const [purpose,setPurpose]=useState<'help'|'planning'|'summary'>(()=>{const saved=viewState?.getItem(`${key}:purpose`);return saved==='planning'||saved==='summary'?saved:'help';});
 return <><label className="assistant-purpose">Conversation<select aria-label="Assistant purpose" value={purpose} onChange={e=>{setPurpose(e.target.value as typeof purpose);viewState?.setItem(`${key}:purpose`,e.target.value);setSessionId('');viewState?.setItem(key,'');}}><option value="help">Help</option><option value="planning">Planning</option><option value="summary">Summary</option></select></label><ChatPanel key={purpose} collectionId={workspaceId} purpose={purpose} embedded showCommands={false} initialSessionId={sessionId} viewState={viewState} onSessionChange={id=>{setSessionId(id);viewState?.setItem(key,id);}}/></>;
}
/** First-party modules use the same runtime and target contract in every page. */
export const builtinPanels: Readonly<Record<string, PanelModule>> = {
  collections: { title: 'Workspaces', Component: CollectionPanel },
  chat: { title: 'Agent chat', Component: ChatModule },
  console: { title: 'Command output', Component: CommandPanel },
  assistant: { title: 'Ask Lazybox', Component: AssistantModule },
  changes: { title: 'Changes', Component: ({workspaceId}) => <ChangesPanel key={workspaceId} workspaceId={workspaceId}/> },
  usage: { title: 'Usage & cost', Component: ({workspaceId}) => <TelemetryPanel workspaceId={workspaceId}/> },
  sessions: { title: 'Sessions', Component: ({workspaceId,target,onTarget}) => <ConversationList collectionId={workspaceId} target={target} onTarget={onTarget}/> },
};
type Props = { modules?: Readonly<Record<string, PanelModule>>; fixedWorkspaceId?: string; connection: RunnerConnection; viewState?: ViewStateStore; initialTarget?: ViewTarget; host?: WorkspaceHost; onOpenViewer?(sessionId: string, commandId: string): Promise<void>; onTargetChange?(target: ViewTarget): Promise<void> | void };
export function Workspace(props: Props) { return <RunnerProvider connection={props.connection}><RepositoryHostContext.Provider value={props.host||{}}><WorkspaceView {...props}/></RepositoryHostContext.Provider></RunnerProvider>; }

function WorkspaceView({ modules, fixedWorkspaceId, viewState, initialTarget = emptyTarget(), host, onOpenViewer, onTargetChange }: Props) {
  const keyboardHint=useKeyboardNavigation(viewState);
  const { connection, api, info, connecting, error: connectionError, refresh: refreshRunner } = useRunner();
  const registry=useMemo(()=>({...builtinPanels,...modules}),[modules]);
  const [workspaceID,setWorkspaceID]=useState(()=>fixedWorkspaceId || initialTarget.collectionId || viewState?.getItem(`${viewState.key}:active-workspace`) || '');
  const workspaceKey=(id:string)=>`${viewState?.scope || viewState?.key}:workspace:${id || 'home'}:pages`;
  const key=workspaceKey(workspaceID);
  const [restored] = useState(() => {
    try { const saved = viewState?.getItem(key); return { layout: saved ? retireMonitor(parseLayout(saved)) : defaultLayout({...initialTarget,collectionId:workspaceID}), error: '' }; }
    catch (error) { return { layout: defaultLayout({...initialTarget,collectionId:workspaceID}), error: `Saved pages could not be restored: ${String(error)}. Their data has been retained.` }; }
  });
  const [layout, setLayout] = useState(restored.layout), [locked, setLocked] = useState(!!restored.error);
  const [error, setError] = useState(restored.error), [editing, setEditing] = useState(false), [configuring, setConfiguring] = useState(false), [json, setJSON] = useState('');
  const [collections, setCollections] = useState<Collection[]>([]), [catalogError, setCatalogError] = useState(''), [revision, setRevision] = useState(0);
  const [repositoryWorkspace,setRepositoryWorkspace]=useState<Collection>();
  const [navigatorOpen, setNavigatorOpen] = useState(false);
  const [mobilePanelId, setMobilePanelId] = useState('');
  const [settings, setSettings] = useState(false);
  const [newSession, setNewSession] = useState(false);
  const [quickStart, setQuickStart] = useState(false);
  const sessionStore=useMemo(()=>viewState ? {...viewState,scope:`${viewState.scope || viewState.key}:workspace:${workspaceID || 'home'}`} : undefined,[viewState,workspaceID]);
  const sessionDialog = useRef<HTMLDialogElement>(null);
  useEffect(() => { if (newSession) sessionDialog.current?.showModal(); else sessionDialog.current?.close(); }, [newSession]);
  const [module, setModule] = useState<string>('console'), [pending, setPending] = useState<string>('');
  const current = useRef(layout); current.current = layout;
  const stores = useMemo(() => new Map<string, ViewStateStore>(), [viewState,workspaceID]);
  useEffect(() => {
    const retained = new Set(layout.pages.flatMap(page => panels(page.layout).map(panel => panel.id)));
    for (const id of stores.keys()) if (!retained.has(id)) stores.delete(id);
  }, [layout, stores]);
  const hasDetached = layout.pages.some(page => panels(page.layout).some(panel => panel.detached));
  const page = layout.pages.find(value => value.id === layout.selectedPage)!;
  const refreshCatalog = () => setRevision(value => value + 1);
  const refresh = () => { refreshCatalog(); refreshRunner(); };
  useEffect(() => {
    const control = new AbortController(); let updating = false;
    const update = async () => {
      if (updating || !connection.token) return; updating = true;
      try { const result = await api.listCollections({}, { signal: control.signal }); if (!control.signal.aborted) { setCollections(result.collections); setCatalogError(''); } }
      catch (error) { if (!control.signal.aborted) setCatalogError(String(error)); }
      finally { updating = false; }
    };
    void update(); const timer = setInterval(() => void update(), 3000);
    return () => { control.abort(); clearInterval(timer); };
  }, [api, revision, connection.token]);
  const commit = (next: WorkspaceLayout, reset = false) => {
    try {
      if (locked && !reset) throw new Error('Restore, import, or explicitly reset the retained layout before editing.');
      const checked = parseLayout(JSON.stringify(next));
      viewState?.setItem(key, JSON.stringify(checked)); current.current = checked; setLayout(checked); setError(''); if (reset) setLocked(false);
      return true;
    } catch (error) { setError(String(error)); return false; }
  };
  const changePage = (next: WorkspacePage) => commit({ ...current.current, pages: current.current.pages.map(value => value.id === next.id ? next : value) });
  const changePanel = (panelID: string, change: (panel: Panel) => Panel) => {
    const live = current.current.pages.find(value => value.id === page.id);
    if (live) return changePage({ ...live, layout: mapPanels(live.layout, panel => panel.id === panelID ? change(panel) : panel) });
    return false;
  };
  const select = (target: ViewTarget) => {
    if(target.collectionId!==workspaceID){
      if(fixedWorkspaceId){if(target.collectionId)void host?.openWorkspace?.(target.collectionId);return;}
      try{
        if (!locked) viewState?.setItem(workspaceKey(workspaceID), JSON.stringify(current.current));
        const saved=viewState?.getItem(workspaceKey(target.collectionId));
        const next=saved?retireMonitor(parseLayout(saved)):defaultLayout(target);
        if(target.sessionId)next.pages.find(page=>page.id===next.selectedPage)!.selection=target;
        viewState?.setItem(`${viewState.key}:active-workspace`,target.collectionId);
        setWorkspaceID(target.collectionId);current.current=next;setLayout(next);setLocked(false);setEditing(false);setConfiguring(false);setError('');
      }catch(error){setError(`Could not open workspace layout: ${String(error)}`);}
      return;
    }
    const live = current.current.pages.find(value => value.id === page.id);
    if (live) changePage({ ...live, selection: target });
  };
  useEffect(() => {
    if (page.selection.sessionId && !page.selection.collectionId) {
      const session = info?.sessions.find(value => value.id === page.selection.sessionId);
      if (session?.collectionId) select({ ...page.selection, collectionId: session.collectionId });
    }
  }, [info, page.id, page.selection.sessionId, page.selection.collectionId]);
  useEffect(() => {
    if (onTargetChange) void Promise.resolve(onTargetChange(page.selection)).catch(error => setError(String(error)));
  }, [onTargetChange, page.selection.collectionId, page.selection.sessionId, page.selection.commandId]);
  const canShowNewSession = panels(page.layout).some(p => p.kind === 'chat' && p.binding.mode === 'follow' && !p.detached) || panels(page.layout).length < 12;
  const showSession = (selection: ViewTarget) => {
    setMobilePanelId('');
    if(selection.collectionId!==workspaceID){select(selection);setSettings(false);setNewSession(false);return;}
    const live = current.current.pages.find(value => value.id === page.id)!;
    const hasChat = panels(live.layout).some(p => p.kind === 'chat' && p.binding.mode === 'follow' && !p.detached);
    const nextLayout: LayoutNode = hasChat ? mapPanels(live.layout, p => p.kind === 'chat' && p.binding.mode === 'follow' ? { ...p, collapsed: false } : p) : { type: 'split', direction: 'row', ratio: 0.5, children: [live.layout, { type: 'panel', panel: { id: crypto.randomUUID(), kind: 'chat', binding: { mode: 'follow' }, detached: false } }] };
    if (changePage({ ...live, selection, layout: nextLayout })) { setSettings(false); setNewSession(false); }
  };
  const showChanges = () => {
    const existing = current.current.pages.find(value=>panels(value.layout).some(panel=>panel.kind==='changes'));
    if(existing){commit({...current.current,selectedPage:existing.id});setSettings(false);return;}
    const id=crypto.randomUUID();
    const review:WorkspacePage={id,name:'Changes',selection:{...page.selection,collectionId:workspaceID},layout:{type:'panel',panel:{id:crypto.randomUUID(),kind:'changes',binding:{mode:'follow'},detached:false}}};
    if(commit({...current.current,selectedPage:id,pages:[...current.current.pages,review]}))setSettings(false);
  };
  const beginConversation = (collectionId = workspaceID) => {
    if (collectionId !== workspaceID) select({ collectionId, sessionId: '', commandId: '' });
    setNavigatorOpen(false); setQuickStart(false); setNewSession(true);
  };
  const showNewSession = (sessionId: string) => {
    showSession({ collectionId: workspaceID || info?.sessions.find(s => s.id === sessionId)?.collectionId || '', sessionId, commandId: '' });
    refreshRunner();
  };
  const detach = async (panel: Panel) => {
    if (!host || pending) return;
    const target = resolveTarget(panel, page.selection);
    if (!target.sessionId) return;
    setPending(panel.id);
    try {
      if (!changePanel(panel.id, panel => ({ ...panel, binding: { mode: 'pinned', target: { ...target } }, detached: true }))) return;
      await host.openChat({ panelId: panel.id, collectionId: target.collectionId, sessionId: target.sessionId });
    }
    catch (error) { changePanel(panel.id, () => panel); setError(String(error)); }
    finally { setPending(''); }
  };
  const dock = async (panel: Panel) => {
    if (!host || pending) return;
    setPending(panel.id);
    try { await host.dockChat(panel.id); changePanel(panel.id, value => ({ ...value, detached: false })); }
    catch (error) { setError(String(error)); }
    finally { setPending(''); }
  };
  const resize = (path: number[], change: (node: Extract<LayoutNode, { type: 'split' }>) => LayoutNode) => {
    const update = (node: LayoutNode, rest: number[]): LayoutNode => {
      if (node.type !== 'split') return node;
      if (!rest.length) return change(node);
      return { ...node, children: [rest[0] === 0 ? update(node.children[0], rest.slice(1)) : node.children[0], rest[0] === 1 ? update(node.children[1], rest.slice(1)) : node.children[1]] };
    };
    changePage({ ...page, layout: update(page.layout, path) });
  };
  const renderNode = (node: LayoutNode, path: number[] = []): ReactNode => {
    if (node.type === 'split') return <div className={`layout-split split-${node.direction}`}>
      {editing && <div className="split-controls"><label>Direction<select aria-label={`Direction split ${path.join('-') || 'root'}`} value={node.direction} onChange={event => resize(path, node => ({ ...node, direction: event.target.value as 'row' | 'column' }))}><option value="row">Side by side</option><option value="column">Stacked</option></select></label><label>Size<input aria-label={`Resize split ${path.join('-') || 'root'}`} type="range" min={15} max={85} value={Math.round(node.ratio * 100)} onChange={event => resize(path, node => ({ ...node, ratio: Number(event.target.value) / 100 }))}/></label></div>}
      {node.children.map((child, index) => {
        const sibling = node.children[index ? 0 : 1];
        const collapsed = child.type === 'panel' && child.panel.collapsed && !editing;
        const siblingCollapsed = sibling.type === 'panel' && sibling.panel.collapsed && !editing;
        return <div key={index} className="layout-cell" style={{ minWidth: node.direction === 'row' && child.type === 'panel' && child.panel.kind === 'collections' && !collapsed ? 240 : undefined, flex: collapsed ? `0 0 ${node.direction === 'column' ? 42 : 44}px` : `${siblingCollapsed ? 1 : index ? 1 - node.ratio : node.ratio} 1 0` }}>{renderNode(child, [...path, index])}</div>;
      })}
    </div>;
    const panel = node.panel, definition = Object.hasOwn(registry, panel.kind) ? registry[panel.kind] : undefined, target = resolveTarget(panel, page.selection);
    if (viewState && !stores.has(panel.id)) stores.set(panel.id, { ...viewState, scope: `${viewState.scope || viewState.key}:workspace:${workspaceID || 'home'}`, key: `${viewState.scope || viewState.key}:workspace:${workspaceID || 'home'}:panel:${panel.id}` });
    const store = stores.get(panel.id);
    const onTarget = (target: ViewTarget) => panel.binding.mode === 'pinned' ? changePanel(panel.id, value => ({ ...value, binding: { mode: 'pinned', target } })) : select(target);
    return <section className={`workspace-panel panel-${panel.kind}${panel.collapsed && !editing ? ' is-collapsed' : ''}${editing ? ' is-editing' : ''}`} aria-label={definition?.title || panel.kind} data-panel-id={panel.id} data-mobile-active={panel.id === mobilePanel?.id || undefined}>
      <header className="module-header">{panel.kind === 'console' ? <button className="module-title" aria-label={panel.collapsed ? 'Show command output' : 'Hide command output'} aria-expanded={!panel.collapsed} onClick={() => changePanel(panel.id, value => ({ ...value, collapsed: !value.collapsed }))}><Icon name="terminal"/><span>{definition?.title}</span><Icon name="chevron" size={13}/></button> : <strong>{definition?.title || panel.kind}</strong>}{editing && <span>{panel.binding.mode === 'pinned' ? 'Pinned' : 'Follows page'}</span>}
        {editing && <div className="panel-config"><button disabled={panel.detached || !!pending} onClick={() => changePanel(panel.id, value => ({ ...value, binding: value.binding.mode === 'pinned' ? { mode: 'follow' } : { mode: 'pinned', target: { ...target } } }))}>{panel.binding.mode === 'pinned' ? 'Follow page' : 'Pin view'}</button><button aria-label={`Move ${panel.kind} earlier`} onClick={() => changePage({ ...page, layout: movePanel(page.layout, panel.id, -1) })}>←</button><button aria-label={`Move ${panel.kind} later`} onClick={() => changePage({ ...page, layout: movePanel(page.layout, panel.id, 1) })}>→</button><button aria-label={`Remove ${panel.kind} panel`} disabled={panels(page.layout).length === 1 || panel.detached || !!pending} onClick={() => { const next = removePanel(page.layout, panel.id); if (next) changePage({ ...page, layout: next }); }}>Remove</button></div>}
      </header>
      {(!panel.collapsed || editing) && <div className="module-body">{panel.detached ? <div className="module-empty detached-message"><Icon name="external" size={24}/><h3>Chat is in its own window</h3><p>Your conversation continues there.</p><button disabled={!host || !!pending} onClick={() => void detach(panel)}>Open chat window ↗</button><button disabled={!host || !!pending} onClick={() => void dock(panel)}>Bring chat back here</button></div> : definition ? <definition.Component workspaceId={workspaceID} chooseRepositoryFolder={host?.chooseRepositoryFolder} target={target} onTarget={panel.kind==='collections' ? target => target.sessionId ? showSession(target) : onTarget(target) : onTarget} collections={collections} error={catalogError} refresh={refreshCatalog} onNewConversation={beginConversation} viewState={store} onOpenViewer={onOpenViewer} onOpenChat={panel.kind === 'chat' && host && target.sessionId ? () => void detach(panel) : undefined}/> : <p className="module-empty">The “{panel.kind}” module is unavailable. Its configuration has been retained.</p>}</div>}
    </section>;
  };
  const displayedLayout = editing ? page.layout : visibleLayout(page.layout);
  const mobilePanels = panels(displayedLayout || page.layout).filter(panel => panel.kind !== 'collections');
  const mobilePanel = mobilePanels.find(panel => panel.id === mobilePanelId) || mobilePanels.find(panel => panel.kind === 'chat') || mobilePanels[0];
  return <main className={`workspace-shell${editing ? ' layout-editing' : ''}`}>{keyboardHint&&<div className="keyboard-sequence-hint" role="status">{keyboardHint}</div>}
    <header className="workspace-header">
      <button className="mobile-navigation-button icon-button" aria-label="Open workspace navigator" aria-expanded={navigatorOpen} onClick={() => setNavigatorOpen(true)}><Icon name="sidebar"/></button>
      <div className="workspace-brand"><span className="brand-mark">l<span>↗</span></span><strong>lazybox</strong></div>
      <nav aria-label="Pages">{layout.pages.map(value => <button key={value.id} disabled={!!pending} className={!settings && value.id === page.id ? 'selected' : ''} onClick={() => { setSettings(false); commit({ ...layout, selectedPage: value.id }); }}>{value.name}</button>)}{!fixedWorkspaceId&&<button className={settings ? 'selected' : ''} onClick={() => setSettings(true)}>Settings</button>}</nav>
      <div className="workspace-header-actions">
        <button className="toolbar-action" disabled={!workspaceID} aria-label="Review changes" title="Review changes" onClick={showChanges}><Icon name="branch"/><span>Changes</span></button>
        <button className="toolbar-action new-conversation-action" disabled={!info || !!pending || !canShowNewSession} aria-label="New conversation" title="New conversation" onClick={() => beginConversation()}><Icon name="plus"/><span>New conversation</span></button>
        <span className={'runner-indicator' + (connectionError ? ' offline' : '')} title={connectionError || (connecting ? 'Connecting to Runner' : 'Runner connected')}><i/>{connecting ? 'Connecting' : 'Connected'}</span>
        <ActionMenu label="Page actions">
          {workspaceID&&host?.openWorkspace&&!fixedWorkspaceId&&<button onClick={()=>void host.openWorkspace!(workspaceID)}>Open workspace window</button>}
          <button disabled={settings} onClick={() => setEditing(value => !value)}>{editing ? 'Finish customizing' : 'Customize page'}</button>
          <button disabled={settings} onClick={() => { setJSON(JSON.stringify(layout, null, 2)); setConfiguring(value => !value); }}>Layout configuration</button>
        </ActionMenu>
      </div>
    </header>
    {!settings&&panels(page.layout).some(p=>p.detached)&&<div className="detached-tabs">{panels(page.layout).filter(p=>p.detached).map(p=><span key={p.id}><button onClick={()=>void detach(p)}>↗ {info?.sessions.find(s=>s.id===resolveTarget(p,page.selection).sessionId)?.title||'Chat in separate window'}</button><button onClick={()=>void dock(p)}>Bring back</button></span>)}</div>}
    {repositoryWorkspace&&<WorkspaceRepositories workspace={collections.find(value=>value.id===repositoryWorkspace.id)||repositoryWorkspace} onClose={()=>setRepositoryWorkspace(undefined)} onSaved={refreshCatalog} chooseRepositoryFolder={host?.chooseRepositoryFolder}/>}
    <DraggableDialog ref={sessionDialog} className="new-session-dialog" onCancel={() => setNewSession(false)} onClose={() => setNewSession(false)} aria-label="New conversation">
      <DialogHeader><strong>New conversation</strong><button aria-label="Close new conversation" onClick={() => setNewSession(false)}>×</button></DialogHeader>
      <p className="conversation-scope">{workspaceID ? <>In <strong>{collections.find(value => value.id === workspaceID)?.name || 'this workspace'}</strong></> : 'Starts in a new workspace without repositories.'}</p>
      {newSession && !quickStart && <button className="quick-start-option" onClick={() => setQuickStart(true)}>Start with last settings</button>}
      {newSession && <ChatPanel autoStart={quickStart} viewState={sessionStore} embedded emptyWorkspace={!workspaceID} collectionId={workspaceID} showCommands={false} onSessionChange={showNewSession}/>}
    </DraggableDialog>
    {(error || connectionError) && <div className="error" role="alert">{connectionError ? <><strong>Runner disconnected.</strong> Reconnecting automatically… <details><summary>Connection details</summary>{connectionError}</details></> : error}<button onClick={()=>{if(host?.reconnect)void host.reconnect().catch(e=>setError(String(e)));else refresh();}}>Retry now</button></div>}
    {settings ? <SettingsPage chooseRepositoryFolder={host?.chooseRepositoryFolder} viewState={viewState} openSignIn={host?.openSignIn} copySignInCode={host?.copySignInCode}/> : <>
    {locked && <button onClick={() => commit(defaultLayout({...initialTarget,collectionId:workspaceID}), true)}>Reset retained layout</button>}
    {configuring && <section className="layout-json"><div className="inspector-heading"><strong>Page configuration</strong><button className="icon-button" aria-label="Close layout configuration" onClick={() => setConfiguring(false)}>×</button></div><label className="sr-only" htmlFor="page-config-json">Page configuration</label><textarea id="page-config-json" value={json} onChange={event => setJSON(event.target.value)} spellCheck={false}/><button disabled={hasDetached || !!pending} onClick={() => { try { if (commit(parseLayout(json), true)) setConfiguring(false); } catch (error) { setError(String(error)); } }}>Import layout</button><small>Panel references and arrangement only. Your account and permissions stay unchanged.</small></section>}
    <div className="workspace-context">
      <div className="collection-breadcrumb"><Icon name="folder" size={17}/><label><span className="sr-only">Workspace</span><select disabled={!!pending || !!fixedWorkspaceId} value={page.selection.collectionId} onChange={event => { if (event.target.value !== workspaceID) select({ collectionId: event.target.value, sessionId: '', commandId: '' }); }}><option value="">Choose a workspace</option>{page.selection.collectionId && !collections.some(value => value.id === page.selection.collectionId) && <option value={page.selection.collectionId}>Unavailable workspace</option>}{collections.map(value => <option key={value.id} value={value.id}>{value.name}</option>)}</select></label><button className="workspace-repositories-link" disabled={!collections.some(value=>value.id===workspaceID)} onClick={()=>setRepositoryWorkspace(collections.find(value=>value.id===workspaceID))}>{collections.find(value=>value.id===workspaceID)?.members.length||0} {collections.find(value=>value.id===workspaceID)?.members.length===1?'repository':'repositories'}</button></div>
      <label className="session-switcher"><span className="sr-only">Session</span><select value={page.selection.sessionId} onChange={event => { const session = info?.sessions.find(value => value.id === event.target.value); select({ collectionId: session ? session.collectionId : page.selection.collectionId, sessionId: event.target.value, commandId: '' }); }}><option value="">Choose a conversation</option>{page.selection.sessionId && !info?.sessions.some(value => value.id === page.selection.sessionId) && <option value={page.selection.sessionId}>Unavailable session</option>}{mergeConversations([], info?.sessions || []).filter(value => !page.selection.collectionId || value.collectionId === page.selection.collectionId).map(value => <option key={value.id} value={value.id}>{value.title||'Conversation'} · {info?.profiles.find(profile => profile.id === value.profileId)?.provider === 'codex' ? 'Codex' : 'Claude'} · {info?.profiles.find(profile => profile.id === value.profileId)?.label || value.id.slice(0, 6)}</option>)}</select></label>
    </div>
    {editing && <div className="page-config"><PageName key={page.id} name={page.name} onSave={name => changePage({ ...page, name })}/><button disabled={layout.pages.length >= 8} onClick={() => { const next = clonePage(page, () => crypto.randomUUID()); commit({ ...layout, selectedPage: next.id, pages: [...layout.pages, next] }); }}>Duplicate page</button><button disabled={layout.pages.length <= 1 || layout.pages.some(value => value.id === page.id && panels(value.layout).some(panel => panel.detached))} onClick={() => commit({ ...layout, selectedPage: layout.pages.find(value => value.id !== page.id)!.id, pages: layout.pages.filter(value => value.id !== page.id) })}>Remove page</button><label>Add panel<select value={module} onChange={event => setModule(event.target.value)}>{Object.entries(registry).map(([kind,module]) => <option key={kind} value={kind}>{module.title}</option>)}</select></label><button disabled={panels(page.layout).length >= 12} onClick={() => changePage({ ...page, layout: { type: 'split', direction: 'row', ratio: 0.65, children: [page.layout, { type: 'panel', panel: { id: crypto.randomUUID(), kind: module, binding: { mode: 'follow' }, detached: false } }] } })}>Add panel</button><button disabled={hasDetached || !!pending} onClick={() => commit(defaultLayout(page.selection), true)}>Reset pages</button></div>}
    {mobilePanels.length > 1 && <nav className="mobile-panel-switcher" aria-label="Workspace panels">{mobilePanels.map(panel => <button key={panel.id} aria-pressed={panel.id === mobilePanel?.id} onClick={() => { setMobilePanelId(panel.id); if (panel.collapsed) changePanel(panel.id, value => ({ ...value, collapsed: false })); }}>{registry[panel.kind]?.title || panel.kind}</button>)}</nav>}
    <div className="page-layout" data-page-id={page.id}>{displayedLayout ? renderNode(displayedLayout) : <p className="module-empty">Your conversations are open in separate windows.</p>}</div>
    </>}
    {navigatorOpen && <WorkspaceNavigator target={page.selection} onTarget={target => target.sessionId ? showSession(target) : select(target)} collections={collections} error={catalogError} refresh={refreshCatalog} chooseRepositoryFolder={host?.chooseRepositoryFolder} onNewConversation={beginConversation} onClose={() => setNavigatorOpen(false)}/>}
  </main>;
}

function PageName({ name, onSave }: { name: string; onSave(name: string): void }) {
  const [draft, setDraft] = useState(name);
  return <form onSubmit={event => { event.preventDefault(); onSave(draft); }}><label>Page name<input required maxLength={64} value={draft} onChange={event => setDraft(event.target.value)}/></label><button>Rename page</button></form>;
}
