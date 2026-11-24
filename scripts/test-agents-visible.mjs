// Agents always on screen, and the conversation row as a first-class object
// (audits/2026-11-16/herdr-gui.md, R1). On a fresh profile, Workspaces and
// Agents are both visible without a click, in dark and light, wide and
// narrow; no tab strip scrolls; the pin never covers the age; the panel's
// background reaches the bottom. Every workspace lists its conversations with
// status and age, and folds. Agents: Priority / By workspace, folding with
// counts, unread dots, sub-agents nested under their conversation,
// j/k/Enter/Space, filter as you type. Every conversation row has the same
// menu, in order, Delete last. Pins are the Runner's: a pin made on the
// desktop shows on the paired phone. The Inbox opens as the Agents panel.
// WebKit against the fake Runner; no agent runs.
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from './fixtures/vite-cache.mjs';
import react from '@vitejs/plugin-react';
import {webkit, devices, expect} from './fixtures/playwright.mjs';
import {fileURLToPath} from 'node:url';
import {mkdir} from 'node:fs/promises';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import { collectPageErrors } from './fixtures/page-errors.mjs';

const f = await startRunnerFixture();
const now = Date.now(), ms = value => String(value);
const data = value => Buffer.from(JSON.stringify(value)).toString('base64');
const root = (status, updated, extra = {}) => ({id: 'root', name: 'Main agent', status, updatedAtUnixMillis: ms(updated), ...extra});
const base = {profileId: 'claude-default', provider: 'claude', model: 'claude-fable-5', capabilities: {files: true}, pending: [], permission: 'ask'};
Object.assign(f.sessions[0], {title: 'Index the API', model: 'claude-fable-5', status: 'running', busy: true, createdAtUnixMillis: ms(now - 9_000_000),
  agents: [root('running', now - 40_000, {phase: 'tool', tool: 'Bash', phaseSinceUnixMillis: ms(now - 12_000), turnStartedAtUnixMillis: ms(now - 40_000)}),
    {id: 'sub-flaky', parentId: 'root', name: 'Find the flaky test', status: 'running', phase: 'tool', tool: 'Grep', phaseSinceUnixMillis: ms(now - 8_000), updatedAtUnixMillis: ms(now - 8_000)}]});
Object.assign(f.sessions[1], {title: 'Ship release notes', provider: 'codex', model: 'gpt-codex-6', status: 'waiting_input', createdAtUnixMillis: ms(now - 8_000_000), agents: [root('waiting_input', now - 300_000)],
  pending: [{type: 'question', requestId: 'q-release', providerDataJson: data({questions: [{question: 'Which version should the notes cover?'}]})}]});
f.sessions.push(
  {...base, id: 'review-session', collectionId: 'alpha', workingDirectory: '/fixture/collections/alpha', title: 'Refactor the parser into three files', status: 'done', createdAtUnixMillis: ms(now - 3_600_000), agents: [root('done', now - 120_000)], lastMessage: 'All 42 tests pass.'},
  {...base, id: 'failing-session', collectionId: 'beta', workingDirectory: '/fixture/collections/beta', title: 'Deploy preview', status: 'failed', fault: 'provider delivery failed; reconcile before continuing', createdAtUnixMillis: ms(now - 7_200_000), agents: [root('failed', now - 20_000)]},
  {...base, id: 'old-session', collectionId: 'alpha', workingDirectory: '/fixture/collections/alpha', title: 'Old exploration of the auth flow', status: 'done', createdAtUnixMillis: ms(now - 9e7), agents: [root('done', now - 8.64e7, {turnEndedAtUnixMillis: ms(now - 8.64e7), lastTurnStatus: 'completed'})], lastMessage: 'Nothing else to do.'},
);
const session = id => f.sessions.find(s => s.id === id);
f.remote.tailscale = {...f.remote.tailscale, enabled: true, state: 'serving'};

