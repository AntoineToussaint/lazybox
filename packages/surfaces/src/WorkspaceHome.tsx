import { useEffect, useState } from 'react';
import type { Collection, Repository } from '@lazybox/protocol';
import type { ViewStateStore } from '@lazybox/platform';
import { Icon } from './Icons';
import { useRunner } from './RunnerContext';
import { useKeymap } from './keyboard';
import { repositoryLabel } from './repository-model';
import { useConversationIndex } from './useConversationIndex';
import { LiveStatusLabel } from './LiveIndicator';
import { conversationName } from './conversation-model';
import type { ViewTarget } from './workspace-model';
import './workspace-home.css';

/**
 * What a workspace shows before (or between) conversations: its repositories
 * and branch, one primary action to start, and the other ways in — a terminal
 * and the changes. Never a form: starting asks in the New conversation dialog.
 */
export function WorkspaceHome({ workspace, viewState, onNewConversation, onShowModule, onTarget }: { workspace: Collection; viewState?: ViewStateStore; onNewConversation?(workspaceId: string): void; onShowModule?(kind: 'terminal' | 'changes'): void; onTarget(target: ViewTarget): void }) {
  const { api, revision } = useRunner();
  const keymap = useKeymap(viewState);
  const index = useConversationIndex();
  const [repositories, setRepositories] = useState<Repository[]>([]);
  useEffect(() => {
    const control = new AbortController();
    void api.listRepositories({}, { signal: control.signal }).then(value => { if (!control.signal.aborted) setRepositories(value.repositories); }).catch(() => {});
    return () => control.abort();
  }, [api, revision, workspace.members.length]);
  const members = workspace.members.map(member => {
    const repo = repositories.find(value => value.id === member.spec?.repositoryId);
    return { key: member.spec?.name || member.path, label: repo ? repositoryLabel(repo) : member.spec?.name || 'Repository', branch: member.spec?.branch || '', ready: member.ready };
  });
  const branches = [...new Set(members.map(member => member.branch).filter(Boolean))];
  const ready = workspace.state === 'ready' && !workspace.ownershipUncertain;
  const recent = index.sessions.filter(session => session.collectionId === workspace.id).slice(0, 3);
  const shortcut = keymap['conversation.new'];
  return <section className="workspace-home" aria-labelledby={`workspace-home-${workspace.id}`}>
    <div className="workspace-home-body">
      <header>
        <span className="workspace-home-icon" aria-hidden="true"><Icon name="folder" size={18}/></span>
        <div>
          <h2 id={`workspace-home-${workspace.id}`}>{workspace.name}</h2>
          <p>{members.length ? `${members.length} ${members.length === 1 ? 'repository' : 'repositories'}` : 'No repositories'}{branches.length === 1 && <> · <span className="workspace-home-branch"><Icon name="branch" size={12}/>{branches[0]}</span></>}</p>
        </div>
      </header>
      {members.length > 0 && <ul className="workspace-home-repos" aria-label="Repositories">{members.map(member => <li key={member.key}><Icon name="folder" size={13}/><span title={member.label}>{member.label}</span>{branches.length > 1 && member.branch && <small><Icon name="branch" size={11}/>{member.branch}</small>}{!member.ready && <small className="pending">Preparing…</small>}</li>)}</ul>}
      {!ready && <p className="workspace-home-note" role="status">{workspace.ownershipUncertain ? 'Another Runner may own this workspace. Conversations can still be read.' : 'This workspace is getting ready. You can start once its repositories are in place.'}</p>}
      <div className="workspace-home-actions">
        <button className="primary" disabled={!ready || !onNewConversation} onClick={() => onNewConversation?.(workspace.id)}><Icon name="plus" size={15}/>Start a conversation{shortcut && <kbd>{shortcut}</kbd>}</button>
        <button disabled={!ready || !onShowModule} onClick={() => onShowModule?.('terminal')}><Icon name="terminal" size={15}/>Open a terminal</button>
        <button disabled={!onShowModule} onClick={() => onShowModule?.('changes')}><Icon name="branch" size={15}/>Show changes</button>
      </div>
      {recent.length > 0 && <nav className="workspace-home-recent" aria-label="Recent conversations in this workspace">
        <h3>Recent conversations</h3>
        {recent.map(session => <button key={session.id} onClick={() => onTarget({ collectionId: workspace.id, sessionId: session.id, commandId: '' })}><Icon name="chat" size={13}/><span className="workspace-home-title">{conversationName(session)}</span><LiveStatusLabel session={session}/></button>)}
      </nav>}
    </div>
  </section>;
}
