import { createServer } from 'vite';
import react from '@vitejs/plugin-react';
import { chromium, expect } from '@playwright/test';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';

const fixture = await startRunnerFixture();
const server = await createServer({ configFile: false, root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: fixture.url } } } });
let browser;
try {
  await server.listen(); browser = await chromium.launch({ headless: true });
  const page = await browser.newPage({ viewport: { width: 1400, height: 1000 } });
  page.setDefaultTimeout(10_000);
  const errors = []; page.on('pageerror', error => errors.push(String(error)));
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

  await page.getByRole('button', { name: 'New conversation', exact: true }).click();
  const session = page.getByRole('dialog', { name: 'New conversation', exact: true });
  await checkMovement(session);
  await session.getByRole('button', { name: 'Close new conversation' }).click();
  await expect(session).not.toBeVisible();

  await page.getByRole('button',{name:'Work',exact:true}).click();
  await page.getByRole('button',{name:'New workspace',exact:true}).click();
  const workspace=page.getByRole('dialog',{name:'New workspace',exact:true});
  await expect(workspace.getByRole('status')).toHaveCount(0);
  await checkMovement(workspace);
  await dragHeader(workspace,230,180);await page.setViewportSize({width:700,height:500});await expect.poll(async()=>{const b=await workspace.boundingBox();return b.x>=7&&b.y>=7&&b.x+b.width<=693&&b.y+b.height<=493;}).toBe(true);
  await workspace.getByRole('button',{name:'Close workspace dialog'}).click();
  await expect(workspace).toHaveCount(0);
  assert.deepEqual(fixture.mutations, [], 'Moving dialogs must not start sessions, login or repository operations');
  assert.deepEqual(errors, []);
  console.log('PASS: subscription, session and workspace dialogs drag by header, preserve input, keyboard movement/recenter, viewport bounds/resize, readable backdrop, Escape, close and reopen. Fake Runner only.');
} finally { await browser?.close(); await server.close(); await fixture.close(); }
