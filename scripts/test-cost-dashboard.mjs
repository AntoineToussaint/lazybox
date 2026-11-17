// Fixture only: the Cost dashboard example extension (EXTENSIONS.md) with six
// weeks of usage across two workspaces, six conversations and five models.
// From an empty Extensions panel the person adds it and allows what its sheet
// says; it renders the total against the period before, the token and cache
// tiles, spend per day stacked by model with a tooltip, and the ranked lists;
// switching the period (by click and by keyboard) changes the numbers to what
// the usage says; a workspace's row narrows everything; a conversation's row
// shows it in the window. It works in a 320-pixel panel and a wide one, with
// no policy violation, no console error and no horizontal scroll. A Runner
// that reports no hours gets all time from the groups; no usage at all gets a
// calm empty state.
// Screenshots: .cache/cost-dashboard-{wide,narrow}-{dark,light}.png. WebKit,
// the browser build, the fake Runner (its GetTelemetry answers the series
// built here); the content origin and its policies are real.
import { testViteCache } from './fixtures/vite-cache.mjs';
import { createServer } from 'vite';
import react from '@vitejs/plugin-react';
import { webkit, expect } from '@playwright/test';
import { fileURLToPath } from 'node:url';
import { mkdir } from 'node:fs/promises';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';

const HOUR = 3600000, DAY = 24 * HOUR;
// The browser runs in UTC, so the dashboard's local days are UTC days here.
const now = Date.now(), today = now - (now % DAY), thisHour = now - (now % HOUR);

