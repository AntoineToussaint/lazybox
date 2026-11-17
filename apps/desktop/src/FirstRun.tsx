import { useCallback, useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import icon from '../src-tauri/icons/icon.png';
import './first-run.css';

/**
 * The first-run window of an installed release (src-tauri/src/release.rs):
 * what the Runner service is and one button to set it up. It also repairs a
 * service that stopped answering. Everything it does goes through the
 * bundled `lazybox` CLI, the same commands a person can run in a terminal.
 */
type ServiceStatus = { installed: boolean; loaded: boolean; running: boolean; reachable: boolean; version: string; runnerVersion: string; endpoint: string; log: string; state?: string; problem: string; unstable: string };
type SetupState = { version: string; app: string; status: ServiceStatus };
type Check = { id: string; title: string; status: 'ok' | 'warn' | 'fail'; detail: string; fix?: string };
/**
 * Another `lazybox` that comes first on the person's PATH, so typing
 * `lazybox` runs it (shellpath.go). `remove` is set only for an older
 * Lazybox installed by Homebrew: the exact command that removes it.
 */
type Shadow = { path: string; kind: 'homebrew' | 'older' | 'copy' | 'shim' | 'other'; what: string; problem: string; fix: string; remove?: string[]; removeCommand?: string };
/** `lazybox link --json`: judged against the person's login-shell PATH, not the app's. */
type LinkReport = { link?: string; completion?: string; onPath?: boolean; shadow?: Shadow; hints?: string[]; error?: string };
/** `lazybox link --remove-older --yes --json`: what ran, what it printed, and what `lazybox` runs now. */
type RemoveReport = { command?: string; ran?: string[]; output?: string; error?: string; removed: boolean; runs?: string; ours: boolean; shadow?: Shadow };
/** `lazybox service install --json`; shellProblem: the login shell's PATH could not be read. */
type InstallReport = { shellProblem?: string };
type Phase = { kind: 'idle' } | { kind: 'working'; text: string } | { kind: 'done'; link?: LinkReport; shellProblem?: string } | { kind: 'failed'; message: string };

export function FirstRun() {
  const [state, setState] = useState<SetupState>();
  const [loadError, setLoadError] = useState('');
  const [linkCli, setLinkCli] = useState(true);
  const [phase, setPhase] = useState<Phase>({ kind: 'idle' });
  const [checks, setChecks] = useState<Check[]>();
  const load = useCallback(() => invoke<SetupState>('setup_state').then(s => { setState(s); setLoadError(''); }, e => setLoadError(String(e))), []);
  useEffect(() => { void load(); }, [load]);

  const finish = async () => {
    setPhase({ kind: 'working', text: 'Opening Lazybox…' });
    try { await invoke('setup_finish'); } catch (e) { setPhase({ kind: 'failed', message: String(e) }); }
  };
  const install = async () => {
    setChecks(undefined);
    setPhase({ kind: 'working', text: state?.status.installed ? 'Reinstalling the Runner…' : 'Setting up the Runner…' });
    try {
      const result = await invoke<{ service?: InstallReport; link?: LinkReport | null }>('setup_install', { linkCli: linkCli && !state?.status.installed });
      setPhase({ kind: 'done', link: result.link || undefined, shellProblem: result.service?.shellProblem });
      await load();
    } catch (e) { setPhase({ kind: 'failed', message: String(e) }); }
  };
  const start = async () => {
    setChecks(undefined);
    setPhase({ kind: 'working', text: 'Starting the Runner…' });
    try { await invoke('setup_start'); setPhase({ kind: 'done' }); await load(); }
    catch (e) { setPhase({ kind: 'failed', message: String(e) }); }
  };
  const diagnose = async () => {
    setPhase(p => (p.kind === 'working' ? p : { kind: 'idle' }));
    try { setChecks(((await invoke<{ checks: Check[] }>('run_doctor')).checks)); }
    catch (e) {
      // doctor exits non-zero when a check fails, with its report on stdout.
      setPhase({ kind: 'failed', message: String(e) });
    }
  };
  const showLog = () => void invoke('show_runner_log').catch(e => setPhase({ kind: 'failed', message: String(e) }));
  const quit = () => window.close();

  if (loadError) return <main className="first-run"><Header version=""/><p className="first-run-error" role="alert">{loadError}</p><div className="first-run-actions"><button onClick={() => void load()}>Try again</button><button onClick={quit}>Quit</button></div></main>;
  if (!state) return <main className="first-run" aria-busy="true"><Header version=""/><p role="status">Checking for the Runner…</p></main>;
  const { status } = state;
  const repairing = status.installed;
  const busy = phase.kind === 'working';

  return <main className="first-run">
    <Header version={state.version}/>
    {status.unstable ? <section className="first-run-warning" role="alert">
      <h2>Move Lazybox to Applications first</h2>
      <p>{status.unstable}</p>
    </section> : repairing ? <section>
      <h2>Lazybox can't reach its Runner</h2>
      <p>The background service is installed{status.running ? ' and running' : ''}, but it is not answering{status.problem ? `: ${status.problem}` : '.'}</p>
      <p className="first-run-muted">Your agents, conversations and settings are safe. Starting the Runner again re-attaches agents that are still running.</p>
    </section> : <section>
      <h2>One thing before you start</h2>
      <p>Lazybox runs your agents in a small background service, the <strong>Runner</strong>.</p>
      <ul className="first-run-points">
        <li><strong>Agents keep working</strong> when you close this window or quit Lazybox.</li>
        <li><strong>It starts when you log in</strong> and listens only on this Mac, never on your network.</li>
        <li><strong>Your data stays here</strong>, in your Library: conversations, subscriptions and workspaces.</li>
        <li><strong>You stay in control:</strong> Settings → About, or <code>lazybox service stop</code>, stops or removes it.</li>
      </ul>
      <label className="first-run-option"><input type="checkbox" checked={linkCli} disabled={busy} onChange={e => setLinkCli(e.target.checked)}/>
        <span>Add the <code>lazybox</code> command to your terminal<small>Links <code>lazybox</code> into /usr/local/bin or ~/.local/bin, with completions for your shell. No password needed.</small></span>
      </label>
    </section>}

    {phase.kind === 'working' && <p className="first-run-progress" role="status"><span className="first-run-spinner" aria-hidden="true"/>{phase.text}</p>}
    {phase.kind === 'failed' && <p className="first-run-error" role="alert">{phase.message}</p>}
    {phase.kind === 'done' && <section className="first-run-done" role="status">
      <p>✓ The Runner is running.</p>
      {phase.link && (phase.link.error ? <p className="first-run-muted">The lazybox command was not linked: {phase.link.error}</p> : <LinkDone link={phase.link}/>)}
      {phase.shellProblem && <p className="first-run-hint">Lazybox could not read your shell's PATH ({phase.shellProblem}), so agents get a standard one and may not find your tools. <code>lazybox doctor</code> says more.</p>}
    </section>}
    {checks && <ul className="first-run-checks" aria-label="Diagnostics">{checks.map(k => <li key={k.id} className={`check-${k.status}`}>
      <span aria-hidden="true">{k.status === 'ok' ? '✓' : k.status === 'warn' ? '!' : '✗'}</span><strong>{k.title}</strong> {k.detail}{k.fix && <small>{k.fix}</small>}
    </li>)}</ul>}

    <div className="first-run-actions">
      {phase.kind === 'done'
        ? <button className="primary" autoFocus onClick={() => void finish()}>Open Lazybox</button>
        : status.unstable ? null
        : repairing ? <>
          <button className="primary" disabled={busy} onClick={() => void start()}>Start the Runner</button>
          <button disabled={busy} onClick={() => void install()}>Reinstall the Runner</button>
        </> : <button className="primary" autoFocus disabled={busy} onClick={() => void install()}>Set up Lazybox</button>}
      {(repairing || phase.kind === 'failed') && <>
        <button disabled={busy} onClick={showLog}>Show log</button>
        <button disabled={busy} onClick={() => void diagnose()}>Run diagnostics</button>
      </>}
      <span className="first-run-spacer"/>
      <button disabled={busy} onClick={quit}>Quit</button>
    </div>
  </main>;
}

/**
 * What linking did. When another `lazybox` comes first on the person's PATH
 * (an older Lazybox from Homebrew, usually), typing `lazybox` runs that one:
 * said plainly, with the fix. An older Lazybox from Homebrew can be removed
 * from here, after the person confirms the exact command, whose output is
 * shown; anything else (a version manager's shim, another program) gets
 * its fix only. Nothing is ever removed silently.
 */
function LinkDone({ link }: { link: LinkReport }) {
  const [shadow, setShadow] = useState(link.shadow);
  const [removal, setRemoval] = useState<{ kind: 'confirm' } | { kind: 'running' } | { kind: 'ran'; report: RemoveReport } | { kind: 'failed'; message: string }>();
  const remove = async () => {
    setRemoval({ kind: 'running' });
    try {
      const report = await invoke<RemoveReport>('remove_older_lazybox');
      setRemoval({ kind: 'ran', report });
      if (report.removed || report.ours) setShadow(report.shadow);
    } catch (e) { setRemoval({ kind: 'failed', message: String(e) }); }
  };
  const ran = removal?.kind === 'ran' ? removal.report : undefined;
  // Offered again after a failure; never while asking or running.
  const offer = shadow?.kind === 'homebrew' && !!shadow.remove && (!removal || removal.kind === 'failed' || !!ran?.error);
  return <>
    <p className="first-run-muted">✓ <code>lazybox</code> is linked at {link.link}.</p>
    {shadow && <div className="first-run-warning" role="note">
      <p><strong><Ticks text={shadow.problem}/></strong></p>
      <p className="first-run-hint">Fix: <Ticks text={shadow.fix}/></p>
      {offer && <div className="first-run-inline-actions">
        <button onClick={() => setRemoval({ kind: 'confirm' })}>Remove the older Lazybox…</button>
      </div>}
      {shadow.remove && removal?.kind === 'confirm' && <div className="first-run-confirm" role="alertdialog" aria-label="Remove the older Lazybox"
        onKeyDown={e => { if (e.key === 'Escape') setRemoval(undefined); }}>
        <p>This runs, in Homebrew:</p>
        <pre>{shadow.remove.join(' ')}</pre>
        <p className="first-run-hint">It removes the older Lazybox ({shadow.path}). This Lazybox, your conversations and settings are not touched.</p>
        <div className="first-run-inline-actions">
          <button className="primary" onClick={() => void remove()}>Remove</button>
          <button autoFocus onClick={() => setRemoval(undefined)}>Cancel</button>
        </div>
      </div>}
    </div>}
    {removal?.kind === 'running' && <p className="first-run-progress" role="status"><span className="first-run-spinner" aria-hidden="true"/>Running <code>{link.shadow?.removeCommand}</code>…</p>}
    {removal?.kind === 'failed' && <p className="first-run-error" role="alert">{removal.message}</p>}
    {ran && <div role="status">
      <p className="first-run-muted">$ {ran.ran?.join(' ')}</p>
      {ran.output && <pre className="first-run-output">{ran.output}</pre>}
      {ran.error ? <p className="first-run-error">{ran.error}</p>
        : ran.ours ? <p className="first-run-muted">✓ The older Lazybox is removed: <code>lazybox</code> now runs this one.</p>
        : ran.removed && !ran.shadow ? <p className="first-run-muted">✓ The older Lazybox is removed. Open a new terminal once {link.link?.replace(/\/lazybox$/, '')} is on your PATH.</p> : null}
    </div>}
    {link.hints?.filter(h => !(link.shadow && h.includes(link.shadow.problem))).map(h => <p key={h} className="first-run-hint">{h}</p>)}
  </>;
}

/** Text with `code` spans, as the CLI writes its messages. */
function Ticks({ text }: { text: string }) {
  return <>{text.split('`').map((part, i) => i % 2 ? <code key={i}>{part}</code> : part)}</>;
}

function Header({ version }: { version: string }) {
  return <header className="first-run-header">
    <img src={icon} alt="" width={64} height={64}/>
    <div><h1>Welcome to Lazybox</h1>{version && <p className="first-run-muted">Version {version}</p>}</div>
  </header>;
}
