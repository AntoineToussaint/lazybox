import type { JournalEvent } from '@lazybox/protocol';
import { providerData } from '@lazybox/client-core';
import { failureSentence, isLimitState, isProviderDiagnostic, switchNote, type SwitchNote } from './subscription-model';
import { emptySubagents, SubagentFold, subagentRelevant, type Subagents } from './subagent-model';
import { applyToolPayload, hiddenTool, THINKING, type ToolEntry } from './tool-lines';
import { noteStops, stoppedEntry, type TurnStops } from './tool-stops';
import { answerNote, requestNote, type AnswerNote, type RequestNote } from './request-model';
import { commandLine, emptyCatalog, mergeCatalog, parseSlash, COMMAND_NAME, type CommandCatalog } from './slash-commands';
import { legacyOrigin, messageOrigin, type MessageOrigin, type RunnerNotice } from './system-events';
import { LINEAGE_EVENTS } from './orchestration-model';

/**
 * `agentId` names the sub-agent whose card a `subagent` message is.
 * `tools`: a run of the main agent's tool calls, one line each in the
 * conversation. `answered`: how a question or approval was answered.
 */
export type TranscriptMessage = { id: string; type: string; text: string; turnId: string; files?: { name: string; size: number; mediaType?: string; path?: string }[]; count?: number; switched?: SwitchNote; operationId?: string; agentId?: string; tools?: ToolEntry[]; answered?: AnswerNote; /** A message that ran one of the agent's slash commands: shown as one compact line. */ command?: boolean; review?: ReviewNote;
  /** A message Lazybox wrote to the agent (operation_accepted): an event of the conversation, never the person's words. */
  origin?: MessageOrigin;
  compaction?: boolean;
  promptModel?: string;
  promptContext?: { used: number; capacity?: number };
  /** A `notice`: what a Runner restart (or a revert) left to know. */
  notice?: RunnerNotice;
  /** A `reattached` marker: when the last of its restarts was. A `connection-lost` notice: when the stream was lost. */
  at?: number;
  /** A `connection-lost` notice the Runner's re-attachment completed: when it reconnected. */
  reconnected?: { at?: number };
  /** `lineage`: an agent-tree line (started, messaged, stopped, granted) and what it points at. */
  lineage?: LineageNote;
  /** A `failure`: what the provider said, when the line says it in plain words instead. */
  detail?: string;
  /** A queued message given to the agent inside its running turn (Steer), shown where it was. */
  steered?: boolean;
  delegationRequested?: boolean;
  terminalHandoff?: TerminalHandoff };

export type TerminalHandoff = { id: string; workspaceId: string; cwd: string; argv: string[]; agentInput?: boolean; status: string; terminalId?: string };
/** One line about an agent tree, as the Runner recorded it (runner/orchestrator.go). */
export type LineageNote = { kind: string; conversation: string; session: string; mission: string; direction: string; plan: string };
/** An agent review of this conversation's work (the Runner's review_started /
 * review_finished events): its card follows the review live; the counts here
 * are what the last event said, for when the review record is gone. */
export type ReviewNote = { review: string; workspace: string; reviewerSession: string; reviewerLabel: string; scopeLabel: string; blockers: number; majors: number; minors: number; nits: number; finished: boolean;
  /** "cancelled" or "stopped" when the person ended it by hand (the event says so). */
  state?: string };
function reviewNote(data: Record<string, unknown>, finished: boolean): ReviewNote | undefined {
  const text = (key: string) => typeof data[key] === 'string' ? data[key] as string : '';
  const count = (key: string) => typeof data[key] === 'number' ? data[key] as number : 0;
  if (!text('review') || !text('reviewerSession')) return;
  const state = text('state') === 'cancelled' || text('state') === 'stopped' ? text('state') : '';
  return { review: text('review'), workspace: text('workspace'), reviewerSession: text('reviewerSession'), reviewerLabel: text('reviewerLabel') || 'Reviewer', scopeLabel: text('scopeLabel'), blockers: count('blockers'), majors: count('majors'), minors: count('minors'), nits: count('nits'), finished, ...(state ? { state } : {}) };
}
/** `requests`: what each question or approval asked, to say how it was answered. */
export type Transcript = { messages: TranscriptMessage[]; recent: JournalEvent[]; activity: JournalEvent[]; activityTruncated: boolean; cursor: bigint; appendToLast: boolean; reportedPermission?: string; reportedModel:string; lastObservedModel?: string; reportedContext?: TranscriptMessage['promptContext']; subagents: Subagents; requests: Record<string, RequestNote>; /** The agent's slash commands and models (`command_catalog`). */ catalog: CommandCatalog;
  /** Send now stopped the turn in progress, which has not ended yet. */
  sendingNow?: boolean;
  /** The agent's turn is running (or a switch of subscription started one): a message accepted now waits in the queue. */
  turnRunning?: boolean;
  /** Where the running turn's output starts in `messages`, until a receipt arrives in it. */
  turnStart?: number;
  /**
   * Messages accepted while the agent was busy and not delivered yet, by
   * operation ID, in the order accepted. The Queued strip shows them; the
   * conversation shows each once, where its receipt says the agent got it.
   */
  pending?: Record<string, TranscriptMessage>;
  canceled?: ReadonlySet<string>;
  /** Whether this turn was interrupted or an approval declined: a call that then failed was stopped or denied. */
  stops?: TurnStops };

