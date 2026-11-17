// Attention inbox: counts across workspaces, grouping, keyboard navigation,
// mark-as-seen, opening a conversation in its workspace, sidebar unread
// markers, notifications on watched transitions (and none for the baseline),
// and the phone layout. Fake Runner; no agent execution is started or stopped.
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from 'vite';
import react from '@vitejs/plugin-react';
import {webkit, chromium, expect} from './fixtures/playwright.mjs';
import {fileURLToPath} from 'node:url';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';

const f = await startRunnerFixture();
const now = Date.now(), ms = value => String(value);
const data = value => Buffer.from(JSON.stringify(value)).toString('base64');
const root = (status, updated, extra = {}) => [{id: 'root', name: 'Main agent', status, updatedAtUnixMillis: ms(updated), ...extra}];
const base = {profileId: 'claude-default', provider: 'claude', capabilities: {files: true}, pending: []};
Object.assign(f.sessions[0], {title: 'Index the API', model: 'claude-fable-5', busy: true});
Object.assign(f.sessions[1], {title: 'Ship release notes', provider: 'codex', model: 'gpt-codex-6', status: 'waiting_input', agents: root('waiting_input', now - 300_000),
  pending: [{type: 'question', requestId: 'q-release', providerDataJson: data({questions: [{question: 'Which version should the notes cover?'}]})}]});
f.sessions.push(
  {...base, id: 'review-session', collectionId: 'alpha', title: 'Refactor the parser', model: 'claude-fable-5', status: 'running', busy: true, createdAtUnixMillis: ms(now - 3_600_000), agents: root('running', now - 10_000), lastMessage: 'Reading parser.go'},
  {...base, id: 'failing-session', collectionId: 'beta', title: 'Deploy preview', model: 'claude-fable-5', status: 'running', busy: true, createdAtUnixMillis: ms(now - 7_200_000), agents: root('running', now - 20_000)},
  {...base, id: 'approval-session', collectionId: 'beta', title: 'Bump dependencies', model: 'claude-fable-5', status: 'running', busy: true, createdAtUnixMillis: ms(now - 5_000_000), agents: root('running', now - 30_000)},
  {...base, id: 'old-session', collectionId: 'alpha', title: 'Old exploration', model: 'claude-fable-5', status: 'done', createdAtUnixMillis: ms(now - 9e7), agents: root('done', now - 8.64e7, {turnEndedAtUnixMillis: ms(now - 8.64e7), lastTurnStatus: 'completed'}), lastMessage: 'Nothing else to do.'},
  {...base, id: 'helper-session', collectionId: 'alpha', title: 'Ask Lazybox', purpose: 'help', status: 'waiting_input', pending: [{type: 'question', requestId: 'q-help'}]},
);
const session = id => f.sessions.find(s => s.id === id);

