import { useEffect, useRef, useState } from 'react';
import type { OwnedProcess, Ownership } from '@lazybox/protocol';
import { DialogHeader, DraggableDialog } from './DraggableDialog';
import { humanError } from './human-error';
import { Icon } from './Icons';
import { useRunner } from './RunnerContext';
import './ownership.css';

/** A workspace, or a conversation (any of its attempts). */
export type OwnershipTarget = { collectionId: string; sessionId?: never } | { sessionId: string; collectionId?: never };

const STATE: Record<string, { label: string; tone: string }> = {
  running: { label: 'Running', tone: 'running' },
  gone: { label: 'Stopped', tone: 'gone' },
  unknown: { label: 'Can’t check', tone: 'unknown' },
};

/**
 * What a previous Lazybox left unconfirmed here, and the way forward
 * (RESOURCE_OWNERSHIP.md, "Unconfirmed ownership"): each agent and command it
 * recorded, its PID and command line, and whether the Runner sees it running.
 * The Runner settles by itself what it proves stopped, so this shows only
 * when it cannot: then "Stop them and continue" stops exactly those
 * processes, "They're already stopped, continue" takes the person's word.
 * `onSettled` runs once nothing is unconfirmed, the way the caller continues.
 */
export function OwnershipPanel({ target, what, onSettled, onCancel, autoFocus = true }: {
  target: OwnershipTarget;
  /** What waits for it, with its verb: "Deleting this workspace waits". */
  what: string;
  onSettled(note: string): void;
  onCancel?(): void;
  autoFocus?: boolean;
}) {
  const { api } = useRunner();
  const [ownership, setOwnership] = useState<Ownership>();
  const [error, setError] = useState(''), [busy, setBusy] = useState<'stop' | 'stopped' | ''>('');
  const settled = useRef(onSettled); settled.current = onSettled;
  const first = useRef<HTMLButtonElement>(null);
  const request = target.collectionId ? { collectionId: target.collectionId, sessionId: '' } : { collectionId: '', sessionId: target.sessionId || '' };
  const key = request.collectionId + '/' + request.sessionId;
  useEffect(() => {
    const control = new AbortController();
    setOwnership(undefined); setError('');
    void api.getOwnership(request, { signal: control.signal, timeoutMs: 30_000 }).then(result => {
      if (control.signal.aborted) return;
      if (result.settled) settled.current(result.note);
      else setOwnership(result);
    }, failure => { if (!control.signal.aborted) setError(humanError(failure)); });
    return () => control.abort();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [api, key]);
  useEffect(() => { if (ownership && autoFocus) first.current?.focus(); }, [ownership, autoFocus]);
  const resolve = async (stop: boolean) => {
    if (busy) return;
    setBusy(stop ? 'stop' : 'stopped'); setError('');
    try {
      const result = await api.resolveOwnership({ ...request, stop, reason: '' }, { timeoutMs: 60_000 });
      if (result.settled) settled.current(result.note);
      else setOwnership(result);
    } catch (failure) {
      setError(humanError(failure));
      // What is still unconfirmed, as the Runner sees it now.
      try { const now = await api.getOwnership(request, { timeoutMs: 30_000 }); if (now.settled) settled.current(now.note); else setOwnership(now); } catch { /* the error says enough */ }
    } finally { setBusy(''); }
  };
  if (!ownership) return <div className="ownership-panel">
    {error ? <p className="ownership-error" role="alert">{error}</p> : <p className="ownership-status" role="status">Checking whether what a previous Lazybox left here still runs…</p>}
    {error && onCancel && <div className="ownership-actions"><button type="button" onClick={onCancel}>Cancel</button></div>}
  </div>;
  const processes = ownership.processes;
  const running = processes.filter(p => p.state === 'running').length;
  const unknown = processes.filter(p => p.state === 'unknown').length;
  return <section className="ownership-panel" aria-label="Unconfirmed processes">
    <p className="ownership-lead">
      <strong>A previous Lazybox left {processes.length === 1 ? 'a process' : 'processes'} here that it can’t confirm stopped.</strong>{' '}
      {what} until that is settled.
    </p>
    <ul className="ownership-list">
      {processes.map(p => <ProcessRow key={`${p.sessionId}/${p.commandId}/${p.pid}/${p.kind}`} process={p}/>)}
    </ul>
    {running > 0 && <p className="ownership-hint">Stop stops exactly {running === 1 ? 'the one marked running' : `the ${running} marked running`}, checked by its process ID and start time: never another program that has its number now.</p>}
    {!running && unknown > 0 && <p className="ownership-hint">Lazybox can’t check {unknown === 1 ? 'this one' : 'these'}. If you know {unknown === 1 ? 'it has' : 'they have'} stopped (you closed it, or the computer restarted since), say so.</p>}
    {error && <p className="ownership-error" role="alert">{error}</p>}
    <div className="ownership-actions">
      {onCancel && <button type="button" disabled={!!busy} onClick={onCancel}>Cancel</button>}
      <button type="button" ref={running ? undefined : first} disabled={!!busy || running > 0} title={running ? 'Lazybox sees it running: stop it first' : undefined} onClick={() => void resolve(false)}>
        {busy === 'stopped' ? 'Continuing…' : 'They’re already stopped, continue'}
      </button>
      <button type="button" className="primary" ref={running ? first : undefined} disabled={!!busy || running === 0} onClick={() => void resolve(true)}>
        <Icon name="stop" size={12}/>{busy === 'stop' ? 'Stopping…' : 'Stop them and continue'}
      </button>
    </div>
  </section>;
}

function ProcessRow({ process: p }: { process: OwnedProcess }) {
  const state = STATE[p.state] || STATE.unknown;
  return <li className="ownership-process" data-state={state.tone}>
    <div className="ownership-process-head">
      <Icon name={p.kind === 'agent' ? 'agent' : 'terminal'} size={13}/>
      <span className="ownership-process-name"><span className="ownership-process-kind">{p.kind === 'agent' ? 'Agent' : 'Command'}</span> {p.label || (p.kind === 'agent' ? 'Untitled conversation' : 'Command')}</span>
      <span className="ownership-state" data-tone={state.tone}>{state.label}</span>
    </div>
    {(p.pid > 0 || p.commandLine) && <code className="ownership-process-command">{p.pid > 0 ? `PID ${p.pid}` : 'No PID recorded'}{p.commandLine ? ` · ${p.commandLine}` : ''}</code>}
    {p.detail && <small>{p.detail}</small>}
  </li>;
}

/**
 * The panel in a dialog of its own: from the workspace row (New conversation,
 * its menu) when the workspace's ownership is unconfirmed.
 */
export function OwnershipDialog({ target, title, subject, what, onClose, onSettled }: {
  target: OwnershipTarget; title: string; subject?: string; what: string; onClose(): void; onSettled(note: string): void;
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  useEffect(() => { dialog.current?.showModal(); }, []);
  return <DraggableDialog ref={dialog} className="confirm-dialog ownership-dialog" aria-label={title} onCancel={event => { event.preventDefault(); onClose(); }}>
    <DialogHeader><h2>{title}</h2><button type="button" className="confirm-close" aria-label="Close" onClick={onClose}>×</button></DialogHeader>
    {subject && <p className="confirm-subject"><span>{subject}</span></p>}
    <OwnershipPanel target={target} what={what} onCancel={onClose} onSettled={onSettled}/>
  </DraggableDialog>;
}
