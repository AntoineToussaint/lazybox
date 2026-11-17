import { useEffect, useState } from 'react';
import type { AppHost, AppInfo } from '@lazybox/platform';
import type { AgentCliCheck } from '@lazybox/protocol';
import { useRunner } from './RunnerContext';
import { SettingsGroup, SettingsHeader, SettingsRow } from './settings-parts';
import { useAppUpdate } from './app-update';
import { openOnboarding } from './Onboarding';
import { Icon } from './Icons';
import { agentCliStatus } from './lazybox-cli';
import { updatePath } from './update-model';
import './about.css';

/**
 * Settings → About: the one version the app, its Runner and its CLI share,
 * updates for an installed release, and the CLI on PATH.
 */
export function AboutSettings({ app, onShowUpdates }: { app?: AppHost; onShowUpdates?(): void }) {
  const { api, revision } = useRunner();
  const [info, setInfo] = useState<AppInfo>();
  const [runnerVersion, setRunnerVersion] = useState('');
  const [agentCli, setAgentCli] = useState<AgentCliCheck>();
  const [link, setLink] = useState<{ link: string; hints: string[] } | { error: string }>();
  const update = useAppUpdate(app);
  useEffect(() => { void app?.info().then(setInfo, () => {}); }, [app]);
  useEffect(() => {
    const control = new AbortController();
    void api.version({}, { signal: control.signal, timeoutMs: 5000 }).then(v => { setRunnerVersion(v.version); setAgentCli(v.agentCli); }, () => setRunnerVersion(''));
    return () => control.abort();
  }, [api, revision]);
  const release = info?.channel === 'release';
  const mismatch = release && runnerVersion && info && runnerVersion !== info.version;
  const path = updatePath(info);
  return <>
    <SettingsHeader title="About Lazybox" description={path.kind === 'source' ? `This app, its Runner and the lazybox command are one build. ${path.text}` : release ? 'This app, its Runner and the lazybox command are one release and update together.' : path.text}
      actions={<button onClick={() => openOnboarding('whats-new')}><Icon name="notes" size={14}/> What's new</button>}/>
    <SettingsGroup title="Version">
      <SettingsRow label="Lazybox" description={path.kind === 'source' ? `Built from source${path.commit ? ` at ${path.commit}` : ''}` : release ? 'The installed app' : info ? `Build ${info.channel}` : 'In a browser, the app version is the Runner\'s'}>
        <span className="about-version">{info?.version || '—'}</span>
      </SettingsRow>
      <SettingsRow label="Runner" description={mismatch ? 'Different from the app: it moves to the app\'s version when the app next starts.' : 'The background service your agents run in'}>
        <span className={`about-version${mismatch ? ' is-warn' : ''}`}>{runnerVersion || 'not answering'}</span>
      </SettingsRow>
    </SettingsGroup>
    {info?.updater && <SettingsGroup title="Updates">
      <SettingsRow label={update.available && update.offer !== 'none' ? `Lazybox ${update.available.version} is available` : 'Updates'}
        description={update.prefs.automatic ? 'Checked every few hours; installed only when you say so.' : 'Automatic checks are off.'}>
        <button onClick={onShowUpdates}><Icon name="download" size={14}/> Updates…</button>
      </SettingsRow>
    </SettingsGroup>}
    {agentCli && <AgentCliRow check={agentCli}/>}
    {app?.linkCli && release && <SettingsGroup title="Command line" description="The lazybox command does from a terminal what you do here: lazybox ls, lazybox send, lazybox doctor.">
      <SettingsRow label="lazybox on your PATH" description={info?.cli ? `Runs ${info.cli}` : undefined}>
        <button onClick={() => void app.linkCli!().then(setLink, error => setLink({ error: String(error) }))}><Icon name="terminal" size={14}/> Add to terminal</button>
      </SettingsRow>
      {link && ('error' in link ? <p className="settings-error" role="alert">{link.error}</p> : <div className="about-link" role="status">
        <p>Linked at <code>{link.link}</code>. Open a new terminal and run <code>lazybox doctor</code>.</p>
        {link.hints.map(h => <p key={h} className="about-hint">{h}</p>)}
      </div>)}
    </SettingsGroup>}
  </>;
}

/**
 * Which lazybox agents run: shell startup files can put another one (V1's)
 * first on PATH. Lazybox puts its own back first; this says so, and how to
 * fix what it cannot.
 */
export function AgentCliRow({ check }: { check: AgentCliCheck }) {
  const status = agentCliStatus(check);
  return <SettingsGroup title="Agents" description="Agents run every command in your shell, so its startup files decide which lazybox they reach.">
    <SettingsRow label="lazybox for agents" description={check.notice || (check.problem ? `Could not check ${check.shell}: ${check.problem}` : `Agents run ${check.cli}`)}>
      <span className={`about-version${status.tone === 'ok' ? '' : ' is-warn'}`} role="status" title={check.unfixed.join('\n') || check.shadowIn.join(', ') || undefined}>{status.label}</span>
    </SettingsRow>
  </SettingsGroup>;
}

/** The header's update notice lives with the install dialog (UpdateDialog.tsx). */
export { UpdateNotice } from './UpdateDialog';
