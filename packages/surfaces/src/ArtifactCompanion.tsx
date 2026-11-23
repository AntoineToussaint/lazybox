import { useContext, useEffect, useMemo, useRef, useState, type ComponentProps } from 'react';
import type { ViewStateStore } from '@lazybox/platform';
import type { Artifact, Session } from '@lazybox/protocol';
import { ChatPanel } from './AgentWindow';
import { ArtifactPage } from './ArtifactPage';
import { appendToDraft } from './command-runs';
import { conversationName } from './conversation-model';
import { Icon } from './Icons';
import { Resizer } from './Resizer';
import { PanelShownContext } from './useAttention';
import { SPLITS } from './resizable-model';
import { useRunner } from './RunnerContext';
import type { ViewTarget } from './workspace-model';

type Props = Omit<ComponentProps<typeof ArtifactPage>, 'onDiscuss' | 'chatOpen'> & { viewState?: ViewStateStore };

/** The same version-aware conversation beside a docked or detached artifact. */
export function ArtifactCompanion({ collectionId, artifactId, viewState, ...page }: Props) {
  const { api, info } = useRunner();
  const shown = useContext(PanelShownContext);
  const archived = page.collection?.state === 'deleted';
  // A workspace may already have a composer for this conversation. Give the
  // companion its own draft so typing in either surface preserves both.
  const companionStore = useMemo(() => page.standalone || !viewState ? viewState : {
    ...viewState, key: viewState.key + ':artifact-companion:' + collectionId + '/' + artifactId,
  }, [page.standalone, viewState, collectionId, artifactId]);
  const [history, setHistory] = useState<Session[]>([]);
  const [chatOpen, setChatOpen] = useState(false);
  const [sessionId, setSessionId] = useState<string>();
  const [requested, setRequested] = useState('');
  const [chatKey, setChatKey] = useState(0);
  const [pending, setPending] = useState<{ text: string }>();
  const lastDiscussed = useRef<{ sessionId: string; reference: string }>(undefined);
  const pane = useRef<HTMLElement>(null);
  const documentPane = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!chatOpen) return;
    const control = new AbortController();
    void api.listHistory({ collectionId, limit: 30 }, { signal: control.signal, timeoutMs: 10000 }).then(result => {
      if (!control.signal.aborted) setHistory(result.sessions);
    }, () => { /* Live sessions are still available from Runner info. */ });
    return () => control.abort();
  }, [api, chatOpen, collectionId]);
  const sessions = new Map([...history, ...(info?.sessions || [])]
    .filter(session => session.collectionId === collectionId && (!session.purpose || session.purpose === 'coding'))
    .map(session => [session.id, session]));

  // Origin sessions can outlive their workspace, or fall outside the first
  // history page. Resolve them before mounting a composer for this artifact.
  useEffect(() => {
    if (!requested) return;
    const control = new AbortController();
    const adopt = (session?: Session) => {
      if (control.signal.aborted) return;
      const id = session?.collectionId === collectionId && (!session.purpose || session.purpose === 'coding') ? session.id : '';
      if (lastDiscussed.current?.sessionId === requested) lastDiscussed.current.sessionId = id;
      setSessionId(id);
      setRequested('');
    };
    void api.getSession({ sessionId: requested }, { signal: control.signal, timeoutMs: 10000 }).then(result => adopt(result.session), () => adopt());
    return () => control.abort();
  }, [api, collectionId, requested]);
  const resolveSession = (id: string) => {
    if (!id || sessions.has(id)) setSessionId(id);
    else setRequested(id);
  };
  const showChat = () => {
    if (!chatOpen) {
      setChatOpen(true);
      requestAnimationFrame(() => {
        if (!pane.current?.contains(document.activeElement)) pane.current?.focus();
      });
    }
  };
  const closeChat = () => {
    setChatOpen(false);
    requestAnimationFrame(() => documentPane.current?.querySelector<HTMLButtonElement>('.artifact-discuss')?.focus());
  };
  const rememberSession = (id: string) => {
    try { companionStore?.setItem(companionStore.key + ':conversation', id); } catch { /* the conversation stays open */ }
    setSessionId(id);
  };
  const followSession = (id: string) => {
    const last = lastDiscussed.current;
    if (last && last.sessionId === sessionId) last.sessionId = id;
    rememberSession(id);
  };
  const pickSession = (id: string) => {
    if (id !== sessionId) setChatKey(key => key + 1);
    rememberSession(id);
  };
  const discuss = (artifact: Artifact, reference: string) => {
    let id = sessionId;
    if (id === undefined) {
      id = artifact.originSessionId;
      try { id = companionStore?.getItem(companionStore.key + ':conversation') ?? id; } catch { /* session storage is optional */ }
      resolveSession(id);
    }
    if (chatOpen || lastDiscussed.current?.sessionId !== id || lastDiscussed.current.reference !== reference) {
      setPending(previous => previous?.text === reference ? previous : { text: appendToDraft(previous?.text || '', reference) });
      lastDiscussed.current = { sessionId: id, reference };
    }
    showChat();
  };
  const showSession = (target: ViewTarget) => {
    if (target.collectionId !== collectionId) return;
    if (target.sessionId !== sessionId) setChatKey(key => key + 1);
    resolveSession(target.sessionId);
    showChat();
  };

  return <div className="artifact-chat-host"><div className="artifact-chat-layout">
    <div ref={documentPane} className="artifact-chat-document">
      <ArtifactPage {...page} collectionId={collectionId} artifactId={artifactId}
        onDiscuss={archived ? undefined : discuss} chatOpen={chatOpen} onShowSession={page.onShowSession ?? showSession}
        conversationId={archived ? undefined : sessionId ?? page.conversationId}/>
    </div>
    {chatOpen && <Resizer split={SPLITS.artifactChat} label="chat" store={viewState}/>}
    {(sessionId !== undefined || !!requested) &&
      <aside ref={pane} tabIndex={-1} hidden={!chatOpen} className="artifact-chat-pane" aria-label="Agent companion">
        <header className="detached-artifacts-bar">
          <strong>Agent companion</strong>
          <select aria-label="Conversation" disabled={!!requested} value={sessionId || ''} onChange={event => pickSession(event.target.value)}>
            <option value="">New conversation</option>
            {!!sessionId && !sessions.has(sessionId) && <option value={sessionId}>Artifact conversation</option>}
            {[...sessions.values()].map(session => <option key={session.id} value={session.id}>{conversationName(session)}</option>)}
          </select>
          <button type="button" className="icon-button" aria-label="Close agent companion" onClick={closeChat}><Icon name="close" size={14}/></button>
        </header>
        {requested && <p className="artifact-chat-pending" role="status">Opening conversation…</p>}
        {!requested && !sessionId && pending && <p className="artifact-chat-pending">The artifact reference will be added to your draft when you start the chat.</p>}
        {!requested && <PanelShownContext.Provider value={shown && chatOpen}>
          <ChatPanel key={chatKey} embedded collectionId={collectionId} viewState={companionStore}
            initialSessionId={sessionId} onSessionChange={followSession} showCommands={false}
            draftAddition={pending} onDraftAdded={() => setPending(undefined)}/>
        </PanelShownContext.Provider>}
      </aside>
    }
  </div></div>;
}
