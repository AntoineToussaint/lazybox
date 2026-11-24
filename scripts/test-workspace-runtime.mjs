// The workspace runtime against the fake Runner: Run in the header starts the
// default action and shows it live; the menu starts the others; a
// non-concurrent action says where it was stopped; the Actions panel shows
// state, logs, restart and stop; Preview loads the running page with its own
// address bar, history, reload and open in browser; repository scripts are
// reviewed (changed entries marked) and approved by hash before setup runs;
// Settings → Workspaces saves your own overrides. Chromium; TEST_WEBKIT=1 runs WebKit.
import { createServer } from './fixtures/vite-cache.mjs';
import react from '@vitejs/plugin-react';
import { chromium, webkit, expect } from './fixtures/playwright.mjs';
import { fileURLToPath } from 'node:url';
import { mkdir } from 'node:fs/promises';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';
import { testViteCache } from './fixtures/vite-cache.mjs';
import { collectPageErrors, drainPageErrors } from './fixtures/page-errors.mjs';

const fixture = await startRunnerFixture();
const server = await createServer({ ...testViteCache(import.meta.url), configFile: false, root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: fixture.url } } } });
const mutations = method => fixture.mutations.filter(m => m.method === method);
const engine = process.env.TEST_WEBKIT ? webkit : chromium;
const home = `http://127.0.0.1:${fixture.previewPort}/`;
let browser;
try {
  await server.listen(); browser = await engine.launch({ headless: true });
  const context = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  const page = await context.newPage();
  const errors = []; await collectPageErrors(page, errors);
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`);
  await expect(page.locator('.panel-chat').getByRole('textbox', { name: 'Message', exact: true })).toBeVisible();
  await mkdir('.cache', { recursive: true });

  // The header offers the default action, and flags scripts waiting for review.
  const run = page.getByRole('group', { name: 'Run', exact: true });
  await expect(run.getByRole('button', { name: 'Run dev', exact: true })).toBeVisible();
  await expect(run.getByRole('button', { name: 'Review web’s scripts', exact: true })).toBeVisible();
  await page.screenshot({ path: '.cache/runtime-header.png', clip: { x: 640, y: 0, width: 800, height: 60 } });

  // Run starts it here and stops it in Beta release, where it was running: it says so.
  await run.getByRole('button', { name: 'Run dev', exact: true }).click();
  await expect.poll(() => mutations('StartWorkspaceAction').map(m => [m.body.collectionId, m.body.action, !!m.body.restart])).toEqual([['alpha', 'api/dev', false]]);
  await expect(run.getByRole('status')).toHaveText('Stopped dev in Beta release to run it here.');
  const live = run.getByRole('button', { name: new RegExp(`^dev running on port ${fixture.previewPort}, \\d+s\\. Open its preview$`) });
  await expect(live).toBeVisible();
  await expect(live).toContainText(`:${fixture.previewPort}`);
  await expect(run.getByRole('button', { name: 'Stop dev', exact: true })).toBeEnabled();
  await page.screenshot({ path: '.cache/runtime-running.png', clip: { x: 520, y: 0, width: 920, height: 110 } });

  // The live status opens the preview: the page itself, in the workspace.
  await live.click();
  const preview = page.getByRole('region', { name: 'App preview', exact: true });
  await expect(preview).toBeVisible();
  const frame = page.frameLocator('iframe[title="Preview of dev"]');
  await expect(frame.getByRole('heading', { name: 'Fixture app home' })).toBeVisible();
  const address = preview.getByRole('textbox', { name: 'Preview address', exact: true });
  await expect(address).toHaveValue(home);
  await expect(preview.getByRole('button', { name: 'Back', exact: true })).toBeDisabled();
  // An address typed in the bar is history the panel can step through.
  await address.fill(`127.0.0.1:${fixture.previewPort}/about`);
  await address.press('Enter');
  await expect(frame.getByRole('heading', { name: 'About the fixture app' })).toBeVisible();
  await preview.getByRole('button', { name: 'Back', exact: true }).click();
  await expect(frame.getByRole('heading', { name: 'Fixture app home' })).toBeVisible();
  await expect(address).toHaveValue(home);
  await preview.getByRole('button', { name: 'Forward', exact: true }).click();
  await expect(frame.getByRole('heading', { name: 'About the fixture app' })).toBeVisible();
  await preview.getByRole('button', { name: 'Reload preview', exact: true }).click();
  await expect(frame.getByRole('heading', { name: 'About the fixture app' })).toBeVisible();
  await address.fill('javascript:alert(1)'); await address.press('Enter');
  await expect(preview.getByRole('alert')).toContainText('Lazybox shows web pages (http and https), not javascript: links.');
  await address.fill(`${home}about`); await address.press('Enter');
  const [opened] = await Promise.all([context.waitForEvent('page'), preview.getByRole('button', { name: 'Open in browser', exact: true }).click()]);
  await expect.poll(() => opened.url()).toBe(`${home}about`);
  await opened.close();
  await page.screenshot({ path: '.cache/runtime-preview.png' });

  // The menu runs the others and offers the panels.
  await run.getByLabel('More actions', { exact: true }).click();
  const menu = page.getByRole('group', { name: 'More actions menu', exact: true });
  await expect(menu.getByRole('button', { name: 'Run storybook', exact: true })).toBeDisabled();
  await page.screenshot({ path: '.cache/runtime-menu.png' });
  await menu.getByRole('button', { name: 'Run test', exact: true }).click();
  await expect.poll(() => mutations('StartWorkspaceAction').at(-1).body.action).toBe('api/test');
  await run.getByLabel('More actions', { exact: true }).click();
  await menu.getByRole('button', { name: 'Show actions and logs', exact: true }).click();

  // The Actions panel: each repository's setup and actions, with logs.
  const actions = page.getByRole('region', { name: 'Workspace actions', exact: true });
  await expect(actions).toBeVisible();
  await expect(actions).toContainText(`ports ${fixture.previewPort}–${fixture.previewPort + 9}`);
  const dev = actions.getByRole('listitem', { name: 'dev, Running' });
  await expect(dev).toContainText('one at a time');
  await expect(dev).toContainText('default');
  await expect(actions.getByRole('listitem', { name: 'test, Running' })).toBeVisible();
  await expect(actions.getByRole('group', { name: 'api setup, Done' })).toContainText('Copied from your checkout: .env');
  await expect(actions.getByRole('group', { name: 'web setup, Waiting for your approval' })).toContainText('Not in your checkout, so not copied: .env.local');
  await expect(actions.getByRole('listitem', { name: 'storybook, Not running' })).toContainText('Approve web’s scripts first');
  await dev.getByRole('button', { name: 'Show dev log', exact: true }).click();
  await expect(dev.getByTestId('console-output')).toContainText(`ready on ${home.slice(0, -1)}`);
  await dev.getByRole('button', { name: 'Restart dev', exact: true }).click();
  await expect.poll(() => mutations('StartWorkspaceAction').at(-1).body).toMatchObject({ action: 'api/dev', restart: true });
  await page.screenshot({ path: '.cache/runtime-actions.png' });

  // Review: the file, its changed entries marked, and what approving means. Not now changes nothing.
  await run.getByRole('button', { name: 'Review web’s scripts', exact: true }).click();
  let dialog = page.getByRole('dialog', { name: 'Review web’s workspace scripts' });
  await expect(dialog).toContainText('These scripts run code on this machine, as you');
  await expect(dialog.getByRole('list', { name: 'Changed entries' })).toHaveText(/Setup script\s*Action “storybook”/);
  await expect(dialog.locator('.approval-file .is-changed')).toHaveCount(7);
  await expect(dialog.getByRole('button', { name: 'Not now', exact: true })).toBeFocused();
  await page.screenshot({ path: '.cache/runtime-approval.png' });
  await dialog.getByRole('button', { name: 'Not now', exact: true }).click();
  await expect(dialog).toHaveCount(0);
  assert.equal(mutations('ApproveWorkspaceScripts').length, 0, 'Not now approved something');

  // Approve and run setup, from the panel's banner: the hash shown is the hash approved.
  await actions.getByRole('note').filter({ hasText: 'web’s scripts changed since you approved them' }).getByRole('button', { name: 'Review…' }).click();
  dialog = page.getByRole('dialog', { name: 'Review web’s workspace scripts' });
  await dialog.getByRole('button', { name: 'Approve and run setup', exact: true }).click();
  await expect(dialog).toHaveCount(0);
  assert.deepEqual(mutations('ApproveWorkspaceScripts').map(m => m.body), [{ collectionId: 'alpha', member: 'web', hash: 'web-alpha-2', runSetup: true }]);
  const settingUp = run.getByRole('button', { name: /^Setting up web, \d+s\. Show its log$/ });
  await expect(settingUp).toBeVisible();
  await expect(run.getByRole('button', { name: /Review/ })).toHaveCount(0);
  await expect(actions.getByRole('group', { name: 'web setup, Running' })).toBeVisible();
  await expect(actions.getByRole('listitem', { name: 'storybook, Not running' }).getByRole('button', { name: 'Start storybook', exact: true })).toBeEnabled();
  await page.screenshot({ path: '.cache/runtime-setup.png' });
  // The setup status opens its log.
  await settingUp.click();
  await expect(actions.getByRole('group', { name: 'web setup, Running' }).getByTestId('console-output')).toContainText('pnpm install');
  fixture.finishSetup('alpha', 'web');
  await expect(actions.getByRole('group', { name: 'web setup, Done' })).toBeVisible();
  await expect(run.getByRole('button', { name: /^dev running on port/ })).toBeVisible();

  // Stop from the header: SIGTERM, then the control offers Run again.
  await run.getByRole('button', { name: 'Stop dev', exact: true }).click();
  await expect.poll(() => mutations('StopWorkspaceAction').map(m => m.body.action)).toEqual(['api/dev']);
  await expect(actions.getByRole('listitem', { name: 'dev, Stopped' })).toBeVisible();
  await actions.getByRole('listitem', { name: 'test, Running' }).getByRole('button', { name: 'Stop test', exact: true }).click();
  await expect(run.getByRole('button', { name: 'Run dev', exact: true })).toBeVisible();
  // With nothing running, the preview offers to start what it would show.
  await expect(preview.getByRole('button', { name: 'Start dev', exact: true })).toBeVisible();

  // Settings → Workspaces: the port range and your own additions, refused with the line when wrong.
  await page.getByRole('button', { name: 'Settings', exact: true }).click();
  await page.getByRole('tab', { name: 'Workspaces', exact: true }).click();
  const settings = page.getByRole('form', { name: 'Workspaces' });
  await expect(settings.getByRole('spinbutton', { name: 'From' })).toHaveValue('20000');
  await settings.locator('summary').filter({ hasText: 'backend' }).click();
  const additions = settings.getByRole('textbox', { name: 'Your workspace settings for backend' });
  await additions.fill('bogus: 1');
  await settings.getByRole('button', { name: 'Save workspace settings' }).click();
  await expect(settings.getByRole('alert')).toContainText('field bogus not found');
  await additions.fill('actions:\n  lint:\n    run: npm run lint\n');
  await settings.getByRole('button', { name: 'Save workspace settings' }).click();
  await expect(settings.getByRole('status')).toContainText('Workspace settings saved.');
  assert.equal(mutations('SaveWorkspaceRuntimeSettings').at(-1).body.repositories.find(r => r.repositoryId === 'backend').config, 'actions:\n  lint:\n    run: npm run lint\n');
  await page.screenshot({ path: '.cache/runtime-settings.png' });
  await page.getByRole('navigation', { name: 'Pages' }).getByRole('button', { name: 'Work' }).click();

  // A phone: the control is one icon, and nothing scrolls sideways.
  await run.getByRole('button', { name: 'Run dev', exact: true }).click();
  await page.setViewportSize({ width: 390, height: 844 });
  // A phone gets one button: the live status; Stop and the menu live in the Actions panel.
  await expect(run.getByRole('button', { name: /^dev running on port/ })).toBeVisible();
  await expect(run.getByRole('button', { name: 'Stop dev', exact: true })).toBeHidden();
  await page.getByRole('navigation', { name: 'Workspace panels' }).getByRole('button', { name: 'Actions', exact: true }).click();
  await expect(actions.getByRole('listitem', { name: 'dev, Running' })).toBeVisible();
  assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false, 'no horizontal overflow on a phone');
  await page.screenshot({ path: '.cache/runtime-phone.png' });

  await drainPageErrors(errors);assert.deepEqual(errors, []);
  console.log('Workspace runtime: ran, previewed, listed, logged, restarted, stopped; scripts reviewed and approved by hash; settings saved.');
} catch (error) {
  for (const p of browser?.contexts().flatMap(c => c.pages()) || []) await p.screenshot({ path: '.cache/runtime-failure.png', fullPage: true }).catch(() => {});
  throw error;
} finally {
  await browser?.close(); await server.close(); await fixture.close();
}
