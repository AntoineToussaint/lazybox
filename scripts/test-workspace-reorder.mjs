// Reordering workspaces in the sidebar, against the fake Runner, in WebKit:
// dragging a row with the mouse shows a line where it lands and keeps the new
// order on the Runner (ArrangeWorkspaces), so it survives a reload and every
// window and the phone list alike; the drag selects no text, opens no hover
// card, and the click it ends with selects nothing; Esc puts the row back; the
// list scrolls near its edge. ⌥⇧↑ / ⌥⇧↓ move a focused row one place and keep
// its focus; ⌥↓ alone still opens the hover card, and a right-click its menu.
// Dark and light screenshots of a drag. Nothing runs Git or an agent.
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from './fixtures/vite-cache.mjs';
import react from '@vitejs/plugin-react';
import {webkit, expect} from './fixtures/playwright.mjs';
import {fileURLToPath} from 'node:url';
import {mkdir} from 'node:fs/promises';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import {collectPageErrors} from './fixtures/page-errors.mjs';

const f = await startRunnerFixture();
// Eight workspaces: more than a short window shows, so the list scrolls.
for (const [id, name] of [['gamma', 'Gamma docs'], ['delta', 'Delta notes'], ['epsilon', 'Epsilon API'], ['zeta', 'Zeta billing'], ['eta', 'Eta search'], ['theta', 'Theta mobile']])
  f.collections.push({...structuredClone(f.collections[1]), id, name, workId: id, root: `/fixture/collections/${id}`, ownerExecutionId: ''});
