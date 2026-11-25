import { useState, type ComponentType } from 'react';
import type { Collection } from '@lazybox/protocol';
import type { ViewStateStore } from '@lazybox/platform';
import { ChatPanel } from './AgentWindow';
import type { OpenHandoffConversation } from './ConversationHandoff';
import { ChangesPanel } from './ChangesPanel';
import { CollectionPanel } from './CollectionPanel';
import { CommandPanel } from './CommandPanel';
import { ConversationList } from './ConversationList';
import { EditorArea } from './EditorArea';
import { ArtifactsPanel } from './ArtifactsPanel';
import { AgentsPanel } from './AgentsPanel';
import { FileExplorer } from './FileExplorer';
import { ProcessesPanel } from './ProcessesPanel';
import { ActionsPanel } from './ActionsPanel';
import { PreviewPanel } from './PreviewPanel';
import { WebPanel } from './WebPanel';
import { BrowserPanel } from './BrowserPanel';
import { PullRequestsPanel } from './PullRequestsPanel';
import { useRunner } from './RunnerContext';
import { TelemetryPanel } from './TelemetryPanel';
import { TerminalPanel } from './TerminalPanel';
import { conversationReader } from './transcript-cache';
import type { ViewTarget } from './workspace-model';
import { WorkspaceHome } from './WorkspaceHome';
import { NewTaskStart } from './NewTaskSheet';
import { TrainingWheels } from './TrainingWheels';
import { HelpAssistant } from './HelpAssistant';
import { ExtensionModule } from './ExtensionPanel';
import { useAskRequests } from './help-requests';

/** What a panel's header shows while its module has something better than its kind's title: a web page's title and icon. */
export type PanelTitle = { text: string; /** An image URL (a page's icon). */ icon?: string; tooltip?: string; webUrl?: string; agent?: { sessionId: string; provider: string } };
export type ExtensionPanelChoice = { id: string; panel: string };
export type ModuleProps = {
  onArrangeWindows?(choice: string): void;
  currentLayoutYaml?(): string;
  onReviewLayout?(yaml: string): void;
  extensionRequest?: { choice?: ExtensionPanelChoice; onOpened(): void };
  workspaceId: string;
  /** This panel's identity on its page, stable while it is open. */
  panelId?: string;
  chooseRepositoryFolder?(): Promise<string | null>;
  target: ViewTarget;
  onTarget(target: ViewTarget): void;
  collections: Collection[];
  error: string;
  loaded?: boolean;
  refresh(): void;
  viewState?: ViewStateStore;
  onOpenViewer?(sessionId: string, commandId: string): Promise<void>;
  onOpenChat?(): void;
  onWorkspaceCreated?(workspaceId: string): void;
  onNewConversation?(workspaceId: string): void;
  openMission?(id: string): Promise<void>;
  onShowSession?(target: ViewTarget): void;
  onOpenHandoff?: OpenHandoffConversation;
  onShowPanel?(kind: string, target?: ViewTarget): void;
  onShowModule?(kind: 'terminal' | 'changes'): void;
  /** Closes this panel (a terminal panel whose last shell exited cleanly, a web page moved to its own window). */
  onClosePanel?(): void;
  /** Replaces the header's title while set; undefined restores the kind's title. */
  onPanelTitle?(title?: PanelTitle): void;
};
/** `multiple`: a page may hold several panels of this kind (the page menu adds a new one). */
export type PanelModule = { title: string; icon?: string; multiple?: boolean; Component: ComponentType<ModuleProps> };

function ChatModule({ target, onTarget, viewState, collections, onOpenChat, onNewConversation, onShowModule, onShowPanel, onOpenHandoff, onPanelTitle }: ModuleProps) {
  const { api } = useRunner();
  const collection = collections.find(value => value.id === target.collectionId);
  // A workspace with no conversation open shows what it holds and how to start, not a form.
  if (collection && !target.sessionId) return <WorkspaceHome workspace={collection} viewState={viewState} onNewConversation={onNewConversation} onShowModule={onShowModule} onTarget={onTarget}/>;
  // Nothing chosen yet: how to start, not an empty chat.
  if (!target.collectionId && !target.sessionId) return <NewTaskStart viewState={viewState}/>;
  return <ChatPanel key={JSON.stringify([target.collectionId, conversationReader(api, target.sessionId).identity])} embedded showCommands={false} onOpenChat={onOpenChat}
    collectionId={target.collectionId} initialSessionId={target.sessionId}
    onOpenHandoff={onOpenHandoff} onChatTitle={onPanelTitle}
    allowStart={!!collection && collection.state === 'ready' && !collection.ownershipUncertain}
    viewState={viewState}
    onShowBrowser={onShowPanel ? () => onShowPanel('browser') : undefined}
    onOpenCommand={commandId => { onTarget({ ...target, commandId }); onShowPanel?.('console', { ...target, commandId }); }}
    onSessionChange={sessionId => onTarget({ ...target, sessionId, commandId: '' })}/>;
}

/**
 * Ask Lazybox: the help assistant (HelpAssistant.tsx), or a planning or
 * summary conversation. "Ask Lazybox" from the Help menu, ⌘K or ]] h brings
 * it back to Help and focuses its message box.
 */