// Dollars per million tokens: input, cache read, cache write, output.
const rates = { 'claude-opus-4-6': [5, 0.5, 6.25, 25], 'claude-sonnet-4-6': [3, 0.3, 3.75, 15], 'gpt-5.1-codex': [1.25, 0.125, 0, 10], 'claude-haiku-4-5': [1, 0.1, 1.25, 5] };
const conversations = [
  { id: 'alpha', workspace: 'alpha', title: 'Fix the parser', models: ['claude-opus-4-6', 'claude-haiku-4-5'], days: [0, 1, 2, 3, 5, 8, 13, 20], weight: 1.6 },
  { id: 'c-alpha-2', workspace: 'alpha', title: 'Refactor the cache layer', models: ['claude-sonnet-4-6'], days: [0, 2, 4, 6, 9, 15, 33], weight: 1.1 },
  { id: 'c-alpha-3', workspace: 'alpha', title: 'Write the migration guide', models: ['gpt-5.1-codex'], days: [1, 3, 10, 11, 12, 26, 40], weight: 0.6 },
  { id: 'beta', workspace: 'beta', title: 'Ship the release', models: ['claude-sonnet-4-6', 'claude-haiku-4-5'], days: [0, 4, 5, 6, 7, 18, 19, 27], weight: 1.2 },
  { id: 'c-beta-2', workspace: 'beta', title: 'Investigate the flaky CI job on main', models: ['gpt-5.1-codex', 'local-llama'], days: [2, 3, 16, 17, 30, 41], weight: 0.8 },
  { id: 'c-beta-3', workspace: 'beta', title: 'Upgrade dependencies', models: ['claude-opus-4-6'], days: [8, 9, 22, 23, 24, 36, 44], weight: 1.4 },
];
let seed = 7;
const random = () => (seed = (seed * 16807) % 2147483647) / 2147483647;
const series = [];
for (const c of conversations) for (const day of c.days) for (const [m, model] of c.models.entries()) {
  for (let k = 0; k < 2; k++) {
    // Today's turns are in hours that have begun; other days' at 09:00 and 15:00.
    const at = day === 0 ? Math.max(today, thisHour - (k + m) * HOUR) : today - day * DAY + (9 + k * 6 + m) * HOUR;
    const input = Math.round((20 + random() * 60) * 1000 * c.weight), cacheRead = Math.round(input * (3 + random() * 6)), cacheWrite = Math.round(input * 0.3), output = Math.round(input * (0.15 + random() * 0.2));
    const rate = rates[model];
    const usd = rate ? (input * rate[0] + cacheRead * rate[1] + cacheWrite * rate[2] + output * rate[3]) / 1e6 : null;
    series.push({ at, workspace: c.workspace, conversation: c.id, model, provider: model.startsWith('gpt') ? 'codex' : model === 'local-llama' ? 'ollama' : 'claude', input, cacheRead, cacheWrite, output, usd, records: 1 + Math.floor(random() * 4) });
  }
}
const nanos = usd => String(Math.round(usd * 1e9));
const tokensOf = b => ({ input: String(b.input), output: String(b.output), cacheRead: String(b.cacheRead), cacheWrite5m: String(b.cacheWrite) });
function report(workspace) {
  const mine = series.filter(b => b.workspace === workspace);
  const groups = new Map();
  for (const b of mine) {
    const key = `${b.conversation}/${b.model}`;
    const g = groups.get(key) || { scope: { workspace, conversation: b.conversation, provider: b.provider }, model: b.model, input: 0, output: 0, cacheRead: 0, cacheWrite: 0, usd: null, records: 0, unpriced: 0 };
    g.input += b.input; g.output += b.output; g.cacheRead += b.cacheRead; g.cacheWrite += b.cacheWrite; g.records += b.records;
    if (b.usd === null) g.unpriced += b.records; else g.usd = (g.usd || 0) + b.usd;
    groups.set(key, g);
  }
  return {
    groups: [...groups.values()].map(g => ({ scope: g.scope, model: g.model, source: 'gateway', tokens: tokensOf(g), ...(g.usd === null ? {} : { nanos: nanos(g.usd) }), records: String(g.records), unpriced: String(g.unpriced) })),
    contexts: [], recent: [],
    series: mine.sort((a, b) => a.at - b.at).map(b => ({ hourUnixMillis: String(b.at), conversation: b.conversation, provider: b.provider, model: b.model, tokens: tokensOf(b), ...(b.usd === null ? {} : { nanos: nanos(b.usd) }), records: String(b.records), unpriced: String(b.usd === null ? b.records : 0) })),
  };
}
const dollars = v => new Intl.NumberFormat('en-US', { style: 'currency', currency: 'USD', minimumFractionDigits: 2, maximumFractionDigits: 2 }).format(v);
const spent = (from, to = Infinity, keep = () => true) => series.filter(b => b.at >= from && b.at < to && keep(b)).reduce((n, b) => n + (b.usd || 0), 0);

const f = await startRunnerFixture();
const usage = body => body.series ? report(body.workspace) : { ...report(body.workspace), series: [] };
f.telemetry = usage;
Object.assign(f.sessions.find(s => s.id === 'alpha-session'), { title: 'Fix the parser' });
Object.assign(f.sessions.find(s => s.id === 'beta-session'), { title: 'Ship the release' });
for (const c of conversations.filter(c => c.id.startsWith('c-'))) f.sessions.push({ id: c.id, collectionId: c.workspace, profileId: 'claude-default', title: c.title, status: 'ready', closed: true, capabilities: {}, pending: [] });

const server = await createServer({ ...testViteCache(import.meta.url), configFile: false, logLevel: 'error', root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: f.url } } } });
await server.listen(); await mkdir('.cache', { recursive: true });
const url = `http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${f.token}`;
const innerOf = page => page.frames().find(frame => frame.parentFrame() && frame.parentFrame().url().includes('/_lazybox/shell.html') && frame.url().endsWith('index.html'));

