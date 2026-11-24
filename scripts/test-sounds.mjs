// Fixture only (R14): notification sounds you choose. Settings → Notifications
// → Sounds: per kind, Lazybox's chime, a macOS system sound (desktop host,
// mocked here) or a file of your own, previewed when chosen; one volume; quiet
// while Lazybox is in front if asked. A chosen file plays, and every choice
// survives a reload. Screenshots in .cache/sounds-settings-{dark,light}.png.
// WebKit, the desktop app's engine.
import { testViteCache } from './fixtures/vite-cache.mjs';
import { createServer } from './fixtures/vite-cache.mjs';
import react from '@vitejs/plugin-react';
import { webkit, expect } from './fixtures/playwright.mjs';
import { fileURLToPath } from 'node:url';
import { mkdir } from 'node:fs/promises';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';
import { collectPageErrors } from './fixtures/page-errors.mjs';

/** A short sine beep as a 16-bit mono WAV. */
function wav(seconds = 0.15, rate = 8000) {
  const samples = Math.round(seconds * rate), data = Buffer.alloc(samples * 2);
  for (let i = 0; i < samples; i++) data.writeInt16LE(Math.round(Math.sin(2 * Math.PI * 880 * i / rate) * 12000), i * 2);
  const head = Buffer.alloc(44);
  head.write('RIFF', 0); head.writeUInt32LE(36 + data.length, 4); head.write('WAVE', 8); head.write('fmt ', 12);
  head.writeUInt32LE(16, 16); head.writeUInt16LE(1, 20); head.writeUInt16LE(1, 22); head.writeUInt32LE(rate, 24); head.writeUInt32LE(rate * 2, 28); head.writeUInt16LE(2, 32); head.writeUInt16LE(16, 34);
  head.write('data', 36); head.writeUInt32LE(data.length, 40);
  return Buffer.concat([head, data]);
}

