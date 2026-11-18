import { useEffect, useRef, useState, type FormEvent } from 'react';
import { Code, ConnectError } from '@lazybox/client-core';
import type { Session } from '@lazybox/protocol';
import { revealMessage } from './artifact-events';
import { archivedAt } from './agents-model';
import { composeInConversation } from './compose';
import { conversationSourceLink } from './conversation-source';
import { quoteText } from './context-menu-model';
import { conversationName, conversationOption, conversationProvider, mergeConversations } from './conversation-model';
import { DialogHeader, DraggableDialog } from './DraggableDialog';
import { humanError } from './human-error';
import { Icon } from './Icons';
import { agentTitle } from './providers';
import { useRunner } from './RunnerContext';
import { showToast } from './toasts';
import type { ViewTarget } from './workspace-model';
import './conversation-handoff.css';

export type HandoffMessage = { id: string; text: string };
export type OpenHandoffConversation = (target: ViewTarget) => boolean;

export function ConversationHandoff({ source, message, open, onClose }: {
  source: Session;
  message: HandoffMessage;
  open: OpenHandoffConversation;
  onClose(): void;
}) {
  const { api, info } = useRunner();
  const dialog = useRef<HTMLDialogElement>(null);
  const [history, setHistory] = useState<Session[]>([]);
  const [loading, setLoading] = useState(true);
  const [targetId, setTargetId] = useState('');
  const [note, setNote] = useState('');
  const [error, setError] = useState('');
  const workspaceId = source.collectionId;
  useEffect(() => { dialog.current?.showModal(); }, []);
  useEffect(() => {
    const controller = new AbortController();
    setLoading(true);
    const load = async () => {
      try {
        const sessions: Session[] = [];
        const limit = 100;
        for (let offset = 0; ; offset += limit) {
          const page = await api.listHistory({ collectionId: workspaceId, limit, offset }, { signal: controller.signal, timeoutMs: 5000 });
          sessions.push(...page.sessions);
          if (!page.hasMore || !page.sessions.length) break;
        }
        if (!controller.signal.aborted) setHistory(sessions);
      } catch (error) {
        if (!controller.signal.aborted && ConnectError.from(error).code !== Code.Unimplemented)
          setError('Could not load saved conversations. ' + humanError(error));
      } finally {
        if (!controller.signal.aborted) setLoading(false);
      }
    };
    void load();
    return () => controller.abort();
  }, [api, workspaceId]);

  const profiles = info?.profiles || [];
  const targets = mergeConversations(history, info?.sessions || []).filter(session =>
    session.collectionId === workspaceId && session.id !== source.id &&
    (!source.conversationId || source.conversationId !== session.conversationId) &&
    (!session.purpose || session.purpose === 'coding') && !archivedAt(session) &&
    (!session.closed || (!session.resumeBlocked && !session.recoveryRequired && !!session.providerReference && !!session.capabilities?.resume)));
  const target = targets.find(session => session.id === targetId);
  const address = (session: Session): ViewTarget => ({ collectionId: workspaceId, sessionId: session.id, commandId: '' });

  const submit = (event: FormEvent) => {
    event.preventDefault();
    if (!target || !workspaceId) return;
    if (!open(address(target))) {
      setError('Could not open that conversation here. If it is in its own window, put it back in the workspace first.');
      return;
    }
    const title = conversationName(source);
    const sourceMessage = { sessionId: source.id, messageId: message.id };
    const link = conversationSourceLink(sourceMessage);
    const text = [note.trim(), 'From “' + title.replace(/\s+/g, ' ') + '” ([source message](' + link + '))', quoteText(message.text).trimEnd()].filter(Boolean).join('\n\n');
    composeInConversation({ sessionId: target.id, text, api });
    onClose();
    showToast({
      tone: 'info', title: 'Handoff ready for review', body: 'Review the draft in “' + conversationName(target) + '” and send it when ready.',
      action: { label: 'Open source message', run: () => { if (open(address(source))) revealMessage(sourceMessage); } },
    });
  };

  return <DraggableDialog ref={dialog} className="conversation-handoff" aria-label="Send to conversation"
    onCancel={event => { event.preventDefault(); onClose(); }}>
    <DialogHeader><h2>Send to conversation</h2><button type="button" aria-label="Close" onClick={onClose}><Icon name="close"/></button></DialogHeader>
    <form onSubmit={submit}>
      <label>Conversation
        <select aria-label="Conversation" data-autofocus value={targetId} onChange={event => setTargetId(event.target.value)} disabled={!targets.length}>
          <option value="">Choose a conversation in this workspace</option>
          {targets.map(session => <option key={session.id} value={session.id}>{conversationOption(
            session, targets,
            (_profileId, value) => agentTitle(conversationProvider(value, profiles), profiles),
            profileId => profiles.find(profile => profile.id === profileId)?.label,
          )}</option>)}
        </select>
      </label>
      {loading && <p role="status">Loading conversations…</p>}
      {!loading && !targets.length && <p role="status">There are no other conversations available in this workspace.</p>}
      <label>Note (optional)<textarea rows={3} value={note} onChange={event => setNote(event.target.value)}/></label>
      <p>The quoted text goes into the destination’s draft for review.</p>
      {error && <p className="error" role="alert">{error}</p>}
      <footer><button type="button" onClick={onClose}>Cancel</button><button type="submit" className="primary" disabled={!target}>Add to draft</button></footer>
    </form>
  </DraggableDialog>;
}
