// No separate title bar in the desktop app (src-tauri/src/title_bar.rs,
// packages/surfaces/src/title-bar.ts): each window kind's header is its title
// bar, with the macOS window buttons over its leading edge. The native shell
// is simulated (scripts/fixtures/title-bar-window): the host's answers are the
// ones it gives on macOS 26 and what the page asks of it is recorded.
//
// In every window kind (workspace, chat, artifact, web page, a panel popped
// out, the setup window) the header leaves the buttons their room and is tall
// enough to centre them; its empty parts move the window and a double-click
// there asks the host for the system's action; no control in it (buttons,
// tabs, pills, menus, fields) moves the window. In full screen the room goes;
// zoom keeps it the same on screen. A browser's window is unchanged. Screenshots
// in .cache/title-bar-{kind}-{dark,light}.png, the buttons drawn where the host
// puts them. WebKit, the desktop app's engine.
import { testViteCache } from './fixtures/vite-cache.mjs';
import { createServer } from 'vite';
import react from '@vitejs/plugin-react';
import { webkit, expect } from './fixtures/playwright.mjs';
import { fileURLToPath } from 'node:url';
import { mkdir } from 'node:fs/promises';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';
import { monacoVimCompat } from './monaco-vim-compat.mjs';
import { collectPageErrors } from './fixtures/page-errors.mjs';

const f = await startRunnerFixture();
const server = await createServer({ ...testViteCache(import.meta.url), configFile: false, logLevel: 'error',
  root: fileURLToPath(new URL('./fixtures/title-bar-window', import.meta.url)),
  // popout.html, the page a popped-out panel group is drawn into.
  publicDir: fileURLToPath(new URL('../apps/web/public', import.meta.url)),
  plugins: [react(), monacoVimCompat()], server: { host: '127.0.0.1', port: 0 } });

const px = value => Number.parseFloat(value);
/** The header's room, height, and the leftmost of what it shows. */
const measure = header => header.evaluate(node => {
  const style = getComputedStyle(node), box = node.getBoundingClientRect();
  let left = Infinity;
  for (const child of node.querySelectorAll('*')) {
    const rect = child.getBoundingClientRect(), shown = getComputedStyle(child);
    if (rect.width && rect.height && shown.visibility !== 'hidden' && shown.display !== 'none' && rect.right > box.left + 1) left = Math.min(left, rect.left - box.left);
  }
  return { padding: style.paddingLeft, height: box.height, top: box.top, left, drag: node.getAttribute('data-window-drag-region') };
});
/**
 * Every control in `root` (anything a person presses: a pointer cursor, a
 * button, link, field, summary or tab) pressed where it shows: none of them
 * may move the window. The press is a real mousedown at the topmost element
 * under the control's centre, through the page's own handler.
 */
const controlsThatDrag = (page, selector) => page.evaluate(selector => {
  const root = document.querySelector(selector), out = [], checked = [];
  for (const node of root.querySelectorAll('*')) {
    const rect = node.getBoundingClientRect();
    if (!rect.width || !rect.height) continue;
    const style = getComputedStyle(node);
    const control = style.cursor === 'pointer' || /^(BUTTON|A|INPUT|SELECT|TEXTAREA|SUMMARY)$/.test(node.tagName) || ['tab', 'button', 'link'].includes(node.getAttribute('role'));
    if (!control || style.visibility === 'hidden') continue;
    const target = document.elementFromPoint(rect.left + rect.width / 2, rect.top + rect.height / 2);
    if (!target || !node.contains(target)) continue;
    const before = window.titleBarCalls.length;
    target.dispatchEvent(new MouseEvent('mousedown', { bubbles: true, composed: true, cancelable: true, button: 0, detail: 1, clientX: rect.left + rect.width / 2, clientY: rect.top + rect.height / 2 }));
    target.dispatchEvent(new MouseEvent('mouseup', { bubbles: true, composed: true, cancelable: true, button: 0, detail: 1 }));
    checked.push(node.getAttribute('aria-label') || node.textContent.trim().slice(0, 24) || node.className);
    if (window.titleBarCalls.length !== before) out.push(`${node.tagName.toLowerCase()}.${node.className} "${(node.getAttribute('aria-label') || node.textContent).trim().slice(0, 40)}"`);
  }
  window.titleBarCalls.length = 0;
  return { out, checked: checked.length };
}, selector);
/** A point on the header where nothing is: the press lands on the header or a non-control in it. */
const emptySpot = (page, selector) => page.evaluate(selector => {
  const node = document.querySelector(selector), box = node.getBoundingClientRect(), y = box.top + box.height / 2;
  const controls = 'button,a,input,select,textarea,summary,label,[role=tab],[role=button],[tabindex]:not([tabindex="-1"]),[draggable=true]';
  // From the right edge of the room leftwards first, then across.
  const room = Number.parseFloat(getComputedStyle(node).paddingLeft);
  for (const x of [room - 4, ...Array.from({ length: Math.floor(box.width / 6) }, (_, i) => box.left + room + i * 6)]) {
    const hit = document.elementFromPoint(x, y);
    if (hit && node.contains(hit) && !hit.closest(controls) && !hit.closest('[data-window-drag-region="false"]')) return { x, y };
  }
  return null;
}, selector);