async function scenario(browser, scheme, full) {
  const context = await browser.newContext({ viewport: { width: 1600, height: 1000 }, colorScheme: scheme, timezoneId: 'UTC', locale: 'en-US' });
  const page = await context.newPage();
  page.setDefaultTimeout(10000);
  // Every error the extension's frames log, and every page error. (A screenshot
  // injects a style into each frame, which the policy refuses: not the extension's.)
  const errors = [];
  let shooting = false;
  page.on('pageerror', e => errors.push(String(e)));
  page.on('console', m => { if (m.type() === 'error' && m.location().url.includes('/x/') && !shooting) errors.push(`console: ${m.text()} @ ${m.location().url}`); });
  const shot = async (locator, name) => { shooting = true; await locator.screenshot({ path: `.cache/cost-dashboard-${name}-${scheme}.png` }); await page.waitForTimeout(150); shooting = false; };
  await page.goto(url);
  await expect(page.locator('.page-layout .panel-chat').getByRole('textbox', { name: 'Message', exact: true })).toBeEnabled();

  await page.locator('summary[aria-label="Panels"]').click();
  await page.getByRole('button', { name: 'New Extensions panel', exact: true }).click();
  const panel = page.locator('.extension-home, .extension-panel').first();
  if (full) {
    await panel.locator('li', { hasText: 'Cost dashboard' }).getByRole('button', { name: 'Add' }).click();
    const sheet = page.getByRole('dialog', { name: 'Allow Cost dashboard?' });
    const can = sheet.getByRole('list', { name: 'What it can do' });
    await expect(can.locator('li')).toHaveCount(3);
    for (const text of ['See token use and cost', 'See your conversations', 'Show a conversation']) await expect(can).toContainText(text);
    await expect(sheet).toContainText('Nothing: it cannot use the network.');
    await sheet.getByRole('button', { name: 'Allow and turn on' }).click();
    await expect(sheet).toHaveCount(0);
    assert.deepEqual(f.mutations.filter(m => m.method === 'ApproveExtension').at(-1).body, { id: 'cost-dashboard', grants: ['read:usage', 'read:sessions', 'show:conversations'] });
  } else {
    const picker = page.getByRole('combobox', { name: 'Extension panel' });
    if (await picker.count()) await picker.selectOption({ label: 'Cost dashboard' });
    else await panel.locator('.extension-tile', { hasText: 'Cost dashboard' }).click();
  }

  const board = page.frameLocator('iframe.extension-frame').frameLocator('iframe');
  const frameEl = page.locator('iframe.extension-frame');
  // ---- The header: 7 days by default, the total and the delta against the 7 days before.
  const week = spent(today - 6 * DAY), weekBefore = spent(today - 13 * DAY, today - 6 * DAY);
  await expect(board.locator('#total')).toHaveText(dollars(week));
  await expect(board.getByRole('radio', { name: '7 days' })).toHaveAttribute('aria-checked', 'true');
  const change = Math.round(Math.abs(week - weekBefore) / weekBefore * 100);
  await expect(board.locator('#delta')).toContainText(`${week > weekBefore ? '▲' : '▼'} ${change}%`);
  await expect(board.locator('#delta')).toContainText(`vs ${dollars(weekBefore)} the 7 days before`);
  await expect(board.locator('#hero-note')).toContainText('unpriced');
  // ---- The tiles.
  const inWeek = series.filter(b => b.at >= today - 6 * DAY);
  const input = inWeek.reduce((n, b) => n + b.input + b.cacheRead + b.cacheWrite, 0), cached = inWeek.reduce((n, b) => n + b.cacheRead, 0);
  await expect(board.locator('#cache-rate')).toHaveText(`${Math.round(cached / input * 100)}%`);
  for (const id of ['#tokens-in', '#tokens-out']) await expect(board.locator(id)).not.toHaveText('–');
  const byConversation = new Map();
  for (const b of inWeek) byConversation.set(b.conversation, (byConversation.get(b.conversation) || 0) + (b.usd || 0));
  const [topId, topUsd] = [...byConversation].sort((a, b) => b[1] - a[1])[0];
  await expect(board.locator('#top-cost')).toHaveText(dollars(topUsd));
  await expect(board.locator('#top-title')).toHaveText(conversations.find(c => c.id === topId).title);
  // ---- Spend per day: seven columns, stacked by model, a legend, a tooltip on hover.
  await expect(board.getByRole('heading', { name: 'Spend per day' })).toBeVisible();
  await expect(board.locator('#chart-svg .col')).toHaveCount(7);
  assert.ok(await board.locator('#chart-svg path.seg').count() >= 7, 'stacked segments drawn');
  await expect(board.locator('#legend li')).not.toHaveCount(0);
  const col = board.locator('#chart-svg .col').nth(6).locator('.hit');
  await col.hover();
  await expect(board.locator('#tooltip')).toBeVisible();
  await expect(board.locator('#tooltip .sum')).toHaveText(dollars(spent(today)));
  // ---- The lists, ranked, with the share of the period.
  await expect(board.locator('#by-model .row')).toHaveCount(new Set(inWeek.map(b => b.model)).size);
  await expect(board.locator('#by-workspace .row')).toHaveCount(2);
  await expect(board.locator('#by-conversation .row').first()).toContainText(conversations.find(c => c.id === topId).title);
  await expect(board.locator('#by-conversation .row').first()).toContainText(`${Math.round(topUsd / week * 100)}%`);

  if (full) {
    // ---- The period: a click, then the keyboard.
    await board.getByRole('radio', { name: '30 days' }).click();
    await expect(board.locator('#total')).toHaveText(dollars(spent(today - 29 * DAY)));
    await expect(board.locator('#chart-svg .col')).toHaveCount(30);
    await board.getByRole('radio', { name: '30 days' }).press('ArrowRight');
    await expect(board.getByRole('radio', { name: 'All' })).toBeFocused();
    await expect(board.locator('#total')).toHaveText(dollars(spent(0)));
    await expect(board.locator('#delta')).toBeHidden();
    await board.getByRole('radio', { name: 'All' }).press('Home');
    await expect(board.getByRole('radio', { name: 'Today' })).toHaveAttribute('aria-checked', 'true');
    await expect(board.locator('#total')).toHaveText(dollars(spent(today)));
    await expect(board.getByRole('heading', { name: 'Spend per hour' })).toBeVisible();
    await expect(board.locator('#chart-svg .col')).toHaveCount(24);
    await board.getByRole('radio', { name: 'Today' }).press('ArrowRight');
    await expect(board.locator('#total')).toHaveText(dollars(week));
    // The chart by keyboard: focus reads the latest column; the arrows move.
    await board.locator('#chart').focus();
    await expect(board.locator('#tooltip')).toBeVisible();
    await board.locator('#chart').press('ArrowLeft');
    await expect(board.locator('#tooltip .sum')).toHaveText(dollars(spent(today - DAY, today)));
    await board.locator('#chart').press('Escape');
    await expect(board.locator('#tooltip')).toBeHidden();

    // ---- A workspace's row narrows everything; the chip clears it.
    await board.locator('#by-workspace .row', { hasText: 'Beta release' }).click();
    await expect(board.locator('#total')).toHaveText(dollars(spent(today - 6 * DAY, Infinity, b => b.workspace === 'beta')));
    await expect(board.locator('#by-workspace .row', { hasText: 'Beta release' })).toHaveAttribute('aria-pressed', 'true');
    await expect(board.locator('#hero-label')).toHaveText('Spent in the last 7 days · Beta release');
    await board.getByRole('button', { name: 'Clear Workspace: Beta release' }).click();
    await expect(board.locator('#total')).toHaveText(dollars(week));

    // ---- A conversation's row shows it in the window (one of this workspace's: another's moves to its page).
    await f.power.waitForWindow();
    await board.locator('#by-conversation .row', { hasText: 'Fix the parser' }).click();
    await expect.poll(() => [...f.power.requests.values()].some(r => r.request.kind === 'open' && r.request.sessionId === 'alpha-session' && r.request.state === 'done')).toBe(true);
    await page.keyboard.press('Escape');
  }

  // ---- The panel as the dock gives it; then maximized, its frame wide,
  // medium and 320 pixels: no horizontal scroll, every part still there.
  let inner; await expect.poll(() => { inner = innerOf(page); return !!inner; }).toBe(true);
  const overflow = () => inner.evaluate(() => document.querySelector('.page').scrollWidth - document.querySelector('.page').clientWidth);
  await page.mouse.move(0, 0);
  await shot(frameEl, 'docked');
  await page.locator('.dv-tab').filter({ has: page.locator('[data-kind="extension"]') }).dblclick();
  await expect(page.getByRole('button', { name: 'Restore layout' }).first()).toBeVisible();
  for (const [name, width] of [['wide', 1360], ['medium', 680], ['narrow', 320]]) {
    await frameEl.evaluate((node, width) => Object.assign(node.style, { width: `${width}px`, flex: '1 0 auto', alignSelf: 'flex-start' }), width);
    await expect.poll(() => inner.evaluate(() => innerWidth)).toBe(width);
    await expect(board.locator('#total')).toHaveText(dollars(week));
    await expect(board.locator('#chart-svg .col')).toHaveCount(7);
    assert.ok(await overflow() <= 0, `no horizontal scroll at ${width} px`);
    await shot(frameEl, name);
    if (name === 'wide') {
      await board.locator('#chart-svg .col').nth(4).locator('.hit').hover();
      await expect(board.locator('#tooltip')).toBeVisible();
      await shot(frameEl, 'wide-tooltip');
      await page.mouse.move(0, 0);
      for (const [period, file] of [['30 days', 'wide-30-days'], ['Today', 'wide-today'], ['7 days', '']]) {
        await board.getByRole('radio', { name: period }).click();
        if (file) await shot(frameEl, file);
      }
    }
  }
  const tiles = await inner.evaluate(() => [...document.querySelectorAll('.tile')].map(t => Math.round(t.getBoundingClientRect().width)));
  assert.ok(tiles.every(w => w >= 120), `tiles fit two across: ${tiles}`);
  await inner.evaluate(() => document.querySelector('.page').scrollTo(0, 99999));
  await shot(frameEl, 'narrow-end');

  if (!full) {
    // ---- A Runner that reports no hours: all time from the groups, said, and no chart.
    f.telemetry = body => ({ ...report(body.workspace), series: [] });
    await page.getByRole('button', { name: 'Reload the extension' }).click();
    await expect(board.locator('#notice')).toContainText('does not report usage over time');
    await expect(board.locator('#total')).toHaveText(dollars(spent(0)));
    await expect(board.getByRole('radio', { name: 'All' })).toHaveAttribute('aria-checked', 'true');
    await expect(board.getByRole('radio', { name: '7 days' })).toBeDisabled();
    await expect(board.locator('.spend')).toBeHidden();
    // ---- Nothing spent yet: calm, and it says why.
    f.telemetry = () => ({ groups: [], contexts: [], recent: [], series: [] });
    await page.getByRole('button', { name: 'Reload the extension' }).click();
    await expect(board.getByRole('heading', { name: 'Nothing spent yet' })).toBeVisible();
    await expect(board.locator('#board')).toBeHidden();
    await shot(frameEl, 'narrow-empty');
    f.telemetry = usage;
  }

  // No policy violation, no refusal and no error, in the panel or the app.
  await expect(page.locator('.extension-notice')).toHaveCount(0);
  assert.deepEqual(errors, []);
  await context.close();
}

let browser;
try {
  browser = await webkit.launch();
  await scenario(browser, 'dark', true);
  await scenario(browser, 'light', false);
  console.log('cost dashboard: ok');
} finally {
  await browser?.close();
  await server.close();
  await f.close();
}
