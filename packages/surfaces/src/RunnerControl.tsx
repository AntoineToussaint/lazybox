import { useEffect, useRef, useState } from 'react';
import { DraggableDialog, DialogHeader } from './DraggableDialog';
import type { ProcessInventory } from '@lazybox/protocol';
import { useRunner } from './RunnerContext';
import { useRunnerIdentity } from './RunnerDirectory';
import { useRunnerSettings } from './RunnerSettings';
import { Icon } from './Icons';
import { SettingsGroup, SettingsHeader, SettingsRow, settingsError } from './settings-parts';
import './runner-control.css';
import './processes.css';

/** The Runner's state in a word, restart first: it explains a dropped connection. */
function runnerState(runner: ReturnType<typeof useRunner>): { label: string; tone: 'ok' | 'busy' | 'bad' } {
  const { restart, connecting, error } = runner;
  if (restart.phase === 'restarting') return { label: 'Restarting…', tone: 'busy' };
  if (restart.phase === 'restarted') return { label: 'Restarted', tone: 'ok' };
  if (restart.phase === 'failed') return { label: 'Restart failed', tone: 'bad' };
  if (connecting) return { label: 'Connecting', tone: 'busy' };
  // Header says so when the Runner refuses this window's bearer, even while Info itself answered.
  if (error) return { label: runner.authFailed ? 'Not authorized' : 'Disconnected', tone: 'bad' };
  return { label: 'Connected', tone: 'ok' };
}

/** Header status. Opens the Runner settings when the window has them. */
export function RunnerIndicator({ onOpen }: { onOpen?(): void }) {
  const runner = useRunner();
  const { label: state, tone } = runnerState(runner);
  // With several Runners in the window, which one the page shows: "Working · Connected".
  const identity = useRunnerIdentity();
  const label = identity.multi && identity.label ? `${identity.label} · ${state}` : state;
  const detail = runner.restart.phase === 'failed' ? runner.restart.message : runner.error || (runner.connecting ? 'Connecting to Runner' : 'Runner connected');
  const className = `runner-indicator tone-${tone}${tone === 'bad' ? ' offline' : ''}`;
  if (!onOpen) return <span className={className} title={detail}><i/>{label}</span>;
  return <button type="button" className={className} title={`${detail}. Open Runner settings`} aria-label={`Runner status: ${label} (open Runner settings)`} onClick={onOpen}><i/>{label}</button>;
}

/** Workspace-wide notice for a restart the person asked for. */
export function RestartNotice() {
  const { restart, dismissRestart, restartRunner } = useRunner();
  if (restart.phase === 'restarting') return <div className="runner-notice tone-busy" role="status"><strong>Restarting the Runner…</strong> Agents, commands and terminals keep running. This window reconnects by itself.</div>;
  if (restart.phase === 'restarted') return <div className="runner-notice tone-ok" role="status"><strong>Runner restarted.</strong> Agents, commands and terminals kept running and are connected again.<button onClick={dismissRestart}>Dismiss</button></div>;
  if (restart.phase === 'failed') return <div className="error runner-notice" role="alert"><strong>Could not restart the Runner.</strong> {restart.message}<button onClick={() => void restartRunner()}>Try again</button><button onClick={dismissRestart}>Dismiss</button></div>;
  return null;
}

/** Settings → Runner: status, and Restart Runner when a launcher will start it again. */
export function RunnerSection({ onShowProcesses }: { onShowProcesses?(): void } = {}) {
  const runner = useRunner();
  const { info, restart, restartRunner } = runner;
  const identity = useRunnerIdentity();
  const [confirming, setConfirming] = useState(false);
  const { label, tone } = runnerState(runner);
  const available = !!info?.restartAvailable && !!info.instanceId;
  const restarting = restart.phase === 'restarting';
  return <section className="runner-preferences runner-control" aria-labelledby="runner-heading">
    <SettingsHeader id="runner-heading" title="Runner" description={identity.multi && identity.label ? `${identity.label}, the Runner this page shows. It runs its agents, managed commands and terminals; Settings → Runners lists the others.` : 'The local service behind this window. It runs your agents, managed commands and terminals.'}/>
    <SettingsGroup>
      <SettingsRow label="Status" description={!info && !restarting ? 'Waiting for the Runner…' : available || restarting ? 'Restarting loads its current code and configuration. Agents, managed commands and terminals keep running, and this window reconnects by itself.' : undefined}>
        <div className="runner-status-control">
          <span className={`runner-indicator tone-${tone}${tone === 'bad' ? ' offline' : ''}`}><i/>{label}</span>
          {(available || restarting) && <button type="button" disabled={restarting} onClick={() => setConfirming(true)}><Icon name="refresh" size={14}/>{restarting ? 'Restarting…' : restart.phase === 'failed' ? 'Try restarting again…' : 'Restart Runner…'}</button>}
        </div>
      </SettingsRow>
      {info && !available && !restarting && <div className="runner-unavailable" role="note">
        <p><strong>Restart is not available for this Runner.</strong> Nothing would start it again once it stops.</p>
        <p>A Runner launched with <code>make dev</code> or <code>make working</code> can be restarted here. For the Codefly-managed Working service, run <code>make runner-service-stop</code> and then <code>make runner-service-start</code>. Agents keep running across either restart.</p>
      </div>}
    </SettingsGroup>
    <ProcessHostingSettings onShowProcesses={onShowProcesses}/>
    {confirming && <RestartRunnerDialog onClose={() => setConfirming(false)} onConfirm={() => { setConfirming(false); void restartRunner(); }}/>}
  </section>;
}

