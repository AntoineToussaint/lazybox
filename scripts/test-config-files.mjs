// Fixture only: settings as files and one command plane (POWER.md, "Config
// files", "lazybox do"). keys.yaml saved in an editor changes a key in the open
// window within two seconds, with no reload; a mistake in it shows in a toast
// and in Settings by line, and a file that does not load keeps the last good
// keys; Settings → Shortcuts and → Notifications write their file; `lazybox
// do` runs registry actions in the window (maximize, settings with an
// argument) and says when one cannot run; ⌘⇧L cycles the presets and your
// layout files; a layout dropped into layouts/ shows in the Layout menu, a
// broken one by line; Copy puts the layout's YAML on the clipboard, pasting
// layout YAML into ⌘K imports it (a bad one is refused by line); Save, rename
// and delete with Undo write the files. Screenshots dark and light:
// .cache/config-files-*.png. WebKit, the browser build, the fake Runner.
import { testViteCache } from './fixtures/vite-cache.mjs';
import { createServer } from 'vite';
import react from '@vitejs/plugin-react';
import { webkit, expect } from './fixtures/playwright.mjs';
import { fileURLToPath } from 'node:url';
import { mkdir } from 'node:fs/promises';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';
import { collectPageErrors } from './fixtures/page-errors.mjs';

const f = await startRunnerFixture();
const server = await createServer({ ...testViteCache(import.meta.url), configFile: false, logLevel: 'error', root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: f.url } } } });
await server.listen(); await mkdir('.cache', { recursive: true });
const url = `http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${f.token}`;
const updates = name => f.mutations.filter(m => m.method === 'UpdateConfigFile' && m.body.name === name);
const keysFile = (bindings, extra = {}) => ({ preset: 'vim', bindings, menu: [], vim: { messageBox: false, fileEditor: false }, ...extra });
const wideLayout = `# yaml-language-server: $schema=../layout.schema.json
name: Wide review
layout:
  split: row
  sizes: [20, 80]
  children:
    - panels: [collections]
    - panels: [changes, chat]
`;

