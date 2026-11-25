import { createContext, useContext, useEffect, useRef, useState } from 'react';
import type { ViewStateStore, WebWindowTarget } from '@lazybox/platform';
import { useContextMenuContribution } from './ContextMenu';
import { Icon } from './Icons';
import type { ModuleProps } from './PanelRegistry';
import { useRunner } from './RunnerContext';
import { current, pageLabel, parseHistory, validZoom, visit, webPage, type WebHistory } from './web-address';
import { useLinkPreferences } from './web-links';
import { WebViewer } from './WebViewer';
import { actionName, previewable, useWorkspaceRuntime } from './workspace-runtime';

/** Opens a web page in a window of its own, where the host can (Detach). */
export const WebWindowContext = createContext<((target: WebWindowTarget) => Promise<void>) | undefined>(undefined);

// Pages asked of a Web panel before it mounted (a link ⌥-clicked into a new panel).
const pending = new Map<string, string>();
const requestListeners = new Set<() => void>();
/** Shows `url` in the Web panel `panelId`; an empty url puts the cursor in its address bar. */
export function requestWebPage(panelId: string, url: string) {
  pending.set(panelId, url);
  for (const listener of requestListeners) listener();
}
let lastActive = '';
/** The Web panel a person used last: a link opened "in Lazybox" goes there. */
export const lastWebPanel = () => lastActive;

type Saved = { history: WebHistory; zoom: number };
/** Saves a viewer's pages a moment after they settle (a page reports its
 * loads and title in bursts), and at once when the viewer or the window goes away. */
export function useSavedPage(store: ViewStateStore | undefined, key: string, history: WebHistory, zoom: number) {
  const latest = useRef('');
  latest.current = JSON.stringify({ entries: history.entries, index: history.index, zoom });
  const written = useRef('');
  const save = () => {
    if (!store || written.current === latest.current) return;
    try { store.setItem(key, latest.current); written.current = latest.current; } catch { /* the history is a convenience */ }
  };
  useEffect(() => { const timer = setTimeout(save, 400); return () => clearTimeout(timer); }, [store, key, latest.current]);
  useEffect(() => { window.addEventListener('pagehide', save); return () => { window.removeEventListener('pagehide', save); save(); }; }, [store, key]);
}
export function readSaved(store: ViewStateStore | undefined, key: string): Saved {
  if (!store) return { history: parseHistory(null), zoom: 1 };
  try {
    const text = store.getItem(key);
    const zoom = validZoom(JSON.parse(text || '{}').zoom);
    return { history: parseHistory(text), zoom };
  } catch { return { history: parseHistory(null), zoom: 1 }; }
}

/** A Web panel: one page with its history, kept with the panel across restarts. */
export function WebPanel({ panelId = '', workspaceId, viewState, onPanelTitle, onClosePanel }: ModuleProps) {
  const { engine } = useLinkPreferences(viewState);
  const key = viewState ? `${viewState.key}:web` : '';
  const [saved] = useState(() => readSaved(viewState, key));
  const [history, setHistory] = useState(saved.history);
  const [zoom, setZoom] = useState(saved.zoom);
  const [icon, setIcon] = useState('');
  // An empty panel is for typing an address, unless a page is already on its way.
  const [focusAddress, setFocusAddress] = useState(() => saved.history.entries.length || webPage(pending.get(panelId) || '') ? 0 : 1);
  const openWindow = useContext(WebWindowContext);
  const [detachError, setDetachError] = useState('');
  useSavedPage(viewState, key, history, zoom);
  useEffect(() => {
    const take = () => {
      if (!pending.has(panelId)) return;
      const url = pending.get(panelId)!; pending.delete(panelId);
      lastActive = panelId;
      const page = webPage(url);
      if (page) setHistory(h => current(h)?.url === page && h.expect === undefined ? h : visit(h, page));
      else setFocusAddress(n => n + 1);
    };
    take(); requestListeners.add(take);
    return () => { requestListeners.delete(take); };
  }, [panelId]);
  const entry = current(history);
  const page = history.expect !== undefined ? history.entries[history.expect]?.url : entry?.url;
  useEffect(() => {
    onPanelTitle?.(entry ? { text: pageLabel(entry), webUrl: page, icon: icon || undefined, tooltip: entry.title ? `${entry.title}\n${entry.url}` : entry.url } : undefined);
  }, [entry?.url, entry?.title, icon, page]);
  const detach = async () => {
    if (!openWindow || !page) return;
    try {
      await openWindow({ webId: panelId || crypto.randomUUID(), url: page, collectionId: workspaceId });
      onClosePanel?.();
    } catch (error) { setDetachError(String(error)); }
  };
  // The panel header's right-click menu moves the page too.
  useContextMenuContribution<{ id?: string }>('panel', context => context.detail?.id === panelId && openWindow ? [{ label: 'Move to a new window', icon: 'external', disabled: !page && 'Open a page first', run: () => void detach() }] : []);
  return <div className="web-panel" style={{ display: 'contents' }} onPointerDownCapture={() => { lastActive = panelId; }} onFocusCapture={() => { lastActive = panelId; }}>
    {detachError && <div className="error" role="alert">{detachError}<button type="button" onClick={() => setDetachError('')}>Dismiss</button></div>}
    <WebViewer id={`web-${panelId}`} label="Web page" frameTitle={entry?.title || 'Web page'} history={history} setHistory={setHistory} zoom={zoom} onZoom={setZoom}
      engine={engine} focusAddress={focusAddress} onIcon={setIcon}
      start={<RunningPreviews workspaceId={workspaceId} onOpen={url => setHistory(h => visit(h, url))}/>}
      menu={openWindow && <button type="button" disabled={!page} onClick={() => void detach()}><Icon name="external"/><span>Move to a new window</span></button>}/>
  </div>;
}

/** On an empty page, the workspace's running apps are one click away. */
function RunningPreviews({ workspaceId, onOpen }: { workspaceId: string; onOpen(url: string): void }) {
  const { api } = useRunner();
  const { runtime } = useWorkspaceRuntime(api, workspaceId);
  const running = previewable(runtime).filter(action => webPage(action.url || ''));
  if (!running.length) return null;
  return <>
    <p>Running in this workspace:</p>
    <ul className="web-suggestions">{running.map(action => <li key={action.key}><button type="button" onClick={() => onOpen(webPage(action.url))}><span className="run-state-dot tone-ok is-live" aria-hidden="true"/>{actionName(action, runtime!.actions)}<small>{action.url.replace(/^https?:\/\//, '')}</small></button></li>)}</ul>
  </>;
}
