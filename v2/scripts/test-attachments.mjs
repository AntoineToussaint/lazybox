import { createServer } from 'vite';
import react from '@vitejs/plugin-react';
import { chromium, expect } from '@playwright/test';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';

// Exercise the real shared composer against fake RPC, without accounts/inference.
const fixture = await startRunnerFixture();
const server = await createServer({ configFile: false, root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: fixture.url } } } });
let browser;
try {
  await server.listen();
  browser = await chromium.launch({ headless: true });
  const errors = [];
  const submissions = [];
  for (const view of ['agent', 'chat', 'workspace']) {
    const context = await browser.newContext();
    const page = await context.newPage();
    page.on('pageerror', error => errors.push(String(error)));
    await page.route('**/Info', async route => {
      const response = await route.fetch();
      const body = await response.json();
      body.attachmentLimits = { maxFiles: 4, maxFileBytes: '1024', maxMessageBytes: '1500' };
      await route.fulfill({ response, json: body });
    });
    let failNext = true;
    await page.route('**/Apply', async route => {
      const body = route.request().postDataJSON(); submissions.push(body);
      if (failNext) {
        failNext = false;
        await route.fulfill({ status: 503, json: { code: 'unavailable', message: 'Test delivery failure' } });
      } else await route.fulfill({ json: { operationId: body.operationId, delivery: 'accepted' } });
    });
    await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=${view}&session=alpha-session&collection=alpha#token=${fixture.token}`);
    const chat = view === 'workspace' ? page.getByRole('region', { name: 'Agent chat', exact: true }) : page.locator('.agent-window');
    const composer = chat.getByRole('textbox', { name: 'Message', exact: true });
    await expect(composer).toBeEnabled();
    const originalUrl = page.url();
    const drop = async (files, options = {}) => composer.evaluate((node, { files, options }) => {
      const dataTransfer = new DataTransfer();
      for (const file of files) dataTransfer.items.add(new File([new Uint8Array(file.bytes)], file.name, { type: file.type || '', lastModified: 123 }));
      const originalEntry = DataTransferItem.prototype.webkitGetAsEntry;
      if (options.directory) DataTransferItem.prototype.webkitGetAsEntry = () => ({ isDirectory: true });
      if (options.text) dataTransfer.setData('text/plain', options.text);
      const event = new DragEvent(options.type || 'drop', { dataTransfer, bubbles: true, cancelable: true });
      try { node.dispatchEvent(event); return event.defaultPrevented; }
      finally { DataTransferItem.prototype.webkitGetAsEntry = originalEntry; }
    }, { files, options });
    const text = { name: 'notes.txt', type: 'text/plain', bytes: [...Buffer.from('dropped attachment')] };
    const binary = { name: 'archive.bin', bytes: [0, 255, 128, 13, 10, 0] };
    assert.equal(await drop([text], { type: 'dragover' }), true);
    await expect(chat.getByText('Drop files to attach', { exact: true })).toBeVisible();
    assert.equal(await drop([text, binary]), true);
    await expect(chat.getByText('Drop files to attach', { exact: true })).toHaveCount(0);
    const attached = chat.getByRole('list', { name: 'Attached files' });
    await expect(attached).toContainText(text.name);
    await expect(attached).toContainText(binary.name);
    const before = submissions.length;
    assert.equal(fixture.mutations.length, 0, 'Dropping must not send or start an execution');
    await composer.fill('Read these files');
    await composer.press('Enter');
    await expect(chat.getByRole('alert')).toContainText('Test delivery failure');
    await expect(attached).toContainText(binary.name);
    await composer.press('Enter');
    await expect(attached).toHaveCount(0);
    assert.equal(submissions.length, before + 2);
    assert.equal(submissions[before].operationId, submissions[before + 1].operationId);
    assert.deepEqual(submissions[before].message.attachments, submissions[before + 1].message.attachments);
    for (const file of [text, binary]) {
      const received = submissions[before + 1].message.attachments.find(value => value.name === file.name);
      assert.deepEqual([...Buffer.from(received.data, 'base64')], file.bytes, 'Drop must preserve exact bytes through the existing upload path');
    }
    // Shared limits and directory rejection must leave the existing draft intact.
    await drop([text]);
    await drop([{ name: 'too-large', bytes: Array(1025).fill(1) }]);
    await expect(chat.getByRole('alert')).toContainText('Attach up to 4 files');
    await expect(attached.locator('li')).toHaveCount(1);
    await drop([text], { directory: true });
    await expect(chat.getByRole('alert')).toContainText('folders are not supported');
    await expect(attached.locator('li')).toHaveCount(1);
    await drop(Array(4).fill(text));
    await expect(attached.locator('li')).toHaveCount(1);
    await drop([{ name: 'first', bytes: Array(800).fill(1) }, { name: 'second', bytes: Array(800).fill(2) }]);
    await expect(attached.locator('li')).toHaveCount(1);
    assert.equal(await drop([], { text: 'ordinary dragged text' }), false, 'Text drops retain native editor behavior');
    assert.equal(page.url(), originalUrl, 'File drop must not navigate the webview');
    await chat.getByRole('button', { name: 'Remove notes.txt', exact: true }).click();
    // An ended session cannot acquire attachments, even if a drop is delivered.
    fixture.sessions[0].closed = true;
    await expect(composer).toBeDisabled();
    assert.equal(await drop([text]), true);
    await expect(chat.getByRole('alert')).toContainText('cannot accept files');
    await expect(attached).toHaveCount(0);
    fixture.sessions[0].closed = false;
    await context.close();
  }
  assert.deepEqual(errors, []);
  console.log('PASS: standalone, detached and embedded chat file drops; exact text/binary payloads; retry identity; no auto-send; size/count/total limits; folder and ended-session rejection; text drops unaffected. Fake RPC, no credentials or inference.');
} finally {
  await browser?.close(); await server.close(); await fixture.close();
}
