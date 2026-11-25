import { useEffect, useRef, useState } from 'react';
import type { RunnerClient } from '@lazybox/client-core';
import type { Collection, CommandStatus, Session } from '@lazybox/protocol';
import { continueConversation, type ConversationReader } from './transcript-cache';
import { refreshFailure } from './human-error';

/** The workspace a conversation belongs to, for its branch: the header leads with it until the conversation has a title. */
export function useSessionWorkspace(api: RunnerClient, workspaceId: string, connected: boolean, revision: number) {
  const [workspace, setWorkspace] = useState<Collection>();
  useEffect(() => {
    setWorkspace(undefined);
    if (!workspaceId || !connected) return;
    const control = new AbortController();
    void api.getCollection({ collectionId: workspaceId }, { signal: control.signal }).then(result => { if (!control.signal.aborted) setWorkspace(result.collection); }, () => {});
    return () => control.abort();
  }, [api, workspaceId, connected, revision]);
  return workspace;
}

/** How long one refresh may take: a read that never answers must not stop the polling behind it. */
const REFRESH_TIMEOUT_MS = 10_000;

/**
 * Refreshes the viewed session and its commands while it is shown, and follows
 * a session to its successor execution (unless a send is moving it there).
 * Returns a refresh to call now, after a send: a message queued behind a busy
 * turn shows in the Queued strip without waiting for the next poll.
 */
export function useSessionRefresh({ api, sessionID, connected, showCommands, reader, sending, setSession, setCommands, setRefreshError, onSuccessor }: {
  api: RunnerClient;
  sessionID: string;
  connected: boolean;
  showCommands: boolean;
  reader: ConversationReader;
  sending: { current: boolean };
  setSession(session: Session | undefined): void;
  setCommands(commands: CommandStatus[]): void;
  setRefreshError(error: string): void;
  onSuccessor(session: Session): void;
}): () => void {
  const now = useRef<() => void>(() => {});
  useEffect(() => {
    if (!sessionID || !connected) return;
    const control = new AbortController(); setSession(reader.session); setCommands([]);
    let refreshing = false, again = false, failingSince: number | undefined;
    const options = () => ({ signal: control.signal, timeoutMs: REFRESH_TIMEOUT_MS });
    const update = async () => {
      if (control.signal.aborted) return;
      // Asked while a refresh is out: one more right after it, so what was just sent is in it.
      if (refreshing) { again = true; return; }
      refreshing = true;
      try {
        const [state, output] = await Promise.all([api.getSession({ sessionId: sessionID }, options()), showCommands ? api.listCommands({ sessionId: sessionID }, options()) : Promise.resolve({ commands: [] })]);
        if (!control.signal.aborted) {
          if (state.session?.successorExecutionId && !sending.current) {
            const next = await api.getSession({ sessionId: state.session.successorExecutionId }, options());
            if (!control.signal.aborted && next.session) { continueConversation(api, sessionID, next.session); onSuccessor(next.session); }
          } else { reader.session = state.session; setSession(state.session); setCommands(output.commands); }
          failingSince = undefined; setRefreshError('');
        }
      } catch (e) {
        // A slow or cancelled refresh under load is retried by the next poll: only refreshes that keep failing are shown.
        if (!control.signal.aborted) { const next = refreshFailure(failingSince, Date.now(), e); failingSince = next.failingSince; setRefreshError(next.shown); }
      }
      finally { refreshing = false; if (again) { again = false; void update(); } }
    };
    now.current = () => void update();
    void update(); const timer = setInterval(() => void update(), 1500);
    return () => { control.abort(); clearInterval(timer); now.current = () => {}; };
  }, [api, sessionID, connected, showCommands]);
  return () => now.current();
}
