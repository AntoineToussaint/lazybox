import { useMemo, useState, type ReactNode } from 'react';
import type { RunnerClient } from '@lazybox/client-core';
import type { Session } from '@lazybox/protocol';
import type { OpenModifiers } from './file-links';
import type { FileLink } from './file-reference';
import { ConversationLoading, MessageList } from './MessageList';
import { MessageScopeProvider } from './MessageArtifactActions';
import { useFileLinkTracking } from './FileArtifactActions';
import { useRevealRequests } from './artifact-events';
import { PromptNavigator } from './PromptNavigator';
import { RequestCard } from './session-parts';
import { ToolLineContext } from './ToolLine';
import { latestPlanId, type ActivityDetail } from './tool-lines';
import { exitFault, faultSentence, transportFault } from './subscription-model';
import type { useChatScroll } from './useChatScroll';
import type { useTranscript } from './useTranscript';

type ChatScroll = ReturnType<typeof useChatScroll>;
type Transcript = ReturnType<typeof useTranscript>;

/**
 * The conversation pane: the prompt navigator on its edge, the messages and
 * the agent's pending requests (and a subscription limit) in the scrolling pane, and a way back to the
 * latest message once the reader scrolled away from it.
 */
export function MessageArea({ api, session, sessionID, transcript, scroll, spoken, agentName, limited, nameFor, onOpenFile, onError, subscription, detail = 'smart' }: {
  api: RunnerClient;
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
  useRevealRequests(sessionID, !transcript.loading && conversation.length > 0, scroll.revealMessage);
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
          <MessageScopeProvider scope={artifactScope}><ToolLineContext.Provider value={toolScope}><MessageList messages={spoken ? conversation : []} visible={scroll.visibleMessages} agentName={agentName} onOpenFile={onOpenFile} onShowEarlier={scroll.showEarlier} nameFor={nameFor} checkpoints={checkpoints} sessionId={sessionID} onReveal={scroll.revealMessage} detail={detail} loadingEarlier={transcript.partial}/></ToolLineContext.Provider></MessageScopeProvider>
          {session?.fault && !limited && <FaultBanner api={api} session={session} sessionID={sessionID} onError={onError}/>}
          <PendingRequests api={api} session={session} sessionID={sessionID} agentName={agentName} onError={onError}/>
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
function PendingRequests({ api, session, sessionID, agentName, onError }: { api: RunnerClient; session?: Session; sessionID: string; agentName: string; onError(message: string): void }) {
  const count = session?.pending.length || 0;
  return <>{session?.pending.map((p, index) => <RequestCard key={p.requestId} request={p} api={api} session={sessionID} onError={onError} root={session.workingDirectory} agentName={agentName} position={{ index, count }}/>)}</>;
}
