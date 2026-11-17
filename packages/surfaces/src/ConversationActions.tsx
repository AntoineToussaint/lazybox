import { useCallback, useRef, useState, type FormEvent, type KeyboardEvent as ReactKeyboardEvent } from 'react';
import type { Session } from '@lazybox/protocol';
import { useRunner } from './RunnerContext';
import { useRunnerDirectory, useRunnerIdentity } from './RunnerDirectory';
import { useSeen } from './useAttention';
import { conversationKey, deriveAttention, seenAt, seenMark } from './attention';
import { archivedAt, isPinned } from './agents-model';
import { conversationName, conversationTitle } from './conversation-model';
import { conversationOpeners } from './conversation-requests';
import { copyText, showMenuNotice, type MenuItem } from './ContextMenu';
import { openPeek } from './Peek';
import { conversationHandoffItems } from './handoff';
import { DeleteConversationDialog } from './DeleteConversationDialog';
import { ConfirmDialog } from './ConfirmDialog';
import type { ViewTarget } from './workspace-model';

/**
 * A conversation row is the same object in every list (Workspaces, Agents,
 * the Inbox, History): one menu, in one order, from one place. Opening comes
 * first, then naming and marking, then the agent, and the two that take the
 * conversation away come last, apart, each behind a confirmation.
 *
 *   Open · Open in new tab · Open beside · Open below · Open in new window ·
 *   Move to new page · Peek
 *   Close other tabs · Close tabs to the right (when its tab has neighbours)
 *   Rename · Pin · Mark as read
 *   Copy ID · Stop · Open in Terminal · Continue in Claude Code…
 *   Archive… · Delete…
 */
export function useConversationActions({ onOpen, onDeleted }: { onOpen?(target: ViewTarget): void; onDeleted?(session: Session): void } = {}) {
  const { api, info, refresh } = useRunner();
  const { seen, mark } = useSeen();
  const [deleting, setDeleting] = useState<Session>();
  const [archiving, setArchiving] = useState<Session>();
  // In a window showing several Runners, a conversation names its own: opening it shows that Runner's page.
  const { id: runner, multi } = useRunnerIdentity();
  const shownRunner = useRunnerDirectory()?.active;
  const onPage = !multi || runner === shownRunner;
  const target = (session: Session): ViewTarget => ({ collectionId: session.collectionId, sessionId: session.id, commandId: '', ...(multi ? { runner } : {}) });
  const name = (session: Session) => conversationName(session);

  const pin = useCallback(async (session: Session, pinned = !isPinned(session)) => {
    try { await api.pinConversation({ sessionId: session.id, pinned }); refresh(); showMenuNotice(pinned ? `Pinned “${name(session)}”` : `Unpinned “${name(session)}”`); }
    catch (error) { showMenuNotice(`Could not ${pinned ? 'pin' : 'unpin'}: ${String(error)}`); }
  }, [api, refresh]);
  const unarchive = useCallback(async (session: Session) => {
    try { await api.archiveConversation({ sessionId: session.id, archived: false }); refresh(); showMenuNotice(`“${name(session)}” is back in your lists`); }
    catch (error) { showMenuNotice(`Could not unarchive: ${String(error)}`); }
  }, [api, refresh]);
  const attention = (session: Session) => deriveAttention(session, seenAt(seen, conversationKey(session)));
  const unread = (session: Session) => { const row = attention(session); return row.unread && row.state !== 'needs-input'; };
  const markRead = useCallback((session: Session) => { const row = deriveAttention(session, seenAt(seen, conversationKey(session))); mark([{ key: row.key, at: seenMark(row) }]); }, [seen, mark]);
  const stop = useCallback(async (session: Session) => {
    try { await api.apply({ sessionId: session.id, operationId: crypto.randomUUID(), operation: { case: 'interrupt', value: {} } }); refresh(); showMenuNotice(`Stopping “${name(session)}”`); }
    catch (error) { showMenuNotice(`Could not stop the agent: ${String(error)}`); }
  }, [api, refresh]);

  const items = (session: Session, row: { rename?(): void } = {}): MenuItem[] => {
    const openers = conversationOpeners(), here = target(session), pinned = isPinned(session), archived = archivedAt(session) > 0;
    const state = attention(session);
    // Tabs belong to the page of this Runner, in a workspace window.
    const tabs = !openers ? 'Only in a workspace window' : !onPage ? 'Show its Runner first' : false;
    const tab = !tabs ? openers?.tabOf(here) : undefined;
    return [
      onOpen && { label: 'Open', icon: 'chat', shortcut: 'Enter', run: () => onOpen(here) },
      { label: 'Open in new tab', icon: 'plus', disabled: tabs, run: () => openers?.open(here, 'tab') },
      { label: 'Open beside', icon: 'split-right', disabled: tabs, run: () => openers?.beside(here) },
      { label: 'Open below', icon: 'split-down', disabled: tabs, run: () => openers?.below(here) },
      { label: 'Open in new window', icon: 'external', disabled: !onPage ? 'Show its Runner first' : !openers?.window && 'This window cannot open another one', run: () => openers?.window?.(here) },
      { label: 'Move to new page', icon: 'panels', disabled: tabs, run: () => openers?.newPage(here, conversationName(session)) },
      { label: 'Peek', icon: 'eye', shortcut: 'Space', run: () => openPeek({ kind: 'conversation', sessionId: session.id, workspaceId: session.collectionId, title: session.title, ...(multi ? { runner } : {}) }) },
      // Open in a tab with others beside it: close them from here, as from the tab.
      !!tab?.others && 'separator',
      !!tab?.others && { label: 'Close other tabs', icon: 'close', run: () => openers?.closeTabs(here, 'others') },
      !!tab?.others && { label: 'Close tabs to the right', icon: 'close', disabled: !tab.right && 'No tab to its right', run: () => openers?.closeTabs(here, 'right') },
      'separator',
      { label: 'Rename', icon: 'pencil', shortcut: 'F2', disabled: !row.rename && 'Rename it from its header', run: () => row.rename?.() },
      { label: pinned ? 'Unpin' : 'Pin', icon: 'pin', shortcut: 'P', run: () => void pin(session, !pinned) },
      { label: 'Mark as read', icon: 'check', shortcut: 'M', disabled: state.state === 'needs-input' ? 'It waits for your answer' : !state.unread && 'Nothing new to read', run: () => markRead(session) },
      'separator',
      { label: 'Copy ID', icon: 'copy', run: () => void copyText(session.conversationId || session.id, 'Copied conversation ID') },
      { label: 'Stop', icon: 'stop', disabled: (!session.busy || session.closed) && 'The agent is not working', run: () => void stop(session) },
      // Open in Terminal · Continue in Claude Code / Codex… (handoff.tsx).
      ...conversationHandoffItems(api, session, info),
      'separator',
      archived
        ? { label: 'Unarchive', icon: 'archive', run: () => void unarchive(session) }
        : { label: 'Archive…', icon: 'archive', run: () => setArchiving(session) },
      { label: 'Delete…', icon: 'close', danger: true, run: () => setDeleting(session) },
    ];
  };

  const dialogs = <>
    {archiving && <ArchiveConversationDialog session={archiving} onClose={() => setArchiving(undefined)}/>}
    {deleting && <DeleteConversationDialog session={deleting} onClose={() => setDeleting(undefined)} onDeleted={() => { const gone = deleting; setDeleting(undefined); onDeleted?.(gone); }}/>}
  </>;
  return { items, dialogs, pin, markRead, stop, unread, remove: setDeleting, archive: setArchiving };
}

