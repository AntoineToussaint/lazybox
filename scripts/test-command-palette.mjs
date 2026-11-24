// Command palette, shortcuts overlay, navigation keys, toasts and sounds:
// ⌘K / ⌘⇧P over the action registry and "go to" anything; the `>` `@` `#`
// `/` prefixes; `?` and ⌘/ for every shortcut with Customize…; ⌘1…9;
// "next conversation that needs you" across workspaces; a toast and a sound
// (AudioContext stubbed) when an agent starts waiting while the window is in
// front; and the palette in a detached chat window driving the main window.
// Fake Runner; no agent is started or stopped.
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from './fixtures/vite-cache.mjs';
import react from '@vitejs/plugin-react';
import {webkit, chromium, expect} from './fixtures/playwright.mjs';
import {fileURLToPath} from 'node:url';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import { collectPageErrors } from './fixtures/page-errors.mjs';

const f = await startRunnerFixture();
const now = Date.now(), ms = value => String(value);
const data = value => Buffer.from(JSON.stringify(value)).toString('base64');
const root = (status, updated, extra = {}) => [{id: 'root', name: 'Main agent', status, updatedAtUnixMillis: ms(updated), ...extra}];
const base = {profileId: 'claude-default', provider: 'claude', capabilities: {files: true}, pending: []};
Object.assign(f.sessions[0], {title: 'Index the API', model: 'claude-fable-5', createdAtUnixMillis: ms(now - 4_000_000)});
Object.assign(f.sessions[1], {title: 'Ship release notes', model: 'claude-fable-5', status: 'waiting_input', createdAtUnixMillis: ms(now - 4_000_000), agents: root('waiting_input', now - 300_000),
  pending: [{type: 'question', requestId: 'q-release', providerDataJson: data({questions: [{question: 'Which version should the notes cover?'}]})}]});
f.sessions.push(
  {...base, id: 'parser-session', collectionId: 'alpha', title: 'Refactor the parser', model: 'claude-fable-5', status: 'running', busy: true, createdAtUnixMillis: ms(now - 1_000_000), agents: root('running', now - 10_000)},
  {...base, id: 'deps-session', collectionId: 'beta', title: 'Bump dependencies', model: 'claude-fable-5', status: 'running', busy: true, createdAtUnixMillis: ms(now - 2_000_000), agents: root('running', now - 30_000)},
);
const session = id => f.sessions.find(s => s.id === id);
const mod = 'Meta';

