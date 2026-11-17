import { useEffect, useRef, useState } from 'react';
import type { WorkspaceAction, WorkspaceScriptSource, WorkspaceSetup } from '@lazybox/protocol';
import type { ViewStateStore } from '@lazybox/platform';
import { useRunner } from './RunnerContext';
import { Console } from './session-parts';
import { Icon } from './Icons';
import { ScriptApprovalDialog } from './ScriptApprovalDialog';
import { actionName, changedLabel, elapsed, isLive, needsApproval, portLabel, requestPreview, runLabel, setupLabel, useRecentNotice, useRuntimeRequest, useTicker, useWorkspaceRuntime, type RuntimeStore } from './workspace-runtime';
import './workspace-runtime.css';

type Props = { workspaceId: string; viewState?: ViewStateStore; onShowPanel?(kind: string): void };

const example = `# .lazybox/workspace.yaml, committed with the repository
setup: npm ci
files: [.env]
actions:
  dev:
    run: npm run dev -- --port $LAZYBOX_PORT
    default: true
    url: http://localhost:$LAZYBOX_PORT`;

/** Every repository's setup and actions in this workspace: state, ports, logs, start and stop. */
export function ActionsPanel({ workspaceId, viewState, onShowPanel }: Props) {
  const { api } = useRunner();
  const { runtime, error, unsupported, notice, busy, store } = useWorkspaceRuntime(api, workspaceId);
  const [logs, setLogs] = useState('');
  const [reviewing, setReviewing] = useState('');
  const body = useRef<HTMLDivElement>(null);
  const recent = useRecentNotice(notice, 10000);
  useRuntimeRequest(workspaceId, 'focus', target => {
    setLogs(target);
    requestAnimationFrame(() => body.current?.querySelector(`[data-runtime-row="${CSS.escape(target)}"]`)?.scrollIntoView({ block: 'nearest' }));
  });
  const actions = runtime?.actions || [];
  const live = actions.filter(a => isLive(a.runState)).length + (runtime?.setups.filter(s => isLive(s.runState)).length || 0);
  const now = useTicker(live > 0);
  if (!workspaceId) return <p className="module-empty">Choose a workspace to see its actions.</p>;
  if (unsupported) return <p className="module-empty">This Runner does not run workspace actions yet. Update Lazybox to use them.</p>;
  const members = [...new Set([...(runtime?.sources.map(s => s.member) || []), ...(runtime?.setups.map(s => s.member) || []), ...actions.map(a => a.member)])];
  const ports = runtime?.ports;
  const configured = members.some(m => runtime?.sources.find(s => s.member === m)?.path || runtime?.setups.some(s => s.member === m && s.run) || actions.some(a => a.member === m));
  const pending = needsApproval(runtime);
  const toggle = (key: string) => setLogs(current => current === key ? '' : key);
  const preview = (action: WorkspaceAction) => { requestPreview(workspaceId, action.key); onShowPanel?.('preview'); };
  return <section className="actions-panel" aria-label="Workspace actions">
    <header className="actions-header">
      <div className="actions-summary"><strong>{!runtime ? 'Actions' : live ? `${live} running` : actions.length ? 'Nothing running' : 'Actions'}</strong>
        {ports && ports.count > 0 && <span className="mono" title="This workspace’s ports: LAZYBOX_PORT_0 … LAZYBOX_PORT_9. They stay the same for the workspace’s life.">ports {ports.base}–{ports.base + ports.count - 1}</span>}
        {runtime?.portsError && <span className="tone-warn-text" title={runtime.portsError}>no ports: {runtime.portsError}</span>}
      </div>
      <button type="button" className="icon-button" aria-label="Refresh actions" title="Refresh" onClick={() => void store?.refresh()}><Icon name="refresh" size={14}/></button>
    </header>
    <div className="actions-body" ref={body}>
      {error && <p className="runtime-notice tone-warn" role="status">Actions unavailable; retrying. {error}</p>}
      {recent && <p className={`runtime-notice tone-${recent.tone}`} role="status">{recent.text}</p>}
      {!runtime && !error && <p className="runtime-empty" role="status">Loading actions…</p>}
      {runtime && !configured && <div className="runtime-empty">
        <Icon name="play" size={22}/>
        <p><strong>No actions in this workspace yet.</strong></p>
        <p>Commit a <code>.lazybox/workspace.yaml</code> to a repository to give every workspace a setup script, run actions and its own ports, or add your own in Settings → Workspaces.</p>
        <pre className="runtime-example">{example}</pre>
      </div>}
      {runtime && members.map(member => {
        const source = runtime.sources.find(s => s.member === member);
        const setup = runtime.setups.find(s => s.member === member);
        const own = actions.filter(a => a.member === member);
        if (!source?.path && !source?.overridden && !setup?.run && !own.length && !source?.error) return null;
        return <section key={member} className="runtime-group" aria-labelledby={`runtime-${member}`}>
          <header><h3 id={`runtime-${member}`}><Icon name="repository" size={13}/> {member}</h3>
            {source?.path && <span className="mono runtime-source" title="Committed with the repository">{source.path}</span>}
            {source?.overridden && <span className="runtime-badge" title="Settings → Workspaces adds or replaces entries for this repository">+ your settings</span>}
          </header>
          {source?.error && <p className="runtime-notice tone-warn" role="alert"><Icon name="alert" size={14}/> {source.path || 'workspace.yaml'} cannot be used: {source.error}. Nothing from it runs until it is fixed.</p>}
          {source && pending.includes(source) && <ApprovalBanner source={source} onReview={() => setReviewing(member)}/>}
          {setup?.run && <SetupRow setup={setup} store={store!} busy={busy.has(`setup:${member}`)} now={now} open={logs === `setup:${member}`} onLogs={() => toggle(`setup:${member}`)} onReview={() => setReviewing(member)} viewState={viewState}/>}
          {own.length > 0 && <ul className="runtime-actions">{own.map(action => <ActionRow key={action.key} action={action} all={actions} store={store!} busy={busy.has(action.key)} now={now} open={logs === action.key} onLogs={() => toggle(action.key)} onPreview={() => preview(action)} isDefault={action.key === runtime.defaultAction} viewState={viewState}/>)}</ul>}
        </section>;
      })}
    </div>
    {reviewing && <ScriptApprovalDialog workspaceId={workspaceId} member={reviewing} onClose={() => setReviewing('')}/>}
  </section>;
}