export const emptyTranscript = (): Transcript => ({ messages: [], recent: [], activity: [], activityTruncated: false, cursor: 0n, appendToLast: false, reportedModel:'', subagents: emptySubagents(), requests: {}, catalog: emptyCatalog() });

/** A message waiting behind the turn in progress, as the Queued strip shows it. */
export type QueuedEntry = { editing?: boolean; operationId: string; text: string; /** A message Lazybox wrote (QueuedMessage.origin, or the transcript's parsed one). */ origin?: unknown };
/**
 * What the Queued strip shows: the Runner's queue (polled every 1.5 s)
 * merged with the transcript's, which knows at once. A message accepted
 * behind a busy turn is in the transcript's queue the moment its record
 * arrives, before the next poll; one the agent got is in the conversation
 * the moment its receipt does, though the last poll still lists it. Either
 * way it is in one place only: never gone for a poll, never shown twice.
 * The Runner's order wins; the transcript's newer ones follow it.
 */
export function queuedMessages(polled: readonly QueuedEntry[] | undefined, transcript: Pick<Transcript, 'pending' | 'messages' | 'canceled'>): QueuedEntry[] {
  const pending = transcript.pending ?? {};
  const out: QueuedEntry[] = [];
  let placed: Set<string> | undefined;
  for (const message of polled ?? []) {
    if (transcript.canceled?.has(message.operationId)) continue;
    const waiting = pending[message.operationId];
    if (!waiting) {
      // In the conversation already: the agent got it since the poll.
      placed ??= new Set(transcript.messages.flatMap(m => m.operationId ? [m.operationId] : []));
      if (placed.has(message.operationId)) continue;
    }
    out.push(message);
  }
  const listed = new Set(out.map(message => message.operationId));
  for (const [operationId, message] of Object.entries(pending)) if (!listed.has(operationId)) out.push({ operationId, text: message.text, ...(message.origin ? { origin: message.origin } : {}) });
  return out;
}

/** The prompt an agent message answers: the last thing you sent before it (never a message Lazybox wrote to the agent). */
export function promptBefore(messages: readonly TranscriptMessage[], id: string): TranscriptMessage | undefined {
  for (let i = messages.findIndex(message => message.id === id) - 1; i >= 0; i--) if (messages[i].type === 'operation_accepted' && !messages[i].origin) return messages[i];
  return undefined;
}

/** Fold one record into durable message-shaped state; diagnostic events are bounded separately. */
export function appendTranscript(state: Transcript, record: JournalEvent): Transcript {
  return foldTranscript(state, [record]);
}

const RECENT = 80, ACTIVITY = 1024;
/**
 * Fold a batch of records with one copy of each array, not one per record.
 * Replaying a long conversation is tens of thousands of streamed deltas; a
 * per-record copy of the message list made opening one quadratic. The input
 * state is never mutated: objects created in this batch are the only ones
 * extended in place.
 */
