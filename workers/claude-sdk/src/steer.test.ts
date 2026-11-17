import assert from 'node:assert/strict';
import test from 'node:test';
import { steerPriority, userMessage } from './steer.ts';
import { TurnInterrupts } from './interrupt.ts';

test('a steered message asks Claude Code to fold it into the running turn; a message of its own has no priority', () => {
  const content = [{ type: 'text', text: 'Also log the port.' }];
  const steered = userMessage(content, 'session-1', steerPriority);
  assert.equal(steered.priority, 'next');
  assert.deepEqual(steered.message, { role: 'user', content });
  assert.equal(steered.session_id, 'session-1');
  assert.equal(steered.parent_tool_use_id, null);
  assert.equal('priority' in userMessage(content, 'session-1'), false);
  assert.notEqual(userMessage(content, 's').uuid, userMessage(content, 's').uuid);
});

test('a steer is taken only while a turn runs or is about to start', () => {
  const turns = new TurnInterrupts();
  assert.equal(turns.inTurn, false, 'idle: the Runner keeps the message for its own turn');
  turns.delivered();
  assert.equal(turns.inTurn, true, 'a message was handed over: its turn is about to start');
  turns.pass({ type: 'turn_started' });
  assert.equal(turns.inTurn, true);
  turns.pass({ type: 'turn_ended', status: 'completed' });
  assert.equal(turns.inTurn, false, 'ended: a steer now would be a turn of its own');
});
