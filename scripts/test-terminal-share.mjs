// A command piped from a workspace terminal (`cmd | lazybox log`) shows in the
// console of the conversation being viewed, labelled as the person's own run.
// "Send to agent" puts its compact view, never the full output, in the message
// box; nothing is sent until the person sends. A run shared from the terminal
// with `--to` lands in that conversation's message box by itself, once.
import {testViteCache} from './fixtures/vite-cache.mjs';
import { createServer } from 'vite';
import react from '@vitejs/plugin-react';
import { chromium, webkit, expect } from './fixtures/playwright.mjs';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';

const fixture = await startRunnerFixture();
const server = await createServer({...testViteCache(import.meta.url), configFile: false, root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: fixture.url } } } });
let browser;
try {
  await server.listen();
  browser = await (process.env.TEST_WEBKIT ? webkit : chromium).launch({ headless: true });
  const page = await (await browser.newContext({ viewport: { width: 1400, height: 1000 } })).newPage();
  const errors = []; page.on('pageerror', error => errors.push(String(error)));
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace#token=${fixture.token}`);
  await page.getByRole('combobox', { name: 'Workspace', exact: true }).selectOption('alpha');
  await page.getByRole('combobox', { name: 'Session', exact: true }).selectOption('alpha-session');
  const chat = page.locator('[data-panel-id="alpha-work-chat"]');
  const composer = chat.getByRole('textbox', { name: 'Message', exact: true });
  await expect(composer).toBeEnabled();
  // Command output is a dock group, minimized to its tab until a run starts.
  const minimized = () => page.locator('.dv-groupview').filter({ has: page.locator('[data-kind="console"]') }).evaluate(node => node.classList.contains('dock-group-minimized'));
  const lists = () => fixture.calls.filter(call => call.method === 'ListCommands' && call.body.sessionId === 'alpha-session').length;
  const baseline = lists();
  await expect.poll(lists, { timeout: 5000 }).toBeGreaterThan(baseline + 1);

  // The person pipes a test run in the workspace terminal while drafting a message.
  await composer.click(); await composer.fill('The parser regressed:');
  const full = 'ok   example.com/lexer 0.01s\n'.repeat(20) + 'FULL_OUTPUT_ONLY_MARKER\n--- FAIL: TestParse (0.00s)\n';
  const view = '[lazybox] go test ./... → piped from a terminal, exit status unknown (2.3s)\n--- FAIL: TestParse (0.00s)\n    parser_test.go:42: want 3, got 4\nFAIL\texample.com/parser\n[lazybox] full output (1.4 MiB): /state/command-output/workspace-alpha/go-1.log\n';
  const run = fixture.startTerminalRun('alpha', 'go test ./...', { piped: true, exitCode: 0, outputBytes: '1468006', agentBytes: String(Buffer.byteLength(view)), filter: 'go-test', outputPath: '/state/command-output/workspace-alpha/go-1.log', agentView: view }, { output: full });

  // The console stays folded until the person opens it (COMMAND_OUTPUT.md): the run never takes the
  // screen or the composer's focus. Opening Command output shows that run and where it came from.
  // Two listings after the run started: the page has seen it.
  const seen = lists();
  await expect.poll(lists, { timeout: 5000 }).toBeGreaterThan(seen + 1);
  assert.equal(await minimized(), true, 'a run never unfolds the console by itself');
  await expect(composer).toBeFocused();
  await page.getByRole('tab', { name: 'Command output', exact: true }).click();
  await expect.poll(minimized, { timeout: 5000 }).toBe(false);
  const panel = page.locator('.command-panel');
  await expect(panel.locator('select')).toHaveValue(run.id);
  await expect(panel.locator('select option:checked')).toHaveText('✓ go test ./... · from your terminal · completed');
  await expect(panel.locator('.command-origin')).toBeVisible();
  await expect(panel.locator('.command-origin')).toHaveText('From your terminal');
  await expect(page.getByTestId('command-report')).toBeVisible();
  await expect(page.getByTestId('command-report')).toContainText('Piped, exit unknown · an agent would get a 247 B summary of 1.4 MiB (go-test filter)');
  await expect(page.getByTestId('console-output')).toContainText('FULL_OUTPUT_ONLY_MARKER');
  await expect(panel.locator('footer')).toContainText('upstream exit unknown');

  // "Send to agent" adds the compact view after the draft, as a fenced block.
  const send = panel.getByRole('button', { name: 'Send to agent', exact: true });
  await expect(send).toBeVisible();
  await expect(send).toBeEnabled();
  await send.click();
  const expected = 'The parser regressed:\n\nOutput of `go test ./...` from my terminal:\n```text\n' + view.trimEnd() + '\n```\n';
  await expect(composer).toHaveValue(expected);
  await expect(composer).toBeFocused();
  assert.equal(await composer.evaluate(box => box.selectionStart), expected.length, 'the cursor goes after the added output');
  await expect(panel.locator('.command-share').getByRole('status')).toContainText('Added 247 B to your message');
  await expect(chat.locator('.composer-handover')).toContainText('Review it, add what you want, and send.');
  assert.ok(!(await composer.inputValue()).includes('FULL_OUTPUT_ONLY_MARKER'), 'the agent never gets the full output');
  assert.equal(fixture.mutations.filter(m => m.method === 'Apply').length, 0, 'nothing is sent until the person sends');

  // The person sends: the agent receives the compact view.
  await composer.press('Enter');
  await expect.poll(() => fixture.mutations.filter(m => m.method === 'Apply').length, { timeout: 5000 }).toBe(1);
  const sent = fixture.mutations.find(m => m.method === 'Apply').body;
  assert.ok(sent.message.text.includes('parser_test.go:42: want 3, got 4') && sent.message.text.includes('/state/command-output/workspace-alpha/go-1.log'), 'the failure and the full-output path reach the agent');
  assert.ok(!sent.message.text.includes('FULL_OUTPUT_ONLY_MARKER') && sent.message.text.length < 1024, 'the agent never gets the full output');

  // While the output is still arriving, there is nothing to send yet.
  const running = fixture.startTerminalRun('alpha', 'cargo build', undefined, { done: false });
  await expect(panel.locator('select')).toHaveValue(running.id, { timeout: 5000 });
  await expect(panel.getByRole('button', { name: 'Send to agent', exact: true })).toBeDisabled();
  await expect(panel.locator('.command-share small')).toHaveText('You can send it to an agent when the output ends.');

  // Another workspace's terminal never shows here.
  const other = fixture.startTerminalRun('beta', 'beta secret build', { piped: true, agentView: 'beta' });
  const after = lists();
  await expect.poll(lists, { timeout: 5000 }).toBeGreaterThan(after + 1);
  assert.equal(await panel.locator(`select option[value="${other.id}"]`).count(), 0, 'a run from another workspace leaked into this console');

  // `lazybox log --to` from the terminal: the run lands in this conversation's message box by itself.
  await composer.fill('');
  const lint = '[lazybox] make lint → exit 2 (4.1s)\nsrc/parse.ts:12:3 error no-unused-vars\n';
  const shared = fixture.startTerminalRun('alpha', 'make lint', { exitCode: 2, outputBytes: '90000', agentBytes: String(Buffer.byteLength(lint)), filter: 'passthrough', agentView: lint }, { shareWith: 'alpha-session' });
  await expect(composer).toHaveValue('Output of `make lint` from my terminal:\n```text\n' + lint.trimEnd() + '\n```\n', { timeout: 5000 });
  await expect(chat.locator('.composer-handover')).toContainText('“make lint” was shared from your terminal');
  assert.ok(fixture.claimedShares.has(shared.id));
  assert.equal(fixture.mutations.filter(m => m.method === 'Apply').length, 1, 'a shared run is not sent by itself');

  // Claimed once: a reload keeps the draft but does not add the run again.
  await page.reload();
  await expect(composer).toHaveValue('Output of `make lint` from my terminal:\n```text\n' + lint.trimEnd() + '\n```\n', { timeout: 5000 });
  const claims = () => fixture.calls.filter(call => call.method === 'ClaimCommandShare').length;
  const before = claims();
  await expect.poll(claims, { timeout: 5000 }).toBeGreaterThan(before);
  await page.waitForTimeout(500);
  assert.equal((await composer.inputValue()).split('Output of `make lint`').length - 1, 1, 'a shared run is added to one message box once');

  assert.equal(errors.length, 0, errors.join('\n'));
  console.log('PASS: a terminal run shows in the workspace console; Send to agent and `--to` put its compact view, never the full output, in the message box, and the person decides when to send. Explicitly fake RPC fixture.');
} finally {
  await browser?.close(); await server.close(); await fixture.close();
}
