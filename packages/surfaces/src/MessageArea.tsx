import { useCallback, useContext, useMemo, useRef, useState, type ReactNode } from 'react';
import type { RunnerClient } from '@lazybox/client-core';
import type { Session } from '@lazybox/protocol';
import type { OpenModifiers } from './file-links';
import type { FileLink } from './file-reference';
import { ConversationSourceContext, currentConversationSource } from './conversation-source';
import { humanError } from './human-error';
import type { HandoffMessage, OpenHandoffConversation } from './ConversationHandoff';
import { ConversationLoading, MessageList } from './MessageList';
import { MessageScopeProvider } from './MessageArtifactActions';
import { useFileLinkTracking } from './FileArtifactActions';
import { revealMessage, useRevealRequests, type RevealMessageRequest } from './artifact-events';
import { PromptNavigator } from './PromptNavigator';
import { RequestCard } from './session-parts';
import { ToolLineContext } from './ToolLine';
import { latestPlanId, type ActivityDetail } from './tool-lines';
import { exitFault, faultSentence, transportFault } from './subscription-model';
import type { useChatScroll } from './useChatScroll';
import type { TranscriptMessage } from './transcript-model';
import type { useTranscript } from './useTranscript';
import { PanelShownContext } from './useAttention';

type ChatScroll = ReturnType<typeof useChatScroll>;
type Transcript = ReturnType<typeof useTranscript>;

/**
 * The conversation pane: the prompt navigator on its edge, the messages and
 * the agent's pending requests (and a subscription limit) in the scrolling pane, and a way back to the
 * latest message once the reader scrolled away from it.
 */
export function MessageArea({ api, session, sessionID, transcript, scroll, spoken, agentName, limited, nameFor, onOpenFile, onError, onHandoff, onOpenHandoff, onBranch, subscription, enableFullAccess, detail = 'smart', alwaysShowTime }: {
  api: RunnerClient;
  alwaysShowTime?: boolean;
  session?: Session;
  sessionID: string;
  transcript: Transcript;
  scroll: ChatScroll;
  /** Something was said: a conversation that only saw Runner restarts is still empty. */
  spoken: boolean;
  agentName: string;
  /** At a subscription limit: the limit card stands in for the fault. */
  limited: boolean;
  /** A subscription's name by profile ID, for switch lines. */
  nameFor(id: string): string;
  onOpenFile?(reference: FileLink, how: OpenModifiers): void;
  onError(message: string): void;
  onHandoff?(message: HandoffMessage): void;
  enableFullAccess?(): Promise<boolean>;
  onBranch?(message: TranscriptMessage, sideQuestion?: boolean): void;
  onOpenHandoff?: OpenHandoffConversation;
  /** The subscription limit card and switch dialog, after the requests. */
  subscription?: ReactNode;
  /** How much of the agent's tool activity to show. */
  detail?: ActivityDetail;
}) {
  const conversation = transcript.messages;
  // Artifact actions on agent messages, and "Open source message" from an artifact.
  const artifactScope = useMemo(() => session?.collectionId ? { collectionId: session.collectionId, sessionId: sessionID, conversationId: session.conversationId } : undefined, [session?.collectionId, session?.conversationId, sessionID]);
  // A file an agent named is tracked from its link, as from the Files tree.
  const fileTracking = useFileLinkTracking(artifactScope?.collectionId, scroll.paneRef);
  const shown = useContext(PanelShownContext);
  useRevealRequests(sessionID, shown && !transcript.loading && !transcript.partial && conversation.length > 0, scroll.revealMessage);
  const handoffWorkspace = session?.collectionId;
  const sourceHost = useRef({ api, handoffWorkspace, onOpenHandoff, onError });
  sourceHost.current = { api, handoffWorkspace, onOpenHandoff, onError };
  const openSource = useCallback(async (source: RevealMessageRequest) => {
    const { api, handoffWorkspace, onOpenHandoff, onError } = sourceHost.current;
    if (!handoffWorkspace || !onOpenHandoff) return;
    try {
      const original = await currentConversationSource(api, handoffWorkspace, source);
      const latest = sourceHost.current;
      if (latest.api !== api || latest.handoffWorkspace !== handoffWorkspace || !latest.onOpenHandoff) return;
      if (!original) {
        onError('The source message is unavailable in this workspace.');
        return;
      }
      if (latest.onOpenHandoff({ collectionId: handoffWorkspace, sessionId: original.sessionId, commandId: '' })) revealMessage(original);
      else onError('Could not open the source conversation here.');
    } catch (error) { onError('Could not open the source message. ' + humanError(error)); }
  }, []);
  // Coding turns are checkpointed: each gets a marker to show or revert its changes.
  const coding = !!session && (!session.purpose || session.purpose === 'coding');
  // Tool lines show paths relative to the conversation's folder, and open files.
  const latestPlan = useMemo(() => latestPlanId(conversation), [conversation]);
  const toolScope = useMemo(() => ({ root: session?.workingDirectory || '', onOpenFile, latestPlan, detail }), [session?.workingDirectory, onOpenFile, latestPlan, detail]);
  const checkpoints = useMemo(() => coding ? { sessionId: sessionID, workspaceId: session!.collectionId, busy: !!session!.busy, provider: session!.provider, profileId: session!.profileId } : undefined, [coding, sessionID, session?.collectionId, session?.busy, session?.provider, session?.profileId]);
  return <>
    <div className="messages-frame">
      <PromptNavigator messages={conversation} paneRef={scroll.paneRef} onReveal={scroll.revealMessage}/>
      <div className="messages" tabIndex={-1} ref={scroll.paneRef} onScroll={scroll.onScroll}>
        <div className="messages-content" ref={scroll.contentRef}>
          {transcript.loading && !conversation.length && <ConversationLoading replay={transcript.replay}/>}
          {!transcript.loading && !transcript.error && !spoken && <EmptyConversation agentName={agentName}/>}
          <MessageScopeProvider scope={artifactScope}><ToolLineContext.Provider value={toolScope}><ConversationSourceContext.Provider value={onOpenHandoff && handoffWorkspace ? openSource : undefined}><MessageList conversationId={session?.conversationId || sessionID} key={session?.conversationId || sessionID} messages={spoken ? conversation : []} visible={scroll.visibleMessages} agentName={agentName} onOpenFile={onOpenFile} onShowEarlier={scroll.showEarlier} nameFor={nameFor} checkpoints={checkpoints} sessionId={sessionID} onReveal={scroll.revealMessage} onHandoff={onHandoff} onBranch={onBranch} detail={detail} alwaysShowTime={alwaysShowTime} loadingEarlier={transcript.partial}/></ConversationSourceContext.Provider></ToolLineContext.Provider></MessageScopeProvider>
          {session?.fault && !limited && <FaultBanner api={api} session={session} sessionID={sessionID} onError={onError}/>}
          <PendingRequests enableFullAccess={enableFullAccess} api={api} session={session} sessionID={sessionID} agentName={agentName} onError={onError}/>
          {subscription}
        </div>
      </div>
    </div>
    {fileTracking.element}
    {!scroll.followingLatest && conversation.length > 0 && <button className="jump-to-latest" onClick={scroll.jumpToLatest}>Jump to latest</button>}
  </>;
}

