import {testViteCache} from './fixtures/vite-cache.mjs';
import { createServer } from './fixtures/vite-cache.mjs';
import react from '@vitejs/plugin-react';
import { webkit, chromium, expect } from './fixtures/playwright.mjs';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';
import { monacoVimCompat } from './monaco-vim-compat.mjs';
import { collectPageErrors, drainPageErrors } from './fixtures/page-errors.mjs';

// Keys that land a millisecond apart (Playwright's fast type(), fast typing, a
// paste then typing, dictation streaming words, an IME) never loop the message
// box's updates ("Maximum update depth exceeded"), and the box ends with
// exactly what was typed. The page's main thread is kept busy meanwhile, so
// keys queue up and land in bursts, as on a loaded machine: a render React
// queues per key (a setState in an effect keyed on the text) then never gets
// to run between keys, and React gives up after 50 of them.
const fixture = await startRunnerFixture();
// A hot reload during keyboard.type destroys the focused box halfway through a burst.
// This suite exercises explicit reloads only.
const server = await createServer({...testViteCache(import.meta.url), configFile: false, root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react(), monacoVimCompat()], server: { watch: null, host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: fixture.url } } } });
const browsers = [];
const rounds = Number(process.env.ROUNDS || 3);
const words = 'the quick brown fox jumps over the lazy dog 0123456789 ';
const text = words.repeat(Math.ceil(500 / words.length)).slice(0, 500);
try {
  await server.listen();
  const url = `http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`;
  for (const engine of [chromium, webkit]) {
    const browser = await engine.launch({ headless: true }); browsers.push(browser);
    // Vim editing is kept in keys.yaml: each engine starts without it.
    fixture.config.remove('keys');
    const page = await browser.newPage({ viewport: { width: 1300, height: 850 } });
    page.setDefaultTimeout(30000);
    const errors = await collectPageErrors(page);
    page.on('console', message => { if (message.type() === 'error' && /Maximum update depth/.test(message.text())) errors.push(message.text()); });
    await page.goto(url);
    const message = page.getByRole('textbox', { name: 'Message', exact: true });
    await expect(message).toBeEnabled();

    // Reproduce an unrelated commit between the browser changing the value
    // and React receiving its input event. An attachment renders the composer.
    await message.fill('before ');
    await message.evaluate(box => box.setRangeText('input', box.value.length, box.value.length, 'end'));
    await page.locator('input[type="file"][aria-label="Attach files"]').setInputFiles({ name: 'sample.txt', mimeType: 'text/plain', buffer: Buffer.from('test') });
    const remove = page.getByRole('button', { name: 'Remove sample.txt', exact: true });
    await expect(remove).toBeVisible();
    await expect(message).toHaveValue('before input');
    await message.dispatchEvent('input', { bubbles: true });
    await remove.click();

    /** Clears the box, runs `input` with the main thread mostly busy, and checks the result. */
    const burst = async (label, input, expected) => {
      for (let round = 0; round < rounds; round++) {
        await message.fill('');
        await message.focus();
        await page.evaluate(() => { const busy = () => { const until = performance.now() + 12; while (performance.now() < until); }; window.__load = setInterval(busy, 15); });
        try { await input(); } finally { await page.evaluate(() => clearInterval(window.__load)); }
        await expect(message, `${engine.name()} ${label}, round ${round + 1}`).toHaveValue(expected);
        await drainPageErrors(errors);assert.deepEqual(errors, [], `${engine.name()} ${label}, round ${round + 1}: no update loop`);
      }
    };

    await burst('500 keys with no delay', () => page.keyboard.type(text, { delay: 0 }), text);
    // A blank box: what the chat clears when the draft empties runs on every key.
    await burst('300 spaces', () => page.keyboard.type(' '.repeat(300), { delay: 0 }), ' '.repeat(300));
    // A command being typed: the slash menu is open throughout.
    await burst('a slash command', () => page.keyboard.type('/' + text.slice(0, 300), { delay: 0 }), '/' + text.slice(0, 300));
    // Esc closes the menu; typing on keeps it closed and still lands every key.
    await burst('a dismissed slash menu', async () => { await page.keyboard.type('/mo', { delay: 0 }); await page.keyboard.press('Escape'); await page.keyboard.type(text.slice(0, 300), { delay: 0 }); }, '/mo' + text.slice(0, 300));
    // Dictation and paste write whole words at once, back to back.
    await burst('streamed words', async () => { for (const word of text.split(/(?<= )/)) await page.keyboard.insertText(word); }, text);
    await burst('a paste, then typing', async () => { await page.keyboard.insertText(text.slice(0, 250)); await page.keyboard.type(text.slice(250), { delay: 0 }); }, text);
    if (engine === chromium) {
      // An IME: each key updates a composition, and the end commits the word.
      const cdp = await page.context().newCDPSession(page);
      await burst('IME composition', async () => {
        for (const word of text.slice(0, 200).split(/(?<= )/)) {
          for (let i = 1; i <= word.length; i++) await cdp.send('Input.imeSetComposition', { text: word.slice(0, i), selectionStart: i, selectionEnd: i });
          await cdp.send('Input.insertText', { text: word });
        }
      }, text.slice(0, 200));
    }

    // Vim editing (Settings → Shortcuts): insert mode takes fast keys the same way.
    await page.getByRole('button', { name: 'Settings', exact: true }).click();
    await page.getByRole('tab', { name: 'Shortcuts' }).click();
    await page.getByRole('switch', { name: 'Vim editing in the message box' }).check();
    await page.getByRole('navigation', { name: 'Pages' }).getByRole('button', { name: 'Work', exact: true }).click();
    await expect(message).toBeEnabled();
    await burst('Vim insert mode', async () => { await expect(page.locator('.vim-mode-chip')).toHaveText('INSERT'); await page.keyboard.type(text, { delay: 0 }); }, text);

    // The draft is still saved: it survives a reload.
    await message.fill('');
    await message.focus();
    await page.keyboard.type('kept draft', { delay: 0 });
    await page.waitForTimeout(200);
    await page.reload();
    await expect(page.getByRole('textbox', { name: 'Message', exact: true })).toHaveValue('kept draft');
    await drainPageErrors(errors);assert.deepEqual(errors, [], `${engine.name()}: no page error`);
  }
  console.log(`PASS: under load, ${rounds} rounds each of 500 keys with no delay, blank keys, a slash command (open and dismissed), streamed words, a paste then typing, IME composition (Chromium) and Vim insert mode, in Chromium and WebKit: every key lands, no update loop, and the draft survives a reload.`);
} finally {
  for (const browser of browsers) await browser.close();
  await server.close(); await fixture.close();
}