const proxy = {'/lazybox.bench.v1.RunnerService/': {target: f.url}, '/remote/v1/': {target: f.url}};
const server = await createServer({...testViteCache(import.meta.url), configFile: false, root: fileURLToPath(new URL('../apps/web', import.meta.url)), logLevel: 'warn', plugins: [react()], server: {host: '127.0.0.1', port: 0, proxy}});
const shot = async (page, name) => { await mkdir('.cache', {recursive: true}); await page.screenshot({path: `.cache/agents-visible-${name}.png`}); };
let browser;
try {
  await server.listen();
  const origin = `http://127.0.0.1:${server.httpServer.address().port}`;
  f.remote.phoneUrl = origin;
  const url = `${origin}/?view=workspace&collection=alpha&session=alpha-session#token=${f.token}`;
  browser = await webkit.launch();
  const errors = [];
  const watch = page => collectPageErrors(page, errors);

  // A review row needs a turn that ended after the profile's first look: end one once the page is open.
  const endReviewTurn = () => Object.assign(session('review-session'), {agents: [root('done', Date.now(), {turnEndedAtUnixMillis: ms(Date.now()), lastTurnStatus: 'completed'})]});

  /** What a fresh profile sees, without a click: both lists, in full, nothing clipped or overlapping. */
  const fresh = async (scheme, width, height) => {
    const context = await browser.newContext({viewport: {width, height}, colorScheme: scheme});
    const page = await context.newPage(); page.setDefaultTimeout(15000); await watch(page);
    await page.goto(url);
    const workspaces = page.locator('.panel-collections'), agents = page.locator('.panel-agents');
    await expect(workspaces.locator('.workspace-row').first()).toBeVisible();
    await expect(agents.locator('.agent-row').first()).toBeVisible();
    const top = await workspaces.boundingBox(), below = await agents.boundingBox();
    assert.ok(top && below, `${scheme} ${width}: both regions are on screen`);
    assert.ok(Math.abs(top.x - below.x) < 2 && below.y >= top.y + top.height - 2, `${scheme} ${width}: Agents sits under Workspaces`);
    assert.ok(top.height >= 150 && below.height >= 150, `${scheme} ${width}: neither is squeezed (${top.height}, ${below.height})`);
    assert.ok(below.y + below.height <= height + 1, `${scheme} ${width}: Agents ends inside the window`);
    // No tab strip scrolls: overflow goes into the group's chevron menu.
    for (const strip of await page.locator('.dv-tabs-container').all()) {
      const {scroll, client, text} = await strip.evaluate(n => ({scroll: n.scrollWidth, client: n.clientWidth, text: n.textContent}));
      assert.ok(scroll <= client, `${scheme} ${width}: tab strip "${text}" scrolls (${scroll} > ${client})`);
    }
    // Nothing hovered: no pin box over an age box.
    await page.mouse.move(width - 2, height - 2);
    const overlaps = await page.evaluate(() => {
      const boxes = s => [...document.querySelectorAll(s)].map(n => n.getBoundingClientRect()).filter(r => r.width && r.height);
      const hit = (a, b) => a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom;
      return boxes('.agent-age').filter(age => boxes('.agent-pin, .agent-seen').some(pin => hit(age, pin))).length;
    });
    assert.equal(overlaps, 0, `${scheme} ${width}: a pin covers an age`);
    // The panel's background fills its place, to the bottom.
    const fill = await page.evaluate(() => { const panel = document.querySelector('.agents-panel'), host = panel.closest('[data-dock-panel]') || panel.parentElement; return [panel.getBoundingClientRect().bottom, host.getBoundingClientRect().bottom]; });
    assert.ok(Math.abs(fill[0] - fill[1]) <= 1, `${scheme} ${width}: the Agents background stops at ${fill[0]}, its panel at ${fill[1]}`);
    // Every row has its age, right-aligned in the row.
    for (const row of await agents.locator('.agent-row').all()) {
      const [rowBox, ageBox] = await Promise.all([row.boundingBox(), row.locator('.agent-age').boundingBox()]);
      assert.ok(rowBox.x + rowBox.width - (ageBox.x + ageBox.width) < 12, `${scheme} ${width}: the age is at the row's end`);
    }
    return {context, page};
  };

  for (const scheme of ['dark', 'light']) for (const [width, height] of [[1440, 900], [1024, 680]]) {
    const {context, page} = await fresh(scheme, width, height);
    await shot(page, `${scheme}-${width}`);
    await page.locator('.panel-agents .agent-row').first().hover();
    await shot(page, `${scheme}-${width}-hover`);
    await context.close();
  }

  const context = await browser.newContext({viewport: {width: 1440, height: 900}, colorScheme: 'dark'});
  await context.addInitScript(() => { window.__copied = []; Object.defineProperty(navigator, 'clipboard', {configurable: true, value: {writeText: async text => { window.__copied.push(text); }}}); });
  const page = await context.newPage(); page.setDefaultTimeout(15000); await watch(page);
  await page.goto(url);
  const workspaces = page.locator('.panel-collections'), agents = page.locator('.panel-agents');
  const sessionChoice = page.getByRole('combobox', {name: 'Session', exact: true});
  await expect(agents.locator('.agent-row').first()).toBeVisible();
  // A turn ends after the person's first look: it is ready for review, unread.
  endReviewTurn();
  await expect(agents.locator('li[data-state="review"] .unread-dot')).toBeVisible({timeout: 10_000});
  await shot(page, 'unread-dark-1440');

  // Workspaces: every workspace with conversations lists them all, one line each — status mark, title, age — and folds.
  const alpha = workspaces.locator('.collection-group').filter({has: page.locator('.workspace-row', {hasText: 'Alpha feature'})});
  const lines = alpha.locator('.workspace-conversations .conversation-row');
  await expect(lines).toHaveCount(3);
  for (const line of await lines.all()) {
    await expect(line.locator('.session-mark, .session-dot')).toHaveCount(1);
    await expect(line.locator('.conversation-age')).toHaveText(/^(now|\d+(m|h|d|mo))$/);
    await expect(line.locator('.conversation-agent')).toHaveCount(0);
    const box = await line.boundingBox(); assert.ok(box.height <= 30, `one line per conversation (${box.height}px)`);
  }
  await expect(lines.filter({hasText: 'Index the API'}).locator('.conversation-link')).toHaveAttribute('title', /Claude · claude-fable-5/);
  const fold = alpha.getByRole('button', {name: /^Fold the 3 conversations of Alpha feature/});
  await fold.click();
  await expect(lines).toHaveCount(0);
  await page.reload(); await expect(agents.locator('.agent-row').first()).toBeVisible();
  await expect(lines).toHaveCount(0);
  await alpha.getByRole('button', {name: /^Show the 3 conversations of Alpha feature/}).click();
  await expect(lines).toHaveCount(3);

  // Agents by priority: what needs you first, folded sections keep their counts.
  const titles = () => agents.locator('.agents-group-title').allInnerTexts().then(v => v.map(t => t.replace(/\s+/g, ' ').trim()));
  assert.deepEqual(await titles(), ['NEEDS INPUT 1', 'FAILED 1', 'READY FOR REVIEW 1', 'WORKING 1', 'DONE 1']);
  await expect(agents.locator('.agents-summary')).toHaveText('1 waiting for you');
  await expect(agents.locator('li[data-state="needs-input"] .agent-meta')).toHaveText('Beta release · Question · Which version should the notes cover?');
  // Done starts folded, with its count; a fold is remembered.
  await expect(agents.locator('[data-group="idle"] .agent-row')).toHaveCount(0);
  await agents.locator('[data-group="idle"] .agents-group-title').click();
  await expect(agents.locator('[data-group="idle"] .agent-row')).toHaveCount(1);
  await agents.locator('[data-group="failed"] .agents-group-title').click();
  await expect(agents.locator('[data-group="failed"] .agent-row')).toHaveCount(0);
  await expect(agents.locator('[data-group="failed"] .agents-group-title')).toContainText('1');
  await page.reload(); await expect(agents.locator('[data-group="idle"] .agent-row')).toHaveCount(1);
  await expect(agents.locator('[data-group="failed"] .agent-row')).toHaveCount(0);
  await agents.locator('[data-group="failed"] .agents-group-title').click();
  await agents.getByRole('button', {name: 'By workspace'}).click();
  assert.deepEqual(await titles(), ['BETA RELEASE 2', 'ALPHA FEATURE 3']);
  await agents.getByRole('button', {name: 'Priority'}).click();

  // Sub-agents still at work nest under their conversation, folded.
  const working = agents.locator('li[data-state="working"]');
  await expect(working.locator('.agent-subagents-toggle')).toHaveText('1 sub-agent working');
  await expect(working.locator('.agent-subagent')).toHaveCount(0);
  await working.locator('.agent-row').focus();
  await page.keyboard.press('ArrowRight');
  await expect(working.locator('.agent-subagent')).toContainText('Find the flaky test');
  await page.keyboard.press('ArrowLeft');
  await expect(working.locator('.agent-subagent')).toHaveCount(0);
  // The main agent's turn ends while its sub-agent works on: the row's own line says Running, and
  // does not count the sub-agent again (it truncated as "Running · 1 sub-ag…"): the nested row does.
  Object.assign(f.sessions[0], {busy: false}); Object.assign(f.sessions[0].agents[0], {status: 'done', phase: ''});
  await expect(working.locator('.agent-meta')).toHaveText(/^(alpha feature\s*)?Running$/i, {timeout: 10000});
  await expect(working.locator('.agent-subagents-toggle')).toHaveText('1 sub-agent working');
  Object.assign(f.sessions[0], {busy: true, status: 'running'}); Object.assign(f.sessions[0].agents[0], {status: 'running', phase: 'tool'});

  // Keyboard: j/k move, Space peeks, Enter opens.
  const rows = agents.locator('.agent-row');
  await rows.first().focus();
  await page.keyboard.press('j'); await expect(rows.nth(1)).toBeFocused();
  await page.keyboard.press('j'); await expect(rows.nth(2)).toBeFocused();
  await page.keyboard.press('k'); await expect(rows.nth(1)).toBeFocused();
  await page.keyboard.press(' ');
  const peek = page.locator('dialog.peek-dialog');
  await expect(peek).toBeVisible();
  await expect(peek).toHaveAttribute('aria-label', /Deploy preview/);
  await page.keyboard.press('Escape');
  await expect(peek).toHaveCount(0);
  await rows.nth(1).focus();
  await page.keyboard.press('Enter');
  await expect(sessionChoice).toHaveValue('failing-session');

  // Filter as you type: / focuses it, Escape clears it.
  await rows.first().focus();
  await page.keyboard.press('/');
  const filter = agents.getByRole('searchbox', {name: 'Filter agents'});
  await expect(filter).toBeFocused();
  await page.keyboard.type('flaky');
  await expect(rows).toHaveCount(1);
  await expect(rows.first()).toContainText('Index the API');
  await expect(agents.locator('.agents-summary')).toHaveText('1 of 5');
  await page.keyboard.press('Escape');
  await expect(filter).toHaveValue('');
  await expect(rows).toHaveCount(5);

  // Every conversation row has the same menu, in one order, Delete last.
  const menu = page.locator('.context-menu[data-depth="0"]');
  // The development build adds Inspect element after everything, in its own section.
  const entries = () => menu.evaluate(n => [...n.children].map(child => child.getAttribute('role') === 'separator' ? '—' : child.querySelector('.context-menu-label')?.textContent || '')).then(list => list.join('|').replace(/\|—\|Inspect element$/, '').split('|'));
  const expected = ['Open', 'Open in new tab', 'Open beside', 'Open below', 'Open in new window', 'Move to new page', 'Peek', '—', 'Rename', 'Pin', 'Mark as read', '—', 'Copy ID', 'Stop', 'Open in Terminal…', 'Continue in Claude Code…', '—', 'Archive…', 'Delete'];
  await agents.locator('li', {hasText: 'Refactor the parser'}).locator('.agent-row').click({button: 'right'});
  await expect(menu).toBeVisible();
  assert.deepEqual(await entries(), expected, 'the Agents row menu');
  await expect(menu.getByRole('menuitem', {name: /^Stop/})).toBeDisabled();
  await expect(menu.getByRole('menuitem', {name: /^Mark as read/})).toBeEnabled();
  await page.keyboard.press('Escape');
  await lines.filter({hasText: 'Refactor the parser'}).locator('.conversation-link').click({button: 'right'});
  await expect(menu).toBeVisible();
  assert.deepEqual(await entries(), expected, 'the Workspaces row menu is the same');
  await menu.getByRole('menuitem', {name: /^Copy ID/}).click();
  await expect.poll(() => page.evaluate(() => window.__copied)).toEqual(['review-session']);

  // Mark as read (m) clears the unread dot.
  const review = agents.locator('li', {hasText: 'Refactor the parser'});
  await expect(review.locator('.unread-dot')).toBeVisible();
  await review.locator('.agent-row').focus();
  await page.keyboard.press('m');
  await expect(review.locator('.unread-dot')).toHaveCount(0);

  // Rename in place (F2).
  await review.locator('.agent-row').focus();
  await page.keyboard.press('F2');
  const name = agents.getByRole('textbox', {name: 'Conversation name'});
  await expect(name).toBeFocused();
  await name.fill('Parser split');
  await page.keyboard.press('Enter');
  await expect(agents.locator('.agent-row', {hasText: 'Parser split'})).toBeVisible();
  assert.deepEqual(f.mutations.filter(m => m.method === 'RenameConversation').map(m => m.body), [{sessionId: 'review-session', title: 'Parser split'}]);

  // Pin (p): kept by the Runner, on top of the list, and the pin never covers the age.
  await agents.locator('.agent-row', {hasText: 'Ship release notes'}).focus();
  await page.keyboard.press('p');
  await expect(agents.locator('[data-group="pinned"] .agent-row')).toHaveCount(1);
  await expect(agents.locator('[data-group="pinned"] .agent-row')).toContainText('Ship release notes');
  assert.deepEqual(f.pins.map(p => [p.method, p.body]), [['PinConversation', {sessionId: 'beta-session', pinned: true}]]);
  assert.ok(Number(session('beta-session').pinnedAtUnixMillis) > 0, 'the Runner keeps the pin');
  await page.mouse.move(1430, 890);
  const pinnedRow = agents.locator('[data-group="pinned"] li');
  const [age, pin] = await Promise.all([pinnedRow.locator('.agent-age').boundingBox(), pinnedRow.locator('.agent-pin').boundingBox()]);
  assert.ok(age && !pin, 'a pinned row shows its age, not a pin button, when not hovered');
  await pinnedRow.hover();
  await expect(pinnedRow.locator('.agent-age')).toBeHidden();
  await expect(pinnedRow.locator('.agent-pin')).toHaveAttribute('aria-pressed', 'true');
  await shot(page, 'pinned-dark-1440');

  // The phone sees the desktop's pin.
  const offer = await fetch(`${f.url}/lazybox.bench.v1.RunnerService/BeginDevicePairing`, {method: 'POST', headers: {authorization: `Bearer ${f.token}`, 'content-type': 'application/json'}, body: JSON.stringify({grants: ['respond']})}).then(r => r.json());
  const phoneContext = await browser.newContext({...devices['iPhone 13']});
  const phone = await phoneContext.newPage(); await watch(phone);
  await phone.goto(offer.url);
  await phone.getByRole('textbox', {name: 'Name this phone'}).fill('iPhone');
  await phone.getByRole('button', {name: 'Pair this phone'}).click();
  await phone.getByRole('button', {name: 'Open inbox'}).click();
  // It still waits for an answer, so it stays with what needs the person, marked pinned (a pin never hides a request).
  const phoneRow = title => phone.locator('.remote-row').filter({hasText: title});
  await expect(phone.locator('.group-needs-input .remote-row[data-pinned]')).toContainText('Ship release notes');
  await expect(phoneRow('Ship release notes').locator('.remote-pin-mark')).toBeVisible();
  // A phone that may respond pins too, and the computer shows it.
  await phoneRow('Deploy preview').getByRole('button', {name: 'Actions for Deploy preview'}).click();
  await phone.getByRole('dialog', {name: 'Actions'}).getByRole('button', {name: 'Pin', exact: true}).click();
  await expect(phone.locator('.group-pinned .remote-row')).toContainText('Deploy preview');
  await expect(agents.locator('[data-group="pinned"] .agent-row')).toHaveText([/Ship release notes/, /Deploy preview/], {timeout: 10_000});
  await phone.screenshot({path: '.cache/agents-visible-phone-pinned.png'});
  // Pins this phone kept on itself before the Runner kept them move to the Runner, once.
  const deviceId = [...f.remote.devices.keys()][0];
  await phone.evaluate(id => localStorage.setItem(`remote:${id}:pins:v1`, JSON.stringify(['review-session'])), deviceId);
  await phone.reload();
  await expect.poll(() => Number(session('review-session').pinnedAtUnixMillis || 0), {timeout: 10_000}).toBeGreaterThan(0);
  await expect.poll(() => phone.evaluate(id => localStorage.getItem(`remote:${id}:pins:v1`), deviceId)).toBe('[]');
  await expect(phone.locator('.group-pinned .remote-row')).toHaveCount(2);
  await expect(phoneRow('Old exploration')).toHaveCount(1);

  // Open beside: that conversation in a chat group of its own, right of the conversation on screen
  // (a workspace's conversations are tabs, so it may move out of the chat group into its own).
  const chatGroups = page.locator('.dv-groupview').filter({has: page.locator('.dock-tab[data-kind="chat"]')});
  const groupsBefore = await chatGroups.count();
  await agents.locator('.agent-row', {hasText: 'Index the API'}).click({button: 'right'});
  await menu.getByRole('menuitem', {name: /^Open beside/}).click();
  await expect(chatGroups).toHaveCount(groupsBefore + 1);
  const beside = page.locator('.page-layout .panel-chat').filter({visible: true, hasText: 'Index the API'});
  await expect(beside).toHaveCount(1);
  await expect.poll(async () => { const xs = await page.locator('.page-layout .panel-chat').filter({visible: true}).evaluateAll(nodes => nodes.map(node => node.getBoundingClientRect().x)); const own = (await beside.boundingBox())?.x ?? -1; return xs.length > 1 && own === Math.max(...xs); }, {message: 'the conversation opened beside is the rightmost chat'}).toBe(true);
  await shot(page, 'beside-dark-1440');

  // Archive asks, then the conversation leaves both lists; Delete asks and can be cancelled.
  await lines.filter({hasText: 'Old exploration'}).locator('.conversation-link').click({button: 'right'});
  await menu.getByRole('menuitem', {name: /^Archive…/}).click();
  const archive = page.getByRole('dialog', {name: 'Archive conversation'});
  await expect(archive).toContainText('History keeps it');
  await archive.getByRole('button', {name: 'Archive', exact: true}).click();
  await expect(archive).toHaveCount(0);
  await expect(agents.locator('.agent-row', {hasText: 'Old exploration'})).toHaveCount(0);
  await expect(lines).toHaveCount(2);
  assert.deepEqual(f.pins.at(-1), {method: 'ArchiveConversation', body: {sessionId: 'old-session', archived: true}});
  // The phone stops listing it too.
  await expect(phoneRow('Old exploration')).toHaveCount(0, {timeout: 10_000});
  await phoneContext.close();
  await agents.locator('.agent-row', {hasText: 'Deploy preview'}).click({button: 'right'});
  await menu.getByRole('menuitem', {name: /^Delete$/}).click();
  await expect(agents.locator('.agent-row', {hasText:'Deploy preview'})).toHaveCount(0);
  await page.locator('.toast').getByRole('button',{name:'Undo',exact:true}).click();
  await expect(agents.locator('.agent-row', {hasText:'Deploy preview'})).toBeVisible();

  // The Inbox opens as the Agents panel, its first row focused; no popover over the work.
  await page.locator('.inbox-action').click();
  await expect(page.getByRole('dialog', {name: 'Inbox'})).toHaveCount(0);
  await expect(agents.locator('.agent-row').first()).toBeFocused();
  // ] ] i too.
  await page.locator('.workspace-brand').click();
  await page.keyboard.press(']'); await page.keyboard.press(']'); await page.keyboard.press('i');
  await expect(agents.locator('.agent-row').first()).toBeFocused();

  assert.equal(f.mutations.filter(m => ['Apply', 'OpenSession', 'CloseSession', 'DeleteConversation'].includes(m.method)).length, 0, 'nothing started, stopped, answered or deleted an agent');
  assert.deepEqual(errors, []);
  console.log('PASS agents visible: Workspaces above Agents in dark/light at 1440 and 1024, no scrolling tab strip, no pin over an age, a full-height background; every workspace lists its conversations with status and age, folds and remembers; Priority/By workspace, folded counts, unread dots, nested sub-agents, j/k/Space/Enter, filter as you type; one conversation menu in order with Delete last; rename, mark read, pin kept by the Runner and shown on the phone, open beside, archive confirmed and deletion undone; the Inbox opens the Agents panel. Fake Runner.');
} finally { await browser?.close(); await server.close(); await f.close(); }
