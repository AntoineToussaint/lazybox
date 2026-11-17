import { useEffect, useState } from 'react';
import type { Collection, OrchestrationProcess } from '@lazybox/protocol';
import { useRunner } from './RunnerContext';
import { processCards } from './orchestration-process-model';
import { SettingsHeader, settingsError } from './settings-parts';
import './orchestration-process.css';

/**
 * Settings → Process: the orchestration process a mission would run, read-only.
 * Layers: built-in preset ← your process file ← the workspace repository's
 * .lazybox/process.yaml, under your ceiling. Editing comes later (slice 4).
 */
export function OrchestrationProcessSettings() {
  const { api } = useRunner();
  const [workspaces, setWorkspaces] = useState<Collection[]>([]);
  const [workspace, setWorkspace] = useState('');
  const [repository, setRepository] = useState('');
  const [shown, setShown] = useState<OrchestrationProcess>();
  const [error, setError] = useState('');
  useEffect(() => {
    const control = new AbortController();
    void api.listCollections({}, { signal: control.signal, timeoutMs: 8000 }).then(r => { if (!control.signal.aborted) setWorkspaces(r.collections); }, () => { /* the presets still show */ });
    return () => control.abort();
  }, [api]);
  useEffect(() => {
    const control = new AbortController();
    setError('');
    void api.getOrchestrationProcess({ collectionId: workspace, repository, tasks: 0 }, { signal: control.signal, timeoutMs: 8000 })
      .then(next => { if (!control.signal.aborted) setShown(next); }, e => { if (!control.signal.aborted) { setShown(undefined); setError(settingsError(e, 'load')); } });
    return () => control.abort();
  }, [api, workspace, repository]);
  const cards = shown?.json ? processCards(shown.json) : undefined;
  const errors = shown?.issues.filter(i => i.severity === 'error') || [];
  const warnings = shown?.issues.filter(i => i.severity !== 'error') || [];
  return <section className="orchestration-process" aria-labelledby="process-heading">
    <SettingsHeader id="process-heading" title="Process" description={<>How a mission runs: its phases, the gates between them, and the roles that do the work. Read-only for now; edit <code>.lazybox/process.yaml</code> in a repository{shown?.userFile ? <>, or your own file at <code>{shown.userFile}</code></> : null}. Check either with <code>lazybox process validate</code>.</>}/>
    <div className="process-scope">
      <label><span>For workspace</span><select value={workspace} onChange={e => { setWorkspace(e.target.value); setRepository(''); }}>
        <option value="">None: presets and your file</option>
        {workspaces.map(w => <option key={w.id} value={w.id}>{w.name}</option>)}
      </select></label>
      {(shown?.candidates.length || 0) > 1 && <label><span>Repository</span><select value={repository} onChange={e => setRepository(e.target.value)}>
        <option value="">Choose…</option>
        {shown!.candidates.map(c => <option key={c.name} value={c.name}>{c.name}</option>)}
      </select></label>}
    </div>
    {error && <p className="settings-error" role="alert">{error}</p>}
    {!shown && !error && <p className="settings-empty" role="status">Loading the process…</p>}
    {shown && <>
      <p className="process-layers" aria-label="Layers">{shown.layers.map(l => <span key={l.kind} title={l.digest}>{l.kind === 'preset' ? `Preset ${l.source}` : l.kind === 'user' ? 'Your file' : l.source}</span>)}</p>
      {shown.notice && <p className="processes-notice tone-warn" role="status">{shown.notice}</p>}
      <p className={`process-verdict ${shown.valid ? 'tone-ok' : 'tone-bad'}`} role="status">{shown.valid ? `Valid · ${cards?.name || shown.preset} · autonomy ${cards?.autonomy ?? ''}${cards?.budget ? ` · ${cards.budget} per mission` : ''}` : `${errors.length} problem${errors.length === 1 ? '' : 's'}: a mission could not run this process`}</p>
      {[...errors, ...warnings].length > 0 && <ul className="process-issues">{[...errors, ...warnings].map((i, n) => <li key={n} className={i.severity === 'error' ? 'tone-bad' : 'tone-warn'}>
        <code>{i.source}{i.line ? `:${i.line}` : ''}</code> {i.path && <strong>{i.path}</strong>} {i.message}
      </li>)}</ul>}
      {cards && <>
        <h3>Phases</h3>
        <ol className="process-phases">{cards.phases.map(p => <li key={p.id}><strong>{p.id}</strong><span>exits through <em>{p.exit}</em> ({p.gate})</span>{p.detail && <small>{p.detail}</small>}</li>)}</ol>
        <h3>Roles</h3>
        <div className="subscription-grid">{cards.roles.map(r => <article className="subscription-card process-role" key={r.name} aria-label={`${r.name} role`}>
          <header><div className="subscription-identity"><h3>{r.name}</h3><span>{r.providers || 'no provider'}{r.model ? ` · ${r.model}` : ''}</span></div><span className={`account-state${r.permission === 'YOLO' ? ' tone-warn' : ''}`}>{r.permission}</span></header>
          <div className="process-role-body">
            {r.scopes.length > 0 && <p className="process-scopes">{r.scopes.map(s => <code key={s}>{s}</code>)}</p>}
            {r.wakes.length > 0 ? <ul>{r.wakes.map(w => <li key={w}>{w}</li>)}</ul> : <small>No triggers: rules or another role start it</small>}
            {r.budget && <small>Budget {r.budget}</small>}
          </div>
        </article>)}</div>
        <p className="usage-note">{cards.rules} rule{cards.rules === 1 ? '' : 's'} move the work between phases. <code>lazybox process explain &lt;event&gt;</code> shows what fires on an event, in order.</p>
      </>}
    </>}
  </section>;
}
