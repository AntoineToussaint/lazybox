import { useState, type FormEvent } from 'react';
import { Icon } from './Icons';
import { AddComputerSheet } from './AddComputerSheet';
import { RunnerHealthDot, useRunnerDirectory, type RunnerView } from './RunnerDirectory';
import { OWN_RUNNER, optInRunner, routeLabel, rttText, runnerStateText, withoutRunner, withRunnerShown } from './runner-directory';
import { SettingsHeader } from './settings-parts';

/**
 * Settings → Runners: every Runner this window can show — the launchers'
 * (Dev, Working, the installed release), any the person added, and Runners
 * on other computers (REMOTE_RUNNERS.md) — with its health and version;
 * which one the window opens on (primary), which are hidden, adding one by
 * its locator or endpoint file, and Add a computer. A Runner is never reached
 * with another's credentials: one on another computer is reached through a
 * link this computer's own key opens, and its owner bearer stays there.
 */
export function RunnersSettings() {
  const directory = useRunnerDirectory();
  const [path, setPath] = useState('');
  const [adding, setAdding] = useState(false);
  const [addingComputer, setAddingComputer] = useState(false);
  const [error, setError] = useState('');
  if (!directory) return <><SettingsHeader title="Runners" description="This window shows one Runner."/></>;
  const { runners, preferences, setPreferences, host, active } = directory;
  const setHidden = (row: RunnerView, hidden: boolean) => setPreferences(withRunnerShown(preferences, row.id, !hidden));
  // The primary Runner is shown: the window opens on it.
  const setPrimary = (row: RunnerView) => setPreferences(row.id === OWN_RUNNER ? { ...preferences, primary: '' } : { ...withRunnerShown(preferences, row.id, true), primary: row.id });
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
    try { await host.forget(row.id); setPreferences(withoutRunner(preferences, row.id)); await directory.reread(); }
    catch (failure) { setError(String(failure).replace(/^Error: /, '')); }
  };
  const remove = async (row: RunnerView) => {
    if (!host?.removeComputer || !row.listing) return;
    setError('');
    try { await host.removeComputer(row.listing.name); setPreferences(withoutRunner(preferences, row.id)); await directory.reread(); }
    catch (failure) { setError(String(failure).replace(/^Error: /, '')); }
  };
  return <>
    <SettingsHeader title="Runners" description="Every Runner this window can show: Dev and Working, the installed app's, a standalone Runner or another checkout, and Runners on your other computers. A window shows its own Runner; another Runner on this computer shows only when you turn on Show in this window. The page shows one at a time; Workspaces, Agents, the Inbox and ⌘K show every Runner shown."/>
    {directory.listError && <p className="settings-error" role="alert">Could not look for Runners: {directory.listError}</p>}
    {error && <p className="settings-error" role="alert">{error}</p>}
    {host?.addComputer && <div className="runners-computers">
      <div><strong>Runners on other computers</strong><small>A Linux box, a cloud VM, another Mac: anything you reach with <code>ssh</code>. Agents there keep working while this computer sleeps.</small></div>
      <button type="button" className="primary" onClick={() => setAddingComputer(true)}><Icon name="plus" size={13}/>Add a computer…</button>
    </div>}
    {addingComputer && host && <AddComputerSheet host={host} onClose={() => setAddingComputer(false)} onAdded={() => void directory.reread()}/>}
    <ul className="runners-list" aria-label="Runners">
      {runners.map(row => <li key={row.id} className="runners-item" data-runner={row.id} data-health={row.health} data-hidden={row.hidden || undefined}>
        <div className="runners-item-main">
          <RunnerHealthDot health={row.health}/>
          <div className="runners-item-text">
            <strong>{row.label}{row.primary && <span className="runners-tag is-primary" title="The window opens on this Runner">Primary</span>}{row.id === active && <span className="runners-tag">Showing</span>}{row.own && <span className="runners-tag is-quiet" title="The Runner this window was opened on">This window's</span>}</strong>
            <small>{[row.channel && row.channel !== row.label ? row.channel : '', row.version ? `version ${row.version.replace(/^v/, '')}` : '', row.hidden ? (row.listing && optInRunner(row.listing) && !preferences.hidden.includes(row.id) ? 'Not shown in this window' : 'Hidden') : runnerStateText(row)].filter(Boolean).join(' · ')}</small>
            {row.listing?.remote
              ? <small className="runners-remote" title={row.listing.remote.fingerprint ? `Pinned Runner key ${row.listing.remote.fingerprint}` : undefined}>{[`On ${row.listing.remote.machine || row.listing.remote.host}`, `${routeLabel(row.listing.remote.route)} ${row.listing.remote.host}`, rttText(row.listing.remote.rttMs), row.listing.remote.fingerprint].filter(Boolean).join(' · ')}</small>
              : row.listing?.locator && <code className="runners-path" title={row.listing.locator}>{row.listing.locator}</code>}
            {(row.listing?.remote?.state === 'refused' ? row.listing.remote.problem : row.health !== 'online' && row.listing?.problem) && <small className="runners-problem">{row.listing?.remote?.state === 'refused' ? row.listing.remote.problem : row.listing?.problem}</small>}
          </div>
        </div>
        <div className="runners-item-actions">
          {row.id !== active && <button type="button" disabled={!row.state || row.hidden || row.health === 'stopped'} onClick={() => directory.activate(row.id)}>Show</button>}
          <button type="button" aria-pressed={row.primary} disabled={row.primary} title="Open the window on this Runner when it is running" onClick={() => setPrimary(row)}><Icon name="pin" size={12}/>{row.primary ? 'Primary' : 'Make primary'}</button>
          <label className="settings-switch runners-toggle" title={row.own ? 'This window was opened on it' : row.hidden ? 'Show it in this window' : 'Hide it from this window'}>
            <input type="checkbox" role="switch" checked={!row.hidden} disabled={row.own} onChange={event => setHidden(row, !event.target.checked)}/>
            <span>Show in this window</span>
          </label>
          {row.listing?.source === 'added' && host?.forget && <button type="button" className="danger-link" onClick={() => void forget(row)} aria-label={`Forget ${row.label}`}>Forget</button>}
          {row.listing?.remote && host?.removeComputer && <button type="button" className="danger-link" title={`Forget it here and revoke this computer's key on ${row.listing.remote.host}. Its agents and workspaces stay there.`} onClick={() => void remove(row)} aria-label={`Remove ${row.label} from this computer`}>Remove</button>}
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
    <p className="settings-note">A computer added here is yours: this computer gets full owner access to its Runner, over SSH, with a key of its own that it can revoke there (<code>lazybox link owners revoke</code>). A phone or a computer you trust less is paired as a device instead (Settings → Devices on that computer).</p>
  </>;
}
