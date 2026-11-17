// Opening a conversation: its latest turns from one ReadSessionTail read, then
// the earlier history in pages, and the whole conversation folded from its
// first record once that is in. These check the reader against one fold of
// the whole journal, with what a fold carries across turns (sub-agents, the
// slash-command catalog, questions answered later, review cards) on both
// sides of where the tail starts.
import test from 'node:test';
import assert from 'node:assert/strict';
import { create } from '@bufbuild/protobuf';
import { Code, ConnectError } from '@connectrpc/connect';
import { JournalEventSchema, type JournalEvent } from '@lazybox/protocol';
import type { RunnerClient } from '@lazybox/client-core';
import { emptyTranscript, foldTranscript } from '../packages/surfaces/src/transcript-model.ts';
import { ConversationReader, conversationReader, continueConversation, forgetConversation, TAIL_TURNS } from '../packages/surfaces/src/transcript-cache.ts';
import { SessionSchema } from '@lazybox/protocol';
// @ts-ignore: a plain module shared with the fixture Runner
import { longConversation, deliveredQueuedTurn } from './fixtures/long-conversation.mjs';
// @ts-ignore
import { tailPage } from './fixtures/session-tail.mjs';

const json = (value: unknown) => Buffer.from(JSON.stringify(value)).toString('base64');
/** Fixture events (base64 provider data, a receipt beside) as the Runner's records 1…n. */
const records = (events: any[], from = 1) => events.map(({ receipt, providerDataJson, capturedAt, ...event }, i) => create(JournalEventSchema, {
  cursor: BigInt(from + i), capturedAt: capturedAt || new Date(Date.parse('2026-11-17T09:00:00Z') + (from + i) * 1000).toISOString(), ...(receipt ? { receipt } : {}),
  event: { ...event, providerDataJson: providerDataJson ? Buffer.from(providerDataJson, 'base64') : new Uint8Array() },
}));
const size = (record: JournalEvent) => 120 + (record.event?.text.length ?? 0) + (record.event?.providerDataJson.length ?? 0);
const sleep = (ms: number) => new Promise(resolve => setTimeout(resolve, ms));
const until = async (check: () => boolean, ms = 4000) => { const end = Date.now() + ms; while (!check()) { if (Date.now() > end) throw new Error('timed out'); await sleep(5); } };
/**
 * A transcript to compare: the records it keeps (recent, activity) without
 * `retained`, which says when each was read, not what it is.
 */
const comparable = (transcript: ReturnType<typeof emptyTranscript>) => ({ ...transcript, recent: transcript.recent.map(r => ({ ...r, retained: 0n })), activity: transcript.activity.map(r => ({ ...r, retained: 0n })) });
/** The transcript of a reader's snapshot, without the reader's own status. */
const transcriptOf = ({ status, error, loading, replay, partial, ...transcript }: ReturnType<ConversationReader['getSnapshot']>) => comparable(transcript);
const whole = (journal: JournalEvent[]) => comparable(foldTranscript(emptyTranscript(), journal));

/**
 * A Runner serving `journal` (records can be added with `add`): ReadSessionTail
 * pages it like internal/runner/timeline.go, WatchEvents replays and follows it.
 * `hold` keeps earlier pages back until released; `fail` makes the next calls
 * of a method fail with a code.
 */
function runner(journal: JournalEvent[], options: { tail?: boolean } = {}) {
  const calls: { method: string; before?: bigint; after?: bigint }[] = [];
  let wake: () => void = () => {};
  let hold: Promise<void> | undefined, release = () => {};
  const failures: Record<string, Code[]> = { tail: [], page: [], watch: [] };
  const fail = (kind: keyof typeof failures) => { const code = failures[kind].shift(); if (code !== undefined) throw new ConnectError(`fixture ${kind} failure`, code); };
  const withRetained = (record: JournalEvent, retained: number) => create(JournalEventSchema, { ...record, retained: BigInt(Math.max(retained, Number(record.cursor))) });
  const api = {
    async readSessionTail({ before = 0n, turns = 0, maxBytes = 0 }: { before?: bigint; turns?: number; maxBytes?: number }) {
      calls.push({ method: 'ReadSessionTail', before });
      if (options.tail === false) throw new ConnectError('unknown method', Code.Unimplemented);
      if (before === 0n) fail('tail'); else { if (hold) await hold; fail('page'); }
      const page = tailPage(journal, { before: Number(before), turns, maxBytes }, size);
      if (!page) throw new ConnectError('cursor outside retained range', Code.OutOfRange);
      return { events: page.events.map((record: JournalEvent) => withRetained(record, page.retained)), start: BigInt(page.start), retained: BigInt(page.retained) };
    },
    async *watchEvents({ after }: { after: bigint }, { signal }: { signal: AbortSignal }) {
      calls.push({ method: 'WatchEvents', after });
      fail('watch');
      let cursor = after;
      while (!signal.aborted) {
        const retained = journal.length;
        for (const record of journal.slice(Number(cursor))) { yield withRetained(record, retained); cursor = record.cursor; }
        await new Promise<void>(resolve => { wake = resolve; signal.addEventListener('abort', () => resolve(), { once: true }); });
      }
    },
  } as unknown as RunnerClient;
  return {
    api, calls, failures,
    add(events: any[]) { journal.push(...records(events, journal.length + 1)); wake(); },
    hold() { hold = new Promise(resolve => { release = resolve; }); },
    release() { hold = undefined; release(); },
  };
}

