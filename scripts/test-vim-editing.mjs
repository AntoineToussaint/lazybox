import {testViteCache} from './fixtures/vite-cache.mjs';
import { createServer } from './fixtures/vite-cache.mjs';
import react from '@vitejs/plugin-react';
import { webkit, chromium, expect } from './fixtures/playwright.mjs';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';
import { monacoVimCompat } from './monaco-vim-compat.mjs';
import { collectPageErrors } from './fixtures/page-errors.mjs';

// Opt-in Vim editing in the message box and the Monaco file editor, against
// the fake Runner fixture. WebKit is the desktop shell's engine; Chromium
// covers the browser client.
const fixture = await startRunnerFixture();
const channel = fixture.openEvents('alpha-session');
channel.push({ type: 'message_delta', text: 'Open [README](api/README.md).' });
const server = await createServer({...testViteCache(import.meta.url), configFile: false, root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react(), monacoVimCompat()], server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: fixture.url } } } });
const browsers = [];
try {
  await server.listen();
  const url = `http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`;
  for (const engine of [webkit, chromium]) {
    const browser = await engine.launch({ headless: true }); browsers.push(browser);
    fixture.workspaceFiles.delete('alpha/api/README.md');
    // Vim editing is kept in keys.yaml (POWER.md, "Config files"): each engine starts from none.
    fixture.config.remove('keys');
    const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
    page.setDefaultTimeout(15000);
    const errors = []; await collectPageErrors(page, errors);
    await page.goto(url);
    const message = page.getByRole('textbox', { name: 'Message', exact: true });
    const chip = page.locator('.vim-mode-chip');
    await expect(message).toBeEnabled();
    await expect(chip).toHaveCount(0, { message: 'Vim editing is off until chosen' });

    // Settings → Shortcuts: both switches apply immediately.
    await page.getByRole('button', { name: 'Settings', exact: true }).click();
    await page.getByRole('tab', { name: 'Shortcuts' }).click();
    await page.getByRole('switch', { name: 'Vim editing in the message box' }).check();
    await expect(page.getByRole('status').filter({ hasText: 'Vim editing on in the message box.' })).toBeVisible();
    await page.getByRole('switch', { name: 'Vim editing in the file editor' }).check();
    await expect(page.getByRole('switch', { name: 'Vim editing in the message box' })).toBeChecked();
    await page.screenshot({ path: `/tmp/lazybox-vim-settings-${engine.name()}.png` });
    await page.getByRole('navigation', { name: 'Pages' }).getByRole('button', { name: 'Work', exact: true }).click();

    // An empty message box starts in insert mode; Esc switches to normal.
    await message.click();
    await expect(chip).toHaveText('INSERT');
    await expect(page.locator('.keyboard-hint')).toContainText('Esc for normal mode');
    await page.keyboard.type('hello world');
    await page.keyboard.press('Escape');
    await expect(chip).toHaveText('NORMAL');
    await expect(message).toHaveAttribute('data-vim-mode', 'normal');
    await expect(page.locator('.keyboard-hint')).toHaveText('Enter to send · i to type');
    await page.locator('.composer').screenshot({ path: `/tmp/lazybox-vim-normal-${engine.name()}.png` });
    await page.keyboard.type('zq');
    await expect(message).toHaveValue('hello world', { message: 'normal mode takes no typing' });
    await page.keyboard.type('0wi');
    await expect(chip).toHaveText('INSERT');
    await page.keyboard.type('big ');
    await page.keyboard.press('Escape');
    await expect(message).toHaveValue('hello big world');
    await page.keyboard.type('bd');
    await expect(chip).toHaveText('NORMAL d', { message: 'a pending operator is shown' });
    await page.keyboard.type('w');
    await expect(message).toHaveValue('hello world');
    await page.keyboard.type('u');
    await expect(message).toHaveValue('hello big world');
    await page.keyboard.press('Control+r');
    await expect(message).toHaveValue('hello world');
    await page.keyboard.type('yyp');
    await expect(message).toHaveValue('hello world\nhello world');
    await page.keyboard.type('2ddu');
    await expect(message).toHaveValue('hello world\nhello world');
    await page.keyboard.type('ggvey');
    await expect(chip).toHaveText('NORMAL');
    await page.keyboard.type('jdd');
    await expect(message).toHaveValue('hello world');
    await page.keyboard.type('$x');
    await expect(message).toHaveValue('hello worl');
    // In normal mode a lone ] is the leader, not text: the menu appears, and a Vim key cancels it.
    const menu = page.getByRole('status', { name: 'Keyboard menu' });
    await page.keyboard.press(']');
    await expect(menu).toBeVisible();
    await expect(message).toHaveValue('hello worl');
    await page.keyboard.type('x');
    await expect(menu).toHaveCount(0);
    await expect(message).toHaveValue('hello wor');
    await page.keyboard.press(']');
    await page.keyboard.press('Escape');
    await expect(menu).toHaveCount(0);
    await expect(message).toBeFocused();
    await page.keyboard.type('u');
    await expect(message).toHaveValue('hello worl');
    // Vim edits are ordinary input: the saved draft survives a reload.
    await page.reload();
    await expect(message).toHaveValue('hello worl');
    await message.click();
    await page.keyboard.press('End');
    await page.keyboard.type('d');
    await expect(message).toHaveValue('hello world', { message: 'a non-empty draft reopens in insert mode' });
    await page.keyboard.press('Escape');

    // ]] still opens the leader from normal mode, without a stray bracket.
    await page.keyboard.press(']');
    await page.keyboard.press(']');
    await expect(menu).toBeVisible();
    await expect(menu).toContainText('New workspace');
    await expect(page.locator('.workspace-row').filter({ hasText: 'Alpha feature' }).locator('.workspace-link')).toBeFocused();
    await expect(message).toHaveValue('hello world');
    await page.keyboard.press('Escape');
    await expect(menu).toHaveCount(0);
    await page.keyboard.press('l');
    await expect(message).toBeFocused();
    await expect(chip).toHaveText('NORMAL');
    // ...and from insert mode, where the first bracket is text until the second arrives.
    await page.keyboard.type('A!');
    await page.keyboard.press(']');
    await expect(message).toHaveValue('hello world!]');
    await page.keyboard.press(']');
    await expect(menu).toBeVisible();
    await expect(message).toHaveValue('hello world!');
    await page.keyboard.press('Escape');
    await page.keyboard.press('l');
    await expect(message).toBeFocused();

    // Enter in normal mode sends; the next message starts in insert mode.
    await page.keyboard.press('Escape');
    await expect(chip).toHaveText('NORMAL');
    const sent = fixture.mutations.filter(call => call.method === 'Apply').length;
    await page.keyboard.press('Enter');
    await expect(message).toHaveValue('');
    await expect.poll(() => fixture.mutations.filter(call => call.method === 'Apply').length).toBe(sent + 1);
    assert.match(JSON.stringify(fixture.mutations.filter(call => call.method === 'Apply').at(-1).body), /hello world!/);
    await expect(chip).toHaveText('INSERT');
    // Insert-mode Enter keeps its usual meaning: Shift+Enter is a new line, Enter sends.
    await message.focus();
    await page.keyboard.type('one');
    await page.keyboard.press('Shift+Enter');
    await page.keyboard.type('two');
    await expect(message).toHaveValue('one\ntwo');
    await page.keyboard.press('Enter');
    await expect(message).toHaveValue('');
    await expect.poll(() => fixture.mutations.filter(call => call.method === 'Apply').length).toBe(sent + 2);
    await page.screenshot({ path: `/tmp/lazybox-vim-composer-${engine.name()}.png` });

    // Monaco: Vim keys, a status line, :w saves, :q goes through the unsaved-changes guard.
    await page.getByRole('button', { name: 'README', exact: true }).click({ modifiers: ['Alt'] });
    const dialog = page.getByRole('dialog', { name: 'File preview', exact: true });
    await dialog.getByRole('button', { name: 'Edit file', exact: true }).click();
    const status = dialog.getByLabel('Vim status');
    await expect(status).toContainText('NORMAL');
    await dialog.locator('.view-lines').click();
    await page.keyboard.type('ggI');
    await expect(status).toContainText('INSERT');
    await page.keyboard.type(`vim ${engine.name()} `);
    await page.keyboard.press('Escape');
    await expect(dialog, { message: 'Esc leaves insert mode, not the dialog' }).toBeVisible();
    await expect(status).toContainText('NORMAL');
    await page.keyboard.type(':w');
    await page.keyboard.press('Enter');
    await expect.poll(() => fixture.workspaceFiles.get('alpha/api/README.md').text).toMatch(new RegExp(`^vim ${engine.name()} # File preview`));
    await expect(dialog.locator('.file-preview-toolbar')).toContainText('Saved');
    await expect(status).toContainText('written');
    await page.keyboard.type('d');
    await page.keyboard.press('Escape');
    await expect(dialog, { message: 'Esc cancels a pending operator, not the dialog' }).toBeVisible();
    await page.keyboard.type('x');
    await expect.poll(() => dialog.locator('.view-lines').textContent()).toMatch(new RegExp(`^vim\\s${engine.name()}#`), 'Esc cancelled d, so x deletes only the space under the cursor');
    await expect(dialog.locator('.file-preview-toolbar')).toContainText('Unsaved changes');
    await page.keyboard.type(':q');
    await page.keyboard.press('Enter');
    await expect(dialog.getByText('Discard your unsaved changes?')).toBeVisible();
    await dialog.getByRole('button', { name: 'Keep editing' }).click();
    await page.screenshot({ path: `/tmp/lazybox-vim-editor-${engine.name()}.png` });
    await dialog.locator('.view-lines').click();
    await page.keyboard.type(':q!');
    await page.keyboard.press('Enter');
    await expect(dialog).toHaveCount(0);
    assert.match(fixture.workspaceFiles.get('alpha/api/README.md').text, new RegExp(`^vim ${engine.name()} `), ':q! discards only the unsaved edit');

    // Esc belongs to the editor even in idle normal mode; :q closes a clean file at once,
    // and Esc outside the editor still closes the dialog.
    await page.getByRole('button', { name: 'README', exact: true }).click({ modifiers: ['Alt'] });
    await dialog.getByRole('button', { name: 'Edit file', exact: true }).click();
    await expect(status).toContainText('NORMAL');
    await dialog.locator('.view-lines').click();
    await page.keyboard.press('Escape');
    await page.keyboard.press('Escape');
    await expect(dialog).toBeVisible();
    await page.keyboard.type(':q');
    await page.keyboard.press('Enter');
    await expect(dialog).toHaveCount(0);
    await page.getByRole('button', { name: 'README', exact: true }).click({ modifiers: ['Alt'] });
    await expect(dialog.getByRole('button', { name: 'Close file preview' })).toBeFocused();
    await page.keyboard.press('Escape');
    await expect(dialog).toHaveCount(0);
    assert.deepEqual(errors, []);
    await page.close();
  }
  console.log('PASS: Vim editing toggles, composer normal/insert/visual editing with counts, undo/redo, draft persistence, ]] leader from both modes, Enter sends in normal mode, Monaco Vim with :w, :q guard, :q!, Esc kept inside the editor. WebKit and Chromium fixtures only.');
} finally {
  for (const browser of browsers) await browser.close();
  await server.close(); await fixture.close();
}
