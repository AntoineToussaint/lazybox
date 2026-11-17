// The workspace hover card against the fake Runner, in WebKit: resting on a
// workspace row shows, after a moment and beside the row, its task and age,
// its repositories with branch, ahead/behind main and changes, its pull
// requests with pills, its agents with their status, what waits for you (a
// line that opens the Inbox), what serves on which port, and its cost; line
// counts, runs and usage are read only when it opens. The pointer can cross
// into it; leaving closes it. ⌥↓ opens it from the keyboard, Esc closes it
// and gives the row its focus back. It fits beside a narrow sidebar and in a
// narrow window, in dark and light. Nothing runs Git or an agent.
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from 'vite';
import react from '@vitejs/plugin-react';
import {webkit, expect} from '@playwright/test';
import {fileURLToPath} from 'node:url';
import {mkdir} from 'node:fs/promises';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';

const f = await startRunnerFixture();
const now = Date.now(), ms = value => String(value);
// Alpha: started from an issue two days ago, two repositories, two pull requests.
Object.assign(f.collections[0], {createdAtUnixMillis: ms(now - 2 * 24 * 3_600_000), origin: {provider: 'github', key: 'acme/api#123'}});
Object.assign(f.library[0], {name: 'api', gitIdentity: 'github:acme/api'});
Object.assign(f.library[1], {name: 'web', gitIdentity: 'github:acme/web'});
const api = f.shipState('alpha', 'api'), web = f.shipState('alpha', 'web');
Object.assign(api, {branch: 'feature/uploads-retry-with-a-rather-long-branch-name', githubRepository: 'acme/api', upstream: 'origin/feature/uploads', commits: [{}, {}, {}], behindDefault: 1, lines: {base: 'origin/main', files: 6, additions: 120, deletions: 30, truncated: false}});
Object.assign(web, {branch: 'feature/uploads', githubRepository: 'acme/web'});
const item = (key, kind, title, extra = {}) => ({ref: {provider: 'github', key}, kind, title, url: `https://github.com/${key.replace('#', kind === 'issue' ? '/issues/' : '/pull/')}`, repository: key.split('#')[0], state: {category: 'review', label: 'Open'}, updatedAtUnixMillis: ms(now - 600_000), ...extra});
f.tracked.links.push(
  {collectionId: 'alpha', collectionName: 'Alpha feature', role: 'origin', source: 'started-from', linkedAtUnixMillis: ms(now - 2 * 24 * 3_600_000), item: item('acme/api#123', 'issue', 'Fix uploads')},
  {collectionId: 'alpha', collectionName: 'Alpha feature', role: 'delivers', source: 'agent-output', sessionId: 'alpha-session', member: 'api', linkedAtUnixMillis: ms(now - 3_600_000),
    item: item('acme/api#812', 'pull_request', 'Retry failed uploads from the worker', {pullRequest: {head: api.branch, base: 'main', headSha: 'sha-812', checks: 'failure', failingChecks: [{name: 'lint'}, {name: 'unit-api'}], checksTotal: 14}})},
  {collectionId: 'alpha', collectionName: 'Alpha feature', role: 'delivers', source: 'agent-output', sessionId: 'alpha-session', member: 'web', linkedAtUnixMillis: ms(now - 3_600_000),
    item: item('acme/web#90', 'pull_request', 'Upload progress bar', {pullRequest: {head: 'feature/uploads', base: 'main', headSha: 'sha-90', checks: 'success', review: 'approved', mergeable: true, mergeableState: 'clean'}})},
);
// Its conversations: one asks two things of you, one works, one is done.
Object.assign(f.sessions[0], {title: 'Retry the uploads', provider: 'claude', model: 'claude-opus-5-5', modelReported: true, busy: false, status: 'waiting_approval', createdAtUnixMillis: ms(now - 5 * 60_000),
  pending: [{requestId: 'r1', type: 'approval', text: 'npm test'}, {requestId: 'r2', type: 'question', text: 'Which bucket?'}]});