/** The common checks for one header; returns the page's errors seen. */
async function checkHeader(page, { kind, scheme, selector, inset, height }) {
  const header = page.locator(selector).first();
  await expect(header).toBeVisible();
  assert.equal(await page.evaluate(() => document.documentElement.dataset.titleBar), 'overlay', `${kind}: the window is marked`);
  const shown = await measure(header);
  assert.equal(shown.padding, `${inset}px`, `${kind} ${scheme}: the header leaves the buttons ${inset}px`);
  assert.ok(Math.round(shown.height) >= height, `${kind}: the header is at least ${height}px tall to centre them (${shown.height})`);
  assert.equal(Math.round(shown.top), 0, `${kind}: the header is the top of the window`);
  assert.ok(shown.left >= inset - 0.5, `${kind}: nothing in the header is under the buttons (leftmost at ${shown.left})`);
  assert.equal(shown.drag, 'deep', `${kind}: the header is a drag region`);
  // No control moves the window.
  const controls = await controlsThatDrag(page, selector);
  assert.deepEqual(controls.out, [], `${kind}: controls that would move the window: ${controls.out.join('; ')}`);
  assert.ok(controls.checked > 0, `${kind}: some controls were pressed`);
  // An empty part moves it; a double-click there asks for the system's action.
  const spot = await emptySpot(page, selector);
  assert.ok(spot, `${kind}: the header has an empty part`);
  await page.evaluate(() => { window.titleBarCalls.length = 0; });
  await page.mouse.move(spot.x, spot.y); await page.mouse.down(); await page.mouse.up();
  assert.deepEqual(await page.evaluate(() => window.titleBarCalls), [{ call: 'drag', popout: false }], `${kind}: a press on an empty part moves the window`);
  await page.evaluate(() => { window.titleBarCalls.length = 0; });
  await page.mouse.dblclick(spot.x, spot.y);
  assert.deepEqual(await page.evaluate(() => window.titleBarCalls), [{ call: 'drag', popout: false }, { call: 'double-click', popout: false }], `${kind}: a double-click on an empty part`);
  await page.evaluate(() => { window.titleBarCalls.length = 0; });
  // A double-click whose second press moves away does nothing (as in macOS).
  await page.mouse.click(spot.x, spot.y);
  await page.mouse.move(spot.x, spot.y); await page.mouse.down({ clickCount: 2 }); await page.mouse.move(spot.x + 30, spot.y); await page.mouse.up({ clickCount: 2 });
  assert.ok(!(await page.evaluate(() => window.titleBarCalls)).some(call => call.call === 'double-click'), `${kind}: a double-click that moved away does nothing`);
  await page.evaluate(() => { window.titleBarCalls.length = 0; });
  await page.mouse.move(0, 400);
  await page.screenshot({ path: `.cache/title-bar-${kind}-${scheme}.png`, clip: { x: 0, y: 0, width: Math.min(page.viewportSize().width, 1100), height: 96 } });
  // Full screen: the buttons are hidden, and so is their room.
  await page.evaluate(() => window.fixtureTitleBar.fullScreen(true));
  assert.equal(await page.evaluate(() => document.documentElement.hasAttribute('data-full-screen')), true);
  const full = await measure(header);
  assert.ok(px(full.padding) < 24, `${kind}: no room for hidden buttons in full screen (${full.padding})`);
  if (kind === 'workspace') await page.screenshot({ path: `.cache/title-bar-${kind}-full-screen-${scheme}.png`, clip: { x: 0, y: 0, width: 1100, height: 96 } });
  await page.evaluate(() => window.fixtureTitleBar.fullScreen(false));
  assert.equal((await measure(header)).padding, `${inset}px`, `${kind}: the room comes back out of full screen`);
  // The buttons are not zoomed with the page: the room is the same on screen.
  await page.evaluate(() => window.fixtureTitleBar.zoom(2));
  await expect.poll(async () => (await measure(header)).padding).toBe(`${Math.ceil(inset / 2)}px`);
  await page.evaluate(() => window.fixtureTitleBar.zoom(0.5));
  await expect.poll(async () => (await measure(header)).padding).toBe(`${inset * 2}px`);
  assert.ok((await measure(header)).height >= height * 2 - 1, `${kind}: zoomed out, the header still holds the buttons`);
  await page.evaluate(() => window.fixtureTitleBar.zoom(1));
  await expect.poll(async () => (await measure(header)).padding).toBe(`${inset}px`);
}

