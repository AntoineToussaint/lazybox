import { useState, type FormEvent } from 'react';
import { Icon } from './Icons';
import { RunnerHealthDot, useRunnerDirectory, type RunnerView } from './RunnerDirectory';
import { healthLabel, OWN_RUNNER } from './runner-directory';
import { SettingsHeader } from './settings-parts';

/**
 * Settings → Runners: every Runner this window can show — the launchers'
 * (Dev, Working, the installed release) and any the person added — with its
 * health and version; which one the window opens on (primary), which are
 * hidden, and adding one by its locator or endpoint file. A Runner is never
 * reached with another's credentials, and Runners on other machines are
 * reached only as paired devices (MULTI_RUNNER.md step 3), not from here.
 */
export function RunnersSettings() {
  const directory = useRunnerDirectory();
  const [path, setPath] = useState('');
  const [adding, setAdding] = useState(false);
  const [error, setError] = useState('');
  if (!directory) return <><SettingsHeader title="Runners" description="This window shows one Runner."/></>;
  const { runners, preferences, setPreferences, host, active } = directory;
  const setHidden = (row: RunnerView, hidden: boolean) => setPreferences({ ...preferences, hidden: hidden ? [...preferences.hidden, row.id] : preferences.hidden.filter(id => id !== row.id), primary: hidden && preferences.primary === row.id ? '' : preferences.primary });
  const setPrimary = (row: RunnerView) => setPreferences({ ...preferences, primary: row.id === OWN_RUNNER ? '' : row.id, hidden: preferences.hidden.filter(id => id !== row.id) });
  const add = async (event: FormEvent) => {
    event.preventDefault();
    if (!host?.add || !path.trim()) return;
    setAdding(true); setError('');
    try { await host.add(path.trim()); setPath(''); await directory.reread(); }
    catch (failure) { setError(String(failure).replace(/^Error: /, '')); }
    finally { setAdding(false); }
  };
  const forget = async (row: RunnerView) => {
    if (!host?.forget) return;
    setError('');
    try { await host.forget(row.id); setPreferences({ hidden: preferences.hidden.filter(id => id !== row.id), primary: preferences.primary === row.id ? '' : preferences.primary }); await directory.reread(); }
    catch (failure) { setError(String(failure).replace(/^Error: /, '')); }
  };
  return <>
    <SettingsHeader title="Runners" description="Every Runner this window shows: Dev and Working, the installed app's, a standalone Runner or another checkout. The page shows one at a time; Workspaces, Agents, the Inbox and ⌘K show them all."/>
    {directory.listError && <p className="settings-error" role="alert">Could not look for Runners: {directory.listError}</p>}
    {error && <p className="settings-error" role="alert">{error}</p>}
    <ul className="runners-list" aria-label="Runners">
      {runners.map(row => <li key={row.id} className="runners-item" data-runner={row.id} data-health={row.health} data-hidden={row.hidden || undefined}>
        <div className="runners-item-main">
          <RunnerHealthDot health={row.health}/>
          <div className="runners-item-text">
            <strong>{row.label}{row.primary && <span className="runners-tag is-primary" title="The window opens on this Runner">Primary</span>}{row.id === active && <span className="runners-tag">Showing</span>}{row.own && <span className="runners-tag is-quiet" title="The Runner this window was opened on">This window's</span>}</strong>
            <small>{[row.channel && row.channel !== row.label ? row.channel : '', row.version ? `version ${row.version.replace(/^v/, '')}` : '', row.hidden ? 'Hidden' : healthLabel[row.health]].filter(Boolean).join(' · ')}</small>
            {row.listing?.locator && <code className="runners-path" title={row.listing.locator}>{row.listing.locator}</code>}
            {row.listing?.problem && row.health !== 'online' && <small className="runners-problem">{row.listing.problem}</small>}
          </div>
        </div>
        <div className="runners-item-actions">
          {row.id !== active && <button type="button" disabled={!row.state || row.hidden || row.health === 'stopped'} onClick={() => directory.activate(row.id)}>Show</button>}
          <button type="button" aria-pressed={row.primary} disabled={row.primary} title="Open the window on this Runner when it is running" onClick={() => setPrimary(row)}><Icon name="pin" size={12}/>{row.primary ? 'Primary' : 'Make primary'}</button>
          <label className="settings-switch runners-toggle" title={row.own ? 'This window was opened on it' : row.hidden ? 'Show it in this window' : 'Hide it from this window'}>
            <input type="checkbox" role="switch" checked={!row.hidden} disabled={row.own} onChange={event => setHidden(row, !event.target.checked)}/>
            <span>Shown</span>
          </label>
          {row.listing?.source === 'added' && host?.forget && <button type="button" className="danger-link" onClick={() => void forget(row)} aria-label={`Forget ${row.label}`}>Forget</button>}
        </div>
      </li>)}
    </ul>
    {host?.add && <form className="runners-add" onSubmit={event => void add(event)}>
      <label htmlFor="runner-locator-path"><strong>Add a Runner on this computer</strong><small>Its locator (<code>~/.config/lazybox/runners/*.json</code>) or the endpoint file it writes: another checkout, a standalone Runner. Only a loopback Runner whose file only you can read is accepted.</small></label>
      <div className="runners-add-row">
        <input id="runner-locator-path" className="mono" spellCheck={false} autoComplete="off" placeholder="/path/to/endpoint.json" value={path} disabled={adding} onChange={event => setPath(event.target.value)}/>
        <button type="submit" className="primary" disabled={adding || !path.trim()}>{adding ? 'Adding…' : 'Add'}</button>
      </div>
    </form>}
    {!host && <p className="settings-note">This window finds no other Runners. The desktop app lists every Runner <code>lazybox runners</code> shows.</p>}
    <p className="settings-note">A Runner on another machine is reached as a paired device over Tailscale, with the grants it gave (see <em>Several Runners in one window</em> in the docs). It is not added here.</p>
  </>;
}