export function foldTranscript(state: Transcript, records: readonly JournalEvent[]): Transcript {
  const fresh: JournalEvent[] = [];
  let cursor = state.cursor;
  for (const record of records) if (record.cursor > cursor) { fresh.push(record); cursor = record.cursor; }
  if (!fresh.length) return state;
  const next: Transcript = { ...state, messages: state.messages, recent: state.recent, activity: state.activity };
  let messages: TranscriptMessage[] | undefined;
  const activity: JournalEvent[] = [];
  const owned = new Set<TranscriptMessage>();
  const list = () => messages ??= [...state.messages];
  const replaceLast = (message: TranscriptMessage) => { const m = list(); m[m.length - 1] = message; owned.add(message); };
  const add = (message: TranscriptMessage) => { list().push(message); owned.add(message); };
  let subagents: SubagentFold | undefined;
  const replaceAt = (index: number, message: TranscriptMessage) => { list()[index] = message; owned.add(message); };
  let requests: Record<string, RequestNote> | undefined;
  let pending: Record<string, TranscriptMessage> | undefined;
  let canceled: Set<string> | undefined;
  const queued = () => pending ?? state.pending ?? {};
  /** A message the person (or Lazybox) said, at the end or at `at`. */
  const place = (message: TranscriptMessage, at?: number) => {
    // A line the review already said ("Codex asked Claude: …") is this message now.
    const said = message.origin?.summary ? saidBy(messages ?? state.messages, message.origin.summary, m => m.type === 'exchange') : -1;
    if (said >= 0) {
      list().splice(said, 1);
      if (next.turnStart !== undefined && said < next.turnStart) next.turnStart--;
      if (at !== undefined && said < at) at--;
    }
    if (at === undefined || at >= list().length) { add(message); next.appendToLast = false; }
    else { list().splice(at, 0, message); owned.add(message); }
  };
  /**
   * A message accepted while the agent is busy waits in the queue: the Queued
   * strip shows it, and the conversation does once it is delivered. One sent
   * while idle is the next turn's start, shown at once.
   */
  const accept = (message: TranscriptMessage, held = false) => {
    const id = message.operationId;
    if (id && (held || next.turnRunning || Object.keys(queued()).length)) (pending ??= { ...state.pending })[id] = message;
    else place(message);
  };
  for (const record of fresh) {
    next.cursor = record.cursor;
    const event = record.event;
    if (event && ACTIVITY_TYPES.has(event.type)) activity.push(record);
    // A sub-agent's card sits where it started; its own messages and tools live in it.
    if (subagentRelevant(event)) {
      const fold = subagents ??= new SubagentFold(state.subagents ?? emptySubagents());
      fold.record(record);
      for (const agentId of fold.takeCards()) { add({ id: `subagent:${agentId}`, type: 'subagent', text: '', turnId: event!.turnId, agentId }); next.appendToLast = false; }
    }
    if (!event || (event.agentId && event.agentId !== 'root')) continue;
    // While a turn runs (as the Runner counts it), a message waits in the queue.
    if (event.type === 'turn_started') { next.turnRunning = true; next.turnStart = (messages ?? state.messages).length; }
    else if (event.type === 'turn_ended' || event.type === 'session_closed') { next.turnRunning = false; next.turnStart = undefined; }
    else if (event.type === 'profile_changed' && event.status !== 'checkpoint_next_message') next.turnRunning = true;
    // A queued message joins the conversation where its receipt says the agent
    // got it: after the turn it waited for (and the stop Send now made), as
    // the start of its own turn; or, steered, inside the running one. One the
    // session dropped while it waited never does.
    if (event.type === 'queued_message_edit') {
      if (event.status === 'save_edit') {
        const message = queued()[event.operationId];
        if (message) (pending ??= { ...state.pending })[event.operationId] = { ...message, text: event.text };
        else {
          const i = lastWith(messages ?? state.messages, event.operationId);
          if (i >= 0) list()[i] = { ...list()[i], text: event.text };
        }
      }
      continue;
    }
    const receipt = receiptOf(record);
    if (receipt) {
      if (receipt.delivery === 'canceled_before_delivery') {
        (canceled ??= new Set(next.canceled)).add(receipt.id);
        next.canceled = canceled;
        const index = lastWith(messages ?? state.messages, receipt.id);
        if (index >= 0) {
          list().splice(index, 1);
          if (next.turnStart !== undefined && index < next.turnStart) next.turnStart--;
        }
      }
      const message = queued()[receipt.id];
      if (message) {
        delete (pending ??= { ...state.pending })[receipt.id];
        if (receipt.delivery !== 'canceled_before_delivery') {
          const delegationRequested = event.status === 'delegation_requested';
          const steered = delegationRequested || event.status === 'steered';
          // The turn it starts can be recorded a moment before its receipt (Codex): it opens that turn.
          place(steered ? { ...message, steered, ...(delegationRequested && { delegationRequested }) } : message, steered || !next.turnRunning ? undefined : next.turnStart);
        }
      }
      // A receipt during a turn is its start's, or later than it.
      if (next.turnRunning) next.turnStart = undefined;
    }
    next.stops = noteStops(next.stops, event);
    if (event.type === 'session_ready' || event.type === 'context' || event.type === 'usage') {
      const data = providerData(event.providerDataJson);
      const model = data.model;
      const used = event.type === 'context' ? data.used : data.tokenUsage?.last?.totalTokens;
      const capacity = event.type === 'context' ? data.capacity : data.tokenUsage?.modelContextWindow;
      if (typeof used === 'number' && Number.isFinite(used) && used >= 0) {
        next.reportedContext = { used, capacity: typeof capacity === 'number' && capacity > 0 ? capacity : undefined };
      }
      if (typeof model === 'string' && model.trim()) {
        next.reportedModel = model;
        next.lastObservedModel = model;
      }
    }
    if (event.type === 'agent_permission') {
      next.reportedPermission = event.text === 'bypassPermissions' || event.text.toLowerCase() === 'yolo' ? 'YOLO' : event.text;
      continue;
    }
    if (event.type === 'command_catalog') { next.catalog = mergeCatalog(next.catalog ?? emptyCatalog(), providerData(event.providerDataJson)); continue; }
    // A command run (a model or permission switch, a compaction) is one quiet line.
    const ran = commandLine(event.type, event.text, event.status, event.type === 'compacted' ? providerData(event.providerDataJson) : {});
    if (ran) {
      if (event.type === 'model_changed') next.reportedModel = event.text;
      add({ id: record.cursor.toString(), type: 'command', text: ran, turnId: event.turnId, ...(event.type === 'compacted' && {compaction: true}) });
      next.appendToLast = false;
      continue;
    }
    const last = (messages ?? state.messages).at(-1);
    // A restart that interrupted nothing is not news: one quiet marker, which
    // every later restart joins until the person says something, so hot
    // reloads never stack up in the conversation.
    // The Runner lost its stream from the agent (its own failure, or the
    // agent's exit): one quiet line, kept as history. The cause and the
    // supervisor's end that follows it are the same loss.
    if (event.type === 'process_exit') {
      const current = messages ?? state.messages, at = Date.parse(record.capturedAt);
      if (openLoss(current) < 0) add({ id: record.cursor.toString(), type: 'notice', notice: 'connection-lost', text: event.text, turnId: event.turnId, ...(Number.isFinite(at) ? { at } : {}) });
      next.appendToLast = false;
      continue;
    }
    if (event.type === 'runner_reattached' && !record.receipt) {
      const current = messages ?? state.messages, at = Date.parse(record.capturedAt), when = Number.isFinite(at) ? { at } : {};
      // The re-attachment completes the line the loss left: "· reconnected at 12:23".
      const lost = openLoss(current);
      if (lost >= 0) { replaceAt(lost, { ...current[lost], reconnected: when }); continue; }
      const index = restartMarker(current);
      if (index >= 0) replaceAt(index, { ...current[index], count: (current[index].count || 1) + 1, ...when });
      else add({ id: record.cursor.toString(), type: 'reattached', text: '', turnId: event.turnId, count: 1, ...when });
      continue;
    }
    if (event.type === 'terminal_handoff') {
      if (event.status === 'pending') {
        let argv: unknown;
        try { argv = JSON.parse(new TextDecoder().decode(event.providerDataJson)); } catch { continue; }
        const legacyShared = !!argv && typeof argv === 'object' && 'agentInput' in argv && argv.agentInput === true;
        const agentInput = event.terminalAgentInput || legacyShared;
        if (legacyShared && argv && typeof argv === 'object' && 'argv' in argv) argv = argv.argv;
        if (!Array.isArray(argv) || argv.some(value => typeof value !== 'string')) continue;
        add({ id: record.cursor.toString(), type: event.type, text: '', turnId: event.turnId,
          terminalHandoff: { id: event.requestId, workspaceId: event.name, cwd: event.text, argv, agentInput, status: 'pending' } });
        next.appendToLast = false;
      } else {
        const current = messages ?? state.messages;
        const index = current.findLastIndex(message => message.terminalHandoff?.id === event.requestId);
        const handoff = current[index]?.terminalHandoff;
        if (handoff) replaceAt(index, { ...current[index], terminalHandoff: {
          ...handoff, status: event.status, terminalId: event.text || handoff.terminalId,
        } });
      }
      continue;
    }
    if (event.type === 'question' || event.type === 'approval') {
      const note = event.requestId ? requestNote(event.type, providerData(event.providerDataJson), event.text) : undefined;
      if (note) (requests ??= { ...state.requests })[event.requestId] = note;
    }
    // A question's answer, an "always allow" or a decline: one quiet line.
    if (event.type === 'operation_accepted' && event.requestId) {
      const response = providerData(event.providerDataJson).response;
      const answered = response ? answerNote((requests ?? state.requests)[event.requestId], response) : undefined;
      if (answered) add({ id: record.cursor.toString(), type: 'answered', text: '', turnId: event.turnId, answered });
      next.appendToLast = false;
      continue;
    }
    // Thinking is one quiet line ("Thinking…", then "Thought for 4s"); what
    // the agent thought is private and never shown.
    const at = Date.parse(record.capturedAt);
    const when = Number.isFinite(at) ? { at } : {};
    if (event.type === 'activity' && event.status === 'thinking') {
      startThinking(messages ?? state.messages, replaceAt, add, owned, event.turnId, record.cursor.toString(), Number.isFinite(at) ? at : undefined);
      next.appendToLast = false;
      continue;
    }
    if (THINKING_ENDS.has(event.type)) endThinking(messages ?? state.messages, replaceAt, owned, Number.isFinite(at) ? at : undefined);
    if (event.type === 'tool_activity') {
      foldTool(messages ?? state.messages, record, list, replaceAt, add, owned, next.stops);
      next.appendToLast = false;
      continue;
    }
    if (event.type === 'turn_ended' || event.type === 'session_closed') settleTools(messages ?? state.messages, replaceAt, owned, !!next.sendingNow);
    // Send now stops the turn in progress (the Runner's interrupt
    // "<message>:now"), and the queued message is the next turn. The stop is
    // one quiet line, never an error: a journal from before the Runner knew
    // (or a worker that still reports it) has Claude's "error during
    // execution" failure here, which only reports the stop.
    if (event.type === 'operation_accepted' && !event.text && event.operationId.endsWith(':now')) { next.sendingNow = true; next.appendToLast = false; continue; }
    if (next.sendingNow && event.type === 'failure' && stopReport(event.status, event.account?.state)) { next.appendToLast = false; continue; }
    if (next.sendingNow && (event.type === 'turn_ended' || event.type === 'session_closed')) {
      next.sendingNow = false;
      if (event.type === 'turn_ended') add({ id: record.cursor.toString(), type: 'stopped', text: 'Stopped to send your message', turnId: event.turnId });
      next.appendToLast = false;
      continue;
    }
    // A review of this conversation's work: one card, where the review started.
    if (event.type === 'review_started' || event.type === 'review_finished') {
      const note = reviewNote(providerData(event.providerDataJson), event.type === 'review_finished');
      if (note) {
        const current = messages ?? state.messages, id = `review:${note.review}`;
        const index = current.findIndex(message => message.id === id);
        if (index >= 0) replaceAt(index, { ...current[index], text: event.text, review: { ...note, finished: note.finished || !!current[index].review?.finished } });
        else add({ id, type: 'review', text: event.text, turnId: event.turnId, review: note });
      }
      next.appendToLast = false;
      continue;
    }
    // The two agents of a review asking each other ("Codex asked Claude: …"),
    // and a loop that stopped to ask the person: one labelled line each.
    if (event.type === 'review_exchange' || event.type === 'review_escalated') {
      // The question as the agent received it already says this line.
      if (event.type === 'review_exchange' && saidBy(messages ?? state.messages, event.text, m => !!m.origin) >= 0) { next.appendToLast = false; continue; }
      const data = providerData(event.providerDataJson);
      const review = typeof data.review === 'string' ? data.review : '', workspace = typeof data.workspace === 'string' ? data.workspace : '';
      add({ id: record.cursor.toString(), type: event.type === 'review_exchange' ? 'exchange' : 'review-alert', text: event.text, turnId: event.turnId, ...(review ? { review: { review, workspace, reviewerSession: typeof data.reviewerSession === 'string' ? data.reviewerSession : '', reviewerLabel: typeof data.reviewerLabel === 'string' ? data.reviewerLabel : '', scopeLabel: '', blockers: 0, majors: 0, minors: 0, nits: 0, finished: true } } : {}) });
      next.appendToLast = false;
      continue;
    }
    // An agent tree's own lines: an agent started, a message sent, a grant given or taken back.
    if (LINEAGE_EVENTS.has(event.type)) {
      const data = providerData(event.providerDataJson);
      const text = (key: string) => typeof data[key] === 'string' ? data[key] as string : '';
      add({ id: record.cursor.toString(), type: 'lineage', text: event.text, turnId: event.turnId, lineage: { kind: event.type, conversation: text('conversation') || text('to'), session: text('session'), mission: text('mission'), direction: text('direction'), plan: text('plan') } });
      next.appendToLast = false;
      continue;
    }
    // What Lazybox told the agent of its own changes (runner/briefing.go): one
    // quiet event, opened to exactly what the agent received, just before the
    // message it went with. Never the person's words.
    if (event.type === 'briefing_updated' && event.text) {
      const message: TranscriptMessage = { id: record.cursor.toString(), type: 'briefing', text: event.text, turnId: event.turnId, origin: briefingOrigin(event.status) };
      const at = event.operationId ? lastWith(list(), event.operationId) : -1;
      if (at < 0) add(message);
      else {
        list().splice(at, 0, message); owned.add(message);
        if (next.turnStart !== undefined && at <= next.turnStart) next.turnStart++;
      }
      next.appendToLast = false;
      continue;
    }
    const notice = noticeOf(record);
    if (notice) {
      // The same uncertainty twice in a row is one line that says how often.
      if (last?.type === 'notice' && last.notice === notice.notice && last.text === notice.text) replaceLast({ ...last, count: (last.count || 1) + 1 });
      else add({ id: record.cursor.toString(), type: 'notice', text: notice.text, turnId: event.turnId, notice: notice.notice });
      next.appendToLast = false;
      continue;
    }
    // A message Lazybox wrote to the agent is an event of the conversation.
    if (event.type === 'operation_accepted' && event.text) {
      const data = providerData(event.providerDataJson);
      const origin = messageOrigin(data.origin) ?? legacyOrigin(event.operationId);
      if (origin) {
        accept({ id: record.cursor.toString(), type: event.type, text: event.text, turnId: event.turnId, ...when, origin, ...(event.operationId ? { operationId: event.operationId } : {}), ...(Array.isArray(data.files) && data.files.length ? { files: data.files } : {}) });
        continue;
      }
    }
    // A switch of subscription is one quiet line. A limit is not a message at
    // all: the conversation's limit card says it while it matters, and the
    // switch line keeps the record.
    if (event.type === 'profile_changed') {
      add({ id: record.cursor.toString(), type: 'switched', text: '', turnId: event.turnId, switched: switchNote(event.text, event.status, providerData(event.providerDataJson)) });
      next.appendToLast = false;
      continue;
    }
    if (event.type === 'failure' && (isLimitState(event.status) || isLimitState(event.account?.state))) { next.appendToLast = false; continue; }
    const files = event.type === 'operation_accepted' ? providerData(event.providerDataJson).files as TranscriptMessage['files'] : undefined;
    if (event.type === 'message_delta' && event.text) {
      if (next.appendToLast && last?.type === 'message_delta' && last.turnId === event.turnId) {
        const firstKnownTime = last.at === undefined ? when : {};
        if (owned.has(last)) { last.text += event.text; Object.assign(last, firstKnownTime); }
        else replaceLast({ ...last, text: last.text + event.text, ...firstKnownTime });
      } else add({ id: record.cursor.toString(), type: event.type, text: event.text, turnId: event.turnId, ...when });
      next.appendToLast = true;
    } else if (event.type === 'operation_accepted' && (event.text || files?.length)) {
      accept({ id: record.cursor.toString(), type: event.type, text: event.text, turnId: event.turnId, files, ...when, promptModel: next.lastObservedModel, promptContext: next.reportedContext, ...(event.operationId ? { operationId: event.operationId } : {}), ...(ranCommand(event.text, next.catalog) ? { command: true } : {}) }, event.status === 'queued_for_edit');
    } else if (event.type === 'failure' && (event.text || files?.length)) {
      const sentence = failureSentence(event.status, event.text);
      add({ id: record.cursor.toString(), type: event.type, text: sentence, turnId: event.turnId, files, ...(event.text.trim() && sentence !== event.text.trim() ? { detail: event.text.trim() } : {}) });
      next.appendToLast = false;
    } else if (BOUNDARY_TYPES.has(event.type) || (event.type === 'activity' && event.status === 'tool')) {
      next.appendToLast = false;
    }
  }
  if (messages) next.messages = messages;
  if (subagents) next.subagents = subagents.result();
  if (requests) next.requests = requests;
  if (pending) next.pending = pending;
  if (activity.length) {
    // Bound only what is retained: a long replay drops most activity anyway.
    const room = ACTIVITY - activity.length;
    const kept = [...(room > 0 ? state.activity.slice(-room) : []), ...activity.slice(-ACTIVITY).map(boundedActivity)];
    if (state.activity.length + activity.length > ACTIVITY) next.activityTruncated = true;
    next.activity = kept;
  }
  next.recent = fresh.length >= RECENT ? fresh.slice(-RECENT) : [...state.recent, ...fresh].slice(-RECENT);
  return next;
}
/** How far back a tool result looks for its call: past the calls of a long parallel run. */
const TOOL_LOOKBACK = 24;
function foldTool(messages: TranscriptMessage[], record: JournalEvent, list: () => TranscriptMessage[], replaceAt: (index: number, message: TranscriptMessage) => void, add: (message: TranscriptMessage) => void, owned: Set<TranscriptMessage>, stops?: TurnStops) {
  const event = record.event!;
  const payload = providerData(event.providerDataJson);
  const id = payload.type === 'tool_result' ? payload.tool_use_id : payload.id;
  if (typeof id !== 'string' || !id) return;
  const parsed = Date.parse(record.capturedAt), at = Number.isFinite(parsed) ? parsed : undefined;
  for (let i = messages.length - 1; i >= Math.max(0, messages.length - TOOL_LOOKBACK); i--) {
    const message = messages[i];
    if (message.type === 'operation_accepted' && !message.steered) break;
    const index = message.tools?.findIndex(t => t.id === id) ?? -1;
    if (index < 0) continue;
    const applied = applyToolPayload(message.tools![index], payload, at);
    if (!applied) return;
    const entry = stoppedEntry(applied, stops);
    if (owned.has(message)) message.tools![index] = entry;
    else { const tools = [...message.tools!]; tools[index] = entry; replaceAt(i, { ...message, tools }); }
    return;
  }
  // A call starts with its name; a hidden one (a question, a sub-agent's card) gets no line.
  if (typeof payload.name !== 'string' && payload.type !== 'tool_use') return;
  const entry = applyToolPayload(undefined, payload, at);
  if (!entry || hiddenTool(entry.name)) return;
  const current = list(), last = current.at(-1);
  if (last?.type === 'tools' && last.turnId === event.turnId) {
    if (owned.has(last)) last.tools!.push(entry);
    else replaceAt(current.length - 1, { ...last, tools: [...last.tools!, entry] });
  } else add({ id: record.cursor.toString(), type: 'tools', text: '', turnId: event.turnId, tools: [entry] });
}
/** Events after which the agent is no longer thinking. */
const THINKING_ENDS = new Set(['activity', 'message_delta', 'tool_activity', 'turn_ended', 'session_closed', 'failure', 'approval', 'question']);
function startThinking(current: TranscriptMessage[], replaceAt: (index: number, message: TranscriptMessage) => void, add: (message: TranscriptMessage) => void, owned: Set<TranscriptMessage>, turnId: string, cursor: string, at: number | undefined) {
  const last = current.at(-1);
  if (last?.tools?.at(-1)?.name === THINKING && last.tools.at(-1)!.status === 'running') return;
  const entry: ToolEntry = { id: `thinking:${cursor}`, name: THINKING, status: 'running', startedAt: at };
  if (last?.type === 'tools' && last.turnId === turnId) {
    if (owned.has(last)) last.tools!.push(entry); else replaceAt(current.length - 1, { ...last, tools: [...last.tools!, entry] });
  } else add({ id: cursor, type: 'tools', text: '', turnId, tools: [entry] });
}
function endThinking(current: TranscriptMessage[], replaceAt: (index: number, message: TranscriptMessage) => void, owned: Set<TranscriptMessage>, at: number | undefined) {
  // Only the last line can be thinking: anything said or done after it ended it.
  const last = current.at(-1), thinking = last?.tools?.at(-1);
  if (!last || thinking?.name !== THINKING || thinking.status !== 'running') return;
  const done: ToolEntry = { ...thinking, status: 'completed', endedAt: at };
  const tools = [...last.tools!.slice(0, -1), done];
  if (owned.has(last)) last.tools = tools; else replaceAt(current.length - 1, { ...last, tools });
}
/**
 * At a turn's end a call still running never reported its result. `pastQueued`:
 * the turn had messages queued during it (Send now stopped it for one), so a
 * message is not where the turn began.
 */
