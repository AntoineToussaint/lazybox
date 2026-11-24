// Fixture only: the person's actions and hooks file (POWER.md) in the app.
// Actions are on their ]] keys and run their steps (send, run a workspace
// action, the failed branch, a refusal that stays on screen with its reason);
// the "Run an action…" picker filters and runs by keyboard, asking first when
// the action says so; workspace and conversation menus list theirs; a new
// revision of the file shows without a reload; the CLI's open, action and
// shot requests are answered by the window; Settings → Actions & hooks shows
// problems by line, taken keys, a stale file, creating the file, testing a
// hook and its output; and it fits a phone.
import { testViteCache } from './fixtures/vite-cache.mjs';
import { createServer } from './fixtures/vite-cache.mjs';
import react from '@vitejs/plugin-react';
import { webkit, expect } from './fixtures/playwright.mjs';
import { fileURLToPath } from 'node:url';
import { mkdir } from 'node:fs/promises';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';
import { collectPageErrors } from './fixtures/page-errors.mjs';

const f = await startRunnerFixture();
Object.assign(f.sessions[0], { title: 'fix the tests', status: 'done', agents: [{ id: 'root', status: 'done' }] });
Object.assign(f.sessions[1], { title: 'release notes' });
const step = s => ({ when: 'always', ...s });
const actions = [
  { id: 'nudge', title: 'Nudge the agent', icon: 'send', key: 'g', where: ['palette', 'menu', 'conversation'], confirm: false, steps: [step({ kind: 'send', text: 'Continue please.', to: 'current' })] },
  { id: 'test-then-fix', title: 'Test then fix', icon: 'play', key: 't', where: ['palette', 'menu', 'workspace'], confirm: false, steps: [step({ kind: 'run', action: 'api/test' }), step({ kind: 'send', text: 'The tests failed; fix them.', to: 'current', when: 'failed' }), step({ kind: 'open', panel: 'changes', when: 'succeeded' })] },
  { id: 'storybook', title: 'Storybook', icon: 'eye', key: 'y', where: ['palette', 'menu'], confirm: false, steps: [step({ kind: 'run', action: 'web/storybook' })] },
  { id: 'clash', title: 'Clashing key', icon: 'play', key: 'w', where: ['menu'], confirm: false, steps: [step({ kind: 'open', panel: 'files' })] },
  { id: 'unattended', title: 'Run unattended', icon: 'shield', key: '', where: ['palette'], confirm: true, description: 'No approvals until you change it back.', steps: [step({ kind: 'permission', permission: 'yolo' })] },
];
const hook = h => ({ description: '', workspaces: [], conversations: [], repositories: [], run: [], shell: '', webhook: '', headers: {}, grants: [], timeoutSeconds: 60, enabled: true, ...h });
f.power.setFile({
  actions,
  hooks: [hook({ id: 'notify', on: ['agent.needs-input'], run: ['terminal-notifier', '-message', 'needs you'], workspaces: ['Alpha*'] }), hook({ id: 'keep-going', on: ['turn.done'], shell: 'lazybox send "$LAZYBOX_EVENT_CONVERSATION" Continue.', grants: ['respond'] })],
  issues: [{ path: 'actions[5].key', line: 27, message: '"tt" must be one lowercase letter or digit (or Shift+letter); it follows the ]] leader', severity: 'error' }, { path: 'extras', line: 2, message: 'unknown section "extras" is ignored (expected version, actions, hooks)', severity: 'warning' }],
});
const applied = text => f.mutations.some(m => m.method === 'Apply' && m.body.message?.text === text);

