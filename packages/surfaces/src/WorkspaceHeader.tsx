import { HelpItems, HelpMenu } from './HelpMenu';
import type { Collection, InfoResponse } from '@lazybox/protocol';
import type { AppHost, ViewStateStore } from '@lazybox/platform';
import { UpdateNotice } from './UpdateDialog';
import { ActionMenu } from './ActionMenu';
import { Icon } from './Icons';
import { actionEnabled, bindings, displayBinding, runAction, useActionRevision } from './actions';
import { useKeymap } from './keyboard';
import { AttentionCenter } from './Inbox';
import { useSyncExternalStore, type ReactNode } from 'react';
import { RunControl } from './RunControl';
import { RunnerIndicator } from './RunnerControl';
import { useWindowChannel } from './RunnerDirectory';
import { PullRequestPills } from './PullRequestsPanel';
import { useGitSummaries } from './useGitSummaries';
import { shortcut } from './workbench';
import type { ViewTarget, WorkspacePage } from './workspace-model';

/**
 * The window's header: the navigator (on a phone), its pages and Settings,
 * the inbox, the workspace's tools, a new conversation, the Runner, and the
 * page's own actions.
 */
const PHONE = '(max-width: 479px)';
export function WorkspaceHeader({ onBack, app, pages, selectedPage, missions, onOpenMissions, settings, pending, fixedWorkspace, navigatorOpen, collections, selection, viewState, workspaceID, info, terminalReady, canShowNewSession, editing, layoutMenu, canOpenWorkspaceWindow, onOpenNavigator, onSelectPage, extensionsOpen, onManageExtensions, onOpenSettings, onOpenAttention, onShowChanges, onBrowseFiles, onShowTerminal, onNewConversation, onOpenRunner, onOpenWorkspaceWindow, onToggleEditing, onToggleConfiguring, onShowModule }: {
  /** The desktop app, for its update notice (absent in a browser and in workspace windows). */
  app?: AppHost;
  pages: WorkspacePage[];
  selectedPage: string;
  /** Settings are on screen instead of a page. */
  settings: boolean;
  missions: boolean;
  onOpenMissions(): void;
  pending: boolean;
  /** A window that shows one workspace: no Settings page and no Runner settings. */
  fixedWorkspace: boolean;
  navigatorOpen: boolean;
  collections: Collection[];
  selection: ViewTarget;
  viewState?: ViewStateStore;
  workspaceID: string;
  info?: InfoResponse;
  terminalReady: boolean;
  canShowNewSession: boolean;
  editing: boolean;
  /** The Panels and Layout menus, when the page is shown as a dock. */
  layoutMenu?: ReactNode;
  canOpenWorkspaceWindow: boolean;
  onOpenNavigator(): void;
  onSelectPage(id: string): void;
  onBack?: () => void;
  extensionsOpen: boolean;
  onManageExtensions(): void;
  onOpenSettings(): void;
  onOpenAttention(target: ViewTarget): void;
  onShowChanges(): void;
  onBrowseFiles(): void;
  onShowTerminal(): void;
  onNewConversation(): void;
  onOpenRunner(): void;
  onOpenWorkspaceWindow(): void;
  onToggleEditing(): void;
  onToggleConfiguring(): void;
  onShowModule(kind: string): void;
}) {
  // A phone's header has room for the essentials: Files, Terminal and Help go into ⋯
  // (hidden by CSS at once as the window narrows, listed in ⋯ here).
  const phone = useSyncExternalStore(listener => { const query = matchMedia(PHONE); query.addEventListener('change', listener); return () => query.removeEventListener('change', listener); }, () => matchMedia(PHONE).matches);
  // Each repository's current branch: a pill whose pull request is on it opens the PR loop.
  const git = useGitSummaries();
  const branches = new Map((git.byWorkspace.get(workspaceID)?.repositories || []).map(repo => [repo.member, repo.branch]));
  // New task where this window can start one (a workspace's own window cannot: it has no sheet).
  const keymap = useKeymap(viewState);
  useActionRevision();
  const keysOf = (id: 'task.new' | 'conversation.new') => { const first = bindings(keymap[id])[0]; return first ? displayBinding(first).join('') : ''; };
  const taskKeys = fixedWorkspace ? undefined : keysOf('task.new'), conversationKeys = keysOf('conversation.new');
  const channel = useWindowChannel(app);
  const workspaceName = workspaceID ? collections.find(workspace => workspace.id === workspaceID)?.name || "Current workspace" : "No workspace selected";
  const newConversation = <button className={`toolbar-action new-conversation-action${taskKeys !== undefined ? ' icon-only' : ''}`}
    disabled={!info || pending || !canShowNewSession} aria-label="New conversation"
    title={`${workspaceID ? `New conversation in ${workspaceName}` : 'New conversation in a new workspace'}${conversationKeys ? ` (${conversationKeys})` : ''}`}
    onClick={onNewConversation}><Icon name={workspaceID ? 'chat' : 'plus'}/>{taskKeys === undefined && <span>New conversation</span>}</button>;
  // The window's title bar in the desktop app (title-bar.ts): its empty parts move the window.
  return <header className="workspace-header" data-window-drag-region="deep">
    {!missions && <button className="mobile-navigation-button icon-button" aria-label="Open workspace navigator" aria-expanded={navigatorOpen} onClick={onOpenNavigator}><Icon name="sidebar"/></button>}
    <div className="workspace-brand"><span className="brand-mark">l<span>↗</span></span><strong>lazybox</strong>
      {/* Which Lazybox this window is, unmistakably, when it is not the release app: Dev's and Working's windows look alike otherwise. */}
      {channel && <span className="brand-channel" data-channel={channel.toLowerCase()} title={`This window is Lazybox ${channel}: it shows ${channel}'s Runner`}>{channel}</span>}
    </div>
    <button className="icon-button" disabled={!onBack} onClick={onBack} aria-label="Go back" title="Back (Alt+← or ⌘[)"><Icon name="back"/></button>
    <nav aria-label="Pages">
      {!fixedWorkspace && <button className={missions ? 'selected' : ''} aria-current={missions ? 'page' : undefined} onClick={onOpenMissions}>Missions</button>}
      {missions ? <button onClick={() => onSelectPage(selectedPage)}>Workspaces</button> : pages.map(value => <button key={value.id} disabled={pending} className={!settings && value.id === selectedPage ? 'selected' : ''} aria-current={!settings && value.id === selectedPage ? 'page' : undefined} onClick={() => onSelectPage(value.id)}>{value.name}</button>)}
      {!fixedWorkspace && <button className={extensionsOpen ? 'selected' : ''} aria-current={extensionsOpen ? 'page' : undefined} onClick={onManageExtensions}>Extensions</button>}
      {!fixedWorkspace && <button className={settings && !extensionsOpen ? 'selected' : ''} aria-current={settings && !extensionsOpen ? 'page' : undefined} onClick={onOpenSettings}>Settings</button>}
    </nav>
    <div className="workspace-header-actions">
      <AttentionCenter collections={collections} selection={selection} viewState={viewState} onOpen={onOpenAttention}/>
      {!missions && <>
        <div className="workspace-tools" role="group" aria-label={workspaceName}>
          <span className="workspace-tool-name" title={workspaceName}>{workspaceName}</span>
          {!settings && !!workspaceID && <>
            <PullRequestPills workspaceId={workspaceID} branches={branches} onShowChanges={onShowChanges} onShowPanel={() => onShowModule('pulls')}/>
            <button className="toolbar-action phone-overflow" aria-label="Browse files" title={`Browse files · Go to file ${shortcut('Mod+P')}`} onClick={onBrowseFiles}><Icon name="folder"/><span>Files</span></button>
            <RunControl workspaceId={workspaceID} disabled={!terminalReady} onShowPanel={onShowModule}/>
            <button className="toolbar-action phone-overflow" disabled={!terminalReady} aria-label="Open terminal" title="Open a terminal in this workspace" onClick={onShowTerminal}><Icon name="terminal"/><span>Terminal</span></button>
          </>}
          {workspaceID && newConversation}
        </div>

        {(taskKeys !== undefined || !workspaceID) && <span className="new-work-split" role="group" aria-label="Start work">
          {taskKeys !== undefined && <button className="toolbar-action new-task-action" disabled={!actionEnabled('task.new')} aria-label="New task" title={`New task: repositories, an agent and what to do${taskKeys ? ` (${taskKeys})` : ''}`} onClick={() => runAction('task.new')}><Icon name="plus"/><span>New task</span>{taskKeys && <kbd>{taskKeys}</kbd>}</button>}
          {!workspaceID && newConversation}
        </span>}
        {/* Panels, Layout and Help (with What's new) sit together: how the window is arranged and explained. */}
        {layoutMenu}
      </>}
      <HelpMenu settings={!fixedWorkspace}/>
      <UpdateNotice app={app}/>
      <RunnerIndicator onOpen={fixedWorkspace ? undefined : onOpenRunner}/>
      {!missions && <ActionMenu label="Page actions">
        {!fixedWorkspace && phone && <button onClick={onManageExtensions}>Extensions</button>}
        {canOpenWorkspaceWindow && <button onClick={onOpenWorkspaceWindow}>Open workspace window</button>}
        <button disabled={settings} onClick={onToggleEditing}>{editing ? 'Finish customizing' : 'Customize pages'}</button>
        <button disabled={settings} onClick={onToggleConfiguring}>Layout configuration</button>
        {!settings && workspaceID && !phone && <button onClick={onShowChanges}>Review changes</button>}
        {!settings && workspaceID && <button className="workspace-pulls-overflow" onClick={() => onShowModule('pulls')}>Pull requests in {workspaceName}</button>}
        {phone && <>
          <small className="action-menu-heading">{workspaceName}</small>
          <button disabled={settings || !workspaceID} onClick={onShowChanges}>Review changes</button>
          <button disabled={settings || !workspaceID} onClick={onBrowseFiles}>Browse files</button>
          <button disabled={settings || !terminalReady} onClick={onShowTerminal}>Open terminal</button>
          <small className="action-menu-heading">Help</small>
          <HelpItems settings={!fixedWorkspace}/>
        </>}
      </ActionMenu>}
    </div>
  </header>;
}
