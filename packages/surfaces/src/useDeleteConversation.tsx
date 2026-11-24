import { useState } from 'react';
import type { Session } from '@lazybox/protocol';
import { conversationName } from './conversation-model';
import { conversationOpeners } from './conversation-requests';
import { humanError } from './human-error';
import { OwnershipDialog } from './OwnershipPanel';
import { announceDeletion } from './PendingDeletions';
import { useRunner } from './RunnerContext';
import { showToast } from './toasts';
import { forgetConversation } from './transcript-cache';

export function useDeleteConversation() {
  const { api, refresh } = useRunner();
  const [recovery, setRecovery] = useState<{session: Session; onDeleted(): void}>();
  const report = (error: unknown) => showToast({ tone: 'failed', title: 'Could not delete conversation', body: humanError(error) });

  const schedule = async (session: Session, onDeleted: () => void) => {
    try {
      // The Runner stops resources only when the five-minute undo period expires.
      const entry = await api.scheduleDeletion({ target: { case: 'conversation', value: { sessionId: session.id, stopSessions: true } } }, { timeoutMs: 120000 });
      announceDeletion(entry, async () => { await api.undoDeletion({ id: entry.id }); refresh(); });
      forgetConversation(api, session.id);
      conversationOpeners()?.forget(session.id);
      refresh();
      onDeleted();
    } catch (error) { report(error); }
  };
  const remove = async (session: Session, onDeleted: () => void) => {
    try {
      const current = await api.getSession({sessionId: session.id});
      if (current.session?.recoveryRequired) setRecovery({session, onDeleted});
      else await schedule(session, onDeleted);
    } catch (error) { report(error); }
  };
  const ownership = recovery && <OwnershipDialog target={{sessionId: recovery.session.id}} title="Unconfirmed conversation processes"
    subject={conversationName(recovery.session)} what="Deleting this conversation waits" onClose={() => setRecovery(undefined)}
    onSettled={() => { setRecovery(undefined); void schedule(recovery.session, recovery.onDeleted); }}/>;
  return { remove, ownership };
}
