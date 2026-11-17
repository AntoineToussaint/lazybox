import { useEffect, useState } from 'react';
import { useRunner } from './RunnerContext';
import { Icon } from './Icons';
import { emptyHistory, visit, webPage } from './web-address';
import { useLinkPreferences } from './web-links';
import { WebViewer } from './WebViewer';
import { actionName, isLive, previewable, useRuntimeRequest, useWorkspaceRuntime } from './workspace-runtime';
import './workspace-runtime.css';

/**
 * A running action's page, inside the workspace, in the same viewer as the
 * Web panel: address bar, back and forward, reload, zoom, open in the browser.
 */
export function PreviewPanel({ workspaceId }: { workspaceId: string }) {
  const { api } = useRunner();
  const { runtime, unsupported, busy, store } = useWorkspaceRuntime(api, workspaceId);
  const targets = previewable(runtime);
  const [chosen, setChosen] = useState('');
  useRuntimeRequest(workspaceId, 'preview', key => setChosen(key));
  const target = targets.find(a => a.key === chosen) || targets[0];
  const base = webPage(target?.url || '');
  const [history, setHistory] = useState(() => base ? visit(emptyHistory(), base) : emptyHistory());
  const [zoom, setZoom] = useState(1);
  const { engine } = useLinkPreferences();
  // A new action, URL or run (a restart) starts a new history at its own address.
  useEffect(() => { setHistory(base ? visit(emptyHistory(), base) : emptyHistory()); }, [target?.key, base, target?.runState?.commandId]);
  if (!workspaceId) return <p className="module-empty">Choose a workspace to preview its running app.</p>;
  if (unsupported) return <p className="module-empty">This Runner does not run workspace actions yet.</p>;

  if (!target) {
    const waiting = runtime?.actions.filter(a => a.url) || [];
    const first = waiting.find(a => !a.blocked) || waiting[0];
    const starting = first && (busy.has(first.key) || isLive(first.runState));
    return <section className="preview-panel" aria-label="App preview">
      <div className="runtime-empty preview-empty">
        <Icon name="eye" size={24}/>
        {!runtime ? <p role="status">Loading…</p> : first ? <>
          <p><strong>Start {actionName(first, runtime.actions)} to preview it.</strong></p>
          <p>Its page opens here while it runs{first.port ? `, on port ${first.port}` : ''}.</p>
          {first.blocked ? <p className="tone-warn-text">{first.blocked}</p> : <button type="button" className="primary" disabled={starting} onClick={() => void store?.startAction(first, false, runtime.actions)}><span aria-hidden="true">▶</span> {starting ? 'Starting…' : `Start ${actionName(first, runtime.actions)}`}</button>}
        </> : <>
          <p><strong>Nothing to preview.</strong></p>
          <p>An action with a <code>url:</code> in <code>.lazybox/workspace.yaml</code> shows its page here while it runs.</p>
        </>}
      </div>
    </section>;
  }
  const name = actionName(target, runtime!.actions);
  return <section className="preview-panel" aria-label="App preview">
    <WebViewer id={`preview-${workspaceId}`} label="Preview" frameTitle={`Preview of ${name}`} history={history} setHistory={setHistory} zoom={zoom} onZoom={setZoom} engine={engine}
      addressLabel="Preview address" reloadLabel="Reload preview"
      leading={targets.length > 1
        ? <select aria-label="Action to preview" value={target.key} onChange={e => setChosen(e.target.value)}>{targets.map(a => <option key={a.key} value={a.key}>{actionName(a, runtime!.actions)}</option>)}</select>
        : <span className="preview-target" title={target.run}><span className="run-state-dot tone-ok is-live" aria-hidden="true"/>{name}</span>}/>
  </section>;
}
