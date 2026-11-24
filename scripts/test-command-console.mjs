// Runs stay in the command list until the user opens the console.
// An open console can follow new runs while the user is idle.
import {testViteCache} from './fixtures/vite-cache.mjs';
import { createServer } from './fixtures/vite-cache.mjs';
import react from '@vitejs/plugin-react';
import { chromium, expect } from './fixtures/playwright.mjs';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';
import { collectPageErrors } from './fixtures/page-errors.mjs';

const fixture = await startRunnerFixture();
const server = await createServer({...testViteCache(import.meta.url), configFile: false, root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: fixture.url } } } });
let browser;
try {
  await server.listen();
  browser = await chromium.launch({ headless: true });
  const page = await (await browser.newContext({ viewport: { width: 1400, height: 1000 } })).newPage();
  const errors = []; await collectPageErrors(page, errors);
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace#token=${fixture.token}`);
  await page.getByRole('combobox', { name: 'Workspace', exact: true }).selectOption('alpha');
  await page.getByRole('combobox', { name: 'Session', exact: true }).selectOption('alpha-session');
  const chat = page.locator('[data-panel-id="alpha-work-chat"]');
  const composer = chat.getByRole('textbox', { name: 'Message', exact: true });
  await expect(composer).toBeEnabled();
  // Command output is a dock group, minimized to its tab until the person opens it: its tab opens it,
  // its group menu minimizes it.
  const output = page.locator('.dv-groupview').filter({ has: page.locator('[data-kind="console"]') });
  const minimized = () => output.evaluate(node => node.classList.contains('dock-group-minimized'));
  const show = { click: () => page.getByRole('tab', { name: 'Command output', exact: true }).click() };
  const hide = { click: async () => { await page.getByRole('tab', { name: 'Command output', exact: true }).click({ button: 'right' }); await page.getByRole('menuitem', { name: 'Minimize', exact: true }).click(); } };
  const lists = () => fixture.calls.filter(call => call.method === 'ListCommands' && call.body.sessionId === 'alpha-session').length;

  // A command that existed before the session was viewed does not open the console.
  const baseline = lists();
  await expect.poll(lists, { timeout: 5000 }).toBeGreaterThan(baseline + 1);
  await expect.poll(minimized).toBe(true);

  // A run starts while the user is typing and keeps running: the console stays folded until explicitly opened.
  await composer.click(); await composer.fill('typing while the agent runs tests');
  const run = fixture.streamCommand('alpha-session', 'go test ./...');
  run.startedAtUnixMillis = String(Date.now() - 150_000);
  run.write('=== RUN   TestParse\n');
  const starting = lists();
  await expect.poll(lists, { timeout: 8000 }).toBeGreaterThan(starting + 2);
  await expect.poll(minimized).toBe(true);
  await expect(composer).toBeFocused();
  await show.click();
  await expect(page.locator('.command-panel select')).toHaveValue(run.id);
  run.report = { exitCode: 1, outputBytes: '1468006', agentBytes: '2150', filter: 'go-test', outputPath: '/state/command-output/alpha-session/go-1.log' };
  run.finish(1);
  const report = page.getByTestId('command-report');
  await expect(report).toContainText('Exit 1 · the agent got a 2.1 KiB summary of 1.4 MiB (go-test filter)');
  await expect(page.locator('.command-panel select')).toHaveValue(run.id);
  await expect(page.locator('.command-panel .console-state [role=status]')).toHaveText('Failed · exit 1');
  await expect(composer).toHaveValue('typing while the agent runs tests');
  await expect(page.locator('.command-panel select option:checked')).toContainText(/2 min/);
  const commandTime = page.locator('.command-history-time');
  await expect(commandTime).toContainText(/2 min/);
  await commandTime.click();
  await expect(commandTime).toHaveAttribute('aria-pressed','true');
  assert.equal(await commandTime.textContent(), await commandTime.getAttribute('title'));
  // The docked console is output, not chrome: the output keeps most of the panel and ends inside it.
  {
    const panel = await page.locator('.command-panel').boundingBox(), output = await page.getByTestId('console-output').boundingBox();
    assert.ok(output.height >= panel.height * 0.55, `the output gets most of the panel (${Math.round(output.height)} of ${Math.round(panel.height)}px)`);
    assert.ok(output.y + output.height <= panel.y + panel.height + 1, 'the output ends inside the panel');
    assert.ok(panel.y + panel.height <= page.viewportSize().height + 1, 'the panel ends inside the window');
    // No raw command id is shown to people.
    await expect(page.locator('.command-panel')).not.toContainText(run.id.slice(0, 12));
  }

  // Hidden by the user, it stays hidden: the next run does not reopen it.
  await hide.click();
  await expect.poll(minimized).toBe(true);
  const next = fixture.startRun('alpha-session', 'npm test', { exitCode: 0, outputBytes: '120', agentBytes: '180', filter: 'passthrough', outputPath: '' });
  const closed = lists();
  await expect.poll(lists, { timeout: 5000 }).toBeGreaterThan(closed + 2);
  await expect.poll(minimized).toBe(true);

  // Opened again by the user, it shows the newest run and follows runs again.
  await show.click();
  await expect(page.locator('.command-panel select')).toHaveValue(next.id);
  await expect(report).toContainText('Exit 0 · the agent got all of it (120 B)');
  await expect(report.getByRole('button', { name: 'Copy log path' })).toHaveCount(0);
  await hide.click();
  await show.click();
  const third = fixture.streamCommand('alpha-session', 'make lint');
  third.write('linting\n');
  await expect(page.locator('.command-panel select')).toHaveValue(third.id, { timeout: 8000 });
  await expect.poll(minimized).toBe(false);
  third.finish(0);

  // × closes the panel: its control is plainly visible at rest, and a later run does not bring the panel back.
  const consolePanel = page.locator('.panel-console');
  const close = page.getByRole('button', { name: 'Close Command output panel', exact: true });
  assert.ok(Number(await close.evaluate(node => getComputedStyle(node).opacity)) >= 0.8, 'the close control is not faint at rest');
  await close.click();
  await expect(consolePanel).toHaveCount(0);
  const fourth = fixture.startRun('alpha-session', 'make build', { exitCode: 0, outputBytes: '70', agentBytes: '70', filter: 'passthrough', outputPath: '' });
  const removed = lists();
  await expect.poll(lists, { timeout: 5000 }).toBeGreaterThan(removed + 2);
  await expect(consolePanel).toHaveCount(0);
  // Closing does not survive only until a reload either.
  await page.reload();
  await expect(composer).toBeEnabled();
  await expect(consolePanel).toHaveCount(0);

  // The way back: the Panels menu lists it, and it opens on the newest run.
  await page.locator('.workspace-header-actions summary[aria-label="Panels"]').click();
  await page.getByRole('button', { name: 'Show Command output', exact: true }).click();
  await expect(consolePanel).toHaveCount(1);
  await expect(page.locator('.command-panel select')).toHaveValue(fourth.id);

  // From the keyboard: ]] o puts it away and brings it back.
  await page.locator('.workspace-brand').click();
  await page.keyboard.type(']]o');
  await expect.poll(minimized).toBe(true);
  await page.keyboard.type(']]o');
  await expect.poll(minimized).toBe(false);

  // A run in another session leaves this console alone.
  await hide.click();
  fixture.startRun('beta-session', 'cargo test', { exitCode: 0, outputBytes: '10', agentBytes: '50', filter: 'passthrough' });
  const other = lists();
  await expect.poll(lists, { timeout: 5000 }).toBeGreaterThan(other + 1);
  await expect.poll(minimized).toBe(true);

  await page.context().close();

  // Each case below starts in a window of its own: a fresh view, nothing used yet.
  const base = `http://127.0.0.1:${server.httpServer.address().port}/?view=workspace#token=${fixture.token}`;
  const fresh = async () => {
    const view = await (await browser.newContext({ viewport: { width: 1400, height: 1000 } })).newPage();
    await collectPageErrors(view, errors);
    await view.goto(base);
    await view.getByRole('combobox', { name: 'Workspace', exact: true }).selectOption('alpha');
    await view.getByRole('combobox', { name: 'Session', exact: true }).selectOption('alpha-session');
    await expect(view.locator('[data-panel-id="alpha-work-chat"]').getByRole('textbox', { name: 'Message', exact: true })).toBeEnabled();
    const group = view.locator('.dv-groupview').filter({ has: view.locator('[data-kind="console"]') });
    return { view, picker: view.locator('.command-panel select'), chip: view.locator('.command-panel .console-newer'), folded: () => group.evaluate(node => node.classList.contains('dock-group-minimized')) };
  };
  // A long run: still going when the console next looks (1.5 s on), with output to read.
  const longRun = (title, lines = 3) => { const command = fixture.streamCommand('alpha-session', title); command.write(Array.from({ length: lines }, (_, i) => `${title} line ${i + 1}`).join('\n') + '\n'); return command; };
  const quickRun = title => fixture.startRun('alpha-session', title, { exitCode: 0, outputBytes: '40', agentBytes: '40', filter: 'passthrough', outputPath: '' });
  // Long enough for a run to be listed, and then looked at again as a long run.
  const settle = view => view.waitForTimeout(4500);

  // An agent fires five quick runs while the person reads a finished one: the console stays, and offers them.
  {
    const { view, picker, chip, folded } = await fresh();
    await view.getByRole('tab', { name: 'Command output', exact: true }).click();
    const reading = longRun('go build ./...');
    await expect.poll(folded, { timeout: 8000 }).toBe(false);
    await expect(picker).toHaveValue(reading.id);
    reading.finish(0);
    await expect(view.locator('.command-panel .console-state [role=status]')).toHaveText('Finished');
    const quick = [];
    for (const n of [1, 2, 3, 4, 5]) { quick.push(quickRun(`npm install --no-audit step-${n}`)); await view.waitForTimeout(300); }
    await settle(view);
    await expect(picker).toHaveValue(reading.id);
    await expect(chip).toContainText('5 newer');
    await expect(chip).toContainText('npm install --no-audit step-5');
    // The picker lists them newest first.
    assert.equal(await picker.locator('option').nth(1).getAttribute('value'), quick[4].id);

    // A long run starting while the run shown is finished and nobody used the console: it takes over.
    const tests = longRun('npm test');
    await expect(picker).toHaveValue(tests.id, { timeout: 8000 });
    await expect(chip).toHaveCount(0);
    assert.match(await picker.locator('option:checked').textContent(), /^● npm test/, 'running runs are marked');
    tests.finish(0);

    // Two more quick ones: offered; the chip, from the keyboard, shows the newest.
    quickRun('npm run lint'); const last = quickRun('npm run format');
    await settle(view);
    await expect(picker).toHaveValue(tests.id);
    await expect(chip).toContainText('2 newer');
    await chip.focus(); await view.keyboard.press('Enter');
    await expect(picker).toHaveValue(last.id);
    await expect(chip).toHaveCount(0);
    await view.context().close();
  }

  // A long run starting while the person scrolled the output in the last 30 s: no takeover, the chip offers it.
  {
    const { view, picker, chip, folded } = await fresh();
    await view.getByRole('tab', { name: 'Command output', exact: true }).click();
    const reading = longRun('cargo test', 400);
    await expect.poll(folded, { timeout: 8000 }).toBe(false);
    await expect(picker).toHaveValue(reading.id);
    reading.finish(0);
    const output = view.getByTestId('console-output');
    await expect(output).toContainText('cargo test line 400');
    await output.hover(); await view.mouse.wheel(0, -600);
    const next = longRun('cargo bench');
    await settle(view);
    await expect(picker).toHaveValue(reading.id);
    await expect(chip).toContainText('1 newer');
    await expect(chip).toContainText('cargo bench');
    next.finish(0);
    await view.context().close();
  }

  // Behind another tab of its group, the console is never brought forward by a run.
  {
    const { view, folded } = await fresh();
    await view.getByRole('tab', { name: 'Command output', exact: true }).click();
    await expect.poll(folded).toBe(false);
    await view.locator('summary[aria-label="New tab in the Command output group"]').click();
    await view.getByRole('button', { name: 'Open Usage & cost here', exact: true }).click();
    await expect(view.locator('.panel-usage')).toBeVisible();
    await expect(view.locator('.panel-console')).toBeHidden();
    const behind = longRun('docker compose build');
    quickRun('npm ci');
    await settle(view);
    await expect(view.locator('.panel-usage')).toBeVisible();
    await expect(view.locator('.panel-console')).toBeHidden();
    behind.finish(0);
    await view.context().close();
  }

  // Follow new runs off: the console never switches by itself, it only offers; the choice is remembered.
  {
    const { view, picker, chip, folded } = await fresh();
    await view.getByRole('tab', { name: 'Command output', exact: true }).click();
    await expect.poll(folded).toBe(false);
    await expect(picker).not.toHaveValue('');
    const shown = await picker.inputValue();
    const menu = view.locator('.command-panel summary[aria-label="Output actions"]');
    await menu.click();
    const toggle = view.getByRole('button', { name: 'Follow new runs', exact: true });
    await expect(toggle).toHaveAttribute('aria-pressed', 'true');
    await toggle.click();
    const ignored = longRun('make release');
    await settle(view);
    await expect(picker).toHaveValue(shown);
    await expect(chip).toContainText('1 newer');
    ignored.finish(0);
    await view.reload();
    await view.getByRole('tab', { name: 'Command output', exact: true }).click().catch(() => {});
    await menu.click();
    await expect(view.getByRole('button', { name: 'Follow new runs', exact: true })).toHaveAttribute('aria-pressed', 'false');
    await view.keyboard.press('Escape');

    // The header is one row at a 700 px panel: lesser controls move into ⋯, the rest (the pop-out too, where there is one) stays whole.
    quickRun('npm run typecheck');
    await settle(view);
    await expect(chip).toContainText('1 newer');
    // Docked under the conversation (a window wide enough to be the desktop layout, not the phone one).
    const panelWidth = async () => (await view.locator('.command-panel').boundingBox()).width;
    await view.setViewportSize({ width: 880, height: 1000 });
    await view.waitForTimeout(200);
    for (let i = 0; i < 4 && Math.abs(await panelWidth() - 700) > 1; i++) {
      await view.setViewportSize({ width: Math.round(view.viewportSize().width - (await panelWidth() - 700)), height: 1000 });
      await view.waitForTimeout(200);
    }
    assert.ok(Math.abs(await panelWidth() - 700) <= 1, `the panel is 700 px wide (${await panelWidth()})`);
    await expect(chip).toBeVisible();
    const tools = view.locator('.command-panel .console-tools');
    const items = await tools.evaluate(node => [...node.children].filter(child => child.getClientRects().length).map(child => { const box = child.getBoundingClientRect(); return { name: String(child.className || child.tagName), top: Math.round(box.top), bottom: Math.round(box.bottom), right: Math.round(box.right) }; }));
    const row = await tools.boundingBox();
    for (const item of items) {
      assert.ok(item.top >= row.y - 1 && item.bottom <= row.y + row.height + 1, `${item.name} sits in the header's one row: ${JSON.stringify({ item, row })}`);
      assert.ok(item.right <= row.x + row.width + 1, `${item.name} is not cut off at the right: ${JSON.stringify({ item, row })}`);
    }
    assert.ok(row.height <= 44, `the header is one row (${row.height}px)`);
    // Every control left in the row is whole (the loop above), the ⋯ menu last among them; A−/A+ wait in it.
    assert.match(items.at(-1).name, /console-menu/);
    // The desktop app adds the pop-out icon (26 px and a gap): the picker still has that much to give.
    assert.ok((await view.locator('.command-panel .command-picker').boundingBox()).width >= 72 + 34, 'room for the pop-out icon in the row');
    await expect(tools.getByRole('group', { name: 'Output text size' })).toBeHidden();
    await view.screenshot({ path: '.cache/console-header-700.png' });
    await menu.click();
    await expect(view.getByRole('button', { name: /^Smaller text/ })).toBeVisible();
    await view.keyboard.press('Escape');
    await view.context().close();
  }

  assert.equal(fixture.mutations.length, 0, 'opening the console must not mutate execution');
  assert.equal(errors.length, 0, errors.join('\n'));
  console.log('PASS: the console stays folded for a long run until explicitly opened, labels the run with what its agent received, stays put away (hidden, closed with ×, or from the keyboard) until the user opens it again, offers quick runs instead of cycling through them, takes over only a finished run nobody used, never brings its tab forward, stays put with Follow new runs off, and keeps its header to one row. Explicitly fake RPC fixture.');
} finally {
  await browser?.close(); await server.close(); await fixture.close();
}
