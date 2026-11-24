// CLAUDE_COMPLIANCE.md: how Settings and onboarding say what Claude agents
// sign in with, and the one-time notices, in words.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { accountNotices, claudeSignInLine, CLAUDE_INSTALL_COMMAND, shownNotices } from '../packages/surfaces/src/claude-auth-model.ts';
import { CLAUDE_TERMS_URL, describeGrant, switchedProviders } from '../packages/surfaces/src/extension-grants.ts';

test('the API key wins, then a cloud provider, then the person’s own Claude Code sign-in, said as unconfirmed', () => {
  const key = claudeSignInLine({ apiKeySet: true, apiKeyHint: 'wxyz', cloudProvider: 'bedrock' });
  assert.equal(key.title, 'Anthropic API key ending wxyz');
  assert.equal(key.tone, 'ok');
  assert.equal(claudeSignInLine({ apiKeySet: false, apiKeyHint: '', cloudProvider: 'bedrock' }).title, 'Amazon Bedrock');
  assert.equal(claudeSignInLine({ apiKeySet: false, apiKeyHint: '', cloudProvider: 'vertex' }).title, 'Google Cloud Vertex AI');
  const own = claudeSignInLine({ apiKeySet: false, apiKeyHint: '', cloudProvider: '' });
  assert.equal(own.title, 'Your Claude Code sign-in');
  assert.equal(own.tone, 'warn');
  assert.match(own.detail, /has not confirmed/);
  assert.equal(claudeSignInLine(undefined).title, 'Checking…');
});

test('Install Claude Code runs Anthropic’s own installer, the one its setup page names', () => {
  assert.equal(CLAUDE_INSTALL_COMMAND, 'curl -fsSL https://claude.ai/install.sh | bash');
  // The Runner's availability report offers the same command when Claude Code is missing.
  const runner = readFileSync(new URL('../services/runner/code/internal/runner/availability/availability.go', import.meta.url), 'utf8');
  assert.ok(runner.includes(`"claude": "${CLAUDE_INSTALL_COMMAND}"`));
});

test('the notices this client shows are the Runner’s, each with where to change it back', () => {
  assert.deepEqual(shownNotices(['automatic-switching-moved', 'something-newer', 'claude-sign-in-kept']), ['automatic-switching-moved', 'claude-sign-in-kept']);
  assert.deepEqual(shownNotices(undefined), []);
  assert.equal(accountNotices['automatic-switching-moved'].tab, 'extensions');
  assert.match(accountNotices['automatic-switching-moved'].body, /Settings → Extensions/);
  assert.equal(accountNotices['claude-sign-in-kept'].tab, 'agents');
  // The ids are the Runner's own (account_policy.go).
  const runner = ['runner/account_policy.go', 'power/disk_scan.go'].map(path => readFileSync(new URL('../services/runner/code/internal/' + path, import.meta.url), 'utf8')).join('\n');
  for (const id of Object.keys(accountNotices)) assert.ok(runner.includes(`"${id}"`), id);
});

test('the switch grants read as what they do, and say which providers they move', () => {
  assert.equal(describeGrant('switch:claude').title, 'Move Claude conversations to another of your Claude accounts');
  assert.notEqual(describeGrant('switch:codex').icon, 'alert');
  assert.deepEqual(switchedProviders({ grants: ['read:usage', 'switch:codex', 'switch:claude'] }), ['codex', 'claude']);
  assert.match(CLAUDE_TERMS_URL, /^https:\/\/code\.claude\.com\/docs\/en\/legal-and-compliance$/);
});

test('disk pressure notices are visible from the workspace and lead to Extensions', () => {
  for (const level of ['tight', 'critical']) {
    const id = 'disk-pressure-' + level;
    assert.deepEqual(shownNotices([id]), [id]);
    assert.equal(accountNotices[id].tab, 'extensions');
  }
});
