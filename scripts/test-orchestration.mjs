// The Orchestrator (audits/2026-11-16/sota-agents.md §6.3) against the fake
// Runner: the person lets a conversation run agents from `/orchestrate`,
// within a ceiling that cannot exceed the conversation's own permission; the
// badge shows its agents and revokes the grant; its agents' messages read as
// theirs (never "You"); a child's header leads back to its orchestrator; the
// Missions board shows who started what. The Runner's rules themselves (the
// allowlist, lineage, shrink-only ceilings, rate limits) are Go tests
// (internal/transport/agents_test.go); this checks what the person sees.
//
// Screenshots, dark and light, in .cache/orchestration-*.png.
import { createServer } from './fixtures/vite-cache.mjs';
import react from '@vitejs/plugin-react';
import { chromium, webkit, expect } from './fixtures/playwright.mjs';
import { fileURLToPath } from 'node:url';
import { mkdirSync } from 'node:fs';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';
import { testViteCache } from './fixtures/vite-cache.mjs';
import { collectPageErrors, drainPageErrors } from './fixtures/page-errors.mjs';

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
  console.log('orchestration: grant from /orchestrate, badge, agents, labelled messages, lineage crumb, board and revoke pass (dark and light)');
} finally {
  await browser?.close();
  await server?.close();
  await fixture?.close();
}

