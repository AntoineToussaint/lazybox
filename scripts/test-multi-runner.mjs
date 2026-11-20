// Several Runners in one window (MULTI_RUNNER.md step 2, audit brief R8):
// two fake Runners at once — "Dev", the window's own, and "Working", found
// through the page's Runner list — plus a registered Runner that is not
// running. By default the window shows only its own Runner (the brand says
// "Dev") and hints once that Working runs too; the other local Runners are
// shown after an opt-in (the hint, or Settings → Runners), and a hidden one is
// never called. Then: Workspaces groups by Runner (collapsible, name, channel, version,
// health); the Inbox and Agents merge both Runners with a Runner badge; every
// action goes to the Runner the row belongs to and never to the other (pin,
// approve, peek, start a task); a workspace or conversation of the other
// Runner shows its page; ⌘K and ⌘N pick the Runner; a Runner going away shows
// offline, not an error; Settings → Runners hides and makes primary.
// Screenshots: .cache/multi-runner-*.png, dark and light. WebKit, the desktop
// app's engine. Fixture only: no agent runs.
import { testViteCache } from './fixtures/vite-cache.mjs';
import { createServer } from 'vite';
import react from '@vitejs/plugin-react';
import { webkit, expect } from './fixtures/playwright.mjs';
import { fileURLToPath } from 'node:url';
import { mkdir } from 'node:fs/promises';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';

const dev = await startRunnerFixture(), working = await startRunnerFixture();
const now = Date.now(), ms = value => String(value);
const data = value => Buffer.from(JSON.stringify(value)).toString('base64');
const root = (status, updated, extra = {}) => [{ id: 'root', name: 'Main agent', status, updatedAtUnixMillis: ms(updated), ...extra }];
dev.versionInfo.version = '0.1.22-dev';
working.versionInfo.version = '0.1.21';
// Dev: one agent working, one asking a question.
Object.assign(dev.sessions[0], { title: 'Index the API', busy: true, status: 'running', agents: root('running', now - 30_000) });
Object.assign(dev.sessions[1], { title: 'Ship release notes', status: 'waiting_input', agents: root('waiting_input', now - 300_000), pending: [{ type: 'question', requestId: 'q-release', providerDataJson: data({ questions: [{ question: 'Which version should the notes cover?' }] }) }] });
// Working: the same workspace ids as Dev's (two Runners may share them), other names; one approval waiting.
Object.assign(working.collections[0], { name: 'Billing service' });
Object.assign(working.collections[1], { name: 'Docs site' });
const resetWorking = () => {
  Object.assign(working.sessions[0], { title: 'Migrate the invoices table', status: 'waiting_approval', busy: false, agents: root('waiting_approval', now - 60_000), pending: [{ type: 'approval', requestId: 'a-migrate', text: 'Bash', providerDataJson: data({ input: { command: 'npm run migrate' } }) }] });
  Object.assign(working.sessions[1], { title: 'Rewrite the docs navigation', status: 'done', busy: false, pending: [], agents: root('done', now - 900_000, { turnEndedAtUnixMillis: ms(now - 900_000), lastTurnStatus: 'completed' }), lastMessage: 'The navigation has three levels now.' });
  for (const session of working.sessions) delete session.pinnedAtUnixMillis;
};
resetWorking();

