// Fixture only: attached images as thumbnails. A real clipboard paste (an
// isolated headless clipboard, never the machine's), the hover and keyboard
// preview, the full-size lightbox, sent-message thumbnails read from the
// Runner only when they scroll into view, and every object URL revoked.
import {testViteCache} from './fixtures/vite-cache.mjs';
import { createServer } from './fixtures/vite-cache.mjs';
import react from '@vitejs/plugin-react';
import { chromium, webkit, expect } from './fixtures/playwright.mjs';
import { fileURLToPath } from 'node:url';
import { mkdir, readFile } from 'node:fs/promises';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';
import { collectContextPageErrors, collectPageErrors, drainPageErrors } from './fixtures/page-errors.mjs';

// Exercise the desktop image policy with the shared surface in Chromium.
const desktop = JSON.parse(await readFile(new URL('../apps/desktop/src-tauri/tauri.conf.json', import.meta.url), 'utf8'));
const imagePolicy = desktop.app.security.csp.split(';').find(directive => directive.trim().startsWith('img-src '));
assert.ok(imagePolicy, 'desktop declares an image policy');
const fixture = await startRunnerFixture();
const server = await createServer({...testViteCache(import.meta.url), configFile: false, logLevel: 'error', root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: fixture.url } } } });
const VIEWPORT = { width: 1280, height: 800 };
// Screenshots after the fade and scale-in, not halfway through them. A
// transition cancelled meanwhile (the dock's tab-strip scrollbar fades on a
// resize and is cut short by the next) has settled too: its finished rejects.
const settle = page => page.evaluate(() => Promise.all(document.getAnimations().filter(animation => animation.effect?.getTiming().iterations !== Infinity).map(animation => animation.finished.catch(() => {}))));
const overlaps = (a, b) => a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height;
const reads = path => fixture.calls.filter(call => call.method === 'ReadAttachment' && (!path || call.body.path === path)).length;
const data = value => Buffer.from(JSON.stringify(value)).toString('base64');
// Object URLs the page holds, so the test can tell every one is revoked.
const trackObjectUrls = () => {
  const live = new Set(), create = URL.createObjectURL.bind(URL), revoke = URL.revokeObjectURL.bind(URL);
  URL.createObjectURL = value => { const url = create(value); live.add(url); return url; };
  URL.revokeObjectURL = url => { live.delete(url); revoke(url); };
  window.liveObjectUrls = () => live.size;
};
// A 1600×900 PNG (a wide screenshot) from a canvas, put on the clipboard as an image.
const copyImage = page => page.evaluate(async () => {
  const canvas = document.createElement('canvas'); canvas.width = 1600; canvas.height = 900;
  const context = canvas.getContext('2d'), gradient = context.createLinearGradient(0, 0, 1600, 900);
  gradient.addColorStop(0, '#3b6fd8'); gradient.addColorStop(1, '#d8743b'); context.fillStyle = gradient; context.fillRect(0, 0, 1600, 900);
  const blob = await new Promise(resolve => canvas.toBlob(resolve, 'image/png'));
  await navigator.clipboard.write([new ClipboardItem({ 'image/png': blob })]);
  return blob.size;
});
const pngBytes = (page, width, height) => page.evaluate(async ([width, height]) => {
  const canvas = document.createElement('canvas'); canvas.width = width; canvas.height = height;
  const context = canvas.getContext('2d'); context.fillStyle = '#7ac27a'; context.fillRect(0, 0, width, height);
  return [...new Uint8Array(await (await new Promise(resolve => canvas.toBlob(resolve, 'image/png'))).arrayBuffer())];
}, [width, height]);

