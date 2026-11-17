import { useEffect, useState } from 'react';
import type { HookRun, UserHook } from '@lazybox/protocol';
import type { ViewStateStore } from '@lazybox/platform';
import { useRunner } from './RunnerContext';
import { SettingsGroup, SettingsHeader, settingsError, useNow } from './settings-parts';
import { Icon } from './Icons';
import { leaderConflicts, readKeymap, readMenu } from './keyboard';
import { actionSummary } from './user-actions';
import { userCommands } from './user-commands';
import { useUserActionsFile } from './useUserActions';
import { PICKER_KEY } from './PowerLayer';
import { ConfigIssues, CopyPath } from './ConfigFileStatus';
import { errorCount, fileLabel } from './config-files';
import { useConfigFiles } from './useConfigFiles';
import './power.css';

const CHEATS: [string, string][] = [
  ['lazybox ls', 'Workspaces, conversations and what each agent is doing'],
  ['lazybox send "fix the tests" "Continue."', 'Send a message to a conversation (or - for stdin)'],
  ['lazybox wait "fix the tests" --until idle', 'Block a script until the agent is done'],
  ['lazybox open "fix the tests"', 'Show a conversation in this window'],
  ['lazybox shot --out bug.png', 'A PNG of this window, for bug reports'],
  ['lazybox actions run continue', 'Run one of your actions here'],
  ['lazybox do layout.maximize', 'Run any action of the app here (lazybox do --list)'],
  ['lazybox config edit keys', 'Edit your shortcuts in $EDITOR; checked as you save'],
  ['lazybox config validate', 'Every config file checked, problems by file:line'],
  ['source <(lazybox completion zsh)', 'Shell completion (also bash, fish)'],
];

const ago = (ms: number, now: number) => { const s = Math.max(0, Math.round((now - ms) / 1000)); return s < 60 ? `${s}s ago` : s < 3600 ? `${Math.round(s / 60)}m ago` : `${Math.round(s / 3600)}h ago`; };
const took = (run: HookRun) => run.endedAtUnixMillis ? `${((Number(run.endedAtUnixMillis) - Number(run.startedAtUnixMillis)) / 1000).toFixed(1)}s` : 'running';

