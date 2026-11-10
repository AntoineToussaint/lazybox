import { useEffect, useRef, useState } from 'react';
import type { Collection, Repository } from '@lazybox/protocol';
import { DraggableDialog, DialogHeader } from './DraggableDialog';
import { WorkspaceRepositoryDialog } from './WorkspaceRepositoryDialog';
import { repositoryLabel } from './repository-model';
import { useRunner } from './RunnerContext';
import { Icon } from './Icons';

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
    <DraggableDialog ref={dialog} className="workspace-repositories-dialog" aria-label={`Repositories in ${workspace.name}`} onCancel={onClose}>
      <DialogHeader>
        <div><small>Workspace repositories</small><h2>{workspace.name}</h2></div>
        <button aria-label="Close workspace repositories" onClick={onClose}>×</button>
      </DialogHeader>
      <p>{workspace.members.length} {workspace.members.length === 1 ? 'repository' : 'repositories'} in this workspace</p>
      {loading && <p role="status">Loading repository details…</p>}
      {error && <div className="repository-details-error" role="alert">
        <p>Could not load repository details. Workspace checkouts are still listed below.</p>
        <button type="button" onClick={() => setRevision(value => value + 1)}>Retry</button>
        <details><summary>Error details</summary><p>{error}</p></details>
      </div>}
      <ul className="workspace-repository-list">{workspace.members.map(member => {
        const repo = repos.find(value => value.id === member.spec?.repositoryId);
        return <li key={member.spec?.name}><Icon name="folder"/><div>
          <strong>{repo ? repositoryLabel(repo) : member.spec?.name}</strong>
          <small><Icon name="branch" size={12}/>{member.spec?.branch}</small>
        </div></li>;
      })}</ul>
      {!workspace.members.length && <p className="module-empty">Add a repository to start working with code here.</p>}
      <button className="primary" aria-label={`Add repositories to ${workspace.name}`} disabled={workspace.state !== 'ready' || workspace.ownershipUncertain} onClick={() => setAdding(true)}><Icon name="plus" size={14}/> Add repositories</button>
      <details className="source-details"><summary>Workspace location & source details</summary><p className="mono">{workspace.root}</p>{workspace.members.map(member => <p className="mono" key={member.spec?.name}>{member.path}</p>)}</details>
      {adding && <WorkspaceRepositoryDialog workspace={workspace} chooseRepositoryFolder={chooseRepositoryFolder} onClose={() => setAdding(false)} onSaved={() => { setAdding(false); setRevision(value => value + 1); onSaved(); }}/>}
    </DraggableDialog>
  );
}
