import { useEffect, useRef, useState } from 'react';
import type { Collection, WorkspaceDeletionPreview } from '@lazybox/protocol';
import { DraggableDialog, DialogHeader } from './DraggableDialog';
import { useRunner } from './RunnerContext';

export function DeleteWorkspaceDialog({ workspace, onClose, onDeleted }: {
  workspace: Collection; onClose(): void; onDeleted(): void;
}) {
  const { api } = useRunner();
  const dialog = useRef<HTMLDialogElement>(null);
  const cancel = useRef<HTMLButtonElement>(null);
  const pending = useRef(false);
  const [preview, setPreview] = useState<WorkspaceDeletionPreview>();
  const [error, setError] = useState('');
  const [discard, setDiscard] = useState(false);
  const [stop, setStop] = useState(false);
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    dialog.current?.showModal();
    cancel.current?.focus();
    const control = new AbortController();
    void api.previewWorkspaceDeletion({ collectionId: workspace.id }, { signal: control.signal })
      .then(setPreview, error => { if (!control.signal.aborted) setError(String(error)); });
    return () => control.abort();
  }, [api, workspace.id]);
  const active = preview?.activeSessionIds.length || 0;
  const dirty = !!preview && (preview.dirtyRepositories.length > 0 || preview.filesOutsideRepositories);
  const canDelete = !!preview && !preview.blocked && (!dirty || discard) && (!active || stop);
  return <DraggableDialog ref={dialog} className="delete-workspace-dialog" aria-label="Delete workspace"
    aria-describedby="workspace-deletion-description" onCancel={event => { if (busy) event.preventDefault(); else onClose(); }}>
    <DialogHeader>
      <h2>Delete workspace?</h2>
      <button type="button" aria-label="Cancel workspace deletion" disabled={busy} onClick={onClose}>×</button>
    </DialogHeader>
    <div className="deletion-subject"><strong>{workspace.name}</strong><span>{workspace.members.length} {workspace.members.length === 1 ? 'repository' : 'repositories'}</span></div>
    <p id="workspace-deletion-description">Remove this workspace and its local files.</p>
    <p className="deletion-note">Source repositories, branches, saved conversations and subscriptions are kept.</p>
    {!preview && !error && <p role="status">Checking files and running sessions…</p>}
    {preview?.blocked && <p className="deletion-warning" role="alert">{preview.blocked}</p>}
    <form onSubmit={async event => {
      event.preventDefault();
      if (pending.current || !canDelete) return;
      pending.current = true; setBusy(true); setError('');
      try {
        await api.deleteWorkspace({ collectionId: workspace.id, confirmName: workspace.name, discardChanges: discard, stopSessions: stop }, { timeoutMs: 120000 });
        onDeleted();
      } catch (error) { setError(String(error)); }
      finally { pending.current = false; setBusy(false); }
    }}>
      {!!active && <section className="deletion-warning">
        <strong>{active} running {active === 1 ? 'session' : 'sessions'}</strong>
        <p>Deletion will stop {active === 1 ? 'this session' : 'these sessions'} and their owned commands.</p>
        <label className="cleanup-choice"><input type="checkbox" checked={stop} onChange={event => setStop(event.target.checked)} disabled={busy} />Stop {active} active {active === 1 ? 'session' : 'sessions'} and their owned commands</label>
      </section>}
      {dirty && <section className="deletion-warning">
        <strong>Uncommitted or untracked files</strong>
        <p>{preview!.dirtyRepositories.length ? `In ${preview!.dirtyRepositories.join(', ')}. ` : ''}{preview!.filesOutsideRepositories ? 'Also found outside repository folders. ' : ''}These files, including ignored files, cannot be recovered after deletion.</p>
        <label className="cleanup-choice"><input type="checkbox" checked={discard} onChange={event => setDiscard(event.target.checked)} disabled={busy} />Discard these workspace files permanently</label>
      </section>}
      {error && <p className="deletion-warning" role="alert">{error}</p>}
      <footer>
        <button ref={cancel} type="button" autoFocus data-autofocus disabled={busy} onClick={onClose}>Cancel</button>
        <button className="danger" disabled={busy || !canDelete}>{busy ? 'Deleting…' : 'Delete workspace'}</button>
      </footer>
    </form>
  </DraggableDialog>;
}
