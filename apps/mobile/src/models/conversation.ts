import type { AgentEvent, Session } from '@lazybox/protocol';
import { providerData } from '@lazybox/client-core';
import {
  approvalOf, isFailure, questionsOf, summarizeRun, systemEventView,
  type Question, type Risk, type TranscriptMessage,
} from '@lazybox/surfaces/models';

/**
 * A conversation on the iPhone as data: the transcript the shared fold
 * produced (surfaces/models `foldTranscript`, read by `ConversationReader`),
 * mapped to the rows a native list draws; the pending request as a card; and
 * what the composer offers. Pure, so tests pin each of them.
 */
export type NoteTone = 'neutral' | 'ok' | 'muted' | 'warn' | 'ask';
export type TranscriptRow =
  | { kind: 'you'; id: string; text: string; steered: boolean }
  | { kind: 'agent'; id: string; text: string; streaming: boolean }
  | { kind: 'failure'; id: string; text: string; detail: string }
  /** A run of the agent's tool calls, in one line ("Explored the codebase and ran tests · 8 commands"). */
  | { kind: 'activity'; id: string; text: string; meta: string; running: boolean; failed: number }
  /** One quiet line: an answer given, a Runner notice, a message Lazybox wrote. */
  | { kind: 'note'; id: string; text: string; tone: NoteTone; icon: 'check' | 'stop' | 'info' | 'chat' | 'alert' | 'arrow' };

const answeredLine = (message: TranscriptMessage): TranscriptRow | undefined => {
  const note = message.answered;
  if (!note) return undefined;
  if (note.kind === 'answered') return { kind: 'note', id: message.id, icon: 'check', tone: 'ok', text: note.answers.map(answer => `${answer.question}: ${answer.answer}`).join(' · ') };
  if (note.kind === 'always') return { kind: 'note', id: message.id, icon: 'check', tone: 'ok', text: `${note.label || 'Always allowed'} · ${note.tool}${note.target ? ` ${note.target}` : ''}` };
  return { kind: 'note', id: message.id, icon: 'stop', tone: 'muted', text: `Denied ${note.tool}${note.target ? ` ${note.target}` : ''}${note.reason ? ` · “${note.reason}”` : ''}` };
};

/** Pure: the rows a native transcript draws, oldest first. The last agent message streams while a turn runs. */
export function transcriptRows(messages: readonly TranscriptMessage[], { agentName, turnRunning = false }: { agentName: string; turnRunning?: boolean }): TranscriptRow[] {
  const rows: TranscriptRow[] = [];
  for (const message of messages) {
    const row = transcriptRow(message, agentName);
    if (row) rows.push(row);
  }
  const last = rows.at(-1);
  if (turnRunning && last?.kind === 'agent') rows[rows.length - 1] = { ...last, streaming: true };
  return rows;
}

function transcriptRow(m: TranscriptMessage, agentName: string): TranscriptRow | undefined {
  if (m.type === 'tools') {
    const tools = m.tools || [];
    if (!tools.length) return undefined;
    const run = summarizeRun(tools);
    return { kind: 'activity', id: m.id, text: run.text, meta: run.meta.join(' · '), running: run.running, failed: tools.filter(isFailure).length };
  }
  if (m.type === 'answered') return answeredLine(m);
  if (m.origin) {
    const view = systemEventView(m.origin, agentName);
    return { kind: 'note', id: m.id, icon: 'chat', tone: view.tone === 'review' ? 'neutral' : view.tone, text: view.line };
  }
  if (m.type === 'command' || (m.type === 'operation_accepted' && m.command)) return { kind: 'note', id: m.id, icon: 'arrow', tone: 'neutral', text: m.text.trim() };
  if (m.type === 'stopped') return { kind: 'note', id: m.id, icon: 'stop', tone: 'muted', text: m.text };
  if (m.type === 'operation_accepted') return { kind: 'you', id: m.id, text: m.text, steered: !!m.steered };
  if (m.type === 'message_delta') return m.text.trim() ? { kind: 'agent', id: m.id, text: m.text, streaming: false } : undefined;
  if (m.type === 'failure') return { kind: 'failure', id: m.id, text: m.text || 'The agent stopped', detail: m.detail || '' };
  if (m.type === 'subagent') return { kind: 'note', id: m.id, icon: 'info', tone: 'neutral', text: m.text.trim() || 'A sub-agent worked on part of this' };
  if (m.type === 'switched') return { kind: 'note', id: m.id, icon: 'info', tone: 'neutral', text: 'Continued on another subscription' };
  if (m.type === 'reattached') return { kind: 'note', id: m.id, icon: 'info', tone: 'muted', text: 'The Runner restarted; the agent kept working' };
  if (m.type === 'review' && m.review) {
    const r = m.review, found = [r.blockers && `${r.blockers} blocking`, r.majors && `${r.majors} major`, r.minors && `${r.minors} minor`].filter(Boolean).join(', ');
    return { kind: 'note', id: m.id, icon: 'check', tone: r.blockers ? 'warn' : 'neutral', text: `${r.reviewerLabel} ${r.finished ? 'reviewed' : 'is reviewing'} ${r.scopeLabel || 'this work'}${found ? ` · ${found}` : ''}` };
  }
  if (m.type === 'notice' || m.type === 'lineage' || m.type === 'exchange' || m.type === 'review-alert') return m.text.trim() ? { kind: 'note', id: m.id, icon: 'info', tone: 'neutral', text: m.text.trim() } : undefined;
  return m.text.trim() ? { kind: 'note', id: m.id, icon: 'info', tone: 'neutral', text: m.text.trim() } : undefined;
}