async function run(scheme) {
  const updates = () => fixture.calls.filter(c => c.method === 'UpdateMission').map(c => c.body);
  const orchestrator = fixture.sessions.find(s => s.id === 'alpha-session');
  orchestrator.title = 'Search lead';
  // A live journal for the orchestrator, so the lines the Runner records arrive as they happen.
  fixture.openEvents('alpha-session').push({ type: 'message_delta', text: 'I can split the search work: an index and a ranker.' });
  const context = await browser.newContext({ viewport: { width: 1440, height: 900 }, colorScheme: scheme });
  const page = await context.newPage();
  const errors = []; await collectPageErrors(page, errors);
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`);
  // The conversation in view: typing in one keeps its tab, so the child opens in a preview tab beside it.
  const chat = page.locator('.panel-chat').filter({ visible: true });
  const composer = chat.getByRole('textbox', { name: 'Message', exact: true });
  await expect(composer).toBeVisible();
  await expect(chat.locator('.orchestrator-chip')).toHaveCount(0);
  await chat.getByRole('button', { name: 'Authorize this coordinator' }).click();
  const sheet = page.getByRole('dialog', { name: 'Let this conversation run agents' });
  await expect(sheet).toBeVisible();
  await expect(sheet.getByRole('radio', { name: 'Ask' })).toBeChecked();
  await expect(sheet.getByRole('radio', { name: 'Full access' })).toBeDisabled();
  await expect(sheet.getByRole('button', { name: 'Let it run agents' })).toBeDisabled();
  await expect(sheet.locator('.grant-problem')).toContainText('Name the mission');
  await sheet.getByLabel('New mission').fill('Ship search');
  await sheet.getByLabel('Agents at once').fill('3');
  await sheet.getByLabel('Budget in dollars').fill('20');
  await expect(sheet.locator('.grant-row small').first()).toHaveText('$20 for the whole tree');
  // Approving a plan first is the default; this run starts agents right away.
  await expect(sheet.getByLabel(/Approve its plan first/)).toBeChecked();
  await sheet.getByLabel(/Approve its plan first/).uncheck();
  await expect(sheet.locator('.grant-rules')).toContainText('Never approves, answers for you, passes a gate or accepts a task');
  await page.screenshot({ path: `${shots}orchestration-grant-${scheme}.png` });
  await page.route('**/UpdateMission', route => route.fulfill({status:503,contentType:'application/json',body:JSON.stringify({code:'unavailable',message:'Grant unavailable'})}), { times: 1 });
  await sheet.getByRole('button', { name: 'Let it run agents' }).click();
  await expect(sheet.getByRole('alert')).toContainText('Grant unavailable');
  await sheet.getByRole('button', { name: 'Let it run agents' }).click();
  assert.equal(fixture.missions.missions.length,1,'retry grants the mission that was already created');
  await expect(sheet).toHaveCount(0);
  const granted = updates().find(u => u.grant);
  assert.equal(granted.grant.sessionId, 'alpha-session');
  assert.equal(granted.grant.ceiling.permission, 'ask');
  assert.equal(granted.grant.ceiling.maxChildren, 3);
  assert.equal(granted.grant.ceiling.budgetUsd, 20);
  assert.deepEqual(granted.grant.ceiling.profileIds, ['claude-default']);
  assert.ok(!granted.grant.ceiling.planRequired, "the plan step was turned off");

  // The badge says it holds power, and how much is in use; the transcript says when it was given.
  const chip = chat.locator('.orchestrator-chip');
  await expect(chip).toContainText('Orchestrator0/3', { timeout: 15_000 });
  await expect(chat.locator('.lineage-line.kind-orchestration_granted')).toContainText('You let this conversation run agents for “Ship search”');

  // It starts two agents (the Runner's work; here the fake's).
  const index = fixture.missions.spawnChild('alpha-session', { title: 'Index the corpus', brief: 'Objective: a token index\nDone when: go test ./search passes', role: 'worker' });
  fixture.missions.spawnChild('alpha-session', { title: 'Rank results', role: 'researcher', permission: 'read-only' });
  await expect(chip).toContainText('Orchestrator2/3', { timeout: 15_000 });
  await expect(chat.locator('.lineage-line.kind-agent_spawned')).toHaveCount(2);
  await expect(chat.locator('.lineage-line.kind-agent_spawned').first()).toContainText('Started “Index the corpus” · worker · ask');

  // The Agents panel nests them under their orchestrator, which shows how many run.
  const agents = page.locator('.panel-agents');
  const orchRow = agents.locator('li[data-orchestrator]').filter({ hasText: 'Search lead' });
  await expect(orchRow.locator('.agent-orchestrator')).toHaveText('2/3', { timeout: 15_000 });
  const nested = agents.locator('li[data-depth="1"]');
  await expect(nested).toHaveCount(2);
  await expect(nested.first().locator('.agent-role')).toHaveText(/worker|researcher/);
  const order = await agents.locator('li[data-key]').evaluateAll(nodes => nodes.map(n => n.querySelector('.agent-title strong')?.textContent || ''));
  const lead = order.indexOf('Search lead');
  assert.ok(lead >= 0 && ['Index the corpus', 'Rank results'].every(t => order.indexOf(t) > lead), `children follow their orchestrator: ${order}`);
  // A child that asks for approval asks the person, saying who started it.
  index.pending = [{ type: 'approval', requestId: 'req-1', text: 'Bash go test ./search', providerDataJson: '' }];
  index.status = 'waiting_approval'; index.busy = false;
  const asking = agents.locator('li[data-state="needs-input"]').filter({ hasText: 'Index the corpus' });
  await expect(asking.locator('.agent-lineage')).toHaveText('started by Search lead · Ship search', { timeout: 15_000 });
  // Too long for the row, it ends in "…" rather than being cut mid-letter (text-overflow needs a block, not a flex row).
  const lineageBox = await asking.locator('.agent-lineage').evaluate(node => { const style = getComputedStyle(node); return { display: style.display, overflow: style.textOverflow }; });
  assert.deepEqual(lineageBox, { display: 'block', overflow: 'ellipsis' });
  await page.screenshot({ path: `${shots}orchestration-agents-${scheme}.png` });
  index.pending = []; index.status = 'running'; index.busy = true;

  // A child's message is an event of the conversation, from the child, never "You".
  fixture.missions.agentMessage(index.id, 'alpha-session', 'Found the tokenizer in search/lex.go; see run r_12.');
  const from = chat.locator('.system-event[data-kind=agent_note]').last();
  await expect(from.locator('.system-event-text')).toHaveText('Index the corpus: Found the tokenizer in search/lex.go; see run r_12.');
  await expect(from.locator('.system-event-detail')).toHaveText('your agent');
  await expect(from).not.toContainText('You');
  await from.locator('.system-event-toggle').click();
  await expect(from.locator('.system-event-caption')).toContainText('Index the corpus’s message, as Lazybox gave it to Claude');
  await expect(from.locator('.system-event-text-sent')).toContainText('> Found the tokenizer');
  await from.locator('.system-event-toggle').click();

  // The badge's card: the ceiling, its agents and their state, Revoke.
  await chip.click();
  const card = page.getByRole('dialog', { name: 'Orchestrator grant' });
  await expect(card).toBeVisible();
  await expect(card.locator('.orchestrator-ceiling')).toHaveText('Up to 3 at once · Ask or narrower · $20 cap');
  await expect(card.locator('.orchestrator-children li')).toHaveCount(2);
  await expect(card.locator('.orchestrator-children li').first()).toContainText('Index the corpus');
  await expect(card.locator('.orchestrator-children li').first()).toContainText('Working');
  await expect(card.locator('.orchestrator-never')).toContainText('never approves');
  await page.screenshot({ path: `${shots}orchestration-badge-${scheme}.png` });
  await page.keyboard.press('Escape');
  await expect(card).toHaveCount(0);

  // A report: the person's gate opens, the orchestrator is told.
  fixture.missions.reportChild(index.id, 'Indexed 1,204 files; go test ./search passed (run r_14).');
  await expect(chat.locator('.system-event[data-kind=agent_report]').last()).toContainText('“Index the corpus” reported: Indexed 1,204 files', { timeout: 10_000 });

  // The child's own conversation: its header leads back; its brief is from the orchestrator.
  await chat.locator('.lineage-line.kind-agent_spawned').first().getByRole('button', { name: 'Open' }).click();
  await expect(chat.locator('.lineage-crumb')).toContainText('Search lead', { timeout: 10_000 });
  const brief = chat.locator('.system-event[data-kind=agent_brief]');
  await expect(brief.locator('.system-event-text')).toHaveText('Task from “Search lead”: Index the corpus');
  await expect(brief.locator('.system-event-detail')).toHaveText('worker');
  await brief.locator('.system-event-toggle').click();
  await expect(brief.locator('.system-event-text-sent')).toContainText('Done when: go test ./search passes');
  await page.screenshot({ path: `${shots}orchestration-child-${scheme}.png` });
  await chat.locator('.lineage-crumb').click();
  await expect(chat.locator('.orchestrator-chip')).toBeVisible({ timeout: 10_000 });

  // The board: the orchestrator on top, who started each card.
  await page.getByRole('navigation', { name: 'Pages' }).getByRole('button', { name: 'Missions', exact: true }).click();
  const panel = page.getByRole('region', { name: 'Missions and tasks', exact: true });
  await expect(panel.locator('.mission-orchestrator')).toContainText('Search lead');
  await expect(panel.locator('.mission-orchestrator-count')).toHaveText('2/3 agents');
  await expect(panel.locator('.mission-card .mission-lineage')).toHaveCount(2);
  await expect(panel.locator('.mission-card').filter({ hasText: 'Rank results' }).locator('.mission-lineage')).toContainText('researcher');
  await page.screenshot({ path: `${shots}orchestration-board-${scheme}.png` });

  await page.getByRole('navigation', {name: 'Pages'}).getByRole('button', {name: 'Workspaces', exact: true}).click();
  // Revoke from the badge: confirmed, then gone; its agents keep running.
  await chip.click();
  await card.getByRole('button', { name: 'Revoke grant…' }).click();
  await expect(card.locator('footer')).toContainText('Its agents keep running');
  await card.getByRole('button', { name: 'Revoke', exact: true }).click();
  await expect(chip).toHaveCount(0, { timeout: 15_000 });
  assert.ok(updates().some(u => u.revokeGrant?.grantId === granted.grant && true || u.revokeGrant), 'revoked through the Runner');
  await expect(chat.locator('.lineage-line.kind-orchestration_revoked')).toBeVisible();
  await page.getByRole('navigation', { name: 'Pages' }).getByRole('button', { name: 'Missions', exact: true }).click();
  await expect(panel.locator('.mission-orchestrator')).toHaveCount(0, { timeout: 10_000 });
  await expect(panel.getByRole('button', { name: 'Let a conversation run agents…' })).toBeVisible();
  await panel.getByRole('button', { name: 'Let a conversation run agents…' }).click();
  const picker = sheet.getByLabel('Conversation', {exact:true});
  await expect(picker).toHaveValue('');
  await expect(sheet.getByRole('button', { name: 'Let it run agents' })).toBeDisabled();
  await expect(sheet.getByLabel('New mission')).toHaveCount(0);
  await picker.selectOption('alpha-session');
  await expect(sheet.getByRole('button', { name: 'Let it run agents' })).toBeEnabled();
  await sheet.getByRole('button', {name:'Cancel'}).click();
  assert.equal(updates().filter(u => u.grant).length, 1, 'choosing then canceling grants nothing');
  await page.getByRole('navigation', {name: 'Pages'}).getByRole('button', {name: 'Workspaces', exact: true}).click();
  await composer.fill('/orchestrate');
  await composer.press('Enter');
  await expect(sheet.getByRole('radio', {name: /Ship search/})).not.toBeChecked();
  await expect(sheet.getByRole('button', {name: 'Let it run agents'})).toBeDisabled();
  await sheet.getByRole('radio', {name: /Ship search/}).check();
  await expect(sheet.getByRole('button', {name: 'Let it run agents'})).toBeEnabled();
  await sheet.getByRole('button', {name:'Cancel'}).click();
  await drainPageErrors(errors);
  assert.deepEqual(errors, []);
  await context.close();
}