f.sessions.push(
  {...structuredClone(f.sessions[1]), id: 'alpha-worker', collectionId: 'alpha', workId: 'alpha', title: 'Write the progress bar', provider: 'codex', model: 'gpt-5.4', modelReported: true, busy: true, status: 'running', pending: [], createdAtUnixMillis: ms(now - 20 * 60_000), agents: [{id: 'root', name: 'Main agent', status: 'running', phase: 'tool'}]},
  {...structuredClone(f.sessions[1]), id: 'alpha-done', collectionId: 'alpha', workId: 'alpha', title: 'Read the upload code', provider: 'claude', busy: false, status: 'ready', pending: [], createdAtUnixMillis: ms(now - 26 * 3_600_000), agents: [{id: 'root', name: 'Main agent', status: 'done'}]},
);
// What serves: alpha's dev action, and a command an agent left listening.
const alphaRuntime = f.runtimeFor('alpha');
Object.assign(alphaRuntime.actions[0], {name: 'web', runState: {state: 'running', commandId: 'alpha-dev', startedAtUnixMillis: ms(now - 60_000)}});
f.processes.push({id: 'host-storybook', kind: 'command', backend: 'sessionhost', state: 'running', attached: true, hostPid: 4501, programPid: 4502, uptimeSeconds: '60', label: 'pnpm storybook -p 6006', workspaceId: 'alpha', workspaceName: 'Alpha feature', sessionId: 'alpha-worker', commandId: 'storybook-1', ports: [6006]});
f.telemetry = body => body.workspace === 'alpha' ? {groups: [{scope: {workspace: 'alpha'}, model: 'claude-opus-5-5', source: 'provider', tokens: {input: '1000', output: '500'}, nanos: '1240000000', records: '3', unpriced: '0', incomplete: '0', unpricedReasons: []}], contexts: [], recent: []} : {groups: [], contexts: [], recent: []};

