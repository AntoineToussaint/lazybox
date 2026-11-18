import { create } from '@bufbuild/protobuf';
import { AgentEventSchema, SessionSchema, type Session } from '@lazybox/protocol';
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { ageLabel, buildInbox, defaultNotificationPreferences, deriveAttention, lastLine, markSeen, noticeFor, parseSeen, seenAt, seenMark, type Seen } from '../packages/surfaces/src/attention.ts';

const json = (value: unknown) => new TextEncoder().encode(JSON.stringify(value));
type Agent = { id?: string; status?: string; updated?: number; ended?: number; lastTurnStatus?: string; parentId?: string };
function session(id: string, init: Omit<Partial<Session>, 'agents' | 'createdAtUnixMillis'> & { agents?: Agent[]; created?: number } = {}): Session {
  const { agents = [], created = 1_000, ...rest } = init;
  return create(SessionSchema, {
    id, conversationId: `${id}-conversation`, collectionId: 'alpha', title: `Title ${id}`, provider: 'claude', model: 'claude-fable-5', status: 'ready',
    createdAtUnixMillis: BigInt(created),
    agents: agents.map(agent => ({ id: agent.id ?? 'root', parentId: agent.parentId ?? '', status: agent.status ?? 'done', lastTurnStatus: agent.lastTurnStatus ?? '', updatedAtUnixMillis: BigInt(agent.updated ?? 0), turnEndedAtUnixMillis: BigInt(agent.ended ?? 0) })),
    ...rest,
  });
}
const none: Seen = { since: 0, marks: {} };
const question = create(AgentEventSchema, { type: 'question', requestId: 'q1', providerDataJson: json({ questions: [{ question: 'Which **database** should I use?' }] }) });
const approval = create(AgentEventSchema, { type: 'approval', requestId: 'a1', text: 'Bash', providerDataJson: json({ input: { command: 'npm test' } }) });

test('a pending question or approval needs input, whatever was seen', () => {
  const asked = deriveAttention(session('s', { status: 'waiting_input', pending: [question], agents: [{ updated: 5_000 }] }), 9_999_999);
  assert.equal(asked.state, 'needs-input');
  assert.equal(asked.unread, true);
  assert.equal(asked.reason, 'Question');
  assert.equal(asked.line, 'Which database should I use?');
  assert.deepEqual(asked.requests, ['q1']);
  const approve = deriveAttention(session('s', { status: 'waiting_approval', pending: [approval] }));
  assert.equal(approve.reason, 'Approval');
  assert.equal(approve.line, 'Run npm test');
  // A closed conversation cannot answer: its stale requests do not count.
  assert.equal(deriveAttention(session('s', { closed: true, status: 'stopped', pending: [question] })).state, 'idle');
});

test('faults, failed deliveries and provider limits are failures; an unconfirmed exit is not', () => {
  const delivery = deriveAttention(session('s', { fault: 'provider delivery failed; reconcile before continuing', status: 'failed', agents: [{ status: 'failed', updated: 7_000 }] }), 6_000);
  assert.equal(delivery.state, 'failed');
  assert.equal(delivery.unread, true);
  assert.equal(delivery.line, 'A message could not be delivered to the agent');
  assert.equal(deriveAttention(session('s', { fault: 'provider delivery failed; reconcile before continuing', agents: [{ updated: 7_000 }] }), 7_000).unread, false, 'seen failure stays failed but read');
  const limit = deriveAttention(session('s', { fault: 'usage_limit', account: { state: 'exhausted' } }));
  assert.equal(limit.line, 'Subscription limit reached');
  const unconfirmed = deriveAttention(session('s', { closed: true, fault: 'native_exit_unconfirmed', status: 'unknown' }));
  assert.equal(unconfirmed.state, 'idle');
});

test('a Runner restart is a failure only when it interrupted a running turn', () => {
  const interrupted = deriveAttention(session('s', { historical: true, closed: true, status: 'interrupted', agents: [{ status: 'interrupted', updated: 4_000 }] }));
  assert.equal(interrupted.state, 'failed');
  assert.equal(interrupted.reason, 'Interrupted');
  // Restored after a restart with nothing running: the root kept its finished turn.
  const nothing = session('s', { historical: true, closed: true, status: 'interrupted', agents: [{ status: 'done', updated: 4_000, ended: 4_000 }] });
  assert.equal(deriveAttention(nothing, 4_000).state, 'idle');
});