function ApprovalBanner({ source, onReview }: { source: WorkspaceScriptSource; onReview(): void }) {
  const changed = source.approval === 'changed';
  return <div className="runtime-approval" role="note">
    <Icon name="alert" size={14}/>
    <p>{changed ? <>{source.member}’s scripts changed since you approved them{source.changed.length ? `: ${source.changed.map(changedLabel).join(', ')}` : ''}.</> : <>{source.member}’s scripts run only after you approve them.</>} Nothing from this repository runs until then.</p>
    <button type="button" className="primary" onClick={onReview}>Review…</button>
  </div>;
}

function Log({ command, viewState }: { command: string; viewState?: ViewStateStore }) {
  const { api } = useRunner();
  const [error, setError] = useState('');
  if (!command) return <p className="runtime-log-empty">No log yet: it appears here from the first run.</p>;
  return <div className="runtime-log">{error && <p className="runtime-notice tone-warn" role="alert">{error}</p>}<Console key={command} api={api} command={command} viewState={viewState} onError={setError}/></div>;
}

/**
 * What a row runs. One line reads as one line; a script of several lines keeps
 * its lines (joined on one line it is not the script any more) behind its
 * first line, which opens it.
 */
function Script({ run, label, where }: { run: string; label: string; where?: string }) {
  const lines = run.replace(/\s+$/, '').split('\n');
  if (lines.length < 2) return <code className="runtime-run" title={where ? `${run} (in ${where})` : run}>{run}</code>;
  return <details className="runtime-script">
    <summary aria-label={`${label}, ${lines.length} lines`} title={where ? `Runs in ${where}` : undefined}><code className="runtime-run">{lines[0]}</code><small>{lines.length} lines</small></summary>
    <pre className="mono" aria-label={label}>{lines.join('\n')}</pre>
  </details>;
}

function SetupRow({ setup, store, busy, now, open, onLogs, onReview, viewState }: { setup: WorkspaceSetup; store: RuntimeStore; busy: boolean; now: number; open: boolean; onLogs(): void; onReview(): void; viewState?: ViewStateStore }) {
  const state = setupLabel(setup), live = isLive(setup.runState);
  const ran = !!setup.runState?.commandId;
  return <div className={`runtime-row setup-row tone-${state.tone}`} data-runtime-row={`setup:${setup.member}`} aria-label={`${setup.member} setup, ${state.text}`} role="group">
    <div className="runtime-line">
      {live ? <span className="run-spinner" aria-hidden="true"/> : <span className={`run-state-dot tone-${state.tone}`} aria-hidden="true"/>}
      <div className="runtime-text">
        <div className="runtime-title"><strong>Setup</strong><span className={`runtime-state tone-${state.tone}`}>{state.text}</span>{live && <span className="runtime-elapsed">{elapsed(setup.runState?.startedAtUnixMillis || 0n, now)}</span>}{setup.source === 'settings' && <span className="runtime-badge">yours</span>}</div>
        <Script run={setup.run} label={`${setup.member} setup script`}/>
        {setup.copiedFiles.length > 0 && <span className="runtime-files">Copied from your checkout: {setup.copiedFiles.join(', ')}</span>}
        {setup.missingFiles.length > 0 && <span className="runtime-files tone-warn-text"><Icon name="alert" size={12}/> Not in your checkout, so not copied: {setup.missingFiles.join(', ')}</span>}
        {live && setup.runState?.lastLine && <span className="runtime-last mono" title={setup.runState.lastLine}>{setup.runState.lastLine}</span>}
      </div>
      <div className="runtime-buttons">
        {setup.pendingApproval ? <button type="button" className="primary" onClick={onReview}>Review…</button>
          : <button type="button" disabled={busy || live} onClick={() => void store.runSetup(setup.member)}>{ran ? 'Run again' : 'Run setup'}</button>}
        <button type="button" aria-expanded={open} aria-label={`${open ? 'Hide' : 'Show'} ${setup.member} setup log`} disabled={!ran} onClick={onLogs}><Icon name="output" size={13}/> Log</button>
      </div>
    </div>
    {open && <Log command={setup.runState?.commandId || ''} viewState={viewState}/>}
  </div>;
}

