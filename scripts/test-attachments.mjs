import {testViteCache} from './fixtures/vite-cache.mjs';
import { createServer } from './fixtures/vite-cache.mjs';
import { drawingAssets } from './drawing-assets.mjs';
import react from '@vitejs/plugin-react';
import { chromium, webkit, expect } from './fixtures/playwright.mjs';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';
import { collectPageErrors, drainPageErrors } from './fixtures/page-errors.mjs';

// Exercise the real shared composer against fake RPC, without accounts/inference.
const fixture = await startRunnerFixture();
const server = await createServer({...testViteCache(import.meta.url), configFile: false, root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react(), drawingAssets()], server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: fixture.url } } } });
let browser;
try {
  await server.listen();
  browser = await (process.env.TEST_WEBKIT ? webkit : chromium).launch({ headless: true });
  const errors = [];
  const submissions = [];
  for (const view of ['agent', 'chat', 'workspace']) {
    const context = await browser.newContext({ colorScheme: 'dark' });
    const page = await context.newPage();
    await collectPageErrors(page, errors);
    let drawingLimits = view === 'agent';
    await page.route('**/Info', async route => {
      const response = await route.fetch();
      const body = await response.json();
      body.attachmentLimits = { maxFiles: 4, maxFileBytes: drawingLimits ? '8000000' : '1024', maxMessageBytes: drawingLimits ? '8000000' : '1500' };
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
    if (drawingLimits) {
      await chat.getByRole('button', { name: 'Draw', exact: true }).click();
      const editor = page.getByRole('dialog', { name: 'Drawing editor' });
      await expect(editor.locator('canvas.interactive')).toBeVisible();
      await page.screenshot({ path: '.cache/drawing-editor.png' });
      const attach = editor.getByRole('button', { name: 'Attach drawing' });
      await expect(attach).toBeDisabled();
      await expect(editor.getByRole('radio', { name: /draw/i }).first()).toBeChecked();
      async function drawRectangle() {
        const canvas = editor.locator('canvas.interactive');
        const bounds = await canvas.boundingBox();
        assert.ok(bounds);
        await editor.getByRole('radio', { name: /rectangle/i }).locator('..').click();
        await page.mouse.move(bounds.x + bounds.width * 0.48, bounds.y + bounds.height * 0.48);
        await page.mouse.down();
        await page.mouse.move(bounds.x + bounds.width * 0.68, bounds.y + bounds.height * 0.65, { steps: 5 });
        await page.mouse.up();
      }
      const attachBounds = await editor.getByRole('button', { name: 'Attach drawing' }).boundingBox();
      const canvasBounds = await editor.locator('canvas.interactive').boundingBox();
      assert.ok(attachBounds.width < canvasBounds.width / 2);
      assert.ok(attachBounds.y + attachBounds.height <= canvasBounds.y + 1);
      // Tool shortcuts work on first open, before clicking the toolbar.
      for (const [key, label] of [['v', /selection/i], ['r', /rectangle/i], ['d', /diamond/i],
        ['o', /ellipse/i], ['a', /arrow/i], ['l', /^line/i], ['t', /^text/i], ['e', /eraser/i], ['p', /draw/i]]) {
        await page.keyboard.press(key);
        await expect(editor.getByRole('radio', { name: label }).first()).toBeChecked();
      }
      const cx = canvasBounds.x + canvasBounds.width / 2;
      const cy = canvasBounds.y + canvasBounds.height / 2;
      await page.mouse.move(cx, cy);
      await page.mouse.down();
      await page.mouse.move(cx + 60, cy + 25, { steps: 6 });
      await page.mouse.up();
      await expect(attach).toBeEnabled();
      await page.keyboard.press('e');
      await page.mouse.move(cx + 30, cy - 10);
      await page.mouse.down();
      await page.mouse.move(cx + 30, cy + 40, { steps: 10 });
      await page.mouse.up();
      await expect(attach).toBeDisabled();
      await editor.getByRole('button', { name: 'Undo', exact: true }).click();
      await expect(attach).toBeEnabled();
      await editor.getByRole('button', { name: 'Redo', exact: true }).click();
      await expect(attach).toBeDisabled();
      await page.keyboard.press('t');
      await page.mouse.click(cx, cy);
      const label = editor.locator('textarea.excalidraw-wysiwyg');
      await label.pressSequentially('Retry here');
      await expect(label).toHaveValue('Retry here');
      await page.keyboard.press('Escape');
      await expect(attach).toBeEnabled();
      await page.keyboard.press('Control+z');
      await expect(attach).toBeDisabled();
      await drawRectangle();
      const undo = editor.getByRole('button', { name: 'Undo', exact: true });
      const redo = editor.getByRole('button', { name: 'Redo', exact: true });
      await expect(undo).toBeEnabled();
      await page.keyboard.press('Control+z');
      await expect(redo).toBeEnabled();
      await expect(attach).toBeDisabled();
      await page.keyboard.press('Control+Shift+z');
      await expect(redo).toBeDisabled();
      await page.keyboard.press('Meta+z');
      await expect(redo).toBeEnabled();
      await page.keyboard.press('Control+y');
      await expect(redo).toBeDisabled();
      await page.keyboard.press('Meta+a');
      await page.keyboard.press('Backspace');
      await expect(attach).toBeDisabled();
      await page.keyboard.press('Control+z');
      await expect(attach).toBeEnabled();
      await page.keyboard.press('Escape');
      await expect(editor).toBeVisible();
      await editor.getByRole('button', { name: 'Attach drawing' }).click();
      await expect(editor).toHaveCount(0).catch(async error => { throw new Error(await editor.innerText(), { cause: error }); });
      await chat.getByRole('button', { name: 'Annotate drawing.excalidraw.png' }).click();
      await expect(editor.getByRole('button', { name: 'Attach drawing' })).toBeEnabled();
      await editor.getByRole('button', { name: 'Cancel' }).click();
      await chat.getByRole('button', { name: 'Remove drawing.excalidraw.png' }).click();
      const picture = await page.evaluate(() => {
        const canvas = document.createElement('canvas');
        canvas.width = 700; canvas.height = 480;
        const ctx = canvas.getContext('2d'); ctx.fillStyle = '#00bb66'; ctx.fillRect(0, 0, 700, 480);
        return canvas.toDataURL().split(',')[1];
      });
      await chat.locator('input[type=file]').setInputFiles({ name: 'original.png', mimeType: 'image/png', buffer: Buffer.from(picture, 'base64') });
      await chat.getByRole('button', { name: 'Annotate original.png' }).click();
      await drawRectangle();
      await expect(editor.getByRole('button', { name: 'Attach marks as edit mask' })).toBeHidden();
      await editor.getByText('Image generation options', { exact: true }).click();
      await editor.getByRole('button', { name: 'Attach marks as edit mask' }).click();
      const mask = chat.getByRole('button', { name: /^Open edit-mask.png/ }).locator('img');
      await expect(mask).toBeVisible();
      const alphaRange = await mask.evaluate(image => {
        const canvas = document.createElement('canvas');
        canvas.width = image.naturalWidth; canvas.height = image.naturalHeight;
        const ctx = canvas.getContext('2d'); ctx.drawImage(image, 0, 0);
        const pixels = ctx.getImageData(0, 0, canvas.width, canvas.height).data;
        let min = 255, max = 0;
        for (let i = 3; i < pixels.length; i += 4) { min = Math.min(min, pixels[i]); max = Math.max(max, pixels[i]); }
        return { min, max };
      });
      assert.ok(alphaRange.min < 255 && alphaRange.max === 255, 'mask distinguishes marked and preserved pixels');
      await chat.getByRole('button', { name: 'Remove edit-mask.png' }).click();
      await chat.getByRole('button', { name: 'Annotate original.png' }).click();
      await drawRectangle();
      await editor.getByRole('button', { name: 'Attach drawing' }).click();
      await expect(chat.getByRole('button', { name: 'Annotate original.png' })).toHaveCount(0);
      await chat.getByRole('button', { name: /^Open drawing.excalidraw.png/ }).hover();
      const composite = page.locator('.attachment-preview img');
      const colors = await composite.evaluate(async image => {
        await image.decode();
        const canvas = document.createElement('canvas');
        canvas.width = image.naturalWidth; canvas.height = image.naturalHeight;
        const ctx = canvas.getContext('2d');
        ctx.drawImage(image, 0, 0);
        const pixels = ctx.getImageData(0, 0, canvas.width, canvas.height).data;
        let background = false, stroke = false;
        for (let i = 0; i < pixels.length; i += 4) {
          background ||= pixels[i] < 20 && pixels[i + 1] > 150 && pixels[i + 2] < 130;
          stroke ||= pixels[i] > 150 && pixels[i + 1] < 120 && pixels[i + 2] < 120;
        }
        return { background, stroke };
      });
      assert.deepEqual(colors, { background: true, stroke: true });
      await chat.getByRole('button', { name: 'Remove drawing.excalidraw.png' }).click();
      drawingLimits = false;
      await page.reload();
      await expect(composer).toBeEnabled();
    }
    const originalUrl = page.url();
    // The owner's bearer is read from the fragment, then leaves the address bar and the history entry.
    assert.doesNotMatch(originalUrl, /token=/, `${view}: the bearer is still in the address bar`);
    assert.equal(await page.evaluate(() => sessionStorage.getItem('runner-token')), fixture.token, `${view}: the tab keeps the bearer`);
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
    // Shared limits: what fits is attached; the rest become error boxes that
    // say which limit, and leave the draft intact. Delete dismisses one.
    const kept = attached.locator('li.attachment-chip:not(.error)'), refused = attached.locator('li.error');
    await drop([text]);
    await drop([{ name: 'too-large', bytes: Array(1025).fill(1) }]);
    await expect(attached.getByRole('group', { name: 'too-large was not attached: Too large: 1.0 KiB of 1', exact: true })).toBeVisible();
    await expect(kept).toHaveCount(1);
    await attached.getByRole('button', { name: 'Dismiss too-large', exact: true }).focus();
    await page.keyboard.press('Delete');
    await expect(refused).toHaveCount(0);
    await expect(kept.getByRole('button', { name: /^Open notes\.txt/ }), 'focus moves to the box beside it').toBeFocused();
    await drop([text], { directory: true });
    await expect(chat.getByRole('alert')).toContainText('folders are not supported');
    await expect(kept).toHaveCount(1);
    // One held and three more fit the four-file limit; the fifth is refused.
    await drop(Array(4).fill(text));
    await expect(kept).toHaveCount(4);
    await expect(refused).toHaveCount(1);
    await expect(refused).toContainText('Too many: 4 files per message');
    // Backspace on a focused box removes it, and focus moves to the next.
    for (let i = 0; i < 3; i++) { await kept.last().getByRole('button', { name: /^Open notes\.txt/ }).focus(); await page.keyboard.press('Backspace'); }
    await expect(kept).toHaveCount(1);
    // The message total: the first fits beside the held file, the second would cross it.
    await drop([{ name: 'first', bytes: Array(800).fill(1) }, { name: 'second', bytes: Array(800).fill(2) }]);
    await expect(kept).toHaveCount(2);
    await expect(attached.getByRole('group', { name: 'second was not attached: Over the message limit: 1.6 KiB of 1.5', exact: true })).toBeVisible();
    await chat.getByRole('button', { name: 'Remove first', exact: true }).click();
    while (await refused.count()) await refused.first().getByRole('button', { name: /^Dismiss / }).click();
    await expect(kept).toHaveCount(1);
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
    // A reload signs in from the tab's session storage, and the address bar stays clean.
    await page.reload();
    await expect(composer).toBeEnabled();
    assert.doesNotMatch(page.url(), /token=/, `${view}: the bearer came back into the address bar`);
    await context.close();
  }
  const base = `http://127.0.0.1:${server.httpServer.address().port}`;
  const pasteText = (composer, text) => composer.evaluate((node, text) => {
    const data = new DataTransfer(); data.setData('text/plain', text);
    node.dispatchEvent(new ClipboardEvent('paste', { clipboardData: data, bubbles: true, cancelable: true }));
  }, text);
  const dropImageAndPdf = composer => composer.evaluate(async node => {
    const canvas = document.createElement('canvas'); canvas.width = 64; canvas.height = 48; canvas.getContext('2d').fillRect(0, 0, 64, 48);
    const png = await new Promise(resolve => canvas.toBlob(resolve, 'image/png'));
    const transfer = new DataTransfer();
    transfer.items.add(new File([png], 'chart.png', { type: 'image/png' }));
    transfer.items.add(new File(['%PDF-1.7\n2 0 obj << /Type /Pages /Count 2 >> endobj\n%\xe2\xe3'], 'spec.pdf', { type: 'application/pdf' }));
    node.dispatchEvent(new DragEvent('drop', { dataTransfer: transfer, bubbles: true, cancelable: true }));
  });

  // ── Independent of the agent: a Codex and an ACP conversation take the same
  // boxes, the Runner receives every file in order, and each bubble says how
  // its agent got them (an ACP agent without vision gets the image as a link).
  for (const [id, provider, profileId, images] of [['codex-files', 'codex', 'codex-default', true], ['acp-files', 'acp:gemini', 'acp-gemini', false]]) {
    fixture.sessions.push({ id, collectionId: 'alpha', workId: 'alpha', profileId, provider, workingDirectory: '/fixture/collections/alpha', status: 'ready', capabilities: { files: true, images, resume: true, livePermission: true }, permission: 'ask', pending: [] });
    fixture.openEvents(id);
    const context = await browser.newContext();
    const page = await context.newPage();
    await collectPageErrors(page, errors);
    await page.goto(`${base}/?view=chat&session=${id}&collection=alpha#token=${fixture.token}`);
    const chat = page.locator('.agent-window');
    const composer = chat.getByRole('textbox', { name: 'Message', exact: true });
    const attached = chat.getByRole('list', { name: 'Attached files' });
    await expect(composer).toBeEnabled();
    const log = Array.from({ length: 40 }, (_, i) => `2026-11-17T10:31:${String(i).padStart(2, '0')}Z INFO step ${i}`).join('\n');
    await pasteText(composer, log);
    await expect(attached.getByRole('button', { name: /^Open Pasted text · 40 lines/ })).toBeVisible();
    await expect(composer).toHaveValue('');
    await dropImageAndPdf(composer);
    await expect(attached.locator('li')).toHaveCount(3);
    await composer.fill(`Review these, ${provider}`); await composer.press('Enter');
    await expect(attached).toHaveCount(0);
    const apply = fixture.mutations.findLast(m => m.method === 'Apply' && m.body.sessionId === id).body;
    assert.equal(apply.message.text, `Review these, ${provider}`);
    assert.deepEqual(apply.message.attachments.map(file => [file.name.replace(/^Pasted text .*\.txt$/, 'Pasted text.txt'), file.mediaType]), [['Pasted text.txt', 'text/plain'], ['chart.png', 'image/png'], ['spec.pdf', 'application/pdf']], `${provider} receives every box, in order`);
    assert.equal(Buffer.from(apply.message.attachments[0].data, 'base64').toString(), log);
    const sent = chat.getByRole('list', { name: 'Sent attachments' }).last();
    await expect.poll(() => sent.locator('li').evaluateAll(items => items.map(item => item.dataset.kind))).toEqual(['text', 'image', 'pdf']);
    await expect(sent.getByRole('button', { name: /^Open Pasted text · 40 lines/ })).toHaveAttribute('title', 'The agent got its text');
    await expect(sent.getByRole('button', { name: /^Open chart\.png/ })).toHaveAttribute('title', images ? 'The agent saw it as an image' : 'The agent got a link to it, to open as needed');
    await expect(sent.getByRole('button', { name: /^spec\.pdf, PDF · / })).toHaveAttribute('title', 'The agent got a link to it, to open as needed');
    await context.close();
  }

  // ── The phone: boxes fit the width, a tap opens them, × removes them.
  {
    const context = await browser.newContext({ viewport: { width: 390, height: 844 }, hasTouch: true, isMobile: true });
    const page = await context.newPage();
    await collectPageErrors(page, errors);
    await page.goto(`${base}/?view=chat&session=alpha-session&collection=alpha#token=${fixture.token}`);
    const chat = page.locator('.agent-window');
    const composer = chat.getByRole('textbox', { name: 'Message', exact: true });
    const attached = chat.getByRole('list', { name: 'Attached files' });
    await expect(composer).toBeEnabled();
    await pasteText(composer, 'A long first line of a pasted stack trace that is wider than any phone\n' + '    at frame (file.js:1:1)\n'.repeat(30));
    await dropImageAndPdf(composer);
    await expect(attached.locator('li')).toHaveCount(3);
    for (const item of await attached.locator('li').all()) {
      const box = await item.boundingBox();
      assert.ok(box.x >= 0 && box.x + box.width <= 390, `a box stays on screen: ${JSON.stringify(box)}`);
    }
    assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1), 'no horizontal page scroll');
    await page.screenshot({ path: '.cache/attachment-phone.png' });
    await attached.getByRole('button', { name: /^Open Pasted text · 31 lines/ }).tap();
    const viewer = page.getByRole('dialog', { name: /^Pasted text / });
    await expect(viewer).toBeVisible();
    const sheet = await viewer.boundingBox();
    assert.ok(sheet.width <= 390 && sheet.x >= 0, `the viewer fits the phone: ${JSON.stringify(sheet)}`);
    await viewer.getByRole('button', { name: 'Cancel' }).tap();
    await expect(viewer).toHaveCount(0);
    await attached.getByRole('button', { name: /^Open chart\.png/ }).tap();
    await expect(page.getByRole('dialog', { name: 'chart.png' })).toBeVisible();
    await page.getByRole('button', { name: 'Close image' }).tap();
    await attached.getByRole('button', { name: /^spec\.pdf/ }).tap();
    await expect(page.getByRole('tooltip'), 'a tap on a PDF shows its details').toContainText('PDF · 2 pages');
    await attached.getByRole('button', { name: 'Remove spec.pdf' }).tap();
    await expect(attached.locator('li')).toHaveCount(2);
    await context.close();
  }
  await drainPageErrors(errors);assert.deepEqual(errors, []);
  console.log('PASS: standalone, detached and embedded chat file drops; exact text/binary payloads; retry identity; no auto-send; size/count/total limits; folder and ended-session rejection; text drops unaffected; pasted text, image and PDF boxes sent in order to Codex and ACP conversations with how each agent got them; the phone layout. Fake RPC, no credentials or inference.');
} finally {
  await browser?.close(); await server.close(); await fixture.close();
}
