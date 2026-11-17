/**
 * What Lazybox itself puts in a conversation, shown as events of it rather
 * than as anyone's words: a message Lazybox wrote to the agent (the answer to
 * its repository request, a review's findings, a question from the other
 * agent of a review, a mission's brief), and what a Runner restart left
 * uncertain. The Runner marks a message it wrote with its origin
 * (`MessageOrigin`, `origin` in the operation_accepted record's data); the
 * agent still receives the text unchanged, and the event opens to it.
 */
import type { MessageOrigin as WireOrigin } from '@lazybox/protocol';

/** A message Lazybox wrote: who it speaks for and what happened, in one line. */
export type MessageOrigin = { kind: string; summary: string; detail: string; copy: string; from: string;
  /** From's conversation, when it is another agent of this one's tree (an orchestrator or an agent it started). */
  conversation?: string; session?: string };

const text = (value: unknown) => typeof value === 'string' ? value : '';
/** The origin of an operation_accepted record (its data's `origin`) or of a queued message. */
export function messageOrigin(value: unknown): MessageOrigin | undefined {
  if (!value || typeof value !== 'object') return;
  const o = value as Partial<WireOrigin> & Record<string, unknown>;
  const kind = text(o.kind);
  if (!kind) return;
  return { kind, summary: text(o.summary), detail: text(o.detail), copy: text(o.copy), from: text(o.from), ...(text(o.conversation) ? { conversation: text(o.conversation), session: text(o.session) } : {}) };
}

/**
 * Journals written before the Runner recorded origins: the Runner names the
 * operations of the messages it writes, so those ids (never the text) say
 * which messages were Lazybox's. The summary is generic; the text is all
 * there is.
 */
export function legacyOrigin(operationId: string | undefined): MessageOrigin | undefined {
  if (!operationId) return;
  const kind = operationId.startsWith('repository-note-') ? 'repository_note'
    : operationId.startsWith('review-brief-') ? 'review_brief'
    : operationId.startsWith('review-ask-') ? 'agent_ask'
    : operationId.startsWith('review-answer-') ? 'agent_answer'
    : /^review-.+-findings-\d+$/.test(operationId) ? 'review_findings'
    : /^review-.+-round-\d+$/.test(operationId) ? 'review_round'
    : '';
  return kind ? { kind, summary: '', detail: '', copy: '', from: '' } : undefined;
}

export type SystemTone = 'neutral' | 'review' | 'ask' | 'ok' | 'muted' | 'warn';
export type SystemEventView = {
  icon: string; tone: SystemTone;
  /** What kind of event this is, for a screen reader and the opened view. */
  kind: string;
  /** The event in one line. */
  line: string;
  /** A short label beside it (a branch, "Round 2 of 3"). */
  detail: string;
  /** What the copy button gives, when there is something to copy. */
  copy: string;
  /** Who sent the message the event opens to. */
  sentBy: string;
};

type Kind = { icon: string; tone: SystemTone; kind: string; fallback: string };
const KINDS: Record<string, Kind> = {
  repository_added: { icon: 'repository', tone: 'ok', kind: 'Repository added', fallback: 'Added a repository to this workspace' },
  repository_declined: { icon: 'repository', tone: 'muted', kind: 'Repository declined', fallback: 'Declined adding a repository to this workspace' },
  repository_note: { icon: 'repository', tone: 'neutral', kind: 'Repository request', fallback: 'Answered the request for a repository' },
  review_brief: { icon: 'review', tone: 'review', kind: 'Review brief', fallback: 'Asked for a review' },
  review_findings: { icon: 'review', tone: 'review', kind: 'Review findings', fallback: 'Review findings sent' },
  review_round: { icon: 'refresh', tone: 'review', kind: 'Re-review', fallback: 'Re-review asked' },
  agent_ask: { icon: 'ask', tone: 'ask', kind: 'Question from an agent', fallback: 'A question from the other agent' },
  agent_answer: { icon: 'ask', tone: 'ask', kind: 'Answer from an agent', fallback: 'An answer from the other agent' },
  mission_brief: { icon: 'target', tone: 'neutral', kind: 'Mission task', fallback: 'Task assigned' },
  // Between an orchestrator and an agent it started (runner/orchestrator.go).
  agent_brief: { icon: 'subagent', tone: 'ask', kind: 'Task from your orchestrator', fallback: 'A task from the orchestrator that started this conversation' },
  agent_note: { icon: 'subagent', tone: 'ask', kind: 'Message from an agent', fallback: 'A message from another agent' },
  agent_report: { icon: 'check', tone: 'ok', kind: 'Report from an agent', fallback: 'An agent reported its task done' },
  plan_decided: { icon: 'checklist', tone: 'neutral', kind: 'Plan decision', fallback: 'The person decided the plan' },
  overlap_detected: { icon: 'alert', tone: 'warn', kind: 'Agents overlap', fallback: 'Two of your agents changed the same files' },
  continuation: { icon: 'history', tone: 'neutral', kind: 'Continuation', fallback: 'Continued from a checkpoint' },
  // A conversation another one asked the person to start: its brief; and, to the asking agent, the answer (runner/conversation_starts.go).
  conversation_brief: { icon: 'chat', tone: 'ask', kind: 'Brief from another conversation', fallback: 'Started at your request from another conversation' },
  conversation_started: { icon: 'chat', tone: 'ok', kind: 'Conversation started', fallback: 'The person started the conversation it asked for' },
  conversation_declined: { icon: 'chat', tone: 'muted', kind: 'Conversation declined', fallback: 'The person declined starting a conversation' },
  // A tracked pull request's reactions (runner/item_reactions.go): the person chose to send them.
  pr_checks: { icon: 'pullRequest', tone: 'warn', kind: 'PR checks', fallback: 'The pull request’s failing checks' },
  pr_review: { icon: 'review', tone: 'review', kind: 'PR review', fallback: 'The pull request’s review on GitHub' },
};
const UNKNOWN: Kind = { icon: 'sparkle', tone: 'neutral', kind: 'From Lazybox', fallback: 'A message from Lazybox' };

