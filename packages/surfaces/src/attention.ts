import { conversationName } from './conversation-model';
import type { AgentEvent, Session } from '@lazybox/protocol';
import { repositoryRequestSummary } from './repository-request-model';
import { browserRequestSummary } from './browser-model';
import { startRequestSummary } from './conversation-request-model';
import { questionsOf } from './request-model';
import { providerData } from '@lazybox/client-core';
import { accountLabel, awaitingFirstMessage } from './agent-status';
import { backAt, isLimitState } from './subscription-model';

/**
 * What a conversation asks of the person, in the order it matters:
 * - needs-input: an approval or a question is waiting; the agent is blocked.
 * - failed: a fault, a provider failure, a failed delivery, or a turn a Runner
 *   restart interrupted.
 * - review: a turn ended since the person last looked at the conversation.
 * - working: the agent is on a turn.
 * - idle: nothing new — done and seen, ready, or ended.
 */
export type AttentionState = 'needs-input' | 'failed' | 'review' | 'working' | 'idle';
export const attentionOrder: readonly AttentionState[] = ['needs-input', 'failed', 'review', 'working', 'idle'];
export const attentionTitle: Record<AttentionState, string> = { 'needs-input': 'Needs input', failed: 'Failed', review: 'Ready for review', working: 'Working', idle: 'Done' };

export type Attention = {
  /** Stable across executions of one conversation (a resume keeps it). */
  key: string;
  state: AttentionState;
  /** Newest activity, in Unix milliseconds: what "age" and ordering use. */
  at: number;
  /** When the main agent's last turn ended; 0 when no turn ended yet. */
  turnEndedAt: number;
  /** Not yet seen: needs input, failed since last look, or a turn ended since last look. */
  unread: boolean;
  /** One short phrase for the state ("Approval: npm test", "Delivery failed"). */
  reason: string;
  /** The agent's last line, or the pending question, or the failure. */
  line: string;
  /** Pending request IDs, to tell a new request from one already announced. */
  requests: string[];
  session: Session;
  /** The window's Runner this conversation is on, when the window shows several (RunnerDirectory.tsx). */
  runner?: string;
};

/** Assistant side panels ("Ask Lazybox") are part of the workspace, not work items. */
const assistantPurposes = new Set(['help', 'planning', 'summary']);
export function isWorkConversation(session: Session): boolean {
  return !assistantPurposes.has(session.purpose);
}
export function conversationKey(session: Session): string {
  return session.conversationId || session.id;
}
function millis(value: bigint | number | undefined): number {
  const n = Number(value || 0);
  return Number.isFinite(n) && n > 0 ? n : 0;
}
export function activityAt(session: Session): number {
  return Math.max(millis(session.createdAtUnixMillis), ...session.agents.map(agent => millis(agent.updatedAtUnixMillis)));
}
/** Runners before turn_ended_at report only UpdatedAt; a finished turn falls back to it. */
export function turnEndedAt(session: Session): number {
  const root = session.agents.find(agent => agent.id === 'root') || session.agents.find(agent => !agent.parentId);
  if (!root) return 0;
  const ended = millis(root.turnEndedAtUnixMillis);
  if (ended) return ended;
  return root.lastTurnStatus || ['done', 'interrupted', 'failed'].includes(root.status) ? millis(root.updatedAtUnixMillis) : 0;
}

