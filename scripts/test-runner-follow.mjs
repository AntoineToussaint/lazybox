// A window follows its Runner across restarts nobody asked for in the app:
// a Codefly hot reload (new bearer, same port) and a launcher restart (new
// bearer, new port), plus a slow, a down and a returning Runner. The window is
// the desktop's (it follows the Runner locator with the shared hook and calls
// the Runner's own origin); only the native shell is replaced by an HTTP read of
// the locator. Fake Runner only: no launcher, Codefly, credentials or agents.
//
// The workspace list must never silently lose entries: no empty list while the
// Runner cannot be reached, no "create a workspace" for a Runner that has not
// answered yet, and no stale or empty bearer left behind by a restart.
import {testViteCache} from './fixtures/vite-cache.mjs';
import { createServer } from 'vite';
import react from '@vitejs/plugin-react';
import { chromium, expect } from '@playwright/test';
import { fileURLToPath } from 'node:url';
import { readFile, mkdir } from 'node:fs/promises';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';

const fixture = await startRunnerFixture();
const channel = fixture.openEvents('alpha-session');
channel.push({ type: 'message_delta', text: 'Before any restart.' });
// The window's locator: the endpoint file the Runner writes, as the native host reads it.
const locator = { name: 'fixture-locator', configureServer(server) { server.middlewares.use('/fixture-locator', async (_req, res) => { res.setHeader('Content-Type', 'application/json'); res.setHeader('Cache-Control', 'no-store'); res.end(await readFile(fixture.endpoint)); }); } };
const server = await createServer({...testViteCache(import.meta.url), configFile: false, root: fileURLToPath(new URL('./fixtures/desktop-window', import.meta.url)), plugins: [react(), locator], server: { host: '127.0.0.1', port: 0 } });
const names = ['Alpha feature', 'Beta release'];
let browser, page;
try {
  await server.listen(); browser = await chromium.launch({ headless: true });
  page = await browser.newPage({ viewport: { width: 1280, height: 860 } });
  const errors = []; page.on('pageerror', error => errors.push(String(error)));
  // The ordinary reread is slowed to a minute: only a refused or unreachable
  // request makes the window reread its locator, which is the path under test.
  const origin = `http://127.0.0.1:${server.httpServer.address().port}`;
  await page.goto(`${origin}/?interval=60000`);
  const list = page.getByRole('navigation', { name: 'Workspaces', exact: true });
  const indicator = page.locator('.workspace-header .runner-indicator');
  for (const name of names) await expect(list.getByText(name, { exact: true })).toBeVisible();
  await expect(indicator).toHaveText('Connected');
  await page.getByRole('combobox', { name: 'Workspace', exact: true }).selectOption('alpha');
  await page.getByRole('combobox', { name: 'Session', exact: true }).selectOption('alpha-session');
  await expect(page.getByText('Before any restart.')).toBeVisible();

  // Every entry stays listed, every 100 ms, while the Runner restarts under the window.
  const watchList = async (ms, during) => {
    let stop = false, missing = '';
    const watching = (async () => { while (!stop) { const shown = await list.innerText().catch(() => ''); for (const name of names) if (!shown.includes(name)) missing ||= `${name} disappeared ${during}`; await page.waitForTimeout(100); } })();
    await page.waitForTimeout(ms); stop = true; await watching;
    assert.equal(missing, '', missing);
  };
  const noAuthAlarm = async () => {
    for (const alert of await page.getByRole('alert').allInnerTexts()) assert.doesNotMatch(alert, /unauthenticated|bearer|credentials/i, `a restart left an alarm: ${alert}`);
  };

  // 1. A Codefly hot reload: same port, new bearer, the Runner's streams end.
  const reloadedAt = fixture.calls.length;
  await fixture.hotReload();
  channel.push({ type: 'message_delta', text: 'Streamed after the hot reload.' });
  // The conversation's stream reconnects with the new bearer and continues from its cursor.
  await expect(page.getByText('Streamed after the hot reload.')).toBeVisible({ timeout: 8000 });
  await watchList(3500, 'after a hot reload');
  await expect(indicator).toHaveText('Connected');
  await noAuthAlarm();
  assert(fixture.calls.slice(reloadedAt).some(call => call.method === 'ListCollections'), 'the list was not refreshed from the reloaded Runner');
  // The chat keeps one copy of the earlier text: it was not reloaded from the start.
  await expect(page.getByText('Before any restart.')).toHaveCount(1);

  // 2. A launcher restart nobody armed in this window: a new port and a new bearer.
  const movedAt = fixture.calls.length;
  await fixture.moveToNewPort();
  channel.push({ type: 'message_delta', text: 'Streamed from the new port.' });
  await expect(page.getByText('Streamed from the new port.')).toBeVisible({ timeout: 8000 });
  await watchList(3500, 'after a restart on a new port');
  await expect(indicator).toHaveText('Connected');
  await noAuthAlarm();
  assert(fixture.calls.slice(movedAt).some(call => call.method === 'ListCollections'), 'the window did not follow the Runner to its new port');
  await mkdir('.cache', { recursive: true }); await page.screenshot({ path: '.cache/runner-follow-moved.png' });

  // 3. The Runner is unreachable after the list was known: the list stays, and says it is reconnecting.
  fixture.runner.down = true;
  await expect(page.getByRole('status').filter({ hasText: 'Reconnecting to the Runner… These are the workspaces it last listed.' })).toBeVisible({ timeout: 8000 });
  await watchList(1000, 'while the Runner was down');
  await page.screenshot({ path: '.cache/runner-follow-down.png' });
  fixture.runner.down = false;
  await expect(page.getByRole('status').filter({ hasText: 'Reconnecting to the Runner' })).toHaveCount(0, { timeout: 8000 });
  await expect(indicator).toHaveText('Connected');

  // 4. A window opened while the Runner is down says it is reconnecting, never "no workspaces".
  fixture.runner.down = true;
  await page.reload();
  await expect(page.getByRole('status').filter({ hasText: 'Reconnecting to the Runner… Your workspaces appear as soon as it answers.' })).toBeVisible({ timeout: 8000 });
  await expect(page.getByText('Start a task: its repositories, an agent and your first message, in a workspace of its own.')).toHaveCount(0);
  await page.screenshot({ path: '.cache/runner-follow-reconnecting.png' });
  fixture.runner.down = false;
  for (const name of names) await expect(list.getByText(name, { exact: true })).toBeVisible({ timeout: 8000 });

  // 5. A slow first answer is loading, not empty.
  fixture.runner.listDelayMs = 2500;
  await page.reload();
  await expect(page.getByRole('status').filter({ hasText: 'Loading workspaces…' })).toBeVisible();
  await expect(page.getByText('Start a task: its repositories, an agent and your first message, in a workspace of its own.')).toHaveCount(0);
  for (const name of names) await expect(list.getByText(name, { exact: true })).toBeVisible({ timeout: 8000 });
  fixture.runner.listDelayMs = 0;

  // 6. A window that opens just as the Runner restarts adopts its successor's bearer on the first refusal.
  await fixture.hotReload();
  await page.reload();
  for (const name of names) await expect(list.getByText(name, { exact: true })).toBeVisible({ timeout: 8000 });
  await expect(indicator).toHaveText('Connected');

  // No request ever went out without a bearer, and only the restarts' own gap was refused.
  assert.equal(fixture.runner.refusedBearers.filter(refusal => !refusal.bearer || refusal.bearer === 'Bearer ').length, 0, 'a request was sent without a bearer');
  assert.equal(errors.length, 0, errors.join('\n'));
  console.log(`PASS: hot reload and unarmed restart on a new port followed without a reload (streams resumed from their cursor, ${fixture.runner.refusedBearers.length} stale-bearer requests retried), the workspace list never lost an entry, reconnecting and loading states instead of an empty list, no empty bearer. Fake Runner only.`);
} catch (error) {
  if (page) { await mkdir('.cache', { recursive: true }); await page.screenshot({ path: '.cache/runner-follow-failure.png', fullPage: true }); }
  throw error;
} finally { await browser?.close(); await server.close(); await fixture.close(); }
