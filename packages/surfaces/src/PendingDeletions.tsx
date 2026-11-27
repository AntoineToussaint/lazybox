import { Code, ConnectError } from '@lazybox/client-core';
import type { PendingDeletion } from '@lazybox/protocol';
import { useEffect, useState } from 'react';
import { Icon } from './Icons';
import { useRunner } from './RunnerContext';
import { dismissToast, showToast } from './toasts';

export function announceDeletion(entry: PendingDeletion, undo: () => Promise<void>) {
  showToast({
    id: entry.id,
    tone: 'info',
    title: entry.sessionId ? 'Conversation moved to Recently deleted' : 'Workspace moved to Recently deleted',
    body: 'Undo here or from Recently deleted within five minutes.',
    timeout: Math.min(5_000, Math.max(1, Number(entry.expiresAtUnixMillis) - Date.now())),
    button: { label: 'Undo', run: () => void undo().catch(reportUndoFailure) },
  });
}

function reportUndoFailure(error: unknown) {
  showToast({ tone: 'failed', title: 'Could not undo deletion', body: ConnectError.from(error).rawMessage });
}

export function PendingDeletions() {
  const { api, refresh, revision } = useRunner();
  const [entries, setEntries] = useState<PendingDeletion[]>([]);
  const [undoing, setUndoing] = useState('');
  const [now, setNow] = useState(Date.now);
  useEffect(() => {
    const controller = new AbortController();
    let enabled = true;
    let requesting = false;
    const load = async () => {
      if (!enabled || requesting) return;
      requesting = true;
      try {
        const response = await api.listPendingDeletions({}, { signal: controller.signal, timeoutMs: 5000 });
        if (!controller.signal.aborted) setEntries(response.deletions);
      } catch (error) {
        const code = ConnectError.from(error).code;
        if (code === Code.Unimplemented || code === Code.PermissionDenied) enabled = false;
      } finally {
        requesting = false;
        if (!controller.signal.aborted) setNow(Date.now());
      }
    };
    void load();
    const timer = window.setInterval(load, 5000);
    return () => { controller.abort(); window.clearInterval(timer); };
  }, [api, revision]);
  if (!entries.length) return null;
  const undo = async (entry: PendingDeletion) => {
    const { id } = entry;
    setUndoing(id);
    try {
      const response = await api.undoDeletion({ id });
      setEntries(response.deletions);
      dismissToast(id);
      refresh();
    } catch (error) {
      if (entry.failure) showToast({ tone: 'failed', title: 'Could not dismiss deletion failure', body: ConnectError.from(error).rawMessage });
      else reportUndoFailure(error);
    } finally {
      setUndoing('');
    }
  };
  return <section className="recently-deleted" aria-label="Recently deleted">
    <details>
      <summary><Icon name="archive" size={14}/><span>Recently deleted ({entries.length})</span></summary>
      <ul>{entries.map(entry => {
        const available = Number(entry.expiresAtUnixMillis) > now;
        const kind = entry.sessionId ? 'Conversation' : 'Workspace';
        const title = entry.sessionId ? entry.title : entry.workspaceName || entry.title || entry.workspaceId;
        const context = [kind, entry.sessionId && entry.workspaceName, ...entry.repositoryNames].filter(Boolean).join(' · ');
        return <li key={entry.id}>
          <div className="deleted-item"><strong title={title || kind}>{title || kind}</strong><small title={context}>{context}</small></div>
          {entry.failure ? <>
            <small role="status">Deletion failed. {entry.failure}</small>
            <button type="button" disabled={!!undoing} onClick={() => void undo(entry)} aria-label={`Dismiss deletion failure for ${title || kind}`}>Dismiss</button>
          </> : available
            ? <button type="button" disabled={!!undoing} onClick={() => void undo(entry)} aria-label={`Undo deletion of ${title || kind}`}>Undo</button>
            : <small>Finishing deletion…</small>}
        </li>;
      })}</ul>
    </details>
  </section>;
}