function settleTools(messages: TranscriptMessage[], replaceAt: (index: number, message: TranscriptMessage) => void, owned: Set<TranscriptMessage>, pastQueued = false) {
  for (let i = messages.length - 1; i >= Math.max(0, messages.length - TOOL_LOOKBACK); i--) {
    const message = messages[i];
    if (message.type === 'operation_accepted' && !message.steered) { if (pastQueued) continue; break; }
    if (!message.tools?.some(t => t.status === 'running')) continue;
    const tools = message.tools.map(t => t.status === 'running' ? { ...t, status: 'unknown' as const } : t);
    if (owned.has(message)) message.tools = tools; else replaceAt(i, { ...message, tools });
  }
}
/** A sent message that is one of the agent's own slash commands (`/compact`, `/review-pr 12`). */
export function ranCommand(text: string, catalog: CommandCatalog | undefined): boolean {
  const input = parseSlash(text.trim());
  if (!input || !COMMAND_NAME.test(input.name) || input.args.includes('\n')) return false;
  const name = input.name.toLowerCase();
  return name === 'compact' || !!catalog?.commands.some(c => c.name.toLowerCase() === name || c.aliases?.some(a => a.toLowerCase() === name));
}
/**
 * A failure that only reports a stop Lazybox asked for: a generic provider
 * error, never a limit or a sign-in problem. Mirrors stopReport in the
 * Runner's engine.
 */
