import { peekProps } from './peek-model';
import { useEffect, useRef, useState } from 'react';
import type { HostedProcess, ProcessHosting, ProcessInventory } from '@lazybox/protocol';
import { useRunner } from './RunnerContext';
import { ConfirmDialog } from './ConfirmDialog';
import { Icon } from './Icons';
import { requestTerminal } from './TerminalPanel';
import { backendLabel, formatMemory, formatUptime, groupProcesses, listeningLabel, processState, stopConsequence } from './processes-model';
import type { ViewTarget } from './workspace-model';
import './processes.css';
import { contextMenu, copyText } from './ContextMenu';
import { agentTitle } from './providers';

type Props = {
  workspaceId?: string;
  /** Show a conversation (switching workspace when needed). */
  onShowSession?(target: ViewTarget): void;
  /** Show the panel of this kind for target, switching workspace or conversation first. */
  onShowPanel?(kind: string, target?: ViewTarget): void;
};

const plural = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`;

/** Polls the Runner's inventory while mounted. */
function useProcesses() {
  const { api } = useRunner();
  const [inventory, setInventory] = useState<ProcessInventory>();
  const [error, setError] = useState('');
  const reading = useRef(false), alive = useRef<AbortController | null>(null);
  const refresh = async () => {
    // Each read answers to the mount that started it: a read a remount
    // abandoned (React runs effects twice in development) is not a failure.
    const signal = alive.current?.signal;
    if (reading.current || signal?.aborted) return; reading.current = true;
    try { const next = await api.listProcesses({}, { signal, timeoutMs: 8000 }); if (!signal?.aborted) { setInventory(next); setError(''); } }
    catch (e) { if (!signal?.aborted) setError(String(e)); }
    finally { reading.current = false; }
  };
  useEffect(() => {
    const control = new AbortController(); alive.current = control; reading.current = false;
    void refresh(); const timer = setInterval(() => void refresh(), 3000);
    return () => { control.abort(); clearInterval(timer); };
  }, [api]);
  return { inventory, setInventory, error, refresh };
}

/** Where new processes go, and why not when tmux was chosen but cannot be used. */
export function HostingSummary({ hosting }: { hosting?: ProcessHosting }) {
  if (!hosting) return null;
  const tmux = hosting.selected === 'tmux';
  const problem = tmux ? hosting.fallback || (hosting.tmuxProblem ? `tmux is selected, but ${hosting.tmuxProblem}. New processes run in session hosts until it is available.` : '') : '';
  return <>
    <p className="processes-hosting">New processes run in <strong>{tmux ? 'tmux' : 'session hosts'}</strong>{tmux && hosting.tmuxVersion ? ` (${hosting.tmuxVersion})` : ''}. Change this in Settings → Runner.</p>
    {problem && <p className="processes-notice tone-warn" role="status"><Icon name="alert" size={14}/> {problem}</p>}
  </>;
}

/** Every process the Runner hosts so it outlives the Runner: open, attach to, or stop it. */
export function ProcessesPanel({ workspaceId = '', onShowSession, onShowPanel }: Props) {
  const { api } = useRunner();
  const { inventory, setInventory, error, refresh } = useProcesses();
  const [onlyHere, setOnlyHere] = useState(false);
  const [stopping, setStopping] = useState<HostedProcess>();
  const [cleaning, setCleaning] = useState(false);
  const [notice, setNotice] = useState('');
  const processes = inventory?.processes || [];
  const groups = groupProcesses(processes, onlyHere ? workspaceId : '');
  const orphans = processes.filter(p => !p.attached);
  const counted = (kind: string) => processes.filter(p => p.attached && p.kind === kind).length;
  const summary = [plural(counted('agent'), 'agent'), plural(counted('command'), 'command'), plural(counted('terminal'), 'terminal')].join(' · ');

  const open = (p: HostedProcess) => {
    const target = { collectionId: p.workspaceId, sessionId: p.sessionId, commandId: '' };
    if (p.kind === 'agent') onShowSession?.(target);
    else if (p.kind === 'command') onShowPanel?.('console', { ...target, commandId: p.commandId });
    else { requestTerminal(p.workspaceId, p.terminalId); onShowPanel?.('terminal', { collectionId: p.workspaceId, sessionId: '', commandId: '' }); }
  };
  const openable = (p: HostedProcess) => p.attached && (p.kind === 'agent' ? !!p.sessionId && !!onShowSession : p.kind === 'command' ? !!p.commandId && !!onShowPanel : !!p.terminalId && !!p.workspaceId && !!onShowPanel);
  const copy = async (p: HostedProcess) => {
    try { await navigator.clipboard.writeText(p.attachCommand); setNotice(`Copied. Paste it in any terminal to watch “${p.label}”; detach with your tmux prefix, then d.`); }
    catch { setNotice(`Copy it yourself: ${p.attachCommand}`); }
  };

  return <section className="processes-panel" aria-label="Hosted processes">
    <header className="processes-header">
      <div className="processes-count"><strong>{inventory ? plural(processes.length - orphans.length, 'process', 'processes') + ' running' : 'Processes'}</strong>{inventory && <span>{summary}</span>}</div>
      <div className="processes-tools">
        {workspaceId && <label className="processes-scope"><input type="checkbox" checked={onlyHere} onChange={e => setOnlyHere(e.target.checked)}/>This workspace only</label>}
        <button type="button" className="icon-button" aria-label="Refresh processes" title="Refresh" onClick={() => void refresh()}><Icon name="refresh" size={14}/></button>
      </div>
    </header>
    <div className="processes-body">
      <HostingSummary hosting={inventory?.hosting}/>
      {error && <p className="processes-notice tone-warn" role="status" title={error}><Icon name="alert" size={14}/> The Runner did not list its processes. Trying again every few seconds…</p>}
      {inventory?.problem && <p className="processes-notice tone-warn" role="status">Some processes could not be listed: {inventory.problem}</p>}
      {notice && <p className="processes-notice" role="status">{notice}<button type="button" className="link-button" onClick={() => setNotice('')}>Dismiss</button></p>}
      {!inventory && !error && <p className="processes-empty" role="status">Loading processes…</p>}
      {inventory && !groups.length && <div className="processes-empty"><Icon name="done" size={22}/><p><strong>Nothing is running{onlyHere ? ' in this workspace' : ''}.</strong></p><p>Agents, managed commands and terminals appear here while they run. They keep running when the Runner restarts.</p></div>}
      {groups.map(group => <section key={group.key} className={`process-group group-${group.key}`} aria-labelledby={`processes-${group.key}`}>
        <header><h3 id={`processes-${group.key}`}>{group.title} <span>{group.processes.length}</span></h3>
          {group.key === 'orphan' && <button type="button" className="danger" onClick={() => setCleaning(true)}>Clean up orphans…</button>}</header>
        {group.key === 'orphan' && <p className="process-group-note">No Runner supervises these: a Runner stopped before it could hand them over. Each stops by itself after a day without a Runner.</p>}
        <ul>{group.processes.map(p => <ProcessRow key={p.id} process={p} showWorkspace={!onlyHere} canOpen={openable(p)} onOpen={() => open(p)} onCopy={() => void copy(p)} onStop={() => setStopping(p)}/>)}</ul>
      </section>)}
    </div>
    {stopping && <ConfirmDialog title={`Stop “${stopping.label}”?`} closeLabel="Close without stopping" working="Stopping…" children={stopConsequence(stopping)} confirm={stopping.kind === 'agent' && stopping.attached ? 'Stop agent' : stopping.kind === 'terminal' && stopping.attached ? 'Close terminal' : 'Stop process'} onClose={() => setStopping(undefined)} onConfirm={async () => {
      const next = await api.stopProcess({ id: stopping.id }); setInventory(next); setNotice(`Stopped “${stopping.label}”.`); setStopping(undefined);
    }}/>}
    {cleaning && <ConfirmDialog title={`Stop ${plural(orphans.length, 'orphaned process', 'orphaned processes')}?`} closeLabel="Close without stopping" working="Stopping…" children="These processes run with no Runner to supervise them. Cleaning up stops each of them and everything it started." kept="Processes this Runner supervises are not touched." confirm="Clean up" onClose={() => setCleaning(false)} onConfirm={async () => {
      const result = await api.cleanUpOrphanProcesses({}); if (result.inventory) setInventory(result.inventory);
      setNotice(result.problem ? `Stopped ${result.stopped}; some were left alone: ${result.problem}` : `Stopped ${plural(result.stopped, 'orphaned process', 'orphaned processes')}.`); setCleaning(false);
    }}/>}
  </section>;
}

function ProcessRow({ process: p, showWorkspace, canOpen, onOpen, onCopy, onStop }: { process: HostedProcess; showWorkspace: boolean; canOpen: boolean; onOpen(): void; onCopy(): void; onStop(): void }) {
  const state = processState(p);
  const where = [
    p.kind === 'agent' ? agentTitle(p.provider, [], p.provider) : '',
    p.kind === 'command' && p.conversationTitle ? `in “${p.conversationTitle}”` : '',
    showWorkspace ? p.workspaceName : '',
  ].filter(place => place && place !== p.label);
  const facts = [
    listeningLabel(p.ports),
    `PID ${p.programPid || p.hostPid}`,
    formatUptime(p.uptimeSeconds) && `up ${formatUptime(p.uptimeSeconds)}`,
    p.usage && `${p.usage.cpuPercent.toFixed(1)}% CPU`,
    p.usage && formatMemory(p.usage.memoryBytes),
    p.usage && p.usage.processCount > 2 && plural(p.usage.processCount, 'process', 'processes'),
    backendLabel(p.backend),
  ].filter(Boolean);
  return <li className={`process-row tone-${state.tone}`} aria-label={`${p.label}, ${state.label}`} tabIndex={0} {...peekProps({ kind: 'process', processId: p.id, title: p.label })} onContextMenu={contextMenu('process', () => [
    canOpen && { label: 'Open', icon: p.kind === 'agent' ? 'chat' : p.kind === 'command' ? 'output' : 'terminal', run: onOpen },
    p.attachCommand && { label: 'Copy attach command', icon: 'terminal', run: onCopy },
    { label: 'Copy PID', icon: 'copy', run: () => copyText(String(p.programPid || p.hostPid), 'Copied PID') },
    'separator',
    { label: p.kind === 'terminal' && p.attached ? 'Close terminal…' : p.kind === 'agent' && p.attached ? 'Stop agent…' : 'Stop…', icon: 'stop', danger: true, run: onStop },
  ], { process: p })}>
    <span className={`process-dot tone-${state.tone}`} aria-hidden="true"/>
    <div className="process-text">
      <div className="process-title"><strong title={p.label}>{p.label}</strong>{state.label !== 'Running' && <span className={`process-state tone-${state.tone}`}>{state.label}</span>}</div>
      {!!where.length && <span className="process-where">{where.join(' · ')}</span>}
      <span className="process-facts mono" title={p.backend === 'run' ? `A lazybox run that went on in the background; program PID ${p.programPid}` : `Session host PID ${p.hostPid}${p.programPid ? `, program PID ${p.programPid}` : ''}`}>{facts.join(' · ')}</span>
    </div>
    <div className="process-actions">
      {canOpen && <button type="button" onClick={onOpen} aria-label={`Open ${p.label}`}>Open</button>}
      {p.attachCommand && <button type="button" onClick={onCopy} title={p.attachCommand} aria-label={`Copy the tmux attach command for ${p.label}`}><Icon name="terminal" size={12}/> Copy attach</button>}
      <button type="button" className="danger" onClick={onStop} aria-label={`Stop ${p.label}…`}>Stop…</button>
    </div>
  </li>;
}
