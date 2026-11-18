import { useEffect, useState } from 'react';
import type { NativeWebViews, ViewStateStore } from '@lazybox/platform';
import { Icon } from './Icons';
import { current, pageLabel, parseHistory, validZoom, visit, webPage } from './web-address';
import { useLinkPreferences } from './web-links';
import { useSavedPage } from './WebPanel';
import { WebViewer, WebViewsContext } from './WebViewer';
import './workspace.css';
import './workspace-shell.css';

/**
 * A Web panel moved to a window of its own: the same viewer, its history
 * kept with the window. "Move back to the workspace" docks the page again.
 */
export function WebWindow({ id, url, viewState, webViews, applicationName = 'Lazybox', onDock }: { id: string; url: string; viewState?: ViewStateStore; webViews?: NativeWebViews; applicationName?: string; onDock?(url: string): Promise<void> }) {
  const { engine } = useLinkPreferences(viewState);
  const key = viewState ? `${viewState.key}:web` : '';
  const [history, setHistory] = useState(() => {
    const saved = parseHistory(viewState?.getItem(key) || null);
    const page = webPage(url);
    return saved.entries.length || !page ? saved : visit(saved, page);
  });
  const [zoom, setZoom] = useState(() => { try { return validZoom(JSON.parse(viewState?.getItem(key) || '{}').zoom); } catch { return 1; } });
  const [error, setError] = useState('');
  useSavedPage(viewState, key, history, zoom);
  const entry = current(history);
  useEffect(() => { document.title = entry ? `${pageLabel(entry)} — ${applicationName}` : applicationName; }, [entry?.url, entry?.title, applicationName]);
  return <WebViewsContext.Provider value={webViews}>
    <main className="workspace-shell web-window">
      {error && <div className="error" role="alert">{error}<button onClick={() => setError('')}>Dismiss</button></div>}
      <WebViewer id={`window-${id}`} titleBar label="Web page" frameTitle={entry?.title || 'Web page'} history={history} setHistory={setHistory} zoom={zoom} onZoom={setZoom} engine={engine} focusAddress={entry ? 0 : 1}
        menu={onDock && <button type="button" disabled={!entry} onClick={() => void onDock(entry!.url).catch(e => setError(String(e)))}><Icon name="dock-right"/><span>Move back to the workspace</span></button>}/>
    </main>
  </WebViewsContext.Provider>;
}
