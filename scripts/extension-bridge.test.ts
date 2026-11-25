// The extension bridge (EXTENSIONS.md): the host side's parsing, grant checks,
// limits and error mapping; what the projections let out; the bridge client
// (lazybox.js), its types (lazybox.d.ts), the Runner's grants and icons and
// the content policies in step with it.
import { create } from '@bufbuild/protobuf';
import { CollectionSchema, SessionSchema, TelemetryReportSchema, type Session } from '@lazybox/protocol';
import { ConnectError, Code } from '@connectrpc/connect';
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { BRIDGE_PROTOCOL, BridgeRefusal, bridgeMethods, createBridgeHost, grantFor, parseBridgeRequest, rateLimiter, type BridgeHandlers } from '../packages/surfaces/src/extension-bridge.ts';
import { projectAttention, projectConversations, projectUsage } from '../packages/surfaces/src/extension-data.ts';
import { describeGrant, widening } from '../packages/surfaces/src/extension-grants.ts';

const runtime = (name: string) => readFileSync(new URL(`../services/runner/code/internal/power/extensions/runtime/${name}`, import.meta.url), 'utf8');
const goSource = readFileSync(new URL('../services/runner/code/internal/power/extensions.go', import.meta.url), 'utf8');
const req = (method: string, params: object = {}, id = 'r1') => ({ lazybox: 1, id, method, params });
const handlers = (over: Partial<BridgeHandlers> = {}): BridgeHandlers => Object.fromEntries(Object.keys(bridgeMethods).map(m => [m, over[m as keyof BridgeHandlers] || (async () => ({ ok: m }))])) as BridgeHandlers;

test('only protocol-1 requests with an id are answered; the rest is ignored', () => {
  for (const ignored of [null, 'x', {}, { lazybox: 2, id: 'a', method: 'hello' }, { lazybox: 1, method: 'hello' }, { lazybox: 1, id: 'a b', method: 'hello' }, { lazybox: 1, id: 'x'.repeat(65), method: 'hello' }])
    assert.equal(parseBridgeRequest(ignored), undefined, JSON.stringify(ignored));
  assert.equal(parseBridgeRequest(req('nope'))?.reply?.error?.code, 'unknown-method');
  assert.equal(parseBridgeRequest(req('__proto__'))?.reply?.error?.code, 'unknown-method');
  assert.equal(parseBridgeRequest({ lazybox: 1, id: 'a', method: 'hello', params: [1] })?.reply?.error?.code, 'bad-request');
  assert.equal(parseBridgeRequest(req('messages.send', { text: 'x'.repeat(70_000) }))?.reply?.error?.code, 'too-large');
  assert.equal(parseBridgeRequest(req('messages.send', { text: '界'.repeat(24000) }))?.reply?.error?.code, 'too-large');
  const cyclic: Record<string, unknown> = { lazybox: 1, id: 'a', method: 'hello' }; cyclic.params = { self: cyclic };
  assert.equal(parseBridgeRequest(cyclic)?.reply?.error?.code, 'too-large');
  assert.deepEqual(parseBridgeRequest(req('sessions.list', { limit: 3 }))?.request, { id: 'r1', method: 'sessions.list', params: { limit: 3 } });
});

test('every method needs its grant; do needs do:<that action>', async () => {
  assert.equal(grantFor({ method: 'do', params: { action: 'inbox.open' } }), 'do:inbox.open');
  assert.equal(grantFor({ method: 'hello', params: {} }), '');
  const refused: unknown[] = [];
  const host = createBridgeHost({ grants: ['read:attention', 'do:inbox.open'], handlers: handlers(), onRefused: r => refused.push(r) });
  assert.deepEqual(await host(req('attention.list')), { lazybox: 1, id: 'r1', result: { ok: 'attention.list' } });
  assert.deepEqual(await host(req('do', { action: 'inbox.open' })), { lazybox: 1, id: 'r1', result: { ok: 'do' } });
  const sent = await host(req('messages.send', { sessionId: 's', text: 'hi' }));
  assert.equal(sent?.error?.code, 'not-granted');
  assert.equal(sent?.error?.grant, 'send:messages');
  assert.equal((await host(req('do', { action: 'attention.next' })))?.error?.grant, 'do:attention.next');
  assert.equal((await host(req('do', {})))?.error?.code, 'not-granted');
  assert.equal((await host(req('hello')))?.result !== undefined, true, 'hello needs no grant');
  assert.equal((await host(req('hello', { protocol: 2 })))?.error?.code, 'unsupported-protocol');
  assert.equal(refused.length, 4);
  assert.equal(await host({ not: 'ours' }), undefined);
});