/** The last non-empty line of a message, without Markdown decoration, bounded for a row. */
export function lastLine(text: string, limit = 160): string {
  const lines = text.split('\n').map(line => line.replace(/^\s*(?:[#>*+-]+|\d+\.)\s+/, '').replace(/[*_`]+/g, '').replace(/\s+/g, ' ').trim()).filter(line => line && !/^`{3}|^[-=|: ]+$/.test(line));
  const line = lines.at(-1) || '';
  return line.length > limit ? line.slice(0, limit - 1).trimEnd() + '…' : line;
}

export function requestSummary(request: AgentEvent): string {
  if (request.type === 'repository_request') return lastLine(repositoryRequestSummary(request));
  if (request.type === 'browser_request') return lastLine(browserRequestSummary(request));
  if (request.type === 'conversation_request') return lastLine(startRequestSummary(request));
  const data = providerData(request.providerDataJson);
  if (request.type === 'question') {
    const questions: any[] = data.questions || data.input?.questions || [];
    const question = questions.find(q => typeof q?.question === 'string')?.question;
    return lastLine(question || request.text || 'The agent asked a question');
  }
  const input = data.input || data.arguments || data;
  const command = typeof input?.command === 'string' ? input.command : Array.isArray(input?.command) ? input.command.join(' ') : Array.isArray(input?.argv) ? input.argv.join(' ') : '';
  if (command) return lastLine(`Run ${command}`);
  return lastLine(request.text ? `Allow ${request.text}` : 'Approval required');
}

const faults: Record<string, string> = {
  delivery_failed: 'A message could not be delivered to the agent',
  provider_failure: 'The provider reported a failure',
  provider_error: 'The provider reported an error',
  'provider delivery failed; reconcile before continuing': 'A message could not be delivered to the agent',
};
export function faultSummary(session: Session): string {
  const account = session.account?.state || '';
  if (['auth_required', 'exhausted', 'throttled', 'billing_error', 'context_full', 'budget_exceeded'].includes(account)) {
    const back = isLimitState(account) && !session.closed ? backAt(session.account?.resetAt) : '';
    return accountLabel(account) + (back ? ` · ${back}` : '');
  }
  if (session.fault) return faults[session.fault] || lastLine(session.fault.replaceAll('_', ' '));
  return 'The agent reported a failure';
}

/** A Runner restart that could not re-attach leaves the agents it stopped as interrupted. */
function interruptedByRestart(session: Session): boolean {
  return session.historical && session.agents.some(agent => agent.status === 'interrupted');
}

/** Pure: the attention a conversation needs, given when it was last seen (Unix ms). */
export function deriveAttention(session: Session, seenAt = 0): Attention {
  const at = activityAt(session), ended = turnEndedAt(session);
  const requests = session.closed ? [] : session.pending.map(request => request.requestId);
  const base = { key: conversationKey(session), at, turnEndedAt: ended, requests, session };
  const message = lastLine(session.lastMessage || '');
  if (!session.closed && (session.pending.length || session.status === 'waiting_input' || session.status === 'waiting_approval')) {
    const first = session.pending[0];
    const question = first ? first.type === 'question' : session.status === 'waiting_input';
    const kind = first?.type === 'repository_request' ? 'Repository request' : first?.type === 'browser_request' ? 'Browser' : first?.type === 'conversation_request' ? 'New conversation' : question ? 'Question' : 'Approval';
    return { ...base, state: 'needs-input', unread: true, reason: kind + (session.pending.length > 1 ? ` · ${session.pending.length} waiting` : ''), line: first ? requestSummary(first) : message };
  }
  const fault = session.fault && session.fault !== 'native_exit_unconfirmed';
  if (fault || session.status === 'failed' || interruptedByRestart(session)) {
    const restart = !fault && session.status !== 'failed';
    return { ...base, state: 'failed', unread: at > seenAt, reason: restart ? 'Interrupted' : isLimitState(session.fault) && !session.closed ? 'Limit reached' : 'Failed', line: restart ? 'The Runner stopped during this turn and could not re-attach' : faultSummary(session) };
  }
  if (!session.closed && !awaitingFirstMessage(session) && (session.busy || session.queued > 0 || ['running', 'starting', 'switching_profile'].includes(session.status))) {
    return { ...base, state: 'working', unread: false, reason: session.status === 'switching_profile' ? 'Switching subscription' : 'Working', line: message };
  }
  if (ended > seenAt && ended > 0) {
    const interrupted = session.agents.find(agent => agent.id === 'root')?.lastTurnStatus === 'interrupted';
    return { ...base, state: 'review', unread: true, reason: interrupted ? 'Turn interrupted' : 'Turn ended', line: message };
  }
  return { ...base, state: 'idle', unread: false, reason: session.closed ? 'Ended' : session.status === 'unknown' ? 'Status unknown' : awaitingFirstMessage(session) ? 'Ready' : 'Done', line: message };
}

/**
 * What the person has seen: a mark per conversation, and `since`, before
 * which everything counts as seen — so the first run, or a conversation whose
 * mark was pruned, does not flood the inbox with old turns.
 */
export type Seen = { since: number; marks: Readonly<Record<string, number>> };
export function seenAt(seen: Seen, key: string): number {
  return Math.max(seen.since, seen.marks[key] || 0);
}
const seenLimit = 2000;
/** Pure: record that a conversation was seen up to `at`; never moves a mark back. */
export function markSeen(seen: Seen, key: string, at: number): Seen {
  if (!key || !(at > seenAt(seen, key))) return seen;
  const marks = { ...seen.marks, [key]: at };
  const keys = Object.keys(marks);
  if (keys.length <= seenLimit) return { since: seen.since, marks };
  // Forget the oldest marks; `since` rises to cover what they recorded.
  keys.sort((a, b) => marks[b] - marks[a]);
  let since = seen.since;
  for (const dropped of keys.slice(seenLimit)) { since = Math.max(since, marks[dropped]); delete marks[dropped]; }
  return { since, marks };
}
export function parseSeen(text: string | null, now: number): Seen {
  try {
    const value = text && text.length < 1 << 20 ? JSON.parse(text) : null;
    if (value && Number.isFinite(value.since) && value.marks && typeof value.marks === 'object' && !Array.isArray(value.marks))
      return { since: value.since, marks: Object.fromEntries(Object.entries(value.marks).filter((entry): entry is [string, number] => typeof entry[1] === 'number' && Number.isFinite(entry[1]))) };
  } catch { /* A corrupt record starts over from now, never from zero. */ }
  return { since: now, marks: {} };
}

export type Inbox = { rows: Attention[]; groups: { state: AttentionState; rows: Attention[] }[]; counts: Record<AttentionState, number>; unread: number };
/** Pure: one row per conversation, grouped by state, newest first within a group. */
export function buildInbox(sessions: readonly Session[], seen: Seen): Inbox {
  const byKey = new Map<string, Attention>();
  for (const session of sessions) {
    if (!isWorkConversation(session)) continue;
    const row = deriveAttention(session, seenAt(seen, conversationKey(session)));
    const previous = byKey.get(row.key);
    if (!previous || row.at > previous.at) byKey.set(row.key, row);
  }
  const rows = [...byKey.values()].sort((a, b) => attentionOrder.indexOf(a.state) - attentionOrder.indexOf(b.state) || b.at - a.at || (a.key < b.key ? -1 : 1));
  const counts = Object.fromEntries(attentionOrder.map(state => [state, 0])) as Record<AttentionState, number>;
  for (const row of rows) counts[row.state]++;
  return { rows, groups: attentionOrder.map(state => ({ state, rows: rows.filter(row => row.state === state) })).filter(group => group.rows.length), counts, unread: rows.filter(row => row.unread).length };
}

/** The timestamp that marks everything a conversation has shown so far as seen. */
export function seenMark(row: Pick<Attention, 'at' | 'turnEndedAt'>): number {
  return Math.max(row.at, row.turnEndedAt);
}

/** Notifications (a system notification, or a toast in a focused window) per kind; sounds apart from them. */
export type NotificationPreferences = { needsInput: boolean; failed: boolean; review: boolean; badge: boolean; soundNeedsYou: boolean; soundDone: boolean };
export const defaultNotificationPreferences: NotificationPreferences = { needsInput: true, failed: true, review: false, badge: true, soundNeedsYou: true, soundDone: false };
export type AttentionSpeech = { id: string; text: string; at: number; requestId?: string };
export type AttentionNotice = { speech?: AttentionSpeech; key: string; kind: 'needs-input' | 'failed' | 'review'; title: string; body: string; collectionId: string; sessionId: string; runner?: string };

/**
 * Pure: what deserves a notification between two observations of one
 * conversation. Only a change this window watched happen counts — the first
 * observation never notifies — and only one with new agent activity behind it,
 * so a Runner restart that re-labels state without the agent doing anything
 * never notifies. A conversation the person is looking at never notifies.
 */
export function noticeFor(previous: Attention | undefined, next: Attention, preferences: NotificationPreferences, viewing: boolean, workspace = ''): AttentionNotice | undefined {
  if (!previous || viewing) return undefined;
  const title = conversationName(next.session, 'Untitled conversation');
  const where = workspace ? `${workspace} · ` : '';
  const target = { key: next.key, collectionId: next.session.collectionId, sessionId: next.session.id };
  if (next.state === 'needs-input') {
    const fresh = next.requests.filter(id => !previous.requests.includes(id));
    if (!preferences.needsInput || (previous.state === 'needs-input' && !fresh.length) || (!fresh.length && next.at <= previous.at)) return undefined;
    const request = next.session.pending.find(item => fresh.includes(item.requestId)) || next.session.pending[0];
    const question = request?.type === 'question' ? questionsOf(providerData(request.providerDataJson))[0] : undefined;
    const label = request?.type === 'question' || (!request && next.session.status === 'waiting_input') ? 'Question' : 'Approval needed';
    const detail = question?.text || (request ? requestSummary(request) : 'The agent is waiting for you');
    const choices = question?.options.map(option => option.label) || [];
    const options = choices.length && choices.length <= 2 && choices.every(choice => choice.length <= 32) ? ` Choices: ${choices.join(' or ')}.` : '';
    const context = [workspace, title].filter(Boolean).join('. ');
    const speech = { id: JSON.stringify([next.runner, next.key, 'request', request?.requestId || next.at]), at: next.at, requestId: request?.requestId, text: `${context}. ${label === 'Question' ? '' : 'Approval needed. '}${detail}${options}` };
    return { ...target, speech, kind: 'needs-input', title: `${label} · ${title}`, body: where + (request ? requestSummary(request) : (next.line || 'The agent is waiting for you')) };
  }
  if (next.state === 'failed') {
    if (!preferences.failed || previous.state === 'failed') return undefined;
    // An interruption needs a turn the restart actually stopped; a fault needs new activity.
    if (next.reason === 'Interrupted' ? previous.state !== 'working' : next.at <= previous.at) return undefined;
    const speech = { id: JSON.stringify([next.runner, next.key, 'failed', next.at]), at: next.at, text: [workspace, title, next.reason, next.line].filter(Boolean).join('. ') };
    return { ...target, speech, kind: 'failed', title: `${next.reason} · ${title}`, body: where + next.line };
  }
  if (next.state === 'review') {
    if (!preferences.review || previous.state === 'review' || next.turnEndedAt <= previous.turnEndedAt) return undefined;
    return { ...target, kind: 'review', title: `Ready for review · ${title}`, body: where + (next.line || 'The agent finished its turn') };
  }
  return undefined;
}

/** "now", "4m", "3h", "2d" — the age column. */
export function ageLabel(at: number, now: number): string {
  if (!at) return '';
  const seconds = Math.max(0, Math.round((now - at) / 1000));
  if (seconds < 45) return 'now';
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.round(minutes / 60);
  if (hours < 24) return `${hours}h`;
  const days = Math.round(hours / 24);
  return days < 30 ? `${days}d` : `${Math.round(days / 30)}mo`;
}
