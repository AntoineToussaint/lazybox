// Isolated UI regression: every RPC is intercepted; no Runner, credentials,
// provider process, or existing desktop session is used.
import {testViteCache} from './fixtures/vite-cache.mjs';
import { createServer } from 'vite';
import react from '@vitejs/plugin-react';
import { chromium, expect } from './fixtures/playwright.mjs';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';

const server = await createServer({...testViteCache(import.meta.url), configFile: false, root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: { host: '127.0.0.1', port: 0 } });
let browser;
try {
  await server.listen();
  const url = `http://127.0.0.1:${server.httpServer.address().port}/#token=startup-fixture`;
  browser = await chromium.launch({ headless: true });
  const page = await browser.newPage();
  const opens = [];
  // No account policy until the test turns one on (EXTENSIONS.md, "Account policy").
  let runnerSettings = {};
  await page.route('**/lazybox.bench.v1.RunnerService/*', async route => {
    const method = route.request().url().split('/').at(-1);
    if (method === 'Info') {
      return route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify({ profiles: [{ id: 'claude-default', provider: 'claude', isolation: 'inherited' }, { id: 'codex-default', provider: 'codex', isolation: 'inherited' }, { id: 'codex-work', provider: 'codex', isolation: 'environment' }] }) });
    }
    if (method === 'GetRunnerSettings') return route.fulfill({ json: runnerSettings });
    // The window's command palette and attention read the workspace list: a read, never a provider start.
    if (method === 'ListCollections') return route.fulfill({ json: { collections: [] } });
    // The agent picker reads which agents can run: a read too.
    if (method === 'GetProviderAvailability') return route.fulfill({ json: { providers: [] } });
    assert.equal(method, 'OpenSession', `unexpected provider RPC: ${method}`);
    opens.push(route.request().postDataJSON());
    return route.fulfill({ status: 503, contentType: 'application/json', body: JSON.stringify({ code: 'unavailable', message: 'Fixture stops before provider startup' }) });
  });
  await page.goto(url);
  await page.getByText('Model & context', { exact: true }).click();
  await expect(page.getByRole('combobox', { name: 'Permissions', exact: true })).toHaveValue('');
  await page.getByLabel('Working directory').fill('/fixture/collection');
  for (const provider of ['claude', 'codex']) {
    // The agent picker is a select-only combobox (AgentPicker.tsx).
    await page.getByRole('combobox', { name: 'Agent', exact: true }).click();
    await page.getByRole('listbox').getByRole('option', { name: provider === 'claude' ? /^Claude/ : /^Codex/ }).first().click();
    await expect(page.getByLabel('Account profile')).toHaveValue(`${provider}-default`);
    const before = opens.length;
    await page.getByRole('button', { name: `Start ${provider === 'claude' ? 'Claude' : 'Codex'} ↗` }).click();
    await expect(page.getByRole('alert')).toContainText('Fixture stops before provider startup');
    await expect.poll(() => opens.length).toBe(before + 1);
    const request = opens.at(-1);
    assert.equal(request.profileId, `${provider}-default`);
    assert.ok(!request.options?.permission, 'UI silently sets a permission policy');
    assert.ok(!request.options?.model, 'UI silently selects another model');
    assert.ok(!request.options?.gatewayId, 'UI silently changes the inference route');
    await expect(page.getByLabel('Account profile')).toHaveValue(`${provider}-default`);
  }
  await page.getByRole('combobox', { name: 'Permissions', exact: true }).selectOption('read-only');
  await page.getByRole('button', { name: 'Start Codex ↗' }).click();
  await expect.poll(() => opens.length).toBe(3);
  assert.equal(opens.at(-1).options.permission, 'read-only', 'explicit permission choice is lost');
  // Without an account policy nothing switches by itself: no choice to make.
  const rotation = page.getByRole('checkbox', { name: 'Switch subscriptions automatically', exact: true });
  await expect(rotation).toHaveCount(0);
  // With one for Codex (Subscription rotation allowed), rotation is on by default:
  // the Runner picks the pool; the person can keep this one conversation put.
  runnerSettings = { automaticSwitchingProviders: ['codex'] };
  await page.evaluate(() => window.dispatchEvent(new Event('lazybox-settings-saved')));
  await expect(rotation).toBeChecked();
  await expect(page.locator('.rotation-choice')).toContainText('When this account hits a limit: codex-work');
  assert.ok(!opens.at(-1).options?.noRotation && !opens.at(-1).options?.rotationProfileIds?.length, 'default start must leave the pool to the Runner');
  await rotation.uncheck();
  await expect(page.locator('.rotation-choice')).toContainText('Stays on this account, even at a limit');
  await page.getByRole('button', { name: 'Start Codex ↗' }).click();
  await expect.poll(() => opens.length).toBe(4);
  assert.equal(opens.at(-1).options.noRotation, true, 'opting out is not transmitted');
  assert.ok(!opens.at(-1).options.rotationProfileIds?.length);
  assert.equal(opens.at(-1).profileId, 'codex-default');
  console.log('PASS: both providers retain current permissions/account/model/route by default; failed startup does not switch profiles; no rotation choice without an account policy; with one, rotation is on by default and its opt-out is transmitted. All RPCs intercepted; no provider started.');
} finally {
  await browser?.close();
  await server.close();
}
