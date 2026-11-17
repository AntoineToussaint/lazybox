// Fixture only: a multi-service run (Codefly, docker compose) read as a log in
// the docked console, the classic console window and the live bar. Output
// streams in; the console shows a time gutter, aligned sources with chips to
// filter them, level badges, highlighted milestones whose ports open the Web
// panel, files that open the editor, folded address lists and repeats, and
// "Hide noise" remembered per command. Copy stays the raw output. While it
// streams, the console's chrome does not move, and a long log stays windowed.
// Screenshots: .cache/console-log-{dark,light}.png (maximized), -docked.png, and console-log-window.png.
import { testViteCache } from './fixtures/vite-cache.mjs';
import { createServer } from 'vite';
import react from '@vitejs/plugin-react';
import { chromium, expect } from '@playwright/test';
import { fileURLToPath } from 'node:url';
import { mkdir } from 'node:fs/promises';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';

const E = '\x1b';
const addresses = n => Array.from({ length: n }, (_, i) => i % 3 === 2 ? `[2a01:cb0d:5c4:${i}::1]:57771` : `192.168.${i}.21:57771`).join(' ');
// The person's own paste, as Codefly wrote it, around a fuller multi-service run.
const OUTPUT = [
  '21:28:12 [codefly] starting 3 services in fte-tracker',
  `21:28:14 fte-tracker/web     > ${E}[1m${E}[35m▲ Next.js 15.0.3${E}[0m`,
  '21:28:14 fte-tracker/web     > - Experiments (use with caution):',
  '21:28:14 fte-tracker/web     >   · turbo',
  '21:28:15 fte-tracker/web     > Next.js development server running on port 57771',
  '>> fte-tracker/web: Running on :57771',
  '21:28:15 authority/edge      | (INFO) Building go binary',
  `(WARN) (Flow.checkLoopback) a native service answers off loopback; bind it to 127.0.0.1 answers=[100.77.139.126:57771 ${addresses(8)}]`,
  '21:28:16 authority/edge      | (INFO) GET /healthz 200 in 2ms',
  '21:28:17 authority/edge      | (INFO) GET /healthz 200 in 2ms',
  '21:28:18 authority/edge      | (INFO) GET /healthz 200 in 2ms',
  '[codefly] still starting authority/edge… 5s',
  '[codefly] still starting authority/edge… 10s',
  '21:28:19 authority/edge      | (ERROR) migrations failed: internal/store/migrate.go:88: relation "fte" does not exist',
  '    at internal/store/migrate.go:88 (Migrate)',
  `(WARN) (Flow.checkLoopback) a native service answers off loopback; bind it to 127.0.0.1 answers=[10.0.0.4:57771 ${addresses(5)}]`,
  'db-1  | 2026-11-16T21:28:19Z LOG:  database system is ready to accept connections',
  '21:28:20 fte-tracker/web     > ✓ Ready in 2.1s',
  '21:28:21 fte-tracker/web     > - Local:        http://localhost:57771',
  '>> authority/edge: Running in 15.2s',
].join('\n') + '\n';
const plain = OUTPUT.replace(/\x1b\[[0-9;]*m/g, '');

const fixture = await startRunnerFixture();
const session = fixture.sessions[0];
const run = fixture.streamCommand(session.id, 'codefly run', ['codefly', 'run']);
const server = await createServer({ ...testViteCache(import.meta.url), configFile: false, logLevel: 'error', root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: fixture.url } } } });
let browser;
try {
  await server.listen(); await mkdir('.cache', { recursive: true });
  const base = `http://127.0.0.1:${server.httpServer.address().port}`;
  browser = await chromium.launch();
  const context = await browser.newContext({ viewport: { width: 1440, height: 960 }, colorScheme: 'dark' });
  // What Copy puts on the clipboard.
  await context.addInitScript(() => { window.__copied = []; Object.defineProperty(navigator, 'clipboard', { value: { writeText: async text => { window.__copied.push(text); }, readText: async () => window.__copied.at(-1) || '' } }); });
  // The dev server the log announces: a page, so a browser tab shows where it went.
  await context.route(/^http:\/\/localhost:5777[0-9]\//, route => route.fulfill({ contentType: 'text/html', body: '<h1>fte-tracker</h1>' }));
  const page = await context.newPage(); page.setDefaultTimeout(15000);
  const errors = []; page.on('pageerror', error => errors.push(String(error)));
  page.on('console', message => { if (message.type() === 'error' && /Maximum update depth|ResizeObserver loop|key/.test(message.text())) errors.push(message.text().slice(0, 300)); });
  await page.goto(`${base}/?view=workspace&collection=alpha&session=${session.id}#token=${fixture.token}`);
  await page.getByRole('tab', { name: 'Command output', exact: true }).click();
  const panel = page.locator('.command-panel');
  await expect(panel.locator('select')).toHaveValue(run.id);
  const output = panel.getByTestId('console-output');
  const rows = output.locator('.log-row');

  // It streams: half a line first, then the rest.
  run.write(OUTPUT.slice(0, 30));
  await expect(rows.first()).toContainText('starting 3');
  run.write(OUTPUT.slice(30));
  await expect(output).toContainText('Running in 15.2s');

  // Typography: the bundled JetBrains Mono (no code font is set), tabular figures, theme colours.
  const type = await output.evaluate(async node => { await document.fonts.ready; const style = getComputedStyle(node); return { family: style.fontFamily, numeric: style.fontVariantNumeric, loaded: document.fonts.check(`12px "JetBrains Mono Variable"`) }; });
  assert.match(type.family, /JetBrains Mono Variable/, 'the console reads in the bundled monospace');
  assert.ok(type.loaded, 'the bundled font is loaded');
  assert.match(type.numeric, /tabular-nums/);

  // The time gutter and the source column line up across rows.
  const first = rows.first();
  await expect(first.locator('.log-time')).toHaveText('21:28:12');
  const web = rows.filter({ hasText: 'Experiments' });
  await expect(web.locator('.log-source-name')).toHaveText('fte-tracker/web');
  await expect(web.locator('.log-sep')).toHaveText('>');
  const columns = await rows.evaluateAll(nodes => nodes.filter(row => row.querySelector('.log-time')?.textContent).map(row => Math.round(row.querySelector('.log-msg').getBoundingClientRect().left)));
  assert.equal(new Set(columns).size, 1, `every message starts in one column (${[...new Set(columns)].join(', ')})`);
  // Sources keep distinct colours from the theme's palette.
  const colours = await output.evaluate(node => [...new Set([...node.querySelectorAll('.log-source')].map(s => s.style.getPropertyValue('--log-source')).filter(Boolean))]);
  assert.ok(colours.length >= 3 && colours.every(value => /^var\(--log-source-[1-6]\)$/.test(value)), colours.join(' '));

  // Levels are badges; an error row is tinted.
  await expect(rows.filter({ hasText: 'Building go binary' }).locator('.log-level')).toHaveText('INFO');
  await expect(output.locator('.log-level[data-level=warn]').first()).toHaveText('WARN');
  const error = rows.filter({ hasText: 'migrations failed' });
  await expect(error).toHaveAttribute('data-level', 'error');
  assert.notEqual(await error.evaluate(node => getComputedStyle(node).backgroundColor), 'rgba(0, 0, 0, 0)', 'an error row is tinted');
  // The stack line under it is aligned with its message, not the gutter.
  const [errorMessage, frameMessage] = await Promise.all([error.locator('.log-msg').boundingBox(), rows.filter({ hasText: '(Migrate)' }).locator('.log-msg').boundingBox()]);
  assert.ok(Math.abs(errorMessage.x - frameMessage.x) < 1, 'a continuation hangs under the message');

  // Milestones stand out, and their port opens the Web panel.
  const milestones = output.locator('.log-row[data-milestone]');
  await expect(milestones).toHaveCount(5);
  await expect(milestones.filter({ hasText: 'Running on :57771' }).locator('.log-marker')).toHaveText('>>');
  await milestones.filter({ hasText: 'Running on :57771' }).getByRole('link', { name: ':57771' }).click();
  const viewer = page.getByRole('region', { name: 'Web page' }).first();
  await expect(viewer.getByRole('textbox', { name: 'Address', exact: true })).toHaveValue('http://localhost:57771/');
  // ⌘/Ctrl-click opens the browser instead.
  const [tab] = await Promise.all([context.waitForEvent('page'), milestones.filter({ hasText: 'Local:' }).getByRole('link', { name: 'http://localhost:57771' }).click({ modifiers: ['ControlOrMeta'] })]);
  await expect.poll(() => tab.url()).toMatch(/^http:\/\/localhost:57771\/?$/);
  await tab.close(); await page.bringToFront();
  await page.getByRole('tab', { name: 'Command output', exact: true }).click();

  // A file with a line opens in the editor.
  const resolves = () => fixture.calls.filter(call => call.method === 'ResolveWorkspacePath' && call.body.path === 'internal/store/migrate.go').length;
  await error.getByRole('link', { name: 'internal/store/migrate.go:88' }).click();
  await expect.poll(resolves).toBe(1);

  // Long address lists fold to a count, and open in place.
  const warning = rows.filter({ hasText: 'answers off loopback' }).first();
  const fold = warning.getByRole('button', { name: '… 9 addresses' });
  await expect(fold).toBeVisible();
  await expect(warning).not.toContainText('192.168.4.21:57771');
  await fold.click();
  await expect(warning).toContainText('192.168.4.21:57771');
  await warning.getByRole('button', { name: 'less' }).click();
  await expect(warning).not.toContainText('192.168.4.21:57771');

  // The same line three times is one row, ×3, which opens and folds again.
  const health = rows.filter({ hasText: 'GET /healthz' });
  await expect(health).toHaveCount(1);
  await expect(health.locator('.log-time')).toHaveText('21:28:18');
  await health.getByRole('button', { name: '×3' }).click();
  await expect(health).toHaveCount(3);
  await health.last().getByRole('button', { name: '×3' }).click();
  await expect(health).toHaveCount(1);

  // Source chips: hide one, show only one, and back.
  const chips = panel.getByRole('group', { name: 'Sources' });
  await expect(chips.locator('.log-chip-name')).toHaveText(['codefly', 'fte-tracker/web', 'authority/edge', 'db-1', 'Other']);
  await chips.getByRole('button', { name: /^authority\/edge/ }).click();
  await expect(chips.getByRole('button', { name: /^authority\/edge/ })).toHaveAttribute('aria-pressed', 'false');
  await expect(rows.filter({ hasText: 'Building go binary' })).toHaveCount(0);
  await expect(rows.filter({ hasText: '(Migrate)' })).toHaveCount(0);
  await chips.getByRole('button', { name: 'Only fte-tracker/web' }).click();
  await expect.poll(() => output.locator('.log-source-name').evaluateAll(nodes => [...new Set(nodes.map(node => node.textContent))])).toEqual(['fte-tracker/web']);
  await expect(output).not.toContainText('answers off loopback');
  await chips.getByRole('button', { name: 'Show all' }).click();
  await expect(output).toContainText('Building go binary');

  // Hide noise: the second loopback warning goes; the choice is the command's, kept across a reload.
  // The panel is narrow beside the Web page and the editor, so its one-row header keeps Hide noise,
  // A−/A+ and Copy in its ⋯ menu: maximized, they are back in the row.
  const menu = panel.locator('summary[aria-label="Output actions"]');
  await menu.click();
  await expect(page.getByRole('button', { name: /^Hide noise/ })).toBeVisible();
  await expect(page.getByRole('button', { name: /^Copy output/ })).toBeVisible();
  await page.keyboard.press('Escape');
  await page.getByRole('button', { name: 'Maximize Command output' }).click();
  const noise = panel.getByRole('button', { name: /Hide noise/ });
  await expect(noise).toContainText('1');
  await expect(rows.filter({ hasText: 'answers off loopback' })).toHaveCount(2);
  await noise.click();
  await expect(panel.getByRole('button', { name: /Noise hidden/ })).toHaveAttribute('aria-pressed', 'true');
  await expect(rows.filter({ hasText: 'answers off loopback' })).toHaveCount(1);
  await expect(rows.filter({ hasText: 'still starting' })).toHaveCount(1);
  await expect(rows.filter({ hasText: 'still starting' }).getByRole('button', { name: '×2' })).toBeVisible();


  // Copy is the output as written: every byte of text, padding and all, no escape codes.
  await panel.getByRole('button', { name: 'Copy output', exact: true }).click();
  assert.equal(await page.evaluate(() => window.__copied.at(-1)), plain, 'Copy gives the raw output');

  // While output streams, the console's chrome holds still and the end stays in view.
  {
    const watched = ['.command-panel .console-tools', '.command-panel .log-sources', '.command-panel [data-testid=console-output]'];
    const boxes = () => page.evaluate(selectors => selectors.map(selector => { const r = document.querySelector(selector).getBoundingClientRect(); return [r.x, r.y, r.width, r.height].map(Math.round).join(','); }), watched);
    const before = await boxes();
    for (let i = 0; i < 40; i++) { run.write(`21:29:${String(i % 60).padStart(2, '0')} authority/edge      | (INFO) request ${i} served in ${i % 7}ms\n`); await page.waitForTimeout(20); }
    await expect(output).toContainText('request 39 served');
    assert.deepEqual(await boxes(), before, 'the toolbar, chips and output box do not move while lines arrive');
    const end = await output.evaluate(node => node.scrollHeight - node.scrollTop - node.clientHeight);
    assert.ok(end < 4, `following keeps the newest line in view (${end}px from the end)`);
  }

  // A long log stays windowed: thousands of lines (within the 512 KiB kept), a few hundred rows in the page, and the top still reachable.
  {
    let chunk = '';
    for (let i = 0; i < 4500; i++) { chunk += `21:30:${String(i % 60).padStart(2, '0')} ${['fte-tracker/web     >', 'authority/edge      |'][i % 2]} (INFO) line ${i} of a long run with some words to wrap at narrow widths\n`; if (chunk.length > 64000) { run.write(chunk); chunk = ''; } }
    run.write(chunk);
    await expect(output).toContainText('line 4499 of a long run');
    const count = await rows.count();
    assert.ok(count < 900, `only nearby rows are in the page (${count})`);
    await output.hover(); for (let i = 0; i < 4 && await output.evaluate(node => node.scrollTop > 0); i++) { await page.mouse.wheel(0, -1e6); await page.waitForTimeout(100); }
    await expect(rows.first()).toContainText('starting 3 services');
    await expect(page.getByRole('button', { name: '↓ Latest output' })).toBeVisible();
    await page.getByRole('button', { name: '↓ Latest output' }).click();
    await expect(output).toContainText('line 4499 of a long run');
  }

  // Remembered per command: a reload keeps noise hidden (narrow again, the ⋯ menu says so).
  await page.getByRole('button', { name: 'Restore layout' }).first().click();
  await page.reload();
  await page.getByRole('tab', { name: 'Command output', exact: true }).click().catch(() => {});
  await page.locator('.command-panel summary[aria-label="Output actions"]').click();
  await expect(page.getByRole('button', { name: /^Show every line/ })).toHaveAttribute('aria-pressed', 'true');
  await page.keyboard.press('Escape');

  // Dark and light: the console maximized, and docked beside the conversation.
  const quiet = fixture.streamCommand(fixture.sessions[1].id, 'codefly run', ['codefly', 'run']);
  quiet.write(OUTPUT); quiet.finish(1);
  for (const scheme of ['dark', 'light']) {
    const shot = await browser.newContext({ viewport: { width: 1440, height: 900 }, colorScheme: scheme });
    const shotPage = await shot.newPage();
    await shotPage.goto(`${base}/?view=workspace&collection=beta&session=${fixture.sessions[1].id}#token=${fixture.token}`);
    await shotPage.getByRole('tab', { name: 'Command output', exact: true }).click();
    await expect(shotPage.locator('.command-panel select')).toHaveValue(quiet.id);
    const shown = shotPage.getByTestId('console-output');
    // Native controls in the dock (checkboxes, scrollbars) follow the theme: no dark checkboxes in a light window.
    assert.equal(await shotPage.locator('.dockview-theme-lazybox').first().evaluate(node => getComputedStyle(node).colorScheme), scheme);
    await expect(shown).toContainText('Running in 15.2s');
    await shotPage.locator('.command-panel').screenshot({ path: `.cache/console-log-${scheme}-docked.png` });
    await shotPage.getByRole('button', { name: 'Maximize Command output' }).click();
    await shown.locator('.log-row').filter({ hasText: 'GET /healthz' }).getByRole('button', { name: '×3' }).hover();
    await shotPage.screenshot({ path: `.cache/console-log-${scheme}.png` });
    await shot.close();
  }
  {
    const light = await browser.newContext({ viewport: { width: 1440, height: 960 }, colorScheme: 'light' });
    const lightPage = await light.newPage();

    // The classic console ("Open output in a window": in the browser build, the conversation window with its commands open) reads the same way.
    await lightPage.goto(`${base}/?session=${fixture.sessions[1].id}&command=${quiet.id}#token=${fixture.token}`);
    const classic = lightPage.locator('.session-commands .console');
    await expect(classic.locator('.log-row[data-milestone]')).toHaveCount(5);
    await expect(classic.getByRole('group', { name: 'Sources' })).toBeVisible();
    await expect(classic.locator('.log-level[data-level=error]')).toHaveText('ERROR');
    await lightPage.screenshot({ path: '.cache/console-log-window.png' });
    await light.close();
  }

  // The live bar's tail reads the same way: its last lines, with their badges and sources.
  {
    const now = Date.now();
    Object.assign(session, { busy: true, status: 'running', agents: [{ id: 'root', status: 'running', phase: 'tool', tool: 'Bash', phaseSinceUnixMillis: String(now - 5000), turnStartedAtUnixMillis: String(now - 6000) }] });
    const channel = fixture.openEvents(session.id);
    const b64 = value => Buffer.from(JSON.stringify(value)).toString('base64');
    channel.push({ type: 'turn_started' }, new Date(now - 6000).toISOString());
    channel.push({ type: 'activity', status: 'tool', name: 'Bash' }, new Date(now - 5000).toISOString());
    channel.push({ type: 'tool_activity', providerDataJson: b64({ type: 'tool_call', id: 'toolu_run', name: 'Bash', status: 'running', input: { command: 'lazybox run codefly run' } }) }, new Date(now - 5000).toISOString());
    run.write('21:31:00 authority/edge      | (WARN) slow query 1.2s\n>> authority/edge: Running on :57772\n');
    await page.reload();
    const bar = page.getByRole('region', { name: 'What the agent is doing' }).first();
    const tail = bar.locator('.live-bar-output');
    await expect(tail.locator('.log-row')).toHaveCount(3);
    await expect(tail.locator('.log-level[data-level=warn]')).toHaveText('WARN');
    await expect(tail.locator('.log-row[data-milestone]')).toContainText('Running on :57772');
    assert.equal((await tail.textContent()).split('\n').length, 3, 'three lines, as before');
    await bar.screenshot({ path: '.cache/console-log-live-bar.png' });
  }

  assert.deepEqual(errors, []);
  console.log('PASS console log: a multi-service log streams into a time gutter and aligned, coloured sources with chips that filter them, level badges, milestones whose ports open the Web panel (⌘-click: the browser), files that open the editor, folded address lists and repeats, and noise hidden per command across a reload; Copy stays raw; the chrome holds still while streaming; 4500 lines (430 KB) stay windowed; the classic window and the live bar read the same way. Fixture only.');
} finally {
  await browser?.close(); await server.close(); await fixture.close();
}
