// The Web panel in the browser build, against the fake Runner: a link in a
// chat message opens in the browser by default and in a Web panel with ⌥
// (Alt) or from its right-click menu; the panel's address bar takes an
// address or a search, with back, forward, zoom, copy and open in browser;
// the panel's tab shows the page; a page that does not load says it may not
// be shown here and offers the browser; Settings → Web & links swaps the
// default; a page moves to its own window and back; the history survives a
// reload. Chromium; TEST_WEBKIT=1 runs WebKit.
import { createServer } from './fixtures/vite-cache.mjs';
import react from '@vitejs/plugin-react';
import { chromium, webkit, expect } from './fixtures/playwright.mjs';
import { fileURLToPath } from 'node:url';
import { mkdir } from 'node:fs/promises';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';
import { testViteCache } from './fixtures/vite-cache.mjs';
import { collectPageErrors, drainPageErrors } from './fixtures/page-errors.mjs';

const fixture = await startRunnerFixture();
const site = `http://127.0.0.1:${fixture.previewPort}`;
// The chat links to the fixture's little site: one link in a sentence.
fixture.openEvents('alpha-session').push({ type: 'message_delta', text: `The docs are on [the fixture site](${site}/about).` });
const server = await createServer({ ...testViteCache(import.meta.url), configFile: false, logLevel: 'error', root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: fixture.url } } } });
// The workspace with native web views, whose host records where it would draw them.
const nativeServer = await createServer({ ...testViteCache(import.meta.url), configFile: false, logLevel: 'error', root: fileURLToPath(new URL('./fixtures/web-view-host', import.meta.url)), publicDir: fileURLToPath(new URL('../apps/web/public', import.meta.url)), plugins: [react()], server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: fixture.url } } } });
// The dock tab of the nth Web panel.
const webTabIn = (page, n) => page.locator('.dv-tab').filter({ has: page.locator('[data-kind="web"]') }).nth(n);
const engine = process.env.TEST_WEBKIT ? webkit : chromium;
const alt = '⌥';
let browser;
try {
  await server.listen(); browser = await engine.launch({ headless: true });
  const context = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  if (!process.env.TEST_WEBKIT) await context.grantPermissions(['clipboard-read', 'clipboard-write']);
  // Searches never leave the machine: the engine's page is a fixture.
  await context.route('https://duckduckgo.com/**', route => route.fulfill({ contentType: 'text/html', body: '<title>Search</title><h1>Search fixture</h1>' }));
  const page = await context.newPage();
  const errors = []; await collectPageErrors(page, errors);
  const app = `http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`;
  await page.goto(app);
  await mkdir('.cache', { recursive: true });
  const link = page.locator('.panel-chat').getByRole('link', { name: 'the fixture site' });
  await expect(link).toBeVisible();

  // A plain click opens the browser, as before; hovering says what ⌥-click does.
  const [tab] = await Promise.all([context.waitForEvent('page'), link.click()]);
  await expect.poll(() => tab.url()).toBe(`${site}/about`);
  await tab.close();
  await link.hover();
  await expect(link).toHaveAttribute('title', new RegExp(`${alt}-click to open in Lazybox`));
  await expect(page.getByRole('region', { name: 'Web page' })).toHaveCount(0);

  // ⌥-click opens it in a Web panel beside the conversation.
  await link.click({ modifiers: ['Alt'] });
  const viewer = page.getByRole('region', { name: 'Web page' }).first();
  await expect(viewer).toBeVisible();
  const frame = page.locator('.panel-web').first().frameLocator('iframe');
  await expect(frame.getByRole('heading', { name: 'About the fixture app' })).toBeVisible();
  const address = viewer.getByRole('textbox', { name: 'Address', exact: true });
  await expect(address).toHaveValue(`${site}/about`);
  // The origin is on screen, and the panel's tab names the page.
  await expect(viewer.locator('.web-origin')).toHaveText(`127.0.0.1:${fixture.previewPort}`);
  // The dock tab of a Web panel (its title is the page's).
  const webTab = n => page.locator('.dv-tab').filter({ has: page.locator('[data-kind="web"]') }).nth(n);
  const tabTitle = webTab(0).locator('.dock-tab-title');
  await expect(tabTitle).toHaveText(`127.0.0.1:${fixture.previewPort}`);
  await expect(viewer.getByRole('button', { name: 'Back', exact: true })).toBeDisabled();
  await page.screenshot({ path: '.cache/web-viewer-panel.png' });

  // The address bar: an address without its scheme, then history back and forward.
  await address.fill(`127.0.0.1:${fixture.previewPort}/`);
  await address.press('Enter');
  await expect(frame.getByRole('heading', { name: 'Fixture app home' })).toBeVisible();
  await expect(address).toHaveValue(`${site}/`);
  await viewer.getByRole('button', { name: 'Back', exact: true }).click();
  await expect(frame.getByRole('heading', { name: 'About the fixture app' })).toBeVisible();
  await expect(address).toHaveValue(`${site}/about`);
  await viewer.getByRole('button', { name: 'Forward', exact: true }).click();
  await expect(frame.getByRole('heading', { name: 'Fixture app home' })).toBeVisible();
  await expect(viewer.getByRole('button', { name: 'Forward', exact: true })).toBeDisabled();
  // Anything but a web page is refused.
  await address.fill('javascript:alert(1)'); await address.press('Enter');
  await expect(viewer.getByRole('alert')).toContainText('not javascript: links');
  await viewer.getByRole('alert').getByRole('button', { name: 'Dismiss' }).click();

  // Zoom, from the panel's menu; the badge resets it.
  await viewer.getByLabel('Web page actions', { exact: true }).click();
  await page.getByRole('group', { name: 'Web page actions menu' }).getByRole('button', { name: 'Zoom in' }).click();
  await expect(viewer.getByRole('button', { name: 'Zoom 110%. Reset to 100%' })).toBeVisible();
  await viewer.getByRole('button', { name: 'Zoom 110%. Reset to 100%' }).click();
  await expect(viewer.getByRole('button', { name: /^Zoom \d+%/ })).toHaveCount(0);
  // Copy the address.
  if (!process.env.TEST_WEBKIT) {
    await viewer.getByLabel('Web page actions', { exact: true }).click();
    await page.getByRole('group', { name: 'Web page actions menu' }).getByRole('button', { name: 'Copy address' }).click();
    await expect(viewer.getByRole('status')).toHaveText('Address copied');
    assert.equal(await page.evaluate(() => navigator.clipboard.readText()), `${site}/`);
  }
  // Open in browser.
  const [opened] = await Promise.all([context.waitForEvent('page'), viewer.getByRole('button', { name: 'Open in browser', exact: true }).click()]);
  await expect.poll(() => opened.url()).toBe(`${site}/`);
  await opened.close();

  // Words are a search, with the engine chosen in Settings → Web & links.
  await page.getByRole('navigation', { name: 'Pages' }).getByRole('button', { name: 'Settings' }).click();
  await page.getByRole('tab', { name: 'Web & links' }).click();
  const settings = page.getByRole('tabpanel', { name: 'Web & links' });
  await expect(settings.getByRole('radio', { name: /Your browser/ })).toBeChecked();
  await settings.getByLabel('Search with').selectOption('duckduckgo');
  await page.screenshot({ path: '.cache/web-viewer-settings.png' });
  await page.getByRole('navigation', { name: 'Pages' }).getByRole('button', { name: 'Work' }).click();
  // The panel came back with its page.
  await expect(address).toHaveValue(`${site}/`);
  await address.fill('lazybox web viewer'); await address.press('Enter');
  await expect(address).toHaveValue('https://duckduckgo.com/?q=lazybox%20web%20viewer');
  await expect(frame.getByRole('heading', { name: 'Search fixture' })).toBeVisible();
  await expect(tabTitle).toHaveText('duckduckgo.com');
  await expect(viewer.locator('.web-origin')).toHaveAttribute('title', /Secure connection/);
  // A page from the web may refuse to be framed, which a browser does not tell the page: the panel says what a blank page means.
  const hint = viewer.getByText('Blank page? Some sites refuse to be shown inside other apps.');
  await expect(hint).toBeVisible();
  await page.screenshot({ path: '.cache/web-viewer-search.png' });
  await viewer.locator('.web-framing-hint').getByRole('button', { name: 'Dismiss' }).click();
  await expect(hint).toHaveCount(0);

  // A page that never loads: after a while, the way out. A page that did load
  // meanwhile, in a second panel, stays on screen.
  await page.locator('summary[aria-label="Panels"]').click();
  await page.getByRole('button', { name: 'New Web panel', exact: true }).click();
  const loadedPanel = page.locator('.panel-web').nth(1);
  await loadedPanel.getByRole('textbox', { name: 'Address' }).fill(`${site}/`);
  await loadedPanel.getByRole('textbox', { name: 'Address' }).press('Enter');
  await expect(loadedPanel.frameLocator('iframe').getByRole('heading', { name: 'Fixture app home' })).toBeVisible();
  await address.fill(`${site}/stall`); await address.press('Enter');
  const refused = viewer.getByRole('status').filter({ hasText: 'This page isn’t loading here' });
  await expect(refused).toBeVisible({ timeout: 20_000 });
  await expect(refused).toContainText('Some sites refuse to be shown inside other apps');
  await expect(loadedPanel.frameLocator('iframe').getByRole('heading', { name: 'Fixture app home' })).toBeVisible();
  await expect(loadedPanel.getByText('This page isn’t loading here')).toHaveCount(0);
  await page.screenshot({ path: '.cache/web-viewer-stalled.png' });
  // A browser tab on a page that never answers never reports a page: watch window.open instead.
  await page.evaluate(() => { window.openedUrls = []; window.realOpen = window.open; window.open = url => { window.openedUrls.push(String(url)); return null; }; });
  await refused.getByRole('button', { name: 'Open in browser' }).click();
  assert.deepEqual(await page.evaluate(() => { window.open = window.realOpen; return window.openedUrls; }), [`${site}/stall`]);
  await viewer.getByRole('button', { name: 'Back', exact: true }).click();
  await expect(frame.getByRole('heading', { name: 'Search fixture' })).toBeVisible();
  await webTab(1).locator('.dock-tab-close').click();
  await expect(page.getByRole('region', { name: 'Web page' })).toHaveCount(1);

  // Right-click on a link: every choice. A new panel opens beside the first.
  await link.click({ button: 'right' });
  const menu = page.getByRole('menu', { name: `Link to 127.0.0.1:${fixture.previewPort}` });
  await expect(menu.getByRole('menuitem', { name: /Open in Lazybox/ })).toBeVisible();
  await expect(menu.getByRole('menuitem', { name: /Open in your browser/ })).toBeVisible();
  await page.screenshot({ path: '.cache/web-viewer-link-menu.png' });
  await menu.getByRole('menuitem', { name: 'Open in a new Lazybox panel' }).click();
  await expect(page.getByRole('region', { name: 'Web page' })).toHaveCount(2);
  const second = page.locator('.panel-web').nth(1);
  await expect(second.frameLocator('iframe').getByRole('heading', { name: 'About the fixture app' })).toBeVisible();
  await webTab(1).locator('.dock-tab-close').click();
  await expect(page.getByRole('region', { name: 'Web page' })).toHaveCount(1);

  // The page menu adds an empty Web panel, ready to type in, with the workspace's running app a click away.
  const run = page.getByRole('group', { name: 'Run', exact: true });
  await run.getByRole('button', { name: 'Run dev', exact: true }).click();
  await expect(run.getByRole('button', { name: /^dev running on port/ })).toBeVisible();
  await page.locator('summary[aria-label="Panels"]').click();
  await page.getByRole('button', { name: 'New Web panel', exact: true }).click();
  await expect(page.getByRole('region', { name: 'Web page' })).toHaveCount(2);
  const fresh = page.locator('.panel-web').nth(1);
  await expect(fresh.getByRole('heading', { name: 'Open a web page' })).toBeVisible();
  await expect(fresh.getByRole('textbox', { name: 'Address' })).toBeFocused();
  await expect(fresh.getByText('Running in this workspace:')).toBeVisible();
  await page.screenshot({ path: '.cache/web-viewer-start.png' });
  await fresh.getByRole('button', { name: /dev/ }).click();
  await expect(fresh.frameLocator('iframe').getByRole('heading', { name: 'Fixture app home' })).toBeVisible();
  await webTab(1).locator('.dock-tab-close').click();

  // Settings: links open in Lazybox; ⌥-click is now the browser.
  await page.getByRole('navigation', { name: 'Pages' }).getByRole('button', { name: 'Settings' }).click();
  await page.getByRole('tab', { name: 'Web & links' }).click();
  await settings.getByRole('radio', { name: /A Lazybox Web panel/ }).check();
  await page.getByRole('navigation', { name: 'Pages' }).getByRole('button', { name: 'Work' }).click();
  await link.click();
  await expect(address).toHaveValue(`${site}/about`);
  const [altTab] = await Promise.all([context.waitForEvent('page'), link.click({ modifiers: ['Alt'] })]);
  await expect.poll(() => altTab.url()).toBe(`${site}/about`);
  await altTab.close();

  // The history is kept with the panel across a reload.
  await page.reload();
  await expect(page.getByRole('region', { name: 'Web page' }).getByRole('textbox', { name: 'Address', exact: true })).toHaveValue(`${site}/about`);
  await page.getByRole('region', { name: 'Web page' }).getByRole('button', { name: 'Back', exact: true }).click();
  // The search before it (the stalled page after it was dropped by opening a new one from there).
  await expect(page.getByRole('region', { name: 'Web page' }).getByRole('textbox', { name: 'Address', exact: true })).toHaveValue('https://duckduckgo.com/?q=lazybox%20web%20viewer');

  // A page moves to a window of its own, and back to the workspace.
  await page.getByRole('region', { name: 'Web page' }).getByLabel('Web page actions', { exact: true }).click();
  const [windowed] = await Promise.all([context.waitForEvent('page'), page.getByRole('group', { name: 'Web page actions menu' }).getByRole('button', { name: 'Move to a new window' }).click()]);
  await expect(page.getByRole('region', { name: 'Web page' })).toHaveCount(0);
  await expect(windowed.getByRole('textbox', { name: 'Address', exact: true })).toHaveValue('https://duckduckgo.com/?q=lazybox%20web%20viewer');
  await windowed.getByRole('textbox', { name: 'Address', exact: true }).fill(`${site}/about`);
  await windowed.getByRole('textbox', { name: 'Address', exact: true }).press('Enter');
  await expect(windowed.frameLocator('iframe').getByRole('heading', { name: 'About the fixture app' })).toBeVisible();
  await windowed.screenshot({ path: '.cache/web-viewer-window.png' });
  await windowed.getByLabel('Web page actions', { exact: true }).click();
  await Promise.all([windowed.waitForEvent('close'), windowed.getByRole('group', { name: 'Web page actions menu' }).getByRole('button', { name: 'Move back to the workspace' }).click()]);
  await expect(page.getByRole('region', { name: 'Web page' }).getByRole('textbox', { name: 'Address', exact: true })).toHaveValue(`${site}/about`);

  // A phone: the panel fits, nothing scrolls sideways.
  await page.setViewportSize({ width: 390, height: 844 });
  await page.getByRole('navigation', { name: 'Workspace panels' }).getByRole('button', { name: 'Web', exact: true }).click();
  await expect(page.getByRole('region', { name: 'Web page' }).getByRole('textbox', { name: 'Address', exact: true })).toBeVisible();
  assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false, 'no horizontal overflow on a phone');
  await page.screenshot({ path: '.cache/web-viewer-phone.png' });

  // Native web views (the desktop app), on a Retina display: the host records
  // where it is asked to draw each page. The frame is exactly the page area
  // under the address bar, in every layout, and the view steps aside for
  // anything of the app's own over it.
  await nativeServer.listen();
  const native = await browser.newContext({ viewport: { width: 1440, height: 900 }, deviceScaleFactor: 2 });
  const host = await native.newPage();
  await collectPageErrors(host, errors);
  await host.goto(`http://127.0.0.1:${nativeServer.httpServer.address().port}/?collection=alpha&session=alpha-session#token=${fixture.token}`);
  await host.locator('.panel-chat').getByRole('link', { name: 'the fixture site' }).click({ modifiers: ['Alt'] });
  const nativeViewer = host.getByRole('region', { name: 'Web page' }).first();
  await expect(nativeViewer.locator('.web-surface')).toBeVisible();
  await expect(nativeViewer.locator('iframe')).toHaveCount(0);
  // Every page area on screen has a view shown exactly over it (and no other view is shown).
  const placed = () => host.evaluate(() => {
    const near = (a, b) => Math.abs(a - b) < 0.01;
    const views = Object.values(window.nativeWebViews).filter(view => view.shown);
    const surfaces = [...document.querySelectorAll('.panel-web .web-surface')].filter(surface => surface.checkVisibility());
    if (!surfaces.length || views.length !== surfaces.length) return `${views.length} views shown for ${surfaces.length} pages`;
    const results = surfaces.map(surface => {
      const r = surface.getBoundingClientRect(), bar = surface.closest('.web-viewer').querySelector('.web-toolbar').getBoundingClientRect();
      const want = { x: r.left, y: r.top, width: Math.min(r.right, document.documentElement.clientWidth) - r.left, height: Math.min(r.bottom, document.documentElement.clientHeight) - r.top };
      const view = views.find(({ frame: f }) => near(f.x, want.x) && near(f.y, want.y) && near(f.width, want.width) && near(f.height, want.height));
      if (!view) return `no view at ${JSON.stringify(want)}: ${JSON.stringify(views.map(v => v.frame))}`;
      if (!near(view.frame.viewportWidth, innerWidth) || !near(view.frame.viewportHeight, innerHeight)) return `measured in ${view.frame.viewportWidth}×${view.frame.viewportHeight}, not ${innerWidth}×${innerHeight}`;
      return view.frame.y >= bar.bottom - 0.01 ? 'placed' : `over the address bar (${view.frame.y} < ${bar.bottom})`;
    });
    return results.every(result => result === 'placed') ? 'placed' : results.join('; ');
  });
  const hidden = () => host.evaluate(() => Object.values(window.nativeWebViews).every(view => !view.shown));
  await expect.poll(placed).toBe('placed');
  // The sidebar made wider: the panel moves and narrows, the view with it.
  const sidebar = await host.getByRole('separator', { name: 'Resize Workspaces sidebar' }).boundingBox();
  await host.mouse.move(sidebar.x + sidebar.width / 2, sidebar.y + sidebar.height / 2); await host.mouse.down();
  await host.mouse.move(sidebar.x + sidebar.width / 2 + 60, sidebar.y + sidebar.height / 2, { steps: 6 }); await host.mouse.up();
  await expect.poll(placed).toBe('placed');
  // The window restored smaller, then maximized.
  await host.setViewportSize({ width: 1180, height: 740 });
  await expect.poll(placed).toBe('placed');
  await host.setViewportSize({ width: 1680, height: 1010 });
  await expect.poll(placed).toBe('placed');
  // A notice under the address bar pushes the page down.
  const nativeAddress = nativeViewer.getByRole('textbox', { name: 'Address', exact: true });
  await nativeAddress.fill('javascript:alert(1)'); await nativeAddress.press('Enter');
  await expect(nativeViewer.getByRole('alert')).toBeVisible();
  await expect.poll(placed).toBe('placed');
  await nativeViewer.getByRole('alert').getByRole('button', { name: 'Dismiss' }).click();
  await expect.poll(placed).toBe('placed');
  // The interface's text made larger.
  await host.evaluate(() => document.documentElement.style.setProperty('--text-scale', '1.5'));
  await expect.poll(placed).toBe('placed');
  await host.evaluate(() => document.documentElement.style.removeProperty('--text-scale'));
  // A second Web panel beside the first: both pages are placed.
  await host.locator('summary[aria-label="Panels"]').click();
  await host.getByRole('button', { name: 'New Web panel', exact: true }).click();
  const secondNative = host.locator('.panel-web').nth(1);
  await secondNative.getByRole('textbox', { name: 'Address' }).fill(`${site}/`);
  await secondNative.getByRole('textbox', { name: 'Address' }).press('Enter');
  await expect(secondNative.locator('.web-surface')).toBeVisible();
  await expect.poll(placed).toBe('placed');
  await webTabIn(host, 1).locator('.dock-tab-close').click();
  await expect.poll(placed).toBe('placed');
  // Maximized (double-click its tab), then restored.
  const restoredBox = await nativeViewer.locator('.web-surface').boundingBox();
  await webTabIn(host, 0).dblclick();
  await expect(host.getByRole('button', { name: 'Restore layout' }).first()).toBeVisible();
  await expect.poll(async () => (await nativeViewer.locator('.web-surface').boundingBox()).width).toBeGreaterThan(restoredBox.width);
  await expect.poll(placed).toBe('placed');
  await webTabIn(host, 0).dblclick();
  await expect(host.getByRole('button', { name: 'Restore layout' })).toHaveCount(0);
  await expect.poll(placed).toBe('placed');
  // The panel's ⋯ menu, and a context menu over the page: the page steps aside, and comes back.
  await nativeViewer.getByLabel('Web page actions', { exact: true }).click();
  await expect(host.getByRole('group', { name: 'Web page actions menu' })).toBeVisible();
  await expect.poll(hidden).toBe(true);
  await host.keyboard.press('Escape');
  await expect.poll(placed).toBe('placed');
  await webTabIn(host, 0).click({ button: 'right' });
  await expect(host.getByRole('menu').first()).toBeVisible();
  await expect.poll(hidden).toBe(true);
  await host.keyboard.press('Escape');
  await expect(host.getByRole('menu')).toHaveCount(0);
  await expect.poll(placed).toBe('placed');
  // Another page (Settings) hides it; back at work it is where it was.
  await host.getByRole('navigation', { name: 'Pages' }).getByRole('button', { name: 'Settings' }).click();
  await expect.poll(hidden).toBe(true);
  await host.getByRole('navigation', { name: 'Pages' }).getByRole('button', { name: 'Work' }).click();
  await expect.poll(placed).toBe('placed');
  await host.screenshot({ path: '.cache/web-viewer-native.png' });

  // Open in new window: the page moves into the panel's window, measured there, the same page (never
  // created again). Resized, it follows. Put back, it comes home before that window closes, and shows here.
  const pagesOpened = () => host.evaluate(() => Object.values(window.nativeWebViews).map(view => view.opened));
  const openedBefore = await pagesOpened();
  await webTabIn(host, 0).click({ button: 'right' });
  const [popped] = await Promise.all([native.waitForEvent('page'), host.getByRole('menuitem', { name: 'Open in new window' }).click()]);
  await popped.waitForLoadState();
  const windowToken = new URL(popped.url()).searchParams.get('w');
  assert.ok(windowToken, `the panel window names itself: ${popped.url()}`);
  const inWindow = () => Promise.all([host.evaluate(t => Object.values(window.nativeWebViews).filter(view => view.shown && view.popout === t).map(view => view.frame), windowToken),
    popped.evaluate(() => { const r = document.querySelector('.web-surface')?.getBoundingClientRect(); return r && { x: r.left, y: r.top, width: Math.min(r.right, document.documentElement.clientWidth) - r.left, height: Math.min(r.bottom, document.documentElement.clientHeight) - r.top, viewportWidth: innerWidth, viewportHeight: innerHeight }; })])
    .then(([frames, want]) => frames.length === 1 && want && Object.keys(want).every(key => Math.abs(frames[0][key] - want[key]) < 0.01) ? 'placed' : JSON.stringify({ frames, want }));
  await expect.poll(inWindow).toBe('placed');
  assert.deepEqual(await pagesOpened(), openedBefore, 'the page moved, it was not opened again');
  await popped.setViewportSize({ width: 900, height: 700 });
  await expect.poll(inWindow).toBe('placed');
  await popped.screenshot({ path: '.cache/web-viewer-popout.png' });
  await popped.getByRole('button', { name: 'Put back in main window' }).click();
  await expect.poll(() => host.evaluate(t => window.reclaimedPopouts.includes(t), windowToken)).toBe(true);
  await expect.poll(placed).toBe('placed');
  assert.equal(await host.evaluate(() => Object.values(window.nativeWebViews).filter(view => view.popout).length), 0, 'every view is back in the main window');
  assert.deepEqual(await pagesOpened(), openedBefore, 'put back, it is still the same page');
  // A view that went with a window closed before its page came home opens again on its page, never a blank panel.
  await host.evaluate(() => { window.loseNextView = true; });
  await host.setViewportSize({ width: 1500, height: 960 });
  await expect.poll(placed).toBe('placed');
  await expect.poll(() => host.evaluate(() => Object.values(window.nativeWebViews).every(view => view.url && view.opened === 1))).toBe(true);

  await drainPageErrors(errors);assert.deepEqual(errors, []);
  console.log('Web viewer: links routed by click, ⌥-click and menu; address bar, search, history, zoom, copy, fallback, settings, detach and dock, persistence; native views placed over the page area, in a panel window of its own and back, through sidebar, window, notice, text size, split and maximize changes, and hidden under menus and other pages.');
} catch (error) {
  for (const p of browser?.contexts().flatMap(c => c.pages()) || []) await p.screenshot({ path: '.cache/web-viewer-failure.png', fullPage: true }).catch(() => {});
  throw error;
} finally {
  await browser?.close(); await server.close(); await nativeServer.close(); await fixture.close();
}