const server = await createServer({...testViteCache(import.meta.url), configFile: false, root: fileURLToPath(new URL('../apps/web', import.meta.url)), logLevel: 'warn', plugins: [react()], server: {host: '127.0.0.1', port: 0, proxy: {'/lazybox.bench.v1.RunnerService/': {target: f.url}}}});
let browser;
const arranged = () => f.mutations.filter(m => m.method === 'ArrangeWorkspaces').map(m => m.body.collectionIds);
try {
  await server.listen(); await mkdir('.cache', {recursive: true});
  browser = await webkit.launch({headless: true});
  const context = await browser.newContext({viewport: {width: 1400, height: 900}, colorScheme: 'dark'});
  const page = await context.newPage();
  page.setDefaultTimeout(10_000);
  const errors = await collectPageErrors(page);
  const url = `http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=beta#token=${f.token}`;
  await page.goto(url);
  const nav = page.getByRole('navigation', {name: 'Workspaces'});
  const names = nav.locator(':scope > .collection-group .workspace-label > strong');
  const link = name => nav.locator('.collection-group').filter({has: page.locator('.workspace-label > strong', {hasText: name})}).locator('.workspace-link');
  const initial = ['Alpha feature', 'Beta release', 'Gamma docs', 'Delta notes', 'Epsilon API', 'Zeta billing', 'Eta search', 'Theta mobile'];
  await expect(names).toHaveText(initial);
  const current = nav.locator('.workspace-link[aria-current="page"]');
  await expect(current).toContainText('Beta release');
  const centre = async locator => { const box = await locator.boundingBox(); return {x: box.x + box.width / 2, y: box.y + box.height / 2, box}; };

  // 1. Drag Alpha below Gamma with the mouse: a line shows where it lands, no text is selected, no hover card opens.
  const from = await centre(link('Alpha feature'));
  const gamma = await centre(link('Gamma docs'));
  await page.mouse.move(from.x, from.y);
  await page.mouse.down();
  await page.mouse.move(from.x, from.y + 12, {steps: 3});
  await expect(page.locator('html.is-reordering')).toHaveCount(1);
  await page.mouse.move(gamma.x, gamma.box.y + gamma.box.height - 4, {steps: 12});
  await expect(nav.locator('[data-dragging]')).toContainText('Alpha feature');
  await expect(nav.locator('[data-drop]')).toHaveCount(1);
  await expect(nav.locator('[data-drop]')).toHaveAttribute('data-drop', 'before');
  await expect(nav.locator('[data-drop] .workspace-label > strong')).toHaveText('Delta notes');
  // Resting over another row mid-drag, longer than a card takes to open: none opens.
  await page.waitForTimeout(800);
  await expect(page.locator('.workspace-hover')).toHaveCount(0);
  assert.equal(await page.evaluate(() => String(window.getSelection())), '', 'the drag selected text');
  await page.screenshot({path: '.cache/workspace-reorder-drag-dark.png'});
  await page.mouse.up();
  const afterDrag = ['Beta release', 'Gamma docs', 'Alpha feature', 'Delta notes', 'Epsilon API', 'Zeta billing', 'Eta search', 'Theta mobile'];
  await expect(names).toHaveText(afterDrag);
  await expect(page.locator('html.is-reordering')).toHaveCount(0);
  await expect.poll(arranged).toEqual([['beta', 'gamma', 'alpha', 'delta', 'epsilon', 'zeta', 'eta', 'theta']]);
  // The drop was not a click: Beta is still the workspace shown.
  await expect(current).toContainText('Beta release');
  assert.equal(await page.evaluate(() => String(window.getSelection())), '');

  // 2. Esc puts a dragged row back: nothing moves, nothing is kept.
  const delta = await centre(link('Delta notes'));
  await page.mouse.move(delta.x, delta.y); await page.mouse.down();
  await page.mouse.move(delta.x, delta.y - 120, {steps: 10});
  await expect(nav.locator('[data-drop]')).toHaveCount(1);
  await page.keyboard.press('Escape');
  await expect(nav.locator('[data-drop]')).toHaveCount(0);
  await expect(nav.locator('[data-dragging]')).toHaveCount(0);
  await page.mouse.up();
  await expect(names).toHaveText(afterDrag);
  assert.equal(arranged().length, 1, 'a cancelled drag kept an order');

  // 3. ⌥⇧↑ / ⌥⇧↓ on a focused row move it one place; it keeps its focus.
  await link('Epsilon API').focus();
  await page.keyboard.press('Alt+Shift+ArrowUp');
  await expect(names).toHaveText(['Beta release', 'Gamma docs', 'Alpha feature', 'Epsilon API', 'Delta notes', 'Zeta billing', 'Eta search', 'Theta mobile']);
  await expect(link('Epsilon API')).toBeFocused();
  await page.keyboard.press('Alt+Shift+ArrowUp');
  await expect(link('Epsilon API')).toBeFocused();
  await page.keyboard.press('Alt+Shift+ArrowDown');
  const afterKeys = ['Beta release', 'Gamma docs', 'Alpha feature', 'Epsilon API', 'Delta notes', 'Zeta billing', 'Eta search', 'Theta mobile'];
  await expect(names).toHaveText(afterKeys);
  await expect(link('Epsilon API')).toBeFocused();
  await expect(nav.getByRole('status').filter({hasText: 'moved to'})).toHaveText('Epsilon API moved to 4 of 8');
  await expect.poll(() => arranged().at(-1)).toEqual(['beta', 'gamma', 'alpha', 'epsilon', 'delta', 'zeta', 'eta', 'theta']);
  // A pending focus-restoration frame must not steal a subsequent focus move.
  // Dispatch and move focus in one task, before that frame can run.
  await link('Epsilon API').evaluate(node=>{
    node.dispatchEvent(new KeyboardEvent('keydown',{key:'ArrowUp',altKey:true,shiftKey:true,bubbles:true,cancelable:true}));
    document.querySelector('.workspace-link[aria-current="page"]').focus();
  });
  await page.evaluate(()=>new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve))));
  await expect(link('Beta release')).toBeFocused();
  await link('Epsilon API').focus();
  await page.keyboard.press('Alt+Shift+ArrowDown');
  await expect(names).toHaveText(afterKeys);
  // Move the pointer away so a delayed hover is not part of the keyboard test.
  await page.mouse.move(1300,850);
  // At the top, ⌥⇧↑ changes nothing and keeps nothing.
  await link('Beta release').focus();
  const kept = arranged().length;
  await page.keyboard.press('Alt+Shift+ArrowUp');
  await page.waitForTimeout(300);
  assert.equal(arranged().length, kept, 'moving the first row up kept an order');
  // ⌥↓ alone still opens the row's hover card; Esc closes it.
  await page.keyboard.press('Alt+ArrowDown');
  await expect(page.getByRole('dialog', {name: 'Beta release'})).toBeVisible();
  await page.keyboard.press('Escape');
  await expect(page.locator('.workspace-hover')).toHaveCount(0);
  await expect(names).toHaveText(afterKeys);
  // A right-click still opens the row's menu, and moves nothing.
  await link('Gamma docs').click({button: 'right'});
  await expect(page.getByRole('menu')).toBeVisible();
  await page.keyboard.press('Escape');
  await expect(names).toHaveText(afterKeys);

  // 4. The order is the Runner's: it survives a reload.
  await page.reload();
  await expect(names).toHaveText(afterKeys);

  // 5. Near the list's edge it scrolls while a row is held there.
  await page.setViewportSize({width: 1400, height: 520});
  const scroller = await nav.evaluateHandle(node => { for (let el = node.parentElement; el; el = el.parentElement) { const style = getComputedStyle(el); if (/(auto|scroll)/.test(style.overflowY) && el.scrollHeight > el.clientHeight) return el; } return document.scrollingElement; });
  const top = await scroller.evaluate(el => { el.scrollTop = 0; return el.scrollTop; });
  const edge = await scroller.evaluate(el => { const box = el === document.scrollingElement ? {bottom: innerHeight} : el.getBoundingClientRect(); return box.bottom; });
  const beta = await centre(link('Beta release'));
  await page.mouse.move(beta.x, beta.y); await page.mouse.down();
  await page.mouse.move(beta.x, edge - 6, {steps: 15});
  await expect.poll(() => scroller.evaluate(el => el.scrollTop)).toBeGreaterThan(top + 40);
  await page.keyboard.press('Escape'); await page.mouse.up();
  await expect(names).toHaveText(afterKeys);

  // 6. Light: the line and the lifted row read in both themes.
  await page.setViewportSize({width: 1400, height: 900});
  await page.emulateMedia({colorScheme: 'light'});
  await scroller.evaluate(el => { el.scrollTop = 0; });
  const epsilon = await centre(link('Epsilon API'));
  await page.mouse.move(epsilon.x, epsilon.y); await page.mouse.down();
  await page.mouse.move(epsilon.x, epsilon.y - 150, {steps: 12});
  await expect(nav.locator('[data-drop]')).toHaveCount(1);
  await page.screenshot({path: '.cache/workspace-reorder-drag-light.png'});
  await page.keyboard.press('Escape'); await page.mouse.up();
  const original=f.sessions.find(s=>s.collectionId==='alpha');
  original.title='First discussion';
  f.sessions.push({...original,id:'second-thread',workId:'second-thread',conversationId:'second-thread',title:'Second discussion'});
  await page.reload();
  const conversations=nav.getByRole('navigation',{name:'Workspace conversations'});
  const links=conversations.locator('.conversation-link');
  await expect(links).toHaveCount(2);
  const titles=links.locator('strong');
  const before=await titles.allTextContents();
  await links.first().focus();
  await page.keyboard.press('Alt+Shift+ArrowDown');
  await expect(titles).toHaveText([...before].reverse());
  await expect.poll(()=>f.mutations.filter(m=>m.method==='ArrangeConversations').length).toBe(1);
  await page.reload();
  await expect(titles).toHaveText([...before].reverse());
  const first=await centre(links.first()), second=await centre(links.last());
  await page.mouse.move(first.x,first.y); await page.mouse.down();
  await page.mouse.move(second.x,second.box.y+second.box.height-1,{steps:10}); await page.mouse.up();
  await expect(titles).toHaveText(before);
  await expect.poll(()=>f.mutations.filter(m=>m.method==='ArrangeConversations').length).toBe(2);
  await expect(names).toHaveText(afterKeys);
  assert.deepEqual(errors, []);
  console.log('PASS workspace reorder: a mouse drag shows where the row lands and keeps the order on the Runner (no text selected, no hover card, the drop is no click), Esc puts it back, the list scrolls at its edge, ⌥⇧↑/⌥⇧↓ move a focused row and keep its focus (⌥↓ still opens the card, right-click the menu), and the order survives a reload; dark and light.');
} finally {
  await browser?.close(); await server.close(); await f.close();
}
