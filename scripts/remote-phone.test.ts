import { create } from '@bufbuild/protobuf';
import { AgentEventSchema, CommandStatusSchema, SessionSchema } from '@lazybox/protocol';
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { deriveAttention } from '../packages/surfaces/src/attention.ts';
import { pushSupport, quietNow, base64urlToBytes, type PushEnvironment } from '../packages/surfaces/src/remote/push.ts';
import { chipAnswer, rowPlan } from '../packages/surfaces/src/remote/row-plan.ts';
import { parsePins, togglePin, maxPins, runnerPinStore } from '../packages/surfaces/src/remote/pins.ts';
import { parseRoute, routeSearch } from '../packages/surfaces/src/remote/route.ts';
import { phoneCommands } from '../packages/surfaces/src/remote/commands.ts';

const json = (value: unknown) => new TextEncoder().encode(JSON.stringify(value));
const android: PushEnvironment = { userAgent: 'Mozilla/5.0 (Linux; Android 15; Pixel 7) Chrome/131.0 Mobile Safari/537.36', maxTouchPoints: 5, standalone: false, serviceWorker: true, pushManager: true, notification: true };
const iphone = (version: string, standalone: boolean, push = standalone): PushEnvironment => ({ userAgent: `Mozilla/5.0 (iPhone; CPU iPhone OS ${version} like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1`, maxTouchPoints: 5, standalone, serviceWorker: true, pushManager: push, notification: push });

