import {testViteCache} from './fixtures/vite-cache.mjs';
import { createServer } from 'vite';
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
  const page = await browser.newPage({ viewport: { width: 1400, height: 1000 } });
  page.setDefaultTimeout(10_000);
  const errors = []; await collectPageErrors(page, errors);
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace#token=${fixture.token}`);

  const dragHeader = async (dialog, dx, dy) => {
    const header = await dialog.locator('[data-dialog-drag-handle]').boundingBox();
    await page.mouse.move(header.x + 35, header.y + header.height / 2);
    await page.mouse.down();
    await page.mouse.move(header.x + 35 + dx, header.y + header.height / 2 + dy, { steps: 8 });
    await page.mouse.up();
  };
  const checkMovement = async dialog => {
    await expect(dialog).toBeVisible();
    const before = await dialog.boundingBox();
    await dragHeader(dialog, 100, 65);
    const after = await dialog.boundingBox();
    assert.ok(Math.abs(after.x - before.x - 100) < 2 && Math.abs(after.y - before.y - 65) < 2, `Header drag must move this dialog: ${JSON.stringify({before, after})}`);
    assert.equal(await dialog.evaluate(node => getComputedStyle(node, '::backdrop').backdropFilter), 'none', 'Underlying names must stay readable');
    const header = dialog.locator('[data-dialog-drag-handle]');
    await header.focus(); await header.press('ArrowLeft');
    assert.ok(Math.abs((await dialog.boundingBox()).x - after.x + 10) < 2, 'Keyboard movement');
    await header.press('Home');
    assert.ok(Math.abs((await dialog.boundingBox()).x - before.x) < 2, 'Home recenters');
    await dragHeader(dialog, -2000, -2000);
    const edge = await dialog.boundingBox();
    assert.ok(edge.x >= 7 && edge.y >= 7, 'Title and close control must remain reachable');
    await header.press('Home');
  };

  await page.getByRole('button', { name: 'Settings', exact: true }).click();
  await page.getByRole('button', { name: 'Add subscription', exact: true }).click();
  const subscription = page.getByRole('dialog', { name: 'Connect a subscription' });
  await subscription.getByLabel('Account name').fill('Keep this draft');
  await checkMovement(subscription);
  await expect(subscription.getByLabel('Account name')).toHaveValue('Keep this draft');
  const bounds = await subscription.boundingBox();
  const input = await subscription.getByLabel('Account name').boundingBox();
  await page.mouse.move(input.x + 20, input.y + 15); await page.mouse.down(); await page.mouse.move(input.x + 120, input.y + 15); await page.mouse.up();
  assert.equal((await subscription.boundingBox()).x, bounds.x, 'Selecting input text must not drag the dialog');
  await dragHeader(subscription, 150, 80);
  await page.keyboard.press('Escape'); await expect(subscription).not.toBeVisible();
  await page.getByRole('button', { name: 'Add subscription', exact: true }).click();
  await expect(subscription).toBeVisible();
  assert.ok(Math.abs((await subscription.boundingBox()).x - bounds.x) < 2, 'Reopening recenters');
  await subscription.getByRole('button', { name: 'Close subscription dialog' }).click();
  await expect(subscription).not.toBeVisible();

  const newConversation = page.getByRole('button', { name: 'New conversation', exact: true });
  await newConversation.click();
  const session = page.getByRole('dialog', { name: 'New conversation', exact: true });
  await expect(session.getByRole('heading', { name: 'New conversation', level: 2 })).toBeVisible();
  await checkMovement(session);
  await session.getByRole('button', { name: 'Close new conversation' }).click();
  await expect(session).not.toBeVisible();
  // Closing gives the focus back to what opened the dialog.
  await expect(newConversation).toBeFocused();

  await page.getByRole('button',{name:'Work',exact:true}).click();
  await page.getByRole('button',{name:'New workspace',exact:true}).click();
  const workspace=page.getByRole('dialog',{name:'New workspace',exact:true});
  await expect(workspace.getByRole('status')).toHaveCount(0);
  await checkMovement(workspace);
  await dragHeader(workspace,230,180);await page.setViewportSize({width:700,height:500});await expect.poll(async()=>{const b=await workspace.boundingBox();return b.x>=7&&b.y>=7&&b.x+b.width<=693&&b.y+b.height<=493;}).toBe(true);
  await workspace.getByRole('button',{name:'Close workspace dialog'}).click();
  await expect(workspace).toHaveCount(0);

  // One dialog style: a danger button's label is the colour made for its fill, in either theme, whatever
  // shell-wide rule (`.workspace-shell button.danger`) comes later; and the title is the shared size.
  const contrast = node => {
    const channels = value => { const n = (value.match(/[\d.]+/g) || []).map(Number); return value.startsWith('color(') ? n.slice(0, 3).map(v => v * 255) : n.slice(0, 3); };
    const luminance = rgb => { const [r, g, b] = rgb.map(v => { v /= 255; return v <= 0.03928 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4; }); return 0.2126 * r + 0.7152 * g + 0.0722 * b; };
    const style = getComputedStyle(node), fg = luminance(channels(style.color)), bg = luminance(channels(style.backgroundColor));
    return { ratio: (Math.max(fg, bg) + 0.05) / (Math.min(fg, bg) + 0.05), color: style.color, background: style.backgroundColor };
  };
  await page.setViewportSize({ width: 1400, height: 1000 });
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`);
  await expect(page.locator('.panel-chat').getByRole('textbox', { name: 'Message', exact: true })).toBeVisible();
  await page.locator('summary[aria-label="Panels"]').click();
  await page.getByRole('button', { name: 'Show Processes', exact: true }).click();
  const processes = page.getByRole('region', { name: 'Hosted processes', exact: true });
  for (const colorScheme of ['dark', 'light']) {
    await page.emulateMedia({ colorScheme });
    await processes.getByRole('button', { name: 'Stop Fix the parser…', exact: true }).click();
    const stop = page.getByRole('dialog', { name: 'Stop “Fix the parser”?' });
    const danger = stop.getByRole('button', { name: 'Stop agent', exact: true });
    await expect(danger).toBeVisible();
    const measured = await danger.evaluate(contrast);
    assert.ok(measured.ratio >= 4.5, `${colorScheme}: the danger label reads on its fill (${JSON.stringify(measured)})`);
    assert.equal(await stop.evaluate(node => getComputedStyle(node.querySelector('h2')).fontSize), '15px', `${colorScheme}: the shared dialog title size`);
    await page.waitForTimeout(150);
    await stop.screenshot({ path: `.cache/dialogs-stop-${colorScheme}.png` });
    await stop.getByRole('button', { name: 'Cancel', exact: true }).click();
    await expect(stop).toHaveCount(0);
    // And a dialog with a primary action: New workspace, starting on its name.
    await page.getByRole('button', { name: 'Work', exact: true }).click();
    await page.getByRole('button', { name: 'New workspace', exact: true }).click();
    const created = page.getByRole('dialog', { name: 'New workspace', exact: true });
    await expect(created.getByLabel('Workspace name', { exact: true })).toBeFocused();
    await page.waitForTimeout(150);
    await created.screenshot({ path: `.cache/dialogs-new-workspace-${colorScheme}.png` });
    await created.getByRole('button', { name: 'Close workspace dialog' }).click();
    await expect(created).toHaveCount(0);
  }
  assert.deepEqual(fixture.mutations, [], 'Moving dialogs must not start sessions, login or repository operations');
  assert.deepEqual(errors, []);
  console.log('PASS: subscription, session and workspace dialogs drag by header, preserve input, keyboard movement/recenter, viewport bounds/resize, readable backdrop, Escape, close and reopen, and give the focus back; a danger button reads on its fill and titles share one size, dark and light. Fake Runner only.');
} finally { await browser?.close(); await server.close(); await fixture.close(); }
