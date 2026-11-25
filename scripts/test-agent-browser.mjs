// The agent browser (BROWSER.md) against the fake Runner: the chat's
// "Claude is browsing" chip and the panel shown beside the chat when browsing
// starts; live frames drawn on the canvas; the step log (running → done,
// refused, failed) with the cursor and highlight where the agent acts; the
// site, download and deny-with-a-reason cards and their keys; Take over (your
// clicks and keys reach the page in page pixels, your own address bar) and
// Give back; Stop and "Let Claude browse again"; "Give Claude the browser" for
// an agent that started without it; Preview :<port> for a server the agent
// runs; Settings → Browser, with Chromium missing (Install Chromium and its
// output), off (Turn on) and off for one workspace; a phone width. Frames are
// real JPEGs of little fixture pages. Chromium.
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
const id = 'alpha-session';
const session = fixture.sessions.find(s => s.id === id);
Object.assign(session, { title: 'Research the release notes', provider: 'claude', status: 'running', busy: true });
const fake = fixture.browser;
const server = await createServer({ ...testViteCache(import.meta.url), configFile: false, logLevel: 'error', root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: fixture.url } } } });
let browser;
try {
  await server.listen(); browser = await chromium.launch({ headless: true });
  // Frames: little pages rendered to JPEG, as the Runner's screencast sends them.
  // page-errors-opt-out: paints the fake sites' screenshots from set content; runs no Lazybox code.
  const painter = await browser.newPage({ viewport: { width: 1280, height: 800 } });
  const paint = async (title, body, color = '#2b6cb0') => {
    await painter.setContent(`<!doctype html><body style="margin:0;font:18px system-ui;background:#fff"><header style="background:${color};color:#fff;padding:18px 28px;display:flex;justify-content:space-between"><b>${title}</b><span style="border:2px solid #fff;padding:4px 14px;border-radius:6px">Sign in</span></header><main style="padding:28px">${body}</main></body>`);
    return painter.screenshot({ type: 'jpeg', quality: 70 });
  };
  const docs = await paint('Example docs', '<h1>Guide</h1><p>Install, configure, ship.</p><input placeholder="Search" style="font-size:18px;padding:8px;width:420px">');
  const news = await paint('Example news', '<h1>Release notes</h1><ul><li>2.4: faster</li><li>2.3: safer</li></ul>', '#2f855a');

  const context = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  await context.grantPermissions(['clipboard-read', 'clipboard-write']);
  const page = await context.newPage(); page.setDefaultTimeout(15000);
  const errors = []; await collectPageErrors(page, errors);
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=${id}#token=${fixture.token}`);
  await mkdir('.cache', { recursive: true });
  const chat = page.locator('.panel-chat');
  await expect(chat.getByRole('textbox', { name: 'Message', exact: true })).toBeVisible();
  const chip = chat.locator('.browsing-badge');
  const panel = page.locator('section.workspace-panel.panel-browser');
  await expect(chip).toHaveCount(0);
  await expect(panel).toHaveCount(0);
  // The chat has seen this conversation without a browser before it starts.
  await page.waitForTimeout(1800);

  // ── The agent opens its browser: the chip, and the panel beside the chat (once).
  fake.setState(id, { status: 'open', controller: 'agent', url: 'https://docs.example.com/guide', title: 'Guide — Example docs', canGoBack: false });
  fake.act(id, { kind: 'open', summary: 'Opened docs.example.com', url: 'https://docs.example.com/guide' });
  fake.frame(id, docs);
  await expect(chip).toHaveText('Claude is browsing');
  await expect(panel).toBeVisible();
  const toolbar = panel.getByRole('toolbar', { name: 'Agent browser' });
  await expect(toolbar.getByRole('status')).toHaveText('Claude is browsing');
  await expect(panel.locator('.browser-address .web-origin')).toHaveText('docs.example.com');
  await expect(panel.locator('.browser-address .browser-title')).toHaveText('Guide — Example docs');
  // The panel's tab names the page.
  const tab = page.locator('.dv-tab').filter({ has: page.locator('.dock-tab-title', { hasText: 'docs.example.com' }) });
  await expect(tab).toHaveCount(1);

  await page.evaluate(() => { window.savedOpen = window.open; window.open = () => null; });
  await page.getByRole('button', { name: 'Detach browser panel' }).click();
  const refusal = page.getByRole('alert').filter({ hasText: 'The host blocked the new panel window.' });
  await expect(refusal).toBeVisible();
  await expect(toolbar).toBeVisible();
  await refusal.getByRole('button', { name: 'Dismiss' }).click();
  // A native host can create the window but deny access to its document.
  await page.evaluate(() => {
    window.open = () => ({ close() {}, addEventListener() { throw new Error('Window access refused'); } });
  });
  await page.getByRole('button', { name: 'Detach browser panel' }).click();
  const inaccessible = page.getByRole('alert').filter({ hasText: 'Lazybox could not display the panel in the new window. Window access refused' });
  await expect(inaccessible).toBeVisible();
  await expect(toolbar).toBeVisible();
  await inaccessible.getByRole('button', { name: 'Dismiss' }).click();
  await page.evaluate(() => { window.open = window.savedOpen; });

  const [detached] = await Promise.all([context.waitForEvent('page'), page.getByRole('button', { name: 'Detach browser panel' }).click()]);
  await expect(detached.getByRole('toolbar', { name: 'Agent browser' }).getByRole('status')).toHaveText('Claude is browsing');
  await detached.getByRole('button', { name: 'Put back in main window' }).click();
  await expect(toolbar).toBeVisible();

  // ── Live frames: drawn on the canvas, each one counted.
  const canvas = panel.locator('canvas.browser-canvas');
  await expect(canvas).toHaveAttribute('data-frames', '1');
  const painted = () => canvas.evaluate(node => { const c = node.getContext('2d'); const d = c.getImageData(Math.floor(node.width / 2), Math.floor(node.height / 3), 1, 1).data; return d[3] > 0; });
  await expect.poll(painted).toBe(true);
  // A pixel in the header band: the docs page is blue, the news page green.
  const band = () => canvas.evaluate(node => { const c = node.getContext('2d'), r = node.getBoundingClientRect(); const scale = node.width / r.width; const s = Math.min(r.width / 1280, r.height / 800), top = (r.height - 800 * s) / 2; const d = c.getImageData(Math.floor(20 * scale), Math.floor((top + 10 * s) * scale), 1, 1).data; return d[1] > d[2] ? 'green' : 'blue'; });
  await expect.poll(band).toBe('blue');
  fake.frame(id, news);
  await expect(canvas).toHaveAttribute('data-frames', '2');
  await expect.poll(band).toBe('green');
  fake.frame(id, docs);
  await expect.poll(band).toBe('blue');

  // ── Steps: a click runs, with the cursor and the highlight where it acts, then is done.
  const log = panel.getByRole('region', { name: 'Browser steps' });
  const step = text => log.locator('li').filter({ hasText: text });
  fake.act(id, { seq: 100, kind: 'click', status: 'running', summary: 'Clicking button “Sign in”', box: { x: 1090, y: 12, width: 110, height: 40 } });
  await expect(step('Clicking button “Sign in”')).toHaveAttribute('data-status', 'running');
  await expect(step('Clicking button “Sign in”').locator('.browser-spinner')).toBeVisible();
  const highlight = panel.locator('.browser-highlight'), cursor = panel.locator('.browser-cursor');
  await expect(highlight).toBeVisible(); await expect(cursor).toBeVisible();
  // The highlight sits over the button's place on the drawn page.
  const view = await panel.locator('.browser-view').boundingBox(), mark = await highlight.boundingBox();
  const scale = Math.min(view.width / 1280, view.height / 800), left = view.x + (view.width - 1280 * scale) / 2;
  assert.ok(Math.abs(mark.x - (left + 1090 * scale)) < 6, `highlight x ${mark.x} vs ${left + 1090 * scale}`);
  await page.screenshot({ path: '.cache/agent-browser-acting.png' });
  fake.act(id, { seq: 100, kind: 'click', status: 'done', summary: 'Clicked button “Sign in”', box: { x: 1090, y: 12, width: 110, height: 40 } });
  await expect(step('Clicked button “Sign in”')).toHaveAttribute('data-status', 'done');
  await expect(log.locator('li')).toHaveCount(2);
  fake.act(id, { kind: 'blocked', status: 'blocked', actor: 'lazybox', summary: 'Refused 127.0.0.1:4100 — Lazybox’s own service' });
  fake.act(id, { kind: 'type', status: 'failed', summary: 'Type into e9', error: 'Agents never type into password fields' });
  fake.act(id, { kind: 'popup', summary: 'Folded a popup to accounts.example.com into the page' });
  await expect(step('Refused 127.0.0.1:4100').locator('.browser-step-tag')).toHaveText('Refused');
  await expect(step('Type into e9')).toHaveAttribute('data-status', 'failed');
  await expect(log.getByRole('button', { name: /Steps/ })).toContainText('5 · 2 refused or failed');
  // A step with a place flashes it again when picked.
  await step('Clicked button “Sign in”').getByRole('button').click();
  await expect(highlight).toBeVisible();

  // ── Closed by the person, the panel stays closed while the same browsing goes on; the chip brings it back.
  await tab.hover();
  await tab.locator('.dock-tab-close').click();
  await expect(panel).toHaveCount(0);
  fake.act(id, { kind: 'scroll', summary: 'Scrolled down' });
  fake.setState(id, { title: 'Guide — Example docs (2)' });
  await page.waitForTimeout(1800);
  await expect(panel).toHaveCount(0);
  await chip.click();
  await expect(panel).toBeVisible();
  // Reopened, it shows the page at once: the last frame comes with the stream's first event.
  await expect.poll(painted).toBe(true);
  await expect(log.locator('li')).toHaveCount(6);

  // ── A new site asks, even in full access: Allow for this conversation (c).
  // (A card takes focus only from the chat, never from what the person just used.)
  await chat.locator('.messages:visible').first().click({ position: { x: 5, y: 5 } });
  const card = chat.locator('.request.browser-request-card');
  fake.request(id, { id: 'site-1', kind: 'site', url: 'https://news.example.org/releases/2.4', origin: 'https://news.example.org', reason: 'The release notes for 2.4 are published there.', permission: 'yolo' });
  await expect(card.getByRole('heading')).toHaveText('Claude wants to open news.example.org');
  await expect(card.locator('.browser-request-url')).toHaveText('https://news.example.org/releases/2.4');
  await expect(card.locator('.repository-reason')).toHaveText('WhyThe release notes for 2.4 are published there.');
  await expect(card).toContainText('Full access still asks before a new site.');
  await expect(card.locator('.decision-row')).toHaveText([/Allow once/, /Allow for this conversation/, /Always allow in this workspace/, /Deny…/]);
  await expect(card).toBeFocused();
  await expect(chat.locator('.chat-status [role=status]')).toHaveText('Waiting for you');
  await expect(chip).toHaveText('Waiting for you');
  await page.screenshot({ path: '.cache/agent-browser-site-card.png' });
  // The Inbox lists it as needing the person.
  await page.locator('.inbox-action').click();
  // The Inbox is the docked Agents panel (audit R1): the row says what is asked.
  const row = page.locator('.panel-agents li[data-state="needs-input"]').filter({ hasText: 'Research the release notes' });
  await expect(row.locator('.agent-meta')).toContainText('Browser');
  await expect(row.locator('.agent-meta')).toContainText('Open news.example.org');
  await card.getByRole('button', { name: 'Allow once' }).focus();
  await page.keyboard.press('c');
  await expect.poll(() => fake.decisions.at(-1)).toEqual({ requestId: 'site-1', decision: 'conversation', reason: '' });
  await expect(card).toHaveCount(0);

  // ── y, a, and n with a reason.
  for (const [request, key, decision] of [['site-2', 'y', 'once'], ['site-3', 'a', 'workspace']]) {
    fake.request(id, { id: request, kind: 'site', url: `https://${request}.example.org/`, origin: `https://${request}.example.org` });
    await expect(card.getByRole('heading')).toHaveText(`Claude wants to open ${request}.example.org`);
    await expect(card).toBeFocused();
    await page.keyboard.press(key);
    await expect.poll(() => fake.decisions.at(-1)).toEqual({ requestId: request, decision, reason: '' });
    await expect(card).toHaveCount(0);
  }
  fake.request(id, { id: 'site-4', kind: 'site', url: 'http://tracker.example.net/', origin: 'http://tracker.example.net' });
  await expect(card.getByRole('heading')).toHaveText('Claude wants to open tracker.example.net');
  await expect(card.locator('.request-tool')).toHaveText('http — not secure');
  await card.getByRole('button', { name: 'Allow once' }).focus();
  await page.keyboard.press('n');
  const reason = card.getByRole('textbox');
  await expect(reason).toBeFocused();
  await reason.fill('Use the changelog in the repository instead');
  await reason.press('Enter');
  await expect.poll(() => fake.decisions.at(-1)).toEqual({ requestId: 'site-4', decision: 'deny', reason: 'Use the changelog in the repository instead' });

  // ── A download: allow it once.
  fake.request(id, { id: 'download-1', kind: 'download', url: 'https://docs.example.com/files/report.pdf', origin: 'https://docs.example.com', fileName: 'report.pdf' });
  await expect(card.getByRole('heading')).toHaveText('Claude wants to download report.pdf');
  await expect(card.locator('.decision-row')).toHaveText([/Allow this download/, /Deny…/]);
  await page.screenshot({ path: '.cache/agent-browser-download-card.png' });
  await card.getByRole('button', { name: 'Allow this download' }).click();
  await expect.poll(() => fake.decisions.at(-1)).toEqual({ requestId: 'download-1', decision: 'once', reason: '' });
  await expect(chip).toHaveText('Claude is browsing');

  // ── Take over: Claude pauses; your clicks and keys reach the page in its own pixels.
  await toolbar.getByRole('button', { name: 'Take over' }).click();
  await expect.poll(() => fake.controls.at(-1)?.action).toBe('take_over');
  await expect(toolbar.getByRole('status')).toHaveText('You’re browsing');
  await expect(chip).toHaveText('You’re browsing');
  const banner = panel.locator('.browser-banner');
  await expect(banner).toContainText('You have the browser — Claude is paused until you give it back (G).');
  await expect(canvas).toBeFocused();
  const address = panel.getByRole('textbox', { name: 'Address' });
  await expect(address).toHaveValue('https://docs.example.com/guide');
  await page.screenshot({ path: '.cache/agent-browser-take-over.png' });
  const box = await canvas.boundingBox();
  const s2 = Math.min(box.width / 1280, box.height / 800), x0 = box.x + (box.width - 1280 * s2) / 2, y0 = box.y + (box.height - 800 * s2) / 2;
  await page.mouse.click(x0 + 300 * s2, y0 + 200 * s2);
  await expect.poll(() => fake.inputs.filter(i => i.type === 'mouse_down' || i.type === 'mouse_up').length).toBe(2);
  const down = fake.inputs.find(i => i.type === 'mouse_down');
  assert.ok(Math.abs(down.x - 300) <= 2 && Math.abs(down.y - 200) <= 2, `click at ${down.x},${down.y}`);
  assert.equal(down.button, 'left'); assert.equal(down.clickCount, 1);
  await page.keyboard.type('hi');
  await page.keyboard.press('Enter');
  await expect.poll(() => fake.inputs.filter(i => i.type !== 'mouse_move' && i.type !== 'mouse_down' && i.type !== 'mouse_up').map(i => `${i.type}:${i.key}`)).toEqual(['key_down:h', 'key_up:h', 'key_down:i', 'key_up:i', 'key_down:Enter', 'key_up:Enter']);
  // Keys go to the page, not to Lazybox: g typed into the page is text, not Give back.
  await page.keyboard.type('g');
  await expect.poll(() => fake.inputs.at(-1)).toMatchObject({ type: 'key_up', key: 'g' });
  assert.equal(fake.controls.at(-1).action, 'take_over');
  // Your own address bar.
  await address.fill('example.net/pricing');
  await address.press('Enter');
  await expect.poll(() => fake.controls.at(-1)).toMatchObject({ action: 'navigate', url: 'https://example.net/pricing' });
  await expect(step('You went to example.net')).toHaveAttribute('data-actor', 'person');
  // Shift+Escape leaves the page; G then gives the browser back.
  await canvas.focus();
  await page.keyboard.press('Shift+Escape');
  await expect(canvas).not.toBeFocused();
  await panel.locator('.browser-log-toggle').focus();
  await page.keyboard.press('g');
  await expect.poll(() => fake.controls.at(-1)?.action).toBe('give_back');
  await expect(banner).toHaveCount(0);
  await expect(toolbar.getByRole('status')).toHaveText('Claude is browsing');

  // ── Browse freely, from the panel's menu.
  await toolbar.getByLabel('Agent browser actions', { exact: true }).click();
  await page.getByRole('group', { name: 'Agent browser actions menu' }).getByRole('menuitemcheckbox', { name: 'Browse freely in this conversation' }).click();
  await expect.poll(() => fake.controls.at(-1)?.action).toBe('browse_freely');
  await expect(panel.locator('.browser-note[role=note]')).toContainText('Claude opens any site in this conversation without asking.');
  await panel.locator('.browser-note[role=note]').getByRole('button', { name: 'Ask again' }).click();
  await expect.poll(() => fake.controls.at(-1)?.action).toBe('ask_sites');

  // ── Stop, then let Claude browse again.
  await toolbar.getByRole('button', { name: 'Stop' }).click();
  await expect.poll(() => fake.controls.at(-1)?.action).toBe('stop');
  await expect(toolbar.getByRole('status')).toHaveText('Browser stopped');
  await expect(chip).toHaveCount(0);
  const again = panel.getByRole('button', { name: 'Let Claude browse again' });
  await expect(again).toBeVisible();
  await page.screenshot({ path: '.cache/agent-browser-stopped.png' });
  await again.click();
  await expect.poll(() => fake.controls.at(-1)?.action).toBe('resume');
  await expect(again).toHaveCount(0);

  // ── Give Claude the browser: its agent started before the browser was there (the ⋯ menu).
  const sessionMenu = chat.getByLabel('Session actions', { exact: true });
  const give = page.getByRole('button', { name: 'Give Claude the browser' });
  await sessionMenu.click();
  await expect(page.getByRole('button', { name: 'Switch subscription…' })).toBeVisible();
  await expect(give).toHaveCount(0);
  await page.keyboard.press('Escape');
  Object.assign(session, { browserAccess: 'can_give' });
  // The chat reads the conversation again on its own.
  await expect.poll(async () => { await sessionMenu.click(); const shown = await give.count(); if (!shown) await page.keyboard.press('Escape'); return shown; }, { timeout: 10000 }).toBe(1);
  await give.click();
  await expect.poll(() => fixture.mutations.filter(m => m.method === 'GiveBrowser').map(m => m.body.sessionId)).toEqual([id]);
  await sessionMenu.click();
  await expect(page.getByRole('button', { name: 'Switch subscription…' })).toBeVisible();
  await expect(give).toHaveCount(0);
  await page.keyboard.press('Escape');

  // ── Preview: a server the agent runs with `lazybox run` opens in the Web panel, no workspace action needed.
  const dev = fixture.streamCommand(id, 'npm run dev', ['npm', 'run', 'dev']);
  dev.ports = [5173];
  dev.write('  VITE v5.4.0  ready in 1203 ms\n  ➜  Local:   http://localhost:5173/\n');
  const liveBar = chat.locator('section.live-bar');
  const preview = liveBar.getByRole('button', { name: 'Preview :5173' });
  await expect(preview).toBeVisible();
  await preview.click();
  const web = page.getByRole('region', { name: 'Web page' }).first();
  await expect(web.getByRole('textbox', { name: 'Address', exact: true })).toHaveValue('http://localhost:5173/');
  // The command's console offers it too.
  await liveBar.getByRole('button', { name: 'Open in console' }).click();
  await expect(page.locator('.console-preview').first()).toHaveText('Preview :5173');
  await page.screenshot({ path: '.cache/agent-browser-preview.png' });
  dev.finish(0);
  await expect(preview).toHaveCount(0);

  // ── Settings → Browser.
  await page.getByRole('navigation', { name: 'Pages' }).getByRole('button', { name: 'Settings' }).click();
  await page.getByRole('tab', { name: 'Browser' }).click();
  const settings = page.getByRole('tabpanel', { name: 'Browser' });
  await expect(settings.getByRole('radio', { name: /Ask before each new site/ })).toBeChecked();
  const saves = () => fixture.mutations.filter(m => m.method === 'SaveBrowserSettings').map(m => m.body);
  await settings.getByRole('radio', { name: /Allow any site/ }).check();
  await expect.poll(() => saves().at(-1)?.defaultPermission).toBe('allow');
  const site = settings.getByRole('textbox', { name: 'Site to allow' });
  await site.fill('not a site'); await site.press('Enter');
  await expect(settings.getByRole('alert')).toContainText('is not a site name');
  await site.fill('https://Docs.Rust-lang.org/std'); await site.press('Enter');
  await expect.poll(() => saves().at(-1)?.allowedSites).toEqual(['docs.python.org', 'docs.rust-lang.org']);
  await expect(site).toHaveValue('');
  await settings.getByRole('button', { name: 'Remove docs.python.org' }).click();
  await expect.poll(() => saves().at(-1)?.allowedSites).toEqual(['docs.rust-lang.org']);
  await settings.getByRole('button', { name: 'Remove github.com from Alpha feature' }).click();
  await expect.poll(() => saves().at(-1)?.workspaces?.find(w => w.collectionId === 'alpha')?.sites ?? []).toEqual([]);
  assert.ok(saves().at(-1).workspaces.some(w => w.collectionId === 'alpha'), 'the workspace is saved with no sites');
  await settings.getByRole('switch', { name: /Show the agent browser beside the chat/ }).uncheck();
  await expect.poll(() => fake.settings.showPanel).toBe(false);
  await settings.getByLabel('Downloads', { exact: true }).selectOption('block');
  await expect.poll(() => saves().at(-1)?.downloads).toBe('block');
  await expect(settings.getByRole('article', { name: 'Alpha feature' })).toContainText('Browser data · 3.4 MB');
  await page.screenshot({ path: '.cache/agent-browser-settings.png' });
  await settings.getByRole('article', { name: 'Alpha feature' }).getByRole('button', { name: 'Clear browser data…' }).click();
  const dialog = page.getByRole('dialog', { name: 'Clear browser data of Alpha feature' });
  await expect(dialog).toContainText('sign-ins, cookies, site storage and cache');
  await dialog.getByRole('button', { name: 'Clear browser data' }).click();
  await expect(dialog).toHaveCount(0);
  await expect.poll(() => fixture.mutations.filter(m => m.method === 'ClearBrowserData').map(m => m.body.collectionId)).toEqual(['alpha']);
  await expect(settings.getByRole('article', { name: 'Alpha feature' })).toContainText('No browser data');
  // A save over another window's change says so and reloads.
  fake.settings.revision = '99';
  await settings.getByRole('radio', { name: /Ask before each new site/ }).check();
  await expect(settings.getByRole('alert')).toContainText('changed in another window');
  // Off for one workspace: its agents get no browser; saved like any change.
  fake.setChromium(false);
  await settings.getByRole('switch', { name: 'Agents in Beta release get the browser' }).uncheck();
  await expect.poll(() => saves().at(-1)?.workspaces?.find(w => w.collectionId === 'beta')?.off).toBe(true);
  assert.ok(!saves().at(-1).workspaces.find(w => w.collectionId === 'alpha').off, 'only that workspace');
  // Chromium missing: why, and one click installs it, its output shown here.
  const missing = settings.locator('.browser-availability[data-state=not_installed]');
  await expect(missing).toContainText('Chromium isn’t installed');
  await missing.getByRole('button', { name: 'Install Chromium' }).click();
  await expect.poll(() => fixture.mutations.filter(m => m.method === 'InstallBrowser').length).toBe(1);
  await expect(missing.getByRole('button', { name: 'Installing…' })).toBeDisabled();
  fake.install.write('Downloading Chrome Headless Shell 156.0.8078.4 …\n');
  await expect(settings.getByLabel('Chromium install output').getByTestId('console-output')).toContainText('Downloading Chrome Headless Shell');
  await page.screenshot({ path: '.cache/agent-browser-install.png' });
  fake.setChromium(true);
  fake.install.finish(0);
  await expect(missing).toHaveCount(0);
  await expect(settings.locator('.browser-settings-status')).toContainText('Chromium installed');
  // Off everywhere: said, with Turn on.
  await settings.getByRole('radio', { name: /^Off/ }).check();
  const off = settings.locator('.browser-availability[data-state=off]');
  await expect(off).toContainText('The agent browser is off');
  await off.getByRole('button', { name: 'Turn on' }).click();
  await expect.poll(() => saves().at(-1)?.defaultPermission).toBe('ask');
  await expect(off).toHaveCount(0);
  await page.getByRole('navigation', { name: 'Pages' }).getByRole('button', { name: 'Work' }).click();

  // ── A phone's width: one panel at a time, nothing wider than the screen.
  fake.setState(id, { status: 'open', controller: 'agent', url: 'https://docs.example.com/guide', title: 'Guide — Example docs' });
  fake.frame(id, docs);
  await page.setViewportSize({ width: 420, height: 860 });
  await page.waitForTimeout(400);
  const overflow = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
  assert.ok(overflow <= 0, `the page scrolls sideways by ${overflow}px`);
  await page.screenshot({ path: '.cache/agent-browser-narrow.png' });

  await drainPageErrors(errors);assert.deepEqual(errors, []);
  console.log('agent browser: chip, panel, frames, steps, cards, take over, stop, give the browser, preview and settings (install, off, per workspace) verified');
} finally {
  await browser?.close();
  await server.close();
  await fixture.close();
}