export type RequestView =
  | {
    kind: 'approval'; requestId: string;
    /** "Run a command", "Edit a file", or the tool. */
    title: string; tool: string;
    /** The command, the file, or what the agent says it does. */
    what: string;
    /** What the agent says it is doing, when it says more than `what`. */
    description: string;
    patch: string; added: number; removed: number;
    risks: Risk[];
    /** Allow asks first: flagged risks, or the provider's default-to-no. */
    confirm: boolean;
    defaultToNo: boolean;
    /** Allow is offered here; otherwise the reason it is not. */
    allow: boolean; reason?: string;
  }
  | { kind: 'question'; requestId: string; questions: Question[] }
  /** Answered on the computer, where its context is. */
  | { kind: 'elsewhere'; requestId: string; text: string };

const settingsTool = (tool: string) => tool === 'mcp__lazybox_settings__update_settings' || tool === 'lazybox_update_settings';

/** Pure: the card a pending request shows in the conversation. */
export function requestView(request: AgentEvent): RequestView {
  const requestId = request.requestId;
  if (request.type === 'repository_request') return { kind: 'elsewhere', requestId, text: 'The agent asks to add a repository. Add it or decline on your computer.' };
  if (request.type === 'browser_request') return { kind: 'elsewhere', requestId, text: 'The agent’s browser waits for you. Answer on your computer.' };
  if (request.type === 'conversation_request') return { kind: 'elsewhere', requestId, text: 'The agent asks to start another conversation. Answer on your computer.' };
  const data = providerData(request.providerDataJson);
  if (request.type === 'question') return { kind: 'question', requestId, questions: questionsOf(data) };
  const a = approvalOf(data, request.text, '');
  const what = a.command || a.file || a.description || a.title || a.tool;
  const title = a.command ? 'Run a command' : a.file ? (a.patch ? 'Edit a file' : 'Use a file') : a.tool || 'Approval';
  const reason = settingsTool(a.tool) ? 'Review this settings change on your computer.' : a.acceptUnsupported ? 'Lazybox cannot grant this kind of permission from a phone.' : undefined;
  return {
    kind: 'approval', requestId, title, tool: a.tool, what,
    description: a.description && a.description !== what ? a.description : '',
    patch: a.patch, added: a.added, removed: a.removed, risks: a.risks,
    confirm: a.risks.length > 0 || a.defaultToNo, defaultToNo: a.defaultToNo, allow: !reason, reason,
  };
}

/** What the person picked and typed for each question, by its key. */
export type AnswerDraft = Record<string, { picked: string[]; typed: string }>;

/** The values one question's answer sends, as the computer's question card sends them. */
export function answerValues(question: Question, draft: AnswerDraft[string] = { picked: [], typed: '' }): string[] {
  const typed = draft.typed.trim();
  if (!question.options.length) return typed ? [typed] : [];
  if (question.multi) return [...draft.picked, ...(typed && question.other ? [typed] : [])];
  return typed && question.other ? [typed] : draft.picked.slice(0, 1);
}

/** Pure: the `answer` operation's answers, or undefined while a question has none. */
export function answerPayload(questions: readonly Question[], draft: AnswerDraft): Record<string, { values: string[] }> | undefined {
  const entries = questions.map(question => [question.key, { values: answerValues(question, draft[question.key]) }] as const);
  return questions.length && entries.every(([, value]) => value.values.length > 0) ? Object.fromEntries(entries) : undefined;
}

export type ComposerView =
  | { mode: 'readonly'; note: string }
  | { mode: 'ended'; note: string }
  | {
    mode: 'idle' | 'busy';
    placeholder: string;
    /** "Send", or "Queue" while a turn runs: the message waits for the turn. */
    send: 'Send' | 'Queue';
    /** Send now: queue the message, then stop the current step so it goes next. */
    sendNow: boolean;
    interrupt: boolean;
    /** The conversation ended: sending continues it first (the `start` grant). */
    resume: boolean;
  };

/**
 * Whether a turn runs now. The journal is fresher than the polled session
 * (as `liveStatus` reads it): a turn it saw end is over, though the last
 * poll, up to 3 s old, still says busy. Without a turn in the journal read so
 * far, the session says.
 */
export function turnRunning(session: Session | undefined, transcript?: { turnRunning?: boolean }): boolean {
  if (!session || session.closed) return false;
  return transcript?.turnRunning ?? session.busy;
}

/** Pure: what the composer offers for this conversation and this phone's grants. `busy`: a turn runs (see `turnRunning`). */
export function composerView(session: Session | undefined, grants: readonly string[], busy = !!session?.busy): ComposerView {
  if (!grants.includes('respond')) return { mode: 'readonly', note: 'This phone can read conversations only. Pair it again on your computer to reply.' };
  if (!session) return { mode: 'idle', placeholder: 'Reply…', send: 'Send', sendNow: false, interrupt: false, resume: false };
  if (session.closed) {
    if (!grants.includes('start')) return { mode: 'ended', note: 'This conversation ended. Continue it on your computer.' };
    return { mode: 'idle', placeholder: 'Continue this conversation…', send: 'Send', sendNow: false, interrupt: false, resume: true };
  }
  if (busy) return { mode: 'busy', placeholder: 'Queue a follow-up…', send: 'Queue', sendNow: true, interrupt: true, resume: false };
  return { mode: 'idle', placeholder: 'Reply…', send: 'Send', sendNow: false, interrupt: false, resume: false };
}
