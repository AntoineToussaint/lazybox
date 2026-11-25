// Fixture only: New task (⌘N, R11). From an empty window, ⌘N, a repository, a
// message and Enter give one workspace on its own branch, one conversation in
// the chosen mode, and the message sent — in that order, once. The sheet
// remembers the repositories, agent and mode; ⌘Enter starts in the background;
// Escape keeps an unsent message; the palette and the empty states open it;
// "Start without a repository" makes an empty workspace; each repository's "from
// main ▾" chip (or ⌥B) picks the branch it starts from, which the Runner
// offers in Recent next time; ⌘⇧N still opens the
// quick New conversation in the workspace in front. Screenshots in
// .cache/new-sheet-*.png, dark and light. WebKit, the desktop app's engine.
import { testViteCache } from './fixtures/vite-cache.mjs';
import { createServer } from './fixtures/vite-cache.mjs';
import react from '@vitejs/plugin-react';
import { webkit, expect } from './fixtures/playwright.mjs';
import { fileURLToPath } from 'node:url';
import { mkdir } from 'node:fs/promises';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';
import { collectContextPageErrors, drainPageErrors } from './fixtures/page-errors.mjs';

const f = await startRunnerFixture();
f.completeGitHub();
// Repositories on this machine too, so the picker has a real list to search.
await fetch(`${f.url}/lazybox.bench.v1.RunnerService/ScanRepositoryRoot`, { method: 'POST', headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${f.token}` }, body: JSON.stringify({ path: '/Users/me/src' }) });
const server = await createServer({ ...testViteCache(import.meta.url), configFile: false, logLevel: 'error', root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: f.url } } } });
const PROMPT = 'Add a response cache to the API and benchmark it against main';
const since = method => f.mutations.filter(m => m.method === method);
let browser;
try {
  await server.listen(); await mkdir('.cache', { recursive: true });
  browser = await webkit.launch();
  const base = `http://127.0.0.1:${server.httpServer.address().port}/?view=workspace`;

  for (const scheme of ['dark', 'light']) {
    const context = await browser.newContext({ viewport: { width: 1440, height: 900 }, colorScheme: scheme });
    const errors = await collectContextPageErrors(context);
    await context.addInitScript(() => localStorage.setItem('lazybox:workspace.conversation-tabs', 'off'));
    const page = await context.newPage(); page.setDefaultTimeout(15000);
    await page.goto(`${base}#token=${f.token}`);

    // An empty window says how to start, with the key.
    const start = page.getByRole('region', { name: 'Start' });
    await expect(start.getByRole('heading', { name: 'Start something' })).toBeVisible();
    await expect(start.getByRole('button', { name: /New task/ })).toContainText('⌘N');
    if (scheme === 'dark') await page.screenshot({ path: '.cache/new-sheet-empty-state-dark.png' });

    // ⌘N: the sheet, the message box focused.
    await page.keyboard.press('Meta+n');
    const sheet = page.getByRole('dialog', { name: 'New task' });
    await expect(sheet).toBeVisible();
    const prompt = sheet.getByRole('textbox', { name: 'First message' });
    await expect(prompt).toBeFocused();

    if (scheme === 'dark') {
      // Enter without a repository: the sheet says what is missing and goes there.
      await prompt.fill(PROMPT);
      await page.keyboard.press('Enter');
      await expect(sheet.getByRole('alert')).toHaveText('Choose a repository, or start without one.');
      const search = sheet.getByRole('combobox', { name: 'Add a repository' });
      await expect(search).toBeFocused();
      const list = sheet.getByRole('listbox', { name: 'Repositories' });
      await expect(list).toBeVisible();
      // Search, Enter picks; a second one the same way.
      await search.fill('fixture-team/api');
      await expect(list.getByRole('option').first()).toHaveText('Start without a repository');
      await expect(list.getByRole('option', { selected: true })).toContainText('fixture-team/api');
      await page.keyboard.press('Enter');
      await expect(sheet.locator('.new-task-chip-name')).toHaveText(['fixture-team/api']);
      await search.fill('web');
      await expect(list.getByRole('option', { name: /fixture-team\/web/ })).toBeVisible();
      await page.keyboard.press('ArrowDown'); await page.keyboard.press('ArrowUp');
      await page.keyboard.press('Enter');
      await expect(sheet.locator('.new-task-chip')).toHaveCount(2);
      await expect(sheet.getByRole('alert')).toHaveCount(0);
      // The branch follows the message.
      await expect(sheet.getByTestId('new-task-branch')).toHaveText('feature/add-a-response-cache-to-the-api');
      await expect(sheet.locator('.new-task-name')).toHaveText('Add a response cache to the API');
      await search.fill('');
      await page.screenshot({ path: '.cache/new-sheet-repos-dark.png' });
      // Backspace in the empty search removes the last chip; picking again restores it.
      await page.keyboard.press('Backspace');
      await expect(sheet.locator('.new-task-chip')).toHaveCount(1);
      await search.fill('fixture-team/web'); await page.keyboard.press('Enter');
      await expect(sheet.locator('.new-task-chip-name')).toHaveText(['fixture-team/api', 'fixture-team/web']);
      // Start from: each repository says where its branch starts; neither is on the Runner yet,
      // so nothing is listed, and origin is not asked before a picker opens.
      const apiStart = sheet.locator('[data-start-from="github:fixture-team/api"]'), webStart = sheet.locator('[data-start-from="github:fixture-team/web"]');
      await expect(apiStart).toHaveText(/from\s*default branch/);
      assert.equal(f.branchListings.length, 0, 'origin is asked only when a picker opens');
      // Keyboard only: ⌥B from the search opens the first repository's picker; a typed branch is offered.
      await search.focus();
      await page.keyboard.press('Alt+KeyB');
      const picker = sheet.getByRole('dialog', { name: 'Start fixture-team/api from' });
      await expect(picker).toBeVisible();
      const branchSearch = picker.getByRole('combobox', { name: 'Branch of fixture-team/api to start from' });
      await expect(branchSearch).toBeFocused();
      await expect(picker).toContainText('listed once the repository is on this Runner');
      await page.keyboard.type('feat/v2-command-output');
      await expect(picker.getByRole('option', { name: /Start from feat\/v2-command-output/ })).toBeVisible();
      await page.keyboard.press('Enter');
      await expect(picker).toHaveCount(0);
      await expect(sheet).toBeVisible();
      await expect(apiStart).toHaveText(/from\s*feat\/v2-command-output/);
      await expect(apiStart).toBeFocused();
      // Escape closes a picker, not the sheet; focus goes back to its chip.
      await page.keyboard.press('Alt+KeyB');
      await expect(picker).toBeVisible();
      await page.keyboard.press('Escape');
      await expect(picker).toHaveCount(0);
      await expect(sheet).toBeVisible();
      await expect(apiStart).toBeFocused();
      // Edit the branch; the name stays.
      await sheet.getByRole('button', { name: 'Edit', exact: true }).click();
      await sheet.getByRole('textbox', { name: 'Branch' }).fill('feature/response-cache');
      await sheet.getByRole('button', { name: 'Use the suggestion' }).click();
      await expect(sheet.getByTestId('new-task-branch')).toHaveText('feature/add-a-response-cache-to-the-api');
      await sheet.getByRole('button', { name: 'Edit', exact: true }).click();
      await sheet.getByRole('textbox', { name: 'Branch' }).fill('feature/response-cache');
      // Mode: plan first.
      await sheet.getByRole('combobox', { name: 'Mode' }).selectOption('plan');
      await page.mouse.move(0, 0);
      await sheet.screenshot({ path: '.cache/new-sheet-dark.png' });
      await page.screenshot({ path: '.cache/new-sheet-page-dark.png' });

      // Enter in the message: the workspace, then the conversation, then the message — once each.
      await sheet.getByLabel('Personality').selectOption('hands-on-v2');
      const before = { prepare: since('PrepareWorkspace').length, open: since('OpenSession').length, apply: since('Apply').length };
      await prompt.focus(); await page.keyboard.press('Enter');
      await expect(sheet).toHaveCount(0);
      await expect.poll(() => since('Apply').length).toBe(before.apply + 1);
      assert.equal(since('PrepareWorkspace').length, before.prepare + 1);
      assert.equal(since('OpenSession').length, before.open + 1);
      const prepare = since('PrepareWorkspace').at(-1).body, open = since('OpenSession').at(-1).body, apply = since('Apply').at(-1).body;
      assert.equal(open.options.behaviorProfile, 'hands-on-v2');
      assert.equal(prepare.name, 'Add a response cache to the API');
      assert.deepEqual(prepare.repositoryIdentities, ['github:fixture-team/api', 'github:fixture-team/web']);
      assert.equal(prepare.branch, 'feature/response-cache');
      assert.deepEqual(prepare.startFrom, { 'github:fixture-team/api': 'feat/v2-command-output', 'github:fixture-team/web': '' }, 'web explicitly uses its default');
      assert.ok(prepare.requestKey);
      const created = f.collections.at(-1);
      assert.equal(open.collectionId, created.id);
      assert.equal(open.profileId, 'claude-default');
      assert.equal(open.options.permission, 'ask'); assert.equal(open.options.workflow, 'plan');
      const session = f.sessions.at(-1);
      assert.equal(apply.sessionId, session.id);
      assert.equal(apply.message.text, PROMPT);
      // The window shows the new conversation in its new workspace.
      await expect(page.getByRole('combobox', { name: 'Session', exact: true })).toHaveValue(session.id);
      await expect(page.getByRole('combobox', { name: 'Workspace', exact: true })).toHaveValue(created.id);
      const taskTab = page.locator('.dv-active-tab .dock-tab[data-kind="chat"]');
      await expect(taskTab).toBeVisible();
      await expect(taskTab).not.toHaveAttribute('data-preview');

      // ⌘N again: the last repositories and mode are remembered; the message is not.
      await page.keyboard.press('Meta+n');
      await expect(sheet).toBeVisible();
      await expect(sheet.locator('.new-task-chip-name')).toHaveText(['fixture-team/api', 'fixture-team/web']);
      await expect(sheet.getByRole('combobox', { name: 'Mode' })).toHaveValue('plan');
      await expect(prompt).toHaveValue('');
      // The previously chosen branch remains available, while the new sheet defaults to main.
      await expect(apiStart).toHaveText(/from\s*main/);
      await expect(apiStart).toHaveAttribute('data-default');
      await expect(webStart).toHaveText(/from\s*main/);
      await expect(webStart).toHaveAttribute('data-default');
      assert.equal(f.branchListings.length, 0, 'still not asked: no picker opened');
      // Pick a branch from the chip: origin's branches, its default first, then recent, active, all.
      await webStart.click();
      const webPicker = sheet.getByRole('dialog', { name: 'Start fixture-team/web from' });
      await expect(webPicker).toBeVisible();
      await expect.poll(() => f.branchListings.filter(id => id === 'github:fixture-team/web').length).toBe(1);
      await expect(webPicker.getByRole('option').first()).toContainText('main');
      await expect(webPicker.getByRole('option').first()).toContainText('default');
      await expect(webPicker.locator('.start-from-section')).toHaveText(['Active', 'All branches']);
      await expect(webPicker.getByRole('option').nth(1)).toHaveText('feat/v2-command-output');
      await page.mouse.move(0, 0);
      await sheet.screenshot({ path: '.cache/new-sheet-start-from-dark.png' });
      await webPicker.getByRole('combobox').fill('rel');
      await expect(webPicker.getByRole('option').first()).toHaveText('release/1.4');
      await webPicker.getByRole('option', { name: 'release/1.4' }).click();
      await expect(webPicker).toHaveCount(0);
      await expect(webStart).toHaveText(/from\s*release\/1\.4/);
      // Reopened within the minute: origin is not asked again.
      await webStart.click();
      await expect(webPicker).toBeVisible();
      await page.keyboard.press('Escape');
      assert.equal(f.branchListings.filter(id => id === 'github:fixture-team/web').length, 1, 'origin\'s list is reused for a minute');
      await expect(webStart).toBeFocused();
      // Keyboard only, back to the default: from the message, ⌥B (api, the first), ↑/↓ to main, Enter.
      await prompt.focus();
      await page.keyboard.press('Alt+KeyB');
      await expect(picker).toBeVisible();
      await expect(picker.locator('.start-from-section')).toHaveText(['Recent', 'Active', 'All branches']);
      await expect(picker.getByRole('option').nth(1)).toHaveText(/feat\/v2-command-output/);
      await page.keyboard.press('ArrowDown'); await page.keyboard.press('ArrowUp');
      await expect(picker.getByRole('option').first()).toHaveAttribute('aria-selected', 'true');
      await page.keyboard.press('Enter');
      await expect(apiStart).toHaveText(/from\s*main/);
      await expect(apiStart).toHaveAttribute('data-default');
      // Escape keeps an unsent message for next time.
      await prompt.fill('Write the migration notes');
      await page.keyboard.press('Escape');
      await expect(sheet).toHaveCount(0);
      await page.keyboard.press('Meta+n');
      await expect(prompt).toHaveValue('Write the migration notes');

      // ⌘Enter: started in the background; the window stays where it was.
      const shown = await page.getByRole('combobox', { name: 'Session', exact: true }).inputValue();
      const applied = since('Apply').length;
      await page.keyboard.press('Meta+Enter');
      await expect(sheet).toHaveCount(0);
      await expect(page.getByText('Started “Write the migration notes”')).toBeVisible();
      await expect.poll(() => since('Apply').length).toBe(applied + 1);
      assert.equal(since('Apply').at(-1).body.message.text, 'Write the migration notes');
      assert.notEqual(since('PrepareWorkspace').at(-1).body.requestKey, prepare.requestKey, 'a second task is a second workspace');
      await expect(page.getByRole('combobox', { name: 'Session', exact: true })).toHaveValue(shown);
      // Its toast opens it.
      await page.getByRole('button', { name: 'Open', exact: true }).last().click();
      await expect(page.getByRole('combobox', { name: 'Session', exact: true })).toHaveValue(f.sessions.at(-1).id);

      // The palette opens the sheet too.
      await page.keyboard.press('Meta+k');
      await page.keyboard.type('New task');
      await expect(page.getByRole('option', { name: /New task/ }).first()).toBeVisible();
      await page.keyboard.press('Enter');
      await expect(sheet).toBeVisible();

      // Without a repository: an empty workspace, no branch.
      await search.focus();
      await page.keyboard.press('Backspace'); await page.keyboard.press('Backspace');
      await expect(sheet.locator('.new-task-chip')).toHaveCount(0);
      await sheet.getByRole('option', { name: 'Start without a repository' }).click();
      await expect(sheet.locator('.new-task-chip.is-empty')).toHaveText('No repository');
      await expect(sheet.getByTestId('new-task-branch')).toHaveCount(0);
      await prompt.fill('Sketch a plan for the cache');
      await sheet.getByRole('button', { name: /^(Start|Create)\b(?! in)/ }).click();
      await expect(sheet).toHaveCount(0);
      await expect.poll(() => since('PrepareWorkspace').at(-1).body.name).toBe('Sketch a plan for the cache');
      assert.deepEqual(since('PrepareWorkspace').at(-1).body.repositoryIdentities || [], []);
      assert.equal(since('PrepareWorkspace').at(-1).body.branch || '', '');

      // ⌘⇧N is still the quick New conversation in the workspace in front.
      await page.keyboard.press('Meta+Shift+n');
      const quick = page.getByRole('dialog', { name: 'New conversation', exact: true });
      await expect(quick).toBeVisible();
      await page.keyboard.press('Escape');
      await expect(quick).toBeHidden();
    } else {
      // Light: the sheet as a person first sees it, remembered repositories and all.
      await prompt.fill(PROMPT);
      await page.mouse.move(0, 0);
      await sheet.screenshot({ path: '.cache/new-sheet-light.png' });
      await sheet.getByRole('combobox', { name: 'Add a repository' }).focus();
      await page.screenshot({ path: '.cache/new-sheet-repos-light.png' });
      await page.keyboard.press('Escape');
      await page.keyboard.press('Escape');
      await expect(sheet).toHaveCount(0);
    }

    // At 1024 the sheet fits, nothing cut.
    await page.setViewportSize({ width: 1024, height: 680 });
    await page.keyboard.press('Meta+n');
    await expect(sheet).toBeVisible();
    const box = await sheet.boundingBox();
    assert.ok(box.x >= 0 && box.y >= 0 && box.x + box.width <= 1024 && box.y + box.height <= 680, `the sheet fits at 1024×680: ${JSON.stringify(box)}`);
    await page.mouse.move(0, 0);
    await page.screenshot({ path: `.cache/new-sheet-1024-${scheme}.png` });
    await page.keyboard.press('Escape');
    await drainPageErrors(errors);
    assert.deepEqual(errors, [], `no page errors (${scheme})`);
    await context.close();
  }
  // Use the default kept-tab setting. Creating a workspace on its own and
  // starting its first chat must consume the workspace's starter tab.
  const context = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  const starterErrors = await collectContextPageErrors(context);
  const page = await context.newPage();
  const source = f.collections.find(workspace => workspace.name === 'Add a response cache to the API');
  source.members[0].spec.baseRef = 'refs/remotes/origin/feat/v2-command-output';
  await page.goto(`${base}&collection=${source.id}#token=${f.token}`);
  const row = page.locator('.workspace-row').filter({ hasText: source.name });
  await row.locator('.workspace-link').click({ button: 'right' });
  await page.getByRole('menuitem', { name: 'Clone workspace…' }).click();
  const workspace = page.getByRole('dialog', { name: 'Clone workspace', exact: true });
  await expect(workspace.getByText('feat/v2-command-output', { exact: true })).toBeVisible();
  const workspaceName = workspace.getByRole('textbox', { name: 'Workspace name' });
  await expect(workspaceName).toBeFocused();
  await workspaceName.fill('Cache follow-up');
  await workspaceName.press('Enter');
  await expect(workspace).toBeHidden();
  const cloned = f.collections.at(-1);
  assert.equal(cloned.name, 'Cache follow-up');
  await expect.poll(() => since('OpenSession').filter(m => m.body.collectionId === cloned.id).length).toBe(1);
  await expect(page.getByRole('dialog', { name: 'New conversation', exact: true })).toBeHidden();
  const preparation = since('PrepareWorkspace').at(-1).body;
  assert.equal(preparation.branch, 'feature/cache-follow-up');
  assert.deepEqual(preparation.repositoryIdentities, ['github:fixture-team/api', 'github:fixture-team/web']);
  assert.equal(preparation.startFrom['github:fixture-team/api'], 'feat/v2-command-output');

  await page.keyboard.press('Meta+Shift+n');
  const conversation = page.getByRole('dialog', { name: 'New conversation', exact: true });
  await conversation.getByRole('combobox', { name: 'Agent', exact: true }).click();
  await conversation.getByRole('option', { name: /^Codex/ }).click();
  await conversation.getByText('Model & context', { exact: true }).click();
  await conversation.getByRole('textbox', { name: 'Model', exact: true }).fill('gpt-6.2-sol');
  await conversation.getByRole('combobox', { name: 'Reasoning effort', exact: true }).selectOption('high');
  await conversation.getByRole('combobox', { name: 'Permissions', exact: true }).selectOption('read-only');
  await conversation.getByRole('combobox', { name: 'Workflow', exact: true }).selectOption('plan');
  await conversation.getByRole('button', { name: 'Start Codex ↗' }).click();
  await expect(conversation).toBeHidden();
  const tabs = page.locator('.dock-tab[data-kind="chat"]');
  await expect(tabs).toHaveCount(2);
  const original = f.sessions.at(-1);
  assert.equal(original.collectionId, cloned.id);
  assert.equal(original.effort, 'high');

  // Choices carry across workspace boundaries and between both start forms.
  const sourceRow = page.locator('.workspace-row').filter({ hasText: source.name });
  await sourceRow.locator('.workspace-link').click();
  await page.keyboard.press('Meta+Shift+n');
  await expect(conversation.getByRole('button', { name: 'Start Codex ↗' })).toBeEnabled();
  await expect(conversation.getByText(/Codex.*gpt-6.2-sol/)).toBeVisible();
  await page.keyboard.press('Escape');
  await page.keyboard.press('Meta+n');
  const newTask = page.getByRole('dialog', { name: 'New task' });
  await expect(newTask.getByRole('textbox', { name: 'Model', exact: true })).toHaveValue('gpt-6.2-sol');
  await expect(newTask.getByRole('combobox', { name: 'Reasoning effort', exact: true })).toHaveValue('high');
  await expect(newTask.locator('.new-task-chip-name')).toHaveText(['fixture-team/api', 'fixture-team/web']);
  await page.keyboard.press('Escape');

  await page.locator('.workspace-row').filter({ hasText: cloned.name }).locator('.workspace-link').click();
  await expect(tabs).toHaveCount(2);
  await tabs.last().click({ button: 'right' });
  await page.getByRole('menuitem', { name: 'Clone conversation…' }).click();
  const copy = page.getByRole('dialog', { name: 'Clone conversation', exact: true });
  await expect(copy.getByRole('textbox', { name: 'Model', exact: true })).toHaveValue('gpt-6.2-sol');
  const name = copy.getByRole('textbox', { name: 'Conversation name' });
  await expect(name).toBeFocused();
  await name.fill('Second approach');
  await copy.getByRole('button', { name: 'Start Codex ↗' }).click();
  await expect(copy).toBeHidden();
  await expect(tabs).toHaveCount(3);
  await expect(tabs.filter({ hasText: 'Second approach' })).toBeVisible();
  const started = since('OpenSession').at(-1).body;
  assert.equal(started.collectionId, cloned.id);
  assert.equal(started.options.model, 'gpt-6.2-sol');
  assert.equal(started.options.effort, 'high');
  assert.equal(started.options.workflow, 'execute');
  assert.equal(started.options.permission || '', '');
  assert.ok(!started.resumeReference);
  await drainPageErrors(starterErrors);
  assert.deepEqual(starterErrors, [], 'no page errors (starter tab)');
  await context.close();

  console.log('PASS: ⌘N sheet — workspace, conversation and message in one step, remembered choices, start-from branch (chip, ⌥B, default reset), background start, kept draft, palette and empty state, empty workspace, ⌘⇧N unchanged. Fixture only.');
} finally { await browser?.close(); await server.close(); await f.close(); }
