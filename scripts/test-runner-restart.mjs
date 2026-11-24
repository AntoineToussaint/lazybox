// Restart Runner from the app, against an explicitly fake Runner: no launcher,
// Codefly, credentials or agents. The fixture answers RestartRunner, is down for
// a moment, then serves as a new process (another instance ID).
import {testViteCache} from './fixtures/vite-cache.mjs';
import { createServer } from 'vite';
import react from '@vitejs/plugin-react';
import { chromium, expect } from './fixtures/playwright.mjs';
import { fileURLToPath } from 'node:url';
import { mkdir } from 'node:fs/promises';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';
import { collectPageErrors } from './fixtures/page-errors.mjs';

const fixture = await startRunnerFixture();
const server = await createServer({...testViteCache(import.meta.url), configFile: false, root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: fixture.url } } } });
const restarts = () => fixture.mutations.filter(m => m.method === 'RestartRunner').length;
let browser, page;
try {
  await server.listen(); browser = await chromium.launch({ headless: true });
  page = await browser.newPage({ viewport: { width: 1280, height: 860 } });
  const errors = []; await collectPageErrors(page, errors);
  await page.addInitScript(() => localStorage.setItem('lazybox:workspace.conversation-tabs', 'off'));
  let historyTimedOut = false;
  await page.route('**/ListHistory', async route => {
    if (route.request().postDataJSON()?.limit !== 50) { await route.continue(); return; }
    assert.equal(route.request().headers()['connect-timeout-ms'], '15000');
    if (historyTimedOut) { await route.continue(); return; }
    await route.fulfill({ status: 504, json: { code: 'deadline_exceeded', message: 'Fixture history was slow' } });
    historyTimedOut = true;
  });
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace#token=${fixture.token}`);
  await page.getByRole('combobox', { name: 'Workspace', exact: true }).selectOption('alpha');
  await page.getByRole('combobox', { name: 'Session', exact: true }).selectOption('alpha-session');
  const message = page.getByRole('textbox', { name: 'Message', exact: true });
  await message.fill('draft that survives a Runner restart');
  await expect.poll(() => historyTimedOut).toBe(true);
  await expect(page.getByText('Could not refresh conversation history.', { exact: false })).toHaveCount(0);

  // The header status is the way in: it opens Settings at the Runner section.
  const indicator = page.locator('.workspace-header .runner-indicator');
  await expect(indicator).toHaveText('Connected');
  await mkdir('.cache', { recursive: true });

  const infoRpc = '**/Info';
  await page.route(infoRpc, route => {
    assert.equal(route.request().headers()['connect-timeout-ms'], '15000');
    return route.fulfill({ status: 504, json: { code: 'deadline_exceeded', message: 'Fixture Runner was slow' } });
  });
  await expect(indicator).toHaveText('Slow to respond');
  await expect(page.getByText('Could not refresh conversation history.', { exact: true })).toHaveCount(0);
  await expect(page.getByText('Runner disconnected.', { exact: true })).toHaveCount(0);
  await page.screenshot({ path: '.cache/runner-health-slow.png' });
  await expect(page.getByText('Runner is not responding.', { exact: true })).toBeVisible({ timeout: 10000 });
  await page.unroute(infoRpc);
  const resumedHistory = page.waitForRequest(request => request.url().endsWith('/ListHistory') && request.postDataJSON()?.limit === 50);
  await expect(indicator).toHaveText('Connected');
  await resumedHistory;
  await expect(page.getByText('Runner is not responding.', { exact: true })).toHaveCount(0);
  await expect(message).toHaveValue('draft that survives a Runner restart');

  await indicator.click();
  await expect(page.getByRole('tab', { name: 'Runner', exact: true })).toHaveAttribute('aria-selected', 'true');
  const section = page.getByRole('tabpanel', { name: 'Runner' });
  await expect(section.getByRole('heading', { name: 'Runner', exact: true })).toBeVisible();
  await expect(section).toContainText('Agents, managed commands and terminals keep running');

  // Keyboard: open the confirmation, then Escape leaves everything as it was.
  const restart = section.getByRole('button', { name: 'Restart Runner…', exact: true });
  await restart.focus(); await page.keyboard.press('Enter');
  let dialog = page.getByRole('dialog', { name: 'Restart the Runner?' });
  await expect(dialog).toBeVisible();
  await expect(dialog).toContainText('Agents, managed commands and terminals keep running');
  await expect(dialog.getByRole('button', { name: 'Restart Runner', exact: true })).toBeFocused();
  await page.screenshot({ path: '.cache/runner-restart-confirm.png' });
  await page.keyboard.press('Escape');
  await expect(dialog).toHaveCount(0);
  assert.equal(restarts(), 0, 'cancelling the confirmation restarted the Runner');

  // Confirm with the keyboard: the default action is Restart Runner.
  await restart.focus(); await page.keyboard.press('Enter');
  await expect(dialog.getByRole('button', { name: 'Restart Runner', exact: true })).toBeFocused();
  await page.keyboard.press('Enter');
  await expect(page.getByRole('status').filter({ hasText: 'Restarting the Runner…' })).toBeVisible();
  await expect(indicator).toHaveText('Restarting…');
  await expect(section.getByRole('button', { name: 'Restarting…', exact: true })).toBeDisabled();
  // While the Runner is down the expected outage is explained, not alarmed about.
  await expect.poll(() => fixture.runner.down).toBe(true);
  await page.waitForTimeout(1200);
  await expect(page.getByText('Runner disconnected.', { exact: true })).toHaveCount(0);
  await page.screenshot({ path: '.cache/runner-restart-restarting.png' });
  await expect(page.getByRole('status').filter({ hasText: 'Runner restarted.' })).toBeVisible({ timeout: 10000 });
  await expect(indicator).toHaveText('Restarted');
  await page.screenshot({ path: '.cache/runner-restart-restarted.png' });
  assert.equal(restarts(), 1, 'one confirmation must send exactly one restart');
  await expect(indicator).toHaveText('Connected', { timeout: 12000 });
  await expect(page.getByRole('status').filter({ hasText: 'Runner restarted.' })).toHaveCount(0);

  // Nothing about the window was lost: the draft is where it was.
  await page.getByRole('button', { name: 'Work', exact: true }).click();
  await expect(message).toHaveValue('draft that survives a Runner restart');

  // A refused restart says why, keeps the Runner as it is, and offers a retry.
  fixture.runner.restartError = 'record restart request: no space left on device';
  await indicator.click();
  await section.getByRole('button', { name: 'Restart Runner…', exact: true }).click();
  await page.getByRole('dialog', { name: 'Restart the Runner?' }).getByRole('button', { name: 'Restart Runner', exact: true }).click();
  const alert = page.getByRole('alert').filter({ hasText: 'Could not restart the Runner.' });
  await expect(alert).toContainText('no space left on device');
  await expect(indicator).toHaveText('Restart failed');
  await page.screenshot({ path: '.cache/runner-restart-failed.png' });
  fixture.runner.restartError = '';
  await alert.getByRole('button', { name: 'Try again', exact: true }).click();
  await expect(page.getByRole('status').filter({ hasText: 'Runner restarted.' })).toBeVisible({ timeout: 10000 });
  assert.equal(restarts(), 3);

  // A Runner nothing would start again offers no restart, and says what to do.
  fixture.runner.restartAvailable = false;
  await expect(section.getByRole('button', { name: /Restart Runner/ })).toHaveCount(0, { timeout: 10000 });
  await expect(section).toContainText('Restart is not available for this Runner.');
  await expect(section).toContainText('make runner-service-stop');
  await page.screenshot({ path: '.cache/runner-restart-unavailable.png' });
  assert.equal(restarts(), 3);
  assert.equal(fixture.calls.filter(c => ['OpenSession', 'Apply', 'CloseSession', 'StopSessionResources', 'CancelCommand', 'CloseTerminal'].includes(c.method)).length, 0, 'restarting touched an agent, command or terminal');
  assert.equal(errors.length, 0, errors.join('\n'));
  console.log('PASS: Restart Runner confirm (keyboard and Escape), restarting/restarted states without a disconnect alarm, retained draft, refused restart with retry, unavailable explanation. Fake Runner only.');
} catch (error) {
  if (page) await page.screenshot({ path: '.cache/runner-restart-failure.png', fullPage: true });
  throw error;
} finally { await browser?.close(); await server.close(); await fixture.close(); }