/**
 * A long conversation whose last turns depend on its first: a background
 * sub-agent spawned early reports and finishes late, a question asked early
 * is answered late, a slash command the early catalog named is run late, and
 * a review started early finishes late.
 */
function acrossTurns() {
  const turn = (n: number, text: string) => [
    { type: 'operation_accepted', operationId: `op-${n}`, text },
    { type: 'turn_started', turnId: `t-${n}` },
    { type: 'message_delta', text: `Answer ${n}.`, turnId: `t-${n}` },
    { type: 'turn_ended', status: 'completed', turnId: `t-${n}` },
  ];
  const review = (fields: Record<string, unknown>) => json({ review: 'rv-1', workspace: 'alpha', reviewerSession: 'review-1', reviewerLabel: 'Codex', scopeLabel: 'Turn 1', ...fields });
  const child = 'claude-tool:toolu_bg';
  return [
    ...longConversation({ turns: 40, toolOutputBytes: 2000 }),
    { type: 'command_catalog', providerDataJson: json({ commands: [{ name: 'review-pr', description: 'Review a PR', argumentHint: '<n>' }] }) },
    { type: 'operation_accepted', operationId: 'op-start', text: 'Find the flaky test in the background' },
    { type: 'turn_started', turnId: 't-start' },
    { type: 'tool_activity', turnId: 't-start', providerDataJson: json({ type: 'tool_call', id: 'toolu_bg', name: 'Task', status: 'running', input: { description: 'Find flaky tests', prompt: 'Look.', subagent_type: 'Explore', run_in_background: true } }) },
    { type: 'agent_status', agentId: child, parentAgentId: 'root', nativeId: 'task-1', status: 'running', name: 'Find flaky tests', agentKind: 'local_agent', providerDataJson: json({ toolUseId: 'toolu_bg' }) },
    { type: 'question', requestId: 'q-1', turnId: 't-start', providerDataJson: json({ questions: [{ question: 'Which suite?', header: 'Suite', options: [{ label: 'unit' }, { label: 'e2e' }] }] }) },
    { type: 'review_started', text: 'Codex is reviewing turn 1', providerDataJson: review({}) },
    { type: 'turn_ended', status: 'completed', turnId: 't-start' },
    ...Array.from({ length: TAIL_TURNS - 1 }, (_, i) => turn(100 + i, `Meanwhile ${i}`)).flat(),
    // The last turn: everything that depends on the first.
    { type: 'operation_accepted', operationId: 'op-last', text: '/review-pr 12' },
    { type: 'message_delta', agentId: child, parentAgentId: 'root', text: 'Found two flaky tests.' },
    { type: 'turn_ended', agentId: child, parentAgentId: 'root', status: 'completed' },
    { type: 'operation_accepted', operationId: 'op-answer', requestId: 'q-1', providerDataJson: json({ response: { kind: 'answer', answers: { 'Which suite?': ['e2e'] } } }) },
    { type: 'review_finished', text: 'Codex reviewed turn 1 · 1 blocker', providerDataJson: review({ blockers: 1 }) },
    { type: 'message_delta', text: 'LAST_ANSWER', turnId: 't-last' },
  ];
}