test('a handler failure is an error reply, never a throw', async () => {
  const host = createBridgeHost({ grants: ['read:sessions', 'read:usage', 'read:missions', 'read:commands'], handlers: handlers({
    'sessions.list': async () => { throw new ConnectError('denied', Code.PermissionDenied); },
    'usage.read': async () => { throw new ConnectError('down', Code.Unavailable); },
    'missions.list': async () => { throw new BridgeRefusal('bad-request', 'sessionId is required'); },
    'commands.list': async () => { throw new Error('boom'); },
  }) });
  assert.equal((await host(req('sessions.list')))?.error?.code, 'refused');
  assert.equal((await host(req('usage.read')))?.error?.code, 'unavailable');
  assert.deepEqual((await host(req('missions.list')))?.error, { code: 'bad-request', message: 'sessionId is required' });
  assert.deepEqual((await host(req('commands.list')))?.error, { code: 'failed', message: 'boom' });
});

test('a flood is rate limited, then allowed again', async () => {
  let now = 0;
  const allow = rateLimiter(() => now, 3, 1);
  assert.deepEqual([allow(), allow(), allow(), allow()], [true, true, true, false]);
  now = 1000;
  assert.deepEqual([allow(), allow()], [true, false]);
  const host = createBridgeHost({ grants: [], handlers: handlers(), now: () => 0 });
  const replies = await Promise.all(Array.from({ length: 30 }, (_, i) => host(req('hello', {}, `r${i}`))));
  assert.equal(replies.filter(r => r?.error?.code === 'rate-limited').length, 10);
});

const session = (id: string, init: Partial<Session> = {}) => create(SessionSchema, { id, conversationId: `${id}-c`, collectionId: 'alpha', title: `Title ${id}`, provider: 'claude', model: 'opus', status: 'ready', lastMessage: 'a secret line', workingDirectory: '/home/me/x', createdAtUnixMillis: 1000n, ...init });
const alpha = [create(CollectionSchema, { id: 'alpha', name: 'Alpha', root: '/home/me/alpha', state: 'ready' })];

test('conversations go out with titles and status, never messages, paths or accounts', () => {
  const { conversations } = projectConversations([session('a'), session('b', { conversationId: 'a-c', createdAtUnixMillis: 500n }), session('h', { purpose: 'help' })], alpha);
  assert.equal(conversations.length, 1, 'one row per conversation; assistants are not work');
  assert.deepEqual(Object.keys(conversations[0]).sort(), ['busy', 'closed', 'conversationId', 'createdAt', 'model', 'provider', 'sessionId', 'status', 'title', 'updatedAt', 'workspaceId', 'workspaceName']);
  assert.equal(conversations[0].workspaceName, 'Alpha');
  assert.doesNotMatch(JSON.stringify(conversations), /secret|\/home/);
});

test('attention goes out as the Inbox shows it', () => {
  const { items, counts } = projectAttention([session('w', { status: 'waiting_input' })], alpha, { since: 0, marks: {} });
  assert.equal(items[0].state, 'needs-input');
  assert.equal(counts['needs-input'], 1);
  assert.doesNotMatch(JSON.stringify(items), /\/home/);
});

test('usage sums priced groups in dollars and counts the unpriced', () => {
  const report = create(TelemetryReportSchema, { groups: [
    { scope: { workspace: 'alpha', conversation: 'a-c', provider: 'claude' }, model: 'opus', nanos: 1_500_000_000n, records: 2n, tokens: { input: 10n, output: 5n, cacheRead: 3n, cacheWrite5m: 1n } },
    { scope: { workspace: 'alpha' }, model: 'other', records: 1n, unpriced: 1n },
  ] });
  const usage = projectUsage([{ workspaceId: 'alpha', report }], alpha);
  assert.equal(usage.totalUsd, 1.5);
  assert.equal(usage.unpriced, 1);
  assert.deepEqual(usage.groups[0], { workspaceId: 'alpha', workspaceName: 'Alpha', conversationId: 'a-c', model: 'opus', provider: 'claude', inputTokens: 10, outputTokens: 5, cacheReadTokens: 3, cacheWriteTokens: 1, usd: 1.5, unpriced: 0, records: 2 });
  assert.equal(usage.groups[1].usd, null);
  assert.equal('series' in usage, false, 'no series unless asked');
});