let browser;
try {
  await server.listen(); await mkdir('.cache', { recursive: true });
  const base = `http://127.0.0.1:${server.httpServer.address().port}`;
  browser = await chromium.launch({ headless: true });
  const errors = [];
  const open = async (view, session = 'alpha-session', collection = 'alpha') => {
    const context = await browser.newContext({ viewport: VIEWPORT, permissions: ['clipboard-read', 'clipboard-write'] });
    await collectContextPageErrors(context, errors);
    await context.addInitScript(trackObjectUrls);
    await context.route(base + '/?*', async route => {
      const response = await route.fetch();
      await route.fulfill({ response, headers: { ...response.headers(), 'content-security-policy': imagePolicy.trim() } });
    });
    const page = await context.newPage(); page.setDefaultTimeout(15000);
    await page.goto(`${base}/?view=${view}&session=${session}&collection=${collection}#token=${fixture.token}`);
    const chat = view === 'workspace' ? page.getByRole('region', { name: 'Agent chat', exact: true }) : page.locator('.agent-window');
    const composer = chat.getByRole('textbox', { name: 'Message', exact: true });
    await expect(composer).toBeEnabled();
    return { context, page, chat, composer, attached: chat.getByRole('list', { name: 'Attached files' }) };
  };

  // ── The embedded chat: everything, in detail.
  const alpha = fixture.openEvents('alpha-session');
  alpha.push({ type: 'message_delta', text: 'Ready when you are.', turnId: 't0' });
  const { context, page, chat, composer, attached } = await open('workspace');
  const urlsAtStart = await page.evaluate(() => window.liveObjectUrls());

  // A real paste of an image from the clipboard attaches it, and types nothing.
  await copyImage(page);
  await composer.focus(); await page.keyboard.press('ControlOrMeta+V');
  await expect(attached.locator('li')).toHaveCount(1);
  const thumb = attached.getByRole('button', { name: /^Open Pasted image \d{4}-\d\d-\d\d at \d\d\.\d\d\.\d\d\.png, [\d.]+ [KM]iB$/ });
  await expect(thumb).toBeEnabled();
  await expect(composer).toHaveValue('');
  const thumbImage = thumb.locator('img');
  await expect.poll(() => thumbImage.evaluate(img => img.complete && img.naturalWidth)).toBeGreaterThan(0);
  const natural = await thumbImage.evaluate(img => [img.naturalWidth, img.naturalHeight]);
  assert.ok(natural[0] <= 320 && natural[1] <= 320 && Math.abs(natural[0] / natural[1] - 16 / 9) < 0.02, `the chip shows a small copy of the right shape, not the 1600×900 original: ${natural}`);
  const box = await thumb.boundingBox();
  assert.ok(box.width >= 48 && box.width <= 64 && box.height >= 48 && box.height <= 64, `a 48–64 px thumbnail: ${JSON.stringify(box)}`);

  // A text paste stays a text paste.
  await page.evaluate(() => navigator.clipboard.writeText('see the screenshot'));
  await composer.focus(); await page.keyboard.press('ControlOrMeta+V');
  await expect(composer).toHaveValue('see the screenshot');
  await expect(attached.locator('li')).toHaveCount(1);

  // Resting the pointer shows it larger: fitted in 60% of the window, its shape kept.
  await thumb.hover();
  const preview = page.getByRole('tooltip');
  await expect(preview).toBeVisible();
  await expect(preview).toContainText('Pasted image');
  await expect(preview).toContainText('1600 × 900');
  await expect.poll(() => preview.locator('img').evaluate(img => img.naturalWidth)).toBe(1600);
  await settle(page);
  const shown = await preview.locator('img').boundingBox(), card = await preview.boundingBox();
  assert.ok(card.width <= VIEWPORT.width * 0.6 + 1 && card.height <= VIEWPORT.height * 0.6 + 1 && card.width >= VIEWPORT.width * 0.6 - 4, `the preview card uses 60% of the window: ${JSON.stringify(card)}`);
  assert.ok(Math.abs(shown.width / shown.height - 16 / 9) < 0.02, `at the image's aspect ratio: ${JSON.stringify(shown)}`);
  assert.ok(card.x >= 0 && card.y >= 0 && card.x + card.width <= VIEWPORT.width && card.y + card.height <= VIEWPORT.height, 'the preview stays inside the window');
  assert.ok(card.y + card.height <= box.y, 'above the composer, not over the thumbnail');
  assert.equal(await preview.evaluate(node => getComputedStyle(node).pointerEvents), 'none', 'the preview never takes the pointer');
  await page.screenshot({ path: '.cache/attachment-preview-hover.png' });
  await page.mouse.move(5, 5);
  await expect(preview).toHaveCount(0);

  // Keyboard: focus shows it, Esc hides it, Enter opens the lightbox, Esc closes it.
  await composer.focus(); await page.keyboard.press('Shift+Tab'); await page.keyboard.press('Shift+Tab');
  await expect(thumb).toBeFocused();
  await expect(preview).toBeVisible();
  await page.keyboard.press('Escape');
  await expect(preview).toHaveCount(0);
  await expect(thumb).toBeFocused();
  await page.keyboard.press('Enter');
  const lightbox = page.getByRole('dialog', { name: /^Pasted image/ });
  await expect(lightbox).toBeVisible();
  await expect(lightbox.getByRole('img')).toBeVisible();
  assert.deepEqual(await lightbox.getByRole('img').evaluate(img => [img.naturalWidth, img.naturalHeight]), [1600, 900], 'the lightbox shows the original');
  await expect(lightbox).toContainText('1600 × 900');
  await settle(page); await page.screenshot({ path: '.cache/attachment-lightbox.png' });
  await page.keyboard.press('Escape');
  await expect(lightbox).toHaveCount(0);
  await expect(thumb).toBeFocused();
  await expect(attached.locator('li'), 'Esc closed only the lightbox').toHaveCount(1);
  // A click opens it; a click beside the picture closes it.
  await thumb.click();
  await expect(lightbox).toBeVisible();
  await page.mouse.click(10, VIEWPORT.height / 2);
  await expect(lightbox).toHaveCount(0);
  await thumb.click(); await lightbox.getByRole('button', { name: 'Close image' }).click();
  await expect(lightbox).toHaveCount(0);

  // Reduced motion: nothing animates.
  await page.emulateMedia({ reducedMotion: 'reduce' });
  await thumb.hover(); await expect(preview).toBeVisible();
  assert.equal(await preview.evaluate(node => getComputedStyle(node).animationName), 'none');
  await page.mouse.move(5, 5); await page.emulateMedia({ reducedMotion: 'no-preference' });

  // Removing it revokes what it made.
  await chat.getByRole('button', { name: /^Remove Pasted image/ }).click();
  await expect(attached).toHaveCount(0);
  await expect.poll(() => page.evaluate(() => window.liveObjectUrls())).toBe(urlsAtStart);

  // A file that claims to be an image but does not decode is a named chip, not a broken picture.
  await composer.evaluate(node => {
    const transfer = new DataTransfer(); transfer.items.add(new File(['not a png'], 'broken.png', { type: 'image/png' }));
    node.dispatchEvent(new DragEvent('drop', { dataTransfer: transfer, bubbles: true, cancelable: true }));
  });
  await expect(attached.locator('li')).toHaveCount(1);
  await expect(attached.locator('li')).toContainText('broken.png');
  await expect(attached.getByRole('button', { name: /^Open broken/ })).toHaveCount(0);
  await chat.getByRole('button', { name: 'Remove broken.png' }).click();
  await expect(attached).toHaveCount(0);

  // Send an image and a text file: the message shows the image as a thumbnail
  // and the text file as a box with its lines; each is read back once.
  await copyImage(page); await composer.focus(); await page.keyboard.press('ControlOrMeta+V');
  await expect(attached.locator('li')).toHaveCount(1);
  await composer.evaluate(node => {
    const transfer = new DataTransfer(); transfer.items.add(new File(['notes'], 'notes.txt', { type: 'text/plain' }));
    node.dispatchEvent(new DragEvent('drop', { dataTransfer: transfer, bubbles: true, cancelable: true }));
  });
  await expect(attached.locator('li')).toHaveCount(2);
  await expect(attached.locator('li').nth(1)).toContainText('notes.txt');
  await composer.fill('What is wrong in this screenshot?'); await composer.press('Enter');
  await expect(attached).toHaveCount(0);
  const sent = chat.getByRole('list', { name: 'Sent attachments' }).last();
  const sentThumb = sent.getByRole('button', { name: /^Open Pasted image .*\.png, [\d.]+ [KM]iB$/ });
  await expect(sentThumb).toBeEnabled();
  await expect(sent).toContainText('notes.txt');
  await expect(sent.getByRole('button', { name: 'Open notes.txt, 1 line · 5 B' })).toBeVisible();
  const apply = fixture.mutations.findLast(m => m.method === 'Apply').body;
  const sentPath = [...fixture.attachments.keys()].find(path => path.includes(apply.message.attachments[0].name));
  // The clipboard re-encodes what it holds; what is sent is the PNG the paste delivered.
  const sentBytes = Buffer.from(apply.message.attachments[0].data, 'base64');
  assert.deepEqual([...sentBytes.subarray(0, 8)], [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a], 'a PNG is sent');
  assert.deepEqual([sentBytes.readUInt32BE(16), sentBytes.readUInt32BE(20)], [1600, 900], 'at its full size');
  assert.equal(apply.message.attachments[0].mediaType, 'image/png');
  assert.equal(reads(sentPath), 1, 'the sent image is read once');
  assert.equal(fixture.calls.filter(call => call.method === 'ReadAttachment' && call.body.path.endsWith('notes.txt')).length, 1, 'a text file is read once, for its preview');
  const sentBox = await sentThumb.boundingBox();
  assert.ok(Math.abs(sentBox.height - 90) <= 2 && sentBox.width > sentBox.height, `a sent screenshot keeps its wide shape: ${JSON.stringify(sentBox)}`);
  await sentThumb.hover(); await expect(preview).toBeVisible(); await expect(preview).toContainText('1600 × 900');
  await settle(page);
  assert.ok(!overlaps(await preview.boundingBox(), await sentThumb.boundingBox()), 'the preview never covers its thumbnail');
  await page.screenshot({ path: '.cache/attachment-sent-preview.png' });
  await sentThumb.click(); await expect(lightbox).toBeVisible();
  await page.keyboard.press('Escape'); await expect(lightbox).toHaveCount(0);
  assert.equal(reads(sentPath), 1, 'hover and lightbox reuse the loaded image');
  // Composer URLs are gone; the sent image holds its two (original and thumbnail).
  await expect.poll(() => page.evaluate(() => window.liveObjectUrls())).toBe(urlsAtStart + 2);

  // An attachment the Runner no longer has (a deleted conversation's) is a named box.
  alpha.push({ type: 'operation_accepted', text: 'An old one', providerDataJson: data({ files: [{ name: 'gone.png', mediaType: 'image/png', size: 2048, path: '/fixture/attachments/alpha-session/' + '0'.repeat(64) + '/gone.png' }] }) });
  const gone = chat.locator('.sent-files li', { hasText: 'gone.png' });
  await expect(gone).toBeVisible();
  await expect(gone).toHaveAttribute('title', /Preview unavailable/);
  await context.close();

  // ── A large paste is a box: preview on hover, ⌘Z and Insert inline put it
  // back, the viewer edits it, it survives a reload,
  // and it is sent as a text file the bubble shows as the same box.
  {
    const python = ['def build_report(rows):', '    """Totals per row."""', ...Array.from({ length: 135 }, (_, i) => `    total_${i + 1} = compute(rows, ${i + 1})`), '', 'def compute(rows, n):', '    return sum(r[n] for r in rows)', '', 'import os'].join('\n');
    assert.equal(python.split('\n').length, 142);
    const { context, page, chat, composer, attached } = await open('workspace');
    const paste = async () => { await page.evaluate(text => navigator.clipboard.writeText(text), python); await composer.focus(); await page.keyboard.press('ControlOrMeta+V'); };
    const box = attached.getByRole('button', { name: /^Open Pasted Python · 142 lines/ });
    await composer.fill('Explain this: ');
    await composer.evaluate(node => node.setSelectionRange(node.value.length, node.value.length));
    await paste();
    await expect(box).toBeVisible();
    await expect(composer, 'the paste is a box, not text in the message').toHaveValue('Explain this: ');
    await expect(box).toContainText('def build_report(rows):');
    await expect(box).toContainText('Pasted Python · 142 lines');
    const boxRect = await box.boundingBox();
    assert.ok(Math.abs(boxRect.height - 54) <= 3, `a box is as tall as an image thumbnail: ${JSON.stringify(boxRect)}`);

    // Hover: a monospace preview that takes the pointer and scrolls.
    await box.hover();
    const preview = page.getByRole('tooltip');
    await expect(preview).toBeVisible();
    await expect(preview).toContainText('total_135 = compute(rows, 135)');
    await expect(preview).toContainText('Python');
    const pre = preview.locator('pre');
    assert.match(await pre.evaluate(node => getComputedStyle(node).fontFamily), /mono|Menlo|SFMono/i);
    assert.ok(await pre.evaluate(node => node.scrollHeight > node.clientHeight), 'the preview scrolls');
    await settle(page);
    const card = await preview.boundingBox();
    assert.ok(card.y + card.height <= boxRect.y, 'above the composer');
    await page.mouse.move(card.x + card.width / 2, card.y + card.height / 2);
    await page.mouse.wheel(0, 400);
    await expect.poll(() => pre.evaluate(node => node.scrollTop)).toBeGreaterThan(0);
    await expect(preview, 'the pointer on the preview keeps it open').toBeVisible();
    await page.screenshot({ path: '.cache/attachment-paste-preview.png' });
    await page.mouse.move(5, 5);
    await expect(preview).toHaveCount(0);

    // ⌘Z right after: the text goes in the message, where it was pasted.
    await composer.focus(); await page.keyboard.press('ControlOrMeta+Z');
    await expect(attached).toHaveCount(0);
    await expect(composer).toHaveValue('Explain this: ' + python);

    // ⋯ → Insert inline does the same later.
    await composer.fill(''); await paste();
    await expect(box).toBeVisible();
    await composer.fill('Typed after the paste');
    await composer.press('ControlOrMeta+Z');
    await expect(box, '⌘Z after more typing is the editor’s own undo, not the box').toBeVisible();
    await composer.fill('');
    await attached.getByLabel('More for the pasted text', { exact: true }).click();
    await page.getByRole('group', { name: 'More for the pasted text menu' }).getByRole('button', { name: 'Insert inline' }).click();
    await expect(attached).toHaveCount(0);
    await expect(composer).toHaveValue(python);

    // This paste fits the Runner but would overflow native draft storage when JSON-escaped.
    const dense = 'é"'.repeat(95000);
    await composer.fill('');
    await page.evaluate(text => navigator.clipboard.writeText(text), dense);
    await composer.focus(); await page.keyboard.press('ControlOrMeta+V');
    await expect(attached.getByRole('button', {name: /^Open Pasted text/}).first()).toBeVisible();
    // Metadata is written synchronously. Reload once a second reader can see the
    // exact pasted file, proving that its separate IndexedDB transaction committed.
    await expect.poll(() => page.evaluate(async () => {
      const draft = Object.entries(localStorage).find(([key, value]) => key.includes(':draft:') && value.includes('Pasted text'));
      if (!draft) return false;
      const refs = JSON.parse(draft[1]).files;
      if (refs.length !== 1 || !refs[0].blobId) return false;
      if (!(await indexedDB.databases()).some(db => db.name === 'lazybox-composer-files')) return false;
      return new Promise((resolve, reject) => {
        const open = indexedDB.open('lazybox-composer-files');
        open.onerror = () => reject(open.error);
        open.onsuccess = () => {
          const db = open.result;
          const request = db.transaction('drafts').objectStore('drafts').get(draft[0]);
          request.onsuccess = () => resolve(request.result?.some(entry => entry.id === refs[0].blobId && typeof entry.text === 'string') ?? false);
          request.onerror = () => reject(request.error);
          db.close();
        };
      });
    })).toBe(true);
    await page.reload();
    await attached.getByLabel('More for the pasted text', {exact:true}).click();
    await page.getByRole('group', {name:'More for the pasted text menu'}).getByRole('button', {name:'Insert inline'}).click();
    assert.equal(await composer.inputValue(), dense);
    // A click opens it in the viewer, where it can be edited; Save keeps the edit.
    await composer.fill(''); await paste();
    await box.click();
    const viewer = page.getByRole('dialog', { name: /^Pasted text .*\.py$/ });
    await expect(viewer).toBeVisible();
    const field = viewer.getByRole('textbox', { name: /^Text of Pasted text/ });
    await expect(field).toHaveValue(python);
    await field.evaluate(node => node.setSelectionRange(node.value.length, node.value.length)); await field.pressSequentially('\nprint(os.getcwd())');
    await settle(page); await page.screenshot({ path: '.cache/attachment-text-viewer.png' });
    await viewer.getByRole('button', { name: 'Save' }).click();
    await expect(viewer).toHaveCount(0);
    const edited = attached.getByRole('button', { name: /^Open Pasted Python · 143 lines/ });
    await expect(edited).toBeFocused();
    // Esc leaves it unchanged.
    await edited.press('Enter'); await expect(viewer).toBeVisible();
    await viewer.getByRole('textbox').fill('discarded');
    await page.keyboard.press('Escape');
    await expect(viewer).toHaveCount(0);
    await expect(edited).toBeVisible();

    // A reload keeps the box: its text is in the saved draft, not lost with the window.
    await page.reload();
    await expect(composer).toBeEnabled();
    await expect(attached.getByRole('button', { name: /^Open Pasted Python · 143 lines/ })).toBeVisible();
    await expect(chat.getByRole('group', { name: 'Files to attach again' })).toHaveCount(0);

    // Sent: the agent's message carries the text as a file; the bubble shows the same box.
    await composer.fill('What does this do?'); await composer.press('Enter');
    await expect(attached).toHaveCount(0);
    const apply = fixture.mutations.findLast(m => m.method === 'Apply').body;
    assert.equal(apply.message.text, 'What does this do?', 'the person’s text is unchanged');
    assert.equal(apply.message.attachments.length, 1);
    assert.match(apply.message.attachments[0].name, /^Pasted text \d{4}-\d\d-\d\d at \d\d\.\d\d\.\d\d\.py$/);
    assert.equal(apply.message.attachments[0].mediaType, 'text/plain');
    assert.equal(Buffer.from(apply.message.attachments[0].data, 'base64').toString(), python + '\nprint(os.getcwd())');
    const sentBox = chat.getByRole('list', { name: 'Sent attachments' }).last().getByRole('button', { name: /^Open Pasted Python · 143 lines/ });
    await expect(sentBox).toBeVisible();
    await expect(sentBox).toContainText('def build_report(rows):');
    await expect(sentBox).toHaveAttribute('title', 'The agent got its text');
    await sentBox.hover();
    await expect(page.getByRole('tooltip')).toContainText('print(os.getcwd())');
    await page.mouse.move(5, 5);
    await sentBox.click();
    const sentViewer = page.getByRole('dialog', { name: /^Pasted text .*\.py$/ });
    await expect(sentViewer.getByText('print(os.getcwd())', { exact: false })).toBeVisible();
    await expect(sentViewer.getByRole('textbox'), 'a sent box is read only').toHaveCount(0);
    await page.keyboard.press('Escape');
    await expect(sentViewer).toHaveCount(0);
    await context.close();
  }

  // ── Image, PDF and any other file: one box each, a preview on hover or
  // focus, removed and moved from the keyboard.
  {
    const { context, page, composer, attached } = await open('workspace');
    const pdf = Buffer.from('%PDF-1.7\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n2 0 obj << /Type /Pages /Kids [3 0 R 4 0 R 5 0 R] /Count 3 >> endobj\n%\xe2\xe3\xcf\xd3\n', 'latin1');
    await composer.evaluate(async (node, pdf) => {
      const canvas = document.createElement('canvas'); canvas.width = 400; canvas.height = 300; canvas.getContext('2d').fillRect(0, 0, 400, 300);
      const png = await new Promise(resolve => canvas.toBlob(resolve, 'image/png'));
      const transfer = new DataTransfer();
      transfer.items.add(new File([png], 'diagram.png', { type: 'image/png' }));
      transfer.items.add(new File([new Uint8Array(pdf)], 'report.pdf', { type: 'application/pdf' }));
      transfer.items.add(new File([new Uint8Array([0, 1, 2, 3])], 'archive.bin', { type: '' }));
      node.dispatchEvent(new DragEvent('drop', { dataTransfer: transfer, bubbles: true, cancelable: true }));
    }, [...pdf]);
    const kinds = () => attached.locator('li').evaluateAll(items => items.map(item => item.dataset.kind));
    await expect.poll(kinds).toEqual(['image', 'pdf', 'file']);
    const pdfBox = attached.getByRole('button', { name: /^report\.pdf, PDF · 3 pages · / });
    await expect(pdfBox).toBeVisible();
    const fileBox = attached.getByRole('button', { name: 'archive.bin, 4 B' });
    // Every box is the same height.
    const heights = await attached.locator('li').evaluateAll(items => items.map(item => Math.round(item.getBoundingClientRect().height)));
    assert.ok(Math.max(...heights) - Math.min(...heights) <= 2, `consistent sizes: ${heights}`);
    await settle(page); await page.screenshot({ path: '.cache/attachment-boxes.png' });
    // Hover a PDF: its pages and size. Focus a file: its details; Enter keeps them open.
    await pdfBox.hover();
    await expect(page.getByRole('tooltip')).toContainText('PDF · 3 pages');
    await page.mouse.move(5, 5);
    await expect(page.getByRole('tooltip')).toHaveCount(0);
    await fileBox.focus(); await page.keyboard.press('Shift+Tab'); await page.keyboard.press('Tab');
    await expect(fileBox).toBeFocused();
    await expect(page.getByRole('tooltip')).toContainText('archive.bin');
    await page.keyboard.press('Escape');
    await expect(page.getByRole('tooltip')).toHaveCount(0);
    await page.keyboard.press('Enter');
    await expect(fileBox).toHaveAttribute('aria-expanded', 'true');
    await expect(page.getByRole('tooltip')).toBeVisible();
    await page.keyboard.press('Escape');
    // Alt+← moves a box; dragging one onto another does too.
    await page.keyboard.press('Alt+ArrowLeft');
    await expect.poll(kinds).toEqual(['image', 'file', 'pdf']);
    await expect(fileBox).toBeFocused();
    await attached.locator('li[data-kind=pdf]').dragTo(attached.locator('li[data-kind=image]'));
    await expect.poll(kinds).toEqual(['pdf', 'image', 'file']);
    // Delete removes the focused box, whatever its kind, and focus stays in the row.
    await fileBox.focus(); await page.keyboard.press('Delete');
    await expect.poll(kinds).toEqual(['pdf', 'image']);
    const thumb = attached.getByRole('button', { name: /^Open diagram\.png/ });
    await expect(thumb).toBeFocused();
    await page.keyboard.press('Backspace');
    await expect.poll(kinds).toEqual(['pdf']);
    await expect(pdfBox).toBeFocused();
    await page.keyboard.press('Delete');
    await expect(attached).toHaveCount(0);
    await expect(composer).toBeFocused();
    await context.close();
  }

  // ── Lazy loading: an image far up the conversation is read only when scrolled to.
  const beta = fixture.openEvents('beta-session');
  const oldPath = `/fixture/attachments/beta-session/${'1'.repeat(64)}/early.png`, newPath = `/fixture/attachments/beta-session/${'2'.repeat(64)}/late.png`;
  {
    const scratch = await browser.newPage();
    await scratch.route('http://localhost/**', route => route.fulfill({ contentType: 'text/html', body: '' })); await scratch.goto('http://localhost/');
    fixture.attachments.set(oldPath, { name: 'early.png', mediaType: 'image/png', data: Buffer.from(await pngBytes(scratch, 300, 600)) });
    fixture.attachments.set(newPath, { name: 'late.png', mediaType: 'image/png', data: Buffer.from(await pngBytes(scratch, 640, 480)) });
    await scratch.close();
  }
  beta.push({ type: 'operation_accepted', text: 'The first screenshot', turnId: 'b0', providerDataJson: data({ files: [{ name: 'early.png', mediaType: 'image/png', size: 100, path: oldPath }] }) });
  for (let i = 1; i <= 30; i++) beta.push({ type: 'message_delta', turnId: 'b' + i, text: `Answer ${i}.\n\n` + 'A long paragraph of findings. '.repeat(30) }), beta.push({ type: 'turn_ended', turnId: 'b' + i });
  beta.push({ type: 'operation_accepted', text: 'The latest screenshot', turnId: 'b99', providerDataJson: data({ files: [{ name: 'late.png', mediaType: 'image/png', size: 100, path: newPath }] }) });
  {
    const { context, page, chat } = await open('workspace', 'beta-session', 'beta');
    await expect(chat.getByRole('button', { name: /^Open late\.png/ })).toBeEnabled();
    await page.waitForTimeout(300);
    assert.equal(reads(oldPath), 0, 'an image out of view is not read');
    const pane = chat.locator('.messages:visible');
    for (let i = 0; i < 20 && !(await chat.getByRole('button', { name: /^Open early\.png/ }).count()); i++) {
      await pane.evaluate(node => { node.scrollTop = 0; });
      const earlier = chat.getByRole('button', { name: /^Show earlier messages/ });
      if (await earlier.count()) await earlier.click();
      await page.waitForTimeout(100);
    }
    const early = chat.getByRole('button', { name: /^Open early\.png/ });
    await early.scrollIntoViewIfNeeded();
    await expect(early).toBeEnabled();
    assert.equal(reads(oldPath), 1, 'scrolled into view, it is read once');
    const tall = await early.boundingBox();
    assert.ok(tall.width < tall.height, `a tall image keeps a tall shape: ${JSON.stringify(tall)}`);
    await context.close();
  }

  // ── The detached chat window and the standalone agent window: paste, send, see it.
  for (const view of ['chat', 'agent']) {
    const { context, page, chat, composer, attached } = await open(view);
    await copyImage(page); await composer.focus(); await page.keyboard.press('ControlOrMeta+V');
    await expect(attached.getByRole('button', { name: /^Open Pasted image/ })).toBeEnabled();
    await attached.getByRole('button', { name: /^Open Pasted image/ }).hover();
    await expect(page.getByRole('tooltip')).toBeVisible();
    await page.mouse.move(5, 5);
    await composer.fill(`From the ${view} window`); await composer.press('Enter');
    await expect(attached).toHaveCount(0);
    const sentThumb = chat.getByRole('list', { name: 'Sent attachments' }).last().getByRole('button', { name: /^Open Pasted image/ });
    await expect(sentThumb).toBeEnabled();
    await sentThumb.click();
    await expect(page.getByRole('dialog', { name: /^Pasted image/ })).toBeVisible();
    await page.keyboard.press('Escape');
    await expect(page.getByRole('dialog', { name: /^Pasted image/ })).toHaveCount(0);
    await context.close();
  }

  // ── WebKit, the desktop app's engine: a dropped image, its preview and lightbox.
  {
    const engine = await webkit.launch();
    try {
      const page = await engine.newPage({ viewport: VIEWPORT }); page.setDefaultTimeout(15000);
      await collectPageErrors(page, errors, { label: 'webkit' });
      await page.goto(`${base}/?view=chat&session=alpha-session&collection=alpha#token=${fixture.token}`);
      const composer = page.getByRole('textbox', { name: 'Message', exact: true });
      await expect(composer).toBeEnabled();
      await composer.evaluate(async node => {
        const canvas = document.createElement('canvas'); canvas.width = 800; canvas.height = 600; canvas.getContext('2d').fillRect(0, 0, 800, 600);
        const blob = await new Promise(resolve => canvas.toBlob(resolve, 'image/png'));
        const transfer = new DataTransfer(); transfer.items.add(new File([blob], 'diagram.png', { type: 'image/png' }));
        node.dispatchEvent(new DragEvent('drop', { dataTransfer: transfer, bubbles: true, cancelable: true }));
      });
      const thumb = page.getByRole('button', { name: /^Open diagram\.png/ });
      await expect(thumb).toBeEnabled();
      await expect.poll(() => thumb.locator('img').evaluate(img => img.complete && img.naturalWidth)).toBeGreaterThan(0);
      await thumb.hover(); await expect(page.getByRole('tooltip')).toContainText('800 × 600');
      await thumb.click(); await expect(page.getByRole('dialog', { name: 'diagram.png' })).toBeVisible();
      await page.keyboard.press('Escape'); await expect(page.getByRole('dialog', { name: 'diagram.png' })).toHaveCount(0);
    } finally { await engine.close(); }
  }

  await drainPageErrors(errors);assert.deepEqual(errors, []);
  console.log('PASS: pasted/dropped images as thumbnails, text paste unaffected, hover and keyboard preview fitted to 60% of the window, lightbox with Esc and focus return, reduced motion, object URLs revoked, sent thumbnails read once and only when in view, unavailable attachments as named chips, in the workspace, detached chat and agent windows, and in WebKit. Fake RPC, isolated clipboard.');
} finally {
  await browser?.close(); await server.close(); await fixture.close();
}
