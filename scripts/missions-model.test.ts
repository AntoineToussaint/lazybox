import { test } from 'node:test';
import assert from 'node:assert/strict';
import type { Mission, MissionEvidence, MissionTask } from '@lazybox/protocol';
import { arrange, awaitingRows, columnOf, evidenceChip, headlineEvidence, liveLabel, missionSummary, taskActions } from '../packages/surfaces/src/missions-model.ts';
import { mergeHistory } from '../packages/surfaces/src/prompt-history.ts';

const task = (fields: Partial<MissionTask>) => ({ id: 't', goalId: '', title: 'Task', brief: '', status: 'ready', trust: '', evidenceTrust: 'claimed', reason: '', flags: [], gate: '', attempts: [], evidence: [], report: '', reportedAtUnixMillis: 0n, decisionNote: '', blockedBy: [], createdAtUnixMillis: 0n, updatedAtUnixMillis: 0n, lastSnippet: '', nextSnippets: [], ...fields }) as unknown as MissionTask;
const mission = (fields: Partial<Mission>) => ({ id: 'm', title: 'Mission', brief: '', process: 'solo-pair', processDigest: '', autonomy: 3, createdAtUnixMillis: 0n, updatedAtUnixMillis: 0n, canceled: false, seq: 0n, goals: [], tasks: [], events: [], error: '', ignored: 0, createdId: '', sessionId: '', ...fields }) as unknown as Mission;
const evidence = (fields: Partial<MissionEvidence>) => ({ ref: 'r', kind: 'run', title: '', outcome: 'pass', piped: false, member: '', repository: '', head: '', number: 0n, url: '', state: '', collectionId: '', sessionId: '', commandId: '', outputPath: '', detail: '', atUnixMillis: 0n, ...fields }) as unknown as MissionEvidence;

test('every derived status has one column; what needs the person is never hidden among work in progress', () => {
  assert.deepEqual(['ready', 'blocked'].map(columnOf), ['ready', 'ready']);
  assert.deepEqual(['working', 'verifying', 'changes_requested'].map(columnOf), ['working', 'working', 'working']);
  assert.deepEqual(['needs_you', 'failed', 'claimed_done'].map(columnOf), ['needs', 'needs', 'needs']);
  assert.deepEqual(['done', 'canceled'].map(columnOf), ['done', 'done']);
  assert.equal(columnOf('something_new'), 'ready');
});

test('the board puts tasks at a gate first in Needs you and canceled tasks last in Done', () => {
  const board = arrange([
    task({ id: 'asking', status: 'needs_you', updatedAtUnixMillis: 1n }),
    task({ id: 'gate', status: 'needs_you', gate: 'reviewed', updatedAtUnixMillis: 2n }),
    task({ id: 'canceled', status: 'canceled', updatedAtUnixMillis: 9n }),
    task({ id: 'done', status: 'done', updatedAtUnixMillis: 3n }),
  ]);
  assert.deepEqual(board.needs.map(t => t.id), ['gate', 'asking']);
  assert.deepEqual(board.done.map(t => t.id), ['done', 'canceled']);
});

test('awaiting acceptance is a task at a human gate, oldest report first, never from a canceled mission', () => {
  const rows = awaitingRows([
    mission({ id: 'a', tasks: [task({ id: 'late', status: 'needs_you', gate: 'reviewed', reportedAtUnixMillis: 5n }), task({ id: 'asking', status: 'needs_you' })] }),
    mission({ id: 'b', tasks: [task({ id: 'early', status: 'needs_you', gate: 'reviewed', reportedAtUnixMillis: 1n })] }),
    mission({ id: 'c', canceled: true, tasks: [task({ id: 'gone', status: 'needs_you', gate: 'reviewed' })] }),
  ]);
  assert.deepEqual(rows.map(r => r.task.id), ['early', 'late']);
});

test('the next actions follow the status: assign, report, accept', () => {
  assert.deepEqual(taskActions(task({ status: 'ready' })), ['assign']);
  assert.deepEqual(taskActions(task({ status: 'working', attempt: { endedAtUnixMillis: 0n } as never })), ['open', 'report']);
  assert.deepEqual(taskActions(task({ status: 'needs_you', gate: 'reviewed' })), ['accept', 'reject', 'open']);
  assert.deepEqual(taskActions(task({ status: 'needs_you' })), ['open']);
  assert.deepEqual(taskActions(task({ status: 'failed' })), ['assign']);
  assert.deepEqual(taskActions(task({ status: 'done' })), []);
});

test('a mission summary leads with what needs the person', () => {
  assert.equal(missionSummary(mission({})), 'No tasks yet');
  assert.equal(missionSummary(mission({ tasks: [task({ status: 'needs_you' }), task({ status: 'working' }), task({ status: 'done' }), task({ status: 'canceled' })] })), '1 needs you · 1 working · 1 of 3 done');
  assert.equal(missionSummary(mission({ canceled: true })), 'Canceled');
  assert.match(missionSummary(mission({ error: 'corrupt' })), /Unreadable/);
});

test('evidence chips say what ran and how it ended', () => {
  assert.deepEqual(evidenceChip(evidence({ title: 'go test ./...', exitCode: 1, outcome: 'fail' })).label, 'go test ./... · exit 1');
  assert.equal(evidenceChip(evidence({ title: 'go test ./...', exitCode: 1, outcome: 'fail' })).tone, 'fail');
  assert.equal(evidenceChip(evidence({ title: 'tail', piped: true, outcome: 'unknown' })).label, 'tail · piped');
  assert.equal(evidenceChip(evidence({ kind: 'pr', number: 7n, head: 'abcdef1234', outcome: 'pass' })).label, 'PR #7 · abcdef1');
  assert.equal(evidenceChip(evidence({ kind: 'commit', member: 'api', head: '3f9c2e1aa', outcome: 'unknown' })).label, 'api 3f9c2e1');
  const latest = headlineEvidence([evidence({ ref: 'r2', kind: 'run' }), evidence({ ref: 'p', kind: 'pr' }), evidence({ ref: 'r1', kind: 'run' })]);
  assert.deepEqual(latest.map(e => e.ref), ['r2', 'p']);
});

test('liveness reads briefly', () => {
  assert.equal(liveLabel('running', 'Bash'), 'Running · Bash');
  assert.equal(liveLabel('closed', 'provider failed'), 'Stopped');
  assert.equal(liveLabel('waiting'), 'Waiting for you');
});

test('recall takes this window first, then what the Runner kept from other windows and devices', () => {
  const merged = mergeHistory([{ text: 'mine', conversation: 'c1' }], [{ text: 'mine', conversation: 'c1' }, { text: 'from the phone', conversation: 'c2' }]);
  assert.deepEqual(merged.map(e => e.text), ['mine', 'from the phone']);
});

test('a stale Mission review names its bound inputs', () => {
  const chip = evidenceChip(evidence({kind: 'review', state: 'stale', outcome: 'unknown', contractDigest: 'spec-sha', revisions: {api: 'candidate-sha'}}));
  assert.equal(chip.label, 'Review needs repeating');
  assert.equal(chip.tone, 'neutral');
  assert.match(chip.title, /spec-sha/);
  assert.match(chip.title, /api candidate-sha/);
  assert.equal(liveLabel('reserved'), 'Reserved');
});
