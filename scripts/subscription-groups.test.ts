import { test } from 'node:test';
import assert from 'node:assert/strict';
import { create } from '@bufbuild/protobuf';
import { ProfileSchema, type Profile } from '@lazybox/protocol';
import { compareProviders, groupByProvider, subscriptionGroups, subscriptionOption } from '../packages/surfaces/src/subscription-groups.ts';
import { orderSubscriptions, subscriptionChoices } from '../packages/surfaces/src/subscription-model.ts';

// 2026-11-16 12:00 local time.
const noon = new Date(2026, 10, 16, 12, 0, 0).getTime();
const at = (hours: number) => new Date(2026, 10, 16, hours, 0).getTime() / 1000;
const time = (seconds: number) => new Date(seconds * 1000).toLocaleTimeString(undefined, { hour: 'numeric', minute: '2-digit' });
const profile = (id: string, provider: string, label: string, availability?: Record<string, unknown>): Profile =>
  create(ProfileSchema, { id, provider, label, isolation: 'environment', managed: true, availability: availability as any });

// A flat list as the Runner reports it: providers mixed, in no useful order.
const mixed = [
  profile('codex-work', 'codex', 'Work', { state: 'available' }),
  profile('claude-personal', 'claude', 'Personal', { state: 'exhausted', resetAt: BigInt(at(18)) }),
  profile('gemini', 'acp:gemini', ''),
  profile('claude-work', 'claude', 'Work', { state: 'available' }),
  profile('claude-apple', 'claude', 'Apple', { state: 'unknown' }),
  profile('codex-personal', 'codex', 'Personal', { state: 'available' }),
  profile('goose', 'acp:goose', ''),
];

test('subscriptions group by provider: Claude, Codex, then ACP agents by name', () => {
  const groups = subscriptionGroups(mixed, { now: noon });
  assert.deepEqual(groups.map(g => g.provider), ['claude', 'codex', 'acp:gemini', 'acp:goose']);
  assert.deepEqual(groups.map(g => g.title), ['Claude', 'Codex', 'Gemini CLI', 'Goose']);
  assert.deepEqual(groups.map(g => g.icon), ['agent-claude', 'agent-codex', 'agent-gemini', 'agent-goose']);
});

test('within a group: available first, then not known to be limited, then limited; by name within each', () => {
  const [claude, codex] = subscriptionGroups(mixed, { now: noon });
  assert.deepEqual(claude.items.map(p => p.id), ['claude-work', 'claude-apple', 'claude-personal']);
  assert.deepEqual(codex.items.map(p => p.id), ['codex-personal', 'codex-work'], 'two available ones read by name');
});

test('the current subscription leads its group, whatever its state', () => {
  const [claude] = subscriptionGroups(mixed, { now: noon, currentId: 'claude-personal' });
  assert.deepEqual(claude.items.map(p => p.id), ['claude-personal', 'claude-work', 'claude-apple']);
  assert.deepEqual(subscriptionChoices(mixed, 'claude', 'claude-personal', noon).map(c => c.id), ['claude-personal', 'claude-work', 'claude-apple']);
  assert.deepEqual(subscriptionChoices(mixed, 'codex', 'claude-personal', noon).map(c => c.id), ['codex-personal', 'codex-work'], 'one provider only: a conversation cannot cross providers');
});

test('a limit whose reset has passed counts as ready again', () => {
  const later = at(19) * 1000;
  assert.deepEqual(orderSubscriptions(mixed.filter(p => p.provider === 'claude'), { now: later }).map(p => p.id), ['claude-work', 'claude-apple', 'claude-personal']);
  assert.equal(subscriptionOption(mixed[1], later), 'Personal', 'nothing to say once it is back');
});

test('names compare as people read them: case and numbers', () => {
  const rows = ['Work 10', 'work 2', 'Apple'].map((label, i) => profile(`p${i}`, 'claude', label, { state: 'available' }));
  assert.deepEqual(orderSubscriptions(rows).map(p => p.label), ['Apple', 'work 2', 'Work 10']);
});

test('a picker option says when a limited subscription is back', () => {
  assert.equal(subscriptionOption(mixed[3], noon), 'Work');
  assert.equal(subscriptionOption(mixed[1], noon), `Personal · Limit reached · back at ${time(at(18))}`);
  assert.equal(subscriptionOption(profile('s', 'claude', 'Signed out', { state: 'auth_required' }), noon), 'Signed out · Sign-in required');
  assert.equal(subscriptionOption(create(ProfileSchema, { id: 'claude-default', provider: 'claude', isolation: 'inherited' }), noon), 'Claude CLI sign-in');
});

test('grouping keeps the order it is given and works on any item', () => {
  const groups = groupByProvider([{ p: 'codex', n: 1 }, { p: 'claude', n: 2 }, { p: 'codex', n: 3 }], item => item.p);
  assert.deepEqual(groups.map(g => [g.provider, g.items.map(i => i.n)]), [['claude', [2]], ['codex', [1, 3]]]);
  assert.deepEqual(groupByProvider([], () => 'claude'), []);
  assert.ok(compareProviders('claude', 'codex') < 0 && compareProviders('codex', 'acp:aider') < 0 && compareProviders('acp:gemini', 'acp:goose') < 0);
});
