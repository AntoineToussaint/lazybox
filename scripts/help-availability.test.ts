import { test } from 'node:test';
import assert from 'node:assert/strict';
import type { Profile, ProviderAvailability, ProviderStatus, SubscriptionAvailability } from '@lazybox/protocol';
import { helpChoices, pickHelp, providerLine, providerNeeds, reportFromProfiles, usingLine } from '../packages/surfaces/src/help-availability.ts';

const now = Date.UTC(2026, 10, 16, 12, 0, 0);
const sub = (profileId: string, state: string, extra: Partial<SubscriptionAvailability> = {}) => ({ profileId, label: '', state, resetAt: 0n, detail: state === 'available' ? 'Signed in' : '', managed: false, isolation: 'inherited', ...extra }) as SubscriptionAvailability;
const provider = (name: 'claude' | 'codex', state: string, subscriptions: SubscriptionAvailability[], extra: Partial<ProviderStatus> = {}) =>
  ({ provider: name, title: name === 'claude' ? 'Claude Code' : 'Codex', state, installed: true, path: '/bin/' + name, version: name === 'claude' ? '2.1.311' : '0.98.0', source: 'path', error: '', installCommand: '', subscriptions, ...extra }) as ProviderStatus;
const report = (...providers: ProviderStatus[]) => ({ providers, checkedAtUnixMillis: BigInt(now) }) as ProviderAvailability;

test('Ask Lazybox uses the preferred provider’s best subscription', () => {
  const r = report(
    provider('claude', 'ready', [sub('claude-work', 'available', { label: 'Work subscription', managed: true, isolation: 'environment' }), sub('claude-default', 'available')]),
    provider('codex', 'ready', [sub('codex-default', 'available')]),
  );
  const pick = pickHelp(r, 'claude', '', now);
  assert.equal(pick.kind, 'ready');
  if (pick.kind !== 'ready') return;
  assert.equal(pick.choice.profileId, 'claude-work');
  assert.equal(pick.fallback, false);
  assert.equal(usingLine(pick.choice), 'using Claude (Work subscription)');
  // A person who uses Codex gets Codex.
  const codex = pickHelp(r, 'codex', '', now);
  assert.equal(codex.kind === 'ready' && codex.choice.profileId, 'codex-default');
  assert.equal(codex.kind === 'ready' && usingLine(codex.choice), 'using Codex (Codex CLI sign-in)');
});

test('a limited or signed-out provider falls back to the other one', () => {
  const resetAt = BigInt(Math.floor(now / 1000) + 3600);
  const r = report(
    provider('claude', 'limited', [sub('claude-default', 'limited', { resetAt })]),
    provider('codex', 'ready', [sub('codex-default', 'available')]),
  );
  const pick = pickHelp(r, 'claude', '', now);
  assert.ok(pick.kind === 'ready' && pick.choice.provider === 'codex' && pick.fallback);
  // The limited one is still listed (last), with when it is back, and may be chosen.
  const choices = helpChoices(r, 'claude', now);
  assert.deepEqual(choices.map(c => [c.profileId, c.usable]), [['codex-default', true], ['claude-default', false]]);
  assert.match(choices[1].detail, /^Limit reached · back at /);
  const chosen = pickHelp(r, 'claude', 'claude-default', now);
  assert.ok(chosen.kind === 'ready' && chosen.chosen && chosen.choice.profileId === 'claude-default');
  // A choice that no longer exists is ignored.
  const gone = pickHelp(r, 'claude', 'claude-removed', now);
  assert.ok(gone.kind === 'ready' && gone.choice.profileId === 'codex-default');
  // Signed out everywhere, or a program that cannot run: nothing to pick.
  assert.deepEqual(pickHelp(report(provider('claude', 'sign_in', [sub('claude-default', 'sign_in')]), provider('codex', 'unusable', [sub('codex-default', 'available')], { error: 'codex does not run' })), 'claude', '', now), { kind: 'none' });
  assert.deepEqual(pickHelp(undefined, 'claude'), { kind: 'loading' });
});

test('with nothing ready, each provider says what it needs and how to fix it', () => {
  const r = report(
    provider('claude', 'sign_in', [sub('claude-default', 'sign_in', { detail: 'Not signed in' })]),
    provider('codex', 'not_installed', [sub('codex-default', 'sign_in')], { installed: false, path: '', version: '', error: 'Codex is not installed on the Runner’s computer.', installCommand: 'npm install -g @openai/codex' }),
  );
  const [claude, codex] = providerNeeds(r, now);
  assert.equal(claude.headline, 'Installed 2.1.311 · not signed in');
  assert.equal(claude.action, 'subscription');
  assert.equal(codex.headline, 'Not installed');
  assert.equal(codex.action, 'install');
  assert.equal(codex.command, 'npm install -g @openai/codex');
  assert.equal(providerLine(r.providers[1]), 'Codex not installed');
  assert.equal(providerLine(r.providers[0]), 'Claude Code 2.1.311 installed · not signed in');
  assert.equal(providerLine(provider('claude', 'ready', [sub('claude-default', 'available')])), 'Claude Code 2.1.311 installed · signed in');
  assert.equal(providerLine(provider('claude', 'unusable', [], { error: 'no worker' })), 'Claude Code 2.1.311 installed · cannot run');
});

test('an older Runner’s profiles stand in for the report', () => {
  const profiles = [
    { id: 'claude-default', provider: 'claude', isolation: 'inherited', label: '', managed: false, availability: { state: 'auth_required' } },
    { id: 'codex-default', provider: 'codex', isolation: 'inherited', label: '', managed: false, availability: { state: 'unknown' } },
  ] as unknown as Profile[];
  const r = reportFromProfiles(profiles, now);
  assert.deepEqual(r.providers.map(p => p.state), ['sign_in', 'ready']);
  const pick = pickHelp(r, 'claude', '', now);
  assert.ok(pick.kind === 'ready' && pick.choice.profileId === 'codex-default');
});
