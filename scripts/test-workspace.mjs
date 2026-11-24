import {testViteCache} from './fixtures/vite-cache.mjs';
import { createServer } from 'vite';
import react from '@vitejs/plugin-react';
import { chromium, expect } from './fixtures/playwright.mjs';
import { fileURLToPath } from 'node:url';
import { mkdir } from 'node:fs/promises';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';
import { collectContextPageErrors } from './fixtures/page-errors.mjs';

const fixture = await startRunnerFixture();
const internalRepoID='github-'+'9'.repeat(64);
Object.assign(fixture.library[0],{id:internalRepoID,name:'api',gitIdentity:'github:fixture-team/api'});
for(const workspace of fixture.collections)workspace.members[0].spec.repositoryId=internalRepoID;
fixture.sessions.push({...structuredClone(fixture.sessions[0]),id:'alpha-second',title:'Second alpha conversation'});
const server = await createServer({...testViteCache(import.meta.url), configFile: false, root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: fixture.url } } } });
let browser;
try {
  await server.listen();
  const url = `http://127.0.0.1:${server.httpServer.address().port}/?view=workspace#token=${fixture.token}`;
  browser = await chromium.launch({ headless: true });
  const context = await browser.newContext({ viewport: { width: 1500, height: 1100 } });
  // One chat that shows the conversation chosen (the preview tab): "Open a workspace's conversations as tabs" off; test-conversation-tabs covers it on.
  await context.addInitScript(() => { try { localStorage.setItem('lazybox:workspace.conversation-tabs', 'off'); } catch { /* a popup without storage */ } });
  // Every page of the context: this one, the chat windows it pops out and the page that replaces it.
  const errors = await collectContextPageErrors(context);
  let page = await context.newPage();
  await page.goto(url);
  await expect(page.locator('.workspace-row').filter({hasText:'Alpha feature'}).locator('.workspace-link')).toBeVisible();
  const collection = page.getByRole('combobox', { name: 'Workspace', exact: true });
  const session = page.getByRole('combobox', { name: 'Session', exact: true });
  await collection.selectOption('alpha'); await session.selectOption('alpha-session');
  let chat = page.locator('[data-panel-id="alpha-work-chat"]');
  await expect(chat).toContainText('Retained conversation for alpha-session');
  await expect(page.locator('.workspace-row').filter({hasText:'Alpha feature'})).toContainText('fixture-team/api');
  await expect(page.locator('.workspace-row').filter({hasText:'Alpha feature'})).not.toContainText(internalRepoID);
  await expect(chat.locator('.chat-status')).toContainText('Running');
  // A sub-agent is a card of its own, named as the Runner names it; its prose stays inside it.
  await expect(chat.locator('.subagent-card[data-agent-id="child-review"] .subagent-name')).toHaveText('Review');
  await expect(chat.locator('.messages:visible .message').filter({ hasText: 'CHILD_PRIVATE_TRANSCRIPT' })).toHaveCount(0);
  const initialBounds = await chat.boundingBox();
  assert.ok(initialBounds.height > 800, 'Collapsed output must give the remaining workspace height to chat');
  await mkdir('.cache', { recursive: true }); await page.screenshot({ path: '.cache/workspace-refined-default.png', fullPage: true });
  await page.getByRole('tab', { name: 'Command output', exact: true }).click();
  await expect(page.getByTestId('console-output')).toContainText('fixture retained output');
  await page.getByRole('button',{name:'2 repositories',exact:true}).click();
  await page.getByText('Workspace location & source details', { exact: true }).click();
  await expect(page.locator('.workspace-repositories-dialog .source-details p').first()).toContainText(fixture.collections.find(c=>c.id==='alpha').root);
  await page.getByText('Workspace location & source details', { exact: true }).click();
  await page.getByRole('button',{name:'Close workspace repositories',exact:true}).click();
  assert.equal(fixture.mutations.length, 0, 'viewing a Collection must not mutate execution');
  await chat.getByRole('textbox', { name: 'Message', exact: true }).fill('unsent cross-repository draft');
  const infoCalls = fixture.calls.filter(value => value.method === 'Info').length;
  const journalCalls = fixture.calls.filter(value => value.method === 'WatchEvents').length;
  await expect.poll(() => fixture.calls.filter(value => value.method === 'Info').length).toBeGreaterThan(infoCalls);
  await expect(chat.getByRole('textbox', { name: 'Message', exact: true })).toHaveValue('unsent cross-repository draft');
  assert.equal(fixture.calls.filter(value => value.method === 'WatchEvents').length, journalCalls, 'catalog refresh must not restart a chat subscription');

  // Panels resize by dragging the divider or with arrow keys.
  const divider = page.getByRole('separator', { name: 'Resize Workspaces sidebar', exact: true });
  const before = Number(await divider.getAttribute('aria-valuenow')), grip = await divider.boundingBox();
  await page.mouse.move(grip.x + grip.width / 2, grip.y + grip.height / 2); await page.mouse.down();
  await page.mouse.move(grip.x + 200, grip.y + grip.height / 2, { steps: 5 }); await page.mouse.up();
  await expect.poll(async () => Number(await divider.getAttribute('aria-valuenow'))).toBeGreaterThan(before);
  const dragged = Number(await divider.getAttribute('aria-valuenow'));
  await divider.focus(); await page.keyboard.press('ArrowLeft');
  await expect(divider).toHaveAttribute('aria-valuenow', String(dragged - 16)); // the Workspaces sidebar moves in 16px steps
  // The session menu closes when clicking outside it.
  await chat.getByLabel('Session actions', { exact: true }).click();
  await expect(chat.getByRole('button', { name: 'End session', exact: true })).toBeVisible();
  await page.locator('.workspace-brand').click();
  await expect(chat.getByRole('button', { name: 'End session', exact: true })).toBeHidden();
  // A running session's permission mode changes from the same menu.
  await chat.getByLabel('Session actions', { exact: true }).click();
  const permissions = chat.getByRole('group', { name: 'Permissions', exact: true });
  await expect(permissions.getByRole('button', { name: 'Ask for approval', exact: true })).toHaveAttribute('aria-pressed', 'true');
  await permissions.getByRole('button', { name: 'Full access', exact: true }).click();
  await expect.poll(() => fixture.mutations.find(m => m.method === 'Apply' && m.body.permission)?.body.permission.mode).toBe('yolo');
  await chat.getByLabel('Session actions', { exact: true }).click();
  await expect(permissions.getByRole('button', { name: 'Full access', exact: true })).toHaveAttribute('aria-pressed', 'true');
  await page.locator('.workspace-brand').click();
  // Later assertions count execution starts; this deliberate change is not one.
  fixture.mutations.splice(fixture.mutations.findIndex(m => m.method === 'Apply' && m.body.permission), 1);
  await expect(chat.getByRole('status').filter({ hasText: 'Instructions updated' })).toBeVisible();

  // Typing in the chat kept it as a tab of its own: it stays on its conversation, and choosing
  // another shows that one in a preview tab beside it. From its menu (right-click its tab) a
  // regular tab becomes the preview tab, and the preview tab is kept open.
  const chatTab = title => page.locator('.dv-tab').filter({ has: page.locator('.dock-tab[data-kind="chat"]', { hasText: title }) });
  const previewChat = page.locator('.page-layout .panel-chat').filter({ visible: true });
  await chatTab('Agent chat').click({ button: 'right' });
  await expect(page.getByRole('menuitem', { name: 'Use as preview tab', exact: true })).toBeVisible();
  await page.keyboard.press('Escape');
  await session.selectOption('alpha-second');
  await expect(chat).toContainText('Retained conversation for alpha-session');
  await expect(page.locator('.page-layout .panel-chat')).toHaveCount(2);
  await expect(previewChat).toContainText('Retained conversation for alpha-second');
  await expect(page.locator('.dock-tab[data-kind="chat"][data-preview]')).toContainText('Second alpha conversation');
  await chatTab('Agent chat').click({ button: 'right' });
  await page.getByRole('menuitem', { name: 'Use as preview tab', exact: true }).click();
  await expect(page.locator('.dock-tab[data-kind="chat"][data-preview]'), 'one preview tab: the alpha chat').toHaveText('Agent chat');
  await chatTab('Second alpha conversation').click({ button: 'right' });
  await expect(page.getByRole('menuitem', { name: 'Use as preview tab', exact: true })).toBeVisible();
  await page.keyboard.press('Escape');
  await chatTab('Agent chat').click({ button: 'right' });
  await expect(page.getByRole('menuitem', { name: 'Keep open', exact: true })).toBeVisible();
  await page.keyboard.press('Escape');
  // Closing the second conversation's tab leaves the page with one chat again; the conversation goes on.
  await chatTab('Second alpha conversation').locator('.dock-tab-close').click();
  await expect(page.locator('.page-layout .panel-chat')).toHaveCount(1);
  await collection.selectOption('alpha'); await session.selectOption('alpha-session');
  await expect(chat.getByRole('textbox', { name: 'Message', exact: true })).toHaveValue('unsent cross-repository draft');
  await page.getByLabel('Page actions',{exact:true}).click();await page.getByRole('button', { name: 'Customize pages', exact: true }).click();
  await page.getByRole('textbox', { name: 'Page name', exact: true }).fill('Cross repo');
  await page.getByRole('button', { name: 'Rename page', exact: true }).click();
  // Moving the chat to the other side keeps it mounted, draft and all.
  await page.getByRole('tab', { name: 'Agent chat', exact: true }).click({ button: 'right' });
  await page.getByRole('menuitem', { name: 'Dock left', exact: true }).click();
  chat = page.locator('[data-panel-id="alpha-work-chat"]');
  await expect(chat.getByRole('textbox', { name: 'Message', exact: true })).toHaveValue('unsent cross-repository draft');
  await page.getByLabel('Page actions',{exact:true}).click();await page.getByRole('button', { name: 'Finish customizing', exact: true }).click();
  await page.getByRole('textbox', { name: 'Search output', exact: true }).fill('fixture');
  // Scrolling up stops following the output; a button brings you back to the latest.
  await page.locator('[data-testid=console-output]').first().evaluate(node => { node.scrollTop = 0; node.dispatchEvent(new Event('scroll')); });
  await mkdir('.cache', { recursive: true }); await page.screenshot({ path: '.cache/workspace-work.png', fullPage: true });
  await expect(page.getByRole('button',{name:'Monitor',exact:true})).toHaveCount(0);
  await collection.selectOption('beta'); await session.selectOption('beta-session');
  await page.getByRole('tab', { name: 'Command output', exact: true }).click();
  await expect(page.getByRole('textbox', { name: 'Search output', exact: true })).toHaveValue('');
  await expect(page.getByRole('button', { name: '↓ Latest output', exact: true })).toHaveCount(0);
  await expect(page.locator('.collection-panel')).not.toContainText('No standalone conversations yet.');
  await page.screenshot({ path: '.cache/workspace-secondary.png', fullPage: true });
  await page.reload();
  await expect(collection).toHaveValue('beta'); await expect(session).toHaveValue('beta-session');
  await expect(page.getByRole('button', { name: 'Cross repo', exact: true })).toHaveCount(0);
  await collection.selectOption('alpha');
  await page.getByRole('button', { name: 'Cross repo', exact: true }).click();
  await expect(collection).toHaveValue('alpha'); await expect(session).toHaveValue('alpha-session');
  await expect(chat.getByRole('textbox', { name: 'Message', exact: true })).toHaveValue('unsent cross-repository draft');
  await expect(page.getByRole('textbox', { name: 'Search output', exact: true })).toHaveValue('fixture');

  let popupPromise = page.waitForEvent('popup');
  await chat.getByRole('button', { name: 'Open chat window ↗', exact: true }).click();
  let popup = await popupPromise;
  // The only content panel stays as a card, so the workspace list does not fill the page.
  await expect(chat.getByRole('heading', { name: 'Chat is in its own window', exact: true })).toBeVisible();
  await expect(chat.getByRole('textbox', { name: 'Message', exact: true })).toHaveCount(0);
  await expect(page.getByRole('region', { name: 'In other windows' })).toBeVisible();
  await expect(popup.getByRole('textbox', { name: 'Message', exact: true })).toHaveValue('unsent cross-repository draft');
  await popup.getByRole('textbox', { name: 'Message', exact: true }).fill('draft from detached chat');
  let closed = popup.waitForEvent('close');
  await page.getByRole('region', { name: 'In other windows' }).getByRole('button', { name: /^Put .* back$/ }).click(); await closed;
  await expect(chat.getByRole('textbox', { name: 'Message', exact: true })).toHaveValue('draft from detached chat');
  popupPromise = page.waitForEvent('popup');
  await chat.getByRole('button', { name: 'Open chat window ↗', exact: true }).click(); popup = await popupPromise;
  await expect(popup.getByRole('textbox', { name: 'Message', exact: true })).toHaveValue('draft from detached chat');
  await page.close();
  await expect(popup.getByRole('textbox', { name: 'Message', exact: true })).toBeEnabled();
  page = await context.newPage(); await page.goto(url); chat = page.locator('[data-panel-id="alpha-work-chat"]');
  // The only content panel stays as a card, so the workspace list does not fill the page.
  await expect(chat.getByRole('heading', { name: 'Chat is in its own window', exact: true })).toBeVisible();
  await expect(chat.getByRole('textbox', { name: 'Message', exact: true })).toHaveCount(0);
  await expect(page.getByRole('region', { name: 'In other windows' })).toBeVisible();
  await page.getByRole('region', { name: 'In other windows' }).getByRole('button', { name: /^Show / }).click();
  await expect(popup.getByRole('textbox', { name: 'Message', exact: true })).toHaveValue('draft from detached chat');
  closed = popup.waitForEvent('close'); await page.getByRole('region', { name: 'In other windows' }).getByRole('button', { name: /^Put .* back$/ }).click(); await closed;
  await expect(chat.getByRole('textbox', { name: 'Message', exact: true })).toHaveValue('draft from detached chat');
  // The chat brought back stays on its conversation (a regular tab); choosing conversations in the
  // sidebar brings its tab forward, shows the others in one preview tab beside it, and never
  // stacks a second panel on the same conversation.
  const links = page.locator('.page-layout .panel-collections .conversation-link');
  const second = links.filter({ hasText: 'Second alpha conversation' }), first = links.filter({ hasNotText: 'Second alpha conversation' }).first();
  for (const [link, count] of [[first, 1], [first, 1], [second, 2], [second, 2], [first, 2], [first, 2]]) {
    await link.click();
    await expect(page.locator('.page-layout .panel-chat')).toHaveCount(count);
  }
  await expect(chat).toContainText('Retained conversation for alpha-session');
  await expect(chat.getByRole('textbox', { name: 'Message', exact: true })).toHaveValue('draft from detached chat');
  assert.equal(fixture.mutations.length, 0, 'layout, restore, detach, or dock created/stopped an execution');

  {await page.getByLabel('Page actions',{exact:true}).click();await page.getByRole('button', { name: 'Layout configuration', exact: true }).click();}
  const exported = JSON.parse(await page.getByRole('textbox', { name: 'Page configuration', exact: true }).inputValue());
  assert.equal(exported.pages[0].name, 'Cross repo');
  assert.equal(exported.version, 2);
  assert.equal(exported.pages[0].layout.type, 'split');
  assert.equal(JSON.stringify(exported).includes(fixture.token), false);
  assert.equal(JSON.stringify(exported).includes('permission'), false);
  const eachPanel = (node, change) => { if (node.type === 'group') node.panels.forEach(change); else node.children.forEach(child => eachPanel(child, change)); };
  eachPanel(exported.pages[0].layout, panel => { if (panel.kind === 'console') panel.kind = 'constructor'; });
  await page.getByRole('textbox', { name: 'Page configuration', exact: true }).fill(JSON.stringify(exported));
  await page.getByRole('button', { name: 'Import layout', exact: true }).click();
  await expect(page.getByText('The “constructor” module is unavailable. Its configuration has been retained.')).toBeVisible();
  eachPanel(exported.pages[0].layout, panel => { if (panel.kind === 'chat') panel.kind = 'assistant'; });
  if (!await page.getByRole('textbox', { name: 'Page configuration', exact: true }).isVisible()) {await page.getByLabel('Page actions',{exact:true}).click();await page.getByRole('button', { name: 'Layout configuration', exact: true }).click();}
  await page.getByRole('textbox', { name: 'Page configuration', exact: true }).fill(JSON.stringify(exported));
  await page.getByRole('button', { name: 'Import layout', exact: true }).click();
  const assistant = page.locator('[data-panel-id="alpha-work-chat"]');
  // Ask Lazybox opens on Help (test-help.mjs); its menu leads to a planning conversation.
  await expect(assistant.getByRole('textbox', { name: 'Ask Lazybox', exact: true })).toBeVisible();
  await expect(assistant.getByLabel('Working directory', { exact: true })).toHaveCount(0);
  await assistant.locator('summary[aria-label="Choose the agent for Ask Lazybox"]').click();
  await page.getByRole('button', { name: 'Discuss a plan instead', exact: true }).click();
  await expect(assistant.getByRole('combobox', { name: 'Assistant purpose' })).toHaveValue('planning');
  assert.equal(fixture.mutations.length, 0, 'adding an assistant module started inference');
  await assistant.getByRole('button', { name: 'Start Claude ↗', exact: true }).click();
  await expect(assistant.getByRole('textbox', { name: 'Message', exact: true })).toBeEnabled();
  assert.equal(fixture.mutations.length, 1);
  assert.equal(fixture.mutations[0].method, 'OpenAssistant'); assert.equal(fixture.mutations[0].body.purpose, 'planning');
  assert.equal(fixture.mutations[0].body.collectionId,'alpha','workspace assistant must retain accounting attribution');
  assert.ok(!fixture.mutations[0].body.cwd,'client must not choose an assistant filesystem path');
  await page.reload();
  await expect(page.locator('[data-panel-id="alpha-work-chat"]').getByRole('textbox', { name: 'Message', exact: true })).toBeEnabled();
  assert.equal(fixture.mutations.length, 1, 'restoring assistant started another provider');
  fixture.sessions.push(...['a', 'b', 'c'].map(id => ({ ...fixture.sessions[0], agents: [], busy: false, id: `plain-${id}`, collectionId: '', title: `Conversation ${id}` })));
  const olderContext = await browser.newContext();
  await olderContext.addInitScript(() => { try { localStorage.setItem('lazybox:workspace.conversation-tabs', 'off'); } catch { /* a popup without storage */ } });
  const olderPage = await olderContext.newPage();
  await olderPage.clock.install();
  await olderPage.clock.pauseAt(new Date());
  let olderInfoCalls = 0;
  await olderPage.route('**/Info', async route => {
    const response = await route.fetch(); const body = await response.json();
    if (++olderInfoCalls % 2) body.sessions.reverse();
    body.sessions.find(session => session.id === 'plain-a').status = olderInfoCalls % 2 ? 'ready' : 'done';
    await route.fulfill({ response, json: body });
  });
  await olderPage.route('**/ListHistory', route => route.fulfill({ status: 501, json: { code: 'unimplemented', message: 'Older Runner' } }));
  // The clock is paused to control polling; the dock draws panels on animation frames, so let a few run.
  const frames = () => olderPage.clock.runFor(64);
  await olderPage.goto(url); await frames();
  await expect(olderPage.getByText('Live conversations · saved history requires a Runner update.')).toBeVisible();
  await olderPage.getByRole('combobox', { name: 'Workspace', exact: true }).selectOption('alpha'); await frames();
  await expect(olderPage.locator('.workspace-row').filter({hasText:'Alpha feature'})).toBeVisible();
  await olderPage.getByRole('combobox', { name: 'Session', exact: true }).selectOption('alpha-session'); await frames();
  await expect(olderPage.locator('[data-panel-id="alpha-work-chat"]').getByRole('textbox', { name: 'Message', exact: true })).toBeEnabled();
  const sidebar = olderPage.getByRole('navigation', { name: 'Conversations', exact: true });
  const beforeSelection = await sidebar.locator('strong').allTextContents();
  assert.deepEqual(beforeSelection.filter(title => title.startsWith('Conversation ')), ['Conversation a', 'Conversation b', 'Conversation c']);
  const countBeforeSelection = olderInfoCalls;
  await sidebar.getByRole('button', { name: /Conversation b/ }).click(); await frames();
  await expect(olderPage.locator('[data-panel-id="work-chat"]')).toContainText('Retained conversation for plain-b');
  assert.equal(olderInfoCalls, countBeforeSelection, 'Selecting a row must not refresh or clear the Runner catalog');
  assert.deepEqual(await sidebar.locator('strong').allTextContents(), beforeSelection);
  await olderPage.clock.fastForward(3100);
  await expect.poll(() => olderInfoCalls).toBeGreaterThan(countBeforeSelection);
  await expect(sidebar.getByRole('button', { name: /Conversation a/ })).toContainText(olderInfoCalls % 2 ? 'Ready' : 'Turn done');
  await expect(sidebar.locator('strong')).toHaveText(beforeSelection);
  await olderContext.close();
  assert.equal(errors.length, 0, errors.join('\n'));
  console.log('PASS: real shared panels in two saved compositions; independent targets/drafts/console state; pin/follow, reorder, resize, restore, detach/dock, close-host continuity, unknown module preservation; zero implicit execution mutations; explicit workspace-attributed assistant startup and retained session after reload. Explicitly fake RPC fixture; no Git/model/account access.');
} finally {
  await browser?.close(); await server.close(); await fixture.close();
}
