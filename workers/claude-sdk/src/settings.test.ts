import { strict as assert } from 'node:assert';
import { test } from 'node:test';
import { nativeOptions } from './options.js';
import { canonical, docsTools, readDocTool, readSettingsTool, searchDocsTool, settingsTools, SettingsApprovals, updateSettingsTool } from './settings.js';

const proposal = { revision: 3, reason: 'The user asked for approval prompts.', changes: { permission: 'ask' as const } };

test('help assistant reads settings freely and is always asked before an update, whatever the Runner permission', () => {
  for (const permission of [undefined, 'read-only', 'yolo', 'ask']) {
    const options = nativeOptions({ cwd: '/fixture', purpose: 'help', permission }, {});
    assert.equal(options.permissionMode, 'default');
    assert.equal(options.allowDangerouslySkipPermissions, undefined);
    assert.deepEqual(options.settings, { permissions: { allow: [readSettingsTool, searchDocsTool, readDocTool], ask: [updateSettingsTool] }, disableClaudeAiConnectors: true });
    assert.deepEqual(options.tools, []);
    assert.equal(options.strictMcpConfig, true);
  }
  assert.equal(nativeOptions({ cwd: '/fixture', purpose: 'planning', permission: 'read-only' }, {}).permissionMode, 'plan');
  assert.equal(nativeOptions({ cwd: '/fixture', permission: 'yolo' }, {}).permissionMode, 'bypassPermissions');
});

test('an approval lets exactly that update run once', () => {
  const approvals = new SettingsApprovals();
  assert.equal(approvals.consume(proposal), false);
  approvals.grant({ changes: { permission: 'ask' }, reason: proposal.reason, revision: 3 });
  assert.equal(approvals.consume({ ...proposal, changes: { permission: 'yolo' } }), false);
  assert.equal(approvals.consume(proposal), true);
  assert.equal(approvals.consume(proposal), false);
  approvals.grant({ ...proposal, changes: { prices: 'x' } });
  assert.equal(approvals.consume({ ...proposal, changes: { prices: 'x' } }), false);
  assert.equal(canonical({ b: 1, a: { d: 2, c: 3 } }), '{"a":{"c":3,"d":2},"b":1}');
});

test('an unapproved update never reaches the Runner; an approved one does, once', async () => {
  const calls: Array<[string, unknown]> = [];
  const approvals = new SettingsApprovals();
  const { read, update } = settingsTools(async (name, args) => { calls.push([name, args]); return '{"applied":true}'; }, approvals);
  assert.equal(read.name, 'get_settings');
  assert.equal(update.name, 'update_settings');
  const refused = await update.handler(proposal, undefined);
  assert.equal(refused.isError, true);
  assert.equal(calls.length, 0);
  approvals.grant(proposal);
  const applied = await update.handler(proposal, undefined);
  assert.equal(applied.isError, undefined);
  assert.deepEqual(calls, [['lazybox_update_settings', proposal]]);
  assert.equal((await update.handler(proposal, undefined)).isError, true);
  assert.equal(calls.length, 1);
  await read.handler({}, undefined);
  assert.deepEqual(calls[1], ['lazybox_get_settings', {}]);
});

test('a Runner refusal reaches the agent as a tool error with its message', async () => {
  const approvals = new SettingsApprovals();
  const { update } = settingsTools(async () => { throw new Error(JSON.stringify({ code: -32601, message: 'lazybox settings changed since revision 3 (now 4)' })); }, approvals);
  approvals.grant(proposal);
  const result = await update.handler(proposal, undefined);
  assert.equal(result.isError, true);
  assert.deepEqual(result.content, [{ type: 'text', text: 'lazybox settings changed since revision 3 (now 4)' }]);
});

test('the documentation tools read through the Runner and never ask', async () => {
  const calls: Array<[string, unknown]> = [];
  const { search, page } = docsTools(async (name, args) => { calls.push([name, args]); return name === 'lazybox_read_doc' ? '# Chat' : '{"pages":[]}'; });
  assert.equal(search.name, 'search_docs');
  assert.equal(page.name, 'read_doc');
  assert.equal((await search.handler({ query: 'subscription limit' }, undefined)).isError, undefined);
  assert.deepEqual(await page.handler({ path: 'guides/chat.md' }, undefined), { content: [{ type: 'text', text: '# Chat' }] });
  assert.deepEqual(calls, [['lazybox_search_docs', { query: 'subscription limit' }], ['lazybox_read_doc', { path: 'guides/chat.md' }]]);
  const missing = await docsTools(async () => { throw new Error(JSON.stringify({ message: 'the Lazybox documentation is not available on this Runner' })); }).page.handler({ path: 'x.md' }, undefined);
  assert.equal(missing.isError, true);
});
