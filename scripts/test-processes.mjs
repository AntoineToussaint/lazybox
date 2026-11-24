// The Processes panel and Settings → Runner → Process hosting, against the fake
// Runner: every hosted agent, command and terminal is listed with what it
// belongs to and what it costs; Open goes to its conversation, console or
// terminal; a tmux-hosted one offers its attach command; Stop and Clean up
// confirm first and say what happens; choosing tmux applies to new processes,
// and a tmux that cannot be used is explained. Chromium (clipboard access);
// TEST_WEBKIT=1 runs WebKit without the clipboard check.
import {testViteCache} from './fixtures/vite-cache.mjs';
import { createServer } from './fixtures/vite-cache.mjs';
import react from '@vitejs/plugin-react';
import { chromium, webkit, expect } from './fixtures/playwright.mjs';
import { fileURLToPath } from 'node:url';
import { mkdir } from 'node:fs/promises';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';
import { collectPageErrors, drainPageErrors } from './fixtures/page-errors.mjs';

const fixture = await startRunnerFixture();
const server = await createServer({...testViteCache(import.meta.url), configFile: false, root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: fixture.url } } } });
const mutations = method => fixture.mutations.filter(m => m.method === method);
const engine = process.env.TEST_WEBKIT ? webkit : chromium;
let browser;
try {
  await server.listen(); browser = await engine.launch({ headless: true });
  const context = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  if (!process.env.TEST_WEBKIT) await context.grantPermissions(['clipboard-read', 'clipboard-write']);
  const page = await context.newPage();
  const errors = []; await collectPageErrors(page, errors);
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`);
  await expect(page.locator('.panel-chat').getByRole('textbox', { name: 'Message', exact: true })).toBeVisible();
  await mkdir('.cache', { recursive: true });

  // A shell in this workspace, so the panel lists a terminal too.
  await page.getByRole('button', { name: 'Open terminal', exact: true }).click();
  await expect(page.locator('.panel-terminal').getByTestId('terminal')).toContainText('fixture shell in Alpha feature');
  await page.getByRole('button', { name: 'Close Terminal panel', exact: true }).click();

  // The Panels menu shows the Processes panel like any other.
  await page.locator('summary[aria-label="Panels"]').click();
  await page.getByRole('button', { name: 'Show Processes', exact: true }).click();
  const panel = page.getByRole('region', { name: 'Hosted processes', exact: true });
  await expect(panel).toBeVisible();
  await expect(panel).toContainText('3 processes running');
  await expect(panel).toContainText('1 agent · 1 command · 1 terminal');
  await expect(panel).toContainText('New processes run in session hosts.');

  // Each process says what it is, where it belongs and what it costs.
  const agents = panel.getByRole('region', { name: /Agents/ });
  const agent = agents.getByRole('listitem', { name: 'Fix the parser, Running' });
  await expect(agent).toContainText('Claude · Alpha feature');
  await expect(agent).toContainText('PID 4102 · up 2h 15m · 3.2% CPU · 412 MB · 5 processes · session host');
  const command = panel.getByRole('region', { name: /Commands/ }).getByRole('listitem', { name: 'go test ./..., Running' });
  await expect(command).toContainText('in “Ship the release” · Beta release');
  await expect(command).toContainText('tmux');
  const terminal = panel.getByRole('region', { name: /Terminals/ }).getByRole('listitem', { name: /Alpha feature, Running/ });
  await expect(terminal).toBeVisible();
  const orphans = panel.getByRole('region', { name: /Not supervised by this Runner/ });
  await expect(orphans.getByRole('listitem', { name: 'codex · old-checkout, Orphaned' })).toContainText('up 1d 1h');
  await expect(orphans).toContainText('No Runner supervises these');
  // Only a tmux-hosted process offers an attach command.
  await expect(agent.getByRole('button', { name: /Copy the tmux attach command/ })).toHaveCount(0);
  await page.screenshot({ path: '.cache/processes-panel.png' });

  // Copy attach puts the exact command on the clipboard and says how to use it.
  await command.getByRole('button', { name: 'Copy the tmux attach command for go test ./...', exact: true }).click();
  await expect(panel.getByRole('status').filter({ hasText: 'Copied. Paste it in any terminal' })).toBeVisible();
  if (!process.env.TEST_WEBKIT) assert.equal(await page.evaluate(() => navigator.clipboard.readText()), 'tmux -S /private/tmp/tmux-501/lazybox-fixture attach -t host-command-beta');

  // This workspace only: the command and the orphan live elsewhere.
  await panel.getByRole('checkbox', { name: 'This workspace only', exact: true }).check();
  await expect(panel.getByRole('region', { name: /Commands/ })).toHaveCount(0);
  await expect(orphans).toHaveCount(0);
  await expect(agent).toBeVisible();
  await panel.getByRole('checkbox', { name: 'This workspace only', exact: true }).uncheck();

  // Open takes a terminal to its own tab in the Terminal panel.
  await terminal.getByRole('button', { name: /^Open / }).click();
  await expect(page.locator('.panel-terminal').getByRole('tab', { name: 'Workspace root', exact: true })).toHaveAttribute('aria-selected', 'true');
  assert.equal(fixture.calls.filter(c => c.method === 'OpenTerminal').length, 1, 'opening a listed terminal never starts another shell');

  // Stop asks first, with Cancel focused, and says what stopping an agent does.
  await agent.getByRole('button', { name: 'Stop Fix the parser…', exact: true }).click();
  let dialog = page.getByRole('dialog', { name: 'Stop “Fix the parser”?' });
  await expect(dialog).toContainText('The conversation stays in your history');
  await expect(dialog.getByRole('button', { name: 'Cancel', exact: true })).toBeFocused();
  await page.screenshot({ path: '.cache/processes-stop-confirm.png' });
  await page.keyboard.press('Escape');
  await expect(dialog).toHaveCount(0);
  assert.equal(mutations('StopProcess').length, 0, 'cancelling stopped a process');
  await agent.getByRole('button', { name: 'Stop Fix the parser…', exact: true }).click();
  await dialog.getByRole('button', { name: 'Stop agent', exact: true }).click();
  await expect(dialog).toHaveCount(0);
  await expect(panel.getByRole('status').filter({ hasText: 'Stopped “Fix the parser”.' })).toBeVisible();
  await expect(agents).toHaveCount(0);
  assert.deepEqual(mutations('StopProcess').map(m => m.body.id), ['host-agent-alpha']);

  // Clean up stops only what no Runner supervises.
  await orphans.getByRole('button', { name: 'Clean up orphans…', exact: true }).click();
  dialog = page.getByRole('dialog', { name: 'Stop 1 orphaned process?' });
  await expect(dialog).toContainText('Processes this Runner supervises are not touched');
  await dialog.getByRole('button', { name: 'Clean up', exact: true }).click();
  await expect(panel.getByRole('status').filter({ hasText: 'Stopped 1 orphaned process.' })).toBeVisible();
  await expect(orphans).toHaveCount(0);
  assert.equal(mutations('CleanUpOrphanProcesses').length, 1);
  await expect(command).toBeVisible();

  // A `lazybox run` that went on in the background (a dev server) is a command too: where it listens, what hosts it, and Stop.
  fixture.commands.push({ id: 'bg-dev', sessionId: 'alpha-session', title: 'npm run dev', argv: ['npm', 'run', 'dev'], mode: 'pipe', done: false, background: true, ports: [5173], pid: 7001, offset: '0', output: '' });
  const devServer = panel.getByRole('region', { name: /Commands/ }).getByRole('listitem', { name: 'npm run dev, Running' });
  await expect(devServer).toContainText('listening on :5173 · PID 7001 · up 42s', { timeout: 8000 });
  await expect(devServer).toContainText('lazybox run');
  await page.screenshot({ path: '.cache/processes-background-run.png' });
  await devServer.getByRole('button', { name: 'Stop npm run dev…', exact: true }).click();
  dialog = page.getByRole('dialog', { name: 'Stop “npm run dev”?' });
  await expect(dialog).toContainText('The command gets SIGTERM, then SIGKILL after 5 seconds');
  await dialog.getByRole('button', { name: 'Stop process', exact: true }).click();
  await expect(devServer).toHaveCount(0);
  assert.equal(mutations('StopProcess').at(-1).body.id, 'run:bg-dev');

  // Settings → Runner: choose tmux for new processes; it says what that means.
  await page.getByRole('button', { name: 'Settings', exact: true }).click();
  await page.getByRole('tab', { name: 'Runner', exact: true }).click();
  const hosting = page.getByRole('region', { name: 'Process hosting', exact: true });
  await expect(hosting.getByRole('radio', { name: /Session hosts \(default\)/ })).toBeChecked();
  await expect(hosting).toContainText('Found tmux 3.4.');
  await hosting.getByRole('radio', { name: /tmux/ }).check();
  await expect(hosting.getByRole('status')).toContainText('New processes will run in tmux. Running processes keep their hosts.');
  assert.equal(mutations('SaveRunnerSettings').at(-1).body.processBackend, 'tmux');
  await page.screenshot({ path: '.cache/processes-settings.png' });

  // Show processes goes straight back to the panel, which now says tmux.
  await hosting.getByRole('button', { name: 'Show processes', exact: true }).click();
  await expect(panel).toContainText('New processes run in tmux (tmux 3.4).');

  // tmux chosen but unusable: the panel says new processes fall back, and why.
  Object.assign(fixture.processHosting, { tmuxVersion: '', tmuxProblem: 'tmux is not installed (not found on the Runner\'s PATH)' });
  await expect(panel.getByRole('status').filter({ hasText: 'tmux is selected, but tmux is not installed' })).toContainText('New processes run in session hosts until it is available.', { timeout: 8000 });

  // Open takes a command to its console, in its own workspace.
  await command.getByRole('button', { name: 'Open go test ./...', exact: true }).click();
  await expect(page.getByRole('combobox', { name: 'Workspace', exact: true })).toHaveValue('beta');
  await expect(page.locator('.panel-console')).toBeVisible();
  await expect(page.locator('.panel-console .command-picker select')).toHaveValue('beta-command');

  // A phone: one column, nothing scrolls sideways.
  await page.getByRole('combobox', { name: 'Workspace', exact: true }).selectOption('alpha');
  await expect(page.locator('.panel-processes')).toBeVisible();
  await page.setViewportSize({ width: 390, height: 844 });
  await page.getByRole('navigation', { name: 'Workspace panels' }).getByRole('button', { name: 'Processes', exact: true }).click();
  await expect(panel.getByRole('listitem', { name: 'go test ./..., Running' })).toBeVisible();
  assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false, 'no horizontal overflow on a phone');
  await page.screenshot({ path: '.cache/processes-phone.png' });

  await drainPageErrors(errors);assert.deepEqual(errors, []);
  console.log('Processes panel: listed, opened, copied, stopped, cleaned up; a background lazybox run listed with its port and stopped; hosting chosen and its fallback explained.');
} catch (error) {
  for (const p of browser?.contexts().flatMap(c => c.pages()) || []) await p.screenshot({ path: '.cache/processes-failure.png', fullPage: true }).catch(() => {});
  throw error;
} finally {
  await browser?.close(); await server.close(); await fixture.close();
}
