// Runners on other computers (REMOTE_RUNNERS.md P0): Settings → Runners → Add
// a computer lists the person's ~/.ssh/config hosts in one filtered list,
// shows `lazybox remote add`'s steps as they happen (a tick each, or the
// error in plain words), and the computer's Runner then shows in the sidebar
// under its own computer, with its route, round trip and the link's state
// (Reconnecting…, Access revoked). Opening its workspace gives the full page
// on that Runner, and Remove forgets it. Two fake Runners: "Dev", the
// window's own, and one standing for the link agent's loopback origin of
// build-box. WebKit, the desktop app's engine; dark and light screenshots in
// .cache/remote-computers-*.png. Fixture only: no ssh, no agent runs.
import { testViteCache } from './fixtures/vite-cache.mjs';
import { createServer } from './fixtures/vite-cache.mjs';
import react from '@vitejs/plugin-react';
import { webkit, expect } from './fixtures/playwright.mjs';
import { fileURLToPath } from 'node:url';
import { mkdir } from 'node:fs/promises';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';
import { collectPageErrors } from './fixtures/page-errors.mjs';

const dev = await startRunnerFixture(), box = await startRunnerFixture();
dev.versionInfo.version = '0.1.22-dev';
box.versionInfo.version = '0.1.22';
Object.assign(box.collections[0], { name: 'GPU training' });
Object.assign(box.collections[1], { name: 'Nightly evals' });

