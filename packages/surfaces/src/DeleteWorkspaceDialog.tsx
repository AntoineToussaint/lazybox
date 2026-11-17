import { useEffect, useState } from 'react';
import type { Collection, WorkspaceDeletionPreview } from '@lazybox/protocol';
import { ConfirmCheckbox, ConfirmDialog, ConfirmWarning } from './ConfirmDialog';
import { useRunner } from './RunnerContext';

export function DeleteWorkspaceDialog({ workspace, onClose, onDeleted }: {
  workspace: Collection; onClose(): void; onDeleted(): void;
}) {
  const { api } = useRunner();
  const [preview, setPreview] = useState<WorkspaceDeletionPreview>();
  const [error, setError] = useState('');
  const [discard, setDiscard] = useState(false);
  const [stop, setStop] = useState(false);
  useEffect(() => {
    const control = new AbortController();
    void api.previewWorkspaceDeletion({ collectionId: workspace.id }, { signal: control.signal })
      .then(setPreview, error => { if (!control.signal.aborted) setError(String(error)); });
    return () => control.abort();
  }, [api, workspace.id]);
  const active = preview?.activeSessionIds.length || 0;
  const dirty = !!preview && (preview.dirtyRepositories.length > 0 || preview.filesOutsideRepositories);
  const canDelete = !!preview && !preview.blocked && (!dirty || discard) && (!active || stop);
  return <ConfirmDialog title="Delete workspace?" label="Delete workspace" closeLabel="Cancel workspace deletion"
    subject={workspace.name} subjectMeta={`${workspace.members.length} ${workspace.members.length === 1 ? 'repository' : 'repositories'}`}
    kept="Source repositories, branches, saved conversations and subscriptions are kept."
    status={!preview && !error ? 'Checking files and running sessions…' : undefined} error={preview?.blocked || error}
    details={busy => <>
      {!!active && <ConfirmWarning title={`${active} running ${active === 1 ? 'session' : 'sessions'}`}>
        <p>Deletion will stop {active === 1 ? 'this session' : 'these sessions'} and their owned commands.</p>
        <ConfirmCheckbox label={`Stop ${active} active ${active === 1 ? 'session' : 'sessions'} and their owned commands`} checked={stop} disabled={busy} onChange={setStop}/>
      </ConfirmWarning>}
      {dirty && <ConfirmWarning title="Uncommitted or untracked files">
        <p>{preview!.dirtyRepositories.length ? `In ${preview!.dirtyRepositories.join(', ')}. ` : ''}{preview!.filesOutsideRepositories ? 'Also found outside repository folders. ' : ''}These files, including ignored files, cannot be recovered after deletion.</p>
        <ConfirmCheckbox label="Discard these workspace files permanently" checked={discard} disabled={busy} onChange={setDiscard}/>
      </ConfirmWarning>}
    </>}
    canConfirm={canDelete} confirm="Delete workspace" working="Deleting…" onClose={onClose}
    onConfirm={async () => {
      await api.deleteWorkspace({ collectionId: workspace.id, confirmName: workspace.name, discardChanges: discard, stopSessions: stop }, { timeoutMs: 120000 });
      onDeleted();
    }}>
    Remove this workspace and its local files.
  </ConfirmDialog>;
}
