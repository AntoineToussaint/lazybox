// Settings → About & updates and the install flow against a fake update feed, in the
// desktop's main window (the native shell replaced: scripts/fixtures/updater-window).
// An update is offered and never forced: nothing downloads until Install, a
// skipped version stays quiet, Remind me later is a day of quiet, automatic
// checks can be turned off, offline and bad signatures are explained, the
// install warns that running agents keep running, and the launch after it
// says that it took. App and Runner versions that differ are explained, with
// the fix. Screenshots: .cache/updates-*.png, dark and light.
import { createServer } from './fixtures/vite-cache.mjs';
import react from '@vitejs/plugin-react';
import { chromium, expect } from './fixtures/playwright.mjs';
import { fileURLToPath } from 'node:url';
import { mkdir } from 'node:fs/promises';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';
import { testViteCache } from './fixtures/vite-cache.mjs';
import { collectPageErrors, drainPageErrors } from './fixtures/page-errors.mjs';

const fixture = await startRunnerFixture();
fixture.versionInfo.version = '0.1.0';
const releases = [
  { id: '0.2.0', date: '2026-12-01', title: 'Updates you choose', items: [{ title: 'Settings → About & updates', body: 'Check now, skip a version, or be reminded tomorrow. Nothing installs until you say so.' }, { title: 'Send feedback', body: 'Help → Send feedback writes a report you read before it goes anywhere.' }] },
  { id: '2026-11-25.1', date: '2026-11-25', title: 'Quieter notices', items: [{ title: 'One line in the header', body: 'A new version is one quiet line, never a dialog.' }] },
  { id: '0.1.0', date: '2026-11-17', title: 'First release', items: [{ title: 'Already installed', body: 'This must not be shown as new.' }] },
];
const feed = (signature = 'valid', version = '0.2.0') => ({ version, notes: 'Updates you choose', pub_date: '2026-12-01T10:00:00Z', platforms: { 'darwin-aarch64': { signature, url: 'https://example.invalid/Lazybox.app.tar.gz' } }, lazybox: { releases } });
const state = { feed: feed(), requests: 0, installed: [], moved: [] };
const body = req => new Promise(resolve => { let data = ''; req.on('data', c => { data += c; }); req.on('end', () => resolve(data)); });
const fakeNative = { name: 'fake-native', configureServer(server) {
  server.middlewares.use('/fixture-feed/latest.json', (_req, res) => {
    state.requests++;
    if (state.feed === 'offline') { res.statusCode = 599; res.end(); return; }
    if (state.feed === 'missing') { res.statusCode = 404; res.end('Not Found'); return; }
    res.setHeader('Content-Type', 'application/json'); res.end(JSON.stringify(state.feed));
  });
  // What `lazybox service install` does after the bundle is replaced: the Runner moves to the new version.
  server.middlewares.use('/fixture-feed/installed', async (req, res) => { const v = await body(req); state.installed.push(v); fixture.versionInfo.version = v; res.end('ok'); });
  server.middlewares.use('/fixture-feed/move-runner', async (req, res) => { const v = await body(req); state.moved.push(v); fixture.versionInfo.version = v; res.end('ok'); });
} };
const server = await createServer({ ...testViteCache(import.meta.url), configFile: false, root: fileURLToPath(new URL('./fixtures/updater-window', import.meta.url)), plugins: [react(), fakeNative], server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: fixture.url } } } });
let browser;
const PREF = name => `fixture:${name}`;
try {
  await server.listen(); browser = await chromium.launch({ headless: true });
  await mkdir('.cache', { recursive: true });
  const origin = `http://127.0.0.1:${server.httpServer.address().port}`;
  const url = (version = '0.1.0') => `${origin}/?version=${version}&token=${fixture.token}`;
  const notice = page => page.locator('.workspace-header .update-notice');
  const openUpdates = async page => {
    await page.getByRole('button', { name: 'Settings', exact: true }).click();
    await page.getByRole('tab', { name: 'About & updates' }).click();
    return page.getByRole('tabpanel', { name: 'About & updates' });
  };

  for (const scheme of ['dark', 'light']) {
    const context = await browser.newContext({ viewport: { width: 1440, height: 900 }, colorScheme: scheme });
    const page = await context.newPage(); page.setDefaultTimeout(10000);
    const errors = []; await collectPageErrors(page, errors);
    state.feed = feed(); state.installed = []; fixture.versionInfo.version = '0.1.0';

    // (a) Offered, never forced: the automatic check finds 0.2.0 and the header says so; nothing downloads.
    await page.goto(url());
    await expect(notice(page)).toContainText('Lazybox 0.2.0');
    assert.deepEqual(state.installed, [], 'an offered update must not install without a click');
    const panel = await openUpdates(page);
    const status = panel.locator('.update-status');
    await expect(status).toContainText('Lazybox 0.2.0 is available');
    await expect(status).toContainText('You have 0.1.0');
    await expect(panel.getByRole('switch', { name: 'Check automatically' })).toBeChecked();
    await expect(panel).toContainText('Stable');
    await expect(panel.getByRole('button', { name: 'Check now' })).toBeEnabled();
    await page.screenshot({ path: `.cache/updates-available-${scheme}.png` });

    await panel.getByRole('button', { name: 'Add to terminal' }).click();
    await expect(panel.getByText('/usr/local/bin/lazybox', { exact: true })).toBeVisible();
    await page.setViewportSize({ width: 390, height: 844 });
    assert.equal(await panel.evaluate(el => el.scrollWidth <= el.clientWidth), true, 'the combined page fits a phone');
    await page.setViewportSize({ width: 1440, height: 900 });

    // (b) What's new before installing: everything since 0.1.0, not 0.1.0 itself, and the running agents.
    await panel.getByRole('button', { name: "What's new and install…" }).click();
    const dialog = page.getByRole('dialog', { name: 'Lazybox 0.2.0' });
    await expect(dialog).toBeVisible();
    const notes = dialog.getByRole('region', { name: "What's new in 0.2.0" });
    await expect(notes).toContainText('Updates you choose');
    await expect(notes).toContainText('Quieter notices');
    await expect(notes).not.toContainText('This must not be shown as new');
    const live = fixture.sessions.filter(s => !s.closed && !s.historical).length;
    assert.ok(live > 0, 'the fixture has running agents');
    await expect(dialog.getByRole('note')).toContainText(new RegExp(`${live} agents? (is|are) running`));
    await expect(dialog.getByRole('note')).toContainText(/keeps? running/);
    await expect(dialog.getByRole('note')).toContainText('an approval that is waiting stays waiting');
    await expect(dialog).toContainText('checked against the signing key');
    await expect(dialog.getByRole('button', { name: 'Install and restart' })).toBeFocused();
    await page.screenshot({ path: `.cache/updates-dialog-${scheme}.png` });

    // (c) Remind me later: the dialog closes, the header goes quiet, and Settings says until when.
    await dialog.getByRole('button', { name: 'Remind me later' }).click();
    await expect(dialog).toHaveCount(0);
    await expect(notice(page)).toHaveCount(0);
    await expect(status).toContainText('You asked to be reminded');
    const remind = await page.evaluate(k => localStorage.getItem(k), PREF('updates.remind-later'));
    assert.match(remind, /^0\.2\.0 \d+$/);
    assert.ok(Number(remind.split(' ')[1]) - Date.now() > 23 * 3600_000, 'reminded about a day later');

    // (d) Skip this version: never offered again until a newer one; Stop skipping undoes it.
    await panel.getByRole('button', { name: "What's new and install…" }).click();
    await page.getByRole('dialog', { name: 'Lazybox 0.2.0' }).getByRole('button', { name: 'Skip this version' }).click();
    await expect(status).toContainText('You skipped this version');
    await expect(notice(page)).toHaveCount(0);
    await page.reload(); await expect(page.locator('.workspace-header')).toBeVisible();
    await expect(notice(page)).toHaveCount(0);
    const again = await openUpdates(page);
    await expect(again.locator('.update-status')).toContainText('You skipped this version');
    await again.getByRole('button', { name: 'Stop skipping' }).click();
    await page.evaluate(k => localStorage.removeItem(k), PREF('updates.remind-later'));
    // A newer version than the skipped one is offered.
    state.feed = feed('valid', '0.2.1'); await again.getByRole('button', { name: 'Check now' }).click();
    await expect(again.locator('.update-status')).toContainText('Lazybox 0.2.1 is available');
    state.feed = feed();
    await again.getByRole('button', { name: 'Check now' }).click();
    await expect(again.locator('.update-status')).toContainText('Lazybox 0.2.0 is available');

    // (e) Offline and an unpublished feed: said plainly, with a way to retry.
    state.feed = 'offline';
    await again.getByRole('button', { name: 'Check now' }).click();
    // What was already found stays offered; the failed check is said beside it.
    await expect(again.locator('.update-status')).toContainText('Lazybox 0.2.0 is available');
    await expect(again.locator('.update-status')).toContainText('could not reach GitHub');
    if (scheme === 'dark') await page.screenshot({ path: '.cache/updates-offline-dark.png' });
    state.feed = 'missing';
    await again.getByRole('button', { name: 'Check now' }).click();
    await expect(again.locator('.update-status')).toContainText('No update information is published');
    state.feed = feed();
    await again.getByRole('button', { name: 'Check now' }).click();
    await expect(again.locator('.update-status')).toContainText('Lazybox 0.2.0 is available');

    // (f) A download not signed with the app's key is refused, and nothing changes.
    state.feed = feed('tampered');
    await again.getByRole('button', { name: 'Check now' }).click();
    await again.getByRole('button', { name: "What's new and install…" }).click();
    const install = page.getByRole('dialog', { name: 'Lazybox 0.2.0' });
    await install.getByRole('button', { name: 'Install and restart' }).click();
    await expect(install.getByRole('status')).toContainText(/Downloading|Checking the signature/);
    await expect(install.getByRole('alert')).toContainText('not signed with Lazybox\'s key, so it was not installed');
    assert.deepEqual(state.installed, []);
    await expect(install.getByRole('button', { name: 'Try again' })).toBeVisible();
    if (scheme === 'dark') await page.screenshot({ path: '.cache/updates-signature-dark.png' });
    await install.getByRole('button', { name: 'Close' }).click();

    // (g) A signed update installs only on the click, then the app comes back on it and says so.
    state.feed = feed();
    await again.getByRole('button', { name: 'Check now' }).click();
    await again.getByRole('button', { name: "What's new and install…" }).click();
    await page.getByRole('dialog', { name: 'Lazybox 0.2.0' }).getByRole('button', { name: 'Install and restart' }).click();
    await expect(page.getByRole('dialog', { name: 'Lazybox 0.2.0' }).getByRole('status')).toContainText(/Downloading… \d+%/);
    if (scheme === 'dark') await page.screenshot({ path: '.cache/updates-installing-dark.png' });
    await page.waitForURL(/version=0\.2\.0/);
    assert.deepEqual(state.installed, ['0.2.0']);
    const updated = page.getByRole('complementary', { name: "What's new" });
    await expect(updated).toContainText('Updated to Lazybox 0.2.0');
    await expect(updated).not.toBeFocused();
    assert.equal(await page.evaluate(k => localStorage.getItem(k), PREF('updates.installing')), '', 'read once');
    if (scheme === 'dark') await page.screenshot({ path: '.cache/updates-updated-dark.png' });
    await updated.getByRole('button', { name: 'Got it' }).click();
    const after = await openUpdates(page);
    await expect(after.locator('.update-status')).toContainText('Lazybox 0.2.0 is up to date');
    await expect(after).not.toContainText('Move the Runner now');

    // (h) App and Runner disagree: explained, with the fix (agents keep running).
    fixture.versionInfo.version = '0.1.0';
    await page.reload(); const mismatch = await openUpdates(page);
    await expect(mismatch.getByRole('alert')).toContainText('The Runner is still 0.1.0 and the app is 0.2.0');
    if (scheme === 'dark') await page.screenshot({ path: '.cache/updates-mismatch-dark.png' });
    await mismatch.getByRole('button', { name: 'Move the Runner now' }).click();
    await expect(mismatch).toContainText('The Runner restarted on this release. Agents kept running.');
    assert.deepEqual(state.moved.slice(-1), ['0.2.0']);

    // (i) An install that did not take is said once, on the next launch.
    await page.evaluate(k => localStorage.setItem(k, '0.3.0'), PREF('updates.installing'));
    await page.reload();
    const failed = page.getByRole('complementary', { name: 'Update not installed' });
    await expect(failed).toContainText('The update to 0.3.0 did not finish. Lazybox is still 0.2.0');
    await failed.getByRole('button', { name: 'Not now' }).click();
    await page.reload(); await expect(page.locator('.workspace-header')).toBeVisible();
    await expect(page.getByRole('complementary', { name: 'Update not installed' })).toHaveCount(0);

    // (j) Check automatically off: a new launch asks nobody.
    await page.goto(url('0.1.0'));
    const prefs = await openUpdates(page);
    await prefs.getByRole('switch', { name: 'Check automatically' }).uncheck();
    await expect(prefs).toContainText('Off: Lazybox looks for a new version only when you choose Check now.');
    await page.evaluate(k => localStorage.removeItem(k), PREF('updates.last-checked'));
    const before = state.requests;
    await page.reload(); await expect(page.locator('.workspace-header')).toBeVisible();
    await page.waitForTimeout(1200);
    assert.equal(state.requests, before, 'no automatic check while it is off');
    await expect(notice(page)).toHaveCount(0);
    await page.evaluate(k => localStorage.removeItem(k), PREF('updates.check-automatically'));

    // Help → Check for updates… opens the page and checks.
    await page.locator('summary[aria-label="Help"]').click();
    await page.getByRole('button', { name: 'Check for updates…', exact: true }).click();
    await expect(page.getByRole('tabpanel', { name: 'About & updates' }).locator('.update-status')).toContainText('Lazybox 0.2.0 is available');
    assert.equal(state.requests, before + 1);

    await page.goto(`${url()}&fixedWorkspace=fixture`);
    await page.locator('summary[aria-label="Help"]').click();
    const beforeWorkspaceCheck = state.requests;
    await page.getByRole('button', { name: 'Check for updates…', exact: true }).click();
    await expect(page.getByRole('tabpanel', { name: 'About & updates' }).locator('.update-status')).toContainText('Lazybox 0.2.0 is available');
    assert.equal(state.requests, beforeWorkspaceCheck + 1);

    // (k) A build from source (make install): no updater, so nothing is
    // checked or offered, even with a newer version on the feed.
    state.feed = feed();
    const quiet = state.requests;
    await page.goto(`${url('0.1.0')}&source=1`);
    await expect(page.locator('.workspace-header')).toBeVisible();
    await page.waitForTimeout(1200);
    await expect(notice(page)).toHaveCount(0);
    const sourcePanel = await openUpdates(page);
    await expect(sourcePanel).toContainText('Built from source; update with make update in /Users/tester/lazybox.');
    await expect(sourcePanel.locator('code', { hasText: 'make update' })).toBeVisible();
    await expect(sourcePanel).toContainText('0123456789ab');
    const buildTime = sourcePanel.locator('time[datetime]');
    await expect(buildTime).toHaveAttribute('datetime', '2026-11-17T10:00:00.000Z');
    await expect(buildTime).toHaveText(await page.evaluate(() =>
      new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'short' }).format(new Date('2026-11-17T10:00:00Z'))));
    await expect(sourcePanel.getByRole('button', { name: 'Check now' })).toHaveCount(0);
    await expect(sourcePanel.getByRole('switch', { name: 'Check automatically' })).toHaveCount(0);
    if (scheme === 'dark') await page.screenshot({ path: '.cache/updates-source-dark.png' });
    await expect(page.getByRole('tab', { name: 'Updates', exact: true })).toHaveCount(0);
    await expect(page.getByRole('tab', { name: 'About', exact: true })).toHaveCount(0);
    await expect(sourcePanel.getByRole('button', { name: "What's new" })).toBeVisible();
    // Help → Check for updates… only shows how it updates.
    await page.locator('summary[aria-label="Help"]').click();
    await page.getByRole('button', { name: 'Check for updates…', exact: true }).click();
    await expect(page.getByRole('tabpanel', { name: 'About & updates' })).toContainText('update with make update');
    await expect(notice(page)).toHaveCount(0);
    assert.equal(state.requests, quiet, 'a build from source never asks the update feed');

    await drainPageErrors(errors);assert.deepEqual(errors, []);
    await context.close();
  }
  console.log('PASS Updates: offered never forced, skip and remind, automatic off, offline and signature errors, running-agents warning, install and post-update notice, version mismatch fix, a build from source that never checks.');
} finally { await browser?.close(); await server.close(); await fixture.close(); }
