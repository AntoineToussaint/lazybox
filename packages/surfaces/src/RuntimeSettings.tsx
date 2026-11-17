import { useEffect, useState } from 'react';
import type { WorkspaceRuntimeSettings } from '@lazybox/protocol';
import { useRunner } from './RunnerContext';
import { errorText, unsupported } from './workspace-runtime';
import './workspace-runtime.css';

const placeholder = `# Same schema as .lazybox/workspace.yaml. Entries here add to or
# replace the repository's, never run on anyone else's machine,
# and need no approval: you wrote them.
actions:
  storybook:
    run: npm run storybook -- -p $LAZYBOX_PORT_1
    url: http://localhost:$LAZYBOX_PORT_1`;

/** Settings → Workspaces: the port range and your own per-repository runtime settings. */
export function RuntimeSettings() {
  const { api } = useRunner();
  const [settings, setSettings] = useState<WorkspaceRuntimeSettings>();
  const [error, setError] = useState(''), [missing, setMissing] = useState(false);
  const [busy, setBusy] = useState(false), [saved, setSaved] = useState('');
  const load = async () => {
    try { setSettings(await api.getWorkspaceRuntimeSettings({})); setError(''); }
    catch (e) { if (unsupported(e)) setMissing(true); else setError(errorText(e)); }
  };
  useEffect(() => { void load(); }, [api]);
  if (missing) return <p className="settings-empty">This Runner does not run workspace actions yet. Update Lazybox to use them.</p>;
  if (!settings) return <p role={error ? 'alert' : 'status'}>{error || 'Loading workspace settings…'}</p>;
  const change = (next: Partial<WorkspaceRuntimeSettings>) => { setSettings({ ...settings, ...next }); setSaved(''); };
  const changeRepository = (id: string, next: Record<string, unknown>) => change({ repositories: settings.repositories.map(r => r.repositoryId === id ? { ...r, ...next, error: '' } : r) });
  const rangeProblem = settings.portRangeStart < 1024 || settings.portRangeEnd > 65535 || settings.portRangeEnd - settings.portRangeStart < 9 ? 'Choose a range of at least 10 ports between 1024 and 65535.' : '';
  return <form className="runner-preferences runtime-settings" aria-labelledby="runtime-settings-title" onSubmit={async event => {
    event.preventDefault(); if (rangeProblem) return;
    setBusy(true); setSaved(''); setError('');
    try {
      const next = await api.saveWorkspaceRuntimeSettings(settings);
      setSettings(next);
      setSaved(next.repositories.some(r => r.error) ? '' : 'Workspace settings saved. Running actions keep their ports and scripts until they restart.');
    } catch (e) { setError(errorText(e)); }
    finally { setBusy(false); }
  }}>
    <h2 id="runtime-settings-title">Workspaces</h2>
    <p>Each repository can commit a <code>.lazybox/workspace.yaml</code>: a setup script that runs after its worktree is created, an archive script that runs before a workspace is deleted, the files to copy from your checkout, and actions to run. These settings stay on this Runner.</p>
    <fieldset className="runtime-ports">
      <legend>Ports</legend>
      <p>Every workspace gets its own block of 10 ports from this range (<code>LAZYBOX_PORT</code>, <code>LAZYBOX_PORT_0</code>…<code>_9</code>), kept for the workspace’s life, so two workspaces never fight over a port.</p>
      <div className="setup-row">
        <label>From<input type="number" min={1024} max={65535} required value={settings.portRangeStart || ''} onChange={e => change({ portRangeStart: Number(e.target.value) })}/></label>
        <label>To<input type="number" min={1024} max={65535} required value={settings.portRangeEnd || ''} onChange={e => change({ portRangeEnd: Number(e.target.value) })}/></label>
      </div>
      {rangeProblem ? <small className="tone-warn-text" role="alert">{rangeProblem}</small> : <small>{Math.floor((settings.portRangeEnd - settings.portRangeStart + 1) / 10)} workspaces fit. Workspaces that already have ports keep them.</small>}
    </fieldset>
    <h3>Your settings per repository</h3>
    {!settings.repositories.length && <p className="settings-empty">No repositories yet. Add one from a workspace; it appears here.</p>}
    {settings.repositories.map(repository => <details key={repository.repositoryId} className="runtime-repository" open={!!repository.error || !!repository.config || undefined}>
      <summary><strong>{repository.name || repository.repositoryId}</strong>{(repository.config || repository.defaultAction || repository.skipSetup) && <small>customized</small>}{repository.error && <small className="tone-warn-text">needs a fix</small>}</summary>
      <label>Default action<input placeholder="The repository’s default" maxLength={40} value={repository.defaultAction} onChange={e => changeRepository(repository.repositoryId, { defaultAction: e.target.value.trim() })}/></label>
      <label className="runtime-check"><input type="checkbox" checked={repository.skipSetup} onChange={e => changeRepository(repository.repositoryId, { skipSetup: e.target.checked })}/>Don’t run setup automatically for new workspaces (you can still run it from Actions)</label>
      <label>Your additions<textarea rows={7} spellCheck={false} className="mono" aria-label={`Your workspace settings for ${repository.name || repository.repositoryId}`} placeholder={placeholder} value={repository.config} onChange={e => changeRepository(repository.repositoryId, { config: e.target.value })}/></label>
      {repository.error && <p className="settings-error" role="alert">{repository.error}</p>}
    </details>)}
    {error && <p className="settings-error" role="alert">{error} <button type="button" onClick={() => void load()}>Reload settings</button></p>}
    {saved && <p role="status">{saved}</p>}
    <button className="primary" disabled={busy || !!rangeProblem}>{busy ? 'Saving…' : 'Save workspace settings'}</button>
  </form>;
}