test('working, ready for review until seen, then done', () => {
  assert.equal(deriveAttention(session('s', { busy: true, status: 'running', agents: [{ status: 'running', updated: 3_000 }] })).state, 'working');
  assert.equal(deriveAttention(session('s', { queued: 1 })).state, 'working');
  const finished = session('s', { status: 'done', lastMessage: 'Changed three files.\n\n**All tests pass.**', agents: [{ status: 'done', updated: 9_000, ended: 8_000 }] });
  const review = deriveAttention(finished, 7_000);
  assert.equal(review.state, 'review');
  assert.equal(review.line, 'All tests pass.');
  assert.equal(seenMark(review), 9_000);
  assert.equal(deriveAttention(finished, 8_000).state, 'idle', 'a later status report alone does not make a seen turn new');
  // Runners before turn_ended_at: a finished turn falls back to its update time.
  const older = session('s', { status: 'done', agents: [{ status: 'done', lastTurnStatus: 'completed', updated: 9_000 }] });
  assert.equal(deriveAttention(older, 8_999).state, 'review');
  assert.equal(deriveAttention(session('s', { status: 'ready', agents: [{ status: 'ready', updated: 9_000 }] }), 0).state, 'idle', 'no turn ended yet');
});

test('the inbox has one row per conversation, grouped by need, newest first, without assistants', () => {
  const inbox = buildInbox([
    session('idle-old', { agents: [{ updated: 1_000, ended: 1_000 }] }),
    session('working', { busy: true, status: 'running', agents: [{ status: 'running', updated: 5_000 }] }),
    session('review-new', { agents: [{ updated: 6_000, ended: 6_000 }] }),
    session('review-old', { agents: [{ updated: 3_000, ended: 3_000 }] }),
    session('asks', { pending: [question], status: 'waiting_input', agents: [{ updated: 2_000 }] }),
    session('broken', { fault: 'delivery_failed', agents: [{ updated: 4_000 }] }),
    session('helper', { purpose: 'help', pending: [question] }),
  ], { since: 0, marks: { 'idle-old-conversation': 1_000 } });
  assert.deepEqual(inbox.rows.map(row => row.session.id), ['asks', 'broken', 'review-new', 'review-old', 'working', 'idle-old']);
  assert.deepEqual(inbox.groups.map(group => group.state), ['needs-input', 'failed', 'review', 'working', 'idle']);
  assert.equal(inbox.counts['needs-input'], 1);
  assert.equal(inbox.counts.review, 2);
  assert.equal(inbox.unread, 4);
});

test('seen marks only move forward; the first look and pruning never flood the inbox', () => {
  let seen: Seen = { since: 100, marks: {} };
  seen = markSeen(seen, 'a', 500);
  assert.equal(seenAt(seen, 'a'), 500);
  assert.equal(markSeen(seen, 'a', 400), seen, 'an older mark is ignored');
  assert.equal(markSeen(seen, 'b', 50), seen, 'before `since` everything is already seen');
  assert.equal(seenAt(seen, 'unknown'), 100);
  // A conversation that finished before the inbox existed is not "ready for review".
  const before = buildInbox([session('s', { agents: [{ updated: 90, ended: 90 }] })], { since: 100, marks: {} });
  assert.equal(before.rows[0].state, 'idle');
  let many: Seen = { since: 0, marks: {} };
  for (let i = 1; i <= 2001; i++) many = markSeen(many, `c${i}`, i);
  assert.equal(Object.keys(many.marks).length, 2000);
  assert.equal(many.since, 1, 'the dropped oldest mark is covered by since');
  assert.equal(seenAt(many, 'c1'), 1);
  assert.deepEqual(parseSeen('{broken', 42), { since: 42, marks: {} });
  assert.deepEqual(parseSeen(null, 42), { since: 42, marks: {} });
  assert.deepEqual(parseSeen(JSON.stringify({ since: 7, marks: { a: 9, bad: 'x' } }), 42), { since: 7, marks: { a: 9 } });
});

