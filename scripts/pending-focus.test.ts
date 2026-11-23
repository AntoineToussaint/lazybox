import { test } from 'node:test';
import assert from 'node:assert/strict';
import { PendingFocus, focusWaiting, restoreFocus, trackFocus } from '../packages/surfaces/src/pending-focus.ts';

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

// A closing palette gives focus back a frame later; the action chosen in it
// (focus a panel, show a conversation) asked for focus too, and wins.
test('giving focus back yields to a focus any owner asked for', () => {
  let focused = 0;
  const previous = { isConnected: true, focus: () => { focused++; } } as unknown as HTMLElement;
  const workspace = new PendingFocus<{ panel: string }>(), dock = new PendingFocus<{ panel: string }>();
  const untrack = [trackFocus(workspace), trackFocus(dock)];
  const request = { panel: 'console' };
  dock.ask(request);
  assert.equal(focusWaiting(), true);
  restoreFocus(previous);
  assert.equal(focused, 0, 'the action\'s focus is waiting: the palette stands aside');
  assert.equal(dock.take(request), true, 'and the request still runs');
  restoreFocus(previous);
  assert.equal(focused, 1, 'nothing waiting: focus goes back');
  restoreFocus({ isConnected: false, focus: () => { focused++; } } as unknown as HTMLElement);
  assert.equal(focused, 1, 'an element gone from the page is not focused');
  untrack.forEach(off => off());
});

// Each owner keeps its own request: the dock focusing a panel of its own
// never takes or replaces the workspace's, and an unmounted view's goes away.
test('owners keep their own requests, and an unmounted owner leaves none waiting', () => {
  const workspace = new PendingFocus<{ panel: string }>(), dock = new PendingFocus<{ panel: string }>();
  const offWorkspace = trackFocus(workspace), offDock = trackFocus(dock);
  const mine = { panel: 'chat' };
  workspace.ask(mine);
  dock.ask({ panel: 'console' });
  assert.equal(workspace.waiting, mine, 'the dock asking does not replace the workspace\'s request');
  offWorkspace();
  assert.equal(dock.take(dock.waiting!), true);
  assert.equal(focusWaiting(), false, 'an owner off the page no longer holds the palette back');
  offDock();
});
