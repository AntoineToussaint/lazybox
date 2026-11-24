import {testViteCache} from './fixtures/vite-cache.mjs';
import { createServer } from './fixtures/vite-cache.mjs';
import react from '@vitejs/plugin-react';
import { chromium, expect } from './fixtures/playwright.mjs';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';
import { collectPageErrors } from './fixtures/page-errors.mjs';

const fixture = await startRunnerFixture();
const server = await createServer({...testViteCache(import.meta.url), configFile: false, root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: fixture.url } } } });
let browser;
try {
  await server.listen(); browser = await chromium.launch({ headless: true });
  const page = await browser.newPage(); page.setDefaultTimeout(10_000);
  const errors = []; await collectPageErrors(page, errors);
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace#token=${fixture.token}`);
  const settings = async () => { await page.getByRole('button', { name: 'Settings', exact: true }).click(); await page.getByRole('tab', { name: 'Repositories', exact: true }).click(); await page.getByRole('button',{name:'Connections & folders',exact:true}).first().click(); };
  await settings();
  await expect(page.getByRole('heading', { name: 'Connect with a personal token' })).toBeVisible();
  await expect(page.getByRole('button', { name: 'Connect with GitHub App', exact: true })).toHaveCount(0);
  const token = page.getByLabel('Personal access token', { exact: true });
  await expect(token).toHaveAttribute('type', 'password');
  await token.fill('ghp_rejected_fixture');
  await page.getByRole('button', { name: 'Connect with token', exact: true }).click();
  await expect(page.getByRole('alert')).toContainText('Token rejected');
  await expect(token).toHaveValue('');
  await token.fill('ghp_offline_fixture');
  await page.getByRole('button', { name: 'Connect with token', exact: true }).click();
  await expect(page.getByText('Connected as fixture-user', { exact: true })).toBeVisible();
  await expect(page.getByRole('heading',{name:'fixture-team',exact:true})).toBeVisible();
  await expect(page.getByRole('button',{name:'Remove fixture-team/api from suggestions',exact:true})).toBeVisible();
  await expect(page.getByRole('button', { name: 'Manage repository access ↗', exact: true })).toHaveCount(0);
  assert.equal(fixture.mutations.filter(m=>m.method==='ImportGitHubRepository').length,0,'Connecting and discovery never clone');
  assert.equal(fixture.calls.filter(call => call.method === 'ListMailbox').length, 0);
  await page.route('**/ListMailbox', route => route.fulfill({ json: { items: [{
    id: '1', repository: 'fixture-team/api', title: 'Review API rollout', reason: 'review_requested',
    url: 'https://github.com/fixture-team/api/pull/5', workspaceIds: ['alpha'],
  }] } }));
  await page.route('**/ListReleaseWatch', route => route.fulfill({ json: { items: [{
    id: 'release-1', repository: 'fixture-team/api', environment: 'Production', reason: 'deployment_approval',
    url: 'https://github.com/fixture-team/api/actions/runs/5', workspaceIds: ['alpha'],
  }] } }));
  await page.getByLabel('Show GitHub Mailbox in Inbox on this device').check();
  await page.getByLabel('Show GitHub deployments and approval requests in Inbox on this device').check();
  await page.getByRole('navigation', { name: 'Pages' }).getByRole('button', { name: 'Work' }).click();
  await page.getByRole('button', { name: /^Inbox.*2 GitHub items/ }).click();
  const mailbox = page.getByRole('region', { name: 'GitHub Mailbox' });
  await expect(mailbox.getByRole('link', { name: /Review API rollout/ })).toHaveAttribute('href', 'https://github.com/fixture-team/api/pull/5');
  await expect(mailbox.getByRole('button', { name: 'Alpha feature' })).toBeVisible();
  const release = page.getByRole('region', { name: 'Release Watch' });
  await expect(release.getByRole('link', { name: /Production.*Deployment needs approval/ })).toHaveAttribute('href', 'https://github.com/fixture-team/api/actions/runs/5');
  await page.reload(); await settings();
  await expect(page.getByText('Connected as fixture-user', { exact: true })).toBeVisible();
  await expect(page.getByRole('button',{name:'Remove fixture-team/api from suggestions',exact:true})).toBeVisible();
  assert.equal(await page.evaluate(() => JSON.stringify({ local: { ...localStorage }, session: { ...sessionStorage } }).includes('ghp_')), false, 'Token must not enter browser storage');
  fixture.github.state = 'needs_attention'; fixture.github.error = 'Token expired or revoked';
  await page.reload(); await settings();
  await expect(token).toHaveValue('');
  await token.fill('ghp_offline_fixture');
  await page.getByRole('button', { name: 'Connect with token', exact: true }).click();
  await expect(page.getByText('Connected as fixture-user', { exact: true })).toBeVisible();
  await page.getByRole('button', { name: 'Disconnect', exact: true }).click();
  await page.getByRole('button', { name: 'Disconnect this Runner', exact: true }).click();
  await expect(page.getByRole('heading', { name: 'Connect with a personal token' })).toBeVisible();
  assert.equal(fixture.library.length, 2, 'Connection changes preserve repository sources');
  assert.equal(fixture.calls.filter(call => ['ListGitHubInstallations', 'BeginGitHubSignIn', 'ConfigureGitHubApp', 'CloseSession'].includes(call.method)).length, 0);
  assert.ok(fixture.calls.filter(call => call.method === 'ListGitHubRepositories').every(call => !call.body.installationId || call.body.installationId === '0'));
  assert.deepEqual(errors, []);
  console.log('PASS: personal token without App configuration → shared discovery, reload, rejected/expired token recovery, secret clearing, no installation RPC and non-destructive disconnect. Fake RPC only; no real credential used.');
} finally { await browser?.close(); await server.close(); await fixture.close(); }
