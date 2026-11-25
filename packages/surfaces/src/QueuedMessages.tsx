/**
 * Messages waiting behind the turn in progress, above the message box. A
 * server, a watcher or a long build can keep a turn busy for a long time, so a
 * queued message is never stuck there: **Send now**, one click, interrupts the
 * turn (its tooltip names the step it stops) and sends the message as the next turn, and
 * **Steer**, where the agent can take a message mid-turn (Claude, Codex), gives it to
 * the running turn without stopping it. One that cannot (ACP, an older Claude
 * worker): the strip says so and offers Send now. ⌘↵ (chat.send-now) is Send now from the keyboard: the
 * typed message, or with an empty box the oldest queued one.
 */
import { useEffect, useMemo, useRef, useState } from 'react';
import type { RunnerClient } from '@lazybox/client-core';
import type { JournalEvent, Session } from '@lazybox/protocol';
import { Icon } from './Icons';
import { elapsedLabel, toolLabel, type LiveStatus } from './live-status';
import { useNow } from './LiveIndicator';
import { rootStep, stepDetail } from './live-step';
import { messageOrigin, systemEventView } from './system-events';
import type { QueuedEntry } from './transcript-model';
import './queued-messages.css';
import { humanError } from './human-error';

/**
 * The step a Send now would stop, as the strip names it: its command, else its
 * words; and what it is, with when it started, as the live bar says it.
 */
export function stepName(activity: readonly JournalEvent[]): { code?: string; words?: string; label?: string; startedAt?: number } {
  const step = rootStep(activity);
  if (!step) return {};
  const detail = stepDetail(step);
  const label = detail.code ? toolLabel(step.name) : detail.words;
  return { ...(detail.code ? { code: detail.code } : detail.words ? { words: detail.words } : {}), ...(label ? { label } : {}), startedAt: step.startedAt };
}

const clip = (text: string, length = 140) => { const line = text.replace(/\s+/g, ' ').trim(); return line.length > length ? line.slice(0, length - 1) + '…' : line; };

/**
 * Delivering queued messages ahead of their turn, and what the strip shows
 * meanwhile: one state for the strip's buttons and for ⌘↵ in the message box.
 */
