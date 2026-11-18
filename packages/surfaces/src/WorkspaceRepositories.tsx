import { useEffect, useRef, useState } from 'react';
import type { Collection, Repository } from '@lazybox/protocol';
import { DraggableDialog, DialogHeader } from './DraggableDialog';
import { WorkspaceRepositoryDialog } from './WorkspaceRepositoryDialog';
import { repositoryLabel } from './repository-model';
import { useRunner } from './RunnerContext';
import { Icon } from './Icons';
import { WorkspaceHomes } from './home-checkouts';

/** The dialog is bound to one named workspace, never the changing sidebar selection. */
export function WorkspaceRepositories({ workspace, onClose, onSaved, chooseRepositoryFolder }: {
  workspace: Collection;
  onClose(): void;
  onSaved(): void;
  chooseRepositoryFolder?(): Promise<string | null>;
}) {
  const { api } = useRunner();
  const dialog = useRef<HTMLDialogElement>(null);
  const [repos, setRepos] = useState<Repository[]>([]);
  const [adding, setAdding] = useState(false);
  const [removal, setRemoval] = useState<{ name: string; label: string }>();
  const [removing, setRemoving] = useState(false);
  const [removeError, setRemoveError] = useState('');
  const [removed, setRemoved] = useState<string[]>([]);
  const members = workspace.members.filter(member => !removed.includes(member.spec?.name ?? ''));
  const editable = workspace.state === 'ready' && !workspace.ownershipUncertain;
  async function removeRepository() {
    if (!removal || removing) return;
    setRemoving(true);
    setRemoveError('');
    try {
      await api.removeCollectionRepository({ collectionId: workspace.id, memberName: removal.name });
      setRemoved(names => [...names, removal.name]);
      setRemoval(undefined);
      onSaved();
    } catch (error) {
      setRemoveError(error instanceof Error ? error.message : String(error));
    } finally {
      setRemoving(false);
    }
  }
  const [revision, setRevision] = useState(0);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  useEffect(() => { dialog.current?.showModal(); }, []);
  useEffect(() => {
    const controller = new AbortController();
    setLoading(true);
    setError('');
    void api.listRepositories({}, { signal: controller.signal }).then(value => {
      if (!controller.signal.aborted) setRepos(value.repositories);
    }).catch(error => {
      if (!controller.signal.aborted) setError(String(error));
    }).finally(() => {
      if (!controller.signal.aborted) setLoading(false);
    });
    return () => controller.abort();
  }, [api, revision]);
  return (
    <DraggableDialog ref={dialog} className="workspace-repositories-dialog" aria-label={`Repositories in ${workspace.name}`} onCancel={event => { if (removal) { event.preventDefault(); if (!removing) setRemoval(undefined); } else onClose(); }}>
      <DialogHeader>
        <div><small>Workspace repositories</small><h2>{workspace.name}</h2></div>
        <button aria-label="Close workspace repositories" disabled={removing} onClick={onClose}>×</button>
      </DialogHeader>
      <p>{members.length} {members.length === 1 ? 'repository' : 'repositories'} in this workspace</p>
      {loading && <p role="status">Loading repository details…</p>}
      {error && <div className="repository-details-error" role="alert">
        <p>Could not load repository details. Workspace checkouts are still listed below.</p>
        <button type="button" onClick={() => setRevision(value => value + 1)}>Retry</button>
        <details><summary>Error details</summary><p>{error}</p></details>
      </div>}
      <ul className="workspace-repository-list">{members.map(member => {
        const repo = repos.find(value => value.id === member.spec?.repositoryId);
        const name = member.spec?.name ?? '';
        const label = repo ? repositoryLabel(repo) : name;
        return <li key={name}><Icon name="folder"/><div>
          <strong>{label}</strong>
          <small><Icon name="branch" size={12}/>{member.spec?.branch}</small>
        </div>
          <button type="button" aria-label={`Remove ${name} from ${workspace.name}`} disabled={!editable || !name || !!removal}
            onClick={() => { setRemoveError(''); setRemoval({ name, label }); }}>Remove…</button>
        </li>;
      })}</ul>
      {removal && <section aria-label="Confirm repository removal">
        <p>Remove <strong>{removal.label}</strong> from this workspace? This deletes its workspace checkout and keeps its local branch.</p>
        {removeError && <p role="alert">{removeError}</p>}
        <button type="button" autoFocus disabled={removing} onClick={() => setRemoval(undefined)}>Cancel</button>
        <button type="button" disabled={removing} onClick={() => void removeRepository()}>{removing ? 'Removing…' : 'Remove repository'}</button>
      </section>}
      {!members.length && <p className="module-empty">Add a repository to start working with code here.</p>}
      <WorkspaceHomes workspace={{ ...workspace, members }} chooseFolder={chooseRepositoryFolder} onOpened={onClose}/>
      <button className="primary" aria-label={`Add repositories to ${workspace.name}`} disabled={!editable || !!removal} onClick={() => setAdding(true)}><Icon name="plus" size={14}/> Add repositories</button>
      <details className="source-details"><summary>Workspace location & source details</summary><p className="mono">{workspace.root}</p>{members.map(member => <p className="mono" key={member.spec?.name}>{member.path}</p>)}</details>
      {adding && <WorkspaceRepositoryDialog workspace={workspace} chooseRepositoryFolder={chooseRepositoryFolder} onClose={() => setAdding(false)} onSaved={() => { setAdding(false); setRemoved([]); setRevision(value => value + 1); onSaved(); }}/>}
    </DraggableDialog>
  );
}
