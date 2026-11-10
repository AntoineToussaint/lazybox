import { strict as assert } from 'node:assert';
import { test } from 'node:test';
import { nativeOptions } from './options.js';

test('default Claude startup preserves native permissions, settings, and account context', () => {
  const env = Object.freeze({ CLAUDE_CONFIG_DIR: '/fixture/current-profile', CLAUDE_CODE_OAUTH_TOKEN: 'fixture-subscription', ANTHROPIC_BASE_URL: 'https://fixture-existing-route.invalid' });
  const options = nativeOptions({ cwd: '/fixture/collection' }, env);
  for (const key of ['permissionMode', 'settingSources', 'settings', 'allowedTools', 'disallowedTools', 'allowDangerouslySkipPermissions', 'sandbox']) {
    assert.equal(Object.hasOwn(options, key), false, `startup overrides ${key}`);
  }
  assert.equal(options.env, env);
  assert.equal(options.model, undefined);
  assert.equal(env.CLAUDE_CODE_OAUTH_TOKEN, 'fixture-subscription');
});

test('explicit permission choices remain explicit and retain saved settings sources', () => {
  const ask = nativeOptions({ cwd: '/fixture', permission: 'ask' }, {});
  assert.equal(ask.permissionMode, 'default');
  assert.equal(ask.settingSources, undefined);
  assert.deepEqual(ask.settings, { permissions: { ask: ['Bash', 'mcp__lazybox__command'] } });
  const readOnly = nativeOptions({ cwd: '/fixture', permission: 'read-only' }, {});
  assert.equal(readOnly.permissionMode, 'plan');
  assert.equal(readOnly.settings, undefined);
  const plan = nativeOptions({ cwd: '/fixture', workflow: 'plan' }, {});
  assert.equal(plan.permissionMode, 'plan');
});

test('application assistant disables built-in and ambient MCP tools', () => {
  const options = nativeOptions({ cwd: '/fixture', purpose: 'help' }, {});
  assert.deepEqual(options.tools, []);
  assert.equal(options.strictMcpConfig, true);
});

test('YOLO is explicit, plan remains restricted, and context appends to native instructions', () => {
  const options = nativeOptions({ cwd:'/fixture', permission:'yolo', context:'Lazybox context' }, {});
  assert.equal(options.permissionMode, 'bypassPermissions');
  assert.equal(options.allowDangerouslySkipPermissions, true);
  assert.deepEqual(options.systemPrompt, {type:'preset',preset:'claude_code',append:'Lazybox context'});
  assert.equal(options.settingSources, undefined);
  const plan = nativeOptions({cwd:'/fixture',permission:'yolo',workflow:'plan'}, {});
  assert.equal(plan.permissionMode,'plan');
  assert.equal(plan.allowDangerouslySkipPermissions, undefined);
  assert.equal(nativeOptions({cwd:'/fixture',permission:'inherit'},{}).permissionMode, undefined);
});