type QueueAction = 'now' | 'steer' | 'delete' | 'delegate';
const queueFeedback = {
  now: { pending: 'Stopping the current step…', failure: 'Could not send it now', timeout: 'This message is still queued. You can retry Send now.' },
  steer: { pending: 'Steering…', failure: 'Could not steer', timeout: 'The agent has not taken it yet. Steer again, or Send now.' },
  delete: { pending: 'Deleting…', failure: 'Could not delete', timeout: 'Deletion was not confirmed. Try again.' },
  delegate: { pending: 'Requesting a sub-agent…', failure: 'Could not request a sub-agent', timeout: 'Delegation was not confirmed. Check the conversation.' },
} satisfies Record<QueueAction, { pending: string; failure: string; timeout: string }>;
export type QueuedDelivery = {
  edit(operationId: string, how: EditAction, editId: string, text?: string): Promise<string>;
  working?: { id: string; how: QueueAction; acknowledged?: boolean };
  error?: { id: string; text: string };
  /**
   * Delivers the queued message with this operation id. `justSent`: the
   * message was sent a moment ago, so one the Runner already took (the turn
   * ended meanwhile) went out as it should, and is no error.
   */
  deliver(operationId: string, how: QueueAction, justSent?: boolean): Promise<void>;
};
/** How long Send now or Steer may say it is at work before the strip gives the buttons back to try again. */
export const DELIVERY_TIMEOUT_MS = 20_000;
export function useQueuedDelivery(api: RunnerClient, sessionID: string, queued: readonly { operationId: string; editing?: boolean }[] | undefined, turnBoundary?: bigint): QueuedDelivery {
  // An interrupt belongs to the turn it targeted. Its message may still be in
  // the queue when a different message starts the next turn.
  const [attempt, setWorking] = useState<(NonNullable<QueuedDelivery['working']> & { turn?: bigint })>();
  const working = attempt?.how === 'now' && attempt.turn !== turnBoundary ? undefined : attempt;
  const [failure, setError] = useState<{ id: string; text: string; how: QueueAction; turn?: bigint }>();
  const error = failure?.how === 'now' && failure.turn !== turnBoundary ? undefined : failure;
  // A message that left the queue takes its confirmation, progress and error with it.
  const ids = (queued || []).map(message => message.operationId).join('\0');
  useEffect(() => {
    const present = new Set(ids.split('\0'));
    if (working && !present.has(working.id)) setWorking(undefined);
    if (error && !present.has(error.id)) setError(undefined);
  }, [ids]);
  // Editing supersedes a Send now or Steer still at work on the message: the
  // Runner holds it, so it stays queued, and the timeout below must not call
  // that a step that would not stop. Edit in this window says so at once
  // (`edit` below); a poll says so for an edit from another window.
  const editing = (queued || []).flatMap(message => message.editing ? [message.operationId] : []).join('\0');
  const superseded = (id: string) => {
    setWorking(current => current?.id === id ? undefined : current);
    setError(current => current?.id === id ? undefined : current);
  };
  useEffect(() => { for (const id of editing ? editing.split('\0') : []) superseded(id); }, [editing]);
  useEffect(() => { setWorking(undefined); setError(undefined); }, [sessionID]);
  // An interrupt that never takes (the agent ignores it, the Runner restarted
  // meanwhile) must not leave "Stopping the current step…" for good: the
  // buttons come back, saying so, to try again.
  useEffect(() => {
    if (!working) return;
    const timer = setTimeout(() => {
      setWorking(current => current === working ? undefined : current);
      setError({ id: working.id, how: working.how, turn: working.turn, text: queueFeedback[working.how].timeout });
    }, DELIVERY_TIMEOUT_MS);
    return () => clearTimeout(timer);
  }, [working]);
  const deliver = async (operationId: string, how: QueueAction, justSent = false) => {
    const attempt = { id: operationId, how, turn: turnBoundary };
    setError(undefined); setWorking(attempt);
    try {
      await api.apply({ sessionId: sessionID, operationId: crypto.randomUUID(), operation: { case: 'deliverQueued', value: { operationId, how } } }, { timeoutMs: DELIVERY_TIMEOUT_MS });
      setWorking(current => current === attempt ? { ...attempt, acknowledged: true } : current);
    } catch (e) {
      setWorking(current => current === attempt ? undefined : current);
      const why = humanError(e);
      if (justSent && /no longer queued/.test(why)) return;
      setError({ id: operationId, how, turn: turnBoundary, text: `${queueFeedback[how].failure}: ${why}` });
    }
  };
  const edit: QueuedDelivery['edit'] = async (operationId, how, editId, text = '') => {
    const receipt = await api.apply({ sessionId: sessionID, operationId: crypto.randomUUID(), operation: { case: 'deliverQueued', value: { operationId, how, editId, text } } }, { timeoutMs: DELIVERY_TIMEOUT_MS });
    if (how === 'edit') superseded(operationId);
    return receipt.queuedText;
  };
  return { working, error, deliver, edit };
}

type EditAction = 'edit' | 'save_edit' | 'cancel_edit';

/**
 * Editing one queued message: **Edit** pauses its delivery before the editor
 * opens, **Save** releases the revised text and **Cancel edit** the original.
 * The button sits in the message's action row, before Send now; the editor
 * opens below the row, which it hides while open (queued-messages.css).
 */