test('notifications: only watched transitions with new agent activity, never for the conversation on screen', () => {
  const prefs = defaultNotificationPreferences;
  const working = deriveAttention(session('s', { busy: true, status: 'running', agents: [{ status: 'running', updated: 1_000 }] }));
  const asks = deriveAttention(session('s', { status: 'waiting_input', pending: [question], agents: [{ status: 'waiting_input', updated: 2_000 }] }));
  assert.equal(noticeFor(undefined, asks, prefs, false), undefined, 'the first observation is a baseline');
  const notice = noticeFor(working, asks, prefs, false, 'Alpha feature');
  assert.equal(notice?.kind, 'needs-input');
  assert.equal(notice?.title, 'Question · Title s');
  assert.equal(notice?.body, 'Alpha feature · Which database should I use?');
  assert.equal(notice?.sessionId, 's');
  assert.equal(notice?.speech?.requestId, 'q1');
  assert.match(notice?.speech?.text || '', /Which \*\*database\*\* should I use\?/);
  assert.equal(noticeFor(working, asks, prefs, true), undefined, 'on screen');
  assert.equal(noticeFor(working, asks, { ...prefs, needsInput: false }), undefined);
  assert.equal(noticeFor(asks, asks, prefs, false), undefined, 'the same request is announced once');
  const second = deriveAttention(session('s', { status: 'waiting_approval', pending: [question, approval], agents: [{ updated: 2_500 }] }));
  const nextNotice = noticeFor(asks, second, prefs, false);
  assert.equal(nextNotice?.kind, 'needs-input', 'a new request while one waits');
  assert.equal(nextNotice?.speech?.requestId, 'a1');
  assert.match(nextNotice?.speech?.text || '', /Approval needed. Run npm test/);
  assert.notEqual(nextNotice?.speech?.id, notice?.speech?.id);

  const idle = deriveAttention(session('s', { agents: [{ status: 'done', updated: 1_000, ended: 1_000 }] }), 1_000);
  const relabelled = deriveAttention(session('s', { fault: 'provider_failure', agents: [{ status: 'done', updated: 1_000, ended: 1_000 }] }));
  assert.equal(noticeFor(idle, relabelled, prefs, false), undefined, 'a fault without new activity (a restart) is quiet');
  const failed = deriveAttention(session('s', { fault: 'provider_failure', agents: [{ status: 'failed', updated: 3_000 }] }));
  assert.equal(noticeFor(working, failed, prefs, false)?.kind, 'failed');
  const restartedIdle = deriveAttention(session('s', { historical: true, closed: true, status: 'interrupted', agents: [{ status: 'done', updated: 1_000, ended: 1_000 }] }), 1_000);
  assert.equal(noticeFor(idle, restartedIdle, prefs, false), undefined, 'a restart that interrupted nothing');
  const restartedWorking = deriveAttention(session('s', { historical: true, closed: true, status: 'interrupted', agents: [{ status: 'interrupted', updated: 1_000 }] }));
  assert.equal(noticeFor(working, restartedWorking, prefs, false)?.title, 'Interrupted · Title s');

  const review = deriveAttention(session('s', { lastMessage: 'Done.', agents: [{ status: 'done', updated: 4_000, ended: 4_000 }] }), 1_000);
  assert.equal(noticeFor(working, review, prefs, false), undefined, 'ready for review is opt-in');
  assert.equal(noticeFor(working, review, { ...prefs, review: true }, false)?.body, 'Done.');
  assert.equal(noticeFor(working, review, { ...prefs, review: true }, false)?.speech, undefined);
});

test('rows read well: last line and age', () => {
  assert.equal(lastLine('Intro\n\n- **Fixed** the `parser`\n\n```\n'), 'Fixed the parser');
  assert.equal(lastLine('x'.repeat(200), 20), 'x'.repeat(19) + '…');
  assert.equal(ageLabel(0, 10), '');
  assert.equal(ageLabel(1_000, 20_000), 'now');
  assert.equal(ageLabel(0 + 1, 5 * 60_000), '5m');
  assert.equal(ageLabel(1, 3 * 3_600_000), '3h');
  assert.equal(ageLabel(1, 2 * 86_400_000), '2d');
});
