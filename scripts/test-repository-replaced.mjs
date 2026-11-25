// Fixture only: a repository whose history changed on GitHub (WORKSPACES.md,
// "When a repository's history changes"). New task used to end in
// "repository identity changed; refresh and retry", which refreshing did not
// fix. Now its error card says plainly what happened and offers Replace
// Lazybox's copy: it shows what that changes (the workspaces made from the
// old copy keep their worktrees), replaces it, and starts again by itself.
// When Lazybox only listed the repository, the card says a new repository took
// the name and offers to use it. Settings → Repositories offers the same.
// All by keyboard. Screenshot: .cache/repository-replaced.png. WebKit.
import { testViteCache } from './fixtures/vite-cache.mjs';
import { createServer } from './fixtures/vite-cache.mjs';
import react from '@vitejs/plugin-react';
import { webkit, expect } from './fixtures/playwright.mjs';
import { fileURLToPath } from 'node:url';
import { mkdir } from 'node:fs/promises';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';
import { collectPageErrors, drainPageErrors } from './fixtures/page-errors.mjs';

const f = await startRunnerFixture();
f.completeGitHub();
const since = method => f.mutations.filter(m => m.method === method);
const server = await createServer({ ...testViteCache(import.meta.url), configFile: false, logLevel: 'error', root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: f.url } } } });
let browser;
try {
  await server.listen(); await mkdir('.cache', { recursive: true });
  browser = await webkit.launch();
  const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
  page.setDefaultTimeout(15_000);
  const errors = []; await collectPageErrors(page, errors);
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace#token=${f.token}`);
  await expect(page.getByRole('navigation', { name: 'Workspaces' })).toBeVisible();

  const sheet = page.getByRole('dialog', { name: 'New task' });
  const startWith = async (repository, message, branch = '') => {
    await page.keyboard.press('Meta+n');
    await expect(sheet).toBeVisible();
    await sheet.getByRole('textbox', { name: 'First message' }).fill(message);
    for (const chip of await sheet.locator('.new-task-chip').all()) await chip.getByRole('button', { name: /^Remove / }).click();
    const search = sheet.getByRole('combobox', { name: 'Add a repository' });
    await search.focus(); await search.fill(repository);
    const match = sheet.getByRole('listbox', { name: 'Repositories' }).getByRole('option').filter({ hasText: repository });
    await expect(match).toBeVisible();
    await expect(match).toHaveAttribute('aria-selected', 'true');
    await page.keyboard.press('Enter');
    await expect(sheet.locator('.new-task-chip-name')).toHaveText([repository]);
    if (branch) {
      // The Start-from chip beside it (⌥B): the error card must not lose it.
      await search.focus();
      await page.keyboard.press('Alt+KeyB');
      const picker = sheet.getByRole('dialog', { name: `Start ${repository} from` });
      await expect(picker.getByRole('combobox', { name: `Branch of ${repository} to start from` })).toBeFocused();
      await page.keyboard.type(branch);
      await expect(picker.getByRole('option', { name: new RegExp(`Start from ${branch}`) })).toBeVisible();
      await page.keyboard.press('Enter');
      await expect(picker).toHaveCount(0);
      await expect(sheet.locator(`[data-start-from="github:${repository}"]`)).toHaveText(new RegExp(`from\\s*${branch}`));
    }
    await sheet.getByRole('textbox', { name: 'First message' }).focus();
    await page.keyboard.press('Enter');
  };

  // A copy whose history GitHub replaced: refused, said plainly, and the way forward.
  f.replacedRepositories.set('github:fixture-team/api', { hasCopy: true, workspaces: ['Alpha feature'] });
  await startWith('fixture-team/api', 'Rebuild the cache on the new history', 'release/1.4');
  const card = sheet.getByRole('region', { name: 'fixture-team/api changed on GitHub' });
  await expect(card).toBeVisible();
  await expect(card).toContainText('The repository’s history changed on GitHub since Lazybox copied it.');
  await expect(card).toContainText('Lazybox won’t mix the two histories in one workspace.');
  const offer = card.getByRole('button', { name: 'Replace Lazybox’s copy…' });
  await expect(offer).toBeFocused();
  assert.equal(since('PrepareWorkspace').length, 1);
  assert.equal(since('RefreshRepositoryIdentity').length, 0, 'Nothing is replaced before the person confirms');
  // Enter: what replacing changes, before it does.
  await page.keyboard.press('Enter');
  const effects = card.getByRole('group', { name: 'What replacing the copy changes' });
  await expect(effects).toContainText('Lazybox will copy fixture-team/api as GitHub has it now (the new repository under that name).');
  await expect(effects).toContainText('One workspace was made from the old copy: Alpha feature. It keeps working on its own worktrees and branches');
  const confirm = card.getByRole('button', { name: 'Replace and continue' });
  await expect(confirm).toBeFocused();
  await page.screenshot({ path: '.cache/repository-replaced.png' });
  assert.equal(since('RefreshRepositoryIdentity').length, 0, 'The preview changes nothing');
  // Enter again: replaced, then the start goes on by itself and finishes.
  await page.keyboard.press('Enter');
  await expect(sheet).not.toBeVisible();
  assert.deepEqual(since('RefreshRepositoryIdentity').map(m => m.body), [{ identity: 'github:fixture-team/api', confirm: true }]);
  const prepared = since('PrepareWorkspace');
  assert.equal(prepared.length, 2, 'Retried once, automatically');
  assert.equal(prepared[0].body.requestKey, prepared[1].body.requestKey, 'The retry is the same start, never a second workspace');
  for (const p of prepared) assert.deepEqual(p.body.startFrom, { 'github:fixture-team/api': 'release/1.4' }, 'The retry keeps the branch chosen to start from');
  assert.ok(f.collections.some(c => c.name === 'Rebuild the cache on the new history'));

  // Only listed, never copied, and a new repository took the name (the case
  // from the field): the card says so and offers the repository GitHub has now.
  f.replacedRepositories.set('github:fixture-team/web', { hasCopy: false });
  await startWith('fixture-team/web', 'Ship the web app from its new repository');
  const listed = sheet.getByRole('region', { name: 'fixture-team/web changed on GitHub' });
  await expect(listed).toContainText('fixture-team/web on GitHub is a different repository now: a new one took its name since Lazybox listed it.');
  await expect(listed.getByRole('button', { name: 'Use the new repository…' })).toBeFocused();
  await page.keyboard.press('Enter');
  await expect(listed.getByRole('button', { name: 'Copy it and continue' })).toBeFocused();
  await expect(listed).not.toContainText('made from the old copy');
  // Cancel goes back to the offer; nothing replaced.
  await listed.getByRole('button', { name: 'Cancel' }).click();
  await expect(listed.getByRole('button', { name: 'Use the new repository…' })).toBeVisible();
  assert.equal(since('RefreshRepositoryIdentity').length, 1);
  await listed.getByRole('button', { name: 'Use the new repository…' }).focus();
  await page.keyboard.press('Enter');
  await expect(listed.getByRole('button', { name: 'Copy it and continue' })).toBeFocused();
  await page.keyboard.press('Enter');
  await expect(sheet).not.toBeVisible();
  assert.equal(since('RefreshRepositoryIdentity').length, 2);

  // Settings → Repositories: Replace copy… on a GitHub repository Lazybox copied.
  await page.getByRole('button', { name: 'Settings', exact: true }).click();
  await page.getByRole('tab', { name: 'Repositories', exact: true }).click();
  const replaceCopy = page.getByRole('button', { name: 'Replace Lazybox’s copy of fixture-team/api' });
  await replaceCopy.focus();
  await page.keyboard.press('Enter');
  const inline = page.getByRole('region', { name: 'fixture-team/api changed on GitHub' });
  await expect(inline.getByRole('button', { name: 'Replace Lazybox’s copy…' })).toBeFocused();
  await page.keyboard.press('Enter');
  await expect(inline.getByRole('button', { name: 'Replace and continue' })).toBeFocused();
  await page.keyboard.press('Enter');
  await expect(page.getByRole('status').filter({ hasText: 'Replaced Lazybox’s copy of fixture-team/api. New workspaces start from it.' })).toBeVisible();
  assert.equal(since('RefreshRepositoryIdentity').length, 3);

  await drainPageErrors(errors);assert.deepEqual(errors, []);
  console.log('Repository replaced on GitHub: the error card explains, replaces Lazybox’s copy after showing what changes, and retries; Settings offers the same; all by keyboard.');
} finally {
  await browser?.close(); await server.close(); await f.close();
}
