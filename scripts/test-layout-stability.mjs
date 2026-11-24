// Fixture only: nothing on screen moves because a clock ticked. A running turn
// with failing sub-agents and background commands is watched for about six
// seconds while its elapsed times cross a minute and its status phrase
// changes; the boxes of the chat header's items, the live bar, the sub-agent
// chips and the workspace rows must stay where they are (1px), in WebKit and
// Chromium, and Chromium's own layout-shift entries must add up to nothing.
// LAYOUT_STABILITY_REPORT=1 prints every moving box instead of failing on the first.
import { testViteCache } from './fixtures/vite-cache.mjs';
import { createServer } from './fixtures/vite-cache.mjs';
import react from '@vitejs/plugin-react';
import { chromium, webkit, expect } from './fixtures/playwright.mjs';
import { fileURLToPath } from 'node:url';
import { mkdir } from 'node:fs/promises';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';
import { collectPageErrors } from './fixtures/page-errors.mjs';

const fixture = await startRunnerFixture();
const b64 = value => Buffer.from(JSON.stringify(value)).toString('base64');
const session = fixture.sessions[0];
const channel = fixture.openEvents(session.id);
const push = (event, at = Date.now()) => channel.push(event.data === undefined ? event : { ...Object.fromEntries(Object.entries(event).filter(([key]) => key !== 'data')), providerDataJson: b64(event.data) }, new Date(at).toISOString());
/** A turn whose clocks are about to cross a minute, with sub-agents (one failed) and a background command. */
const seed = () => {
  const now = Date.now();
  Object.assign(session, { title: 'Stabilise the storefront checkout flow before the release', provider: 'claude', model: 'claude-opus-4-8-20260115', busy: true, status: 'running',
    agents: [{ id: 'root', status: 'running', phase: 'model', phaseSinceUnixMillis: String(now - 56500), turnStartedAtUnixMillis: String(now - 56500) },
      ...['Review the API', 'Audit the checkout tests', 'Trace the flaky spec'].map((name, i) => ({ id: `claude-tool:toolu_${i}`, parentId: 'root', status: i === 2 ? 'failed' : 'running', name, kind: 'local_agent' }))] });
  push({ type: 'operation_accepted', text: 'Make the checkout flow stable.' }, now - 58000);
  push({ type: 'turn_started' }, now - 57500);
  push({ type: 'message_delta', text: 'Looking at the checkout flow now.' }, now - 57000);
  ['Review the API', 'Audit the checkout tests', 'Trace the flaky spec'].forEach((name, i) => {
    push({ type: 'tool_activity', data: { type: 'tool_call', id: `toolu_${i}`, name: 'Task', status: i === 2 ? 'failed' : 'running', input: { description: name, prompt: 'Do it.', subagent_type: 'Explore' } } }, now - 56800);
    push({ type: 'agent_status', agentId: `claude-tool:toolu_${i}`, parentAgentId: 'root', nativeId: `task-${i}`, status: i === 2 ? 'failed' : 'running', name, data: { toolUseId: `toolu_${i}`, subagentType: 'Explore' } }, now - 56700 + i * 100);
  });
  push({ type: 'tool_activity', data: { type: 'tool_call', id: 'toolu_bad', name: 'Bash', status: 'failed', input: { command: 'pnpm test', description: 'Run the tests' }, output: 'exit 1' } }, now - 56600);
  push({ type: 'activity', status: 'thinking' }, now - 56500);
};
seed();

const server = await createServer({ ...testViteCache(import.meta.url), configFile: false, logLevel: 'error', root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: fixture.url } } } });
const report = !!process.env.LAYOUT_STABILITY_REPORT;

/** What must not move, by a name a person can read in a failure. Text that ticks is watched through its slot. */
const WATCHED = {
  'chat header': '.panel-chat .chat-heading',
  'chat title': '.panel-chat .conversation-title',
  'header meta item': '.panel-chat .chat-heading-meta > *',
  'header tool': '.panel-chat .chat-heading-actions button, .panel-chat .chat-heading-actions summary',
  'live bar': '.panel-chat .live-bar',
  // The turning ring's box changes as it turns; everything else in the row is watched.
  'live bar part': '.panel-chat .live-bar-row > *:not(.live-indicator)',
  'live bar button': '.panel-chat .live-bar button',
  'sub-agent chip': '.panel-chat .subagent-chip',
  'tray count': '.panel-chat .subagent-tray-count',
  // (The transcript itself scrolls as tool lines arrive: its cards are content, not chrome.)
  'failed chip': '.panel-chat .subagent-attention',
  'message box': '.panel-chat .composer',
  'messages': '.panel-chat .messages',
  'workspace row': '.workspace-row',
  'workspace row line': '.workspace-row .workspace-link > *',
  'page header tool': '.workspace-header-actions > *',
};

