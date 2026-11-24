import { strict as assert } from 'node:assert';
import { test } from 'node:test';
import type { SessionMessage } from '@anthropic-ai/claude-agent-sdk';
import { endOfTurn } from './fork.js';

const entry = (uuid: string, id: string, stop: string | null, parent: string | null = null): SessionMessage =>
  ({ type: 'assistant', uuid, session_id: 's', message: { role: 'assistant', id, stop_reason: stop, content: [] }, parent_tool_use_id: parent, parent_agent_id: null });

test('a turn recorded without its fork point ends at the last entry of its last message', () => {
  const messages = [
    entry('a1', 'msg_one', 'end_turn'),
    // One API message written as two entries.
    entry('a2', 'msg_two', null),
    entry('a3', 'msg_two', 'end_turn'),
    // A sub-agent's message is not the main conversation's.
    entry('child', 'msg_two', 'end_turn', 'call'),
    entry('a4', 'msg_tools', 'tool_use'),
  ];
  assert.equal(endOfTurn(messages, 'msg_one'), 'a1');
  assert.equal(endOfTurn(messages, 'msg_two'), 'a3');
  // A turn that did not end on its own is no boundary; nor is a message the transcript no longer holds.
  assert.equal(endOfTurn(messages, 'msg_tools'), undefined);
  assert.equal(endOfTurn(messages, 'msg_gone'), undefined);
});
