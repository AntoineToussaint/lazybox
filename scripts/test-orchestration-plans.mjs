// Plan approval and budgets (sota-agents.md §6.9, §6.11) against the fake
// Runner: an orchestrator's plan waits for the person in its conversation,
// the inbox and the board; one sheet shows every task in order, the paths
// two tasks share, the estimate, and approves it with a $ cap and a fan-out;
// a tree at its cap pauses new agents and the board raises it; each task
// shows its cost against its estimate. The Runner's rules (only the person
// decides, spawns only from the approved plan, the budget check) are Go
// tests (internal/transport/plans_test.go); this checks what the person sees.
//
// Screenshots, dark and light, in .cache/orchestration-{plan,budget}-*.png.
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
  console.log('orchestration plans: plan in the transcript, inbox and board; sheet with order, overlaps and cap; budget stop and raise; costs pass (dark and light)');
} finally {
  await browser?.close();
  await server?.close();
  await fixture?.close();
}

async function run(scheme) {
  const updates = () => fixture.calls.filter(c => c.method === 'UpdateMission').map(c => c.body);
  fixture.sessions.find(s => s.id === 'alpha-session').title = 'Search lead';
  fixture.openEvents('alpha-session').push({ type: 'message_delta', text: 'Here is how I would split it.' });
  // The person granted it earlier, asking for a plan first.
  const m = fixture.missions.create('Ship search');
  const grant = fixture.missions.handle('UpdateMission', { missionId: m.id, grant: { sessionId: 'alpha-session', ceiling: { maxChildren: 3, planRequired: true } } });
  assert.ok(!grant.error, JSON.stringify(grant.error));
  const plan = fixture.missions.proposePlan('alpha-session', { summary: 'index first, then rank; docs alongside', tasks: [
    { key: 'index', title: 'Index the corpus', brief: 'Objective: a token index\nDone when: go test ./search passes', role: 'worker', workspace: 'pinned', paths: ['search/'], estimateUsd: 4 },
    { key: 'rank', title: 'Rank results', role: 'worker', permission: 'read-only', paths: ['search/rank.go'], estimateUsd: 3, after: ['index'] },
    { key: 'docs', title: 'Document search', role: 'writer', paths: ['docs/search.md'], estimateUsd: 1.5 },
  ] });

  plan.contract = { artifactId: 'search-spec', version: 2, acceptanceCriteria: ['Search restores the last query'] };
  plan.baseline = [{ name: 'api', repositoryId: 'api', commit: 'e3f54af31fae8eb7' }];
  const context = await browser.newContext({ viewport: { width: 1440, height: 900 }, colorScheme: scheme });
  const page = await context.newPage();
  const errors = []; page.on('pageerror', error => errors.push(String(error)));
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`);
  const chat = page.locator('.panel-chat');
  await expect(chat.getByRole('textbox', { name: 'Message', exact: true })).toBeVisible();

  // The plan waits in the orchestrator's conversation…
  const line = chat.locator('.lineage-line.kind-plan_proposed');
  await expect(line).toContainText('Proposed a plan: 3 agents · est. $8.50 · waiting for your approval');
  // …in the Inbox (the Agents panel), on top, its Review focused, and counted…
  const inboxButton = page.locator('.inbox-action');
  await expect(inboxButton).toHaveAccessibleName(/1 plan awaits approval/);
  await inboxButton.click();
  const inbox = page.locator('.panel-agents');
  await expect(inbox.locator('.group-plans')).toContainText('Search lead proposes 3 agents');
  await expect(inbox.locator('.group-plans')).toContainText('Ship search · est. $8.50 · 1 shared path');
  await expect(inbox.locator('.group-plans').getByRole('button', { name: 'Review plan…' })).toBeFocused();
  await expect(inbox.locator('.agents-summary')).toHaveText('1 waiting for you');
  await page.screenshot({ path: `${shots}orchestration-inbox-${scheme}.png` });
  // …and on the board, from where it opens too.
  await page.getByRole('navigation', { name: 'Pages' }).getByRole('button', { name: 'Missions', exact: true }).click();
  const panel = page.getByRole('region', { name: 'Missions and tasks', exact: true });
  await expect(panel.locator('.mission-plan-waiting')).toContainText('Plan waiting for you: 3 agents · est. $8.50 · 1 shared path');
  await expect(panel.locator('.mission-orchestrator-ceiling')).toContainText('plan first');

  // The sheet: in order, the shared path flagged, the estimate, a cap.
  await panel.getByRole('button', { name: 'Review plan…' }).click();
  const sheet = page.getByRole('dialog', { name: 'Plan for Ship search' });
  await expect(sheet).toBeVisible();
  await expect(sheet).toContainText('Spec search-spec · version 2');
  await expect(sheet).toContainText('Search restores the last query');
  await expect(sheet).toContainText('e3f54af31fae');
  await expect(sheet.locator('[data-key=index]')).toContainText('workspace at starting revisions');
  await expect(sheet.locator('h2')).toContainText('Search lead proposes 3 agents');
  await expect(sheet.locator('.plan-tasks > li')).toHaveCount(3);
  await expect(sheet.locator('.plan-tasks > li').nth(0)).toContainText('Index the corpus');
  await expect(sheet.locator('.plan-tasks > li').nth(2)).toContainText('Rank results');
  await expect(sheet.locator('.plan-tasks > li[data-key=rank] .plan-step')).toHaveText('2');
  await expect(sheet.locator('.plan-tasks > li[data-key=rank] .plan-task-meta')).toContainText('Read only');
  await expect(sheet.locator('.plan-tasks > li[data-key=rank] .plan-task-meta')).toContainText('after index');
  await expect(sheet.locator('.plan-paths li.overlap')).toHaveCount(2);
  await expect(sheet.locator('.plan-overlaps')).toContainText('search/rank.go (index, rank)');
  await expect(sheet.locator('.plan-totals')).toContainText('Estimate $8.50');
  await expect(sheet.getByLabel('Budget cap in dollars')).toHaveValue('11');
  await sheet.getByLabel('Budget cap in dollars').fill('12');
  await sheet.getByLabel('Agents at once').fill('2');
  await expect(sheet.getByRole('button', { name: 'Approve with $12 cap' })).toBeEnabled();
  await page.screenshot({ path: `${shots}orchestration-plan-${scheme}.png` });
  await page.keyboard.press('Meta+Enter');
  await expect(sheet).toHaveCount(0);
  const approved = updates().find(u => u.approvePlan);
  assert.equal(approved.approvePlan.planId, plan.id);
  assert.equal(approved.approvePlan.capUsd, 12);
  assert.equal(approved.approvePlan.maxChildren, 2);

  // Approved: three tasks, the line says so, the cap is the tree's.
  await expect(panel.locator('.mission-card')).toHaveCount(3, { timeout: 10_000 });
  await expect(panel.locator('.mission-plan-waiting')).toHaveCount(0);
  await expect(panel.locator('.mission-orchestrator-budget')).toHaveText('$0 spent · $12 cap', { timeout: 10_000 });

  // A task's cost against its estimate.
  const index = fixture.missions.missions.find(v => v.id === m.id).tasks.find(t => t.title === 'Index the corpus');
  fixture.missions.setCost(index.id, 2.1);
  await expect(panel.locator(`.mission-card[data-task="${index.id}"] .mission-cost`)).toHaveText('$2.10 / $4', { timeout: 10_000 });

  // The tree reaches its cap: new agents paused; the board raises it.
  fixture.missions.setBudget(fixture.missions.missions.find(v => v.id === m.id).grants[0].id, { spentUsd: 9.4, reservedUsd: 3, budgetState: 'exhausted' });
  const stop = panel.locator('.mission-budget-stop');
  await expect(stop).toContainText('New agents paused. $9.40 spent and $3 reserved of the $12 budget.', { timeout: 10_000 });
  await expect(panel.locator('.mission-orchestrator-budget')).toHaveClass(/budget-exhausted/);
  await page.screenshot({ path: `${shots}orchestration-budget-${scheme}.png` });
  await stop.getByLabel('New budget cap in dollars').fill('25');
  await stop.getByRole('button', { name: 'Raise budget' }).click();
  await expect(stop).toHaveCount(0, { timeout: 10_000 });
  assert.equal(updates().find(u => u.raiseBudget).raiseBudget.capUsd, 25);
  await page.getByRole('navigation', {name: 'Pages'}).getByRole('button', {name: 'Workspaces', exact: true}).click();
  await expect(chat.locator('.lineage-line.kind-plan_decided')).toContainText('You approved the plan: 3 tasks, $12.00 cap.');
  await expect(line.locator('.lineage-state')).toHaveText('approved');
  assert.deepEqual(errors, []);
  await context.close();
}
