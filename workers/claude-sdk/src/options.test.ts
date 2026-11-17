import { strict as assert } from 'node:assert';
import { test } from 'node:test';
import { nativeOptions } from './options.js';

test('default Claude startup preserves native permissions, settings chain, and account context', () => {
  const env = Object.freeze({ CLAUDE_CONFIG_DIR: '/fixture/current-profile', CLAUDE_CODE_OAUTH_TOKEN: 'fixture-subscription', ANTHROPIC_BASE_URL: 'https://fixture-existing-route.invalid' });
  const options = nativeOptions({ cwd: '/fixture/collection', claudeAiConnectors: true }, env);
  for (const key of ['permissionMode', 'settingSources', 'settings', 'allowedTools', 'disallowedTools', 'allowDangerouslySkipPermissions', 'sandbox']) {
    assert.equal(Object.hasOwn(options, key), false, `startup overrides ${key}`);
  }
  assert.equal(options.env, env);
  assert.equal(options.model, undefined);
  assert.equal(env.CLAUDE_CODE_OAUTH_TOKEN, 'fixture-subscription');
});

test('claude.ai connectors are off unless the person turned them on, whatever the permission mode', () => {
  const env = Object.freeze({ CLAUDE_CONFIG_DIR: '/fixture/current-profile', CLAUDE_CODE_OAUTH_TOKEN: 'fixture-subscription' });
  // Off: the typed setting in the flag layer (it adds to the settings chain, replaces none of it), and the older env switch.
  for (const value of [undefined, false]) {
    const options = nativeOptions({ cwd: '/fixture', claudeAiConnectors: value }, env);
    assert.deepEqual(options.settings, { disableClaudeAiConnectors: true }, String(value));
    assert.equal(options.settingSources, undefined);
    assert.deepEqual(options.env, { ...env, ENABLE_CLAUDEAI_MCP_SERVERS: 'false' });
    assert.equal(options.env!.CLAUDE_CODE_OAUTH_TOKEN, 'fixture-subscription');
  }
  assert.deepEqual(nativeOptions({ cwd: '/fixture', permission: 'ask' }, {}).settings, { permissions: { ask: ['Bash', 'mcp__lazybox__command'] }, disableClaudeAiConnectors: true });
  assert.equal((nativeOptions({ cwd: '/fixture', purpose: 'help' }, {}).settings as { disableClaudeAiConnectors?: boolean }).disableClaudeAiConnectors, true);
  for (const permission of ['read-only', 'yolo', 'inherit']) assert.deepEqual(nativeOptions({ cwd: '/fixture', permission }, {}).settings, { disableClaudeAiConnectors: true }, permission);
  // On: nothing is added, and the permission settings stay as they were.
  const on = nativeOptions({ cwd: '/fixture', permission: 'ask', claudeAiConnectors: true }, env);
  assert.deepEqual(on.settings, { permissions: { ask: ['Bash', 'mcp__lazybox__command'] } });
  assert.equal(on.env, env);
});

test('explicit permission choices remain explicit and retain saved settings sources', () => {
  const ask = nativeOptions({ cwd: '/fixture', permission: 'ask', claudeAiConnectors: true }, {});
  assert.equal(ask.permissionMode, 'default');
  assert.equal(ask.settingSources, undefined);
  assert.deepEqual(ask.settings, { permissions: { ask: ['Bash', 'mcp__lazybox__command'] } });
  const readOnly = nativeOptions({ cwd: '/fixture', permission: 'read-only', claudeAiConnectors: true }, {});
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

test('sub-agent text is forwarded; progress summaries only for coding agents', () => {
  const coding = nativeOptions({ cwd: '/fixture' }, {});
  assert.equal(coding.forwardSubagentText, true);
  assert.equal(coding.agentProgressSummaries, true);
  for (const purpose of ['help', 'planning', 'summary']) {
    const assistant = nativeOptions({ cwd: '/fixture', purpose }, {});
    assert.equal(assistant.forwardSubagentText, true);
    assert.equal(assistant.agentProgressSummaries, undefined, purpose);
  }
});

// CLAUDE_COMPLIANCE.md: with an Anthropic API key saved, the Runner gives the
// worker ANTHROPIC_API_KEY and no other Claude credential. The worker hands
// that environment to Claude Code as it is, whatever the purpose, permission
// or connector setting: it neither drops the key nor brings a sign-in back.
test('an API key from the Runner reaches Claude Code untouched', () => {
  const key = 'sk-ant-api03-fixture-not-a-real-key';
  const env = Object.freeze({ PATH: '/usr/bin', ANTHROPIC_API_KEY: key });
  for (const o of [{ cwd: '/fixture' }, { cwd: '/fixture', claudeAiConnectors: true }, { cwd: '/fixture', purpose: 'help' }, { cwd: '/fixture', permission: 'yolo', browser: true }]) {
    const passed = nativeOptions(o, env).env!;
    assert.equal(passed.ANTHROPIC_API_KEY, key, JSON.stringify(o));
    for (const name of ['CLAUDE_CODE_OAUTH_TOKEN', 'ANTHROPIC_AUTH_TOKEN', 'CLAUDE_CODE_USE_BEDROCK']) assert.equal(name in passed, false, `${name} appeared`);
  }
});