function useQueueEdit(message: QueuedEntry, conversationId: string, edit: QueuedDelivery['edit']) {
  const key = `lazybox:queue-edit:${conversationId}:${message.operationId}`;
  const [draft, setDraft] = useState<string>();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const mounted = useRef(false);
  const activeEdit = useRef('');
  const release = useRef<() => void>(undefined);
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; release.current?.(); }; }, []);
  // A browser lock lets another window recover this draft after the editor closes.
  // Runners also check the edit ID, including requests from other devices.
  const storage = () => navigator.locks ? localStorage : sessionStorage;
  const act = async (how: EditAction) => {
    setBusy(true);
    setError('');
    try {
      if (how === 'edit') {
        activeEdit.current = storage().getItem(key) || crypto.randomUUID();
        storage().setItem(key, activeEdit.current);
      }
      const original = await edit(message.operationId, how, activeEdit.current, draft);
      if (how === 'edit') setDraft(storage().getItem(key + ':draft') ?? original);
      else {
        storage().removeItem(key + ':draft');
        storage().removeItem(key);
        setDraft(undefined);
        release.current?.();
      }
    } catch (e) {
      setError(humanError(e));
      // A lost reply may mean Save already released the message. Reacquire
      // before allowing more typing. The draft stays available for the retry.
      setDraft(undefined);
      release.current?.();
    }
    finally { setBusy(false); }
  };
  const begin = () => {
    if (!navigator.locks) return act('edit');
    return navigator.locks.request(key, { ifAvailable: true }, async lock => {
      if (!mounted.current) return;
      if (!lock) { setError('This message is being edited in another window.'); return; }
      const closed = new Promise<void>(resolve => { release.current = resolve; });
      await act('edit');
      await closed;
      release.current = undefined;
    }).catch(e => setError(humanError(e)));
  };
  const change = (text: string) => {
    setDraft(text);
    try { storage().setItem(key + ':draft', text); } catch { /* The open editor keeps the draft when storage is full. */ }
  };
  // Edit (Resume edit for a message already paused) goes in the action row; the open editor has Save and Cancel edit instead.
  const button = draft === undefined && <button type="button" disabled={busy} onClick={() => void begin()}>{message.editing ? 'Resume edit' : 'Edit'}</button>;
  const editor = (draft !== undefined || error) && <div className="queued-edit">
    {draft !== undefined && <>
      <label>Editing queued message · delivery paused<textarea autoFocus value={draft} disabled={busy} onChange={event => change(event.currentTarget.value)}/></label>
      <button type="button" disabled={busy} onClick={() => void act('save_edit')}>Save</button>
      <button type="button" disabled={busy} onClick={() => void act('cancel_edit')}>Cancel edit</button>
    </>}
    {error && <span role="alert" className="queued-error">{error}</span>}
  </div>;
  return { button, editor };
}

/** One queued message: its text, then its actions in one row, in reading and Tab order: Steer, Request delegation, Edit, Send now, Delete. */
function QueuedItem({ message, index, session, busy, steer, agentName, stops, sendNowKey, delivery }: {
  message: QueuedEntry;
  index: number;
  session: Session;
  busy: boolean;
  steer: boolean;
  agentName: string;
  /** What Send now stops, for its tooltip. */
  stops: string;
  sendNowKey?: string;
  delivery: QueuedDelivery;
}) {
  const { working, error } = delivery;
  const mine = working?.id === message.operationId;
  const deliver = (how: QueueAction) => delivery.deliver(message.operationId, how);
  // A message Lazybox wrote says what it is, not the words it carries, and is not edited.
  const origin = messageOrigin(message.origin), event = origin && systemEventView(origin, agentName);
  const editing = useQueueEdit(message, session.conversationId || session.id, delivery.edit);
  const edit = !origin && editing.button;
  return <li className="queued-message" data-origin={origin?.kind} aria-label={event ? `Queued from Lazybox: ${event.line}` : `Queued message: ${clip(message.text, 80)}`}>
    <div className="queued-row">
      {event ? <span className="queued-text queued-event" title={message.text}><Icon name={event.icon} size={11}/><span>{clip(event.line)}</span>{event.detail && <small>{event.detail}</small>}</span>
        : <q className="queued-text" title={message.text}>{clip(message.text)}</q>}
      <span className="queued-actions">
        {/* Edit stays offered while a delivery is at work, as it was before it joined this row. */}
        {mine ? <><span className="queued-working" role="status">{working.how === 'now' && working.acknowledged ? 'Waiting for the current turn to end…' : queueFeedback[working.how].pending}</span>{edit}</> : <>
          {busy && !message.editing && steer && <button type="button" onClick={() => void deliver('steer')}
            title={`Give ${agentName} this message now, inside the running turn, without stopping it`}><Icon name="send" size={11}/>Steer</button>}
          {busy && !message.editing && steer && session.capabilities?.subagents && !origin && <button type="button" onClick={() => void deliver('delegate')}
            title={`Ask ${agentName} to delegate this message to a sub-agent and continue the current task`}>Request delegation</button>}
          {edit}
          {busy && !message.editing && <button type="button" onClick={() => void deliver('now')}
            title={`${stops} and send this as the next turn${index === 0 && sendNowKey ? ` (${sendNowKey})` : ''}`}><Icon name="stop" size={11}/>Send now</button>}
          <button type="button" onClick={() => void deliver('delete')}
            title="Delete this queued message">Delete</button>
        </>}
      </span>
    </div>
    {!origin && editing.editor}
    {error?.id === message.operationId && <p className="queued-error" role="alert">{error.text}</p>}
  </li>;
}