test('the latest turns show first; the whole conversation, once read, is one fold of every record', async () => {
  const journal = records(acrossTurns());
  const fixture = runner(journal);
  fixture.hold();
  const reader = new ConversationReader(fixture.api, 'long');
  const stop = reader.subscribe(() => {});
  try {
    // From one read: the latest turns, while the earlier history is read.
    await until(() => reader.getSnapshot().messages.length > 0);
    const preview = reader.getSnapshot();
    assert.equal(preview.partial, true);
    assert.equal(preview.loading, true, 'a preview is still loading');
    assert.equal(preview.messages.at(-1)!.text, 'LAST_ANSWER');
    const tail = fixture.calls[0];
    assert.equal(tail.method, 'ReadSessionTail');
    const watch = fixture.calls.find(call => call.method === 'WatchEvents')!;
    assert.equal(watch.after, journal.at(-1)!.cursor, 'it follows the conversation from the tail’s last record');
    assert.ok(preview.messages.length < whole(journal).messages.length / 4, 'only the latest turns are folded');
    assert.ok(preview.messages[0].text.startsWith('Meanwhile'), `the tail starts with a turn: ${preview.messages[0].text}`);
    // What a preview cannot know without the earlier history: the sub-agent's card
    // sits early; the late answer names the early question.
    assert.notDeepEqual(transcriptOf(preview), whole(journal));

    // Live records arrive while the earlier history is read.
    fixture.add([{ type: 'message_delta', text: ' LIVE_ONE', turnId: 't-last' }]);
    await until(() => reader.getSnapshot().messages.at(-1)!.text.endsWith('LIVE_ONE'));
    fixture.release();
    await until(() => !reader.getSnapshot().partial);
    fixture.add([{ type: 'message_delta', text: ' LIVE_TWO', turnId: 't-last' }]);
    await until(() => reader.getSnapshot().messages.at(-1)!.text.endsWith('LIVE_TWO'));
    reader.flush();
    const done = reader.getSnapshot();
    assert.equal(done.loading, false);
    // Exactly one fold of every record, the live ones once each.
    assert.deepEqual(transcriptOf(done), whole(journal));
    const pages = fixture.calls.filter(call => call.method === 'ReadSessionTail' && call.before! > 0n);
    assert.ok(pages.length >= 2, `earlier history read in pages (${pages.length})`);
    for (let i = 1; i < pages.length; i++) assert.ok(pages[i].before! < pages[i - 1].before!, 'pages are read backward, each before the last');
    // Across turns: one card where the sub-agent started, its report in it; the
    // late answer names the early question; the late slash command is one.
    assert.equal(done.messages.filter(m => m.type === 'subagent').length, 1);
    assert.equal(done.subagents.byId['claude-tool:toolu_bg'].status, 'done');
    assert.ok(done.messages.find(m => m.type === 'answered')!.answered!.kind === 'answered');
    assert.equal(done.messages.find(m => m.text === '/review-pr 12')!.command, true);
    assert.equal(done.messages.filter(m => m.type === 'review').length, 1);
  } finally { stop(); }
});

test('a short conversation is its tail: one read, then the live stream after it', async () => {
  const journal = records(longConversation({ turns: 2, toolOutputBytes: 100 }));
  const fixture = runner(journal);
  const reader = new ConversationReader(fixture.api, 'short');
  const stop = reader.subscribe(() => {});
  try {
    await until(() => reader.getSnapshot().messages.length > 0);
    const state = reader.getSnapshot();
    assert.equal(state.partial, undefined);
    assert.equal(state.loading, false);
    assert.deepEqual(transcriptOf(state), whole(journal));
    assert.deepEqual(fixture.calls.map(call => call.method), ['ReadSessionTail', 'WatchEvents']);
    assert.equal(fixture.calls[1].after, journal.at(-1)!.cursor);
  } finally { stop(); }
});

test('a Runner restart while a conversation loads: every read resumes, nothing is missed or repeated', async () => {
  const history = longConversation({ turns: 30, toolOutputBytes: 1000 });
  const journal = records([...history, ...deliveredQueuedTurn().events]);
  const fixture = runner(journal);
  // The restart: the first earlier page and the live stream fail Unavailable once.
  fixture.failures.page.push(Code.Unavailable);
  fixture.failures.watch.push(Code.Unavailable);
  const reader = new ConversationReader(fixture.api, 'restarted');
  const stop = reader.subscribe(() => {});
  try {
    await until(() => !reader.getSnapshot().partial && !reader.getSnapshot().loading, 8000);
    fixture.add([{ type: 'message_delta', text: 'AFTER_RESTART' }]);
    await until(() => reader.getSnapshot().messages.at(-1)!.text.endsWith('AFTER_RESTART'));
    reader.flush();
    const state = reader.getSnapshot();
    assert.equal(state.error, '');
    assert.deepEqual(transcriptOf(state), whole(journal));
    const watches = fixture.calls.filter(call => call.method === 'WatchEvents');
    assert.ok(watches.length >= 2 && watches.every(call => call.after === watches[0].after), 'the live stream reconnects from the same cursor');
  } finally { stop(); }
});