/** Settings → Actions & hooks: what the actions file declares, what is wrong with it, and what the hooks did. */
export function PowerSettings({ viewState }: { viewState?: ViewStateStore }) {
  const { api } = useRunner();
  const { file, error: loadError, replace } = useUserActionsFile();
  const [error, setError] = useState('');
  const [copied, setCopied] = useState('');
  const copy = (text: string) => void navigator.clipboard.writeText(text).then(() => { setCopied(text); setTimeout(() => setCopied(value => value === text ? '' : value), 1500); }, () => setError('Could not copy to the clipboard.'));
  const create = async () => { try { replace(await api.initUserActions({})); setError(''); } catch (e) { setError(settingsError(e)); } };
  const keymap = readKeymap(viewState), menu = readMenu(viewState);
  const conflicts = new Map(leaderConflicts([...(file?.actions || []).filter(a => a.key && a.where.includes('menu')).map(a => ({ id: a.id, key: a.key, label: a.title, run() {} })), { id: 'run-action', key: PICKER_KEY, label: 'Run an action…', run() {} }], keymap, menu).map(c => [c.extension.id, c.takenBy]));
  const errors = (file?.issues || []).filter(i => i.severity === 'error').length;
  return <div className="power-settings">
    <SettingsHeader title="Actions & hooks" description={<>Your own actions in the ]] menu, the action picker and the workspace and conversation menus; hooks that run a program or a webhook when something happens. Declared in one file, reloaded as soon as you save it.</>}
      actions={<button onClick={() => window.dispatchEvent(new Event('lazybox-run-action'))} disabled={!file?.actions.length}><Icon name="play" size={14}/> Run an action…</button>}/>
    {(error || loadError) && <p className="settings-error" role="alert">{error || settingsError(loadError, 'load')}</p>}
    {!file && !loadError && <p className="settings-empty" role="status">Loading your actions…</p>}
    {file && <>
      <SettingsGroup title="File">
        <div className="settings-row"><div className="settings-row-text"><span className="settings-row-label">Actions file</span><small className="power-status">{file.exists ? <>Loaded {ago(Number(file.loadedAtUnixMillis), Date.now())} · revision {String(file.revision)} · {file.actions.length} {file.actions.length === 1 ? 'action' : 'actions'}, {file.hooks.length} {file.hooks.length === 1 ? 'hook' : 'hooks'}{errors ? ` · ${errors} ${errors === 1 ? 'problem' : 'problems'}` : ''}</> : 'Not created yet'}</small></div>
          <div className="settings-row-control power-path"><code title={file.path}>{file.path}</code><button aria-label="Copy the actions file path" title="Copy path" onClick={() => copy(file.path)}><Icon name="copy" size={13}/>{copied === file.path ? ' Copied' : ''}</button>{!file.exists && <button className="primary" onClick={() => void create()}><Icon name="plus" size={13}/> Create actions.yaml</button>}</div>
        </div>
        <div className="settings-row"><div className="settings-row-text"><span className="settings-row-label">JSON Schema</span><small>Completion and checks in your editor; the starter file points at it.</small></div>
          <div className="settings-row-control power-path"><code title={file.schemaPath}>{file.schemaPath}</code><button aria-label="Copy the schema path" title="Copy path" onClick={() => copy(file.schemaPath)}><Icon name="copy" size={13}/></button></div>
        </div>
      </SettingsGroup>
      {file.error && <div className={`power-banner${file.stale ? '' : ' is-bad'}`} role="alert"><Icon name="alert" size={15}/><span><strong>The file does not load:</strong> {file.error}{file.stale ? ' Using the last version that loaded until you fix it.' : ''}</span></div>}
      {file.issues.length > 0 && <SettingsGroup title="Problems" description="An entry with an error is left out; the others work.">
        <ul className="power-issues" aria-label="Problems in the actions file">{file.issues.map((issue, i) => <li key={i}><span className={`sev is-${issue.severity}`}>{issue.severity === 'error' ? 'Error' : 'Warning'}</span><span className="where">{issue.line ? `line ${issue.line}` : ''}</span><span><span className="where">{issue.path}</span> {issue.message}</span></li>)}</ul>
      </SettingsGroup>}
      <SettingsGroup title="Actions" description={file.actions.length ? <>]] {PICKER_KEY} opens the action picker{conflicts.get('run-action') ? ` — taken by ${conflicts.get('run-action')}` : ''}.</> : undefined}>
        {file.actions.length ? <ul className="power-list" aria-label="Your actions">{file.actions.map(action => <li key={action.id}>
          <Icon name={action.icon || 'play'} size={16}/>
          <div className="power-item">
            <strong>{action.title}{action.key && <span className={`power-chip${conflicts.has(action.id) ? ' is-conflict' : ''}`} title={conflicts.has(action.id) ? `]] ${action.key} is taken by ${conflicts.get(action.id)}; this action is not on that key` : `]] ${action.key}`}>]] {action.key}{conflicts.has(action.id) ? ' · taken' : ''}</span>}{action.confirm && <span className="power-chip">asks first</span>}{action.where.map(w => <span key={w} className="power-chip is-off">{w}</span>)}</strong>
            <small title={actionSummary(action)}>{actionSummary(action)}</small>
          </div>
          <button onClick={() => userCommands.find(action.id)?.run()} aria-label={`Run ${action.title}`}><Icon name="play" size={13}/> Run</button>
        </li>)}</ul> : <p className="settings-empty">{file.exists ? 'The file declares no actions yet.' : 'Create the file to add your first action.'}</p>}
      </SettingsGroup>
      <HooksGroup hooks={file.hooks} onError={setError}/>
    </>}
    <ConfigFilesGroup/>
    <SettingsGroup title="From a terminal" description="The lazybox CLI does everything this app does, for scripts and agents you trust with your Runner.">
      <div className="power-cheats">{CHEATS.map(([command, what]) => <div key={command}><code>{command}</code><small>{what}</small><button onClick={() => copy(command)} aria-label={`Copy ${command}`}>{copied === command ? 'Copied' : 'Copy'}</button></div>)}</div>
    </SettingsGroup>
  </div>;
}

/** keys.yaml, settings.yaml and layouts/ beside the actions file: each one's state and problems by line. */
function ConfigFilesGroup() {
  const files = useConfigFiles();
  if (!files.loaded) return null;
  const layouts = files.docs.filter(d => d.kind === 'layout');
  const describe = (name: string) => name === 'keys' ? 'Shortcuts, the ]] menu, Vim editing (Settings → Shortcuts writes it)' : name === 'settings' ? 'Notifications (Settings → Notifications writes it)' : 'A layout in the Layout menu and ⌘⇧L';
  return <SettingsGroup title="Config files" description={<>Plain YAML beside the actions file, with a JSON Schema each for your editor. Lazybox reloads a file as you save it; a mistake is shown here by line and the last version that loaded stays in force. Never secrets: credentials, subscriptions and devices stay in the app.</>}>
    <ul className="power-list config-files-list" aria-label="Config files">
      {[...files.docs.filter(d => d.kind !== 'layout'), ...layouts].map(doc => {
        const errors = errorCount(doc) + (files.appProblems[doc.name] ? 1 : 0);
        return <li key={doc.name}>
          <Icon name={doc.kind === 'layout' ? 'layout' : doc.kind === 'keys' ? 'keyboard' : 'sliders'} size={16}/>
          <div className="power-item">
            <strong>{fileLabel(doc.name)}{!doc.exists && <span className="power-chip is-off">not created</span>}{doc.stale && <span className="power-chip is-conflict">last good version in force</span>}{errors > 0 && <span className="power-chip is-conflict">{errors === 1 ? '1 problem' : `${errors} problems`}</span>}</strong>
            <small>{doc.error || describe(doc.kind === 'layout' ? 'layout' : doc.name)}</small>
            {(doc.issues.length > 0 || files.appProblems[doc.name]) && <ConfigIssues doc={doc} appProblem={files.appProblems[doc.name]}/>}
          </div>
          <CopyPath path={doc.path} label={fileLabel(doc.name)}/>
        </li>;
      })}
    </ul>
    {!layouts.length && <p className="settings-empty">No layout files yet: Layout menu → Save current layout as… writes one to <code>{files.directory}/layouts</code>.</p>}
  </SettingsGroup>;
}

