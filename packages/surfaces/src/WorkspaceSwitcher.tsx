import type { Collection, InfoResponse } from '@lazybox/protocol';
import { agentTitle, isAcp } from './providers';
import { conversationOption, mergeConversations } from './conversation-model';
import { Icon } from './Icons';
import type { ViewTarget } from './workspace-model';
import { RunnerBadge, useRunnerIdentity } from './RunnerDirectory';

/**
 * The page's workspace (with its repositories) and conversation, chosen from
 * lists. While a chat panel shows the conversation its header names it, so
 * the conversation list keeps its place but shows only how many there are,
 * never the title a second time.
 */
export function WorkspaceSwitcher({ selection, workspaceID, collections, info, locked, conversationShown = false, onSelect, onShowRepositories }: {
  selection: ViewTarget;
  workspaceID: string;
  collections: Collection[];
  info?: InfoResponse;
  /** The workspace cannot change: a detach or dock is in flight, or the window shows one workspace. */
  locked: boolean;
  /** A chat panel on the page shows the conversation already. */
  conversationShown?: boolean;
  onSelect(target: ViewTarget): void;
  onShowRepositories(workspace?: Collection): void;
}) {
  const workspace = collections.find(value => value.id === workspaceID);
  const identity = useRunnerIdentity();
  const providerName = (profileId: string) => agentTitle(info?.profiles.find(profile => profile.id === profileId)?.provider, info?.profiles, 'Claude');
  // An ACP agent's profile is the agent itself: its label would only repeat its name.
  const profileLabel = (profileId: string) => { const profile = info?.profiles.find(p => p.id === profileId); return profile && !isAcp(profile.provider) ? profile.label : undefined; };
  const conversations = mergeConversations([], info?.sessions || []).filter(value => !selection.collectionId || value.collectionId === selection.collectionId);
  return <div className="workspace-context">
    <div className="collection-breadcrumb">
      {/* With several Runners in the window, whose workspace this is. */}
      {identity.multi && <RunnerBadge runner={identity.id} className="workspace-runner"/>}
      <Icon name="folder" size={17}/>
      <label><span className="sr-only">Workspace</span><select disabled={locked} value={selection.collectionId} onChange={event => { if (event.target.value !== workspaceID) onSelect({ collectionId: event.target.value, sessionId: '', commandId: '' }); }}>
        <option value="">Choose a workspace</option>
        {selection.collectionId && !collections.some(value => value.id === selection.collectionId) && <option value={selection.collectionId}>Unavailable workspace</option>}
        {collections.map(value => <option key={value.id} value={value.id}>{value.name}</option>)}
      </select></label>
      <button className="workspace-repositories-link" disabled={!workspace} onClick={() => onShowRepositories(workspace)}>{workspace?.members.length || 0} {workspace?.members.length === 1 ? 'repository' : 'repositories'}</button>
    </div>
    <label className={`session-switcher${conversationShown ? ' is-compact' : ''}`}><span className="sr-only">Session</span>
      {conversationShown && <span className="session-switcher-face" aria-hidden="true"><Icon name="chat" size={13}/>{conversations.length} {conversations.length === 1 ? 'conversation' : 'conversations'}</span>}
      <select value={selection.sessionId} onChange={event => { const session = info?.sessions.find(value => value.id === event.target.value); onSelect({ collectionId: session ? session.collectionId : selection.collectionId, sessionId: event.target.value, commandId: '' }); }}>
      <option value="">Choose a conversation</option>
      {selection.sessionId && !info?.sessions.some(value => value.id === selection.sessionId) && <option value={selection.sessionId}>Unavailable session</option>}
      {conversations.map(value => <option key={value.id} value={value.id}>{conversationOption(value, conversations, providerName, profileLabel)}</option>)}
    </select></label>
  </div>;
}
