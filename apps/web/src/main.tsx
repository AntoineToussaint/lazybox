import React from 'react';
import { createRoot } from 'react-dom/client';
import { MissionWindow, AgentWindow, ArtifactWindow, RemoteApp, WebWindow, WindowControls, Workspace, WindowErrorBoundary, installLinkRouting, openInViewer } from '@lazybox/surfaces';
import { browserWorkspaceHost, dockWebPage, onDockedWebPage } from './browser-host';

// A paired phone (REMOTE.md): served by the Runner's device gateway, it never
// holds the owner's bearer, so it takes nothing from the owner's flow below.
if (new URLSearchParams(location.search).get('view') === 'remote') {
  document.title = 'Lazybox';
  createRoot(document.getElementById('root')!).render(<React.StrictMode><WindowErrorBoundary development={import.meta.env.DEV}><RemoteApp/></WindowErrorBoundary></React.StrictMode>);
} else {
// A web link opens in a new tab, or with ⌥ (or the Settings choice) in a Web panel.
installLinkRouting();
const fragment = new URLSearchParams(location.hash.slice(1));
const token = fragment.get('token') || sessionStorage.getItem('runner-token') || '';
if (token) sessionStorage.setItem('runner-token', token);
const initial = new URLSearchParams(location.search);
const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(location.origin + '\0' + token));
const scope = Array.from(new Uint8Array(digest), byte => byte.toString(16).padStart(2, '0')).join('');
const mode = initial.get('view') || 'agent';
const panelId = initial.get('panel') || '';
const storage = mode === 'agent' ? sessionStorage : localStorage;
const panelScope=`${scope}:workspace:${initial.get('collection') || 'home'}`;
const viewState = { key: mode === 'chat' ? `${panelScope}:panel:${panelId}` : mode === 'workspace' ? `${scope}:main` : mode === 'artifact' ? `${scope}:artifact:${initial.get('collection') || ''}:${initial.get('artifact') || ''}` : mode === 'web' ? `${scope}:web-${initial.get('web') || 'page'}` : scope, scope: mode==='chat'?panelScope:scope, root: scope, getItem: (key: string) => storage.getItem(key), setItem: (key: string, value: string) => storage.setItem(key, value) };
// Only the main workspace view notifies; a workspace window or a chat window would repeat it.
// A web window moved back: its page opens in the main window's Web panel.
if (mode === 'workspace' && !initial.get('workspaceWindow')) onDockedWebPage(scope, url => openInViewer(url, true));
const host = browserWorkspaceHost(scope, token, mode === 'chat' ? { panelId, sessionId: initial.get('session') || '', collectionId: initial.get('collection') || '' } : undefined, mode === 'workspace' && !initial.get('workspaceWindow'));
let mainWorkspace: Window | null = window.opener;
function focusMainWorkspace() {
  if (mainWorkspace && !mainWorkspace.closed) { mainWorkspace.focus(); return; }
  const url = new URL(location.href);
  url.search = new URLSearchParams({ view: 'workspace' }).toString();
  url.hash = new URLSearchParams({ token }).toString();
  mainWorkspace = window.open(url, `lazybox-${scope}-main`);
  mainWorkspace?.focus();
}
createRoot(document.getElementById('root')!).render(<React.StrictMode><WindowErrorBoundary development={import.meta.env.DEV}>{mode === 'web' ? <WebWindow id={initial.get('web') || 'page'} url={initial.get('url') || ''} viewState={viewState} onDock={async url => { dockWebPage(scope, url); window.close(); }}/> : mode === 'mission' ? <MissionWindow connection={{baseUrl: location.origin, token}} missionId={initial.get('mission') || ''} viewState={viewState} onFocusMain={focusMainWorkspace}/> : mode === 'artifact' ? <ArtifactWindow connection={{ baseUrl: location.origin, token }} collectionId={initial.get('collection') || ''} artifactId={initial.get('artifact') || ''} viewState={viewState}/> : mode === 'workspace' ? <Workspace fixedWorkspaceId={initial.get('workspaceWindow') ? initial.get('collection') || undefined : undefined} connection={{ baseUrl: location.origin, token }} viewState={viewState} host={host} initialTarget={{ collectionId: initial.get('collection') || '', sessionId: initial.get('session') || '', commandId: '' }}/>: <div className={mode === 'chat' ? 'desktop-root' : 'window-root'}>{mode === 'chat' && <WindowControls applicationName="Lazybox" viewState={viewState} onPutBack={() => void host.putBack?.()} onWorkspace={() => window.opener?.focus()}/>}<AgentWindow
  connection={{ baseUrl: location.origin, token }}
  agent="claude"
  collectionId={initial.get('collection') || undefined}
  allowStart={mode !== 'chat'}
  showCommands={mode !== 'chat'}
  embedded={mode === 'chat'}
  viewState={viewState}
  initialSessionId={initial.get('session') || undefined}
  initialCommandId={initial.get('command') || undefined}
  onFocusMain={() => window.opener?.focus()}
  onSessionChange={session => {
    const url = new URL(location.href);
    if (session) url.searchParams.set('session', session); else url.searchParams.delete('session');
    url.searchParams.delete('command'); history.replaceState(null, '', url);
  }}
  onOpenViewer={(session, command) => {
    const url = new URL(location.href); url.searchParams.set('session', session); url.searchParams.set('command', command);
    url.hash = new URLSearchParams({ token }).toString(); window.open(url, '_blank', 'noopener');
  }}
/></div>}</WindowErrorBoundary></React.StrictMode>);
}