function stopReport(status = '', account = ''): boolean {
  return ['', 'unknown', 'provider_error', 'error_during_execution'].includes(status) && ['', 'unknown', 'available'].includes(account);
}
/**
 * The receipt a record carries: the operation and what became of it. The
 * Runner writes most in the record's receipt; a message the closing session
 * dropped from its queue is a "receipt" event of that operation, with
 * `canceled_before_delivery` as its status (engine.Close).
 */
function receiptOf(record: JournalEvent): { id: string; delivery: string } | undefined {
  if (record.receipt?.operationId) return { id: record.receipt.operationId, delivery: record.receipt.delivery };
  const event = record.event;
  if (event?.type === 'receipt' && event.operationId) return { id: event.operationId, delivery: event.status };
  return undefined;
}
/** A briefing update's event: Lazybox's new features, or only the person's changed instructions. */
function briefingOrigin(status: string): MessageOrigin {
  return { kind: status === 'instructions' ? 'instructions_update' : 'briefing_update', summary: '', detail: '', copy: '', from: '' };
}
/** The latest of the recent messages with this operation ID, or -1. */
function lastWith(messages: readonly TranscriptMessage[], operationId: string): number {
  for (let i = messages.length - 1; i >= Math.max(0, messages.length - 400); i--) if (messages[i].operationId === operationId) return i;
  return -1;
}
const ACTIVITY_TYPES = new Set(['tool_catalog', 'tool_activity', 'turn_ended', 'failure', 'session_closed']);
const BOUNDARY_TYPES = new Set(['turn_started', 'turn_ended', 'session_closed', 'approval', 'question', 'request_resolved']);

