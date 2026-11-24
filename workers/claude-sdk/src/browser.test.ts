import { strict as assert } from 'node:assert';
import { test } from 'node:test';
import { nativeOptions } from './options.js';
import { browserContent, browserRule, browserServer, browserServerName, browserToolDefinitions } from './browser.js';

const names = ['browser_open', 'browser_navigate', 'browser_snapshot', 'browser_click', 'browser_type', 'browser_select', 'browser_press', 'browser_scroll', 'browser_screenshot', 'browser_back', 'browser_forward', 'browser_wait_for', 'browser_close'];

test('the browser server exposes every browser tool and routes each to the Runner tool of the same name', async () => {
  const calls: [string, unknown][] = [];
  const tools = browserToolDefinitions(async (name, args) => { calls.push([name, args]); return JSON.stringify({ text: `ran ${name}` }); });
  assert.deepEqual(tools.map(t => t.name), names);
  for (const t of tools) {
    const result = await t.handler({ ref: 'e1' } as never, undefined);
    assert.deepEqual(result.content, [{ type: 'text', text: `ran ${t.name}` }]);
  }
  assert.deepEqual(calls.map(([name]) => name), names);
  assert.deepEqual(calls[3], ['browser_click', { ref: 'e1' }]);
  const server = browserServer(async () => '');
  assert.equal(server.name, browserServerName);
  assert.ok(server.instance);
});

test('a screenshot reaches Claude as an image and a refusal as an error', async () => {
  assert.deepEqual(browserContent(JSON.stringify({ text: 'Screenshot of Docs', image: { mediaType: 'image/jpeg', data: 'AAAA' } })),
    [{ type: 'text', text: 'Screenshot of Docs' }, { type: 'image', data: 'AAAA', mimeType: 'image/jpeg' }]);
  assert.deepEqual(browserContent('plain'), [{ type: 'text', text: 'plain' }]);
  const [open] = browserToolDefinitions(async () => { throw new Error(JSON.stringify({ message: 'the person did not allow docs.example: Not now' })); });
  const refused = await open.handler({ url: 'https://docs.example/' } as never, undefined);
  assert.equal(refused.isError, true);
  assert.deepEqual(refused.content, [{ type: 'text', text: 'the person did not allow docs.example: Not now' }]);
});

test('the browser tools are allowed without a per-call prompt in every coding mode, never for the help assistant', () => {
  const allow = (o: Parameters<typeof nativeOptions>[0]) => ((nativeOptions(o, {}).settings as { permissions?: { allow?: string[] } } | undefined)?.permissions?.allow || []);
  for (const permission of [undefined, 'ask', 'read-only', 'yolo']) assert.ok(allow({ cwd: '/w', permission, browser: true }).includes(browserRule), String(permission));
  assert.deepEqual(nativeOptions({ cwd: '/w', permission: 'ask', browser: true }, {}).settings, { permissions: { ask: ['Bash', 'Monitor', 'PowerShell', 'mcp__lazybox__command'], allow: [browserRule] }, disableClaudeAiConnectors: true });
  assert.equal(allow({ cwd: '/w', permission: 'ask' }).includes(browserRule), false);
  assert.equal(allow({ cwd: '/w', purpose: 'help', browser: true }).includes(browserRule), false);
});
