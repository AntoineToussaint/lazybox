import { AppearanceSync } from './appearance/AppearanceSync';
import { useEffect, useRef, useState } from 'react';
import type { ViewStateStore } from '@lazybox/platform';
import type { Artifact, Collection, Session } from '@lazybox/protocol';
import { ChatPanel } from './AgentWindow';
import { ArtifactPage } from './ArtifactPage';
import { appendToDraft } from './command-runs';
import { conversationName } from './conversation-model';
import { Icon } from './Icons';
import { Resizer } from './Resizer';
import { AttentionScope, PanelShownContext } from './useAttention';
import { SPLITS } from './resizable-model';
import { RunnerProvider, useRunner, type RunnerConnection } from './RunnerContext';
import type { ViewTarget } from './workspace-model';
import './artifacts.css';

type ArtifactView = { collectionId: string; artifactId: string; viewState?: ViewStateStore };

/** A standalone artifact can share the window with a chat from its workspace. */
export function ArtifactWindow({ connection, ...view }: ArtifactView & { connection: RunnerConnection }) {
  return <RunnerProvider connection={connection}><AppearanceSync store={view.viewState}/><div className="artifact-window workspace-shell"><AttentionScope root={view.viewState?.root}><StandalonePage key={view.artifactId} {...view}/></AttentionScope></div></RunnerProvider>;
}

function StandalonePage({ collectionId, artifactId, viewState }: ArtifactView) {
  const { api, info } = useRunner();
  const [collection, setCollection] = useState<Collection>();
  const [history, setHistory] = useState<Session[]>([]);
  const [chatOpen, setChatOpen] = useState(false);
  const [sessionId, setSessionId] = useState<string>();
  const [chatKey, setChatKey] = useState(0);
  const [pending, setPending] = useState<{ text: string }>();
  const lastDiscussed = useRef<{ sessionId: string; reference: string }>(undefined);
  const pane = useRef<HTMLElement>(null);
  const returnTo = useRef<HTMLElement | null>(null);

  useEffect(() => {
    const control = new AbortController();
    void api.getCollection({ collectionId }, { signal: control.signal, timeoutMs: 10000 }).then(result => { if (!control.signal.aborted) setCollection(result.collection); }, () => {});
    return () => control.abort();
  }, [api, collectionId]);
  useEffect(() => {
    if (!chatOpen) return;
    const control = new AbortController();
    void api.listHistory({ collectionId, limit: 30 }, { signal: control.signal, timeoutMs: 10000 }).then(result => {
      if (!control.signal.aborted) setHistory(result.sessions);
    }, () => { /* Live sessions are still available from Runner info. */ });
    return () => control.abort();
  }, [api, chatOpen, collectionId]);
  useEffect(() => { document.title = collection ? `Artifact · ${collection.name}` : 'Artifact'; }, [collection]);

  const sessions = new Map([...history, ...(info?.sessions || [])]
    .filter(session => session.collectionId === collectionId && (!session.purpose || session.purpose === 'coding'))
    .map(session => [session.id, session]));

  const showChat = () => {
    if (!chatOpen) {
      const active = document.activeElement;
      returnTo.current = active instanceof HTMLElement ? active : null;
      setChatOpen(true);
      requestAnimationFrame(() => {
        if (!pane.current?.contains(document.activeElement)) pane.current?.focus();
      });
    }
  };
  const closeChat = () => {
    setChatOpen(false);
    requestAnimationFrame(() => returnTo.current?.focus());
  };
  const followSession = (id: string) => {
    const last = lastDiscussed.current;
    if (last && last.sessionId === sessionId) last.sessionId = id;
    setSessionId(id);
  };
  const pickSession = (id: string) => {
    if (id !== sessionId) setChatKey(key => key + 1);
    setSessionId(id);
  };
  const discuss = (artifact: Artifact, reference: string) => {
    const id = sessionId ?? artifact.originSessionId;
    if (sessionId === undefined) setSessionId(id);
    if (chatOpen || lastDiscussed.current?.sessionId !== id || lastDiscussed.current.reference !== reference) {
      setPending(previous => previous?.text === reference ? previous : { text: appendToDraft(previous?.text || '', reference) });
      lastDiscussed.current = { sessionId: id, reference };
    }
    showChat();
  };
  const showSession = (target: ViewTarget) => {
    pickSession(target.sessionId);
    showChat();
  };

  if (!collectionId || !artifactId) return <p className="module-empty">This window has no artifact to show.</p>;
  return <div className="artifact-chat-layout">
    <div className="artifact-chat-document">
      <ArtifactPage collectionId={collectionId} artifactId={artifactId} collection={collection} standalone
        onDiscuss={discuss} chatOpen={chatOpen} onShowSession={showSession} conversationId={sessionId} onDeleted={() => window.close()}/>
    </div>
    {chatOpen && <Resizer split={SPLITS.artifactChat} label="chat" store={viewState}/>}
    {sessionId !== undefined &&
      <aside ref={pane} tabIndex={-1} hidden={!chatOpen} className="artifact-chat-pane" aria-label="Artifact chat">
        <header className="detached-artifacts-bar">
          <Icon name="chat" size={14}/>
          <select aria-label="Conversation" value={sessionId || ''} onChange={event => pickSession(event.target.value)}>
            <option value="">New conversation</option>
            {!!sessionId && !sessions.has(sessionId) && <option value={sessionId}>Artifact conversation</option>}
            {[...sessions.values()].map(session => <option key={session.id} value={session.id}>{conversationName(session)}</option>)}
          </select>
          <button type="button" className="icon-button" aria-label="Close artifact chat" onClick={closeChat}><Icon name="close" size={14}/></button>
        </header>
        {!sessionId && pending && <p className="artifact-chat-pending">The artifact reference will be added to your draft when you start the chat.</p>}
        <PanelShownContext.Provider value={chatOpen}>
          <ChatPanel key={chatKey} embedded collectionId={collectionId} viewState={viewState}
            initialSessionId={sessionId} onSessionChange={followSession} showCommands={false}
            draftAddition={pending} onDraftAdded={() => setPending(undefined)}/>
        </PanelShownContext.Provider>
      </aside>
    }
  </div>;
}