/**
 * Runner restarts are not interruptions: agents and commands keep running and
 * the next Runner re-attaches. What a restart can leave uncertain is said
 * plainly, in the conversation, where the user will look for it.
 */
export function runnerNotice(record: JournalEvent): string {
  return noticeOf(record)?.text || '';
}
function noticeOf(record: JournalEvent): { notice: RunnerNotice; text: string } | undefined {
  const event = record.event;
  if (record.receipt?.delivery === 'unknown_after_runner_restart')
    return { notice: 'delivery-unknown', text: 'The Runner restarted before it could confirm this message reached the agent. It was not sent again; resend it if it is still needed.' };
  if (event?.type === 'code_reverted' && event.text) return { notice: 'reverted', text: event.text };
  // Another lazybox comes first on agents' PATH (internal/agentshell): what Lazybox did about it.
  if (event?.type === 'cli_notice' && event.text) return { notice: 'cli', text: event.text };
  // A pull request this conversation delivered changed (runner/items.go): one quiet line.
  if (event?.type === 'pull_request' && event.text) return { notice: 'pull-request', text: event.text };
  // The agent did not survive while no Runner ran (a reboot): the Runner's own sentence says what was lost and what to do.
  if (event?.type === 'agent_lost')
    return { notice: 'agent-lost', text: event.text || 'Lazybox restarted after this agent stopped. Send a message to resume its session.' };
  // The Runner could not save (a full disk): its own failure, said once; the agent kept running.
  if (event?.type === 'storage_error' && event.text) return { notice: 'storage', text: event.text };
  // Something about the agent program itself, said once at its start: a Codex outside the versions Lazybox is tested with.
  if (event?.type === 'provider_notice' && event.text) return { notice: 'provider', text: event.text };
  if (event?.type === 'request_resolved' && event.status === 'outcome_unknown_after_restart')
    return { notice: 'outcome-unknown', text: 'The Runner restarted while a command the agent started was in flight. Its outcome is unknown, so it was not run again; the agent was told to check before retrying.' };
  return undefined;
}

