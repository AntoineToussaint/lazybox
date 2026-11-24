// Tracked pull requests against the fake Runner: the workspace header's
// pills (the branch's opens the PR loop, another branch's opens GitHub), the
// sidebar's one badge, the Pull requests panel and its keys (j/k, s sends
// the failures, m merges, each confirmed; Enter opens the workspace), the
// Inbox's rows whose "Send the failures" reaches the conversation with its
// chip, and the phone's Inbox. Nothing runs an agent or talks to GitHub.
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from 'vite';
import react from '@vitejs/plugin-react';
import {chromium, devices, expect} from './fixtures/playwright.mjs';
import {fileURLToPath} from 'node:url';
import {mkdir} from 'node:fs/promises';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';

const f = await startRunnerFixture();
const now = Date.now(), ms = value => String(value);
Object.assign(f.sessions[0], {title: 'Fix refund rounding', provider: 'claude', status: 'ready'});
Object.assign(f.sessions[1], {title: 'Ship release notes', provider: 'codex', status: 'ready'});
const channel = f.openEvents('alpha-session');
const api = f.shipState('alpha', 'api');
Object.assign(api, {githubRepository: 'acme/api', upstream: 'origin/' + api.branch});
const link = ({key, workspace = 'alpha', session = 'alpha-session', head = 'agent/other', member = 'api', sha = 'sha-' + key.split('#')[1], state = 'review', checks = 'pending', review = '', mergeable = false, mergeableState = '', failing = [], total = 0, closedAt = 0, role = 'delivers', title = ''}) => ({
  collectionId: workspace, collectionName: workspace === 'alpha' ? 'Alpha feature' : 'Beta release', role, source: 'agent-output', sessionId: session, conversationId: session, member, linkedAtUnixMillis: ms(now - 3_600_000),
  item: role === 'mentions' ? {ref: {provider: 'github', key}, kind: 'pull_request', url: `https://github.com/${key.replace('#', '/pull/')}`} : {
    ref: {provider: 'github', key}, kind: 'pull_request', title, url: `https://github.com/${key.replace('#', '/pull/')}`, repository: key.split('#')[0],
    state: {category: state, label: state}, updatedAtUnixMillis: ms(now - 600_000),
    pullRequest: {head, base: 'main', headSha: sha, checks, review, mergeable, mergeableState, checksTotal: total, closedAtUnixMillis: ms(closedAt),
      failingChecks: failing.map(name => ({name, url: `https://github.com/acme/api/actions/runs/41/job/${name}`}))},
  },
});
f.tracked.links.push(
  link({key: 'acme/api#812', head: api.branch, checks: 'failure', failing: ['lint', 'unit-api'], total: 14, title: 'Fix refund rounding'}),
  link({key: 'acme/api#77', checks: 'success', review: 'changes_requested', title: 'Tidy the ledger'}),
  link({key: 'acme/web#90', workspace: 'beta', session: 'beta-session', checks: 'success', review: 'approved', mergeable: true, mergeableState: 'clean', title: 'Release notes page'}),
  link({key: 'acme/api#61', checks: 'pending', title: 'Speed up tests'}),
  link({key: 'acme/api#60', state: 'merged', checks: 'success', closedAt: now - 2 * 24 * 3_600_000, title: 'Old work'}),
  link({key: 'acme/api#9', role: 'mentions'}),
  // acme/platform#1121 as an agent's gh pr create left it: open, checks green, a review required.
  link({key: 'acme/platform#1121', workspace: 'beta', session: 'beta-session', member: 'platform', head: 'feature/quietly-brave-otter', checks: 'success', review: 'review_required', mergeableState: 'blocked', total: 69, title: "Retry failed uploads from the worker"}),
);