const server = await createServer({...testViteCache(import.meta.url), configFile: false, root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: {host: '127.0.0.1', port: 0, proxy: {'/lazybox.bench.v1.RunnerService/': {target: f.url}}}});
let browser;
try {
  await server.listen();
  browser = await (process.env.TEST_CHROMIUM ? chromium : webkit).launch();
  const context = await browser.newContext({viewport: {width: 1440, height: 900}});
  await context.addInitScript(() => {
    // A Mac, as far as the keymap is concerned (⌘ is Mod).
    Object.defineProperty(navigator, 'platform', {get: () => 'MacIntel'});
    // Focus is the test's to decide: the person is looking at this window unless told otherwise.
    window.__focused = true; document.hasFocus = () => window.__focused;
    window.__notices = [];
    window.Notification = class { static permission = 'granted'; static async requestPermission() { return 'granted'; }
      constructor(title, options) { this.title = title; this.body = options?.body; window.__notices.push(this); } close() {} };
    // A recording stand-in for Web Audio: every note an alert plays.
    window.__notes = [];
    const param = () => ({value: 0, setValueAtTime() {}, exponentialRampToValueAtTime() {}});
    window.AudioContext = class { state = 'running'; currentTime = 0; destination = {}; async resume() {}
      createGain() { return {gain: param(), connect() {}}; }
      createOscillator() { const o = {type: '', frequency: param(), connect() {}, start() { window.__notes.push(o.frequency.value); }, stop() {}}; return o; } };
  });
  const page = await context.newPage();
  page.setDefaultTimeout(15000);
  const errors = []; await collectPageErrors(page, errors);
  const origin = `http://127.0.0.1:${server.httpServer.address().port}`;
  await page.goto(`${origin}/?view=workspace&collection=alpha&session=alpha-session#token=${f.token}`);
  const workspace = page.getByRole('combobox', {name: 'Workspace', exact: true});
  const conversation = page.getByRole('combobox', {name: 'Session', exact: true});
  await expect(workspace).toHaveValue('alpha');
  await expect(page.locator('.inbox-action')).toHaveAccessibleName(/1 conversation needs attention/);
  const palette = page.getByRole('dialog', {name: 'Command palette'});
  const search = palette.getByRole('combobox', {name: 'Search commands'});
  const options = palette.getByRole('option');

  // ⌘K from the message box: the empty palette documents its prefixes and leads with what needs you.
  // The message box is disabled until the conversation has loaded, and a disabled box takes no focus.
  // A workspace's conversations open as tabs: the message box is the one in front.
  const composer = page.locator('[data-agent-composer]').filter({visible: true}).first();
  await expect(composer).toBeEnabled();
  await composer.focus();
  await expect(composer).toBeFocused();
  await page.keyboard.press(`${mod}+k`);
  await expect(palette).toBeVisible();
  await expect(search).toBeFocused();
  await expect(palette.locator('.command-palette-prefixes')).toContainText('>Actions');
  await expect(palette.locator('.command-palette-prefixes')).toContainText('@Conversations');
  await expect(palette.locator('.command-palette-prefixes')).toContainText('#Workspaces');
  await expect(palette.locator('.command-palette-prefixes')).toContainText('/Files');
  await expect(palette.getByRole('group', {name: 'Needs you'}).getByRole('option')).toHaveText([/Ship release notes/]);
  await expect(palette.getByRole('group', {name: 'Actions'}).getByRole('option').filter({hasText: 'New conversation'}).locator('kbd')).toHaveText(['⇧⌘N']);
  await page.screenshot({path: '/tmp/lazybox-palette-empty.png'});
  const paletteBounds = () => Promise.all([palette, palette.locator('footer')].map(locator => locator.boundingBox()));
  const unfiltered = await paletteBounds();
  await search.fill('zzqxyunmatched');
  await expect(palette.getByText(/Nothing matches/)).toBeVisible();
  assert.deepEqual(await paletteBounds(), unfiltered, 'filtering must not move the palette or its footer');
  // ⌘K again closes it, and focus goes back where it was.
  await page.keyboard.press(`${mod}+k`);
  await expect(palette).toHaveCount(0);
  await expect.poll(() => page.evaluate(() => document.activeElement?.matches('[data-agent-composer]'))).toBe(true);

  // Fuzzy search runs an action: "new conv" → New conversation opens its dialog.
  await page.keyboard.press(`${mod}+Shift+p`);
  await search.fill('new conv');
  await expect(options.first()).toContainText('New conversation');
  await expect(options.first()).toHaveAttribute('aria-selected', 'true');
  await page.keyboard.press('Enter');
  await expect(palette).toHaveCount(0);
  await expect(page.getByRole('dialog', {name: /New conversation/})).toBeVisible();
  await page.keyboard.press('Escape');

  // @ conversations across workspaces; ↓ moves, Enter goes there.
  await page.locator('.workspace-brand').click();
  await page.keyboard.press(`${mod}+k`);
  await search.fill('@');
  await expect(palette.locator('.command-palette-scope')).toHaveText('Conversations');
  await expect(options).toHaveCount(4);
  await search.fill('@bump');
  await expect(options).toHaveCount(1);
  await expect(options.first()).toContainText('Beta release');
  await page.keyboard.press('Enter');
  await expect(workspace).toHaveValue('beta');
  await expect(conversation).toHaveValue('deps-session');

  // # workspaces, with their ⌘N; Enter goes there.
  await page.keyboard.press(`${mod}+k`);
  await search.fill('#');
  await expect(options).toHaveText([/Alpha feature.*⌘1/, /Beta release.*⌘2/]);
  await search.fill('#alp');
  await page.keyboard.press('Enter');
  await expect(workspace).toHaveValue('alpha');

  // / files in this workspace (Go to file's search); Enter opens it in the editor.
  await page.keyboard.press(`${mod}+k`);
  await search.fill('/rout');
  await expect(options.first()).toContainText('routes.ts');
  await page.keyboard.press('Enter');
  await expect(page.locator('section.workspace-panel.panel-editor').getByRole('tab').filter({hasText: 'routes.ts'})).toBeVisible();

  // Without a prefix, one ranked list across kinds: a settings section, a panel.
  await page.locator('.workspace-brand').click();
  await page.keyboard.press(`${mod}+k`);
  await search.fill('notif');
  await expect(options.first()).toContainText('Notifications settings');
  await expect(options.first().locator('.command-palette-kind')).toHaveText('Settings');
  await search.fill('missions');
  await expect(options.filter({hasText: 'Open the missions board'})).toHaveCount(1);
  await expect(options.filter({hasText: 'Show Missions'})).toHaveCount(0);
  // Recent first: the file opened a moment ago leads the empty palette (the current workspace is left out).
  await search.fill('');
  await expect(palette.getByRole('group', {name: 'Recent'}).getByRole('option').first()).toContainText('routes.ts');
  await page.keyboard.press('Escape');
  await expect(palette).toHaveCount(0);

  // The shortcuts overlay: ? outside a text field, grouped and searchable; ⌘/ closes it.
  const overlay = page.getByRole('dialog', {name: 'Keyboard shortcuts'});
  await page.locator('.workspace-brand').click();
  await page.keyboard.press('?');
  await expect(overlay).toBeVisible();
  await expect(overlay.locator('[data-action="palette.open"] dd')).toContainText('⌘K');
  await expect(overlay.locator('[data-action="workspace.select"] dd')).toContainText('⌘1…⌘9');
  await expect(overlay.getByRole('heading', {name: 'Go to'})).toBeVisible();
  const shortcutsBounds = await overlay.boundingBox();
  await overlay.getByRole('searchbox', {name: 'Filter shortcuts'}).fill('zzqxyunmatched');
  await expect(overlay.getByText(/No shortcut matches/)).toBeVisible();
  assert.deepEqual(await overlay.boundingBox(), shortcutsBounds);
  await overlay.getByRole('searchbox', {name: 'Filter shortcuts'}).fill('needs you');
  await expect(overlay.locator('.shortcut-row')).toHaveCount(2);
  await page.screenshot({path: '/tmp/lazybox-shortcuts.png'});
  await page.keyboard.press(`${mod}+/`);
  await expect(overlay).toHaveCount(0);
  // ? typed in the message box is text, not the overlay.
  await composer.focus();
  await page.keyboard.type('why?');
  await expect(overlay).toHaveCount(0);
  await expect(composer).toHaveValue('why?');
  await composer.fill('');
  // ⌘/ works from the message box, and Customize… opens Settings → Shortcuts.
  await page.keyboard.press(`${mod}+/`);
  await expect(overlay).toBeVisible();
  await overlay.getByRole('button', {name: 'Customize…'}).click();
  await expect(overlay).toHaveCount(0);
  await expect(page.getByRole('tab', {name: 'Shortcuts', exact: true})).toHaveAttribute('aria-selected', 'true');
  await expect(page.getByRole('textbox', {name: 'Command palette', exact: true})).toHaveValue('Meta+k or Meta+Shift+p or ] ] p');

  // ⌘2 and ⌘1 jump to the workspaces in sidebar order (and leave Settings).
  await page.keyboard.press(`${mod}+2`);
  await expect(workspace).toHaveValue('beta');
  await page.keyboard.press(`${mod}+1`);
  await expect(workspace).toHaveValue('alpha');
  await expect(conversation).toHaveValue('alpha-session');
  // ⌘] / ⌘[ step through this workspace's conversations, newest first.
  await page.locator('.workspace-brand').click();
  await page.keyboard.press(`${mod}+]`);
  await expect(conversation).toHaveValue('parser-session');
  await page.keyboard.press(`${mod}+]`);
  await expect(conversation).toHaveValue('alpha-session');
  await page.keyboard.press(`${mod}+[`);
  await expect(conversation).toHaveValue('parser-session');

  // An agent in the other workspace starts waiting while the person looks at this one:
  // a toast (not a system notification) and the "needs you" sound.
  Object.assign(session('deps-session'), {status: 'waiting_approval', busy: false, agents: root('waiting_approval', Date.now()), pending: [{type: 'approval', requestId: 'a-deps', text: 'Bash', providerDataJson: data({input: {command: 'npm update'}})}]});
  const toast = page.locator('.toast').filter({hasText: 'Bump dependencies'});
  await expect(toast).toBeVisible({timeout: 10_000});
  await expect(toast).toHaveAttribute('data-tone', 'needs-input');
  await expect(toast).toContainText('Approval needed · Bump dependencies');
  await expect(toast).toContainText('Beta release · Run npm update');
  await expect(toast.locator('.toast-hint')).toHaveText('⌘J');
  assert.deepEqual(await page.evaluate(() => window.__notices.map(n => n.title)), [], 'a focused window shows a toast, not a banner');
  assert.ok((await page.evaluate(() => window.__notes)).length >= 2, 'the needs-you sound played');
  await page.screenshot({path: '/tmp/lazybox-toast.png'});

  // ⌘J cycles through what needs the person, across workspaces; ⌘⇧J goes back.
  await page.keyboard.press(`${mod}+j`);
  await expect(conversation).toHaveValue('deps-session');
  await expect(workspace).toHaveValue('beta');
  await page.keyboard.press(`${mod}+j`);
  await expect(conversation).toHaveValue('beta-session');
  await page.keyboard.press(`${mod}+j`);
  await expect(conversation).toHaveValue('deps-session');
  await page.keyboard.press(`${mod}+Shift+j`);
  await expect(conversation).toHaveValue('beta-session');

  // With the window in the background, the same change is a system notification; "done" stays silent by default.
  await page.evaluate(() => { window.__focused = false; window.__notes = []; });
  Object.assign(session('parser-session'), {status: 'waiting_input', busy: false, agents: root('waiting_input', Date.now()), pending: [{type: 'question', requestId: 'q-parser', providerDataJson: data({questions: [{question: 'Split by file or by type?'}]})}]});
  await expect.poll(() => page.evaluate(() => window.__notices.map(n => n.title)), {timeout: 10_000}).toEqual(['Question · Refactor the parser']);
  await expect(page.locator('.toast').filter({hasText: 'Refactor the parser'})).toHaveCount(0);
  await page.evaluate(() => { window.__focused = true; });
  Object.assign(session('alpha-session'), {status: 'done', busy: false, agents: root('done', Date.now(), {turnEndedAtUnixMillis: ms(Date.now()), lastTurnStatus: 'completed'}), lastMessage: 'Indexed 12 endpoints.'});
  await expect(page.locator('.inbox-action')).toHaveAccessibleName(/4 conversations need attention/, {timeout: 10_000});
  assert.equal((await page.evaluate(() => window.__notes)).length, 2, 'only the needs-you sound; the done sound is off');

  // Settings → Notifications: the sound switches, needs-you on and done off, each with a preview.
  await page.keyboard.press(`${mod}+,`);
  await page.getByRole('tab', {name: 'Notifications', exact: true}).click();
  await expect(page.getByRole('switch', {name: /^Needs you/})).toBeChecked();
  await expect(page.getByRole('switch', {name: /^Done/})).not.toBeChecked();
  await page.evaluate(() => { window.__notes = []; });
  await page.getByRole('button', {name: 'Play the done sound'}).click();
  assert.equal((await page.evaluate(() => window.__notes)).length, 2);
  await page.getByRole('button', {name: 'Settings', exact: true}).click();

  // A detached chat window has the palette too; going to a conversation drives the main window.
  const chat = await context.newPage();
  await collectPageErrors(chat, errors);
  await chat.goto(`${origin}/?view=chat&panel=detached-test&collection=alpha&session=alpha-session#token=${f.token}`);
  await expect(chat.locator('[data-agent-composer]').filter({visible: true}).first()).toBeVisible();
  await chat.locator('[data-agent-composer]').filter({visible: true}).first().focus();
  await chat.keyboard.press(`${mod}+k`);
  const chatPalette = chat.getByRole('dialog', {name: 'Command palette'});
  await expect(chatPalette).toBeVisible();
  await chatPalette.getByRole('combobox').fill('@ship');
  await chat.keyboard.press('Enter');
  await expect(conversation).toHaveValue('beta-session');
  await expect(workspace).toHaveValue('beta');
  // A chat command run from the palette leaves the message box's draft as it was (it used to empty it, saved copy and all).
  const chatComposer = chat.locator('[data-agent-composer]').filter({visible: true}).first();
  await chatComposer.fill('a draft to keep');
  await chat.keyboard.press(`${mod}+k`);
  await chatPalette.getByRole('combobox').fill('Show context and token cost');
  await expect(chatPalette.getByRole('option').first()).toContainText('Show context and token cost');
  await chat.keyboard.press('Enter');
  await expect(chatPalette).toBeHidden();
  await expect(chatComposer).toHaveValue('a draft to keep');
  await chat.reload();
  await expect(chatComposer).toHaveValue('a draft to keep');

  assert.deepEqual(f.mutations, [], 'the palette and keys never start, stop or answer an agent');
  assert.deepEqual(errors, []);
  console.log('PASS command palette: ⌘K/⌘⇧P, prefixes, recent, settings and panels; shortcuts overlay with Customize; ⌘1…9; ⌘]/⌘[; ⌘J across workspaces; toast and sound when focused, banner when not; detached palette drives the main window; a chat command run from it keeps the draft.');
} finally { await browser?.close(); await server.close(); await f.close(); }