const server = await createServer({...testViteCache(import.meta.url),configFile: false, root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: {host: '127.0.0.1', port: 0, proxy: {'/lazybox.bench.v1.RunnerService/': {target: f.url}}}});
let browser;
try {
  await server.listen();
  browser = await (process.env.TEST_CHROMIUM ? chromium : webkit).launch();
  const page = await browser.newPage({viewport: {width: 1440, height: 900}});
  const errors = []; page.on('pageerror', e => errors.push(String(e)));
  // A recording stand-in for the browser's Notification API, already allowed.
  await page.addInitScript(() => {
    window.__notices = [];
    // Whether Lazybox is the app in front: a window in front shows a toast instead of a banner.
    window.__focused = true; document.hasFocus = () => window.__focused;
    window.Notification = class { static permission = 'granted'; static async requestPermission() { return 'granted'; }
      constructor(title, options) { this.title = title; this.body = options?.body; this.tag = options?.tag; window.__notices.push(this); } close() {} };
  });
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${f.token}`);
  const inboxButton = page.locator('.inbox-action');
  // Only the pending question counts; the assistant panel's question is not a work item.
  await expect(inboxButton).toHaveAccessibleName('Inbox, 1 conversation needs attention');
  await expect(inboxButton.locator('.inbox-count')).toHaveText('1');
  await expect(inboxButton.locator('.inbox-count')).toHaveClass(/urgent/);
  await expect.poll(() => page.title()).toMatch(/^\(1\) /);
  await page.waitForTimeout(3500);
  assert.deepEqual(await page.evaluate(() => window.__notices.map(n => n.title)), [], 'what was already waiting at load is a baseline, not news');

  // Three agents move while the person is in another app: one finishes, one fails, one asks.
  await page.evaluate(() => { window.__focused = false; });
  Object.assign(session('review-session'), {status: 'done', busy: false, agents: root('done', Date.now(), {turnEndedAtUnixMillis: ms(Date.now()), lastTurnStatus: 'completed'}), lastMessage: 'I split the parser into three files.\n\n**All 42 tests pass.**'});
  Object.assign(session('failing-session'), {status: 'failed', busy: false, fault: 'provider delivery failed; reconcile before continuing', agents: root('failed', Date.now())});
  Object.assign(session('approval-session'), {status: 'waiting_approval', busy: false, agents: root('waiting_approval', Date.now()), pending: [{type: 'approval', requestId: 'a-deps', text: 'Bash', providerDataJson: data({input: {command: 'npm update'}})}]});
  await expect(inboxButton).toHaveAccessibleName('Inbox, 4 conversations need attention', {timeout: 10_000});
  await expect.poll(() => page.evaluate(() => window.__notices.map(n => n.title).sort())).toEqual(['Approval needed · Bump dependencies', 'Failed · Deploy preview']);
  assert.equal(await page.evaluate(() => window.__notices.find(n => n.title.startsWith('Failed')).body), 'Beta release · A message could not be delivered to the agent');
  await expect.poll(() => page.title()).toMatch(/^\(2\) /);
  assert.equal(await page.locator('.toast').count(), 0, 'a window in the background shows banners, not toasts');
  await page.evaluate(() => { window.__focused = true; });

  // Sidebar: the finished conversation is marked unread in its workspace.
  const reviewRow = page.locator('.page-layout .conversation-row').filter({hasText: 'Refactor the parser'});
  await expect(reviewRow).toHaveAttribute('data-unread', 'review');
  await expect(reviewRow.locator('.session-dot.unread')).toBeVisible();

  // The Inbox is the Agents panel under Workspaces: the entry brings it forward, its first row focused.
  const agentsList = page.locator('.panel-agents');
  await inboxButton.click();
  await expect(agentsList.locator('.agent-row').first()).toBeFocused();
  await expect(agentsList.locator('.agent-row').first()).toContainText('Bump dependencies');
  await expect(page.getByRole('dialog', {name: 'Inbox'})).toHaveCount(0);
  // With the Agents panel closed, the Inbox is a list over the page.
  await page.locator('.dv-tab').filter({has: page.locator('[data-kind="agents"]')}).locator('.dock-tab-close').click();
  await expect(agentsList).toHaveCount(0);

  // The inbox, grouped by need, newest first, with workspace, agent and last line.
  await inboxButton.click();
  const inbox = page.getByRole('dialog', {name: 'Inbox'});
  await expect(inbox).toBeVisible();
  await expect(inbox.locator('.inbox-summary')).toHaveText('2 waiting for you');
  assert.deepEqual(await inbox.locator('.inbox-group h3').allInnerTexts().then(v => v.map(t => t.replace(/\s+/g, ' ').trim())), ['NEEDS INPUT 2', 'FAILED 1', 'READY FOR REVIEW 1', 'WORKING 1', 'DONE 1']);
  assert.deepEqual(await inbox.locator('.group-needs-input .inbox-title').allInnerTexts(), ['Bump dependencies', 'Ship release notes'], 'newest first');
  const review = inbox.locator('.inbox-item').filter({hasText: 'Refactor the parser'});
  await expect(review).toContainText('Alpha feature');
  await expect(review).toContainText('Claude · claude-fable-5');
  await expect(review.locator('.inbox-line')).toHaveText('Turn ended · All 42 tests pass.');
  await expect(review.locator('.inbox-age')).toHaveText('now');
  await expect(inbox.locator('.inbox-item').filter({hasText: 'Ship release notes'}).locator('.inbox-line')).toHaveText('Question · Which version should the notes cover?');
  await expect(inbox.locator('.inbox-item').filter({hasText: 'Bump dependencies'}).locator('.inbox-line')).toHaveText('Approval · Run npm update');
  await expect(inbox).not.toContainText('Ask Lazybox');
  await page.screenshot({path: '/tmp/lazybox-inbox.png'});

  // Keyboard: j/k move between rows, m marks the focused row seen.
  await expect(inbox.locator('.inbox-row').first()).toBeFocused();
  await page.keyboard.press('j'); await page.keyboard.press('j');
  await expect(inbox.locator('.inbox-row').nth(2)).toBeFocused();
  await page.keyboard.press('k');
  await expect(inbox.locator('.inbox-row').nth(1)).toBeFocused();
  await review.locator('.inbox-row').focus();
  await page.keyboard.press('m');
  await expect(inbox.locator('.group-review')).toHaveCount(0);
  await expect(inbox.locator('.group-idle .inbox-item').filter({hasText: 'Refactor the parser'})).toBeVisible();
  await expect(inboxButton).toHaveAccessibleName('Inbox, 3 conversations need attention');
  await expect(reviewRow).not.toHaveAttribute('data-unread', /.+/);

  // Enter opens a conversation in its own workspace, and looking at it marks it seen.
  await inbox.locator('.inbox-item').filter({hasText: 'Deploy preview'}).locator('.inbox-row').focus();
  await page.keyboard.press('Enter');
  await expect(inbox).toHaveCount(0);
  await expect(page.getByRole('combobox', {name: 'Workspace', exact: true})).toHaveValue('beta');
  await expect(page.getByRole('combobox', {name: 'Session', exact: true})).toHaveValue('failing-session');
  await expect(inboxButton).toHaveAccessibleName('Inbox, 2 conversations need attention', {timeout: 10_000});
  // Closed is closed for the window, not for a page: Beta's page has no Agents either.
  await expect(agentsList).toHaveCount(0);
  // Brought back from Panels, the Inbox brings it forward on Beta's page; it marks the open conversation.
  await page.locator('summary[aria-label="Panels"]').click();
  await page.getByRole('button', {name: 'Show Agents'}).click();
  await expect(agentsList).toBeVisible();
  await page.locator('.inbox-action').click();
  await expect(inbox).toHaveCount(0);
  await expect(agentsList.locator('li[data-state="failed"]')).not.toHaveAttribute('data-unread', /.+/);
  await expect(agentsList.locator('li[data-state="failed"] .agent-row[aria-current]')).toHaveCount(1);
  await page.locator('.dv-tab').filter({has: page.locator('[data-kind="agents"]')}).locator('.dock-tab-close').click();
  await expect(agentsList).toHaveCount(0);
  await page.locator('.inbox-action').click();
  await expect(inbox.locator('.group-failed .inbox-item')).not.toHaveAttribute('data-unread', /.+/);
  await expect(inbox.locator('.group-failed .inbox-row[aria-current]')).toHaveCount(1);
  await page.keyboard.press('Escape');
  await expect(inbox).toHaveCount(0);

  // A click on a notification opens its conversation.
  await page.evaluate(() => window.__notices.find(n => n.title.startsWith('Approval')).onclick());
  await expect(page.getByRole('combobox', {name: 'Session', exact: true})).toHaveValue('approval-session');
  // The needs-input count stays until the request is answered: seeing it is not answering it.
  await expect(inboxButton).toHaveAccessibleName('Inbox, 2 conversations need attention');

  // The keyboard shortcut opens the inbox from anywhere outside a text field.
  await page.locator('.workspace-brand').click();
  await page.keyboard.press(']'); await page.keyboard.press(']'); await page.keyboard.press('i');
  await expect(inbox).toBeVisible();
  await page.keyboard.press('Escape');

  // Settings → Notifications: toggles persist for every window; a test notification is sent.
  await page.getByRole('button', {name: 'Settings', exact: true}).click();
  await page.getByRole('tab', {name: 'Notifications', exact: true}).click();
  const reviewToggle = page.getByRole('switch', {name: /Ready for review/});
  await expect(reviewToggle).not.toBeChecked();
  await expect(page.getByRole('switch', {name: /Needs input/})).toBeChecked();
  await reviewToggle.check();
  await page.getByRole('button', {name: 'Send a test notification'}).click();
  await expect.poll(() => page.evaluate(() => window.__notices.at(-1).title)).toBe('Lazybox notifications are on');
  await page.screenshot({path: '/tmp/lazybox-notification-settings.png'});
  await page.reload();
  await page.getByRole('button', {name: 'Settings', exact: true}).click();
  await page.getByRole('tab', {name: 'Notifications', exact: true}).click();
  await expect(page.getByRole('switch', {name: /Ready for review/})).toBeChecked();
  await page.getByRole('button', {name: 'Settings', exact: true}).click();

  // With review notifications on, the next finished turn of a conversation not on screen notifies.
  await expect(inboxButton).toHaveAccessibleName('Inbox, 2 conversations need attention');
  await inboxButton.click(); await inbox.locator('.inbox-row').filter({hasText: 'Bump dependencies'}).click();
  await expect(page.getByRole('combobox', {name: 'Session', exact: true})).toHaveValue('approval-session');
  Object.assign(session('alpha-session'), {status: 'done', busy: false, agents: root('done', Date.now(), {turnEndedAtUnixMillis: ms(Date.now()), lastTurnStatus: 'completed'}), lastMessage: 'Indexed 12 endpoints.'});
  // Lazybox is in front, so it is a toast in the window, not a system banner.
  await expect(page.locator('.toast[data-tone="review"]')).toContainText('Ready for review · Index the API', {timeout: 10_000});
  assert.equal(await page.evaluate(() => window.__notices.filter(n => n.title.startsWith('Ready for review')).length), 0);
  await page.locator('.toast[data-tone="review"]').getByRole('button', {name: /Ready for review/}).click();
  await expect(page.getByRole('combobox', {name: 'Session', exact: true})).toHaveValue('alpha-session');

  // Phone: the entry stays in the header as an icon with its count; the list fills the screen.
  await page.setViewportSize({width: 390, height: 844});
  await expect(inboxButton).toBeVisible();
  await expect(inboxButton.locator('.inbox-count')).toBeVisible();
  await inboxButton.click();
  const box = await inbox.boundingBox();
  assert.ok(box.width >= 389 && box.height >= 840, 'phone inbox is full screen');
  assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false);
  await page.screenshot({path: '/tmp/lazybox-inbox-phone.png'});
  await inbox.locator('.inbox-row').filter({hasText: 'Index the API'}).click();
  await expect(inbox).toHaveCount(0);

  // A mission task waiting for acceptance is a row j/k stop on, and a note being typed in it is not
  // thrown away: a click outside keeps the Inbox (the note's box shakes), Escape in the note is Back.
  await page.setViewportSize({width: 1400, height: 900});
  const mission = f.missions.create('Ship the parser split');
  const task = f.missions.addTask(mission, 'Split the parser');
  f.missions.assign(mission, task, session('review-session'), false);
  Object.assign(task, {report: 'Split into lexer, parser and ast.', reportedAt: ms(Date.now()), gate: 'reviewed', rejected: false, reportedAfterAttempt: true, updated: ms(Date.now())});
  await expect(inboxButton).toHaveAccessibleName(/1 task awaits acceptance/, {timeout: 15_000});
  await inboxButton.click();
  const taskRow = inbox.locator('.inbox-task').filter({hasText: 'Split the parser'});
  await expect(taskRow).toBeFocused();
  await page.keyboard.press('j');
  await expect(inbox.locator('.inbox-row').first()).toBeFocused();
  await page.keyboard.press('k');
  await expect(taskRow).toBeFocused();
  await taskRow.getByRole('button', {name: 'Ask for changes…'}).click();
  const note = taskRow.getByRole('textbox', {name: 'What should change in Split the parser'});
  await note.fill('Keep the old entry point');
  const inboxBox = await inbox.boundingBox();
  await page.mouse.click(inboxBox.x > 20 ? 8 : inboxBox.x + inboxBox.width + 8, 450);
  await expect(inbox).toBeVisible();
  await expect(note).toHaveValue('Keep the old entry point');
  await note.press('Escape');
  await expect(inbox).toBeVisible();
  await expect(note).toHaveCount(0);
  await expect(taskRow.getByRole('button', {name: 'Ask for changes…'})).toBeFocused();
  // Nothing typed: Escape closes the Inbox as before.
  await page.keyboard.press('Escape');
  await expect(inbox).toHaveCount(0);

  // Settings → Notifications saves to settings.yaml (UpdateConfigFile, package C): a preference, not an agent action.
  assert.deepEqual(f.mutations.filter(m => m.method !== 'UpdateConfigFile'), [], 'the inbox never starts, stops or answers an agent');
  assert.deepEqual(errors, []);
  console.log('PASS inbox: counts across workspaces, grouping, j/k/m/Enter, open in workspace, seen on view, sidebar unread, notifications on watched transitions only, notification click, settings, phone; j/k stop on a mission task, and a typed note survives a click outside and Escape (Back).');
} finally { await browser?.close(); await server.close(); await f.close(); }