const server = await createServer({ ...testViteCache(import.meta.url), configFile: false, logLevel: 'error', root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: f.url } } } });
let browser;
try {
  await server.listen(); await mkdir('.cache', { recursive: true });
  browser = await webkit.launch();
  const page = await browser.newPage({ viewport: { width: 1360, height: 900 } });
  page.setDefaultTimeout(10000);
  const errors = []; await collectPageErrors(page, errors);
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${f.token}`);
  const message = page.getByRole('textbox', { name: 'Message', exact: true });
  const leader = async (...keys) => { await message.focus(); await message.press(']'); await message.press(']'); for (const key of keys) await page.keyboard.press(key); };
  const toasts = page.getByLabel('Running actions');

  // ]] lists the actions with their keys; a key taken by a built-in is not bound.
  await leader();
  const menu = page.getByRole('status', { name: 'Keyboard menu' });
  await expect(menu).toContainText('Nudge the agent');
  await expect(menu).toContainText('Run an action…');
  await expect(menu.locator('li', { hasText: 'Clashing key' })).toHaveCount(0);
  await page.screenshot({ path: '.cache/power-leader-menu.png' });
  await page.keyboard.press('g');
  await expect.poll(() => applied('Continue please.')).toBe(true);
  await expect(toasts.getByRole('status', { name: 'Action Nudge the agent' })).toContainText('Done');
  await expect(toasts.getByRole('status', { name: 'Action Nudge the agent' })).toHaveCount(0, { timeout: 8000 });

  // A workspace action runs; it fails, so the when: failed step runs instead of opening changes.
  await leader('t');
  const run = toasts.getByLabel('Action Test then fix');
  await expect(run).toContainText('Step 1 of 3 · Run api/test');
  await expect(run.getByRole('button', { name: 'Cancel' })).toBeVisible();
  await page.screenshot({ path: '.cache/power-toast-running.png' });
  await expect.poll(() => f.mutations.some(m => m.method === 'StartWorkspaceAction' && m.body.action === 'api/test')).toBe(true);
  const command = f.runtimeCommands.findLast(c => c.origin === 'action' && c.collectionId === 'alpha');
  Object.assign(command, { done: true, exitCode: 1 });
  await expect.poll(() => applied('The tests failed; fix them.')).toBe(true);
  await expect(run).toContainText('Done · a failure was handled');

  // A refusal (scripts not approved) stays on screen with its reason until closed.
  await leader('y');
  const refused = toasts.getByRole('alert', { name: 'Action Storybook' });
  await expect(refused).toContainText('Failed at step 1');
  await expect(refused).toContainText('Approve web’s scripts first');
  await page.screenshot({ path: '.cache/power-toast-failed.png' });
  await refused.getByRole('button', { name: 'Close Storybook' }).click();
  await expect(refused).toHaveCount(0);

  // The picker: type to filter, Enter to run; an action that asks first asks.
  await leader('x');
  const picker = page.getByRole('dialog', { name: 'Run an action' });
  await expect(picker.getByRole('searchbox', { name: 'Find actions' })).toBeFocused();
  await expect(picker.getByRole('option')).toHaveCount(5);
  const actionBounds = await picker.boundingBox();
  const actionSearch = picker.getByRole('searchbox', { name: 'Find actions' });
  await actionSearch.fill('zzqxyunmatched');
  await expect(picker.getByText('No matching actions.')).toBeVisible();
  assert.deepEqual(await picker.boundingBox(), actionBounds);
  await actionSearch.fill('unatt');
  await expect(picker.getByRole('option')).toHaveCount(1);
  await page.screenshot({ path: '.cache/power-picker.png' });
  await page.keyboard.press('Enter');
  const confirm = page.getByRole('dialog', { name: 'Run “Run unattended”?' });
  await expect(confirm).toContainText('No approvals until you change it back.');
  await expect(confirm.getByRole('button', { name: 'Run', exact: true })).toBeFocused();
  await page.keyboard.press('Enter');
  await expect.poll(() => f.mutations.some(m => m.method === 'Apply' && m.body.permission?.mode === 'yolo' && m.body.sessionId === 'alpha-session')).toBe(true);

  // Menus list the actions offered there.
  // Right-click: the shared context menu gains a "Your actions" submenu.
  const contextMenu = page.locator('.context-menu[data-depth="0"]'), submenu = page.locator('.context-menu[data-depth="1"]');
  const closeMenus = async () => { for (let i = 0; i < 3 && await contextMenu.count(); i++) await contextMenu.getByRole('menuitem').first().press('Escape'); await expect(contextMenu).toHaveCount(0); };
  const yours = async () => { await contextMenu.getByRole('menuitem', { name: 'Your actions' }).hover(); await expect(submenu).toBeVisible(); return submenu; };
  const alpha = page.locator('.workspace-row', { hasText: 'Alpha feature' });
  await alpha.click({ button: 'right' });
  let items = await yours();
  await expect(items.getByRole('menuitem', { name: /Test then fix/ })).toBeVisible();
  await expect(items.getByRole('menuitem', { name: /Nudge the agent/ })).toHaveCount(0);
  await closeMenus();
  await page.locator('.panel-chat .chat-heading .agent-avatar').first().click({ button: 'right' });
  items = await yours();
  await expect(items.getByRole('menuitem', { name: /Nudge the agent/ })).toBeVisible();
  await page.screenshot({ path: '.cache/power-context-menu.png' });
  await closeMenus();
  await expect(contextMenu).toHaveCount(0);
  // The command palette lists the actions offered there, with their ]] key.
  await leader(); await page.keyboard.press('p');
  const palette = page.getByRole('dialog', { name: /command palette/i });
  await expect(palette).toBeVisible();
  await page.keyboard.type('nudge');
  const row = palette.getByRole('option', { name: /Nudge the agent/ });
  await expect(row).toBeVisible();
  await expect(row).toContainText('Your action');
  await page.keyboard.press('Escape');
  await expect(palette).toHaveCount(0);

  // A new revision of the file shows without a reload.
  f.power.setFile({ actions: [...actions, { id: 'fresh', title: 'Fresh action', icon: 'sparkle', key: 'k', where: ['menu'], confirm: false, steps: [step({ kind: 'open', panel: 'files' })] }] });
  await expect.poll(async () => { await leader(); const text = await menu.textContent(); await page.keyboard.press('Escape'); return text; }, { timeout: 8000 }).toContain('Fresh action');

  // The CLI asks the window: open a conversation, run an action, take a shot.
  await f.power.waitForWindow();
  assert.deepEqual(f.power.windows, ['web']);
  const opened = await f.power.post({ kind: 'open', collectionId: 'beta', sessionId: 'beta-session' });
  assert.equal(opened.state, 'done', opened.error);
  assert.match(opened.detail, /^Showed Beta release · release notes/);
  await expect(page.locator('.workspace-row', { hasText: 'Beta release' }).locator('[aria-current=page]')).toHaveCount(1);
  const ran = await f.power.post({ kind: 'action', actionId: 'nudge', collectionId: 'alpha', sessionId: 'alpha-session' });
  assert.equal(ran.state, 'done'); assert.equal(ran.detail, 'Ran “Nudge the agent”');
  const shot = await f.power.post({ kind: 'shot' });
  assert.equal(shot.state, 'failed'); assert.match(shot.error, /desktop app/);

  // Settings → Actions & hooks.
  await page.getByRole('button', { name: 'Settings', exact: true }).click();
  await page.getByRole('tab', { name: 'Actions & hooks' }).click();
  const panel = page.getByRole('tabpanel', { name: 'Actions & hooks' });
  await expect(panel).toContainText('/fixture/config/lazybox/actions.yaml');
  const issues = panel.getByRole('list', { name: 'Problems in the actions file' });
  await expect(issues.getByRole('listitem').first()).toContainText('line 27');
  await expect(issues).toContainText('Warning');
  await expect(panel.getByRole('list', { name: 'Your actions' }).getByRole('listitem').filter({ hasText: 'Clashing key' })).toContainText(']] w · taken');
  const hooks = panel.getByRole('list', { name: 'Your hooks' });
  await expect(hooks.getByRole('listitem').filter({ hasText: 'keep-going' })).toContainText('respond');
  await hooks.getByRole('button', { name: 'Test notify' }).click();
  const runs = panel.getByRole('list', { name: 'Recent hook runs' });
  await expect(runs).toContainText('notify');
  await expect(runs.getByLabel('Output of notify')).toContainText('fixture hook notify ran for agent.needs-input');
  await page.screenshot({ path: '.cache/power-settings.png', fullPage: true });
  // Run from Settings.
  await panel.getByRole('button', { name: 'Run Nudge the agent' }).click();
  await expect(toasts.getByLabel('Action Nudge the agent')).toContainText('Done');
  // A file that stops loading keeps the last version, and says so.
  f.power.setFile({ error: 'not valid YAML: line 4: did not find expected key', stale: true });
  await expect(panel.getByRole('alert')).toContainText('Using the last version that loaded');
  // No file yet: create it.
  f.power.setFile({ exists: false, error: '', stale: false, actions: [], hooks: [], issues: [] });
  await panel.getByRole('button', { name: 'Create actions.yaml' }).click();
  await expect(panel.getByRole('list', { name: 'Your actions' })).toContainText('Tell the agent to continue');
  assert.ok(f.mutations.some(m => m.method === 'InitUserActions'));
  // A phone: the tab fits without sideways scrolling.
  await page.setViewportSize({ width: 390, height: 844 });
  await expect(panel).toBeVisible();
  assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1), 'no horizontal page scroll');
  await page.screenshot({ path: '.cache/power-settings-phone.png' });
  assert.deepEqual(errors, []);
  console.log('PASS power actions: ]] keys and taken keys, send/run/failed branch, refusal toast, picker with confirm, workspace and conversation menus, hot reload, CLI open/action/shot answered, Settings problems by line, hook test and output, stale file, create file, phone width. Fixture only.');
} finally { await browser?.close(); await server.close(); await f.close(); }