// What the page's server says about Runners: its own (Dev), Working behind a path of this origin, and the release, not running.
const listing = { workingState: 'running' };
const runners = () => [
  { id: 'dev', name: 'dev', channel: 'dev', product: 'Lazybox Dev', source: 'registered', locator: '/Users/me/.config/lazybox/runners/dev.json', current: true, state: 'running' },
  { id: 'working', name: 'working', channel: 'working', product: 'Lazybox Working', source: 'registered', locator: '/Users/me/.config/lazybox/runners/working.json', current: false, state: listing.workingState, ...(listing.workingState === 'running' ? { url: '/__lazybox/runner/working', token: working.token } : { problem: 'Start it with make working.' }) },
  { id: 'release', name: 'lazybox', channel: 'release', product: 'Lazybox', source: 'registered', locator: '/Users/me/.config/lazybox/runners/lazybox.json', current: false, state: 'stopped', problem: 'Open Lazybox to start it.' },
];
const runnerList = { name: 'runner-list', configureServer(server) { server.middlewares.use('/__lazybox/runners', (_req, res) => { res.setHeader('Content-Type', 'application/json'); res.end(JSON.stringify({ runners: runners() })); }); } };
const proxy = {
  '/lazybox.bench.v1.RunnerService/': { target: dev.url },
  '/__lazybox/runner/working/': { target: working.url, rewrite: path => path.replace(/^\/__lazybox\/runner\/working/, '') },
};
const server = await createServer({ ...testViteCache(import.meta.url), configFile: false, logLevel: 'error', root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react(), runnerList], server: { host: '127.0.0.1', port: 0, proxy } });
const calls = (fixture, method) => fixture.calls.filter(call => call.method === method);
const mutations = (fixture, method) => fixture.mutations.filter(call => call.method === method);
const tab = process.env.TEST_CHROMIUM ? 'Tab' : 'Alt+Tab', shiftTab = process.env.TEST_CHROMIUM ? 'Shift+Tab' : 'Alt+Shift+Tab';
let browser;
try {
  await server.listen(); await mkdir('.cache', { recursive: true });
  browser = await webkit.launch();
  const base = `http://127.0.0.1:${server.httpServer.address().port}/?view=workspace`;

  for (const scheme of ['dark', 'light']) {
    resetWorking();
    const context = await browser.newContext({ viewport: { width: 1440, height: 900 }, colorScheme: scheme });
    const page = await context.newPage(); page.setDefaultTimeout(15000);
    // A reload cuts in-flight polls, which WebKit reports as page errors ("access control checks") on the page being left: not failures.
    let navigating = false;
    const errors = []; page.on('pageerror', e => { if (!navigating) errors.push(String(e)); });
    await page.addInitScript(() => {
      window.__notices = []; window.__focused = true; document.hasFocus = () => window.__focused;
      window.Notification = class { static permission = 'granted'; static async requestPermission() { return 'granted'; }
        constructor(title, options) { this.title = title; this.body = options?.body; window.__notices.push(this); } close() {} };
    });
    const workingCalls = working.calls.length;
    await page.goto(`${base}&collection=alpha&session=alpha-session#token=${dev.token}`);

    // By default a window shows only its own Runner: Working and the release are opt-in, so Dev's window never
    // looks as if it shared Working's state. The brand says which Lazybox this is. One quiet hint names Working.
    const groups = page.locator('.runner-group');
    await expect(page.locator('.workspace-brand .brand-channel')).toHaveText('Dev');
    const hint = page.locator('.toast').filter({ hasText: 'Lazybox Working is also running on this Mac' });
    await expect(hint).toContainText('Show it here? (Settings → Runners)');
    await expect(page.locator('.toast').filter({ hasText: /Release|installed Lazybox/ })).toHaveCount(0);
    await expect(page.locator('.panel-collections')).toContainText('Alpha feature');
    await expect(groups).toHaveCount(0);
    await expect(page.locator('.panel-collections')).not.toContainText('Billing service');
    assert.equal(working.calls.length, workingCalls, 'a Runner the window does not show is not called');
    await page.screenshot({ path: `.cache/multi-runner-own-only-${scheme}.png` });
    const showInWindow = async (...ids) => {
      await page.getByRole('navigation', { name: 'Pages' }).getByRole('button', { name: 'Settings' }).click();
      await page.getByRole('tab', { name: 'Runners', exact: true }).click();
      const list = page.getByRole('list', { name: 'Runners' });
      for (const id of ids) {
        const row = list.locator(`.runners-item[data-runner=${id}]`);
        await expect(row).toHaveAttribute('data-hidden', 'true');
        await expect(row).toContainText('Not shown in this window');
        await row.getByRole('switch', { name: 'Show in this window' }).click();
        await expect(row).not.toHaveAttribute('data-hidden');
      }
      await page.getByRole('navigation', { name: 'Pages' }).getByRole('button').first().click();
    };
    if (scheme === 'dark') {
      // Dismissed, the hint does not come back; Working stays out until the person opts in, in Settings → Runners.
      await hint.getByRole('button', { name: 'Dismiss' }).click();
      navigating = true; await page.reload(); await page.waitForLoadState('load'); navigating = false;
      await expect(page.locator('.panel-collections')).toContainText('Alpha feature');
      await page.waitForTimeout(1000);
      await expect(page.locator('.toast').filter({ hasText: 'also running on this Mac' })).toHaveCount(0);
      await expect(groups).toHaveCount(0);
      assert.equal(working.calls.length, workingCalls, 'still not called');
      await showInWindow('working', 'release');
    } else {
      // "Show it here" opts in from the hint; the release (not running) from Settings.
      await hint.getByRole('button', { name: 'Show it here' }).click();
      await expect(groups).toHaveCount(2);
      await showInWindow('release');
    }

    // Workspaces: one section per Runner, the page's first-class, the stopped one saying so.
    await expect(groups).toHaveCount(3);
    const devGroup = groups.filter({ has: page.locator('.runner-group-name', { hasText: /^Dev$/ }) });
    const workingGroup = groups.filter({ has: page.locator('.runner-group-name', { hasText: /^Working$/ }) });
    const releaseGroup = groups.filter({ has: page.locator('.runner-group-name', { hasText: /^Release$/ }) });
    await expect(devGroup).toHaveAttribute('data-active', 'true');
    await expect(devGroup.locator('.runner-group-version')).toHaveText('0.1.22-dev');
    await expect(workingGroup.locator('.runner-group-version')).toHaveText('0.1.21');
    await expect(workingGroup).toHaveAttribute('data-health', 'online');
    await expect(devGroup.locator('.workspace-row, .collection-group')).not.toHaveCount(0);
    await expect(devGroup).toContainText('Alpha feature');
    await expect(workingGroup).toContainText('Billing service');
    await expect(workingGroup).toContainText('Docs site');
    await expect(devGroup).not.toContainText('Billing service');
    // Each Runner keeps its own order: a row moves within its Runner's section, on that Runner (ArrangeWorkspaces).
    if (scheme === 'dark') {
      const names = workingGroup.locator('.runner-workspace-name');
      const before = await names.allTextContents();
      assert.equal(before.length, 2);
      await workingGroup.locator('.runner-workspace-row').filter({ hasText: before[1] }).focus();
      await page.keyboard.press('Alt+Shift+ArrowUp');
      await expect(names).toHaveText([before[1], before[0]]);
      await expect.poll(() => working.mutations.filter(m => m.method === 'ArrangeWorkspaces').length).toBe(1);
      assert.equal(dev.mutations.filter(m => m.method === 'ArrangeWorkspaces').length, 0, 'another Runner\'s order changed');
      await expect(workingGroup.locator('.runner-workspace-row').filter({ hasText: before[1] })).toBeFocused();
      await page.keyboard.press('Alt+Shift+ArrowDown');
      await expect(names).toHaveText(before);
    }
    await expect(releaseGroup).toHaveAttribute('data-health', 'stopped');
    await expect(releaseGroup.locator('.runner-absent')).toHaveText('Open Lazybox to start it.');
    // What waits on each Runner, on its header.
    await expect(workingGroup.locator('.runner-group-count')).toHaveText('1');
    await expect(devGroup.locator('.runner-group-count')).toHaveText('1');
    assert.equal(await page.locator('.error[role=alert]').count(), 0, 'a stopped Runner is not an error');

    // The Inbox and Agents merge both Runners, most urgent first, with a Runner badge.
    const inboxButton = page.locator('.inbox-action');
    await expect(inboxButton).toHaveAccessibleName('Inbox, 2 conversations need attention');
    const agents = page.locator('.panel-agents');
    const migrate = agents.locator('li').filter({ hasText: 'Migrate the invoices table' });
    const release = agents.locator('li').filter({ hasText: 'Ship release notes' });
    await expect(migrate.locator('.runner-badge')).toHaveText('Working');
    await expect(release.locator('.runner-badge')).toHaveText('Dev');
    await expect(migrate).toHaveAttribute('data-state', 'needs-input');
    await page.mouse.move(0, 0);
    await page.screenshot({ path: `.cache/multi-runner-window-${scheme}.png` });
    await page.locator('.panel-collections').screenshot({ path: `.cache/multi-runner-sidebar-${scheme}.png` }).catch(() => {});

    if (scheme === 'dark') {
      // A pin goes to the Runner the row is on, and only there.
      const devPins = calls(dev, 'PinConversation').length;
      await migrate.hover();
      await migrate.getByRole('button', { name: 'Pin Migrate the invoices table' }).click();
      await expect.poll(() => calls(working, 'PinConversation').length).toBe(1);
      assert.equal(calls(working, 'PinConversation')[0].body.sessionId, 'alpha-session');
      assert.equal(calls(dev, 'PinConversation').length, devPins, 'pinning a Working row never calls Dev');

      // Peek reads the conversation from its Runner.
      const peeked = calls(working, 'WatchEvents').filter(c => c.body.sessionId === 'alpha-session').length;
      await migrate.locator('.agent-row').focus();
      await page.keyboard.press('Space');
      const peek = page.getByRole('dialog', { name: /Peek/ });
      await expect(peek).toBeVisible();
      await expect(peek.locator('.peek-runner')).toHaveText('Working');
      // Working's alpha-session, not Dev's (both Runners have one).
      await expect(peek.getByRole('heading', { name: 'Migrate the invoices table' })).toBeVisible();
      await expect.poll(() => calls(working, 'WatchEvents').filter(c => c.body.sessionId === 'alpha-session').length).toBeGreaterThan(peeked);
      await page.keyboard.press('Escape');
      await expect(peek).toHaveCount(0);

      // Opening a Working conversation shows Working's page; the approval is answered there.
      const devApplies = mutations(dev, 'Apply').length;
      await migrate.locator('.agent-row').click();
      await expect(workingGroup).toHaveAttribute('data-active', 'true');
      await expect(page.getByRole('combobox', { name: 'Workspace', exact: true })).toHaveValue('alpha');
      await expect(page.getByRole('combobox', { name: 'Workspace', exact: true }).locator('option:checked')).toHaveText('Billing service');
      const card = page.locator('.request').filter({ hasText: 'npm run migrate' });
      await expect(card).toBeVisible();
      await page.screenshot({ path: '.cache/multi-runner-other-page-dark.png' });
      await card.getByRole('button', { name: 'Allow once' }).click();
      await expect.poll(() => mutations(working, 'Apply').filter(m => m.body.approval?.requestId === 'a-migrate').length).toBe(1);
      assert.equal(mutations(dev, 'Apply').length, devApplies, 'an approval on Working never reaches Dev');
      Object.assign(working.sessions[0], { status: 'running', busy: true, pending: [], agents: root('running', Date.now()) });
      await expect(inboxButton).toHaveAccessibleName('Inbox, 1 conversation needs attention');

      // Dev's workspaces are still in the sidebar: one of them brings Dev's page back.
      await devGroup.getByText('Alpha feature').first().click();
      await expect(devGroup).toHaveAttribute('data-active', 'true');
      await expect(page.getByRole('combobox', { name: 'Workspace', exact: true }).locator('option:checked')).toHaveText('Alpha feature');

      // The Inbox popover (Agents panel closed) is merged too: answering Dev's question stays on Dev.
      // ⌘K: Working's workspaces and Working itself, badged.
      await page.keyboard.press('Meta+k');
      const palette = page.getByRole('dialog', { name: /Command palette|Go to/ });
      const query = page.getByRole('combobox').filter({ hasNot: page.locator('select') }).last();
      await page.keyboard.type('Docs site');
      const docs = page.locator('.command-palette-row').filter({ hasText: 'Docs site' }).first();
      await expect(docs.locator('.runner-badge')).toHaveText('Working');
      await page.keyboard.press('Enter');
      await expect(workingGroup).toHaveAttribute('data-active', 'true');
      await expect(page.getByRole('combobox', { name: 'Workspace', exact: true })).toHaveValue('beta');
      void palette; void query;

      // ⌘N on Working's page starts on Working, and its title says so; "on Working ▾" moves it to Dev, keeping the message.
      await page.keyboard.press('Meta+n');
      const sheet = page.getByRole('dialog', { name: /^New task/ });
      await expect(sheet).toBeVisible();
      await expect(sheet).toHaveAccessibleName('New task on Working');
      await sheet.getByRole('textbox', { name: 'First message' }).fill('Write the changelog for 0.1.22');
      const runnerPick = sheet.getByRole('button', { name: /^on / });
      await expect(runnerPick).toHaveAccessibleName('on Working');
      await runnerPick.click();
      const runnerMenu = sheet.getByRole('menu', { name: 'Start on' });
      await expect(runnerMenu.getByRole('menuitemradio', { name: /Working/ })).toHaveAttribute('aria-checked', 'true');
      await expect(runnerMenu.getByRole('menuitemradio', { name: /Working/ })).toBeFocused();
      await expect(runnerMenu.getByRole('menuitemradio', { name: /Release/ })).toBeDisabled();
      await sheet.screenshot({ path: '.cache/multi-runner-new-task-dark.png' });
      await runnerMenu.getByRole('menuitemradio', { name: /Dev/ }).click();
      await expect(sheet).toHaveAccessibleName('New task on Dev');
      await expect(runnerPick).toHaveAccessibleName('on Dev');
      await expect(runnerPick).toBeFocused();
      await expect(runnerMenu).toHaveCount(0);
      await expect(sheet.getByRole('textbox', { name: 'First message' })).toHaveValue('Write the changelog for 0.1.22');
      const workingPrepares = mutations(working, 'PrepareWorkspace').length;
      await sheet.getByRole('combobox', { name: 'Add a repository' }).click();
      await sheet.getByRole('option', { name: /Start without a repository/ }).click();
      await sheet.getByRole('textbox', { name: 'First message' }).press('Enter');
      await expect(sheet).toHaveCount(0);
      await expect.poll(() => mutations(dev, 'PrepareWorkspace').length).toBe(1);
      await expect.poll(() => mutations(dev, 'Apply').filter(m => m.body.message?.text === 'Write the changelog for 0.1.22').length).toBe(1);
      assert.equal(mutations(working, 'PrepareWorkspace').length, workingPrepares, 'a task started on Dev never reaches Working');
      await expect(devGroup).toHaveAttribute('data-active', 'true');

      // The panel's + names the Runner it starts on: the one the page shows. Each Runner's section has a + of its own.
      const headerNew = page.locator('.runner-grouped > .explorer-heading').getByRole('button', { name: 'New workspace on Dev' });
      await expect(headerNew).toHaveAttribute('title', /^New workspace on Dev, the Runner the page shows \(⌘N\)/);
      await expect(devGroup.getByRole('button', { name: 'New workspace on Dev' })).toBeVisible();
      await expect(workingGroup.getByRole('button', { name: 'New workspace on Working' })).toBeEnabled();
      await expect(releaseGroup.getByRole('button', { name: 'New workspace on Release' })).toBeDisabled();
      await page.locator('.panel-collections').screenshot({ path: '.cache/multi-runner-sidebar-new-dark.png' }).catch(() => {});
      await headerNew.click();
      await expect(sheet).toHaveAccessibleName('New task on Dev');
      await page.keyboard.press('Escape');
      await expect(sheet).toHaveCount(0);

      // Working's + starts on Working while the page shows Dev: only Working is asked, and the page moves there once it starts.
      const prepares = { dev: mutations(dev, 'PrepareWorkspace').length, working: mutations(working, 'PrepareWorkspace').length };
      await workingGroup.getByRole('button', { name: 'New workspace on Working' }).click();
      await expect(sheet).toHaveAccessibleName('New task on Working');
      await expect(devGroup).toHaveAttribute('data-active', 'true');
      await sheet.getByRole('textbox', { name: 'First message' }).fill('Bump the invoice schema');
      await sheet.getByRole('combobox', { name: 'Add a repository' }).click();
      await sheet.getByRole('option', { name: /Start without a repository/ }).click();
      await sheet.getByRole('textbox', { name: 'First message' }).press('Enter');
      await expect(sheet).toHaveCount(0);
      await expect.poll(() => mutations(working, 'PrepareWorkspace').length).toBe(prepares.working + 1);
      await expect.poll(() => mutations(working, 'Apply').filter(m => m.body.message?.text === 'Bump the invoice schema').length).toBe(1);
      assert.equal(mutations(dev, 'PrepareWorkspace').length, prepares.dev, "Working's + never reaches Dev");
      await expect(workingGroup).toHaveAttribute('data-active', 'true');

      // Keyboard only (⌥Tab in WebKit, as Safari moves between buttons; Tab elsewhere): from Dev's header to its +, Enter, then "on Dev ▾" (⇧⌥Tab) and ↓ to change the Runner; Escape closes the menu, not the sheet.
      await devGroup.locator('.runner-group-toggle').focus();
      await page.keyboard.press(tab);
      await expect(devGroup.getByRole('button', { name: 'New workspace on Dev' })).toBeFocused();
      await page.keyboard.press('Enter');
      await expect(sheet).toHaveAccessibleName('New task on Dev');
      await expect(sheet.getByRole('textbox', { name: 'First message' })).toBeFocused();
      await page.keyboard.type('Tidy the release script');
      await page.keyboard.press(shiftTab);
      await page.keyboard.press(shiftTab);
      await expect(runnerPick).toBeFocused();
      await page.keyboard.press('ArrowDown');
      await expect(runnerMenu.getByRole('menuitemradio', { name: /Dev/ })).toBeFocused();
      await page.keyboard.press('Escape');
      await expect(runnerMenu).toHaveCount(0);
      await expect(sheet).toBeVisible();
      await expect(runnerPick).toBeFocused();
      await page.keyboard.press('Enter');
      await expect(runnerMenu.getByRole('menuitemradio', { name: /Dev/ })).toBeFocused();
      await page.keyboard.press('ArrowDown');
      await expect(runnerMenu.getByRole('menuitemradio', { name: /Working/ })).toBeFocused();
      await page.keyboard.press('Enter');
      await expect(sheet).toHaveAccessibleName('New task on Working');
      await expect(runnerPick).toBeFocused();
      await expect(sheet.getByRole('textbox', { name: 'First message' })).toHaveValue('Tidy the release script');
      await page.keyboard.press(tab);
      await page.keyboard.press(tab);
      await expect(sheet.getByRole('textbox', { name: 'First message' })).toBeFocused();
      // Working remembers no repository: Enter asks for one, ↑ is "Start without a repository", Enter takes it, Enter starts.
      await page.keyboard.press('Enter');
      await expect(sheet.getByRole('combobox', { name: 'Add a repository' })).toBeFocused();
      await page.keyboard.press('ArrowUp');
      await page.keyboard.press('Enter');
      await expect(sheet.getByRole('textbox', { name: 'First message' })).toBeFocused();
      await page.keyboard.press('Enter');
      await expect(sheet).toHaveCount(0);
      await expect.poll(() => mutations(working, 'Apply').filter(m => m.body.message?.text === 'Tidy the release script').length).toBe(1);
      assert.equal(mutations(dev, 'PrepareWorkspace').length, prepares.dev, 'a task moved to Working never reaches Dev');

      // A Runner with no workspaces says so, with "Create one": the same as its +.
      await devGroup.getByText('Alpha feature').first().click();
      await expect(devGroup).toHaveAttribute('data-active', 'true');
      const kept = working.collections.splice(0);
      await expect(workingGroup.locator('.runner-empty')).toContainText('No workspaces on Working yet.', { timeout: 15_000 });
      const createOne = workingGroup.getByRole('button', { name: 'Create one' });
      await workingGroup.locator('.runner-group-toggle').focus();
      await page.keyboard.press(tab);
      await page.keyboard.press(tab);
      await page.keyboard.press(tab);
      await expect(createOne).toBeFocused();
      await page.locator('.panel-collections').screenshot({ path: '.cache/multi-runner-empty-dark.png' }).catch(() => {});
      await page.keyboard.press('Enter');
      await expect(sheet).toHaveAccessibleName('New task on Working');
      await page.keyboard.press('Escape');
      await expect(sheet).toHaveCount(0);
      await createOne.click();
      await expect(sheet).toHaveAccessibleName('New task on Working');
      await page.keyboard.press('Escape');
      await expect(sheet).toHaveCount(0);
      working.collections.push(...kept);
      await expect(workingGroup).toContainText('Billing service', { timeout: 15_000 });
      await expect(devGroup).toHaveAttribute('data-active', 'true');

      // A notification from Working while the person is elsewhere names Working.
      await page.evaluate(() => { window.__focused = false; });
      Object.assign(working.sessions[1], { status: 'waiting_approval', agents: root('waiting_approval', Date.now()), pending: [{ type: 'approval', requestId: 'a-docs', text: 'Bash', providerDataJson: data({ input: { command: 'npm run build-docs' } }) }] });
      await expect.poll(() => page.evaluate(() => window.__notices.map(n => n.title))).toContain('Approval needed · Rewrite the docs navigation');
      assert.match(await page.evaluate(() => window.__notices.find(n => n.title.includes('docs navigation')).body), /^Working · Docs site/);
      await page.evaluate(() => { window.__focused = true; });

      // Working goes away: its section says so, its rows leave the inbox, nothing is an error.
      working.runner.down = true;
      await expect(workingGroup).toHaveAttribute('data-health', 'offline', { timeout: 10_000 });
      await expect(workingGroup.locator('.runner-group-state')).toHaveText('Not reachable');
      await expect(agents.locator('.agents-runner-offline')).toContainText('Not reachable');
      await expect(agents.locator('li').filter({ hasText: 'Rewrite the docs navigation' })).toHaveCount(0);
      await expect(inboxButton).toHaveAccessibleName('Inbox, 1 conversation needs attention');
      assert.equal(await page.locator('.error[role=alert]').count(), 0, 'another Runner going away is not an error on this page');
      await page.mouse.move(0, 0);
      await page.screenshot({ path: '.cache/multi-runner-offline-dark.png' });
      working.runner.down = false;
      await expect(workingGroup).toHaveAttribute('data-health', 'online', { timeout: 10_000 });
      await expect(agents.locator('li').filter({ hasText: 'Rewrite the docs navigation' })).toHaveCount(1);

      // A folded Runner stays folded.
      await workingGroup.locator('.runner-group-toggle').click();
      await expect(workingGroup.locator('.runner-group-toggle')).toHaveAttribute('aria-expanded', 'false');
      await expect(workingGroup).not.toContainText('Billing service');
      await workingGroup.locator('.runner-group-toggle').click();

      // With the Agents panel closed, the Inbox is a list over the page: merged, badged, and it opens on the row's Runner.
      await page.locator('.dv-tab').filter({ has: page.locator('[data-kind="agents"]') }).locator('.dock-tab-close').click();
      await inboxButton.click();
      const inbox = page.getByRole('dialog', { name: 'Inbox' });
      const docsRow = inbox.locator('.inbox-item').filter({ hasText: 'Rewrite the docs navigation' });
      await expect(docsRow.locator('.runner-badge')).toHaveText('Working');
      await expect(inbox.locator('.inbox-item').filter({ hasText: 'Ship release notes' }).locator('.runner-badge')).toHaveText('Dev');
      await inbox.screenshot({ path: '.cache/multi-runner-inbox-dark.png' });
      await docsRow.locator('.inbox-row').click();
      await expect(inbox).toHaveCount(0);
      await expect(workingGroup).toHaveAttribute('data-active', 'true');
      await expect(page.getByRole('combobox', { name: 'Workspace', exact: true })).toHaveValue('beta');
      await expect(page.locator('.request').filter({ hasText: 'npm run build-docs' })).toBeVisible();

      // Settings → Runners: hide one, make another primary; the window then opens on it.
      await page.getByRole('navigation', { name: 'Pages' }).getByRole('button', { name: 'Settings' }).click();
      await page.getByRole('tab', { name: 'Runners', exact: true }).click();
      const list = page.getByRole('list', { name: 'Runners' });
      await expect(list.locator('.runners-item')).toHaveCount(3);
      await page.screenshot({ path: '.cache/multi-runner-settings-dark.png' });
      await list.locator('.runners-item[data-runner=release]').getByRole('switch').click();
      await expect(list.locator('.runners-item[data-runner=release]')).toHaveAttribute('data-hidden', 'true');
      await list.locator('.runners-item[data-runner=working]').getByRole('button', { name: 'Make primary' }).click();
      await expect(list.locator('.runners-item[data-runner=working] .runners-tag.is-primary')).toHaveText('Primary');
      await page.getByRole('navigation', { name: 'Pages' }).getByRole('button').first().click();
      await expect(groups).toHaveCount(2);
      navigating = true; await page.reload(); await page.waitForLoadState('load'); navigating = false;
      await expect(page.locator('.runner-group[data-active]')).toHaveAttribute('data-runner', 'working');
      await expect(page.locator('.runner-group')).toHaveCount(2);
      // Working stopping hands the page back to Dev, quietly.
      listing.workingState = 'stopped';
      await expect(page.locator('.runner-group[data-active]')).toHaveAttribute('data-runner', 'window', { timeout: 15_000 });
      listing.workingState = 'running';
    }
    assert.deepEqual(errors, []);
    await context.close();
  }
  console.log('multi-runner: ok');
} finally {
  await browser?.close(); await server.close(); await dev.close(); await working.close();
}