/** Archiving asks first: it is reversible, but the conversation leaves the lists. */
function ArchiveConversationDialog({ session, onClose }: { session: Session; onClose(): void }) {
  const { api, refresh } = useRunner();
  return <ConfirmDialog title="Archive conversation?" label="Archive conversation" closeLabel="Cancel archiving" subject={conversationName(session)}
    kept={session.busy && !session.closed ? 'Its agent keeps working on this turn. If it needs you, it comes back to the list.' : undefined}
    tone="primary" confirm="Archive" working="Archiving…" onClose={onClose}
    onConfirm={async () => { await api.archiveConversation({ sessionId: session.id, archived: true }); refresh(); showMenuNotice(`Archived “${conversationName(session, 'conversation')}” · it is in History`); onClose(); }}>
    It leaves Workspaces and Agents. Nothing is deleted: History keeps it, and Unarchive brings it back.
  </ConfirmDialog>;
}

/**
 * A conversation's title edited where it is listed (F2 or Rename): Enter or
 * leaving the field saves, Escape puts it back. The title helper decides what
 * is shown; this only writes the person's choice.
 */
export function InlineConversationRename({ session, onDone }: { session: Session; onDone(): void }) {
  const { api, refresh } = useRunner();
  const [value, setValue] = useState(() => conversationTitle(session));
  const [busy, setBusy] = useState(false);
  const done = useRef(false);
  const save = async (event?: FormEvent) => {
    event?.preventDefault();
    // Enter saves, and the field leaving the page then blurs it: one save.
    if (done.current) return;
    done.current = true;
    const title = value.trim();
    if (!title || title === conversationTitle(session)) { onDone(); return; }
    setBusy(true);
    try { await api.renameConversation({ sessionId: session.id, title }); refresh(); onDone(); }
    catch (error) { showMenuNotice(`Could not rename: ${String(error)}`); done.current = false; setBusy(false); }
  };
  const key = (event: ReactKeyboardEvent<HTMLInputElement>) => {
    event.stopPropagation();
    if (event.key === 'Escape') { event.preventDefault(); done.current = true; onDone(); }
  };
  return <form className="conversation-inline-rename" onSubmit={save} onClick={event => event.stopPropagation()}>
    <input autoFocus aria-label="Conversation name" maxLength={120} value={value} disabled={busy} placeholder="Name this conversation"
      onFocus={event => event.target.select()} onChange={event => setValue(event.target.value)} onKeyDown={key} onBlur={() => void save()}/>
  </form>;
}
