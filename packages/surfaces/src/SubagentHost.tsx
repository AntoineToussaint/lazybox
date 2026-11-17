import { useLayoutEffect, useState } from 'react';
import { Code, ConnectError, type RunnerClient } from '@lazybox/client-core';
import type { Session } from '@lazybox/protocol';
import type { OpenModifiers } from './file-links';
import type { FileLink } from './file-reference';
import type { Subagents } from './subagent-model';
import { SubagentView } from './subagent-view';

/**
 * One chat's sub-agent presentation, kept current with its transcript and
 * session. The store is stable for the chat's life, so providing it re-renders
 * nothing; cards and the tray subscribe to what they show.
 */
export function useSubagentHost({ api, sessionID, session, index, revealMessage, openFile, onError }: {
  api: RunnerClient; sessionID: string; session?: Session; index: Subagents;
  revealMessage(messageId: string): void;
  openFile?(reference: FileLink, how: OpenModifiers): void;
  onError(message: string): void;
}): SubagentView {
  const [view] = useState(() => new SubagentView());
  // Read during this render by the cards it mounts; announced once committed.
  // What was cleared and seen belongs to the conversation, which a resume or
  // a restart (a new execution, a new sessionID) continues.
  const conversation = session?.conversationId || sessionID;
  view.setConversation(conversation);
  view.setIndex(index);
  view.actions = {
    revealMessage, openFile,
    stop: async agentId => {
      try { await api.apply({ sessionId: sessionID, operationId: crypto.randomUUID(), operation: { case: 'stopAgent', value: { agentId } } }); return 'stopping'; }
      catch (error) {
        const failure = ConnectError.from(error);
        // "sub-agent is not running": the Runner has nothing to stop. Not an error to
        // the person: the card says it ended, quietly, where they are looking.
        if (failure.code === Code.FailedPrecondition) return 'gone';
        onError(`Could not stop the sub-agent: ${failure.rawMessage || 'the Runner refused.'}`);
        return 'failed';
      }
    },
  };
  useLayoutEffect(() => { view.notify(); }, [view, index]);
  useLayoutEffect(() => { if (view.setSession(session)) view.notify(); }, [view, session]);
  // A different conversation starts with its cards closed and no panel.
  useLayoutEffect(() => { view.expanded.clear(); view.results.clear(); view.panel = null; view.notify(); }, [view, conversation]);
  return view;
}
