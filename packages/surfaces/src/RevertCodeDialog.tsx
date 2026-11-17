import { useEffect, useRef, useState } from 'react';
import type { CodeRevert, RevertCodePreview, TurnCheckpoint } from '@lazybox/protocol';
import { DraggableDialog, DialogHeader } from './DraggableDialog';
import { useRunner } from './RunnerContext';
import { plural, revertAction } from './turn-checkpoints';
import './turn-checkpoints.css';

const SHOWN = 40;

/**
 * Confirms putting the workspace's files back to before a turn. It lists
 * exactly what will change, says what stays (commits, ignored files, files
 * too large to checkpoint, repositories added since), and that the revert
 * can be undone. Nothing changes until the person confirms.
 */
export function RevertCodeDialog({ sessionId, turn, later = 0, onClose, onReverted }: { sessionId: string; turn: TurnCheckpoint; later?: number; onClose(): void; onReverted(revert: CodeRevert): void }) {
  const { api } = useRunner();
  const dialog = useRef<HTMLDialogElement>(null);
  const cancel = useRef<HTMLButtonElement>(null);
  const [preview, setPreview] = useState<RevertCodePreview>();
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    dialog.current?.showModal();
    cancel.current?.focus();
    const control = new AbortController();
    void api.previewRevertCode({ sessionId, turn: turn.turn }, { signal: control.signal, timeoutMs: 60000 })
      .then(setPreview, e => { if (!control.signal.aborted) setError(String(e)); });
    return () => control.abort();
  }, [api, sessionId, turn.turn]);
  const files = preview?.repositories.reduce((sum, repo) => sum + repo.files.length, 0) || 0;
  const commits = preview?.repositories.filter(repo => repo.commitsSince > 0) || [];
  const moved = preview?.repositories.filter(repo => repo.branchThen && repo.branchNow && repo.branchThen !== repo.branchNow) || [];
  const skipped = turn.before.filter(repo => repo.skippedCount > 0);
  const canRevert = !!preview && !preview.blocked && files > 0 && !busy;
  const revert = async () => {
    if (!canRevert) return;
    setBusy(true); setError('');
    try { onReverted(await api.revertCode({ sessionId, turn: turn.turn }, { timeoutMs: 180000 })); }
    catch (e) { setError(String(e)); setBusy(false); }
  };
  return <DraggableDialog ref={dialog} className="revert-code-dialog" aria-label={`Revert files to before turn ${turn.turn}`} aria-describedby="revert-code-description"
    onCancel={event => { if (busy) event.preventDefault(); else onClose(); }}>
    <DialogHeader>
      <h2>Revert files to before turn {turn.turn}?</h2>
      <button type="button" aria-label="Cancel revert" disabled={busy} onClick={onClose}>×</button>
    </DialogHeader>
    {turn.prompt && <p className="revert-prompt">“{turn.prompt}”</p>}
    <p id="revert-code-description">Every file in this workspace goes back to how it was before this turn started: this undoes what {later ? `this turn and the ${plural(later, 'turn')} after it` : 'this turn'} changed, and any edit made since. The conversation stays as it is, and the agent is told with your next message.</p>
    {!preview && !error && <p role="status">Comparing the files now with the checkpoint…</p>}
    {preview?.blocked && <p className="revert-warning" role="alert">{preview.blocked}</p>}
    {preview && !files && !preview.blocked && <p className="revert-note" role="status">The files already match this checkpoint. There is nothing to revert.</p>}
    {preview && files > 0 && <div className="revert-files" aria-label="Files that will change">
      <strong>{plural(files, 'file')} will change</strong>
      {preview.repositories.map(repo => repo.files.length > 0 && <section key={repo.member}>
        <h3>{repo.member}</h3>
        <ul>{repo.files.slice(0, SHOWN).map(file => <li key={file.path}><span className="revert-action" data-status={file.status}>{revertAction(file.status)}</span><code>{file.status === 'R' ? `${file.originalPath} → ${file.path}` : file.path}</code></li>)}</ul>
        {(repo.files.length > SHOWN || repo.truncated) && <p className="revert-note">And {repo.truncated ? 'more' : plural(repo.files.length - SHOWN, 'other file')}.</p>}
      </section>)}
    </div>}
    {preview && <ul className="revert-keeps">
      {commits.map(repo => <li key={'c:' + repo.member}><strong>{repo.member}:</strong> {plural(repo.commitsSince, 'commit')} made since {repo.commitsSince === 1 ? 'stays' : 'stay'} on the branch. The revert shows as uncommitted changes you can review before committing.</li>)}
      {moved.map(repo => <li key={'b:' + repo.member}><strong>{repo.member}:</strong> now on <code>{repo.branchNow}</code>; the checkpoint was taken on <code>{repo.branchThen}</code>. Only files change; the branch does not.</li>)}
      {preview.repositories.filter(repo => repo.error).map(repo => <li key={'e:' + repo.member}><strong>{repo.member}:</strong> {repo.error}</li>)}
      {skipped.map(repo => <li key={'s:' + repo.member}><strong>{repo.member}:</strong> {plural(repo.skippedCount, 'untracked file')} too large or too many to checkpoint {repo.skippedCount === 1 ? 'is' : 'are'} left as {repo.skippedCount === 1 ? 'it is' : 'they are'}.</li>)}
      <li>Ignored files (build output, dependencies) are left as they are.</li>
      <li>The current files are saved first, so you can undo this revert.</li>
    </ul>}
    {error && <p className="revert-warning" role="alert">{error}</p>}
    <footer>
      <button ref={cancel} type="button" autoFocus data-autofocus disabled={busy} onClick={onClose}>Cancel</button>
      <button type="button" className="danger" disabled={!canRevert} onClick={() => void revert()}>{busy ? 'Reverting…' : files ? `Revert ${plural(files, 'file')}` : 'Revert files'}</button>
    </footer>
  </DraggableDialog>;
}