async function scenario(browser, scheme, full) {
  const context = await browser.newContext({ viewport: { width: 1440, height: 900 }, colorScheme: scheme });
  // The clipboard, observable: WebKit headless has no permission prompt to grant.
  await context.addInitScript(() => { window.__clipboard = ''; Object.defineProperty(navigator, 'clipboard', { configurable: true, value: { writeText: async text => { window.__clipboard = text; }, readText: async () => window.__clipboard, write: async () => undefined, read: async () => [] } }); });
  const page = await context.newPage();
  page.setDefaultTimeout(10000);
  const errors = []; await collectPageErrors(page, errors);
  const shot = name => page.screenshot({ path: `.cache/config-files-${name}-${scheme}.png` });
  await page.goto(url);
  const composer = page.locator('.page-layout .panel-chat').getByRole('textbox', { name: 'Message', exact: true });
  await expect(composer).toBeEnabled();
  const overlay = page.getByRole('dialog', { name: 'Keyboard shortcuts' });
  const toast = text => page.locator('.toast, [role=status], [role=alert]').filter({ hasText: text }).first();
  const openSettings = async tab => { await page.keyboard.press('Meta+,'); await page.getByRole('tab', { name: tab }).click(); };
  // Leaves Settings the way `lazybox open` does: by showing the conversation.
  const closeSettings = async () => { await f.power.waitForWindow(); await f.power.post({ kind: 'open', collectionId: 'alpha', sessionId: 'alpha-session' }); await expect(composer).toBeVisible(); };
  const layoutMenu = page.locator('.layout-menu');
  const openLayoutMenu = async () => { if (!await layoutMenu.evaluate(node => node.open)) await layoutMenu.locator('summary').click(); await expect(layoutMenu.locator('.layout-grid').first()).toBeVisible(); };

  if (full) {
    // ---- keys.yaml saved in an editor: the key works within two seconds, no reload.
    f.config.setFile('keys', { value: keysFile([{ action: 'shortcuts.open', keys: ['Mod+Shift+u'], section: 'global', line: 5 }]) });
    const started = Date.now();
    await expect.poll(async () => { if (!await overlay.isVisible()) { await composer.focus(); await page.keyboard.press('Meta+Shift+u'); await page.waitForTimeout(200); } return overlay.isVisible(); }, { timeout: 4000, intervals: [300] }).toBe(true);
    assert.ok(Date.now() - started < 4000, 'applied within the poll');
    await page.keyboard.press('Escape');
    await expect(overlay).toBeHidden();

    // ---- A mistake: a toast with its line, the banner in Settings, the rest keeps working.
    f.config.setFile('keys', { value: keysFile([{ action: 'shortcuts.open', keys: ['Mod+Shift+u'], section: 'global', line: 5 }]), issues: [{ path: 'keys.global.palete.open', line: 7, message: 'there is no action "palete.open" (did you mean "palette.open"?) (lazybox do --list lists every action)', severity: 'error' }] });
    await expect(page.getByText('keys.yaml: a problem')).toBeVisible();
    await expect(page.getByText(/line 7: there is no action "palete.open"/).first()).toBeVisible();
    await shot('toast');
    await openSettings('Shortcuts');
    const problems = page.getByRole('list', { name: 'Problems in keys.yaml' });
    await expect(problems).toContainText('line 7');
    await expect(problems).toContainText('did you mean "palette.open"?');
    await expect(page.getByText('/fixture/config/lazybox/keys.yaml')).toBeVisible();
    await expect(page.locator('#shortcut-shortcuts\\.open')).toHaveValue('Meta+Shift+u');
    await shot('settings-problems');

    // ---- A save that does not load: said so; the last good keys stay.
    f.config.setFile('keys', { error: 'not valid YAML: line 3: did not find expected key', stale: true, value: keysFile([{ action: 'shortcuts.open', keys: ['Mod+Shift+u'], section: 'global', line: 5 }]) });
    await expect(page.getByRole('alert').filter({ hasText: 'keys.yaml does not load' })).toContainText('the last version that loaded');
    await shot('settings-stale');

    // ---- Settings writes the file: the edit at its path, the app keeps it.
    f.config.setFile('keys', { value: keysFile([{ action: 'shortcuts.open', keys: ['Mod+Shift+u'], section: 'global', line: 5 }]) });
    await expect(page.getByRole('alert').filter({ hasText: 'does not load' })).toHaveCount(0);
    const before = updates('keys').length;
    await page.locator('#shortcut-shortcuts\\.open').fill('Meta+Shift+o');
    await page.getByRole('button', { name: 'Save shortcuts' }).click();
    await expect(page.getByRole('status').filter({ hasText: 'Shortcuts saved. keys.yaml updated.' })).toBeVisible();
    const sent = updates('keys').slice(before);
    assert.deepEqual(sent.at(-1).body.edits, [{ path: ['keys', 'global', 'shortcuts.open'], json: '"Mod+Shift+o"' }]);
    assert.deepEqual(f.config.value('keys').bindings.map(b => [b.action, b.keys]), [['shortcuts.open', ['Mod+Shift+o']]]);
    // The Vim switch writes too.
    await page.getByRole('switch').first().check();
    await expect.poll(() => updates('keys').at(-1).body.edits).toEqual([{ path: ['vim', 'message-box'], json: 'true' }]);
    await page.getByRole('switch').first().uncheck();

    // ---- Notifications: a toggle writes settings.yaml; a save in the editor shows here.
    await page.getByRole('tab', { name: 'Notifications' }).click();
    await expect(page.getByText('settings.yaml', { exact: true })).toBeVisible();
    await page.locator('.notification-option').filter({ hasText: 'Ready for review' }).getByRole('switch').check();
    await expect.poll(() => updates('settings').at(-1)?.body.edits).toEqual([{ path: ['notifications', 'review'], json: 'true' }]);
    f.config.setFile('settings', { value: { notifications: { review: true, needsInput: false } } });
    await expect(page.locator('.notification-option').filter({ hasText: 'Needs input' }).getByRole('switch')).not.toBeChecked({ timeout: 4000 });
    await shot('settings-notifications');
    await closeSettings();
    await composer.focus(); await page.keyboard.press('Meta+Shift+o');
    await expect(overlay).toBeVisible();
    await page.keyboard.press('Escape');

    // ---- lazybox do: registry actions in the window.
    await f.power.waitForWindow();
    await composer.click();
    const maximized = await f.power.post({ kind: 'do', actionId: 'layout.maximize' });
    assert.equal(maximized.state, 'done', maximized.error);
    assert.match(maximized.detail, /Ran “Maximize or restore the active panel”/);
    await expect(page.getByRole('button', { name: 'Restore layout' })).toBeVisible();
    await f.power.post({ kind: 'do', actionId: 'layout.maximize' });
    await expect(page.getByRole('button', { name: 'Restore layout' })).toHaveCount(0);
    const settings = await f.power.post({ kind: 'do', actionId: 'settings.open', argument: 'keyboard' });
    assert.equal(settings.state, 'done');
    await expect(page.getByRole('tab', { name: 'Shortcuts', selected: true })).toBeVisible();
    const unavailable = await f.power.post({ kind: 'do', actionId: 'layout.split' });
    assert.equal(unavailable.state, 'failed');
    assert.match(unavailable.error, /not available in the window right now/);
    await closeSettings();

    // ---- A layout file dropped into layouts/ shows in the Layout menu; a broken one by line.
    f.config.writeLayout('layouts/wide-review', wideLayout);
    f.config.writeLayout('layouts/broken', 'name: Broken\nlayout:\n  panels: [nope]\n');
    await openLayoutMenu();
    await expect(layoutMenu.getByRole('button', { name: 'Apply layout Wide review' })).toBeVisible({ timeout: 4000 });
    await expect(layoutMenu.getByRole('list', { name: 'Layout files with problems' })).toContainText('broken.yaml');
    await expect(layoutMenu.getByRole('list', { name: 'Layout files with problems' })).toContainText('there is no panel "nope"');
    await expect(layoutMenu).toContainText('Your layouts are files in');
    await shot('layout-menu');
    await page.keyboard.press('Escape');

    // ---- ⌘⇧L: presets, then your layouts, around.
    // Each press shows a card for 1.8 s that replaces the last one. Read twice from the test, a card can
    // leave between the reads on a slow machine (or be the previous one): the page records each as it comes.
    await page.evaluate(() => {
      window.__layoutCards = [];
      const record = () => { const card = [...document.querySelectorAll('.toast-stack .toast-main')].find(main => main.querySelector('strong')?.textContent.startsWith('Layout: ')); const title = card?.querySelector('strong')?.textContent || '';
        if (title.startsWith('Layout: ') && window.__layoutCards.at(-1)?.title !== title) window.__layoutCards.push({ title, body: card.querySelector('span')?.textContent || '' }); };
      new MutationObserver(record).observe(document.body, { childList: true, subtree: true, characterData: true });
    });
    await composer.click();
    for (let i = 0; i < 6; i++) {
      await page.keyboard.press('Meta+Shift+l');
      await expect.poll(() => page.evaluate(() => window.__layoutCards.length)).toBe(i + 1);
    }
    const cards = await page.evaluate(() => window.__layoutCards);
    assert.deepEqual(cards.map(card => card.title.replace('Layout: ', '')), ['Chat + Changes', 'Chat + Editor + Terminal', 'Review', 'Everything', 'Wide review', 'Chat focus']);
    assert.match(cards.at(-1).body, /^1 of 6/);

    // ---- Copy the layout as YAML; paste it into ⌘K: the import dialog opens with it.
    await openLayoutMenu();
    await layoutMenu.getByRole('button', { name: 'Copy current layout as YAML' }).click();
    await expect.poll(() => page.evaluate(() => window.__clipboard)).toContain('name: Chat focus');
    const copied = await page.evaluate(() => window.__clipboard);
    assert.match(copied, /^# yaml-language-server: \$schema=\.\.\/layout\.schema\.json\nname: Chat focus\n/);
    const pasted = copied.replace('name: Chat focus', 'name: Focus copy').replace('panels: [chat]', 'panels: [chat, changes]');
    await page.keyboard.press('Meta+k');
    const search = page.getByRole('combobox', { name: 'Search commands' });
    await search.evaluate((node, text) => { const data = new DataTransfer(); data.setData('text/plain', text); node.dispatchEvent(new ClipboardEvent('paste', { clipboardData: data, bubbles: true, cancelable: true })); }, pasted);
    const dialog = page.getByRole('dialog', { name: 'Import a layout' });
    await expect(dialog.getByRole('textbox', { name: 'Layout YAML' })).toHaveValue(pasted);
    await shot('import');
    // A mistake is refused by line; nothing is written.
    await dialog.getByRole('textbox', { name: 'Layout YAML' }).fill(pasted.replace('panels: [chat, changes]', 'panels: [chta]'));
    await dialog.getByRole('button', { name: 'Import' }).click();
    await expect(dialog.getByRole('alert', { name: 'Problems in the layout' })).toContainText('there is no panel "chta"');
    await expect(dialog.getByRole('alert', { name: 'Problems in the layout' })).toContainText(/line \d+/);
    await shot('import-problem');
    assert.ok(!f.config.docs.has('layouts/focus-copy'));
    await dialog.getByRole('textbox', { name: 'Layout YAML' }).fill(pasted);
    await page.keyboard.press('Meta+Enter');
    await expect(dialog).toHaveCount(0);
    await expect(page.getByText('Imported “Focus copy”')).toBeVisible();
    assert.equal(f.config.value('layouts/focus-copy').name, 'Focus copy');
    await expect(page.locator('.dv-tab .dock-tab-title', { hasText: 'Changes' })).toBeVisible();

    // ---- Save, rename, delete (with Undo): the files change.
    await openLayoutMenu();
    await layoutMenu.getByRole('button', { name: 'Save current layout as…' }).click();
    await layoutMenu.getByRole('textbox', { name: 'Layout name' }).fill('Mine');
    await layoutMenu.getByRole('button', { name: 'Save', exact: true }).click();
    await expect.poll(() => f.config.value('layouts/mine')?.name).toBe('Mine');
    assert.match(updates('layouts/mine')[0].body.content, /^# yaml-language-server.*\nname: Mine\nlayout:\n  split: row\n/);
    await openLayoutMenu();
    await layoutMenu.getByRole('button', { name: 'Rename layout Mine' }).click();
    await layoutMenu.getByRole('textbox', { name: 'New layout name' }).fill('Mine, renamed');
    await layoutMenu.getByRole('button', { name: 'Rename', exact: true }).click();
    await expect.poll(() => updates('layouts/mine').at(-1).body.edits).toEqual([{ path: ['name'], json: '"Mine, renamed"' }]);
    await openLayoutMenu();
    await layoutMenu.getByRole('button', { name: 'Delete layout Mine, renamed' }).click();
    await expect.poll(() => f.config.docs.has('layouts/mine')).toBe(false);
    await page.getByRole('button', { name: 'Undo' }).click();
    await expect.poll(() => f.config.value('layouts/mine')?.name).toBe('Mine, renamed');

    // ---- Settings → Actions & hooks lists every config file with its state.
    await openSettings('Actions & hooks');
    const list = page.getByRole('list', { name: 'Config files' });
    await expect(list).toContainText('keys.yaml');
    await expect(list).toContainText('layouts/broken.yaml');
    await expect(list.getByRole('list', { name: 'Problems in layouts/broken.yaml' })).toContainText('line 3');
    await list.scrollIntoViewIfNeeded();
    await shot('power-settings');
    await closeSettings();
  } else {
    // Light: the same surfaces, as a person sees them.
    await openSettings('Shortcuts');
    await expect(page.getByRole('list', { name: 'Problems in keys.yaml' })).toHaveCount(0);
    f.config.setFile('keys', { value: f.config.value('keys'), issues: [{ path: 'keys.global.palete.open', line: 7, message: 'there is no action "palete.open" (did you mean "palette.open"?)', severity: 'error' }] });
    await expect(page.getByRole('list', { name: 'Problems in keys.yaml' })).toContainText('line 7');
    await shot('settings-problems');
    await page.getByRole('tab', { name: 'Actions & hooks' }).click();
    await page.getByRole('list', { name: 'Config files' }).scrollIntoViewIfNeeded();
    await shot('power-settings');
    await closeSettings();
    await openLayoutMenu();
    await shot('layout-menu');
    await layoutMenu.getByRole('button', { name: 'Import a layout…' }).click();
    const dialog = page.getByRole('dialog', { name: 'Import a layout' });
    await dialog.getByRole('textbox', { name: 'Layout YAML' }).fill(wideLayout.replace('[changes, chat]', '[chta]'));
    await dialog.getByRole('button', { name: 'Import' }).click();
    await expect(dialog.getByRole('alert')).toBeVisible();
    await shot('import-problem');
  }
  assert.deepEqual(errors, []);
  await context.close();
}

let browser;
try {
  browser = await webkit.launch();
  await scenario(browser, 'dark', true);
  await scenario(browser, 'light', false);
  console.log('PASS config files: keys.yaml hot-reloads a key with no reload; a mistake shows in a toast and Settings by line and a broken save keeps the last good keys; Settings → Shortcuts, Vim and Notifications write their files; settings.yaml from the editor shows in Settings; lazybox do maximizes, opens Settings → Shortcuts and refuses what cannot run; a dropped layout file shows in the Layout menu, a broken one by line; ⌘⇧L cycles the presets and the files; Copy writes the layout YAML; a pasted layout in ⌘K imports, a bad one is refused by line; save, rename and delete with Undo write the files; Settings → Actions & hooks lists every config file. WebKit, fixture Runner, dark and light.');
} finally {
  await browser?.close();
  await server.close();
  await f.close();
}