const proxy = {'/lazybox.bench.v1.RunnerService/': {target: f.url}, '/remote/v1/': {target: f.url}};
const server = await createServer({...testViteCache(import.meta.url), configFile: false, root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: {host: '127.0.0.1', port: 0, proxy}});
const sent = method => f.mutations.filter(m => m.method === method);
let browser;
try {
  await server.listen();
  const origin = `http://127.0.0.1:${server.httpServer.address().port}`;
  browser = await chromium.launch({headless: true});
  const desktop = await browser.newContext({viewport: {width: 1500, height: 950}});
  const page = await desktop.newPage();
  page.setDefaultTimeout(10_000);
  const errors = []; page.on('pageerror', e => errors.push(String(e)));
  await page.goto(`${origin}/?view=workspace&collection=alpha&session=alpha-session#token=${f.token}`);
  await mkdir('.cache', {recursive: true});

  // 1. The header: a pill per open pull request, most urgent first; one merged two days ago is not there.
  const pills = page.getByRole('group', {name: 'Pull requests of this workspace'});
  await expect(pills.locator('.pr-pill')).toHaveText(['#812', '#77', '#61']);
  await expect(pills.locator('.pr-pill').first()).toHaveClass(/is-failure/);
  await expect(pills.getByRole('link', {name: /#77, changes requested: open on GitHub/})).toHaveAttribute('href', 'https://github.com/acme/api/pull/77');
  assert.ok(f.tracked.requests.some(r => r.all), 'every workspace\'s links, in one request');
  // The branch's own pull request opens the PR loop, and still offers GitHub.
  await expect(pills.getByRole('link', {name: 'Open #812 on GitHub'})).toHaveAttribute('href', 'https://github.com/acme/api/pull/812');
  await page.screenshot({path: '.cache/pull-requests-header.png'});
  // 2. The sidebar: one badge per row, the most urgent pull request, and how many more.
  const alphaRow = page.locator('.workspace-row', {hasText: 'Alpha feature'}).first();
  await expect(alphaRow.locator('.git-pull').first()).toHaveText('#812');
  await expect(alphaRow.locator('.git-pull').first()).toHaveClass(/is-failure/);
  await expect(alphaRow.locator('.git-pull.is-more')).toHaveText('+2');
  const betaRow = page.locator('.workspace-row', {hasText: 'Beta release'}).first();
  await expect(betaRow.locator('.git-pull').first()).toHaveText('#90');
  await expect(betaRow.locator('.git-pull').first()).toHaveClass(/is-success/);
  await expect(betaRow.locator('.git-pull.is-more')).toHaveText('+1');
  const sidebarWidth = await page.locator('.collection-list').evaluate(e => e.getBoundingClientRect().width);
  // The branch's own pull request opens the PR loop.
  await pills.getByRole('button', {name: /#812, checks failed: open the PR loop/}).click();
  await expect(page.getByRole('region', {name: 'Workspace changes'})).toBeVisible();

  // 3. The panel, from the palette: three groups and the mentions apart.
  await page.keyboard.press('Meta+k');
  await page.keyboard.type('Show pull requests');
  await page.keyboard.press('Enter');
  const panel = page.locator('.pull-requests-panel');
  await expect(panel).toBeVisible();
  const titles = await panel.locator('.pr-group h3').allInnerTexts();
  assert.deepEqual(titles.map(t => t.replace(/\s+/g, ' ').trim().toLowerCase()), ['needs you 3', 'waiting on others 2', 'merged this week 1']);
  await expect(panel.locator('.group-needs .pr-title strong')).toHaveText(['acme/api#812', 'acme/api#77', 'acme/web#90']);
  await expect(panel.locator('.group-needs .pr-item').first().locator('.pr-meta em')).toHaveText('lint, unit-api (2 of 14)');
  await expect(panel.locator('.pr-mentioned summary')).toContainText('1');
  // A pull request waiting on a required review, checks green: waiting on others, and it says why.
  await expect(panel.locator('.group-waiting .pr-title strong')).toHaveText(['acme/api#61', 'acme/platform#1121']);
  const waiting = panel.locator('.group-waiting .pr-item', {hasText: 'acme/platform#1121'});
  await expect(waiting.locator('.pr-meta em')).toHaveText('Review required · checks passed');
  await expect(waiting.locator('.pr-status')).toHaveClass(/is-success/);
  await expect(waiting.getByRole('link', {name: 'Open acme/platform#1121 on GitHub'})).toHaveAttribute('href', 'https://github.com/acme/platform/pull/1121');
  if (await page.locator('.collection-list').count()) assert.equal(await page.locator('.collection-list').evaluate(e => e.getBoundingClientRect().width), sidebarWidth, 'nothing resizes the sidebar');
  await page.screenshot({path: '.cache/pull-requests-panel.png'});

  // j/k move; s on the failing one asks, Enter sends; nothing is sent before the confirmation.
  await panel.locator('.pr-row').first().focus();
  await page.keyboard.press('j');
  await expect(panel.locator('.pr-row').nth(1)).toBeFocused();
  await page.keyboard.press('k');
  await expect(panel.locator('.pr-row').first()).toBeFocused();
  await page.keyboard.press('s');
  const confirm = panel.getByRole('group', {name: 'Confirm sending the failures'});
  await expect(confirm).toContainText('Send the failing checks of #812 (lint, unit-api (2 of 14)) to Claude?');
  await expect(confirm.getByRole('button', {name: 'Send'})).toBeFocused();
  assert.equal(sent('SendPullRequestFailures').length, 0, 'asked first');
  await page.keyboard.press('Escape');
  await expect(confirm).toHaveCount(0);
  await expect(panel.locator('.pr-row').first()).toBeFocused();
  // The send takes a moment; moving to another row meanwhile keeps you there when it ends
  // (its end used to take focus back to #812, so m or Enter acted on the wrong pull request).
  await page.route('**/SendPullRequestFailures', async route => { await new Promise(resolve => setTimeout(resolve, 800)); await route.continue(); });
  await page.keyboard.press('s');
  await page.keyboard.press('Enter');
  const ready = panel.locator('.pr-item', {hasText: 'acme/web#90'}).locator('.pr-row');
  await ready.focus();
  await expect.poll(() => sent('SendPullRequestFailures').length).toBe(1);
  assert.deepEqual(sent('SendPullRequestFailures')[0].body.ref, {provider: 'github', key: 'acme/api#812'});
  await expect(panel.locator('.pr-item').first().locator('.pr-note')).toContainText('Sent 2 failing checks of #812 to the agent');
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  await expect(ready, 'the end of the send leaves focus where the person moved it').toBeFocused();
  await page.unroute('**/SendPullRequestFailures');
  // m on the ready one merges after its confirmation, with the head the person saw.
  await ready.focus();
  await page.keyboard.press('m');
  await expect(panel.getByRole('group', {name: 'Confirm merge'})).toContainText('Merge #90 into main?');
  await page.keyboard.press('Enter');
  await expect.poll(() => sent('MergePullRequest').length).toBe(1);
  const merge = sent('MergePullRequest')[0].body;
  assert.deepEqual([merge.item?.key, merge.expectedHead, merge.collectionId], ['acme/web#90', 'sha-90', 'beta']);
  await expect(panel.locator('.group-merged .pr-title strong')).toHaveText(['acme/web#90', 'acme/api#60'], {timeout: 10_000});
  // Enter opens the workspace, at the conversation that delivered it.
  await panel.locator('.pr-item', {hasText: 'acme/web#90'}).locator('.pr-row').focus();
  await page.keyboard.press('Enter');
  await expect(page.locator('.workspace-row[aria-current], .workspace-link[aria-current=page]').first()).toContainText('Beta release');
  // Its header: #1121's pill, green checks dot, review required, opening GitHub.
  const betaPill = page.getByRole('group', {name: 'Pull requests of this workspace'}).getByRole('link', {name: 'Pull request #1121, review required, checks passed: open on GitHub'});
  await expect(betaPill).toHaveAttribute('href', 'https://github.com/acme/platform/pull/1121');
  await expect(betaPill).toHaveText('#1121review required');
  await expect(betaPill.locator('.pr-dot')).toHaveClass(/is-success/);
  await page.screenshot({path: '.cache/pull-requests-review-required.png'});

  // 4. The conversation shows what Lazybox sent, as its chip, not the person's words.
  // (A fresh window: this one remembers the page it last showed for the workspace.)
  const chat = await (await browser.newContext({viewport: {width: 1400, height: 900}})).newPage(); chat.on('pageerror', e => errors.push(String(e)));
  await chat.goto(`${origin}/?view=workspace&collection=alpha&session=alpha-session#token=${f.token}`);
  const chip = chat.locator('.system-event').filter({hasText: 'Checks failed on acme/api#812'});
  await expect(chip).toBeVisible();
  await expect(chip).toContainText('lint, unit-api');
  await chat.screenshot({path: '.cache/pull-requests-chip.png'});
  await chat.close();

  // 5. The Inbox (the page's Agents panel, or its dialog where the page has none): a row per
  // pull request that needs the person, with its reaction.
  const inboxButton = page.locator('.inbox-action');
  await page.reload();
  await expect(inboxButton).toHaveAccessibleName(/1 pull request/);
  await inboxButton.click();
  const prior = page.locator('.pr-inbox-item', {hasText: 'acme/api#812'});
  await expect(prior.getByRole('button', {name: 'Send the failures to Claude'})).toHaveCount(0);
  assert.equal(sent('SendPullRequestFailures').length, 1);
  f.tracked.links[0].item.pullRequest.failingChecks[0].url = 'https://github.com/acme/api/actions/runs/42/job/lint';
  await page.reload();
  await expect(inboxButton).toHaveAccessibleName(/2 pull requests need you/);
  await inboxButton.click();
  const pullRows = page.locator('.group-pulls .pr-inbox-item');
  await expect(pullRows).toHaveCount(2);
  await expect(pullRows.first()).toContainText('acme/api#812 checks failed · lint, unit-api (2 of 14)');
  await expect(pullRows.first().getByRole('link', {name: 'Open checks'})).toHaveAttribute('href', 'https://github.com/acme/api/actions/runs/42/job/lint');
  const sibling = await page.context().newPage();
  await sibling.goto(page.url().split('#')[0] + '#token=' + f.token);
  await sibling.locator('.inbox-action').click();
  const siblingFailure = sibling.locator('.pr-inbox-item', {hasText: 'acme/api#812'});
  const siblingSend = siblingFailure.getByRole('button', {name: 'Send the failures to Claude'});
  await expect(siblingSend).toBeEnabled();
  let releaseSend;
  const release = new Promise(resolve => { releaseSend = resolve; });
  await page.route('**/SendPullRequestFailures', async route => { await release; await route.fetch(); await route.abort(); });
  await page.screenshot({path: '.cache/pull-requests-inbox.png'});
  await pullRows.first().getByRole('button', {name: 'Send the failures to Claude'}).click();
  try { await expect(siblingSend).toBeDisabled(); } finally { releaseSend(); }
  await expect.poll(() => sent('SendPullRequestFailures').length).toBe(2);
  await expect(siblingSend).toBeEnabled();
  await siblingSend.click();
  await expect(siblingFailure).toContainText('Auto-dismiss in');
  assert.equal(f.tracked.sent.filter(item => item.method === 'SendPullRequestFailures').length, 2, 'a lost acknowledgement recovers the original dispatch');
  await expect(siblingSend).toHaveCount(0);
  await sibling.close();
  await expect(pullRows.first().locator('.pr-inbox-note')).toContainText('Sent 2 failing checks');
  await expect(pullRows.first().getByRole('button', {name: 'Send the failures to Claude'})).toHaveCount(0);
  await expect(pullRows.first()).toContainText(/Auto-dismiss in \d+s/);
  const reviewRow = pullRows.nth(1);
  f.tracked.refusal = 'Review unavailable';
  await reviewRow.getByRole('button', {name: 'Address the review'}).click();
  await expect(reviewRow.getByRole('alert')).toContainText('Review unavailable');
  await expect(reviewRow.getByRole('button', {name: 'Address the review'})).toBeEnabled();
  await page.reload();
  await inboxButton.click();
  await expect(reviewRow.getByRole('alert')).toContainText('Review unavailable');
  await expect(pullRows.first()).toContainText('Auto-dismiss in');
  await expect(pullRows.first().getByRole('button', {name: 'Send the failures to Claude'})).toHaveCount(0);
  f.tracked.refusal = '';
  f.tracked.externalReviewOnly = true;
  await reviewRow.getByRole('button', {name: 'Address the review'}).click();
  await expect(reviewRow.getByRole('status')).toContainText('No new review feedback was sent.');
  await expect(reviewRow.getByRole('link', {name: 'Open review'})).toHaveAttribute('href', 'https://github.com/acme/api/pull/77/files');
  await reviewRow.screenshot({path: '.cache/pull-requests-external-review.png'});
  assert.equal(f.tracked.sent.filter(item => item.method === 'ImportPullRequestReview').length, 0);
  f.tracked.externalReviewOnly = false;
  await pullRows.nth(1).getByRole('button', {name: /Dismiss acme\/api#77/}).click();
  await expect(pullRows).toHaveCount(1);
  await expect(pullRows).toHaveCount(0, {timeout: 12_000});
  await page.keyboard.press('Escape');

  // 5b. Approved and green, but behind a base its repository requires it to match: GitHub calls it
  // mergeable yet refuses the merge. The row offers Update branch (asked first, at the head shown), never Merge.
  f.tracked.links.push(link({key: 'acme/web#94', workspace: 'beta', session: 'beta-session', checks: 'success', review: 'approved', mergeable: true, mergeableState: 'behind', title: 'Solution page'}));
  await page.reload();
  await inboxButton.click();
  const behind = pullRows.filter({hasText: 'acme/web#94'});
  await expect(behind).toContainText('acme/web#94 checks green · out of date with main');
  await expect(behind.getByRole('button', {name: /Merge/})).toHaveCount(0);
  await behind.getByRole('button', {name: 'Update branch…'}).click();
  const askUpdate = behind.getByRole('group', {name: 'Confirm updating the branch'});
  await expect(askUpdate).toContainText('Update #94 with main? GitHub merges main into its branch');
  await behind.screenshot({path: '.cache/pull-requests-behind.png'});
  await askUpdate.getByRole('button', {name: 'Update #94'}).click();
  await expect.poll(() => sent('UpdatePullRequestBranch').length).toBe(1);
  const update = sent('UpdatePullRequestBranch')[0].body;
  assert.deepEqual({collection: update.collectionId, head: update.expectedHead, key: update.item?.key}, {collection: 'beta', head: 'sha-94', key: 'acme/web#94'});
  // Updated, its checks run again: it no longer waits on the person.
  await expect(pullRows.filter({hasText: 'acme/web#94'})).toHaveCount(0, {timeout: 12_000});
  await page.keyboard.press('Escape');

  // 6. The phone: the same rows, thumb-sized, with Undo before a send.
  f.remote.tailscale = {...f.remote.tailscale, enabled: true, state: 'serving'};
  f.remote.phoneUrl = origin;
  const offer = await (await fetch(`${f.url}/lazybox.bench.v1.RunnerService/BeginDevicePairing`, {method: 'POST', headers: {authorization: `Bearer ${f.token}`, 'content-type': 'application/json'}, body: JSON.stringify({grants: ['respond']})})).json();
  const phoneContext = await browser.newContext({...devices['iPhone 13'], isMobile: false});
  const phone = await phoneContext.newPage(); phone.on('pageerror', e => errors.push(String(e)));
  await phone.goto(offer.url);
  await phone.getByRole('textbox', {name: 'Name this phone'}).fill('iPhone');
  await phone.getByRole('button', {name: 'Pair this phone'}).click();
  await phone.getByRole('button', {name: 'Open inbox'}).click();
  const phoneRows = phone.locator('.group-pulls .pr-inbox-item');
  await expect(phoneRows).toHaveCount(2);
  await expect(phoneRows.first().getByRole('button', {name: 'Send the failures to Claude'})).toBeVisible();
  assert.equal(await phoneRows.nth(1).getByRole('button', {name: /Merge/}).count(), 0, 'a phone does not merge');
  await phone.screenshot({path: '.cache/pull-requests-phone.png'});
  assert.equal(await phone.evaluate(() => document.documentElement.scrollWidth > innerWidth), false, 'no sideways scrolling on a phone');
  await phoneRows.first().getByRole('button', {name: 'Send the failures to Claude'}).click();
  await expect.poll(() => sent('SendPullRequestFailures').length, {timeout: 15_000}).toBe(4);
  assert.equal(f.tracked.sent.filter(item => item.method === 'SendPullRequestFailures').length, 2, 'the phone recovers the desktop delivery receipt');
  await expect(phoneRows.first().getByRole('button', {name: 'Send the failures to Claude'})).toHaveCount(0);
  await expect(phoneRows.first()).toContainText('Auto-dismiss in');

  await phoneRows.nth(1).getByRole('button', {name: 'Address the review'}).click();
  await expect.poll(() => f.tracked.sent.filter(item => item.method === 'ImportPullRequestReview').length, {timeout: 15_000}).toBe(1);
  assert.deepEqual(errors, []);
  console.log('PASS: header pills, sidebar badge, Pull requests panel and keys with confirmations, the chip in the conversation, Inbox reactions, phone rows.');
} finally { await browser?.close(); await server.close(); await f.close(); }