let browser;
try {
  await server.listen(); await mkdir('.cache', { recursive: true });
  browser = await webkit.launch();
  const origin = `http://127.0.0.1:${server.httpServer.address().port}`;
  const runner = `runner=${encodeURIComponent(f.url)}&token=${encodeURIComponent(f.token)}`;
  for (const scheme of ['dark', 'light']) {
    const context = await browser.newContext({ viewport: { width: 1280, height: 800 }, colorScheme: scheme });
    const errors = [];
    const open = async query => { const page = await context.newPage(); page.setDefaultTimeout(15000); await collectPageErrors(page, errors, { label: query }); await page.goto(`${origin}/?${query}&${runner}`); return page; };

    // A workspace window: brand, pages and actions after the buttons.
    {
      const page = await open('kind=workspace');
      await expect(page.locator('.workspace-header .runner-indicator')).toHaveText('Connected');
      await checkHeader(page, { kind: 'workspace', scheme, selector: '.workspace-header', inset: 89, height: 48 });
      // The page tabs, the pills and the menus are pressed, not dragged: a real click on Settings opens it.
      await page.getByRole('navigation', { name: 'Pages' }).getByRole('button', { name: 'Settings' }).click();
      assert.deepEqual(await page.evaluate(() => window.titleBarCalls), [], 'a page tab never moves the window');
      await page.getByRole('navigation', { name: 'Pages' }).getByRole('button', { name: 'Work', exact: true }).click();

      // A panel group popped out: its tab bar is that window's title bar.
      if (scheme === 'dark') {
        const tab = page.locator('.dv-tab').filter({ has: page.locator('[data-kind="files"]') });
        if (!(await tab.count())) {
          await page.locator('summary[aria-label="Panels"]').click();
          await page.getByRole('button', { name: 'Show Files' }).click();
        }
        const opened = page.waitForEvent('popup');
        await tab.click({ button: 'right' });
        await page.getByRole('menuitem', { name: 'Open in new window', exact: true }).click();
        const popup = await opened;
        await expect(popup.locator('.dv-tab').first()).toBeVisible();
        // A window as wide as the panel was; widened, its tab bar has an empty part.
        await popup.setViewportSize({ width: 640, height: 420 });
        await expect.poll(() => popup.locator('.dv-void-container').first().evaluate(node => node.getBoundingClientRect().width)).toBeGreaterThan(40);
        const bar = popup.locator('.dv-tabs-and-actions-container').first();
        const shown = await bar.evaluate(node => ({ marked: node.ownerDocument.documentElement.dataset.titleBar, popout: node.ownerDocument.documentElement.hasAttribute('data-popout'), padding: getComputedStyle(node).paddingLeft, height: node.getBoundingClientRect().height, drag: node.getAttribute('data-window-drag-region'), void: node.querySelector('.dv-void-container')?.getAttribute('data-window-drag-region') }));
        assert.deepEqual(shown, { marked: 'overlay', popout: true, padding: '85px', height: 40, drag: 'deep', void: '' }, 'the popped-out tab bar leaves the buttons room and moves its window');
        // Its empty part moves that window; its tabs and buttons do not.
        const empty = await popup.locator('.dv-void-container').first().boundingBox();
        await popup.mouse.move(empty.x + empty.width / 2, empty.y + empty.height / 2); await popup.mouse.down(); await popup.mouse.up();
        // So does the room beside the buttons.
        await popup.mouse.move(80, 34); await popup.mouse.down(); await popup.mouse.up();
        const tabBox = await popup.locator('.dv-tab').first().boundingBox();
        await popup.mouse.move(tabBox.x + 8, tabBox.y + tabBox.height / 2); await popup.mouse.down(); await popup.mouse.up();
        assert.deepEqual(await page.evaluate(() => window.titleBarCalls), [{ call: 'drag', popout: true }, { call: 'drag', popout: true }], 'the empty part of a popped-out tab bar moves that window, its tab does not');
        await page.evaluate(() => { window.titleBarCalls.length = 0; });
        // The buttons, where the host puts them in a 40-point bar (as the fixture draws them in its own window).
        await popup.evaluate(() => {
          const lights = document.createElement('div');
          lights.style.cssText = 'position:fixed;z-index:2147483647;pointer-events:none;left:13px;top:13px;display:flex;gap:9px';
          for (const color of ['#ff5f57', '#febc2e', '#28c840']) { const light = document.createElement('span'); light.style.cssText = `width:14px;height:14px;border-radius:50%;background:${color}`; lights.append(light); }
          document.body.append(lights);
        });
        await popup.mouse.move(600, 300);
        await popup.screenshot({ path: `.cache/title-bar-popout-${scheme}.png`, clip: { x: 0, y: 0, width: 600, height: 80 } });
        await popup.bringToFront(); await popup.evaluate(() => window.focus());
        // Full screen is entered by the window in front: the popped-out one here.
        assert.ok(await popup.evaluate(() => document.hasFocus()), 'the popped-out window is in front');
        await page.evaluate(() => window.fixtureTitleBar.popoutFullScreen(true));
        assert.ok(px(await bar.evaluate(node => getComputedStyle(node).paddingLeft)) < 24, 'a popped-out window in full screen drops the room');
        await page.evaluate(() => window.fixtureTitleBar.popoutFullScreen(false));
        assert.equal(await bar.evaluate(node => getComputedStyle(node).paddingLeft), '85px');
        // The native popout scale and its opener's scale are independent.
        await popup.evaluate(() => document.documentElement.style.setProperty('--popout-page-zoom', '1.25'));
        await expect.poll(() => bar.evaluate(node => getComputedStyle(node).paddingLeft)).toBe('68px');
        const openerHeader = page.locator('.workspace-header');
        await expect.poll(async () => (await measure(openerHeader)).padding).toBe('89px');
        await page.evaluate(() => window.fixtureTitleBar.zoom(2));
        await expect.poll(async () => (await measure(openerHeader)).padding).toBe('45px');
        await expect.poll(() => bar.evaluate(node => getComputedStyle(node).paddingLeft)).toBe('68px');
        await page.evaluate(() => window.fixtureTitleBar.zoom(1));
        await popup.evaluate(() => document.documentElement.style.setProperty('--popout-page-zoom', '1'));

        const closed = popup.waitForEvent('close');
        await popup.locator('.dock-put-back').first().click();
        await closed;
      }
      await page.close();
    }

    // A chat, an artifact and a command console share the window's toolbar.
    for (const [kind, query] of [['chat', `kind=chat&session=${f.sessions[0].id}`], ['artifact', 'kind=artifact&artifact=missing']]) {
      const page = await open(query);
      await checkHeader(page, { kind, scheme, selector: '.native-toolbar.window-controls', inset: 85, height: 40 });
      await page.close();
    }

    // A web page in its own window: the viewer's toolbar; the address is edited, never dragged.
    {
      const page = await open('kind=web');
      await checkHeader(page, { kind: 'web', scheme, selector: '.web-window .web-toolbar', inset: 85, height: 40 });
      const address = page.getByRole('textbox', { name: 'Address' });
      const box = await address.boundingBox();
      await page.mouse.move(box.x + box.width - 6, box.y + box.height / 2); await page.mouse.down(); await page.mouse.up();
      await expect(address).toBeFocused();
      assert.deepEqual(await page.evaluate(() => window.titleBarCalls), [], 'a press on the address never moves the window');
      await page.close();
    }

    // The setup window: a strip under the buttons moves it; the content starts below.
    {
      const page = await open('kind=setup');
      const strip = page.locator('.first-run-title-bar');
      await expect(strip).toBeVisible();
      const shown = await strip.evaluate(node => ({ height: node.getBoundingClientRect().height, top: node.getBoundingClientRect().top, drag: node.getAttribute('data-window-drag-region') }));
      assert.deepEqual(shown, { height: 40, top: 0, drag: '' });
      assert.ok((await page.locator('.first-run-header').boundingBox()).y >= 40, 'the setup window\'s content starts under the strip');
      await page.mouse.move(300, 20); await page.mouse.down(); await page.mouse.up();
      assert.deepEqual(await page.evaluate(() => window.titleBarCalls), [{ call: 'drag', popout: false }], 'the strip moves the setup window');
      await page.mouse.move(0, 400);
      await page.screenshot({ path: `.cache/title-bar-setup-${scheme}.png`, clip: { x: 0, y: 0, width: 700, height: 160 } });
      await page.close();
    }

    // A browser's window (the web app, the phone): nothing changes.
    for (const width of [1280, 390]) {
      const page = await open('kind=workspace&overlay=0');
      await page.setViewportSize({ width, height: 800 });
      await expect(page.locator('.workspace-header .runner-indicator')).toHaveCount(1);
      assert.equal(await page.evaluate(() => document.documentElement.dataset.titleBar), undefined, 'a browser has its own title bar');
      const padding = await page.locator('.workspace-header').evaluate(node => getComputedStyle(node).paddingLeft);
      assert.ok(px(padding) <= 16, `a browser's header keeps its own padding at ${width}px (${padding})`);
      await page.close();
    }
    assert.deepEqual(errors, [], `page errors: ${errors.join('\n')}`);
    await context.close();
  }
  console.log('title bar: every window kind leaves the buttons their room, moves by its empty header, and drops the room in full screen');
} finally {
  await browser?.close();
  await server.close();
  await f.close();
}