function RestartRunnerDialog({ onClose, onConfirm }: { onClose(): void; onConfirm(): void }) {
  const dialog = useRef<HTMLDialogElement>(null), confirm = useRef<HTMLButtonElement>(null);
  // Restarting is the reason the dialog opened and it is not destructive: Enter confirms, Escape cancels.
  useEffect(() => { dialog.current?.showModal(); confirm.current?.focus(); }, []);
  return <DraggableDialog ref={dialog} className="restart-runner-dialog" aria-labelledby="restart-runner-title" aria-describedby="restart-runner-description" onCancel={onClose} onClose={onClose}>
    <DialogHeader><h2 id="restart-runner-title">Restart the Runner?</h2><button type="button" aria-label="Cancel restart" onClick={onClose}>×</button></DialogHeader>
    <p id="restart-runner-description">Agents, managed commands and terminals keep running. The new Runner re-attaches to them and this window reconnects by itself.</p>
    <p className="restart-note">Usually a few seconds; longer in Dev while the Runner is rebuilt. Something you send while it restarts may need to be sent again.</p>
    <footer>
      <button type="button" onClick={onClose}>Cancel</button>
      <button ref={confirm} type="button" className="primary" autoFocus data-autofocus onClick={onConfirm}>Restart Runner</button>
    </footer>
  </DraggableDialog>;
}

/** Settings → Runner: how new agents, commands and terminals are hosted. Applies to the
 * next process, without a restart; running processes keep the backend they started with. */
function ProcessHostingSettings({ onShowProcesses }: { onShowProcesses?(): void }) {
  const { api } = useRunner();
  const { settings, setSettings, error, setError } = useRunnerSettings();
  const [inventory, setInventory] = useState<ProcessInventory>();
  const [saving, setSaving] = useState(''), [saved, setSaved] = useState('');
  useEffect(() => {
    const control = new AbortController();
    const load = () => void api.listProcesses({}, { signal: control.signal, timeoutMs: 8000 }).then(value => { if (!control.signal.aborted) setInventory(value); }, () => { /* the section still works without it */ });
    load(); const timer = setInterval(load, 5000);
    return () => { control.abort(); clearInterval(timer); };
  }, [api]);
  const hosting = inventory?.hosting, configured = hosting?.configured || 'sessionhost';
  // The choice shows at once; the radio settles on what the Runner saved.
  const chosen = saving || settings?.processBackend || configured;
  const running = inventory?.processes.filter(p => p.attached).length ?? 0;
  const tmuxReady = !!hosting?.tmuxVersion;
  const choose = async (backend: string) => {
    if (!settings || backend === chosen) return;
    setSaving(backend); setSaved(''); setError('');
    try {
      // Choosing the configured backend follows the Runner configuration again.
      setSettings(await api.saveRunnerSettings({ ...settings, processBackend: backend === configured ? '' : backend }));
      window.dispatchEvent(new Event('lazybox-settings-saved'));
      setSaved(backend === 'tmux' ? 'New processes will run in tmux. Running processes keep their hosts.' : 'New processes will run in session hosts. Running processes keep their hosts.');
      setInventory(await api.listProcesses({}));
    } catch (e) { setError(settingsError(e)); }
    finally { setSaving(''); }
  };
  return <section className="runner-hosting" aria-labelledby="hosting-heading">
    <SettingsGroup id="hosting-heading" title="Process hosting" description="Agents, managed commands and terminals each run in a host that outlives the Runner, so a restart never stops them. A change applies to processes started from now on.">
    <fieldset disabled={!!saving || !settings}>
      <legend>Host new processes in</legend>
      <label className="hosting-choice"><input type="radio" name="process-backend" value="sessionhost" checked={chosen === 'sessionhost'} onChange={() => void choose('sessionhost')}/>
        <strong>Session hosts{configured === 'sessionhost' ? ' (default)' : ''}</strong>
        <span>Each process runs in a detached host of its own. Nothing to install.</span></label>
      <label className="hosting-choice"><input type="radio" name="process-backend" value="tmux" checked={chosen === 'tmux'} disabled={!tmuxReady && chosen !== 'tmux'} onChange={() => void choose('tmux')}/>
        <strong>tmux{configured === 'tmux' ? ' (default)' : ''}</strong>
        <span>Each process also gets a window in a private tmux server, so you can watch it from any terminal with the attach command the Processes panel copies. Closing a window stops its process.{' '}
          {!hosting ? '' : tmuxReady ? `Found ${hosting.tmuxVersion}.` : `Unavailable: ${hosting.tmuxProblem}.`}</span></label>
    </fieldset>
    {chosen === 'tmux' && hosting?.fallback && <p className="processes-notice tone-warn" role="status">{hosting.fallback}</p>}
    {error && <p className="settings-inline-error hosting-message" role="alert">{error}</p>}{saved && <p className="settings-saved hosting-message" role="status"><Icon name="check" size={14}/>{saved}</p>}
    <div className="runner-processes-summary"><span>{inventory ? `${running} ${running === 1 ? 'process' : 'processes'} running` : 'Counting processes…'}</span>{onShowProcesses && <button type="button" className="quiet" onClick={onShowProcesses}><Icon name="processes" size={14}/>Show processes</button>}</div>
    </SettingsGroup>
  </section>;
}