function hookTarget(h: UserHook) { return h.webhook ? `POST ${h.webhook}` : h.shell || h.run.join(' '); }
function hookFilter(h: UserHook) {
  const parts = [...h.workspaces.map(w => `workspace ${w}`), ...h.conversations.map(c => `conversation ${c}`), ...h.repositories.map(r => `repository ${r}`)];
  return parts.length ? ` · ${parts.join(', ')}` : '';
}

function HooksGroup({ hooks, onError }: { hooks: UserHook[]; onError(message: string): void }) {
  const { api } = useRunner();
  const now = useNow(5000);
  const [runs, setRuns] = useState<HookRun[]>([]);
  const [open, setOpen] = useState(''), [log, setLog] = useState('');
  const [testing, setTesting] = useState('');
  const [tick, setTick] = useState(0);
  useEffect(() => {
    const control = new AbortController();
    const load = () => void api.listHookRuns({ limit: 30 }, { signal: control.signal }).then(r => { if (!control.signal.aborted) setRuns(r.runs); }, () => undefined);
    load(); const timer = setInterval(load, 2000);
    return () => { control.abort(); clearInterval(timer); };
  }, [api, tick]);
  const run = runs.find(r => r.id === open);
  useEffect(() => {
    if (!run?.commandId) { setLog(''); return; }
    const control = new AbortController();
    void api.readCommand({ commandId: run.commandId, offset: 0n, limit: 64 << 10 }, { signal: control.signal }).then(r => { if (!control.signal.aborted) setLog(new TextDecoder().decode(r.data) || '(no output)'); }, e => { if (!control.signal.aborted) setLog(`The output could not be read: ${String(e)}`); });
    return () => control.abort();
  }, [api, run?.commandId, run?.state]);
  const test = async (id: string) => {
    setTesting(id);
    try { const result = await api.testHook({ hookId: id }); setOpen(result.id); setTick(t => t + 1); onError(''); }
    catch (e) { onError(settingsError(e)); }
    finally { setTesting(''); }
  };
  return <>
    <SettingsGroup title="Hooks" description="Run as commands of the event's workspace: listed in Processes while they run, with their output below. A hook holds no Lazybox credential unless the file grants it read, respond or start.">
      {hooks.length ? <ul className="power-list" aria-label="Your hooks">{hooks.map(h => <li key={h.id}>
        <Icon name={h.webhook ? 'external' : 'terminal'} size={16}/>
        <div className="power-item">
          <strong>{h.id}{h.on.map(e => <span key={e} className="power-chip">{e}</span>)}{h.grants.map(g => <span key={g} className="power-chip is-grant" title={`This hook may ${g === 'read' ? 'read conversations' : g === 'respond' ? 'send, answer and approve' : 'start conversations'}`}>{g}</span>)}{!h.enabled && <span className="power-chip is-off">off</span>}</strong>
          <small title={hookTarget(h)}>{hookTarget(h)}{hookFilter(h)}</small>
        </div>
        <button disabled={!!testing || !h.enabled} onClick={() => void test(h.id)} aria-label={`Test ${h.id}`} title="Run it now with a sample event"><Icon name="play" size={13}/> {testing === h.id ? 'Testing…' : 'Test'}</button>
      </li>)}</ul> : <p className="settings-empty">No hooks. Add one under hooks: to run a program or a webhook when an agent needs you, a turn ends, checks fail or work is accepted.</p>}
    </SettingsGroup>
    <SettingsGroup title="Recent hook runs">
      {runs.length ? <ul className="power-list power-runs" aria-label="Recent hook runs">{runs.map(r => <li key={r.id}>
        <span className={`power-run-state ${r.state}`} aria-label={r.state}><Icon name={r.state === 'succeeded' ? 'check' : r.state === 'failed' ? 'alert' : r.state === 'skipped' ? 'skip' : 'working'} size={15}/></span>
        <div className="power-item">
          <strong>{r.hookId}<span className="power-chip">{r.event}</span>{r.test && <span className="power-chip is-off">test</span>}</strong>
          <small>{r.summary} · {ago(Number(r.startedAtUnixMillis), now)} · {took(r)}{r.exitCode !== undefined ? ` · exit ${r.exitCode}` : ''}{r.httpStatus ? ` · HTTP ${r.httpStatus}` : ''}{r.error ? ` · ${r.error}` : ''}</small>
        </div>
        {r.commandId && <button aria-expanded={open === r.id} onClick={() => setOpen(open === r.id ? '' : r.id)}>{open === r.id ? 'Hide output' : 'Output'}</button>}
        {open === r.id && <pre className="power-log" aria-label={`Output of ${r.hookId}`}>{log || 'Loading…'}</pre>}
      </li>)}</ul> : <p className="settings-empty">No hook has run yet.</p>}
    </SettingsGroup>
  </>;
}