test('usage hour by hour, oldest first, when asked', () => {
  const hour = 1_763_283_600_000;
  const report = create(TelemetryReportSchema, { series: [
    { hourUnixMillis: BigInt(hour + 3_600_000), conversation: 'a-c', provider: 'claude', model: 'opus', nanos: 250_000_000n, records: 1n, tokens: { input: 4n, cacheRead: 6n, cacheWrite1h: 2n } },
    { hourUnixMillis: BigInt(hour), conversation: 'a-c', provider: 'claude', model: 'mystery', records: 1n, unpriced: 1n, tokens: { output: 3n } },
  ] });
  const { series } = projectUsage([{ workspaceId: 'alpha', report }], alpha, { series: true });
  assert.deepEqual(series?.map(b => b.at), [hour, hour + 3_600_000]);
  assert.deepEqual(series?.[1], { at: hour + 3_600_000, workspaceId: 'alpha', workspaceName: 'Alpha', conversationId: 'a-c', model: 'opus', provider: 'claude', inputTokens: 4, outputTokens: 0, cacheReadTokens: 6, cacheWriteTokens: 2, usd: 0.25, records: 1, unpriced: 0 });
  assert.equal(series?.[0].usd, null);
});

test('the bridge client, its types and the Runner agree on every method and grant', () => {
  const sdk = runtime('lazybox.js'), types = runtime('lazybox.d.ts');
  const called = new Set([...sdk.matchAll(/call\('([a-z.]+)'/g)].map(m => m[1]));
  assert.deepEqual([...called].sort(), Object.keys(bridgeMethods).sort());
  assert.match(sdk, new RegExp(`var PROTOCOL = ${BRIDGE_PROTOCOL};`));
  const goGrants = [...goSource.matchAll(/Grant\w+\s+= "([a-z:]+)"/g)].map(m => m[1]).filter(g => g !== 'do:');
  const bridgeGrants = [...new Set(Object.values(bridgeMethods).filter(g => g && g !== 'do:'))];
  // The Runner itself acts on these, not a panel: it schedules scan:disk and serves its report
  // on the content origin, answers routine questions (Clone Me) and reviews archives (Compound Engineering).
  const runnerGrants = ['scan:disk', 'answer:preferences', 'review:archives'];
  assert.deepEqual(goGrants.sort(), [...bridgeGrants, ...runnerGrants].sort(), 'each fixed grant has a bridge method or a Runner workflow');
  for (const grant of [...bridgeGrants, 'do:']) assert.ok(types.includes(grant), `lazybox.d.ts documents ${grant}`);
  for (const grant of goGrants) assert.notEqual(describeGrant(grant).icon, 'alert', `the sheet describes ${grant}`);
  assert.equal(describeGrant('do:inbox.open').title, 'Run “Open the inbox”');
});

test('every icon an extension may name is one the app draws', () => {
  const icons = readFileSync(new URL('../packages/surfaces/src/Icons.tsx', import.meta.url), 'utf8');
  const allowed = /ExtensionIcons = \[\]string\{([^}]*)\}/.exec(goSource)![1].match(/"([a-z-]+)"/g)!.map(s => s.slice(1, -1));
  for (const icon of allowed) assert.match(icons, new RegExp(`^\\s+'?${icon}'?: \\[`, 'm'), icon);
});

test('the content policies never loosen: no inline code, no same-origin, network only as approved', () => {
  const policy = JSON.parse(runtime('policy.json'));
  for (const text of [policy.shell, policy.extension]) {
    assert.match(text, /^default-src 'none';/);
    assert.match(text, /sandbox allow-scripts/);
    assert.match(text, /webrtc 'block';/);
    for (const never of ["'unsafe-inline'", "'unsafe-eval'", "'self'", 'allow-same-origin', 'allow-top-navigation', 'allow-popups', 'allow-forms', ' *', 'data: ;']) assert.ok(!text.includes(never), never);
  }
  assert.match(policy.shell, /frame-src \{base\};/);
  assert.match(policy.extension, /connect-src \{hosts\};/);
  assert.match(policy.extension, /frame-src 'none'/);
});

test('an update that asks for more names exactly what is new', () => {
  assert.deepEqual(widening({ grants: ['read:sessions', 'send:messages'], hosts: ['https://a.com'], approvedGrants: ['read:sessions'], approvedHosts: [] }), { grants: ['send:messages'], hosts: ['https://a.com'] });
});
