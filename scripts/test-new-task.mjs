// Fixture only: New task (⌘N, R11). From an empty window, ⌘N, a repository, a
// message and Enter give one workspace on its own branch, one conversation in
// the chosen mode, and the message sent — in that order, once. The sheet
// remembers the repositories, agent and mode; ⌘Enter starts in the background;
// Escape keeps an unsent message; the palette and the empty states open it;
// "Start without a repository" makes an empty workspace; ⌘⇧N still opens the
// quick New conversation in the workspace in front. Screenshots in
// .cache/new-sheet-*.png, dark and light. WebKit, the desktop app's engine.
import { testViteCache } from './fixtures/vite-cache.mjs';
import { createServer } from 'vite';
import react from '@vitejs/plugin-react';
import { webkit, expect } from './fixtures/playwright.mjs';
import { fileURLToPath } from 'node:url';
import { mkdir } from 'node:fs/promises';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';

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
    const page = await context.newPage(); page.setDefaultTimeout(15000);
    const errors = []; page.on('pageerror', e => errors.push(String(e)));
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
      await expect(list.getByRole('option').first()).toContainText('fixture-team/api');
      await page.keyboard.press('Enter');
      await expect(sheet.locator('.new-task-chip')).toHaveText(['fixture-team/api']);
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
      await expect(sheet.locator('.new-task-chip')).toHaveText(['fixture-team/api', 'fixture-team/web']);
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
      const before = { prepare: since('PrepareWorkspace').length, open: since('OpenSession').length, apply: since('Apply').length };
      await prompt.focus(); await page.keyboard.press('Enter');
      await expect(sheet).toHaveCount(0);
      await expect.poll(() => since('Apply').length).toBe(before.apply + 1);
      assert.equal(since('PrepareWorkspace').length, before.prepare + 1);
      assert.equal(since('OpenSession').length, before.open + 1);
      const prepare = since('PrepareWorkspace').at(-1).body, open = since('OpenSession').at(-1).body, apply = since('Apply').at(-1).body;
      assert.equal(prepare.name, 'Add a response cache to the API');
      assert.deepEqual(prepare.repositoryIdentities, ['github:fixture-team/api', 'github:fixture-team/web']);
      assert.equal(prepare.branch, 'feature/response-cache');
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

      // ⌘N again: the last repositories and mode are remembered; the message is not.
      await page.keyboard.press('Meta+n');
      await expect(sheet).toBeVisible();
      await expect(sheet.locator('.new-task-chip')).toHaveText(['fixture-team/api', 'fixture-team/web']);
      await expect(sheet.getByRole('combobox', { name: 'Mode' })).toHaveValue('plan');
      await expect(prompt).toHaveValue('');
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
    assert.deepEqual(errors, [], `no page errors (${scheme})`);
    await context.close();
  }
  console.log('PASS: ⌘N sheet — workspace, conversation and message in one step, remembered choices, background start, kept draft, palette and empty state, empty workspace, ⌘⇧N unchanged. Fixture only.');
} finally { await browser?.close(); await server.close(); await f.close(); }