function AssistantModule({ viewState, workspaceId, collections, currentLayoutYaml, onReviewLayout, onArrangeWindows }: ModuleProps) {
  const key = `${viewState?.key}:assistant-session`;
  const [sessionId, setSessionId] = useState(() => viewState?.getItem(key) || '');
  const [purpose, setPurpose] = useState<'help' | 'planning' | 'summary'>(() => { const saved = viewState?.getItem(`${key}:purpose`); return saved === 'planning' || saved === 'summary' ? saved : 'help'; });
  const [focusToken, setFocusToken] = useState(0);
  const changePurpose = (next: typeof purpose) => {
    if (next === purpose) return;
    setPurpose(next); viewState?.setItem(`${key}:purpose`, next); setSessionId(''); viewState?.setItem(key, '');
  };
  const [question, setQuestion] = useState<string>();
  useAskRequests(suggestion => {
    if (suggestion) setQuestion(draft => draft ?? suggestion);
    changePurpose('help');
    setFocusToken(token => token + 1);
  });
  if (purpose === 'help') return <HelpAssistant onArrangeWindows={onArrangeWindows} currentLayoutYaml={currentLayoutYaml} onReviewLayout={onReviewLayout} question={question} setQuestion={setQuestion} key={purpose} workspaceId={workspaceId} workspaceName={collections.find(c => c.id === workspaceId)?.name} viewState={viewState} sessionKey={key} focusToken={focusToken} onPurpose={changePurpose}/>;
  return <>
    <label className="assistant-purpose">Conversation<select aria-label="Assistant purpose" value={purpose} onChange={e => changePurpose(e.target.value as typeof purpose)}>
      <option value="help">Help</option>
      <option value="planning">Planning</option>
      <option value="summary">Summary</option>
    </select></label>
    <ChatPanel key={purpose} collectionId={workspaceId} purpose={purpose} embedded showCommands={false} initialSessionId={sessionId} viewState={viewState}
      onSessionChange={id => { setSessionId(id); viewState?.setItem(key, id); }}/>
  </>;
}

/** First-party modules use the same runtime and target contract in every page. */
export const builtinPanels: Readonly<Record<string, PanelModule>> = {
  collections: { title: 'Workspaces', icon: 'collections', Component: CollectionPanel },
  agents: { title: 'Agents', icon: 'activity', Component: ({ collections, target, viewState, onShowSession }) => <AgentsPanel collections={collections} target={target} viewState={viewState} onShowSession={onShowSession}/> },
  chat: { title: 'Agent chat', icon: 'chat', Component: ChatModule },
  console: { title: 'Command output', icon: 'output', Component: CommandPanel },
  terminal: { title: 'Terminal', icon: 'terminal', Component: ({ workspaceId, collections, viewState, onClosePanel, target, onShowSession }) => <TerminalPanel key={workspaceId} workspaceId={workspaceId} collections={collections} viewState={viewState} onClosePanel={onClosePanel} sessionId={target.collectionId === workspaceId ? target.sessionId : ''} onShowSession={onShowSession}/> },
  assistant: { title: 'Ask Lazybox', icon: 'ask', Component: AssistantModule },
  training: { title: 'Training Wheels', icon: 'keyboard', Component: TrainingWheels },
  changes: { title: 'Changes', icon: 'branch', Component: ({ workspaceId, viewState, target, onShowSession }) => <ChangesPanel key={workspaceId} workspaceId={workspaceId} sessionId={target.collectionId === workspaceId ? target.sessionId : ''} onShowSession={onShowSession} viewState={viewState}/> },
  usage: { title: 'Usage & cost', icon: 'sliders', Component: ({ workspaceId }) => <TelemetryPanel workspaceId={workspaceId}/> },
  sessions: { title: 'Sessions', icon: 'history', Component: ({ workspaceId, target, onTarget }) => <ConversationList collectionId={workspaceId} target={target} onTarget={onTarget}/> },
  pulls: { title: 'Pull requests', icon: 'pullRequest', Component: ({ collections, onShowSession }) => <PullRequestsPanel collections={collections} onShowSession={onShowSession}/> },
  processes: { title: 'Processes', icon: 'processes', Component: ({ workspaceId, onShowSession, onShowPanel }) => <ProcessesPanel workspaceId={workspaceId} onShowSession={onShowSession} onShowPanel={onShowPanel}/> },
  actions: { title: 'Actions', icon: 'play', Component: ({ workspaceId, viewState, onShowPanel }) => <ActionsPanel key={workspaceId} workspaceId={workspaceId} viewState={viewState} onShowPanel={onShowPanel}/> },
  preview: { title: 'Preview', icon: 'eye', Component: ({ workspaceId }) => <PreviewPanel key={workspaceId} workspaceId={workspaceId}/> },
  web: { title: 'Web', icon: 'globe', multiple: true, Component: props => <WebPanel key={props.panelId} {...props}/> },
  browser: { title: 'Agent browser', icon: 'browser', Component: BrowserPanel },
  files: { title: 'Files', icon: 'folder', Component: ({ workspaceId, collections }) => <FileExplorer workspaceId={workspaceId} collections={collections}/> },
  editor: { title: 'Editor', icon: 'file', Component: ({ workspaceId, viewState }) => <EditorArea workspaceId={workspaceId} viewState={viewState}/> },
  artifacts: { title: 'Artifacts', icon: 'artifact', Component: ({ workspaceId, collections, viewState, target, onShowSession }) => <ArtifactsPanel workspaceId={workspaceId} collections={collections} viewState={viewState} target={target} onShowSession={onShowSession}/> },
  // The person's sandboxed extensions (EXTENSIONS.md): each panel shows one of them, or chooses.
  extension: { title: 'Extensions', icon: 'puzzle', multiple: true, Component: ExtensionModule },
};


/** A panel title inside a sentence: "Add usage & cost". */
export const sentence = (title: string) => title.charAt(0).toLowerCase() + title.slice(1);