const server = await createServer({...testViteCache(import.meta.url), configFile: false, root: fileURLToPath(new URL('../apps/web', import.meta.url)), logLevel: 'warn', plugins: [react()], server: {host: '127.0.0.1', port: 0, proxy: {'/lazybox.bench.v1.RunnerService/': {target: f.url}}}});
const counted = () => f.calls.filter(c => c.method === 'ListWorkspaceGitSummaries' && c.body.changes).length;
let browser;
try {
  await server.listen();
  await mkdir('.cache', {recursive: true});
  browser = await webkit.launch({headless: true});
  const context = await browser.newContext({viewport: {width: 1400, height: 900}, colorScheme: 'dark'});
  const page = await context.newPage();
  page.setDefaultTimeout(10_000);
  const errors = []; page.on('pageerror', e => errors.push(String(e)));
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=beta#token=${f.token}`);
  const nav = page.getByRole('navigation', {name: 'Workspaces'});
  const row = nav.locator('.workspace-row').filter({hasText: 'Alpha feature'});
  const link = row.locator('.workspace-link');
  await expect(row.locator('.workspace-git .git-pull').first()).toHaveText('#812');
  const card = page.getByRole('dialog', {name: 'Alpha feature'});
  assert.equal(counted(), 0, 'nothing heavy is read before a card opens');

  // 1. Resting on the row: nothing at once, the card after a moment, beside the row.
  // The line counts are held back: the card never waits on them, it shows where they will be.
  let release; const held = new Promise(resolve => { release = resolve; });
  await page.route('**/lazybox.bench.v1.RunnerService/ListWorkspaceGitSummaries', async route => { if ((route.request().postData() || '').includes('"changes":true')) await held; await route.continue(); });
  const box = await link.boundingBox();
  await page.mouse.move(box.x + 40, box.y + 12);
  await page.waitForTimeout(150);
  await expect(card).toHaveCount(0);
  await expect(card).toBeVisible();
  await expect(card.getByRole('region', {name: 'Repositories'}).locator('.workspace-hover-skeleton')).toHaveCount(2);
  await expect(card).toContainText('feature/uploads-retry');
  await card.screenshot({path: '.cache/workspace-hover-card-reading.png'});
  release();
  // No tooltip over the row while its card says it all.
  await page.waitForTimeout(500);
  await expect(page.locator('.lazybox-tooltip:popover-open')).toHaveCount(0);

  // 2. What it says.
  await expect(card.getByRole('heading', {name: 'Alpha feature'})).toBeVisible();
  await expect(card.getByRole('link', {name: /#123 Fix uploads/})).toHaveAttribute('href', 'https://github.com/acme/api/issues/123');
  await expect(card).toContainText('created 2 days ago · active 5 min ago');
  const repos = card.getByRole('region', {name: 'Repositories'});
  const apiRepo = repos.locator('li').filter({hasText: 'acme/'}).filter({hasText: 'api'}).first();
  await expect(apiRepo).toContainText('feature/uploads-retry');
  await expect(apiRepo.locator('.workspace-hover-level')).toHaveText('↑3 ↓1');
  await expect(apiRepo.locator('.workspace-hover-level')).toHaveAttribute('aria-label', '3 commits ahead of main, 1 commit behind main');
  await expect(apiRepo.locator('.workspace-hover-changes')).toHaveText('+120 −30 · 6 files');
  await expect(apiRepo.locator('.workspace-hover-changes')).toHaveAttribute('aria-label', '120 lines added, 30 lines deleted, in 6 files against main');
  await expect(repos.locator('li').nth(1).locator('.workspace-hover-changes')).toHaveText('clean');
  assert.equal(counted(), 1, 'line counts are read when the card opens, once');
  // A long branch is cut, not wrapped: each repository stays two lines.
  const branch = apiRepo.locator('.workspace-hover-branch');
  assert.ok(await branch.evaluate(n => n.scrollWidth > n.clientWidth), 'the long branch is truncated');
  const pulls = card.getByRole('region', {name: 'Pull requests'});
  await expect(pulls.locator('li')).toHaveCount(2);
  await expect(pulls.locator('li').first()).toContainText('#812');
  await expect(pulls.locator('li').first().locator('.workspace-hover-pill')).toHaveText(['Open', '2 failing']);
  await expect(pulls.locator('li').nth(1).locator('.workspace-hover-pill')).toHaveText(['Open', 'Checks', 'Approved']);
  await expect(pulls.getByRole('link', {name: /#812 Retry failed uploads/})).toHaveAttribute('href', 'https://github.com/acme/api/pull/812');
  const agents = card.getByRole('region', {name: 'Agents'});
  await expect(agents.locator('.workspace-hover-agent-title')).toHaveText(['Retry the uploads', 'Write the progress bar', 'Read the upload code']);
  await expect(agents.locator('.workspace-hover-state')).toHaveText(['Needs you', 'Working', 'Done']);
  await expect(agents.locator('.live-indicator').first()).toHaveAttribute('data-tone', 'waiting');
  await expect(agents.locator('.live-indicator').nth(1)).toHaveAttribute('data-tone', 'busy');
  await expect(agents.locator('li').first()).toContainText('Claude');
  await expect(agents.locator('li').nth(1)).toContainText('gpt-5.4');
  const running = card.getByRole('region', {name: 'Running'});
  await expect(running.locator('li')).toHaveCount(2);
  await expect(running.locator('li').first()).toHaveText(new RegExp(`^web:${alphaRuntime.actions[0].port}$`));
  await expect(running.getByRole('link', {name: /^web on port \d+, running: open it$/})).toHaveAttribute('href', alphaRuntime.actions[0].url);
  await expect(running.locator('li').nth(1)).toHaveText('pnpm storybook:6006');
  await expect(card.locator('.workspace-hover-cost')).toHaveText('$1.24 so far');
  const waiting = card.getByRole('button', {name: '2 waiting for you: open the Inbox'});
  await expect(waiting).toBeVisible();
  for (const name of ['Open Alpha feature', 'New conversation', 'Open in Terminal', 'Copy path']) await expect(card.getByRole('button', {name, exact: true})).toBeVisible();

  // 3. Placement: beside the row, never over it, whole inside the window.
  const rowBox = await row.boundingBox(), cardBox = await card.boundingBox();
  assert.ok(cardBox.x >= rowBox.x + rowBox.width, `the card is right of the row (${cardBox.x} vs ${rowBox.x + rowBox.width})`);
  assert.ok(cardBox.y >= 0 && cardBox.y + cardBox.height <= 900, 'the card is inside the window');
  await page.screenshot({path: '.cache/workspace-hover-dark.png'});
  await card.screenshot({path: '.cache/workspace-hover-card-dark.png'});

  // 4. The pointer crosses into the card: it stays; it leaves: the card goes.
  await page.mouse.move(cardBox.x + 30, cardBox.y + 20, {steps: 4});
  await page.waitForTimeout(400);
  await expect(card).toBeVisible();
  await page.mouse.move(cardBox.x + cardBox.width + 200, cardBox.y + 20, {steps: 2});
  await expect(card).toHaveCount(0);

  // 5. "2 waiting for you" opens the Inbox; the card closes.
  await page.mouse.move(box.x + 40, box.y + 12);
  await expect(card).toBeVisible();
  const again = await card.boundingBox();
  await page.mouse.move(again.x + 40, again.y + 40, {steps: 3});
  await waiting.click();
  await expect(card).toHaveCount(0);
  // The Inbox is the Agents panel under Workspaces: brought forward, the conversation that needs you first.
  await expect(page.locator('.panel-agents .agent-row').first()).toBeFocused();
  await expect(page.locator('.panel-agents .agent-row').first()).toContainText('Retry the uploads');
  // Opened again within a short while: what it read is shown at once, not read again.
  await page.mouse.move(box.x + 40, box.y + 12);
  await expect(card.locator('.workspace-hover-changes').first()).toHaveText('+120 −30 · 6 files');
  assert.equal(counted(), 1, 'a card opened again soon after shows what it read');
  // An agent row opens its conversation.
  await page.mouse.move((await card.boundingBox()).x + 40, (await card.boundingBox()).y + 40, {steps: 3});
  await card.getByRole('button', {name: /^Write the progress bar, Working/}).click();
  await expect(card).toHaveCount(0);
  await expect(link).toHaveAttribute('aria-current', 'page');
  await expect(nav.locator('.conversation-link[aria-current=page]')).toContainText('Write the progress bar');
  await page.mouse.move(1300, 850);

  // 6. The keyboard: ⌥↓ on the focused row opens it, focus inside; Tab moves through it; Esc closes and returns.
  await link.focus();
  await page.keyboard.press('Alt+ArrowDown');
  await expect(card).toBeVisible();
  await expect(card).toBeFocused();
  await page.keyboard.press('Tab');
  await expect(card.getByRole('link', {name: /#123 Fix uploads/})).toBeFocused();
  await page.keyboard.press('Tab');
  await expect(card.getByRole('button', {name: '2 waiting for you: open the Inbox'})).toBeFocused();
  await page.keyboard.press('Shift+Tab'); await page.keyboard.press('Shift+Tab');
  await expect(card.getByRole('button', {name: 'Copy path'})).toBeFocused();
  await page.keyboard.press('Escape');
  await expect(card).toHaveCount(0);
  await expect(link).toBeFocused();
  // The row says its shortcut.
  await expect(link).toHaveAttribute('aria-keyshortcuts', 'Alt+ArrowDown');
  // Esc on the row closes a card the keyboard opened, even before moving into it.
  await page.keyboard.press('Alt+ArrowDown');
  await expect(card).toBeVisible();
  await link.focus();
  await page.keyboard.press('Escape');
  await expect(card).toHaveCount(0);

  // 7. Light.
  await page.emulateMedia({colorScheme: 'light'});
  await page.mouse.move(box.x + 40, box.y + 12);
  await expect(card).toBeVisible();
  await expect(card.locator('.workspace-hover-changes').first()).toHaveText('+120 −30 · 6 files');
  await page.waitForTimeout(200);
  await page.screenshot({path: '.cache/workspace-hover-light.png'});
  await card.screenshot({path: '.cache/workspace-hover-card-light.png'});
  await page.mouse.move(1300, 850);
  await expect(card).toHaveCount(0);

  // 8. A narrow sidebar: the card still sits beside the row, whole.
  const handle = page.getByRole('separator', {name: 'Resize Workspaces sidebar'});
  await handle.focus();
  for (let i = 0; i < 12; i++) await page.keyboard.press('ArrowLeft');
  await page.waitForTimeout(300);
  const narrowLink = await link.boundingBox();
  await page.mouse.move(narrowLink.x + 30, narrowLink.y + 12);
  await expect(card).toBeVisible();
  const narrowRow = await row.boundingBox(), narrowCard = await card.boundingBox();
  assert.ok(narrowRow.width < 260, `the sidebar is narrow (${narrowRow.width})`);
  assert.ok(narrowCard.x >= narrowRow.x + narrowRow.width, 'beside the narrow row');
  assert.ok(narrowCard.x + narrowCard.width <= 1400 && narrowCard.y + narrowCard.height <= 900, 'whole inside the window');
  await page.screenshot({path: '.cache/workspace-hover-narrow-sidebar.png'});
  await page.mouse.move(1300, 850);
  await expect(card).toHaveCount(0);

  // 9. A narrow window: the workspaces are a navigator over the page; no room beside a row, so the card goes below it, never over it.
  await page.setViewportSize({width: 700, height: 900});
  await page.getByRole('button', {name: 'Open workspace navigator'}).click();
  const navigator = page.getByRole('dialog', {name: 'Workspace navigator'});
  const smallLink = navigator.locator('.workspace-row').filter({hasText: 'Alpha feature'}).locator('.workspace-link');
  const small = await smallLink.boundingBox();
  await page.mouse.move(small.x + 30, small.y + 12);
  await expect(card).toBeVisible();
  const smallRow = await navigator.locator('.workspace-row').filter({hasText: 'Alpha feature'}).boundingBox(), smallCard = await card.boundingBox();
  const overlaps = !(smallCard.x >= smallRow.x + smallRow.width || smallCard.x + smallCard.width <= smallRow.x || smallCard.y >= smallRow.y + smallRow.height || smallCard.y + smallCard.height <= smallRow.y);
  assert.ok(!overlaps, 'the card never covers its row');
  assert.ok(smallCard.x >= 0 && smallCard.x + smallCard.width <= 700 && smallCard.y + smallCard.height <= 900, 'whole inside the window');
  assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false, 'no sideways scroll');
  // Over the navigator (a modal), the card is not inert: its buttons answer.
  await page.mouse.move(smallCard.x + 40, smallCard.y + 30, {steps: 3});
  await expect(card.getByRole('button', {name: 'Copy path'})).toBeEnabled();
  await page.screenshot({path: '.cache/workspace-hover-narrow-window.png'});
  await page.keyboard.press('Escape');
  await expect(card).toHaveCount(0);
  await expect(navigator).toBeVisible();

  assert.deepEqual(errors, []);
  console.log('PASS workspace hover: after a moment beside the row (never over it), the task and age, repositories with branch, ↑↓ against main and +/− lines (read on open, once), pull requests with pills, agents with live status, "2 waiting for you" opening the Inbox, ports, cost and quick actions; the pointer crosses into it, leaving closes it; ⌥↓ and Esc from the keyboard; dark, light, a narrow sidebar and a narrow window. Fake Runner, WebKit.');
} finally {
  await browser?.close(); await server.close(); await f.close();
}