/** The lost-connection line no re-attachment completed yet, since the person last said something. */
function openLoss(messages: readonly TranscriptMessage[]): number {
  for (let i = messages.length - 1; i >= Math.max(0, messages.length - 400); i--) {
    const message = messages[i];
    if (message.type === 'notice' && message.notice === 'connection-lost') return message.reconnected ? -1 : i;
    if (message.type === 'operation_accepted' && !message.origin) return -1;
  }
  return -1;
}

/** The restart marker still open: one since the person last said something. */
function restartMarker(messages: readonly TranscriptMessage[]): number {
  for (let i = messages.length - 1; i >= Math.max(0, messages.length - 400); i--) {
    const message = messages[i];
    if (message.type === 'reattached') return i;
    if (message.type === 'operation_accepted' && !message.origin) return -1;
  }
  return -1;
}

const oneLine = (text: string) => text.replace(/\s+/g, ' ').trim();
/** A recent message `match` accepts that says `line` (a bounded summary says its start). */
function saidBy(messages: readonly TranscriptMessage[], line: string, match: (message: TranscriptMessage) => boolean): number {
  const want = oneLine(line), prefix = want.endsWith('…') ? want.slice(0, -1) : '';
  for (let i = messages.length - 1; i >= Math.max(0, messages.length - 12); i--) {
    const message = messages[i];
    if (!match(message)) continue;
    const said = oneLine(message.origin?.summary || message.text);
    const saidPrefix = said.endsWith('…') ? said.slice(0, -1) : '';
    if (said === want || (prefix && said.startsWith(prefix)) || (saidPrefix && want.startsWith(saidPrefix))) return i;
  }
  return -1;
}

