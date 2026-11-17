import { create } from '@bufbuild/protobuf';
import { AgentEventSchema, SessionSchema, type Session } from '@lazybox/protocol';
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { buildInbox, type Seen } from '../packages/surfaces/src/attention.ts';
import { agentSections, isArchivedAway, liveSubagents, matchesFilter, metaLine, visibleRows } from '../packages/surfaces/src/agents-model.ts';

type Agent = { id?: string; parentId?: string; name?: string; status?: string; phase?: string; tool?: string; since?: number; updated?: number; ended?: number };
function session(id: string, init: Omit<Partial<Session>, 'agents'> & { agents?: Agent[]; pinned?: number; archived?: number } = {}): Session {
  const { agents = [{ updated: 1_000 }], pinned = 0, archived = 0, ...rest } = init;
  return create(SessionSchema, {
    id, conversationId: id, collectionId: 'alpha', title: `Title ${id}`, provider: 'claude', model: 'claude-fable-5', status: 'ready', createdAtUnixMillis: 1_000n,
    pinnedAtUnixMillis: BigInt(pinned), archivedAtUnixMillis: BigInt(archived),
    agents: agents.map(agent => ({ id: agent.id ?? 'root', parentId: agent.parentId ?? '', name: agent.name ?? '', status: agent.status ?? 'done', phase: agent.phase ?? '', tool: agent.tool ?? '', phaseSinceUnixMillis: BigInt(agent.since ?? 0), updatedAtUnixMillis: BigInt(agent.updated ?? 0), turnEndedAtUnixMillis: BigInt(agent.ended ?? 0) })),
    ...rest,
  });
}
const none: Seen = { since: 0, marks: {} };
const names: Record<string, string> = { alpha: 'Alpha feature', beta: 'Beta release' };
const workspace = (row: { session: Session }) => names[row.session.collectionId] || 'No workspace';
const question = create(AgentEventSchema, { type: 'question', requestId: 'q1', text: 'Which version?' });

const sessions = [
  session('asks', { collectionId: 'beta', status: 'waiting_input', pending: [question] }),
  session('works', { status: 'running', busy: true, agents: [{ status: 'running', updated: 5_000 }] }),
  session('done', { agents: [{ updated: 2_000, ended: 2_000 }], lastMessage: 'All tests pass.' }),
  session('pinned-late', { collectionId: 'beta', pinned: 900 }),
  session('pinned-early', { pinned: 100 }),
  session('archived', { archived: 50 }),
  session('archived-asks', { collectionId: 'beta', archived: 50, status: 'waiting_approval', pending: [question] }),
];
const inbox = buildInbox(sessions, { since: 10_000, marks: {} });

test('pinned conversations lead, oldest pin first, then the Inbox order', () => {
  const sections = agentSections(inbox, 'priority', workspace);
  assert.deepEqual(sections.map(section => section.id), ['pinned', 'needs-input', 'working', 'idle']);
  assert.deepEqual(sections[0].rows.map(row => row.session.id), ['pinned-early', 'pinned-late']);
  assert.deepEqual(sections[1].rows.map(row => row.session.id).sort(), ['archived-asks', 'asks']);
});

test('an archived conversation leaves the list, unless it waits for the person', () => {
  const ids = agentSections(inbox, 'priority', workspace).flatMap(section => section.rows.map(row => row.session.id));
  assert.ok(!ids.includes('archived'));
  assert.ok(ids.includes('archived-asks'));
  assert.equal(isArchivedAway(sessions[5]), true);
  assert.equal(isArchivedAway(sessions[6]), false);
});

test('by workspace, each workspace is a section in urgency order', () => {
  const sections = agentSections(inbox, 'workspace', workspace);
  assert.deepEqual(sections.map(section => section.title), ['Pinned', 'Beta release', 'Alpha feature']);
  assert.deepEqual(sections[1].rows.map(row => row.session.id), ['archived-asks', 'asks']);
});

test('the filter matches title, workspace, reason and every word', () => {
  const asks = inbox.rows.find(row => row.session.id === 'asks')!;
  assert.equal(matchesFilter(asks, 'Beta release', 'beta'), true);
  assert.equal(matchesFilter(asks, 'Beta release', 'title ASKS'), true);
  assert.equal(matchesFilter(asks, 'Beta release', 'question'), true);
  assert.equal(matchesFilter(asks, 'Beta release', 'alpha'), false);
  const sections = agentSections(inbox, 'priority', workspace, 'pinned');
  assert.deepEqual(sections.flatMap(section => section.rows.map(row => row.session.id)), ['pinned-early', 'pinned-late']);
  assert.deepEqual(agentSections(inbox, 'priority', workspace, 'nothing like this'), []);
});

test('folded sections are skipped by the keyboard walk', () => {
  const sections = agentSections(inbox, 'priority', workspace);
  assert.deepEqual(visibleRows(sections, ['pinned', 'idle']).map(row => row.session.id).sort(), ['archived-asks', 'asks', 'works']);
});

test('only sub-agents still at work nest under their conversation, newest first', () => {
  const parent = session('p', { status: 'running', busy: true, agents: [
    { status: 'running' },
    { id: 'a', parentId: 'root', name: 'Find the flaky test', status: 'running', phase: 'tool', tool: 'Bash', since: 3_000 },
    { id: 'b', parentId: 'root', name: 'Read the docs', status: 'waiting_approval', since: 4_000 },
    { id: 'c', parentId: 'root', name: 'Finished', status: 'done', since: 5_000 },
  ] });
  const rows = liveSubagents(parent);
  assert.deepEqual(rows.map(row => [row.id, row.tone]), [['b', 'waiting'], ['a', 'busy']]);
  assert.equal(rows[1].name, 'Find the flaky test');
  assert.notEqual(rows[1].detail, 'Working', 'a tool step says which tool');
  assert.deepEqual(liveSubagents(session('closed', { closed: true, agents: [{ id: 'a', parentId: 'root', status: 'running' }] })), []);
  // The filter finds a conversation by its sub-agent's task.
  const row = buildInbox([parent], none).rows[0];
  assert.equal(matchesFilter(row, '', 'flaky'), true);
});

test('the meta line has a separator between workspace and reason', () => {
  const asks = inbox.rows.find(row => row.session.id === 'asks')!;
  assert.equal(metaLine(asks, 'Beta release', true), 'Beta release · Question · Which version?');
  assert.equal(metaLine(asks, 'Beta release', false), 'Question · Which version?');
  const done = inbox.rows.find(row => row.session.id === 'works')!;
  assert.equal(metaLine(done, 'Alpha feature', true), 'Alpha feature · Working');
});
