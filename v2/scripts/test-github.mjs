import { createServer } from 'vite';
import react from '@vitejs/plugin-react';
import { chromium, expect } from '@playwright/test';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';

const fixture = await startRunnerFixture();
const server = await createServer({ configFile: false, root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: fixture.url } } } });
let browser;
try {
  await server.listen(); browser = await chromium.launch({ headless: true });
  const context = await browser.newContext({ viewport: { width: 1400, height: 1000 }, permissions: ['clipboard-read', 'clipboard-write'] });
  // Keep this fixture entirely offline, including the sign-in browser handoff.
  await context.route('https://github.com/**', route => route.fulfill({ contentType: 'text/html', body: '<h1>GitHub fixture sign-in</h1>' }));
  const page = await context.newPage(); page.setDefaultTimeout(10_000); const errors = []; page.on('pageerror', error => errors.push(String(error)));
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace#token=${fixture.token}`);
  const settings = async () => { await page.getByRole('button', { name: 'Settings', exact: true }).click(); await page.getByRole('tab', { name: 'Repositories', exact: true }).click(); await page.getByRole('button',{name:'Connections & folders',exact:true}).first().click(); };
  await settings();
  await expect(page.getByRole('heading', { name: 'Connect with a personal token' })).toBeVisible();
  assert.equal(fixture.mutations.length, 0);
  await expect(page.getByLabel('App client ID', { exact: true })).toHaveCount(0);
  // The developer configures Lazybox's App on the Runner. End users only sign in.
  Object.assign(fixture.github, { state: 'not_connected', clientId: 'Iv.fixture', appSlug: 'fixture-app' });
  await page.reload(); await settings();
  await expect(page.getByRole('button', { name: 'Connect with GitHub App', exact: true })).toBeVisible();
  await page.getByRole('button', { name: 'Connect with GitHub App', exact: true }).click();
  await expect(page.getByLabel('GitHub sign-in code')).toHaveValue('TEST-CODE');
  await page.getByRole('button', { name: 'Copy code', exact: true }).click();
  await expect(page.getByRole('button', { name: 'Copied', exact: true })).toBeVisible();
  assert.equal(await page.evaluate(() => navigator.clipboard.readText()), 'TEST-CODE');
  await page.reload(); await settings();
  await expect(page.getByLabel('GitHub sign-in code')).toHaveValue('TEST-CODE');
  assert.equal(fixture.calls.filter(call => call.method === 'BeginGitHubSignIn').length, 1, 'Reload must recover the existing flow');
  fixture.completeGitHub();
  await expect(page.getByText('Connected as fixture-user', { exact: true })).toBeVisible();
  await expect(page.getByRole('heading',{name:'fixture-team',exact:true})).toBeVisible();
  await expect(page.getByRole('button',{name:'Remove fixture-team/api from suggestions',exact:true})).toBeVisible();
  assert.equal(fixture.library.length,2,'Discovery does not import repositories');
  fixture.github.state = 'needs_attention'; fixture.github.error = 'GitHub sign-in required';
  await page.reload(); await settings(); await expect(page.getByRole('button', { name: 'Connect with GitHub App', exact: true })).toBeVisible();
  await page.getByRole('button', { name: 'Disconnect', exact: true }).click();
  await page.getByRole('button', { name: 'Disconnect this Runner', exact: true }).click();
  await expect(page.getByRole('button', { name: 'Connect with GitHub App', exact: true })).toBeVisible();
  assert.equal(fixture.library.length, 2, 'Disconnect must keep repositories');
  assert.equal(fixture.calls.filter(call => call.method === 'CloseSession').length, 0);
  assert.deepEqual(errors, []);
  console.log('PASS: GitHub App connection inside shared repository browser, device-code copy, reload recovery, scoped discovery without clones, expired authorization and non-destructive disconnect.');
} finally { await browser?.close(); await server.close(); await fixture.close(); }
