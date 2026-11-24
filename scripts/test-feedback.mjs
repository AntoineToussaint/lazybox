// Help → Send feedback in the desktop's main window (native shell replaced:
// scripts/fixtures/updater-window). The person writes what happened and
// chooses what to attach; the preview is exactly what every destination
// gets, with credentials removed, the home folder as ~, and the conversation
// only when ticked (never its messages). Nothing is uploaded: Save to file,
// Copy, or a pre-filled GitHub issue the person posts. Screenshots:
// .cache/feedback-*.png, dark and light.
import { createServer } from 'vite';
import react from '@vitejs/plugin-react';
import { chromium, expect } from './fixtures/playwright.mjs';
import { fileURLToPath } from 'node:url';
import { mkdir } from 'node:fs/promises';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';
import { testViteCache } from './fixtures/vite-cache.mjs';
import { collectPageErrors } from './fixtures/page-errors.mjs';
// Fake credentials for the redaction tests, assembled so no source line looks like a real one (the secret scan in scripts/check-private.mjs).
const FAKE_GITHUB_TOKEN = ['ghp', '0123456789abcdefghijABCDEFGHIJ012345'].join('_');

const fixture = await startRunnerFixture();
fixture.versionInfo.version = '0.2.0';
const saved = [];
const body = req => new Promise(resolve => { let data = ''; req.on('data', c => { data += c; }); req.on('end', () => resolve(data)); });
const fakeNative = { name: 'fake-native', configureServer(server) {
  // No update: this test is about feedback.
  server.middlewares.use('/fixture-feed/latest.json', (_req, res) => { res.setHeader('Content-Type', 'application/json'); res.end(JSON.stringify({ version: '0.2.0', notes: '', pub_date: '2026-12-01T10:00:00Z', platforms: {} })); });
  server.middlewares.use('/fixture-feedback', async (req, res) => { saved.push(JSON.parse(await body(req))); res.end('ok'); });
} };
const server = await createServer({ ...testViteCache(import.meta.url), configFile: false, root: fileURLToPath(new URL('./fixtures/updater-window', import.meta.url)), plugins: [react(), fakeNative], server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: fixture.url } } } });
let browser;
try {
  await server.listen(); browser = await chromium.launch({ headless: true });
  await mkdir('.cache', { recursive: true });
  const origin = `http://127.0.0.1:${server.httpServer.address().port}`;
  const session = fixture.sessions[0];
  session.title = 'Fix the login bug in /Users/tester/code/app';
  for (const scheme of ['dark', 'light']) {
    const context = await browser.newContext({ viewport: { width: 1440, height: 900 }, colorScheme: scheme, permissions: ['clipboard-read', 'clipboard-write'] });
    const page = await context.newPage(); page.setDefaultTimeout(10000);
    const errors = []; await collectPageErrors(page, errors);
    saved.length = 0;
    await page.goto(`${origin}/?version=0.2.0&token=${fixture.token}&collection=${session.collectionId}&session=${session.id}`);
    await expect(page.locator('.workspace-header')).toBeVisible();

    await page.locator('summary[aria-label="Help"]').click();
    await page.getByRole('button', { name: 'Send feedback…', exact: true }).click();
    const dialog = page.getByRole('dialog', { name: 'Send feedback' });
    await expect(dialog).toBeVisible();
    const what = dialog.getByRole('textbox', { name: 'What happened?' });
    await expect(what).toBeFocused();
    const preview = dialog.getByLabel('Report preview');
    await page.route('**/ReviewWorkflow', async route => {
      assert.equal(route.request().postDataJSON().collectionId, session.collectionId);
      await route.fulfill({ json: { proposalTitles: ['New snippet · Verify release'] } });
    });
    await dialog.getByText('Review recent work', { exact: true }).click();
    await dialog.getByRole('button', { name: 'Run workflow review' }).click();
    await expect(dialog.getByRole('status')).toContainText('Verify release');
    await expect(preview).not.toContainText('Verify release');
    await dialog.getByText('Review recent work', { exact: true }).click();

    // What is attached by default: a screenshot (taken before the dialog opened) and diagnostics; never the conversation.
    await expect(dialog.getByRole('img', { name: 'The screenshot to attach' })).toBeVisible();
    await expect(dialog.getByRole('checkbox', { name: /A screenshot of this window/ })).toBeChecked();
    await expect(dialog.getByRole('checkbox', { name: /Diagnostics/ })).toBeChecked();
    await expect(dialog.getByRole('checkbox', { name: /The open conversation/ })).not.toBeChecked();
    await expect(preview).toContainText('## Versions');
    await expect(preview).toContainText('- Lazybox 0.2.0 (release)');
    await expect(preview).toContainText('- Runner 0.2.0');
    await expect(preview).toContainText('- macOS 15.1 (aarch64)');
    await expect(preview).toContainText('Runner log: last 5 lines of ~/Library/Logs/Lazybox/runner.log');
    await expect(preview).toContainText('state ~/Library/Application Support/Lazybox/state');
    await expect(preview).toContainText('input_tokens=1200');
    const shown = await preview.textContent();
    for (const secret of ['/Users/tester', 'sk-ant-api03', 'SECRETSECRET', 'ghp_0123', 'tester@example.com']) assert.ok(!shown.includes(secret), `the preview shows ${secret}`);
    assert.ok(!shown.includes('Fix the login bug'), 'the conversation is not named unless ticked');
    await expect(preview).toContainText('Screenshot: `screenshot.png`');

    // Nothing goes anywhere before the person says what happened.
    await dialog.getByRole('button', { name: 'Open a GitHub issue…' }).click();
    await expect(dialog.getByRole('alert')).toContainText('Write what happened first.');
    await expect(what).toBeFocused();
    assert.equal(saved.length, 0);
    assert.deepEqual(await page.evaluate(() => window.fixtureOpened), []);

    await what.fill(`The diff went blank after ⌘J. Token in my clipboard was ${FAKE_GITHUB_TOKEN}.`);
    await expect(preview).toContainText('The diff went blank after ⌘J. Token in my clipboard was [redacted].');
    // The conversation, only when ticked: its title (home as ~), agent and state, never its messages.
    await dialog.getByRole('checkbox', { name: /The open conversation/ }).check();
    await expect(preview).toContainText('## Conversation');
    await expect(preview).toContainText('- Title: Fix the login bug in ~/code/app');
    await expect(preview).toContainText('Its messages are not included.');
    await page.screenshot({ path: `.cache/feedback-${scheme}.png` });

    // Diagnostics off: no OS, no log.
    await dialog.getByRole('checkbox', { name: /Diagnostics/ }).uncheck();
    await expect(preview).not.toContainText('## Diagnostics');
    await expect(preview).not.toContainText('runner.log');
    await dialog.getByRole('checkbox', { name: /Diagnostics/ }).check();
    const report = await preview.textContent();

    // Copy: exactly the preview.
    await dialog.getByRole('button', { name: 'Copy' }).click();
    await expect(dialog.getByRole('status')).toContainText('Copied the report');
    assert.equal(await page.evaluate(() => navigator.clipboard.readText()), report);

    // Open a GitHub issue: the report is saved first (with its screenshot), then a pre-filled issue opens. Nothing is posted.
    await dialog.getByRole('button', { name: 'Open a GitHub issue…' }).click();
    await expect(dialog.getByRole('status')).toContainText('Opened a new GitHub issue in your browser');
    assert.equal(saved.length, 1);
    assert.equal(saved[0].markdown, report, 'the saved report is the preview');
    assert.ok(saved[0].screenshot > 0, 'the screenshot is saved beside it');
    const opened = await page.evaluate(() => window.fixtureOpened);
    assert.equal(opened.length, 1);
    const issue = new URL(opened[0]);
    assert.equal(issue.origin + issue.pathname, 'https://github.com/AntoineToussaint/lazybox/issues/new');
    assert.equal(issue.searchParams.get('title'), 'Feedback: The diff went blank after ⌘J. Token in my clipboard was [redacted].');
    assert.ok(issue.searchParams.get('body').startsWith(report));
    assert.match(issue.searchParams.get('body'), /saved at `~\/Downloads\/Lazybox feedback 2026-12-02 09\.00\.00`/);
    await dialog.getByRole('button', { name: 'Show in Finder' }).click();
    assert.deepEqual(await page.evaluate(() => window.fixtureRevealed), ['/Users/tester/Downloads/Lazybox feedback 2026-12-02 09.00.00']);
    // Saving again without a change reuses the same folder.
    await dialog.getByRole('button', { name: 'Save to file' }).click();
    assert.equal(saved.length, 1);
    if (scheme === 'dark') await page.screenshot({ path: '.cache/feedback-sent-dark.png' });

    await page.keyboard.press('Escape');
    await expect(dialog).toHaveCount(0);

    // Narrow: the preview goes below the form.
    if (scheme === 'dark') {
      await page.setViewportSize({ width: 720, height: 860 });
      await page.locator('summary[aria-label="Help"]').isVisible() ? await page.locator('summary[aria-label="Help"]').click() : null;
      const help = page.getByRole('button', { name: 'Send feedback…', exact: true });
      if (await help.isVisible()) { await help.click(); await expect(dialog).toBeVisible(); await page.screenshot({ path: '.cache/feedback-narrow-dark.png' }); await page.keyboard.press('Escape'); }
    }
    assert.deepEqual(errors, []);
    await context.close();
  }
  console.log('PASS Feedback: what is attached is chosen and previewed exactly, credentials and home redacted, conversation only when ticked, saved/copied/pre-filled issue with no upload.');
} finally { await browser?.close(); await server.close(); await fixture.close(); }
