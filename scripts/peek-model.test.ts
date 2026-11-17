import { test } from 'node:test';
import assert from 'node:assert/strict';
import { parsePeek, peekKey, peekList, peekProps, peekTitle, stepPeek, typingTarget, type PeekItem } from '../packages/surfaces/src/peek-model.ts';

test('an element says what it is with data-peek, and only well-formed items are peekable', () => {
  const item: PeekItem = { kind: 'conversation', sessionId: 's1', workspaceId: 'w', title: 'Fix it' };
  assert.deepEqual(parsePeek(peekProps(item)['data-peek']), item);
  assert.deepEqual(parsePeek('{"kind":"file","workspaceId":"w","path":"api/x.go","line":12,"extra":1}'), { kind: 'file', workspaceId: 'w', path: 'api/x.go', line: 12 });
  assert.equal(parsePeek('{"kind":"file","workspaceId":"w","path":"api/x.go","line":-1}')?.kind, 'file');
  assert.equal((parsePeek('{"kind":"file","workspaceId":"w","path":"x","line":-1}') as { line?: number }).line, undefined);
  for (const bad of [null, '', 'not json', '[]', '{"kind":"nope"}', '{"kind":"conversation"}', '{"kind":"conversation","sessionId":7}', '{"kind":"artifact","workspaceId":"w"}', 'x'.repeat(5000)])
    assert.equal(parsePeek(bad), undefined, String(bad).slice(0, 40));
  // __proto__ and friends are not kinds.
  assert.equal(parsePeek('{"kind":"toString","sessionId":"s"}'), undefined);
});

test('one key per item, whatever its title', () => {
  assert.equal(peekKey({ kind: 'conversation', sessionId: 's', title: 'a' }), peekKey({ kind: 'conversation', sessionId: 's', title: 'b' }));
  assert.notEqual(peekKey({ kind: 'artifact', workspaceId: 'a', artifactId: '1' }), peekKey({ kind: 'artifact', workspaceId: 'b', artifactId: '1' }));
  assert.equal(peekKey({ kind: 'mission-task', taskId: 't' }), 'mission-task:t');
});

test('a list keeps the order shown, each item once, and leaves sub-agents to their own panel', () => {
  const a: PeekItem = { kind: 'conversation', sessionId: 'a' }, b: PeekItem = { kind: 'process', processId: 'p' }, c: PeekItem = { kind: 'command', commandId: 'c' };
  assert.deepEqual(peekList([a, undefined, b, { ...a, title: 'again' }, { kind: 'subagent', agentId: 'x' }, c]), [a, b, c]);
});

test('arrows step through the list and stop at either end', () => {
  const list: PeekItem[] = [{ kind: 'conversation', sessionId: 'a' }, { kind: 'conversation', sessionId: 'b' }, { kind: 'conversation', sessionId: 'c' }];
  assert.deepEqual(stepPeek(list, list[1], 1), list[2]);
  assert.deepEqual(stepPeek(list, list[1], -1), list[0]);
  assert.equal(stepPeek(list, list[2], 1), undefined);
  assert.equal(stepPeek(list, list[0], -1), undefined);
  assert.equal(stepPeek(list, { kind: 'conversation', sessionId: 'z' }, 1), undefined);
});

test('a title before the live data arrives', () => {
  assert.equal(peekTitle({ kind: 'file', workspaceId: 'w', path: 'api/src/main.go' }), 'main.go');
  assert.equal(peekTitle({ kind: 'process', processId: 'p' }), 'Process');
  assert.equal(peekTitle({ kind: 'conversation', sessionId: 's', title: 'Fix it' }), 'Fix it');
});

test('Space and arrows belong to a field the person types in', () => {
  assert.equal(typingTarget({ tagName: 'TEXTAREA' }), true);
  assert.equal(typingTarget({ tagName: 'INPUT', type: 'text' }), true);
  assert.equal(typingTarget({ tagName: 'INPUT', type: 'search' }), true);
  assert.equal(typingTarget({ tagName: 'INPUT', type: 'checkbox' }), false);
  assert.equal(typingTarget({ tagName: 'DIV', isContentEditable: true }), true);
  assert.equal(typingTarget({ tagName: 'BUTTON' }), false);
  assert.equal(typingTarget(null), false);
});
