// A window's Runner link across a Runner restart: a new bearer (a Codefly hot
// reload) or a new port (a launcher restart), with no network and no Runner.
import test from 'node:test';
import assert from 'node:assert/strict';
import { Code, ConnectError } from '@connectrpc/connect';
import { RunnerLink, follow, reconnectDelay, runnerLink, type RunnerAuthority } from '../packages/client-core/src/index.ts';

/** A fake Runner reachable at one origin with one bearer; it records every request it is sent. */
function fakeRunner(origin: string, token: string) {
  const runner = { origin, token, sent: [] as { url: string; bearer: string | null }[], down: false, delays: [] as number[] };
  const frame = (value: unknown, flag = 0) => { const data = new TextEncoder().encode(JSON.stringify(value)); const out = new Uint8Array(5 + data.length); out[0] = flag; new DataView(out.buffer).setUint32(1, data.length); out.set(data, 5); return out; };
  const fetch: typeof globalThis.fetch = async (input, init) => {
    const url = String(input), bearer = new Headers(init?.headers).get('authorization');
    runner.sent.push({ url, bearer });
    const delay = runner.delays.shift(); if (delay) await new Promise(resolve => setTimeout(resolve, delay));
    if (runner.down || !url.startsWith(runner.origin + '/')) throw new TypeError('fetch failed: connection refused');
    if (bearer !== `Bearer ${runner.token}`) return new Response(JSON.stringify({ code: 'unauthenticated', message: 'local test bench bearer required' }), { status: 401, headers: { 'Content-Type': 'application/json' } });
    if (url.endsWith('/WatchEvents')) {
      const body = new Blob([frame({ cursor: '1', event: { type: 'message_delta', text: 'after restart' } }), frame({}, 2)]);
      return new Response(body, { status: 200, headers: { 'Content-Type': 'application/connect+json' } });
    }
    return new Response(JSON.stringify({ instanceId: 'fixture' }), { status: 200, headers: { 'Content-Type': 'application/json' } });
  };
  return { runner, fetch, locator: async (): Promise<RunnerAuthority> => ({ baseUrl: runner.origin, token: runner.token }) };
}

test('a Runner hot reload (new bearer, same port) is adopted by every client of the window, and the refused call is retried once', async () => {
  const { runner, fetch, locator } = fakeRunner('http://127.0.0.1:4000', 'first');
  const link = new RunnerLink({ baseUrl: runner.origin, token: 'first' }, { fetch, renew: locator });
  const before = link.client(); // e.g. held by a transcript cache from before the restart
  assert.equal((await before.info({})).instanceId, 'fixture');
  runner.token = 'second'; // the Runner restarted; the window has not reread its locator yet
  runner.sent.length = 0;
  assert.equal((await before.info({})).instanceId, 'fixture');
  assert.deepEqual(runner.sent.map(r => r.bearer), ['Bearer first', 'Bearer second']);
  assert.equal(link.authority.token, 'second');
  assert.equal(link.authFailure, '');
  // A client made after, and the one made before, now both send the new bearer.
  runner.sent.length = 0;
  await link.client().info({}); await before.info({});
  assert.deepEqual(runner.sent.map(r => r.bearer), ['Bearer second', 'Bearer second']);
  assert.equal(runnerLink(before), link);
});

test('a request refused after another one already adopted the successor is resent, without rereading again', async () => {
  const { runner, fetch, locator } = fakeRunner('http://127.0.0.1:4000', 'second');
  let reads = 0;
  const link = new RunnerLink({ baseUrl: runner.origin, token: 'first' }, { fetch, renew: () => { reads++; return locator(); } });
  const api = link.client();
  // Both go out with the stale bearer; the slow one is refused well after the fast one adopted the successor.
  runner.delays.push(0, 400);
  const [fast, slow] = await Promise.all([api.info({}), api.info({})]);
  assert.equal(fast.instanceId, 'fixture'); assert.equal(slow.instanceId, 'fixture');
  assert.equal(reads, 1);
  assert.equal(link.authFailure, '');
});

test('a launcher restart on a new port is followed without being armed, and an unfinished request is not repeated', async () => {
  const { runner, fetch, locator } = fakeRunner('http://127.0.0.1:4000', 'first');
  const link = new RunnerLink({ baseUrl: runner.origin, token: 'first' }, { fetch, renew: locator });
  const api = link.client();
  runner.origin = 'http://127.0.0.1:4555'; runner.token = 'moved';
  // The old port is gone. The request may have run there before it went away, so it fails as unavailable, once.
  await assert.rejects(api.openSession({ profileId: 'claude-default' }), (error: unknown) => ConnectError.from(error).code === Code.Unavailable);
  assert.equal(runner.sent.length, 1);
  assert.deepEqual(link.authority, { baseUrl: 'http://127.0.0.1:4555', token: 'moved' });
  assert.equal((await api.info({})).instanceId, 'fixture');
  assert.deepEqual(runner.sent.map(r => r.url), ['http://127.0.0.1:4000/lazybox.bench.v1.RunnerService/OpenSession', 'http://127.0.0.1:4555/lazybox.bench.v1.RunnerService/Info']);
});