test('push needs the Home Screen on an iPhone, iOS 16.4 or later, and a browser with push', () => {
  assert.deepEqual(pushSupport(android), { kind: 'supported' });
  assert.deepEqual(pushSupport(iphone('18_1', false)), { kind: 'install' });
  assert.deepEqual(pushSupport(iphone('18_1', true)), { kind: 'supported' });
  assert.deepEqual(pushSupport(iphone('16_3', true)), { kind: 'old-ios', version: '16.3' });
  assert.deepEqual(pushSupport(iphone('16_4', false)), { kind: 'install' });
  // iPadOS says it is a Mac; touch gives it away.
  assert.deepEqual(pushSupport({ ...android, userAgent: 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.1 Safari/605.1.15', maxTouchPoints: 5 }), { kind: 'install' });
  assert.deepEqual(pushSupport({ ...android, pushManager: false }), { kind: 'unsupported' });
});

test('quiet hours hold within the window, across midnight too', () => {
  const at = (h: number, m = 0) => new Date(2026, 10, 16, h, m);
  const night = { quietStart: '22:00', quietEnd: '07:00' };
  assert.equal(quietNow(night, at(23)), true);
  assert.equal(quietNow(night, at(6, 59)), true);
  assert.equal(quietNow(night, at(7)), false);
  assert.equal(quietNow(night, at(12)), false);
  assert.equal(quietNow({ quietStart: '12:00', quietEnd: '14:00' }, at(13)), true);
  assert.equal(quietNow({ quietStart: '', quietEnd: '' }, at(23)), false);
  assert.equal(quietNow({ quietStart: '08:00', quietEnd: '08:00' }, at(8)), false);
});

test('the Runner key decodes to the bytes PushManager wants', () => {
  assert.deepEqual([...base64urlToBytes('BP-_AQ')], [4, 255, 191, 1]);
});

function needs(request: ReturnType<typeof create<typeof AgentEventSchema>>) {
  return deriveAttention(create(SessionSchema, { id: 's', conversationId: 'c', status: request.type === 'question' ? 'waiting_input' : 'waiting_approval', pending: [request], agents: [{ id: 'root', status: 'waiting', updatedAtUnixMillis: 5n }] }));
}
const approval = (input: unknown, extra: Record<string, unknown> = {}) => create(AgentEventSchema, { type: 'approval', requestId: 'a', text: 'Bash', providerDataJson: json({ input, ...extra }) });
const question = (questions: unknown[]) => create(AgentEventSchema, { type: 'question', requestId: 'q', providerDataJson: json({ input: { questions } }) });

test('an approval row allows a plain command, asks first for a flagged one, and opens what it cannot show', () => {
  const plain = rowPlan(needs(approval({ command: 'npm test' })), true);
  assert.equal(plain.kind, 'approval');
  assert.ok(plain.kind === 'approval' && plain.allow && !plain.confirm && plain.what === 'npm test');
  const flagged = rowPlan(needs(approval({ command: 'git push --force origin main' })), true);
  assert.ok(flagged.kind === 'approval' && flagged.allow && flagged.confirm);
  assert.deepEqual(flagged.kind === 'approval' && flagged.risks.map(r => r.kind), ['discard', 'network']);
  const careful = rowPlan(needs(approval({ command: 'ls' }, { defaultToNo: true })), true);
  assert.ok(careful.kind === 'approval' && careful.confirm, 'the provider’s default-to-no asks first');
  const edit = rowPlan(needs(approval({ file_path: '/w/a.ts', old_string: 'a', new_string: 'b' }, { tool: 'Edit' })), true);
  assert.ok(edit.kind === 'approval' && !edit.allow && /Read the change/.test(edit.openReason || ''), 'a diff is read before it is allowed');
  const settings = rowPlan(needs(approval({}, { tool: 'lazybox_update_settings' })), true);
  assert.ok(settings.kind === 'approval' && !settings.allow);
  assert.deepEqual(rowPlan(needs(approval({ command: 'npm test' })), false), { kind: 'none' }, 'a read-only phone gets no actions');
});

test('a question row offers chips only when one short question can be answered by tapping', () => {
  const single = rowPlan(needs(question([{ question: 'Which database?', options: [{ label: 'Postgres' }, { label: 'SQLite' }] }])), true);
  assert.ok(single.kind === 'question' && single.mode === 'chips');
  assert.deepEqual(single.kind === 'question' && chipAnswer(single.question, ['SQLite', 'ignored']), { 'Which database?': { values: ['SQLite'] } });
  const multi = rowPlan(needs(question([{ question: 'Which checks?', multiSelect: true, options: [{ label: 'lint' }, { label: 'test' }] }])), true);
  assert.ok(multi.kind === 'question' && multi.mode === 'multi');
  assert.deepEqual(multi.kind === 'question' && chipAnswer(multi.question, ['lint', 'test']), { 'Which checks?': { values: ['lint', 'test'] } });
  assert.ok((p => p.kind === 'question' && p.mode === 'open' && p.count === 2)(rowPlan(needs(question([{ question: 'A?', options: [{ label: 'x' }] }, { question: 'B?', options: [{ label: 'y' }] }])), true)), 'several questions open the conversation');
  assert.ok((p => p.kind === 'question' && p.mode === 'open')(rowPlan(needs(question([{ question: 'Name?' }])), true)), 'a typed answer opens the conversation');
  assert.ok((p => p.kind === 'question' && p.mode === 'open')(rowPlan(needs(question([{ question: 'Pick', options: Array.from({ length: 7 }, (_, i) => ({ label: `o${i}` })) }])), true)), 'too many options to tap');
  assert.ok((p => p.kind === 'question' && p.mode === 'open')(rowPlan(needs(question([{ question: 'Layout?', options: [{ label: 'A', preview: '+--+' }] }])), true)), 'previews need the conversation');
});

test('repository and browser requests are answered on the computer', () => {
  const repo = rowPlan(needs(create(AgentEventSchema, { type: 'repository_request', requestId: 'r', providerDataJson: json({ name: 'api' }) })), true);
  assert.equal(repo.kind, 'elsewhere');
});

test('pins toggle newest first, are bounded, and survive bad storage', () => {
  assert.deepEqual(togglePin(['a'], 'b'), ['b', 'a']);
  assert.deepEqual(togglePin(['b', 'a'], 'b'), ['a']);
  assert.equal(togglePin(Array.from({ length: maxPins }, (_, i) => `k${i}`), 'new').length, maxPins);
  assert.deepEqual(parsePins('not json'), []);
  assert.deepEqual(parsePins(JSON.stringify(['a', 3, 'b'])), ['a', 'b']);
});

test('the route round-trips through the URL, and a notification link opens its conversation', () => {
  assert.deepEqual(parseRoute('?view=remote&session=s1'), { session: 's1' });
  assert.deepEqual(parseRoute('?view=remote&session=s1&command=c1'), { session: 's1', command: 'c1' });
  assert.deepEqual(parseRoute('?view=remote&command=c1'), {}, 'a command needs its conversation');
  assert.deepEqual(parseRoute('?view=remote&screen=settings'), { screen: 'settings' });
  assert.equal(routeSearch('?view=remote&session=old', { session: 's2', command: 'c' }), '?view=remote&session=s2&command=c');
  assert.equal(routeSearch('?view=remote&session=old', {}), '?view=remote');
});

test('the commands strip shows running first, then the newest finished', () => {
  const c = (id: string, done: boolean) => create(CommandStatusSchema, { id, done });
  assert.deepEqual(phoneCommands([c('a', true), c('b', false), c('c', true)]).map(x => x.id), ['b', 'c', 'a']);
});

test("the Runner's pins: the computer's order, shown at once, undone when refused, read-only without respond", async () => {
  const calls: [string, boolean][] = [];
  let refuse = false, refreshed = 0;
  const runner = { pin: async (id: string, pinned: boolean) => { calls.push([id, pinned]); if (refuse) throw new Error('denied'); }, refresh: () => { refreshed++; } };
  const store = runnerPinStore(runner, { writable: true });
  let heard = 0; store.subscribe(() => { heard++; });
  store.update([{ key: 'a', sessionId: 'a1', pinnedAt: 200 }, { key: 'b', sessionId: 'b1', pinnedAt: 100 }, { key: 'c', sessionId: 'c1', pinnedAt: 0 }]);
  assert.deepEqual(store.list(), ['b', 'a'], 'oldest pin first, as on the computer');
  assert.equal(store.list(), store.list(), 'a stable list for useSyncExternalStore');
  store.set(togglePin(store.list(), 'c'));
  assert.deepEqual(store.list(), ['b', 'a', 'c'], 'a new pin shows at once');
  assert.deepEqual(calls, [['c1', true]]);
  await new Promise(resolve => setTimeout(resolve));
  assert.equal(refreshed, 1);
  // The Runner's next answer keeps the pin; nothing is pending.
  store.update([{ key: 'a', sessionId: 'a1', pinnedAt: 200 }, { key: 'b', sessionId: 'b1', pinnedAt: 100 }, { key: 'c', sessionId: 'c1', pinnedAt: 300 }]);
  assert.deepEqual(store.list(), ['b', 'a', 'c']);
  refuse = true;
  store.set(togglePin(store.list(), 'a'));
  assert.deepEqual(store.list(), ['b', 'c']);
  await new Promise(resolve => setTimeout(resolve));
  assert.deepEqual(store.list(), ['b', 'a', 'c'], 'a refused change is undone');
  assert.ok(heard >= 3);
  const reader = runnerPinStore(runner, { writable: false, legacy: ['c', 'x'] });
  reader.update([{ key: 'a', sessionId: 'a1', pinnedAt: 200 }, { key: 'c', sessionId: 'c1', pinnedAt: 0 }]);
  assert.equal(reader.shared, true); assert.equal(reader.writable, false);
  assert.deepEqual(reader.list(), ['a', 'c', 'x'], "the computer's pins, then the phone's own from before");
  reader.set([]);
  assert.deepEqual(reader.list(), ['a', 'c', 'x'], 'a read-only phone changes nothing');
  assert.equal(calls.length, 2);
});