/** How a conversation shows a message Lazybox wrote. */
export function systemEventView(origin: MessageOrigin, agentName = 'the agent'): SystemEventView {
  const k = KINDS[origin.kind] || UNKNOWN;
  return {
    icon: k.icon, tone: k.tone, kind: k.kind,
    line: origin.summary.trim() || k.fallback,
    detail: origin.detail.trim(),
    copy: origin.copy.trim(),
    sentBy: origin.from ? `${origin.from}’s message, as Lazybox gave it to ${agentName}` : `What Lazybox told ${agentName}`,
  };
}

/** A Runner restart that left something uncertain, in one quiet line. */
export type RunnerNotice = 'delivery-unknown' | 'outcome-unknown' | 'agent-lost' | 'reverted' | 'cli' | 'pull-request' | 'connection-lost' | 'storage' | 'provider';
export const NOTICE_ICONS: Record<RunnerNotice, { icon: string; tone: SystemTone }> = {
  'delivery-unknown': { icon: 'alert', tone: 'warn' },
  'outcome-unknown': { icon: 'alert', tone: 'warn' },
  'agent-lost': { icon: 'refresh', tone: 'warn' },
  reverted: { icon: 'back', tone: 'neutral' },
  cli: { icon: 'terminal', tone: 'neutral' },
  'pull-request': { icon: 'pullRequest', tone: 'muted' },
  'connection-lost': { icon: 'alert', tone: 'warn' },
  storage: { icon: 'alert', tone: 'warn' },
  provider: { icon: 'alert', tone: 'warn' },
};

const clock = (at: number) => new Date(at).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });

/**
 * The line a lost stream leaves in the conversation (the Runner's
 * `process_exit`), completed by the re-attachment that followed it:
 * "Lazybox lost the connection at 11:39 · reconnected at 12:23". `cause` is
 * what the Runner recorded; an agent that really exited says so instead.
 */
export function connectionLine(cause: string, at?: number, reconnectedAt?: number, reconnected = reconnectedAt !== undefined): string {
  const when = (t?: number) => t !== undefined && Number.isFinite(t) ? ` at ${clock(t)}` : '';
  const lost = cause.startsWith('native process exited') ? 'The agent process stopped' : 'Lazybox lost the connection to the agent';
  return `${lost}${when(at)}${reconnected ? ` · reconnected${when(reconnectedAt)}` : ''}`;
}

/** "Restarted 13 times · last at 14:02": what the restart marker says on hover. */
export function restartTitle(count: number, at?: number): string {
  const when = at !== undefined && Number.isFinite(at) ? `, last at ${new Date(at).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })}` : '';
  return `The Runner restarted ${count === 1 ? 'once' : `${count} times`} here${when}. Each time it re-attached to this agent; nothing was interrupted.`;
}

/** Lazybox's events in a conversation, grouped when they follow each other (a restart mark is not one). */
export const isSystemEvent = (message: { type: string; origin?: unknown }) =>
  !!message.origin || message.type === 'notice' || message.type === 'exchange';