/** The event stream can announce a running turn before the next session poll. */
export function queueTurnRunning(session: Session | undefined, turnRunning?: boolean): boolean {
  return !!session && !session.closed && (session.busy || !!turnRunning);
}

export function QueuedMessages({ session, queued, turnRunning, live, agentName, activity, delivery, sendNowKey }: {
  session?: Session;
  /** What waits, as `queuedMessages` (transcript-model.ts) merges it: at once, and in one place only. */
  queued: readonly QueuedEntry[];
  /** The transcript says a turn runs, before a poll of the session does. */
  turnRunning?: boolean;
  live: LiveStatus;
  agentName: string;
  /** The conversation's activity, to name the step Send now stops. */
  activity: readonly JournalEvent[];
  delivery: QueuedDelivery;
  /** Send now's key as it reads (⌘↵), when it has one: it sends the oldest queued message. */
  sendNowKey?: string;
}) {
  const busy = queueTurnRunning(session, turnRunning);
  const [collapsed, setCollapsed] = useState(false);
  const [showAll, setShowAll] = useState(false);
  const step = useMemo(() => busy ? stepName(activity) : {}, [busy, activity]);
  const now = useNow(busy && queued.length > 0);
  if (!session || session.closed || !queued.length) return null;

  const steer = !!session?.capabilities?.steer;
  const many = queued.length > 1;
  const shown = collapsed ? [] : !showAll && queued.length > 2 ? queued.slice(0, 2) : queued;
  // What the agent is busy with, as the live bar says it: the step and its time, else the status line.
  const stops = step.code ? `Stop ${step.code}` : step.words ? `Stop the running step (${step.words.charAt(0).toLowerCase() + step.words.slice(1)})` : `Stop what ${agentName} is doing`;
  // The time ticks every second: it is not part of what the status line announces, or a screen reader repeats the line each second.
  const busyWith = step.label || (live.tone === 'busy' || live.tone === 'waiting' ? live.text : 'working');
  const busyFor = step.label && step.startedAt !== undefined && now >= step.startedAt ? ` · ${elapsedLabel(now - step.startedAt)}` : '';

  return <section className="queued-messages" aria-label="Queued messages">
    <header className="queued-head">
      <Icon name="pending" size={12}/>
      <span role="status"><strong>{many ? `${queued.length} messages queued` : 'Queued'}</strong>{busy && <> — {agentName} is busy ({busyWith}{busyFor && <span aria-hidden="true">{busyFor}</span>})</>}</span>
      <button type="button" className="queued-toggle" aria-expanded={!collapsed} onClick={() => setCollapsed(value => !value)}
        title={collapsed ? 'Show the queued messages' : 'Hide them here; they still go after the current turn'}>
        <Icon name={collapsed ? 'expand' : 'collapse'} size={11}/><span>{collapsed ? 'Show' : 'Hide'}</span>
      </button>
    </header>
    {shown.length > 0 && <ul className="queued-list">
      {shown.map((message, index) => <QueuedItem key={session.id + message.operationId} message={message} index={index} session={session} busy={busy}
        steer={steer} agentName={agentName} stops={stops} sendNowKey={sendNowKey} delivery={delivery}/>)}
    </ul>}
    {!collapsed && queued.length > shown.length && <button type="button" className="queued-more" onClick={() => setShowAll(true)}>Show {queued.length - shown.length} more</button>}
    {!collapsed && busy && !steer && <p className="queued-note">{agentName} can’t take a message in the middle of a turn: <strong>Send now</strong>{sendNowKey ? ` (${sendNowKey})` : ''} stops its current step first, then sends yours.</p>}
  </section>;
}
