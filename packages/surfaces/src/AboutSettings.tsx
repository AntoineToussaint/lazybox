import { useEffect, useState } from 'react';
import type { AppHost, AppInfo } from '@lazybox/platform';
import type { AgentCliCheck } from '@lazybox/protocol';
import { useRunner } from './RunnerContext';
import { useRunnerIdentity } from './RunnerDirectory';
import { SettingsGroup, SettingsHeader, SettingsRow, useNow } from './settings-parts';
import { checkForUpdate, setAutomaticChecks, stopSkipping, useAppUpdate } from './app-update';
import { UpdateDialog } from './UpdateDialog';
import { Icon } from './Icons';
import { openOnboarding } from './Onboarding';
import { agentCliStatus } from './lazybox-cli';
import { lastCheckedText, progressText, updatePath, versionAgreement, type UpdatePath } from './update-model';
import './about.css';
import './updates.css';

const buildDateFormat = new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'short' });
const FEED = 'github.com/AntoineToussaint/lazybox';

/** Versions, update controls, and CLI diagnostics for this installation. */
export function AboutSettings({ app }: { app?: AppHost }) {
  const { api, revision } = useRunner();
  const { own, label: runnerLabel } = useRunnerIdentity();
  const update = useAppUpdate(app);
  const now = useNow(30_000);
  const [info, setInfo] = useState<AppInfo>();
  const [runnerVersion, setRunnerVersion] = useState<string>();
  const [agentCli, setAgentCli] = useState<AgentCliCheck>();
  const [link, setLink] = useState<{ link: string; hints: string[] } | { error: string }>();
  const linkCli = app?.linkCli;
  const [dialog, setDialog] = useState(false);
  const [moving, setMoving] = useState<'idle' | 'moving' | 'done' | string>('idle');
  useEffect(() => {
    let current = true;
    setInfo(undefined);
    setLink(undefined);
    void app?.info().then(value => { if (current) setInfo(value); }, () => {});
    return () => { current = false; };
  }, [app]);
  useEffect(() => {
    const control = new AbortController();
    void api.version({}, { signal: control.signal, timeoutMs: 5000 }).then(v => {
      if (control.signal.aborted) return;
      setRunnerVersion(v.version);
      setAgentCli(v.agentCli);
    }, () => {
      if (control.signal.aborted) return;
      setRunnerVersion('');
      setAgentCli(undefined);
    });
    return () => control.abort();
  }, [api, revision]);
  const builtAt = info?.builtAtUnixSeconds ? new Date(info.builtAtUnixSeconds * 1000) : undefined;
  const release = info?.channel === 'release';
  const path = updatePath(info);
  const updater = path.kind === 'updater' && !!app?.checkForUpdate;
  const available = update.available;
  const agreement = versionAgreement({ release, ownRunner: own, app: info?.version || '', runner: runnerVersion ?? '' });
  const cli = agentCli ? agentCliStatus(agentCli) : undefined;
  const move = async () => {
    setMoving('moving');
    try { await app?.moveRunner?.(); setMoving('done'); } catch (error) { setMoving(String(error)); }
  };
  const checkError = update.error?.phase === 'check' ? update.error : undefined;
  return <>
    <SettingsHeader title="About & updates" description={app && !info ? undefined : path.text}
      actions={<>
        <button onClick={() => openOnboarding('whats-new')}><Icon name="notes" size={14}/> What's new</button>
        {updater && <button disabled={update.checking || !!update.installing} onClick={() => void checkForUpdate(app)}><Icon name="refresh" size={14}/> {update.checking ? 'Checking…' : 'Check now'}</button>}
      </>}/>
    {path.kind === 'source' && <SourceBuildGroup path={path}/>}
    {updater && <SettingsGroup title="Update status" className="update-status-group">
      <div className={`update-status tone-${checkError ? 'warn' : available && update.offer !== 'none' ? 'new' : 'ok'}`} role="status" aria-live="polite">
        <span className="update-status-icon" aria-hidden="true"><Icon name={checkError ? 'alert' : available ? 'download' : 'check'} size={18}/></span>
        <div className="update-status-text">
          <strong>{update.installing ? progressText(update.installing, available?.version)
            : update.checking ? 'Checking for updates…'
            : available ? `Lazybox ${available.version} is available`
            : checkError ? 'Could not check for updates'
            : update.checked ? `Lazybox ${info?.version} is up to date` : `Lazybox ${info?.version}`}</strong>
          <small>{checkError ? checkError.text : available
            ? update.offer === 'skipped' ? `You skipped this version. You have ${available.current}.`
              : update.offer === 'snoozed' && update.prefs.remind ? `You asked to be reminded ${new Date(update.prefs.remind.until).toLocaleString(undefined, { weekday: 'long', hour: '2-digit', minute: '2-digit' })}. You have ${available.current}.`
              : `You have ${available.current}. Read what changed, then install when it suits you.`
            : <span title={update.prefs.lastChecked ? new Date(update.prefs.lastChecked).toLocaleString() : undefined}>{lastCheckedText(update.prefs.lastChecked, now)}</span>}</small>
          {available && checkError && <small>{lastCheckedText(update.prefs.lastChecked, now)}</small>}
        </div>
        <div className="update-status-actions">
          {available && !update.installing && <button className="primary" onClick={() => setDialog(true)}>{update.offer === 'skipped' ? 'Review anyway…' : 'What\'s new and install…'}</button>}
          {available && update.offer === 'skipped' && <button className="quiet" onClick={stopSkipping}>Stop skipping</button>}
          {update.installing && <button onClick={() => setDialog(true)}>Details</button>}
          {!available && checkError && <button onClick={() => void checkForUpdate(app)} disabled={update.checking}>Try again</button>}
        </div>
      </div>
    </SettingsGroup>}
    {updater && <SettingsGroup title="Update preferences" className="update-rows">
      <SettingsRow label="Check automatically" htmlFor="updates-automatic" description={update.prefs.automatic
        ? 'Every six hours while Lazybox is open. A check reads one small file from GitHub and sends nothing about you. Nothing is downloaded until you choose Install.'
        : 'Off: Lazybox looks for a new version only when you choose Check now.'}>
        <span className="settings-row-control settings-switch"><input id="updates-automatic" type="checkbox" role="switch" checked={update.prefs.automatic} onChange={event => setAutomaticChecks(event.target.checked)}/></span>
      </SettingsRow>
      <SettingsRow label="Channel" description={`Stable releases from ${FEED}. Prereleases are never offered.`}>
        <span className="update-pill">Stable</span>
      </SettingsRow>
      <SettingsRow label="Signature" description="Every update is checked against the signing key built into this app before anything is replaced. A download that does not match is refused.">
        <span className="update-pill tone-ok"><Icon name="shield" size={12}/> Required</span>
      </SettingsRow>
      <SettingsRow label="Running agents" description="Installing restarts the Runner, never your agents: they keep running and the new Runner picks them up. You are told how many are running before you confirm.">
        <span className="update-pill tone-ok"><Icon name="check" size={12}/> Kept</span>
      </SettingsRow>
    </SettingsGroup>}
    <SettingsGroup title="Versions" className="update-rows" description={release && own ? 'The app, its Runner and the lazybox command are one release and move together.' : undefined}>
      {info && <SettingsRow label="Lazybox app" description={release ? undefined : `Build ${info.channel}`}>
        <span className="about-version">{info.version}</span>
      </SettingsRow>}
      {info && <SettingsRow label="Desktop build time">
        {builtAt ? <time className="about-version" dateTime={builtAt.toISOString()} title={builtAt.toISOString()}>{buildDateFormat.format(builtAt)}</time>
          : <span className="about-version">Unavailable</span>}
      </SettingsRow>}
      <SettingsRow label={own ? "Runner" : runnerLabel || "Selected Runner"} description={own ? "The background service your agents run in" : "The Runner selected in this window"}>
        <span className={`about-version${agreement.agree ? '' : ' is-warn'}`}>{runnerVersion === undefined ? '…' : runnerVersion || 'not answering'}</span>
      </SettingsRow>
      {!agreement.agree && <div className="update-mismatch" role="alert">
        <Icon name="alert" size={14}/>
        <p>{agreement.problem}</p>
        {agreement.canMove && app?.moveRunner && <button disabled={moving === 'moving'} onClick={() => void move()}>{moving === 'moving' ? 'Moving…' : 'Move the Runner now'}</button>}
      </div>}
      {moving !== 'idle' && moving !== 'moving' && <p className={moving === 'done' ? 'update-muted' : 'settings-error'} role="status">{moving === 'done' ? 'The Runner restarted on this release. Agents kept running.' : moving}</p>}
    </SettingsGroup>
    {((agentCli && cli) || (release && linkCli)) && <SettingsGroup title="Command line">
      {agentCli && cli && <SettingsRow label="lazybox for agents" description={agentCli.notice || (agentCli.problem ? `Could not check ${agentCli.shell}: ${agentCli.problem}` : `Agents run ${agentCli.cli}`)}>
        <span className={`about-version${cli.tone === 'ok' ? '' : ' is-warn'}`} role="status" title={agentCli.unfixed.join('\n') || agentCli.shadowIn.join(', ') || undefined}>{cli.label}</span>
      </SettingsRow>}
      {release && linkCli && <SettingsRow label="lazybox on your PATH" description={info?.cli ? `Runs ${info.cli}` : 'Run lazybox from your terminal.'}>
        <button onClick={() => void linkCli().then(setLink, error => setLink({ error: String(error) }))}><Icon name="terminal" size={14}/> Add to terminal</button>
      </SettingsRow>}
      {link && ('error' in link ? <p className="settings-error" role="alert">{link.error}</p> : <div className="about-link" role="status">
        <p>Linked at <code>{link.link}</code>. Open a new terminal and run <code>lazybox doctor</code>.</p>
        {link.hints.map(h => <p key={h} className="about-hint">{h}</p>)}
      </div>)}
    </SettingsGroup>}
    {dialog && available && <UpdateDialog app={app} update={available} onClose={() => setDialog(false)}/>}
  </>;
}

/** A build from source (`make install`): which commit, and how to get a newer one. */
function SourceBuildGroup({ path }: { path: UpdatePath }) {
  return <SettingsGroup title="Built from source" className="update-rows">
    <SettingsRow label="Update" description="In a terminal, in the folder you cloned: pulls the newest commit, builds it and reinstalls. Your conversations and settings are kept; running agents keep running.">
      <code className="about-version">make update</code>
    </SettingsRow>
    {path.commit && <SettingsRow label="Commit" description="The commit this copy was built from">
      <span className="about-version">{path.commit}</span>
    </SettingsRow>}
  </SettingsGroup>;
}
