import { test } from 'node:test';
import assert from 'node:assert/strict';
import { LONG_RUN_MS, RECENT_USE_MS, RunWatch, consolePlace, followRuns, newerRuns, shouldTakeOver, type TakeOverInput } from '../packages/surfaces/src/console-follow.ts';
import type { WorkspacePage } from '../packages/surfaces/src/workspace-model.ts';

const idle = RECENT_USE_MS + 1;
const long = { done: false, runningFor: LONG_RUN_MS + 200 };
const input = (patch: Partial<TakeOverInput>): TakeOverInput => ({ follow: true, shown: { done: true }, sinceUse: idle, run: long, ...patch });

test('a long run takes over a console showing a finished run nobody has used for a while', () => {
  assert.equal(shouldTakeOver(input({})), true);
  assert.equal(shouldTakeOver(input({ shown: undefined })), true, 'a console showing nothing (or a run no longer kept) moves too');
});

test('a run the console shows that is still running is never replaced', () => {
  assert.equal(shouldTakeOver(input({ shown: { done: false } })), false);
});

test('choosing or using the console in the last 30 s keeps it where it is', () => {
  assert.equal(shouldTakeOver(input({ sinceUse: 0 })), false);
  assert.equal(shouldTakeOver(input({ sinceUse: RECENT_USE_MS - 1 })), false);
  assert.equal(shouldTakeOver(input({ sinceUse: RECENT_USE_MS })), true);
});

test('quick runs never take over: one that ended, or has not yet run 1.5 s', () => {
  assert.equal(shouldTakeOver(input({ run: { done: true, runningFor: 358 } })), false);
  assert.equal(shouldTakeOver(input({ run: { done: true, runningFor: 60_000 } })), false, 'an ended run is offered, not shown');
  assert.equal(shouldTakeOver(input({ run: { done: false, runningFor: LONG_RUN_MS - 1 } })), false);
  // The person's own `cmd | lazybox log` is something they wait for, however quick.
  assert.equal(shouldTakeOver(input({ run: { done: true, runningFor: 0, yours: true } })), true);
  assert.equal(shouldTakeOver(input({ run: { done: true, runningFor: 0, yours: true }, sinceUse: 0 })), false);
});

test('with Follow new runs off, nothing takes over', () => {
  assert.equal(shouldTakeOver(input({ follow: false })), false);
  assert.equal(shouldTakeOver(input({ follow: false, run: { done: true, runningFor: 0, yours: true } })), false);
});

const run = (id: string, done: boolean, extra: Record<string, string> = {}) => ({ id, done, origin: '', sessionId: 's', collectionId: '', ...extra });

test('a run is a candidate once it is still running 1.5 s after it was first listed; quick runs never are', () => {
  const watch = new RunWatch();
  assert.deepEqual(watch.next([run('old', false)], 0), { fresh: [], candidates: [] }, 'the first listing is the baseline, running or not');
  // Five quick runs, each ended by the next listing, and one long one.
  const quick = ['q1', 'q2', 'q3', 'q4', 'q5'];
  assert.deepEqual(watch.next([run('old', false), ...quick.map(id => run(id, false)), run('long', false)], 1500, 1520).fresh, [...quick, 'long']);
  const later = watch.next([run('old', false), ...quick.map(id => run(id, true)), run('long', false)], 3000, 3020);
  assert.deepEqual(later, { fresh: [], candidates: [{ id: 'long', runningFor: 1520, yours: false }] });
  assert.deepEqual(watch.next([run('old', false), ...quick.map(id => run(id, true)), run('long', false)], 4500, 4520).candidates, [], 'each run is a candidate once');
});

test("the person's terminal runs are candidates at once; actions and setup scripts never show up", () => {
  const watch = new RunWatch();
  watch.next([], 0);
  const news = watch.next([run('mine', true, { sessionId: '', collectionId: 'alpha', origin: 'terminal' }), run('server', false, { sessionId: '', collectionId: 'alpha', origin: 'action' })], 1500, 1510);
  assert.deepEqual(news, { fresh: ['mine'], candidates: [{ id: 'mine', runningFor: 0, yours: true }] });
});

test('the newer-runs count starts after the run shown, and leaves actions out', () => {
  const listing = [run('a', true), run('b', true), run('c', true, { origin: 'action' }), run('d', false)];
  assert.deepEqual(newerRuns(listing, 'a').map(command => command.id), ['b', 'd']);
  assert.deepEqual(newerRuns(listing, 'd'), []);
  assert.deepEqual(newerRuns(listing, 'gone'), []);
});

test('a console in view moves only when it may be taken over; one out of view points at the newest run, unseen', () => {
  const news = { fresh: ['q1', 'q2'], candidates: [] };
  const front = { follow: true, place: 'front' as const, shown: { done: true }, sinceUse: idle };
  assert.equal(followRuns(news, front), undefined, 'quick runs leave a console in view alone');
  assert.deepEqual(followRuns({ fresh: [], candidates: [{ id: 'long', runningFor: 1600, yours: false }] }, front), { commandId: 'long', open: false });
  assert.equal(followRuns({ fresh: [], candidates: [{ id: 'long', runningFor: 1600, yours: false }] }, { ...front, sinceUse: 5000 }), undefined);
  for (const place of ['behind', 'away', 'none'] as const) {
    assert.deepEqual(followRuns(news, { ...front, place }), { commandId: 'q2', open: false }, `${place}: points at the newest, never comes forward`);
    assert.deepEqual(followRuns({ fresh: [], candidates: [{ id: 'long', runningFor: 1600, yours: false }] }, { ...front, place }), undefined);
  }
  assert.deepEqual(followRuns(news, { ...front, place: 'folded' }), { commandId: 'q2', open: false }, 'a folded console stays folded for quick runs');
  assert.deepEqual(followRuns({ fresh: [], candidates: [{ id: 'long', runningFor: 1600, yours: false }] }, { ...front, place: 'folded' }), { commandId: 'long', open: true });
  assert.equal(followRuns(news, { ...front, place: 'behind', follow: false }), undefined, 'Follow off: never switches by itself');
  assert.equal(followRuns({ fresh: [], candidates: [{ id: 'long', runningFor: 1600, yours: false }] }, { ...front, place: 'folded', follow: false }), undefined);
});

test('where the console is: its group decides whether a run may bring it forward', () => {
  const console = { id: 'out', kind: 'console', binding: { mode: 'follow' }, detached: false };
  const chat = { id: 'chat', kind: 'chat', binding: { mode: 'follow' }, detached: false };
  const page = (active: string, minimized = false, panels = [chat, console]) => ({ id: 'p', layout: { type: 'group', id: 'g', panels, active, minimized }, detached: [], popouts: [], floating: [], selection: { collectionId: '', sessionId: '', commandId: '' } }) as unknown as WorkspacePage;
  assert.equal(consolePlace(page('chat'), false), 'behind');
  assert.equal(consolePlace(page('out'), false), 'front');
  assert.equal(consolePlace(page('out', true, [console]), false), 'folded');
  assert.equal(consolePlace(page('out', true, [console]), true), 'away');
  assert.equal(consolePlace(page('chat', false, [chat]), false), 'none');
});
