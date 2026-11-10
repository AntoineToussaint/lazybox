import React from 'react';
import { createRoot } from 'react-dom/client';
import { AgentWindow, Workspace } from '@lazybox/surfaces';
import { browserWorkspaceHost } from './browser-host';

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
const viewState = { key: mode === 'chat' ? `${panelScope}:panel:${panelId}` : mode === 'workspace' ? `${scope}:main` : scope, scope: mode==='chat'?panelScope:scope, getItem: (key: string) => storage.getItem(key), setItem: (key: string, value: string) => storage.setItem(key, value) };
const host = browserWorkspaceHost(scope, token, mode === 'chat' ? { panelId, sessionId: initial.get('session') || '', collectionId: initial.get('collection') || '' } : undefined);
createRoot(document.getElementById('root')!).render(<React.StrictMode>{mode === 'workspace' ? <Workspace fixedWorkspaceId={initial.get('workspaceWindow') ? initial.get('collection') || undefined : undefined} connection={{ baseUrl: location.origin, token }} viewState={viewState} host={host} initialTarget={{ collectionId: initial.get('collection') || '', sessionId: initial.get('session') || '', commandId: '' }}/>: <AgentWindow
  connection={{ baseUrl: location.origin, token }}
  agent="claude"
  collectionId={initial.get('collection') || undefined}
  allowStart={mode !== 'chat'}
  showCommands={mode !== 'chat'}
  embedded={mode === 'chat'}
  viewState={viewState}
  initialSessionId={initial.get('session') || undefined}
  initialCommandId={initial.get('command') || undefined}
  onSessionChange={session => {
    const url = new URL(location.href);
    if (session) url.searchParams.set('session', session); else url.searchParams.delete('session');
    url.searchParams.delete('command'); history.replaceState(null, '', url);
  }}
  onOpenViewer={(session, command) => {
    const url = new URL(location.href); url.searchParams.set('session', session); url.searchParams.set('command', command);
    url.hash = new URLSearchParams({ token }).toString(); window.open(url, '_blank', 'noopener');
  }}
/>}</React.StrictMode>);