// What the page's server (the desktop host, in the app) says: the Runners, the ssh hosts, and `remote add`'s steps.
const state = { added: false, remote: { host: 'build-box', route: 'ssh', machine: 'build-box', os: 'Linux', fingerprint: 'SHA256:q1w2e3r4t5y6u7i8o9p0', state: 'connected', rttMs: 42, code: '', problem: '' }, adds: [], removes: [] };
const runners = () => [
  { id: 'dev', name: 'dev', channel: 'dev', product: 'Lazybox Dev', source: 'registered', locator: '/Users/me/.config/lazybox/runners/dev.json', current: true, state: 'running' },
  // Another local Runner, running: opt-in, so this window leaves it out (and never calls it) while the computer the person added shows.
  { id: 'working', name: 'working', channel: 'working', product: 'Lazybox', source: 'registered', locator: '/Users/me/.config/lazybox/runners/working.json', current: false, state: 'running', url: '/__lazybox/runner/working-never-called', token: 'not-a-token' },
  ...(state.added ? [{ id: 'buildbox', name: 'build-box', product: 'Lazybox', source: 'registered', locator: '/Users/me/.config/lazybox/runners/build-box.json', current: false, state: 'running', url: '/__lazybox/runner/box', token: box.token, remote: { ...state.remote } }] : []),
];
const hosts = [
  { name: 'build-box', hostName: '10.0.0.2', user: 'me' },
  { name: 'mini', hostName: 'mac-mini.local' },
  { name: 'gpu-vm', hostName: 'gpu.example.net', user: 'ubuntu' },
];
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
const json = (res, value) => { res.setHeader('Content-Type', 'application/json'); res.end(JSON.stringify(value)); };
const body = req => new Promise(resolve => { let text = ''; req.on('data', chunk => { text += chunk; }); req.on('end', () => resolve(text ? JSON.parse(text) : {})); });
const step = (res, value) => res.write(JSON.stringify(value) + '\n');
const fakeHost = { name: 'fake-host', configureServer(server) {
  server.middlewares.use('/__lazybox/runners', (_req, res) => json(res, { runners: runners() }));
  server.middlewares.use('/__lazybox/remote/hosts', (_req, res) => json(res, { hosts: hosts.map(h => ({ ...h, added: state.added && h.name === 'build-box' ? 'build-box' : undefined })) }));
  server.middlewares.use('/__lazybox/remote/add', async (req, res) => {
    const { host } = await body(req);
    state.adds.push(host);
    res.setHeader('Content-Type', 'application/x-ndjson');
    step(res, { step: 'connect', title: `Connect to ${host}`, state: 'running' });
    await sleep(150);
    if (host === 'offline-box') {
      step(res, { step: 'connect', title: `Connect to ${host}`, state: 'failed', detail: `${host} refused the SSH connection: is SSH (Remote Login) turned on there, and is the port right?` });
      step(res, { error: `${host} refused the SSH connection: is SSH (Remote Login) turned on there, and is the port right?` });
      res.end(); return;
    }
    step(res, { step: 'connect', title: `Connect to ${host}`, state: 'done' });
    step(res, { step: 'probe', title: 'Check the host', state: 'done', detail: 'Linux arm64 · systemd · 38 GB free · no Lazybox yet · Claude Code found · Codex missing' });
    step(res, { step: 'install', title: 'Upload 0.1.22 and verify', state: 'running', detail: 'SHA-256 3f9a0c1d2e4b…' });
    await sleep(400);
    step(res, { step: 'install', title: 'Upload 0.1.22 and verify', state: 'done', detail: `SHA-256 3f9a0c1d2e4b… checked on ${host}` });
    step(res, { step: 'start', title: 'Start the Runner', state: 'done', detail: '0.1.22 started for this login (not at boot yet)' });
    step(res, { step: 'enrol', title: 'Add MacBook Pro as owner', state: 'done', detail: 'pinned SHA256:q1w2e3r4t5y6u7i8o9p0' });
    step(res, { step: 'connected', title: 'Connect over the link', state: 'done', detail: 'Runner 0.1.22 answered in 212ms' });
    state.added = true;
    step(res, { done: true, remote: { name: host } });
    res.end();
  });
  server.middlewares.use('/__lazybox/remote/remove', async (req, res) => { const { name } = await body(req); state.removes.push(name); state.added = false; json(res, { removed: name, revoked: true }); });
} };
const proxy = {
  '/lazybox.bench.v1.RunnerService/': { target: dev.url },
  '/__lazybox/runner/box/': { target: box.url, rewrite: path => path.replace(/^\/__lazybox\/runner\/box/, '') },
};
const server = await createServer({ ...testViteCache(import.meta.url), configFile: false, logLevel: 'error', root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react(), fakeHost], server: { host: '127.0.0.1', port: 0, proxy } });
const calls = (fixture, method) => fixture.calls.filter(call => call.method === method);
let browser;
try {
  await server.listen(); await mkdir('.cache', { recursive: true });
  browser = await webkit.launch();
  const base = `http://127.0.0.1:${server.httpServer.address().port}/?view=workspace`;
  for (const scheme of ['dark', 'light']) {
    state.added = scheme === 'light';
    Object.assign(state.remote, { state: 'connected', rttMs: 42, code: '', problem: '' });
    const context = await browser.newContext({ viewport: { width: 1440, height: 900 }, colorScheme: scheme });
    const page = await context.newPage(); page.setDefaultTimeout(15000);
    const errors = await collectPageErrors(page);
    // A headless page never has the window focus; the window under test does.
    await page.addInitScript(() => { document.hasFocus = () => true; });
    await page.goto(`${base}&collection=alpha&session=alpha-session#token=${dev.token}`);
    const settings = async () => {
      await page.getByRole('navigation', { name: 'Pages' }).getByRole('button', { name: 'Settings' }).click();
      await page.getByRole('tab', { name: 'Runners', exact: true }).click();
    };
    // Working, on this Mac, is pointed out once and left out; dismissed here.
    const hint = page.locator('.toast').filter({ hasText: 'Lazybox Working is also running on this Mac' });
    await hint.getByRole('button', { name: 'Dismiss' }).click();
    await expect(hint).toHaveCount(0);

    if (scheme === 'dark') {
      // One Runner, no computer yet: no machine headers.
      await expect(page.locator('.machine-header')).toHaveCount(0);
      await settings();
      await page.getByRole('button', { name: 'Add a computer…' }).click();
      const sheet = page.getByRole('dialog', { name: 'Add a computer' });
      await expect(sheet).toBeVisible();
      const filter = sheet.getByRole('combobox', { name: /Filter hosts/ });
      await expect.poll(() => page.evaluate(() => !!document.activeElement?.classList.contains('add-computer-filter'))).toBe(true);
      const options = sheet.getByRole('listbox', { name: 'SSH hosts' }).getByRole('option');
      await expect(options).toHaveCount(3);
      await expect(options.first()).toContainText('me@10.0.0.2');
      await sheet.screenshot({ path: `.cache/remote-computers-hosts-${scheme}.png` });

      // A host that does not answer: the step fails in plain words; Back returns to the list.
      await filter.fill('offline-box');
      await expect(options).toHaveCount(1);
      await expect(options.first()).toContainText('Add offline-box');
      await page.keyboard.press('Enter');
      await expect(sheet.getByRole('alert')).toContainText('offline-box refused the SSH connection: is SSH (Remote Login) turned on there');
      await expect(sheet.locator('.add-computer-steps > li[data-state=failed]')).toHaveCount(1);
      await sheet.getByRole('button', { name: 'Back' }).click();

      // Filter, arrow keys and Return choose a host; each step shows as it happens.
      await sheet.getByRole('combobox', { name: /Filter hosts/ }).fill('');
      await page.keyboard.type('g');
      await expect(options.first()).toContainText('gpu-vm');
      await sheet.getByRole('combobox', { name: /Filter hosts/ }).fill('');
      await page.keyboard.press('ArrowDown'); await page.keyboard.press('ArrowUp');
      await expect(options.first()).toHaveAttribute('aria-selected', 'true');
      await page.keyboard.press('Enter');
      await expect(sheet.getByRole('heading', { name: 'Add build-box' })).toBeVisible();
      await expect(sheet.locator('.add-computer-steps > li[data-state=running]')).toContainText('Upload 0.1.22 and verify');
      await expect(sheet.locator('.add-computer-steps > li[data-state=done]')).toHaveCount(6);
      await expect(sheet.locator('.add-computer-steps')).toContainText('Linux arm64 · systemd · 38 GB free');
      await expect(sheet.getByRole('status')).toContainText('build-box is added');
      assert.deepEqual(state.adds, ['offline-box', 'build-box']);
      await sheet.screenshot({ path: `.cache/remote-computers-steps-${scheme}.png` });
      await sheet.getByRole('button', { name: 'Done' }).click();
      await expect(sheet).toHaveCount(0);

      // Settings lists it: its computer, route, round trip and pinned key; Remove is there.
      const item = page.getByRole('list', { name: 'Runners' }).locator('.runners-item[data-runner=buildbox]');
      await expect(item).toBeVisible({ timeout: 10_000 });
      await expect(item.locator('.runners-remote')).toHaveText('On build-box · SSH build-box · 42 ms · SHA256:q1w2e3r4t5y6u7i8o9p0');
      await expect(item.getByRole('button', { name: 'Remove build-box from this computer' })).toBeVisible();
      await page.getByRole('navigation', { name: 'Pages' }).getByRole('button', { name: 'Work', exact: true }).click();
    }

    // The sidebar groups by computer: This Mac (Dev), then build-box over SSH.
    const machines = page.locator('.machine-group');
    await expect(machines).toHaveCount(2, { timeout: 10_000 });
    const mac = machines.nth(0), boxMachine = machines.nth(1);
    await expect(mac.locator('.machine-name')).toHaveText(/This (Mac|computer)/);
    await expect(boxMachine.locator('.machine-name')).toHaveText('build-box');
    await expect(boxMachine.locator('.machine-route')).toHaveText('SSH');
    await expect(boxMachine.locator('.machine-header')).toHaveAttribute('title', /round trip 42 ms/);
    await expect(boxMachine.locator('.machine-rtt')).toBeHidden();
    const boxGroup = boxMachine.locator('.runner-group');
    await expect(boxGroup).toHaveAttribute('data-health', 'online');
    await expect(boxGroup).toContainText('GPU training');
    await expect(mac.locator('.runner-group')).toContainText('Alpha feature');
    // The computer added explicitly is shown; Working, another local Runner, is not (opt-in).
    await expect(mac.locator('.runner-group')).toHaveCount(1);
    await expect(page.locator('.runner-group[data-runner=working]')).toHaveCount(0);
    await page.mouse.move(0, 0);
    await page.locator('.panel-collections').screenshot({ path: `.cache/remote-computers-sidebar-${scheme}.png` }).catch(() => {});

    if (scheme === 'dark') {
      // A slow link shows its round trip; a dropped one says Reconnecting…; a revoked key says so.
      state.remote.rttMs = 450;
      await expect(boxMachine.locator('.machine-rtt')).toHaveText('450 ms', { timeout: 10_000 });
      await expect(boxMachine.locator('.machine-rtt')).toBeVisible();
      state.remote.state = 'reconnecting';
      await expect(boxMachine.locator('.machine-state')).toHaveText('Reconnecting…', { timeout: 10_000 });
      await expect(boxMachine).toHaveAttribute('data-link', 'down');
      await page.locator('.panel-collections').screenshot({ path: '.cache/remote-computers-reconnecting-dark.png' }).catch(() => {});
      Object.assign(state.remote, { state: 'refused', code: 'unknown-key', problem: 'build-box no longer accepts this computer (its key was revoked there). Add it again to reconnect.' });
      await expect(boxMachine.locator('.machine-state')).toHaveText('Access revoked', { timeout: 10_000 });
      await expect(boxMachine.locator('.machine-header')).toHaveAttribute('title', /no longer accepts this computer/);
      Object.assign(state.remote, { state: 'connected', rttMs: 42, code: '', problem: '' });
      await expect(boxMachine.locator('.machine-state')).toHaveCount(0, { timeout: 10_000 });
      assert.equal(await page.locator('.error[role=alert]').count(), 0, 'a link state is not an error');

      // Opening build-box's workspace shows that Runner's whole page, through its link.
      const before = calls(box, 'GetCollection').length + calls(box, 'ListCollections').length;
      await boxGroup.getByText('GPU training').click();
      await expect(boxGroup).toHaveAttribute('data-active', 'true');
      await expect(page.getByRole('combobox', { name: 'Workspace', exact: true }).locator('option:checked')).toHaveText('GPU training');
      await expect.poll(() => calls(box, 'GetCollection').length + calls(box, 'ListCollections').length).toBeGreaterThan(before);
      await page.mouse.move(0, 0);
      await page.screenshot({ path: '.cache/remote-computers-page-dark.png' });

      // Remove: forgotten here (and revoked there); the sidebar has one computer again.
      await settings();
      await page.getByRole('list', { name: 'Runners' }).locator('.runners-item[data-runner=buildbox]').getByRole('button', { name: 'Remove build-box from this computer' }).click();
      await expect.poll(() => state.removes).toEqual(['build-box']);
      await expect(page.getByRole('list', { name: 'Runners' }).locator('.runners-item[data-runner=buildbox]')).toHaveCount(0, { timeout: 10_000 });
    }
    assert.deepEqual(errors, []);
    await context.close();
  }
  console.log('remote-computers: ok');
} finally {
  await browser?.close(); await server.close(); await dev.close(); await box.close();
}