/**
 * Why the conversation cannot go on. It follows the Runner's fault, so it is
 * gone as soon as the Runner settles it (a re-adopted agent, Resume). A lost
 * stream offers Resume: the Runner re-attaches the live agent in place and
 * never starts a second one. An agent whose process exited offers it too:
 * its conversation goes on in a new process.
 */
function FaultBanner({ api, session, sessionID, onError }: { api: RunnerClient; session: Session; sessionID: string; onError(message: string): void }) {
  const [resuming, setResuming] = useState(false);
  const resume = async () => {
    if (resuming) return;
    setResuming(true); onError('');
    try { await api.resumeSession({ sessionId: sessionID }); }
    catch (error) { onError(`Could not resume the conversation: ${error instanceof Error ? error.message : String(error)}`); }
    finally { setResuming(false); }
  };
  return <div className="error" role="alert">
    {faultSentence(session.fault)}
    {(transportFault(session.fault) || exitFault(session.fault)) && !session.closed && <button disabled={resuming} onClick={() => void resume()}>{resuming ? 'Resuming…' : 'Resume'}</button>}
  </div>;
}

function EmptyConversation({ agentName }: { agentName: string }) {
  return <div className="empty"><span>READY TO WORK</span><h2>What are we building?</h2><p>Send {agentName} a message to get started.</p></div>;
}

/** Questions and approvals the agent waits on, answered in place. */
function PendingRequests({ api, session, sessionID, agentName, onError, enableFullAccess }: { api: RunnerClient; enableFullAccess?(): Promise<boolean>; session?: Session; sessionID: string; agentName: string; onError(message: string): void }) {
  const count = session?.pending.length || 0;
  return <>{session?.pending.map((p, index) => <RequestCard enableFullAccess={enableFullAccess} key={p.requestId} request={p} api={api} session={sessionID} onError={onError} root={session.workingDirectory} agentName={agentName} position={{ index, count }}/>)}</>;
}
