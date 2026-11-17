import { useEffect, useRef, useState } from 'react';
import { useRunner } from './RunnerContext';
import { DraggableDialog, DialogHeader } from './DraggableDialog';
import { Icon } from './Icons';
import { changedLabel, changedLines, errorText, staleApproval, useWorkspaceRuntime } from './workspace-runtime';
import './workspace-runtime.css';

/**
 * Review one repository's committed workspace scripts before they run: the
 * file itself, what changed since the last approval, and what approving means.
 * The approval names the hash of exactly what is shown here.
 */
export function ScriptApprovalDialog({ workspaceId, member, onClose }: { workspaceId: string; member: string; onClose(): void }) {
  const { api } = useRunner();
  const { runtime, store, busy } = useWorkspaceRuntime(api, workspaceId);
  const source = runtime?.sources.find(s => s.member === member);
  const setup = runtime?.setups.find(s => s.member === member);
  const dialog = useRef<HTMLDialogElement>(null), cancel = useRef<HTMLButtonElement>(null);
  const [error, setError] = useState('');
  // The version the person started reading: a new one on disk is announced, never swapped silently.
  const [seen, setSeen] = useState(source?.hash || '');
  const [replaced, setReplaced] = useState(false);
  useEffect(() => { dialog.current?.showModal(); cancel.current?.focus(); }, []);
  useEffect(() => {
    if (!source?.hash) return;
    if (seen && source.hash !== seen) setReplaced(true);
    setSeen(source.hash);
  }, [source?.hash]);
  useEffect(() => { if (source && source.approval !== 'required' && source.approval !== 'changed' && !busy.has(`approve:${member}`)) onClose(); }, [source?.approval]);
  if (!source || !store) return null;
  const pendingSetup = !!setup?.pendingApproval && !!setup.run;
  const marked = changedLines(source.content, source.changed);
  const working = busy.has(`approve:${member}`);
  const approve = async (runSetup: boolean) => {
    setError(''); setReplaced(false);
    const result = await store.approve(source, runSetup);
    if (result.ok) { onClose(); return; }
    if (staleApproval(result.error)) { setReplaced(true); await store.refresh(); return; }
    if (result.error) setError(errorText(result.error));
  };
  const lines = source.content.replace(/\n$/, '').split('\n');
  const title = `Review ${member}’s workspace scripts`;
  return <DraggableDialog ref={dialog} className="script-approval-dialog" aria-label={title} onCancel={event => { if (working) event.preventDefault(); else onClose(); }}>
    <DialogHeader><div><h2>{title}</h2><p className="approval-path"><Icon name="repository" size={13}/> {member} · <span className="mono">{source.path || '.lazybox/workspace.yaml'}</span></p></div><button type="button" aria-label="Close without approving" disabled={working} onClick={onClose}>×</button></DialogHeader>
    <p className="approval-explain"><strong>These scripts run code on this machine, as you</strong>, with your files, network and credentials. They come from the repository: approve them only if you trust its commits. You are asked again whenever they change.</p>
    {source.approval === 'changed' && source.changed.length > 0 && <div className="approval-changes" role="note"><span>Changed since you approved them:</span><ul aria-label="Changed entries">{source.changed.map(entry => <li key={entry}>{changedLabel(entry)}</li>)}</ul></div>}
    {replaced && <p className="approval-replaced" role="alert"><Icon name="alert" size={14}/> The file changed while you were reading it. This is the new version: review it before approving.</p>}
    <pre className="approval-file" aria-label={`${source.path || '.lazybox/workspace.yaml'} contents`} tabIndex={0}>{lines.map((line, i) => <span key={i} className={marked.has(i + 1) ? 'is-changed' : undefined} title={marked.has(i + 1) ? 'Changed since your last approval' : undefined}><span className="line-number" aria-hidden="true">{i + 1}</span>{line || ' '}{'\n'}</span>)}</pre>
    {error && <p className="settings-error" role="alert">{error}</p>}
    <footer>
      <button ref={cancel} type="button" autoFocus data-autofocus disabled={working} onClick={onClose}>Not now</button>
      <button type="button" className={pendingSetup ? '' : 'primary'} disabled={working} onClick={() => void approve(false)}>{working ? 'Approving…' : 'Approve'}</button>
      {pendingSetup && <button type="button" className="primary" disabled={working} title={setup!.run} onClick={() => void approve(true)}>Approve and run setup</button>}
    </footer>
  </DraggableDialog>;
}
