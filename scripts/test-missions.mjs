// Missions (ORCHESTRATION.md slice 1) against the fake Runner: create a
// mission and its tasks, assign a task to a new conversation and another to
// an existing one, watch evidence attach (a failing then passing run, then
// checks), report, and decide at the gate from the board and from the inbox.
// Every status and trust level on screen is the Runner's (here, the fake's
// small copy of its derivation); the Go tests own the derivation itself.
import { createServer } from 'vite';
import react from '@vitejs/plugin-react';
import { chromium, webkit, expect } from './fixtures/playwright.mjs';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';
import { testViteCache } from './fixtures/vite-cache.mjs';

const fixture = await startRunnerFixture();
const server = await createServer({ ...testViteCache(import.meta.url), configFile: false, root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: fixture.url } } } });
const mutations = method => fixture.mutations.filter(m => m.method === method);
const updates = () => fixture.calls.filter(c => c.method === 'UpdateMission').map(c => c.body);
let browser;
try {
  await server.listen();
  browser = await (process.env.TEST_CHROMIUM ? chromium : webkit).launch();
  const context = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  const page = await context.newPage();
  // A reload cuts every panel's in-flight poll, which WebKit reports as page
  // errors ("access control checks") on the page being left: not failures.
  let navigating = false;
  const errors = []; page.on('pageerror', error => { if (!navigating) errors.push(String(error)); });
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`);
  await expect(page.locator('.panel-chat').getByRole('textbox', { name: 'Message', exact: true })).toBeVisible();

  // The Runner-wide page opens outside the workspace layout.
  await page.getByRole('navigation', { name: 'Pages' }).getByRole('button', { name: 'Missions', exact: true }).click();
  const panel = page.getByRole('region', { name: 'Missions and tasks', exact: true });
  await expect(panel).toBeVisible();
  await expect(page.locator('.workspace-tools')).toHaveCount(0);
  await expect(page.getByRole('navigation', { name: 'Pages' }).getByRole('button', {name: 'Missions', exact: true})).toHaveAttribute('aria-current', 'page');
  await expect(page.getByRole('tab',{name:'Missions',exact:true})).toHaveCount(0);
  await expect(panel.getByRole('heading', { name: 'Create your first mission' })).toBeVisible();
  await expect(panel).toContainText('different workspaces');

  // A mission: an outcome, an optional brief. Its process is pinned and said.
  await panel.getByLabel('Outcome').fill('Ship the parser split');
  await panel.getByLabel(/Brief/).fill('parser.go is 4,000 lines; split it without changing behaviour.');
  // Keep a pre-creation list response in flight until the mutation completes.
  const staleList = Promise.withResolvers();
  const releaseList = Promise.withResolvers();
  const listFinished = Promise.withResolvers();
  let held = false;
  await page.route('**/ListMissions', async route => {
    if (held) return route.continue();
    held = true;
    staleList.resolve();
    await releaseList.promise;
    await route.fulfill({ status: 200, contentType: 'application/json', body: '{"missions":[]}' });
    listFinished.resolve();
  });
  await panel.getByRole('button', { name: 'Refresh missions' }).click();
  await staleList.promise;
  const createKeys = [];
  await page.route('**/CreateMission', async route => {
    createKeys.push(route.request().postDataJSON().requestKey);
    if (createKeys.length === 1) await route.fulfill({ status: 503, contentType: 'application/json', body: JSON.stringify({code:'unavailable',message:'Retry creation'}) });
    else await route.continue();
  });
  await panel.getByRole('button', { name: 'Create mission' }).click();
  await expect(panel.getByRole('alert')).toContainText('Retry creation');
  await panel.getByRole('button', { name: 'Create mission' }).click();
  await expect.poll(() => createKeys.length).toBe(2);
  assert.equal(createKeys[0],createKeys[1]);
  await page.unroute('**/CreateMission');
  await expect(panel.getByRole('heading', { name: 'Ship the parser split' })).toBeVisible();
  releaseList.resolve();
  await listFinished.promise;
  await page.unroute('**/ListMissions');
  await expect(panel.locator('.mission-process')).toHaveText('One agent per task · you accept each task');
  await expect(panel.getByRole('heading', { name: 'Start with one task' })).toBeVisible();
  await expect(panel.locator('.mission-column')).toHaveCount(0);

  // Tasks: the brief starts from V1's four lines; a dependency makes the second wait.
  await panel.getByRole('button', { name: 'Add first task' }).click();
  await panel.getByLabel('Task', { exact: true }).fill('Split the parser');
  await panel.getByLabel(/Brief/).focus();
  await expect(panel.getByLabel(/Brief/)).toHaveValue(/^Objective: \nDone when:/);
  await panel.getByLabel(/Brief/).fill('Objective: three files\nDone when: go test passes');
  await panel.getByRole('button', { name: 'Add task', exact: true }).click();
  await expect(panel.getByLabel('Task', { exact: true })).toHaveValue('');
  await panel.getByLabel('Task', { exact: true }).fill('Document the parser');
  await panel.getByLabel(/Starts after/).selectOption({ label: 'Split the parser' });
  // The Runner applied this task before its response was lost. Retrying
  // the unchanged form must replay the operation and leave one task.
  const taskKeys = [];
  await page.route('**/UpdateMission', async route => {
    const { addTask, operationId } = route.request().postDataJSON();
    if (!addTask) return route.continue();
    taskKeys.push(operationId);
    if (taskKeys.length > 1) return route.continue();
    await route.fetch();
    await route.fulfill({ status: 503, contentType: 'application/json', body: JSON.stringify({code:'unavailable', message:'Task response lost'}) });
  });
  await panel.getByRole('button', { name: 'Add task', exact: true }).click();
  await expect(panel.getByRole('alert').first()).toContainText('Task response lost');
  await panel.getByRole('button', { name: 'Add task', exact: true }).click();
  await expect.poll(() => taskKeys.length).toBe(2);
  assert.equal(taskKeys[0], taskKeys[1]);
  await page.unroute('**/UpdateMission');
  await panel.getByRole('button', { name: 'Done', exact: true }).click();
  const card = title => panel.locator('.mission-card').filter({ has: page.getByRole('button', { name: title, exact: true }) });
  const column = key => panel.locator(`.column-${key}`);
  await expect(column('ready').locator('.mission-card')).toHaveCount(2);
  await expect(panel.locator('.mission-summary')).toContainText('1 blocked · 0 of 2 done');
  await panel.getByRole('button', {name: 'Blocked · Document the parser'}).click();
  await expect(card('Document the parser')).toBeFocused();
  await expect(card('Document the parser').locator('.mission-status')).toHaveText('Blocked');
  await expect(card('Document the parser').locator('.mission-reason')).toHaveText('Waiting for “Split the parser”');
  await expect(card('Split the parser').locator('.trust-badge')).toHaveCount(0);
  await page.screenshot({ path: '/tmp/lazybox-missions-ready.png' });

  // Assign to a new conversation in this workspace, with the brief sent.
  await card('Split the parser').getByRole('button', { name: 'Assign…' }).click();
  const assign = page.getByRole('dialog', { name: 'Assign Split the parser' });
  await expect(assign).toBeVisible();
  await expect(assign.getByRole('radio', { name: 'A new conversation' })).toBeChecked();
  await expect(assign.getByLabel('Workspace')).toHaveValue('');
  await expect(assign.getByRole('button', { name: 'Start conversation' })).toBeDisabled();
  await assign.getByLabel('Workspace').selectOption('alpha');
  await expect(assign.getByLabel('Brief to send')).toContainText('Done when: go test passes');
  await page.screenshot({ path: '/tmp/lazybox-missions-assign.png' });
  await assign.getByRole('button', { name: 'Start conversation' }).click();
  await expect(assign).toHaveCount(0);
  const sent = updates().find(u => u.assign);
  assert.equal(sent.assign.open.collectionId, 'alpha');
  assert.equal(sent.assign.sendBrief, true);
  await expect(panel.locator('.missions-notice')).toContainText('Started a conversation for “Split the parser”');
  const split = card('Split the parser');
  await expect(column('working').locator('.mission-card')).toHaveCount(1);
  await expect(split.locator('.mission-status')).toHaveText('Working');
  await expect(split.locator('.trust-badge')).toHaveText('Observed');
  await expect(split.locator('.mission-conversation')).toContainText('Alpha feature');
  await expect(split.locator('.mission-live')).toHaveText('Running');
  const taskSession = fixture.missions.missions[0].tasks[0].attempts[0].sessionId;

  // The agent's failing `lazybox run` attaches by itself; Open goes to its output.
  const failing = fixture.startRun(taskSession, 'go test ./...', { exitCode: 1, outputBytes: '1468006', agentBytes: '2150', filter: 'go-test', outputPath: '/state/command-output/task/go-1.log' });
  fixture.missions.recordRun(taskSession, { title: 'go test ./...', exitCode: 1, commandId: failing.id });
  await expect(split.locator('.evidence-chip')).toContainText('go test ./... · exit 1', { timeout: 10_000 });
  await expect(split.locator('.evidence-chip.tone-fail')).toHaveCount(1);
  await expect(split.locator('.mission-flag')).toHaveText('Last run failed');

  // Reported done after a failing run: it asks for acceptance, but is only claimed.
  await split.getByRole('button', { name: 'Report done…' }).click();
  const report = page.getByRole('dialog', { name: 'Report “Split the parser” done' });
  await report.getByLabel('What was done').fill('Split into lexer, parser and ast.');
  await report.getByRole('button', { name: 'Report done' }).click();
  await expect(column('needs').locator('.mission-card')).toHaveCount(1);
  await expect(split.locator('.mission-status')).toHaveText('Needs you');
  await expect(split.locator('.trust-badge')).toHaveText('Claimed');
  await expect(split.locator('.trust-badge')).toHaveAttribute('title', /nothing the Runner saw backs it/);
  await expect(split.getByRole('button', { name: 'Accept' })).toBeVisible();
  await page.screenshot({ path: '/tmp/lazybox-missions-claimed.png' });

  // The inbox counts it and offers the decision, with what backs the report:
  // the Inbox is the Agents panel under Workspaces, the decision on top, focused.
  const inboxButton = page.locator('.inbox-action');
  await expect(inboxButton).toHaveAccessibleName(/1 task awaits acceptance/);
  await inboxButton.click();
  const inbox = page.locator('.panel-agents');
  const awaiting = inbox.locator('.group-awaiting');
  await expect(awaiting.getByRole('button', { name: 'Accept' })).toBeFocused();
  await expect(awaiting.locator('h3')).toHaveText(/Awaiting acceptance\s*1/);
  await expect(awaiting.locator('.inbox-task')).toContainText('Split the parser');
  await expect(awaiting.locator('.inbox-task')).toContainText('Ship the parser split · “Split into lexer, parser and ast.”');
  await expect(awaiting.locator('.trust-badge')).toHaveText('Claimed');
  await expect(inbox.locator('.agents-summary')).toHaveText('1 waiting for you');

  const reopenBoard = async () => {
    await page.getByRole('navigation', { name: 'Pages' }).getByRole('button', { name: 'Missions', exact: true }).click();
  };
  await reopenBoard();
  // A passing run makes it observed; checks passing on the PR verify it.
  fixture.missions.recordRun(taskSession, { title: 'go test ./...', exitCode: 0 });
  await expect(split.locator('.trust-badge')).toHaveText('Observed', { timeout: 10_000 });
  await expect(split.locator('.mission-flag')).toHaveCount(0);
  fixture.missions.recordPullRequest('alpha', { number: 1216, head: 'abcd123', outcome: 'pass' });
  assert.ok(!fixture.missions.missions[0].tasks[0].evidence.some(e => e.kind === 'pr'));
  fixture.missions.linkPullRequest(taskSession, 'fixture-team/api', 12);
  fixture.missions.recordPullRequest('alpha', { number: 12, head: '3f9c2e1aa0', outcome: 'pass' });
  await expect(split.locator('.trust-badge')).toHaveText('Verified', { timeout: 10_000 });
  await expect(split.locator('.evidence-chip').filter({ hasText: 'PR #12' })).toBeVisible();
  // A run chip opens that run's output in the console.
  await split.getByRole('button', { name: 'Split the parser', exact: true }).click();
  await split.locator('.evidence-chip').filter({ hasText: 'exit 1' }).click();
  await expect(page.locator('.panel-console')).toContainText('exit 1');
  await expect(page.getByRole('combobox', { name: 'Session', exact: true })).toHaveValue(taskSession);
  await page.screenshot({ path: '/tmp/lazybox-missions-verified.png' });

  await page.getByRole('navigation', { name: 'Pages' }).getByRole('button', { name: 'Missions', exact: true }).click();
  // Accepted from the inbox: done, accepted; the dependent task is ready.
  await inboxButton.click();
  await expect(awaiting.locator('.trust-badge')).toHaveText('Verified');
  await page.screenshot({ path: '/tmp/lazybox-missions-inbox.png' });
  await awaiting.getByRole('button', { name: 'Accept' }).click();
  await expect(awaiting).toHaveCount(0);
  await expect(inboxButton).not.toHaveAccessibleName(/awaits acceptance/);
  await reopenBoard();
  await expect(split.locator('.mission-status')).toHaveText('Done');
  await expect(split.locator('.trust-badge')).toHaveText('Accepted');
  await expect(card('Document the parser').locator('.mission-status')).toHaveText('Ready');
  await expect(panel.locator('.mission-summary')).toHaveText('1 of 2 done');
  assert.ok(updates().some(u => u.accept?.taskId === fixture.missions.missions[0].tasks[0].id), 'accepted through the Runner');

  // The second task goes to a conversation that already exists.
  const docs = card('Document the parser');
  await docs.getByRole('button', { name: 'Assign…' }).click();
  const assignDocs = page.getByRole('dialog', { name: 'Assign Document the parser' });
  await assignDocs.getByRole('radio', { name: 'A conversation you have' }).check();
  await expect(assignDocs.getByLabel('Conversation', { exact: true })).toHaveValue('');
  await assignDocs.getByLabel('Conversation', { exact: true }).selectOption('alpha-session');
  await assignDocs.getByLabel("Send the task's brief to the agent now").uncheck();
  await assignDocs.getByRole('button', { name: 'Assign', exact: true }).click();
  await expect(docs.locator('.mission-conversation')).toBeVisible();
  assert.equal(updates().filter(u => u.assign).at(-1).assign.sessionId, 'alpha-session');
  assert.ok(!updates().filter(u => u.assign).at(-1).assign.sendBrief, 'the brief stays unsent');

  // The snippet chain of its conversation is one click away (V1's ]]n).
  fixture.missions.recordApply({ sessionId: 'alpha-session', message: { text: 'Review the current change carefully.' } }, [{ key: 'review', body: 'Review the current change carefully.', next: ['test'] }]);
  await expect(docs.locator('.mission-next')).toContainText('After review, next:', { timeout: 10_000 });
  await docs.locator('.mission-next').getByRole('button', { name: 'test' }).click();
  await expect(panel.locator('.missions-notice')).toContainText('Sent “test” to the conversation.');
  const chained = mutations('Apply').at(-1).body;
  assert.equal(chained.sessionId, 'alpha-session');
  assert.equal(chained.message.text, 'Run the relevant tests.');
  assert.equal(chained.message.source.kind, 'snippet');
  assert.equal(chained.message.source.snippetKey, 'test');

  // Asking for changes needs a note, and the card says what was asked.
  await docs.getByRole('button', { name: 'Report done…' }).click();
  await page.getByRole('dialog', { name: 'Report “Document the parser” done' }).getByRole('button', { name: 'Report done' }).click();
  await docs.getByRole('button', { name: 'Ask for changes…' }).click();
  const reject = page.getByRole('dialog', { name: 'Ask for changes on “Document the parser”' });
  await expect(reject.getByRole('button', { name: 'Ask for changes' })).toBeDisabled();
  await reject.getByLabel('What should change').fill('Add an example per token kind');
  await reject.getByRole('button', { name: 'Ask for changes' }).click();
  await expect(docs.locator('.mission-status')).toHaveText('Changes requested');
  await expect(docs.locator('.mission-reason')).toHaveText('You asked for changes: Add an example per token kind');

  // The history is the log, newest first, and says who did what.
  await panel.locator('.mission-history summary').click();
  await expect(panel.locator('.mission-history li').first()).toContainText('You asked for changes on “Document the parser”');
  await expect(panel.locator('.mission-history')).toContainText('You accepted “Split the parser”');
  await expect(panel.locator('.mission-history')).toContainText('Ran go test ./... · exit 1');

  // A refusal says why, in the panel.
  fixture.missions.missions[0].tasks[1].gate = '';
  await docs.getByRole('button', { name: 'Report done…' }).click();
  // (Reporting twice is refused by the Runner when a decision is pending; here it is accepted.)
  await page.getByRole('dialog', { name: 'Report “Document the parser” done' }).getByRole('button', { name: 'Report done' }).click();
  await expect(docs.locator('.mission-status')).toHaveText('Needs you');

  // A second mission; the choice is kept across a reload.
  await panel.getByRole('button', { name: 'New mission' }).click();
  await panel.getByLabel('Outcome').fill('Speed up startup');
  await panel.getByRole('button', { name: 'Create mission' }).click();
  await expect(panel.getByRole('heading', { name: 'Speed up startup' })).toBeVisible();
  // The mission picker is the app's menu, each mission with its summary.
  await panel.getByLabel('Mission: Speed up startup', { exact: true }).click();
  await page.getByRole('group', { name: 'Mission: Speed up startup menu' }).getByRole('button', { name: /^Ship the parser split\s*1 needs you · 1 of 2 done$/ }).click();
  await expect(panel.getByLabel('Mission: Ship the parser split', { exact: true })).toBeVisible();
  assert.deepEqual(errors, []);
  navigating = true;
  await page.reload();
  await expect(page.getByRole('region', { name: 'Missions and tasks', exact: true }).getByRole('heading', { name: 'Ship the parser split' })).toBeVisible();
  navigating = false;

  // Docked narrow, columns stack and nothing scrolls sideways; on a phone too.
  await page.setViewportSize({ width: 390, height: 844 });
  await expect(page.getByRole('navigation', {name: 'Pages'}).getByRole('button', {name: 'Workspaces', exact: true})).toBeVisible();
  const body = page.locator('.missions-body');
  await expect(body).toBeVisible();
  assert.ok(await body.evaluate(node => node.scrollWidth <= node.clientWidth + 1), 'no sideways scroll on a phone');
  await page.screenshot({ path: '/tmp/lazybox-missions-phone.png', fullPage: false });
  assert.deepEqual(errors, []);
  const popup = page.waitForEvent('popup');
  await panel.getByRole('button', {name: 'Detach mission'}).click();
  const detached = await popup;
  const missionHeading = detached.getByRole('heading', {name: 'Ship the parser split'});
  await expect(missionHeading).toBeVisible();
  await expect(panel).toHaveCount(0);
  await detached.reload();
  await expect(missionHeading).toBeVisible();
  await expect(detached.getByRole('button', {name: 'New mission'})).toHaveCount(0);
  // A detached mission stays usable after closing its main window.
  await page.close();
  const reopened = detached.waitForEvent('popup', { timeout: 6000 });
  await detached.locator('.mission-card').filter({ has: detached.getByRole('button', {name: 'Split the parser', exact: true}) }).locator('.mission-conversation').click();
  const workspace = await reopened;
  await expect(workspace.locator('.panel-chat').getByRole('textbox', {name: 'Message', exact: true})).toBeVisible();
  await expect(workspace.getByRole('tab', {selected: true}).filter({hasText: /Split the parser/i})).toBeVisible();
  console.log('missions: create, assign, evidence, report, accept and changes requested pass');
} finally {
  await browser?.close();
  await server.close();
  await fixture.close();
}