test('a Runner without ReadSessionTail: the conversation opens by its replay, as before', async () => {
  const journal = records(longConversation({ turns: 12, toolOutputBytes: 100 }));
  const fixture = runner(journal, { tail: false });
  const reader = new ConversationReader(fixture.api, 'older');
  const stop = reader.subscribe(() => {});
  try {
    await until(() => !reader.getSnapshot().loading);
    assert.deepEqual(fixture.calls.map(call => [call.method, call.after]), [['ReadSessionTail', undefined], ['WatchEvents', 0n]]);
    assert.deepEqual(transcriptOf(reader.getSnapshot()), whole(journal));
  } finally { stop(); }
});

test('a conversation closed while loading is still completed when it is opened again', async () => {
  const journal = records(longConversation({ turns: 20, toolOutputBytes: 1000 }));
  const fixture = runner(journal);
  fixture.hold();
  const reader = new ConversationReader(fixture.api, 'reopened');
  let stop = reader.subscribe(() => {});
  await until(() => reader.getSnapshot().partial === true);
  // Navigated away mid-load, back again later: no second tail, the earlier history goes on.
  stop();
  fixture.release();
  stop = reader.subscribe(() => {});
  try {
    await until(() => !reader.getSnapshot().partial);
    assert.equal(fixture.calls.filter(call => call.method === 'ReadSessionTail' && !call.before).length, 1);
    assert.deepEqual(transcriptOf(reader.getSnapshot()), whole(journal));
  } finally { stop(); }
});

test('a journal that no longer has the earlier history says so, and Retry reads it again from the tail', async () => {
  const journal = records(longConversation({ turns: 20, toolOutputBytes: 1000 }));
  const fixture = runner(journal);
  fixture.failures.page.push(Code.OutOfRange);
  const reader = new ConversationReader(fixture.api, 'rewritten');
  const stop = reader.subscribe(() => {});
  try {
    await until(() => !!reader.getSnapshot().error);
    assert.equal(reader.getSnapshot().loading, false);
    reader.retry();
    await until(() => !reader.getSnapshot().partial && !reader.getSnapshot().loading && !reader.getSnapshot().error);
    assert.deepEqual(transcriptOf(reader.getSnapshot()), whole(journal));
  } finally { stop(); }
});

test('a window keeps one reader per conversation and Runner, forgets a deleted one, and never continues from a preview', async () => {
  const journal = records(longConversation({ turns: 20, toolOutputBytes: 1000 }));
  const dev = runner(journal), working = runner(journal);
  // Two Runners of one window (Dev and Working): never one reader for both.
  const fromDev = conversationReader(dev.api, 'same-id');
  assert.equal(conversationReader(dev.api, 'same-id'), fromDev, 'switching back is instant: the same reader, already folded');
  assert.notEqual(conversationReader(working.api, 'same-id'), fromDev);
  // A deleted conversation is forgotten: its transcript is not kept.
  forgetConversation(dev.api, 'same-id');
  assert.notEqual(conversationReader(dev.api, 'same-id'), fromDev);
  // A successor continues a whole conversation's transcript, never a preview of its tail.
  dev.hold();
  const previous = conversationReader(dev.api, 'first-attempt');
  const stop = previous.subscribe(() => {});
  try {
    await until(() => previous.getSnapshot().partial === true);
    const next = continueConversation(dev.api, 'first-attempt', create(SessionSchema, { id: 'second-attempt', previousExecutionId: 'first-attempt' }));
    assert.equal(next.getSnapshot().cursor, 0n, 'a successor of a preview reads its own timeline');
    dev.release();
    await until(() => !previous.getSnapshot().partial);
    const after = continueConversation(dev.api, 'first-attempt', create(SessionSchema, { id: 'third-attempt', previousExecutionId: 'first-attempt' }));
    assert.equal(after.getSnapshot().cursor, previous.getSnapshot().cursor);
  } finally { stop(); }
});
