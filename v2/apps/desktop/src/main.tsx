import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { createRoot } from 'react-dom/client';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { fetch as nativeFetch } from '@tauri-apps/plugin-http';
import { AgentWindow, Workspace } from '@lazybox/surfaces';
import type { SurfaceHost, SurfaceTarget, ViewStateStore, WorkspaceHost } from '@lazybox/platform';

type Bootstrap = { url: string; token: string; workingDirectory: string; target: SurfaceTarget; viewKey: string; viewScope: string; applicationName: string; fixedWorkspaceId: string; kind: 'workspace' | 'chat' | 'console'; notice: string; viewState: Record<string,string> };
const fetchRunner: typeof globalThis.fetch = (input, init) => nativeFetch(input, { ...init, maxRedirections: 0 });

function DesktopWindow({ bootstrap }: { bootstrap: Bootstrap }) {
  const [error, setError] = useState(bootstrap.notice);
  const host = useMemo<SurfaceHost>(() => ({
    saveTarget: target => invoke<void>('save_surface', { target }),
    reopenAgent: () => invoke<void>('reopen_agent'),
    reopenClosed: () => invoke<void>('reopen_closed'),
    reconnect: () => invoke<void>('reconnect_view'),
  }), []);
  const retained = useRef(new Map(Object.entries(bootstrap.viewState)));
  const pendingWrites = useRef(Promise.resolve());
  const viewState = useMemo<ViewStateStore>(() => ({ key: bootstrap.viewKey, scope: bootstrap.viewScope,
    getItem: key => retained.current.get(key) ?? localStorage.getItem(key),
    setItem: (key, value) => {
      if (value.length > 256 * 1024) throw new Error('Saved view is too large');
      retained.current.set(key, value);
      pendingWrites.current = pendingWrites.current.then(() => invoke<void>('save_view_state', { key, value })).catch(error => setError(`Could not save this window: ${String(error)}`));
    },
  }), [bootstrap.viewKey, bootstrap.viewScope]);
  const saveTarget = useCallback((target: SurfaceTarget) => host.saveTarget(target), [host]);
  const workspaceHost = useMemo<WorkspaceHost>(() => ({
    reconnect: () => invoke<void>('reconnect_view'),
    openWorkspace: collectionId => invoke<void>('open_workspace', { collectionId }),
    chooseRepositoryFolder: () => invoke<string | null>('choose_repository_folder'),
    openSignIn: url => invoke<void>('open_sign_in', { url }),
    copySignInCode: code => invoke<void>('copy_sign_in_code', { code }),
    openChat: target => invoke<void>('open_chat', target),
    dockChat: panelId => invoke<void>('dock_chat', { panelId }),
  }), []);
  const saveSelection = useCallback((target: { collectionId: string; sessionId: string; commandId: string }) => host.saveTarget({ ...target, commandsOpen: false }), [host]);
  const openViewer = useCallback((sessionId: string, commandId: string) => invoke<void>('open_console', { sessionId, commandId }), []);
  useEffect(() => {
    let disposed = false; let stop: (() => void) | undefined;
    void listen<string>('desktop-error', event => setError(event.payload)).then(unlisten => { if (disposed) unlisten(); else stop = unlisten; });
    return () => { disposed = true; stop?.(); };
  }, []);
  const action = async (callback: () => Promise<void>) => { try { await callback(); setError(''); } catch (error) { setError(String(error)); } };
  return <div className="desktop-root">
    {bootstrap.kind !== 'workspace' && <nav className="native-toolbar" aria-label="Window controls">
      <span className="application-name">{bootstrap.applicationName}</span>
      <button onClick={() => void action(host.reopenAgent)}>← Workspace</button>
      <button onClick={() => void action(host.reopenClosed)}>Reopen closed window</button>
      <button onClick={() => void action(host.reconnect)}>Reconnect this window</button>
    </nav>}
    {error && <div className="error" role="alert">{error}<button onClick={() => setError('')}>Dismiss</button></div>}
    {bootstrap.kind === 'workspace' ? <Workspace
      fixedWorkspaceId={bootstrap.fixedWorkspaceId || undefined}
      connection={{ baseUrl: bootstrap.url, token: bootstrap.token, fetch: fetchRunner }}
      viewState={viewState} host={workspaceHost} onTargetChange={saveSelection} onOpenViewer={openViewer}
      initialTarget={{ collectionId: bootstrap.target.collectionId || '', sessionId: bootstrap.target.sessionId, commandId: bootstrap.target.commandId }}
    /> : <AgentWindow
      connection={{ baseUrl: bootstrap.url, token: bootstrap.token, fetch: fetchRunner }}
      agent="claude" mode={bootstrap.kind === 'console' ? 'console' : 'agent'} viewState={viewState}
      collectionId={bootstrap.target.collectionId} allowStart={false}
      showCommands={false} embedded={bootstrap.kind === 'chat'}
      workingDirectory={bootstrap.workingDirectory}
      initialSessionId={bootstrap.target.sessionId || undefined}
      initialCommandId={bootstrap.target.commandId || undefined}
      initialCommandsOpen={bootstrap.target.commandsOpen}
      onViewChange={bootstrap.kind === 'console' ? saveTarget : undefined}
      onOpenViewer={openViewer}
    />}
  </div>;
}
function DesktopApp() {
  const [bootstrap, setBootstrap] = useState<Bootstrap>();
  const [error, setError] = useState('');
  useEffect(() => {
    let disposed = false, reading = false;
    const read = async () => {
      if (reading) return;
      reading = true;
      try {
        const next = await invoke<Bootstrap>('runner_bootstrap');
        if (!disposed) {
          // Retain the component tree, drafts and selection across a backend
          // rebuild. Changing the bearer recreates only the RPC clients.
          setBootstrap(current => current?.token === next.token && current?.url === next.url ? current : next);
          setError('');
        }
      } catch (error) { if (!disposed) setError(String(error)); }
      finally { reading = false; }
    };
    void read();
    const timer = setInterval(() => void read(), 3000);
    return () => { disposed = true; if (timer) clearInterval(timer); };
  }, []);
  if (bootstrap) return <>{error&&<div className="error" role="alert">{error}<button onClick={()=>void invoke('reconnect_view').catch(e=>setError(String(e)))}>Reconnect this window</button></div>}<DesktopWindow bootstrap={bootstrap}/></>;
  return <p role={error ? 'alert' : 'status'}>{error ? `Unable to restore this window: ${error}` : 'Restoring your windows…'}</p>;
}
createRoot(document.getElementById('root')!).render(<React.StrictMode><DesktopApp/></React.StrictMode>);
