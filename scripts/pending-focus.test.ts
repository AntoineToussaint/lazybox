import { test } from 'node:test';
import assert from 'node:assert/strict';
import { PendingFocus, focusUnmoved } from '../packages/surfaces/src/pending-focus.ts';

// Regression: a task started on another Runner asked for its message box's
// focus, which ran once that Runner's page settled; focus the person had moved
// meanwhile (to a sidebar header) was taken back (multi-runner, ~half of runs).
test('a focus request the person moved on from is dropped', () => {
  const focus = new PendingFocus<{ panel: string }>();
  const request = { panel: 'chat-1' };
  focus.ask(request);
  focus.moved();
  assert.equal(focus.waiting, null);
  assert.equal(focus.take(request), false, 'focus moved while it waited: it stays where it went');
});

test('a focus request runs once, and a later ask replaces it', () => {
  const focus = new PendingFocus<{ panel: string }>();
  const first = { panel: 'a' }, second = { panel: 'b' };
  focus.ask(first);
  assert.equal(focus.waiting, first);
  focus.ask(second);
  assert.equal(focus.take(first), false, 'replaced');
  assert.equal(focus.take(second), true);
  assert.equal(focus.take(second), false, 'already run');
});

test('a focus scheduled for a later frame applies only where focus still is', () => {
  const box = {} as Element, header = {} as Element;
  assert.equal(focusUnmoved(box, box), true);
  assert.equal(focusUnmoved(box, header), false);
  assert.equal(focusUnmoved(null, header), false, 'from nowhere to a control the person chose');
});