test('a server stream refused after a restart is reopened once with the new bearer', async () => {
  const { runner, fetch, locator } = fakeRunner('http://127.0.0.1:4000', 'second');
  const link = new RunnerLink({ baseUrl: runner.origin, token: 'first' }, { fetch, renew: locator });
  const events = [];
  for await (const event of link.client().watchEvents({ sessionId: 'session', after: 0n })) events.push(event.cursor);
  assert.deepEqual(events, [1n]);
  assert.deepEqual(runner.sent.map(r => r.bearer), ['Bearer first', 'Bearer second']);
});

test('an empty bearer is never sent: the locator is reread, and without one the call fails locally', async () => {
  const { runner, fetch } = fakeRunner('http://127.0.0.1:4000', 'resolved');
  let published: RunnerAuthority | undefined;
  const link = new RunnerLink({ baseUrl: runner.origin, token: '' }, { fetch, renew: async () => published });
  await assert.rejects(link.client().info({}), (error: unknown) => ConnectError.from(error).code === Code.Unauthenticated);
  assert.equal(runner.sent.length, 0, 'a request went out without a bearer');
  published = { baseUrl: runner.origin, token: 'resolved' };
  await new Promise(resolve => setTimeout(resolve, 300)); // past the settle time of the failed reread
  await link.client().info({});
  assert.deepEqual(runner.sent.map(r => r.bearer), ['Bearer resolved']);
  // Nor is an empty bearer from a half-written locator adopted over a resolved one.
  assert.equal(link.adopt({ baseUrl: runner.origin, token: '' }), false);
  assert.equal(link.authority.token, 'resolved');
});

test('a bearer the Runner keeps refusing is reported on the link, and cleared by the next accepted call', async () => {
  const { runner, fetch } = fakeRunner('http://127.0.0.1:4000', 'other');
  const link = new RunnerLink({ baseUrl: runner.origin, token: 'stale' }, { fetch, renew: async () => ({ baseUrl: runner.origin, token: 'stale' }) });
  let notified = 0; link.subscribe(() => notified++);
  await assert.rejects(link.client().info({}), (error: unknown) => ConnectError.from(error).code === Code.Unauthenticated);
  assert.match(link.authFailure, /bearer required/);
  assert(notified > 0);
  runner.token = 'stale';
  await link.client().info({});
  assert.equal(link.authFailure, '');
});

test('a followed stream keeps reconnecting after a refused bearer instead of ending', async () => {
  const control = new AbortController(); let attempts = 0; const received: bigint[] = [], statuses: string[] = [];
  await follow(async function* (after) {
    if (++attempts === 1) throw new ConnectError('local test bench bearer required', Code.Unauthenticated);
    assert.equal(after, 0n);
    yield { cursor: 1n };
  }, control.signal, event => received.push(event.cursor), status => statuses.push(status), () => assert.fail('a refused bearer ended the stream'));
  assert.equal(attempts, 2);
  assert.deepEqual(received, [1n]);
  assert.equal(statuses.at(-1), 'Complete');
});

test('a followed stream backs off while it keeps failing, and starts over once an event arrives', async () => {
  const waits: number[] = [];
  let attempt = 0;
  // Three failures, one event, two more failures, then the rest.
  const script = ['fail', 'fail', 'fail', 'event', 'fail', 'fail', 'end'];
  let cursor = 0n;
  await follow(async function* () {
    for (let step = script[attempt++]; ; step = script[attempt++]) {
      if (step === 'fail') throw new ConnectError('connection reset', Code.Unavailable);
      if (step === 'event') { yield { cursor: ++cursor }; continue; }
      return;
    }
  }, new AbortController().signal, () => {}, () => {}, undefined, 0n, { delay: failures => { waits.push(failures); return 0; } });
  assert.deepEqual(waits, [1, 2, 3, 1, 2], 'the count grows while failing and resets on an event');
  // About 1 s, doubling to 30 s, each between half and all of it.
  assert.deepEqual([1, 2, 3, 6, 7, 20].map(n => reconnectDelay(n, () => 1)), [1000, 2000, 4000, 30000, 30000, 30000]);
  assert.deepEqual([1, 2, 6].map(n => reconnectDelay(n, () => 0)), [500, 1000, 15000]);
});
