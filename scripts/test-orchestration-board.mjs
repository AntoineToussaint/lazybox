// The missions board as a page of its own (sota-agents.md §6.8, §6.11)
// against the fake Runner: ⌘K opens it in the main area with its columns;
// tabs (←/→) show the cross-agent Timeline (a lane per actor, what each
// event reacts to one click away), the Messages between agents as threads,
// and the Leases with the overlaps seen after each turn; a card flags its
// overlap; review full holds new agents, said on the board. The Runner's
// rules (leases, live overlap from checkpoints, the hold) are Go tests
// (work/lease_test.go, cmd/lazybox/lease_test.go, transport/plans_test.go).
//
// Screenshots, dark and light, in .cache/orchestration-{page,timeline,messages,leases}-*.png.
import { createServer } from 'vite';
import react from '@vitejs/plugin-react';
import { chromium, webkit, expect } from './fixtures/playwright.mjs';
import { fileURLToPath } from 'node:url';
import { mkdirSync } from 'node:fs';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';
import { testViteCache } from './fixtures/vite-cache.mjs';

const shots = fileURLToPath(new URL('../.cache/', import.meta.url));
mkdirSync(shots, { recursive: true });
let browser, server, fixture;
try {
  browser = await (process.env.TEST_CHROMIUM ? chromium : webkit).launch();
  for (const scheme of ['dark', 'light']) {
    fixture = await startRunnerFixture();
    server = await createServer({ ...testViteCache(import.meta.url), configFile: false, root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: fixture.url } } } });
    await server.listen();
    await run(scheme);
    await server.close(); server = undefined;
    await fixture.close(); fixture = undefined;
  }
  console.log('orchestration board: page, timeline with causes, message threads, leases and overlaps, review hold pass (dark and light)');
} finally {
  await browser?.close();
  await server?.close();
  await fixture?.close();
}

