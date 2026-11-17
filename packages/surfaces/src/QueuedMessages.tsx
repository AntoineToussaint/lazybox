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
import { useEffect, useMemo, useState } from 'react';
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
export type QueuedDelivery = {
  working?: { id: string; how: 'now' | 'steer' };
  error?: { id: string; text: string };
  /**
   * Delivers the queued message with this operation id. `justSent`: the
   * message was sent a moment ago, so one the Runner already took (the turn
   * ended meanwhile) went out as it should, and is no error.
   */
  deliver(operationId: string, how: 'now' | 'steer', justSent?: boolean): Promise<void>;
};
/** How long Send now or Steer may say it is at work before the strip gives the buttons back to try again. */
export const DELIVERY_TIMEOUT_MS = 20_000;
export function useQueuedDelivery(api: RunnerClient, sessionID: string, queued: readonly { operationId: string }[] | undefined): QueuedDelivery {
  const [working, setWorking] = useState<{ id: string; how: 'now' | 'steer' }>();
  const [error, setError] = useState<{ id: string; text: string }>();
  // A message that left the queue takes its confirmation, progress and error with it.
  const ids = (queued || []).map(message => message.operationId).join('\0');
  useEffect(() => {
    const present = new Set(ids.split('\0'));
    if (working && !present.has(working.id)) setWorking(undefined);
    if (error && !present.has(error.id)) setError(undefined);
  }, [ids]);
  useEffect(() => { setWorking(undefined); setError(undefined); }, [sessionID]);
  // An interrupt that never takes (the agent ignores it, the Runner restarted
  // meanwhile) must not leave "Stopping the current step…" for good: the
  // buttons come back, saying so, to try again.
  useEffect(() => {
    if (!working) return;
    const timer = setTimeout(() => {
      setWorking(current => current === working ? undefined : current);
      setError({ id: working.id, text: working.how === 'steer' ? 'The agent has not taken it yet. Steer again, or Send now.' : 'The current step has not stopped yet. Send now again to retry.' });
    }, DELIVERY_TIMEOUT_MS);
    return () => clearTimeout(timer);
  }, [working]);
  const deliver = async (operationId: string, how: 'now' | 'steer', justSent = false) => {
    setError(undefined); setWorking({ id: operationId, how });
    try { await api.apply({ sessionId: sessionID, operationId: crypto.randomUUID(), operation: { case: 'deliverQueued', value: { operationId, how } } }); }
    catch (e) {
      setWorking(undefined);
      const why = humanError(e);
      if (justSent && /no longer queued/.test(why)) return;
      setError({ id: operationId, text: `${how === 'steer' ? 'Could not steer' : 'Could not send it now'}: ${why}` });
    }
  };
  return { working, error, deliver };
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
  const busy = (!!session?.busy || !!turnRunning) && !!session && !session.closed;
  const [collapsed, setCollapsed] = useState(false);
  const { working, error } = delivery;
  const step = useMemo(() => busy ? stepName(activity) : {}, [busy, activity]);
  const now = useNow(busy && queued.length > 0);
  if (!busy || !queued.length) return null;

  const steer = !!session?.capabilities?.steer;
  const deliver = (message: QueuedEntry, how: 'now' | 'steer') => delivery.deliver(message.operationId, how);
  const many = queued.length > 1;
  const shown = collapsed ? [] : many && queued.length > 2 ? queued.slice(0, 2) : queued;
  // What the agent is busy with, as the live bar says it: the step and its time, else the status line.
  const stops = step.code ? `Stop ${step.code}` : step.words ? `Stop the running step (${step.words.charAt(0).toLowerCase() + step.words.slice(1)})` : `Stop what ${agentName} is doing`;
  // The time ticks every second: it is not part of what the status line announces, or a screen reader repeats the line each second.
  const busyWith = step.label || (live.tone === 'busy' || live.tone === 'waiting' ? live.text : 'working');
  const busyFor = step.label && step.startedAt !== undefined && now >= step.startedAt ? ` · ${elapsedLabel(now - step.startedAt)}` : '';

  return <section className="queued-messages" aria-label="Queued messages">
    <header className="queued-head">
      <Icon name="pending" size={12}/>
      <span role="status"><strong>{many ? `${queued.length} messages queued` : 'Queued'}</strong> — {agentName} is busy ({busyWith}{busyFor && <span aria-hidden="true">{busyFor}</span>})</span>
      <button type="button" className="queued-toggle" aria-expanded={!collapsed} onClick={() => setCollapsed(value => !value)}
        title={collapsed ? 'Show the queued messages' : 'Hide them here; they still go after the current turn'}>
        <Icon name={collapsed ? 'expand' : 'collapse'} size={11}/><span>{collapsed ? 'Show' : 'Hide'}</span>
      </button>
    </header>
    {shown.length > 0 && <ul className="queued-list">
      {shown.map((message, index) => {
        const mine = working?.id === message.operationId;
        // A message Lazybox wrote says what it is, not the words it carries.
        const origin = messageOrigin(message.origin), event = origin && systemEventView(origin, agentName);
        return <li key={message.operationId} className="queued-message" data-origin={origin?.kind} aria-label={event ? `Queued from Lazybox: ${event.line}` : `Queued message: ${clip(message.text, 80)}`}>
          <div className="queued-row">
            {event ? <span className="queued-text queued-event" title={message.text}><Icon name={event.icon} size={11}/><span>{clip(event.line)}</span>{event.detail && <small>{event.detail}</small>}</span>
              : <q className="queued-text" title={message.text}>{clip(message.text)}</q>}
            <span className="queued-actions">
              {mine ? <span className="queued-working" role="status">{working!.how === 'steer' ? 'Steering…' : 'Stopping the current step…'}</span> : <>
                {steer && <button type="button" onClick={() => void deliver(message, 'steer')}
                  title={`Give ${agentName} this message now, inside the running turn, without stopping it`}><Icon name="send" size={11}/>Steer</button>}
                <button type="button" onClick={() => void deliver(message, 'now')}
                  title={`${stops} and send this as the next turn${index === 0 && sendNowKey ? ` (${sendNowKey})` : ''}`}><Icon name="stop" size={11}/>Send now</button>
              </>}
            </span>
          </div>
          {error?.id === message.operationId && <p className="queued-error" role="alert">{error.text}</p>}
        </li>;
      })}
    </ul>}
    {!collapsed && queued.length > shown.length && <p className="queued-more">and {queued.length - shown.length} more, sent in order after these</p>}
    {!collapsed && !steer && <p className="queued-note">{agentName} can’t take a message in the middle of a turn: <strong>Send now</strong>{sendNowKey ? ` (${sendNowKey})` : ''} stops its current step first, then sends yours.</p>}
  </section>;
}
