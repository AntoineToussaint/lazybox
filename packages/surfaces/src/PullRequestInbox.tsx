import { useEffect, useMemo, useState } from 'react';
import type { Collection } from '@lazybox/protocol';
import { Icon } from './Icons';
import { useRunner } from './RunnerContext';
import { agentName, pullInboxRows, type PullInboxRow } from './pull-requests-model';
import { PullStatusIcon, reactionLabel } from './PullRequestsPanel';
import { useDismissedPulls, usePullActions, useTrackedItems, type PullAction } from './useTrackedPulls';
import type { ViewTarget } from './workspace-model';
import './pull-requests.css';

/** How many tracked pull requests the Inbox shows: they count with what needs the person. */
export function usePullInboxCount(): number {
  const tracked = useTrackedItems();
  const { dismissed } = useDismissedPulls();
  return useMemo(() => pullInboxRows(tracked.links, dismissed).length, [tracked.links, dismissed]);
}

/** A phone defers a write a few seconds so it can be undone (remote/PhoneSheets.tsx). */
export type DeferredAction = { id: string; label: string; waiting: string; commit(): Promise<void> };

/**
 * The Inbox's "Pull requests" group: each tracked pull request that needs the
 * person — checks failed, changes requested, conflicts, ready to merge — with
 * its reaction prepared: Send the failures to the agent, Address the review,
 * Merge (desktop only, confirmed), Open on GitHub, Dismiss. A dismissed row
 * comes back when something new happens to it (another head, another review).
 */
export function PullRequestInboxSection({ collections, phone = false, canRespond = true, deferredStates, defer, onOpen }: {
  collections: readonly Collection[];
  phone?: boolean;
  /** A phone without the respond grant only reads. */
  canRespond?: boolean;
  deferredStates?: Readonly<Record<string, { phase: string; text: string }>>;
  /** A phone's Undo window; the desktop sends at once. */
  defer?(action: DeferredAction): void;
  onOpen(target: ViewTarget): void;
}) {
  const tracked = useTrackedItems();
  const { dismissed, dismiss, completed } = useDismissedPulls();
  const rows = useMemo(() => {
    const hidden = new Set(dismissed);
    for (const key of completed.keys()) hidden.delete(key);
    return pullInboxRows(tracked.links, hidden);
  }, [tracked.links, dismissed, completed]);
  if (!rows.length) return null;
  const Heading = phone ? 'h2' : 'h3';
  return <section className={`${phone ? 'remote-group' : 'inbox-group'} group-pulls`} aria-labelledby="inbox-group-pulls">
    <Heading id="inbox-group-pulls"><Icon name="pullRequest" size={13}/>Pull requests<span>{rows.length}</span></Heading>
    <ul>{rows.map(row => <PullInboxItem key={row.dismissKey} row={row} pending={deferredStates?.[`${row.dismissKey}:${row.action}`]} phone={phone} canRespond={canRespond} defer={defer} onOpen={onOpen} receipt={completed.get(row.dismissKey)} onDismiss={() => dismiss(row.dismissKey)}
      workspace={collections.find(c => c.id === row.link.collectionId)?.name || row.link.collectionName}/>)}</ul>
  </section>;
}

function PullInboxItem({ row, workspace, phone, canRespond, defer, onOpen, onDismiss, receipt, pending }: { pending?: { phase: string; text: string }; receipt?: { text: string; until: number }; row: PullInboxRow; workspace: string; phone: boolean; canRespond: boolean; defer?(action: DeferredAction): void; onOpen(target: ViewTarget): void; onDismiss(): void }) {
  const { info } = useRunner();
  const actions = usePullActions();
  const [confirmMerge, setConfirmMerge] = useState(false);
  const state = actions.states.get(row.dismissKey);
  const error = state?.phase === 'error' && state.text;
  const [now, setNow] = useState(Date.now);
  useEffect(() => {
    if (!receipt) return;
    setNow(Date.now());
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, [receipt]);
  const agent = agentName(info?.sessions.find(s => s.id === row.link.sessionId)?.provider);
  const busy = state?.phase === 'sending';
  const run = async (action: PullAction) => {
    setConfirmMerge(false);
    if (defer && action !== 'merge') {
      defer({ id: `${row.dismissKey}:${action}`, label: `${reactionLabel(action, agent)} · #${row.number}`, waiting: action === 'send' ? 'Sending the failures…' : 'Sending the review…',
        commit: async () => { const result = await actions.run(row, action); if (!result.ok) throw new Error(result.text); } });
      return;
    }
    await actions.run(row, action);
  };
  const reaction: PullAction | undefined = row.action === 'send' || row.action === 'review' ? row.action : row.action === 'merge' && !phone ? 'merge' : undefined;
  const failing = row.link.item?.pullRequest?.failingChecks?.[0]?.url;
  const openLabel = row.status === 'checks-failed' ? 'Open checks' : row.status === 'changes-requested' ? 'Open review' : 'Open';
  const openUrl = row.status === 'checks-failed' && failing ? failing : row.status === 'changes-requested' ? `${row.url}/files` : row.url;
  return <li className="inbox-item pr-inbox-item" data-pull={row.key} data-status={row.status}>
    <button type="button" className="pr-inbox-head inbox-row" onClick={() => onOpen({ collectionId: row.link.collectionId, sessionId: row.link.sessionId, commandId: '' })} title="Open the workspace at the conversation that delivered it">
      <PullStatusIcon pull={row}/>
      <span className="pr-inbox-text">
        <strong>{row.line}</strong>
        <small>{[workspace, row.title].filter(Boolean).join(' · ')}</small>
      </span>
    </button>
    <div className="pr-inbox-actions">
      {reaction && canRespond && !receipt && !pending && !confirmMerge && <button type="button" className="primary" disabled={busy} onClick={() => reaction === 'merge' ? setConfirmMerge(true) : void run(reaction)}>
        <Icon name={reaction === 'merge' ? 'merge' : reaction === 'review' ? 'review' : 'send'} size={13}/>{reaction === 'merge' ? 'Merge…' : reactionLabel(reaction, agent)}
      </button>}
      {confirmMerge && !receipt && !busy && <span role="group" aria-label="Confirm merge" className="pr-inbox-confirm">
        <button type="button" className="primary" autoFocus disabled={busy} onClick={() => void run('merge')}>Merge #{row.number}</button>
        <button type="button" onClick={() => setConfirmMerge(false)}>Cancel</button>
      </span>}
      {openUrl && <a href={openUrl} target="_blank" rel="noreferrer">{openLabel}<Icon name="external" size={11}/></a>}
      <button type="button" className="pr-dismiss" onClick={onDismiss} aria-label={`Dismiss ${row.repository}#${row.number}`}>Dismiss</button>
    </div>
    {!receipt && (busy || pending) && <p className="pr-inbox-note" role="status">{pending?.text || 'Working…'}</p>}
    {receipt && <p className="pr-inbox-note"><span role="status">{receipt.text}</span> · Auto-dismiss in {Math.max(0, Math.min(10, Math.ceil((receipt.until - now) / 1000)))}s</p>}
    {!receipt && !busy && error && <p className="pr-inbox-note is-error" role="alert">{error}</p>}
  </li>;
}