function ActionRow({ action, all, store, busy, now, open, onLogs, onPreview, isDefault, viewState }: { action: WorkspaceAction; all: readonly WorkspaceAction[]; store: RuntimeStore; busy: boolean; now: number; open: boolean; onLogs(): void; onPreview(): void; isDefault: boolean; viewState?: ViewStateStore }) {
  const name = actionName(action, all), state = runLabel(action.runState), live = isLive(action.runState);
  const stopping = action.runState?.state === 'stopping';
  const ran = !!action.runState?.commandId;
  const row = useRef<HTMLLIElement>(null);
  useEffect(() => { if (open) row.current?.scrollIntoView({ block: 'nearest' }); }, [open]);
  return <li ref={row} className={`runtime-row tone-${state.tone}`} data-runtime-row={action.key} aria-label={`${name}, ${state.text}`}>
    <div className="runtime-line">
      <span className={`run-state-dot tone-${state.tone}${live && !stopping ? ' is-live' : ''}`} aria-hidden="true"/>
      <div className="runtime-text">
        <div className="runtime-title"><strong>{action.name}</strong>
          {live ? <span className={`runtime-state tone-${state.tone}`}>{state.text}</span> : action.runState?.state && action.runState.state !== 'idle' && <span className={`runtime-state tone-${state.tone}`}>{state.text}</span>}
          {live && <span className="runtime-elapsed">{elapsed(action.runState?.startedAtUnixMillis || 0n, now)}</span>}
          {isDefault && <span className="runtime-badge" title="Run in the header starts this action">default</span>}
          {!action.concurrent && <span className="runtime-badge" title="Starting it here stops it in your other workspaces">one at a time</span>}
          {action.source === 'settings' && <span className="runtime-badge">yours</span>}
        </div>
        <Script run={action.run} label={`${action.name} script`} where={action.cwd}/>
        <span className="runtime-facts">
          {action.port > 0 && <span className="mono" title="Its LAZYBOX_PORT">port {action.port}</span>}
          {action.url && (live ? <button type="button" className="link-button mono" onClick={onPreview} title="Open in the Preview panel">{action.url}</button> : <span className="mono">{action.url}</span>)}
        </span>
        {action.runningElsewhere.length > 0 && !live && <span className="runtime-files tone-warn-text"><Icon name="alert" size={12}/> Running in {action.runningElsewhere.join(', ')}: starting it here stops it there.</span>}
        {action.blocked && <span className="runtime-files tone-warn-text">{action.blocked}</span>}
        {live && action.runState?.lastLine && <span className="runtime-last mono" title={action.runState.lastLine}>{action.runState.lastLine}</span>}
      </div>
      <div className="runtime-buttons">
        {live ? <>
          {action.url && <button type="button" onClick={onPreview} aria-label={`Preview ${name}`}><Icon name="eye" size={13}/> Preview</button>}
          <button type="button" disabled={busy || stopping} aria-label={`Restart ${name}`} onClick={() => void store.startAction(action, true, all)}><Icon name="refresh" size={13}/> Restart</button>
          <button type="button" className="danger" disabled={busy || stopping} aria-label={`Stop ${name}`} onClick={() => void store.stopAction(action)}><span className="stop-square" aria-hidden="true"/> {stopping ? 'Stopping…' : 'Stop'}</button>
        </> : <button type="button" className="primary" disabled={busy || !!action.blocked} title={action.blocked || undefined} aria-label={`Start ${name}`} onClick={() => void store.startAction(action, false, all)}><span aria-hidden="true">▶</span> {busy ? 'Starting…' : 'Start'}</button>}
        <button type="button" aria-expanded={open} aria-label={`${open ? 'Hide' : 'Show'} ${name} log`} disabled={!ran} onClick={onLogs}><Icon name="output" size={13}/> Log</button>
      </div>
    </div>
    {open && <Log command={action.runState?.commandId || ''} viewState={viewState}/>}
  </li>;
}