async function watch(page, label, during) {
  await page.evaluate(watched => {
    const boxes = new Map(), moved = new Map();
    const key = (name, node, index) => `${name} #${index} "${(node.getAttribute('aria-label') || node.textContent || '').trim().replace(/\s+/g, ' ').replace(/\d+/g, 'N').slice(0, 40)}"`;
    const sample = () => {
      for (const [name, selector] of Object.entries(watched)) [...document.querySelectorAll(selector)].forEach((node, index) => {
        const r = node.getBoundingClientRect(); if (!r.width && !r.height) return;
        const id = key(name, node, index), first = boxes.get(id);
        if (!first) { boxes.set(id, r); return; }
        const delta = Math.max(Math.abs(r.x - first.x), Math.abs(r.y - first.y), Math.abs(r.width - first.width), Math.abs(r.height - first.height));
        if (delta > 1 && delta > (moved.get(id)?.delta || 0)) moved.set(id, { delta: Math.round(delta * 10) / 10, from: [first.x, first.y, first.width, first.height].map(Math.round).join(','), to: [r.x, r.y, r.width, r.height].map(Math.round).join(',') });
      });
    };
    window.__stability = { moved, timer: setInterval(sample, 100), shift: 0 };
    sample();
    try { new PerformanceObserver(list => { for (const entry of list.getEntries()) if (!entry.hadRecentInput) window.__stability.shift += entry.value; }).observe({ type: 'layout-shift', buffered: false }); } catch { window.__stability.shift = -1; }
  }, WATCHED);
  await during();
  const result = await page.evaluate(() => { clearInterval(window.__stability.timer); return { moved: [...window.__stability.moved].map(([id, value]) => `${id} moved ${value.delta}px (${value.from} → ${value.to})`), shift: window.__stability.shift }; });
  if (report) console.log(`--- ${label}: ${result.moved.length} moving, layout-shift ${result.shift}\n${result.moved.join('\n')}`);
  else {
    assert.deepEqual(result.moved, [], `${label}: nothing moves while the clocks tick`);
    if (result.shift >= 0) assert.ok(result.shift < 0.005, `${label}: cumulative layout shift is ${result.shift}`);
  }
}

let failures = 0;
try {
  await server.listen(); await mkdir('.cache', { recursive: true });
  const url = `http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`;
  for (const [engine, name] of [[webkit, 'WebKit'], [chromium, 'Chromium']]) {
    const browser = await engine.launch();
    try {
      for (const width of [1440, 1000]) {
        const page = await (await browser.newContext({ viewport: { width, height: 900 } })).newPage(); page.setDefaultTimeout(15000);
        const errors = []; await collectPageErrors(page, errors);
        // A render loop is a flicker of its own: React reports it on the console.
        page.on('console', message => { if (message.type() === 'error' && /Maximum update depth|ResizeObserver loop/.test(message.text())) errors.push(message.text().slice(0, 200)); });
        if (report) { await page.addInitScript(() => { const original = console.error; let n = 0; console.error = (...args) => { if (String(args[0]).includes('Maximum update depth') && n++ < 1) original('LOOPSTACK ' + new Error().stack); original(...args); }; }); page.on('console', message => { if (message.text().startsWith('LOOPSTACK')) console.log(message.text().split('\n').filter(line => !line.includes('node_modules')).join('\n').slice(0, 3000)); }); }
        // Each pass starts a little before the minute, so "59s" becomes "1m 0s" while it is watched.
        const now = Date.now();
        session.agents[0].phaseSinceUnixMillis = String(now - 56500); session.agents[0].turnStartedAtUnixMillis = String(now - 56500); session.agents[0].phase = 'model'; delete session.agents[0].tool;
        await page.goto(url);
        const chat = page.locator('.panel-chat');
        await expect(chat.getByRole('region', { name: 'What the agent is doing' })).toBeVisible();
        await expect(chat.locator('.subagent-chip').first()).toBeVisible();
        // The Run control arrives with the workspace's actions, once; the watch starts after the page has loaded.
        await expect(page.locator('.workspace-header .run-control')).toBeVisible();
        await page.waitForTimeout(600);
        await watch(page, `${name} ${width}px`, async () => {
          await page.waitForTimeout(2600);
          // The status phrase changes length mid-watch: the model hands over to a command, then to a file.
          Object.assign(session.agents[0], { phase: 'tool', tool: 'Bash', phaseSinceUnixMillis: String(Date.now()) });
          push({ type: 'activity', status: 'tool', name: 'Bash' });
          push({ type: 'tool_activity', data: { type: 'tool_call', id: `toolu_run_${name}_${width}`, name: 'Bash', status: 'running', input: { command: 'cd services/storefront && pnpm run test:checkout -- --reporter=dot', description: 'Run the checkout tests' } } });
          await page.waitForTimeout(1800);
          push({ type: 'tool_activity', data: { type: 'tool_call', id: `toolu_run_${name}_${width}`, status: 'completed', output: 'ok' } });
          Object.assign(session.agents[0], { phase: 'model', phaseSinceUnixMillis: String(Date.now()) }); delete session.agents[0].tool;
          push({ type: 'activity', status: 'thinking' });
          await page.waitForTimeout(1800);
        }).catch(error => { if (report) throw error; failures++; console.error(String(error.message || error)); });
        if (name === 'WebKit' && width === 1440) await page.screenshot({ path: '.cache/layout-stability.png' });
        assert.deepEqual(errors, []);
        await page.context().close();
      }
    } finally { await browser.close(); }
  }
  assert.equal(failures, 0, 'layout moved while only time passed');
  console.log(report ? 'REPORT only.' : 'PASS layout stability: over six seconds of ticking clocks and changing status, the chat header, live bar, sub-agent chips, message box and workspace rows keep their boxes (1px) at 1440 and 1000px in WebKit and Chromium; Chromium reports no layout shift. Fixture only.');
} finally { await server.close(); await fixture.close(); }
