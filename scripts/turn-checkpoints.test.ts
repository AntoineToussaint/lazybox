import test from 'node:test';
import assert from 'node:assert/strict';
import { create } from '@bufbuild/protobuf';
import { CodeRevertSchema, JournalEventSchema, TurnCheckpointSchema } from '@lazybox/protocol';
import { appendTranscript, emptyTranscript } from '../packages/surfaces/src/transcript-model.ts';
import { lineCounts, parseTurnCompare, revertAction, turnCompare, turnEnds, turnSummary, undoable } from '../packages/surfaces/src/turn-checkpoints.ts';

const transcript = (records: { type: string; text?: string; operationId?: string }[]) => {
  let state = emptyTranscript();
  for (const event of records) state = appendTranscript(state, create(JournalEventSchema, { cursor: state.cursor + 1n, event }));
  return state;
};

test('each turn ends before the next prompt, and the last after the last message', () => {
  const state = transcript([
    { type: 'operation_accepted', text: 'first', operationId: 'op-1' }, { type: 'message_delta', text: 'reply one' }, { type: 'turn_ended' },
    { type: 'operation_accepted', text: 'second', operationId: 'op-2' }, { type: 'turn_ended' },
    { type: 'operation_accepted', text: 'third', operationId: 'op-3' }, { type: 'message_delta', text: 'reply three' },
  ]);
  assert.deepEqual(state.messages.map(m => m.operationId), ['op-1', undefined, 'op-2', 'op-3', undefined]);
  const ends = turnEnds(state.messages);
  // A turn without a reply ends at its own prompt.
  assert.deepEqual([...ends].map(([id, op]) => [state.messages.findIndex(m => m.id === id), op]), [[1, 'op-1'], [2, 'op-2'], [4, 'op-3']]);
  assert.equal(turnEnds([]).size, 0);
});

test('a revert is said in the conversation', () => {
  const state = transcript([{ type: 'operation_accepted', text: 'go', operationId: 'op-1' }, { type: 'code_reverted', text: 'Files reverted to before turn 1.' }]);
  assert.deepEqual(state.messages.at(-1), { id: '2', type: 'notice', text: 'Files reverted to before turn 1.', turnId: '', notice: 'reverted' });
});

test('turn summaries say what changed, without empty counts', () => {
  const turn = (files: number, additions: number, deletions: number, ended = 1n) => create(TurnCheckpointSchema, { turn: 1, files, additions, deletions, endedAtUnixMillis: ended });
  assert.equal(turnSummary(turn(2, 42, 1)), '2 files changed · +42 −1');
  assert.equal(turnSummary(turn(1, 12, 0)), '1 file changed · +12');
  assert.equal(turnSummary(turn(1, 0, 0)), '1 file changed');
  assert.equal(turnSummary(turn(0, 0, 0)), 'No file changes');
  assert.equal(turnSummary(turn(3, 1, 1, 0n)), 'Checkpoint saved before this turn');
  assert.equal(lineCounts(0, 7), '−7');
  assert.equal(lineCounts(1200, 0), '+1,200');
});

test('turns are Changes comparisons, and only the latest revert is undoable', () => {
  assert.deepEqual(parseTurnCompare(turnCompare('ABC123', 4)), { sessionId: 'ABC123', turn: 4 });
  for (const value of [':start', 'origin/main', ':turn:x', ':turn::2']) assert.equal(parseTurnCompare(value), undefined, value);
  const revert = create(CodeRevertSchema, { id: 'r1', kind: 'revert', turn: 2 });
  assert.equal(undoable([revert])?.id, 'r1');
  assert.equal(undoable([{ ...revert, undoneBy: 'r2' }, create(CodeRevertSchema, { id: 'r2', kind: 'undo', undoOf: 'r1' })])?.id, 'r2');
  assert.equal(undoable([{ ...revert, undoneBy: 'r2' }]), undefined);
  assert.deepEqual(['A', 'D', 'M', 'R'].map(revertAction), ['Restored', 'Removed', 'Put back', 'Renamed back']);
});