// Tool results can contain images or megabytes of logs. Keep a bounded display
// projection while the Runner journal owns the original evidence.
function boundedActivity(record:JournalEvent):JournalEvent {
 const event=record.event;if(!event||event.providerDataJson.length<=32768)return record;
 let raw:any;try{raw=JSON.parse(new TextDecoder().decode(event.providerDataJson));}catch{return {...record,event:{...event,providerDataJson:new Uint8Array()}};}
 const limit=(v:any,depth=0):any=>typeof v==='string'?v.length>4096?v.slice(0,4096)+'\n[Preview truncated]':v:depth>5?'[Nested content omitted]':Array.isArray(v)?v.slice(0,20).map(x=>limit(x,depth+1)):v&&typeof v==='object'?Object.fromEntries(Object.entries(v).slice(0,30).map(([k,x])=>[k,limit(x,depth+1)])):v;
 let data=new TextEncoder().encode(JSON.stringify(limit(raw)));
 if(data.length>32768){const item=raw.item;raw=item?{item:{id:item.id,type:item.type,tool:item.tool,status:item.status,exitCode:item.exitCode,result:'[Large payload omitted]'}}:{type:raw.type,id:raw.id,tool_use_id:raw.tool_use_id,name:raw.name,status:raw.status,is_error:raw.is_error,output:raw.output===undefined?undefined:'[Large payload omitted]',input:'[Large payload omitted]',content:'[Large payload omitted]'};data=new TextEncoder().encode(JSON.stringify(raw));}
 return {...record,event:{...event,providerDataJson:data}};
}

export function canRestartPrompt(message: TranscriptMessage): boolean {
  return message.type === 'operation_accepted' && !!message.operationId &&
    !message.origin && !message.command && !message.steered && !message.files?.length;
}