async function run(scheme) {
  fixture.sessions.find(s => s.id === 'alpha-session').title = 'Search lead';
  fixture.openEvents('alpha-session');
  const m = fixture.missions.create('Ship search');
  const granted = fixture.missions.handle('UpdateMission', { missionId: m.id, grant: { sessionId: 'alpha-session', ceiling: { maxChildren: 3, reviewLimit: 1 } } });
  assert.ok(!granted.error, JSON.stringify(granted.error));
  const index = fixture.missions.spawnChild('alpha-session', { title: 'Index the corpus', brief: 'Objective: a token index' });
  const rank = fixture.missions.spawnChild('alpha-session', { title: 'Rank results', role: 'researcher', permission: 'read-only' });
  fixture.missions.agentMessage('alpha-session', index.id, 'Skip vendor/ and testdata/.');
  fixture.missions.agentMessage(index.id, 'alpha-session', 'Found the tokenizer in search/lex.go; see run r_12.');
  fixture.missions.agentMessage('alpha-session', rank.id, 'Wait for the index before ranking.');
  const tasks = fixture.missions.missions.find(v => v.id === m.id).tasks;
  const indexTask = tasks.find(t => t.title === 'Index the corpus'), rankTask = tasks.find(t => t.title === 'Rank results');
  fixture.missions.addLease(m.id, { taskId: indexTask.id, path: 'search/lex/', mode: 'exclusive', reason: 'rewriting the lexer' });
  fixture.missions.addOverlap(m.id, { taskIds: [indexTask.id, rankTask.id], paths: ['search/rank.go'] });
  fixture.missions.reportChild(index.id, 'Indexed 1,204 files; go test ./search passed.');

  const context = await browser.newContext({ viewport: { width: 1440, height: 900 }, colorScheme: scheme });
  const page = await context.newPage();
  const errors = []; page.on('pageerror', error => errors.push(String(error)));
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`);
  await expect(page.locator('.panel-chat').getByRole('textbox', { name: 'Message', exact: true })).toBeVisible();

  // ⌘K → the board opens as a page of its own, with room for its columns.
  await page.keyboard.press('Meta+k');
  await page.keyboard.type('missions board');
  await page.getByRole('option', { name: /Open the missions board/ }).click();
  const panel = page.getByRole('region', { name: 'Missions and tasks', exact: true });
  await expect(panel).toBeVisible();
  const width = await panel.evaluate(node => node.getBoundingClientRect().width);
  assert.ok(width > 1000, `the board has the main area (${width}px)`);
  await panel.locator('.mission-picker > summary').click();
  await page.getByRole('group', { name: /^Mission: .* menu$/ }).getByRole('button').first().click();
  const columns = await panel.locator('.mission-column').evaluateAll(nodes => new Set(nodes.map(n => Math.round(n.getBoundingClientRect().top))).size);
  assert.equal(columns, 1, 'the four columns stand side by side');
  // A card flags its overlap; review full holds new agents.
  await expect(panel.locator(`.mission-card[data-task="${rankTask.id}"] .flag-overlap`)).toContainText('Overlaps “Index the corpus”');
  await expect(panel.locator('.mission-review-held')).toContainText('1 task waits for your review; new agents are held');
  await page.screenshot({ path: `${shots}orchestration-page-${scheme}.png` });

  // Tabs: ←/→ move, the choice stays.
  const tabs = panel.getByRole('tablist', { name: 'Mission views' });
  await tabs.getByRole('tab', { name: 'Board' }).focus();
  await page.keyboard.press('ArrowRight');
  await expect(tabs.getByRole('tab', { name: /Timeline/ })).toHaveAttribute('aria-selected', 'true');
  await expect(tabs.getByRole('tab', { name: /Timeline/ })).toBeFocused();

  // Timeline: a lane per actor, every event in order, causes one click away.
  const timeline = panel.getByRole('table', { name: 'Mission timeline' });
  await expect(timeline.locator('.timeline-lane-head')).toHaveCount(5);
  await expect(timeline.locator('.timeline-lane-head').first()).toContainText('You');
  await expect(timeline.locator('.timeline-lane-head.tone-orchestrator')).toHaveAttribute('title', 'Search lead');
  const gate = timeline.locator('.timeline-row').filter({ hasText: 'Waiting for you to accept “Index the corpus”' });
  await expect(gate.locator('.timeline-cause')).toBeVisible();
  await gate.locator('.timeline-cause').click();
  await expect(timeline.locator('.timeline-row.lit')).toContainText('reported done');
  await expect(timeline.locator('.timeline-row').filter({ hasText: 'Overlap:' })).toBeVisible();
  await page.screenshot({ path: `${shots}orchestration-timeline-${scheme}.png` });

  // Messages: one thread per pair, who said what.
  await tabs.getByRole('tab', { name: /Messages/ }).click();
  const threads = panel.locator('.message-thread');
  await expect(threads).toHaveCount(2);
  // ListMissions omits history and messages. A list refresh keeps the open thread.
  const listRead = page.waitForResponse(response => response.url().endsWith('/ListMissions'));
  await panel.getByRole('button', {name: 'Refresh missions'}).click();
  await listRead;
  await expect(threads.first()).toContainText('Index the corpus');
  await expect(panel.locator('.thread-message.kind-report')).toContainText('Indexed 1,204 files');
  await page.screenshot({ path: `${shots}orchestration-messages-${scheme}.png` });

  // Leases and overlaps.
  await tabs.getByRole('tab', { name: /Leases/ }).click();
  await expect(panel.locator('.mission-leases tbody tr')).toHaveCount(1);
  await expect(panel.locator('.mission-leases tbody tr')).toContainText('search/lex/');
  await expect(panel.locator('.lease-mode')).toHaveText('exclusive');
  await expect(panel.locator('.overlap-list li')).toContainText('Index the corpus and Rank results both changed search/rank.go');
  await page.screenshot({ path: `${shots}orchestration-leases-${scheme}.png` });
  await tabs.getByRole('tab', { name: 'Board' }).click();
  await expect(panel.locator('.mission-columns')).toBeVisible();
  assert.deepEqual(errors, []);
  await context.close();
}