const f = await startRunnerFixture();
const server = await createServer({ ...testViteCache(import.meta.url), configFile: false, logLevel: 'error', root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: f.url } } } });
let browser;
try {
  await server.listen(); await mkdir('.cache', { recursive: true });
  browser = await webkit.launch();
  const base = `http://127.0.0.1:${server.httpServer.address().port}/?view=workspace#token=${f.token}`;
  // What played: a file (a buffer source) or the chime (oscillators), with its gain.
  const listen = () => {
    window.__played = [];
    const buffer = AudioBufferSourceNode.prototype.start, tone = OscillatorNode.prototype.start;
    AudioBufferSourceNode.prototype.start = function (...args) { window.__played.push('file'); return buffer.apply(this, args); };
    OscillatorNode.prototype.start = function (...args) { if (!window.__toneAt || Date.now() - window.__toneAt > 50) window.__played.push('chime'); window.__toneAt = Date.now(); return tone.apply(this, args); };
  };
  const played = page => page.evaluate(() => window.__played.slice());

  for (const scheme of ['dark', 'light']) {
    const context = await browser.newContext({ viewport: { width: 1280, height: 900 }, colorScheme: scheme });
    await context.addInitScript(listen);
    const page = await context.newPage(); page.setDefaultTimeout(15000);
    const errors = []; await collectPageErrors(page, errors);
    const openSounds = async () => {
      await page.getByRole('button', { name: 'Settings', exact: true }).click();
      await page.getByRole('tab', { name: 'Notifications', exact: true }).click();
      await expect(page.getByRole('heading', { name: 'Sounds' })).toBeVisible();
    };
    await page.goto(base);
    await openSounds();
    const needsYou = page.getByRole('combobox', { name: 'Needs you sound' }), done = page.getByRole('combobox', { name: 'Done sound' });
    await expect(needsYou.locator('option:checked')).toHaveText('Lazybox chime');
    // In a browser there are no macOS sounds to offer.
    await expect(needsYou.locator('optgroup[label="macOS"]')).toHaveCount(0);
    const volume = page.getByRole('slider', { name: 'Volume' });
    await expect(volume).toHaveValue('50');

    // Preview: the chime plays.
    await page.getByRole('button', { name: 'Play the needs you sound' }).click();
    await expect.poll(() => played(page)).toContain('chime');

    if (scheme === 'dark') {
      // Your own file: chosen from the list, played at once, kept.
      const [chooser] = await Promise.all([page.waitForEvent('filechooser'), needsYou.selectOption('choose-file')]);
      await chooser.setFiles({ name: 'ding.wav', mimeType: 'audio/wav', buffer: wav() });
      await expect(needsYou.locator('option:checked')).toHaveText('ding.wav');
      await expect.poll(() => played(page)).toContain('file');
      // A file that is too long for an alert is refused, with why; the choice stays.
      await page.getByLabel('Done sound file').setInputFiles({ name: 'song.wav', mimeType: 'audio/wav', buffer: wav(40) });
      await expect(page.getByRole('alert').filter({ hasText: 'Choose a short sound' })).toBeVisible();
      await expect(done.locator('option:checked')).toHaveText('Lazybox chime');
      // A file that is not audio is refused too.
      await page.getByLabel('Done sound file').setInputFiles({ name: 'notes.txt', mimeType: 'text/plain', buffer: Buffer.from('hello') });
      await expect(page.getByRole('alert').filter({ hasText: 'Choose an audio file' })).toBeVisible();
      // Volume and quiet-in-front.
      await volume.fill('80');
      await expect(page.locator('output[for="sound-volume"]')).toHaveText('80%');
      await page.getByRole('switch', { name: /Only when Lazybox is in the background/ }).check();
      // Everything survives a reload.
      await page.reload();
      await openSounds();
      await expect(needsYou.locator('option:checked')).toHaveText('ding.wav');
      await expect(volume).toHaveValue('80');
      await expect(page.getByRole('switch', { name: /Only when Lazybox is in the background/ })).toBeChecked();
      await page.evaluate(() => { window.__played.length = 0; });
      await page.getByRole('button', { name: 'Play the needs you sound' }).click();
      await expect.poll(() => played(page)).toEqual(['file']);

      // With the desktop host (a stand-in): macOS sounds, played by the host at the volume.
      const url = await page.evaluate(() => performance.getEntriesByType('resource').map(e => e.name).find(n => n.includes('/sounds.ts')));
      assert.ok(url, 'the sounds module is loaded');
      await page.evaluate(async url => { const m = await import(url); window.__system = []; m.configureSystemSounds({ play: async (name, level) => { window.__system.push([name, level]); } }); }, url);
      await page.getByRole('tab', { name: 'Chat', exact: true }).click();
      await page.getByRole('tab', { name: 'Notifications', exact: true }).click();
      await expect(done.locator('optgroup[label="macOS"] option')).toHaveCount(14);
      // Done is off by default; its sound can still be chosen and heard before turning it on.
      await expect(page.getByRole('switch', { name: 'Done sound on' })).not.toBeChecked();
      await page.getByRole('switch', { name: 'Done sound on' }).check();
      await done.selectOption('system:Glass');
      await expect.poll(() => page.evaluate(() => window.__system.at(-1))).toEqual(['Glass', 0.8]);
      await expect(done.locator('option:checked')).toHaveText('Glass');
      // Turning a kind off keeps its choice.
      await page.getByRole('switch', { name: 'Done sound on' }).uncheck();
      await expect(done.locator('option:checked')).toHaveText('Glass');
    }
    await page.mouse.move(0, 0);
    await page.getByRole('heading', { name: 'Sounds' }).scrollIntoViewIfNeeded();
    await page.screenshot({ path: `.cache/sounds-settings-${scheme}.png` });
    assert.deepEqual(errors, [], `no page errors (${scheme})`);
    await context.close();
  }
  console.log('PASS: sounds per kind — chime, macOS system sound (host), your own file (refused when too long or not audio), preview, volume, quiet in front; kept across a reload. Fixture only.');
} finally { await browser?.close(); await server.close(); await f.close(); }
