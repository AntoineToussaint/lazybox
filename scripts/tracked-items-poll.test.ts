// The tracked pull requests' poll (tracked-items-poll.ts): one request for
// every workspace's links, saying whether the window has focus; an older
// Runner without ListLinks leaves every view empty, once.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { Code, ConnectError } from '@lazybox/client-core';
import { TrackedItemsPoll } from '../packages/surfaces/src/tracked-items-poll.ts';

const tick = () => new Promise(resolve => setImmediate(resolve));

test('every workspace\'s links, in one request', async () => {
  const requests: unknown[] = [];
  const api = { listLinks: async (request: unknown) => { requests.push(request); return { links: [{ collectionId: 'w1' }], events: [], trackingError: 'GitHub rate limit', pollSeconds: 300 }; } };
  const poll = new TrackedItemsPoll(api as never);
  const leave = poll.subscribe(() => {});
  await tick();
  assert.deepEqual(requests, [{ all: true, focused: false }], 'all links; a window without a document has no focus');
  assert.equal(poll.snapshot.loaded, true);
  assert.equal(poll.snapshot.links.length, 1);
  assert.equal(poll.snapshot.trackingError, 'GitHub rate limit');
  assert.equal(poll.snapshot.pollSeconds, 300);
  leave();
});

test('an older Runner tracks nothing, and is not asked again', async () => {
  let asked = 0;
  const api = { listLinks: async () => { asked++; throw new ConnectError('unimplemented', Code.Unimplemented); } };
  const poll = new TrackedItemsPoll(api as never);
  const leave = poll.subscribe(() => {});
  await tick();
  assert.equal(poll.snapshot.unsupported, true);
  assert.equal(poll.snapshot.loaded, true);
  await poll.load();
  assert.equal(asked, 1);
  leave();
});
