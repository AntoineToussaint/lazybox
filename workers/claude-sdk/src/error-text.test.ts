import { test } from 'node:test';
import assert from 'node:assert/strict';
import { errorReason, errorText } from './error-text.js';

test('an error reads as its message, never as JavaScript\'s "Error: …"', () => {
  assert.equal(errorText(new Error('API error: 429 rate limit')), 'API error: 429 rate limit');
  assert.equal(errorText('plain'), 'plain');
  // An error without a message still says what it was.
  const aborted = new Error(''); aborted.name = 'AbortError';
  assert.equal(errorText(aborted), 'AbortError');
  assert.equal(errorText(new Error('')), 'Error');
});

test('a Runner refusal reads as the message inside its JSON answer', () => {
  assert.equal(errorReason(new Error('{"message":"refused: not your workspace"}')), 'refused: not your workspace');
  assert.equal(errorReason(new Error('not JSON')), 'not JSON');
  // A message that is not a string is not passed on as one.
  assert.equal(errorReason(new Error('{"message":{"code":"E"}}')), '{"message":{"code":"E"}}');
});
