// Settings → Pricing and Subscriptions, against the fake Runner (no provider
// login, no inference). Pricing is one table per provider that never scrolls
// the page sideways or lets one column's text run under the next, at desktop
// and phone width; search, sort, edit, add and remove-with-confirmation work.
// A subscription at its limit says when it is back ("Back at … (in …)"); one
// whose reported reset has passed says it is checking, refreshes itself once,
// and shows what the provider reports now. The sections move with the arrow keys.
import { createServer } from 'vite';
import react from '@vitejs/plugin-react';
import { chromium, expect } from './fixtures/playwright.mjs';
import { fileURLToPath } from 'node:url';
import { mkdir } from 'node:fs/promises';
import assert from 'node:assert/strict';
import { startRunnerFixture, defaultModelPrices } from './fixtures/runner-ui.mjs';
import { collectPageErrors } from './fixtures/page-errors.mjs';

const fixture = await startRunnerFixture();
// A ~/.zshenv that puts V1's lazybox first: the Runner's shell integration fixed it.
fixture.versionInfo.agentCli = { cli: '/Applications/Lazybox.app/Contents/Resources/runtime/bin/lazybox', shell: '/bin/zsh', shadow: '/Users/me/.cargo/bin/lazybox', shadowIn: ['zsh -c', 'zsh -lc'], unfixed: [], fixed: true, problem: '', checkedAtUnixMillis: String(Date.now()), notice: 'Your shell startup files put another lazybox first on PATH (/Users/me/.cargo/bin/lazybox, in zsh -c, zsh -lc). Lazybox puts its own back first in every agent and workspace terminal shell, so agents run this Lazybox\'s lazybox; nothing to do.' };
fixture.runnerSettings.prices = defaultModelPrices();
const now = Math.floor(Date.now() / 1000);
const limited = (id, label, resetAt, extra = {}) => ({ id, provider: 'claude', label, managed: true, isolation: 'environment', availability: { state: 'exhausted', plan: 'max', observedAtUnixMillis: String(Date.now() - 600_000), windows: [{ name: 'five_hour', durationMinutes: '300', usedPercent: 100, resetAt: String(resetAt) }, { name: 'seven_day', durationMinutes: '10080', usedPercent: 64, resetAt: String(now + 300_000) }], ...extra } });
const backAt = now + 2 * 3600 + 10 * 60;
// After 21:50 local time the reset is past midnight, and the line names the day.
const backDay = new Date(backAt * 1000).toDateString() === new Date().toDateString() ? '' : 'tomorrow ';
fixture.profiles.push(
  limited('claude-future', 'Limited until later', backAt),
  limited('claude-past', 'Limit already over', now - 120),
  { id: 'claude-unknown', provider: 'claude', label: 'Limit without a time', managed: true, isolation: 'environment', availability: { state: 'exhausted', observedAtUnixMillis: String(Date.now()) } },
);
const server = await createServer({ configFile: false, root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: fixture.url } } } });
const refreshes = id => fixture.calls.filter(c => c.method === 'RefreshSubscription' && c.body.profileId === id).length;
let browser, page;
try {
  await server.listen(); browser = await chromium.launch({ headless: true });
  page = await browser.newPage({ viewport: { width: 1440, height: 1000 } });
  const errors = []; await collectPageErrors(page, errors);
  await mkdir('.cache', { recursive: true });
  // Hold the automatic refresh, so "checking" can be seen before the answer.
  let release; const held = new Promise(resolve => { release = resolve; });
  await page.route('**/RefreshSubscription', async route => { if (route.request().postDataJSON().profileId === 'claude-past') await held; const id = route.request().postDataJSON().profileId;
    if (id === 'claude-future' || id === 'claude-unknown') {
      return route.fulfill({ json: fixture.profiles.find(profile => profile.id === id) });
    }
    await route.continue(); });
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace#token=${fixture.token}`);
  await page.getByRole('button', { name: 'Settings', exact: true }).click();

  // Subscriptions: when a limited one is back, in one line.
  const future = page.getByRole('article', { name: 'Limited until later subscription', exact: true });
  await expect(future.locator('.subscription-back')).toHaveText(new RegExp(`^Back ${backDay}at \\d{1,2}:\\d{2}\\s?[AP]M \\(in 2h (9|10)m\\)$`));
  await expect(future.locator('.account-state')).toHaveText('Limit reached');
  const past = page.getByRole('article', { name: 'Limit already over subscription', exact: true });
  await expect(past.locator('.subscription-back')).toHaveText('Should be available again — checking…');
  await expect(past.locator('.usage-window').first()).toContainText('Reset');
  await page.screenshot({ path: '.cache/settings-polish-checking.png', fullPage: true });
  release();
  await expect(past.locator('.account-state')).toHaveText('Available');
  await expect(past.locator('.subscription-back')).toHaveCount(0);
  await expect(past).toContainText('42% used');
  await page.waitForTimeout(1500);
  assert.equal(refreshes('claude-past'), 1, 'a passed reset is checked once, by itself');
  assert.equal(refreshes('claude-future'), 0, 'a limit still running is not refreshed');
  await expect(page.getByRole('article', { name: 'Limit without a time subscription', exact: true }).locator('.subscription-back')).toContainText('reset time not reported');
  await expect(page.getByRole('article', { name: 'Saved Codex subscription', exact: true }).locator('.subscription-next-reset')).toHaveText(/^Weekly limit resets .+ \(in 2d\)$/);
  await expect(page.locator('.settings-page')).not.toContainText('Reported window has ended');
  await page.screenshot({ path: '.cache/settings-polish-subscriptions.png', fullPage: true });

  // The sections are a tab list the arrow keys walk.
  await page.getByRole('tab', { name: 'Subscriptions', exact: true }).focus();
  await page.keyboard.press('ArrowDown');
  await expect(page.getByRole('tab', { name: 'Pricing', exact: true })).toHaveAttribute('aria-selected', 'true');
  await expect(page.getByRole('tab', { name: 'Pricing', exact: true })).toBeFocused();

  // Pricing: one table per provider, nothing overlapping, nothing sideways.
  const layout = async width => {
    await page.setViewportSize({ width, height: 1000 });
    await page.waitForTimeout(200);
    assert.ok(await page.locator('.settings-page').evaluate(n => n.scrollWidth <= n.clientWidth), `pricing scrolls sideways at ${width}px`);
    const problems = await page.locator('.price-table').evaluateAll(tables => tables.flatMap(table => [...table.rows].flatMap(row => {
      const cells = [...row.cells].filter(c => getComputedStyle(c).display !== 'none');
      const out = [];
      cells.forEach((cell, i) => {
        const box = cell.getBoundingClientRect(), next = cells[i + 1]?.getBoundingClientRect();
        if (next && box.right > next.left + 0.5) out.push(`${row.rowIndex}:${i} overlaps the next column`);
        // Text runs only to its own cell's edge: numbers always fit; a long model ID ends in an ellipsis.
        for (const el of [cell, ...cell.querySelectorAll('span,small')]) {
          if (el.scrollWidth <= el.clientWidth + 1 || el.closest('.sr-only') || el.textContent === el.querySelector('.sr-only')?.textContent) continue;
          const style = getComputedStyle(el);
          if (cell.classList.contains('num') || style.textOverflow !== 'ellipsis' || style.overflow === 'visible') out.push(`${row.rowIndex}:${i} "${el.textContent}" overflows`);
        }
        if (box.right > table.getBoundingClientRect().right + 0.5) out.push(`${row.rowIndex}:${i} leaves the table`);
      });
      return out;
    })));
    assert.deepEqual(problems, [], `pricing table at ${width}px`);
  };
  const claude = page.getByRole('table', { name: 'Claude model prices', exact: true });
  await expect(claude.locator('tbody tr')).toHaveCount(9);
  await expect(page.getByRole('table', { name: 'Codex model prices', exact: true }).locator('tbody tr')).toHaveCount(2);
  await expect(claude.locator('tbody tr').first()).toContainText('claude-fable-5-1');
  await expect(claude.locator('tbody tr').first()).toContainText('$10.00');
  await expect(claude.locator('tbody tr').first()).toContainText('Nov 3, 2026');
  await expect(page.locator('.pricing-settings')).not.toContainText('anthropic-standard');
  await layout(1440); await page.screenshot({ path: '.cache/settings-polish-pricing-1440.png', fullPage: true });
  await layout(900);
  await layout(480); await page.screenshot({ path: '.cache/settings-polish-pricing-480.png', fullPage: true });
  await layout(360);
  await page.setViewportSize({ width: 1440, height: 1000 });

  // Search and sort.
  await page.getByRole('searchbox', { name: 'Search model prices' }).fill('sonnet');
  await expect(claude.locator('tbody tr')).toHaveCount(3);
  await expect(page.locator('.pricing-count')).toHaveText('3 of 11');
  await expect(page.getByRole('table', { name: 'Codex model prices' })).toHaveCount(0);
  await page.getByRole('searchbox', { name: 'Search model prices' }).fill('');
  await claude.getByRole('button', { name: 'Output' }).click();
  await expect(claude.getByRole('columnheader', { name: 'Output' })).toHaveAttribute('aria-sort', 'descending');
  await expect(claude.locator('tbody tr').first()).toContainText('claude-fable-5-1');
  await expect(claude.locator('tbody tr').last()).toContainText(/claude-sonnet-5/);

  // Edit a list price: it becomes your own.
  await claude.getByRole('button', { name: 'Edit claude-opus-5 price', exact: true }).click();
  let dialog = page.getByRole('dialog', { name: 'Edit model price' });
  await expect(dialog.getByLabel('Exact model ID')).toHaveValue('claude-opus-5');
  await expect(dialog.getByLabel('Input', { exact: true })).toHaveValue('5');
  await expect(dialog).toContainText('Saving makes this your own price');
  await page.screenshot({ path: '.cache/settings-polish-price-dialog.png' });
  await dialog.getByLabel('Input', { exact: true }).fill('4.5');
  await dialog.getByRole('button', { name: 'Save model price', exact: true }).click();
  await expect(dialog).toHaveCount(0);
  await expect(page.getByRole('status').filter({ hasText: 'Model price saved.' })).toBeVisible();
  let saved = fixture.mutations.findLast(m => m.method === 'SetModelPrice').body;
  assert.equal(saved.price.model, 'claude-opus-5'); assert.equal(saved.price.input, '4500'); assert.equal(saved.price.output, '25000'); assert.ok(!saved.remove);
  await expect(claude.locator('tbody tr', { hasText: 'claude-opus-5' }).filter({ hasText: 'Custom' })).toContainText('$4.50');

  // Invalid input is a sentence in the dialog, and nothing is sent.
  const before = fixture.mutations.length;
  await page.getByRole('button', { name: 'Add model price', exact: true }).click();
  dialog = page.getByRole('dialog', { name: 'Add model price' });
  await expect(dialog.getByLabel('Exact model ID')).toBeFocused();
  await dialog.getByLabel('Exact model ID').fill('claude-new-1');
  await dialog.getByLabel('Input', { exact: true }).fill('1.2345');
  await dialog.getByLabel('Output', { exact: true }).fill('6');
  await dialog.getByRole('button', { name: 'Save model price', exact: true }).click();
  await expect(dialog.getByRole('alert')).toHaveText(/at most three decimals/);
  assert.equal(fixture.mutations.length, before);
  // Add a model.
  await dialog.getByLabel('Input', { exact: true }).fill('1.25');
  await dialog.getByRole('button', { name: 'Save model price', exact: true }).click();
  await expect(dialog).toHaveCount(0);
  await expect(claude.locator('tbody tr', { hasText: 'claude-new-1' })).toContainText('$1.25');
  saved = fixture.mutations.findLast(m => m.method === 'SetModelPrice').body;
  assert.deepEqual([saved.price.model, saved.price.input, saved.price.output], ['claude-new-1', '1250', '6000']);

  // Removing your own price asks first; Keep it changes nothing.
  await claude.getByRole('button', { name: 'Edit claude-new-1 price', exact: true }).click();
  dialog = page.getByRole('dialog', { name: 'Edit model price' });
  await dialog.getByRole('button', { name: 'Remove custom price…', exact: true }).click();
  await expect(dialog.getByRole('group', { name: 'Remove custom price' })).toContainText('Remove your price for claude-new-1?');
  await page.screenshot({ path: '.cache/settings-polish-price-remove.png' });
  const beforeRemove = fixture.mutations.length;
  await dialog.getByRole('button', { name: 'Keep it', exact: true }).click();
  assert.equal(fixture.mutations.length, beforeRemove);
  await dialog.getByRole('button', { name: 'Remove custom price…', exact: true }).click();
  await dialog.getByRole('button', { name: 'Remove custom price', exact: true }).click();
  await expect(dialog).toHaveCount(0);
  await expect(claude.locator('tbody tr', { hasText: 'claude-new-1' })).toHaveCount(0);
  assert.equal(fixture.mutations.findLast(m => m.method === 'SetModelPrice').body.remove, true);
  // Escape closes the editor without saving.
  await claude.getByRole('button', { name: 'Edit claude-fable-5-1 price', exact: true }).click();
  await page.keyboard.press('Escape');
  await expect(page.getByRole('dialog', { name: 'Edit model price' })).toHaveCount(0);

  // About says which lazybox agents reach, and what Lazybox did about another one first on PATH.
  await page.getByRole('tab', { name: 'About & updates', exact: true }).click();
  const agents = page.locator('.settings-page').getByText('lazybox for agents', { exact: true }).locator('xpath=ancestor::*[contains(concat(" ",normalize-space(@class)," ")," settings-row ")][1]');
  await expect(agents).toContainText('/Users/me/.cargo/bin/lazybox');
  await expect(agents).toContainText('nothing to do');
  await expect(agents.getByRole('status')).toHaveText('Fixed');
  fixture.versionInfo.agentCli = { ...fixture.versionInfo.agentCli, fixed: false, unfixed: ['sh -lc finds /usr/local/bin/lazybox'], notice: 'Agents\' shells reach another lazybox before this Lazybox\'s (sh -lc finds /usr/local/bin/lazybox), even with Lazybox\'s shell integration. Agents are told to call it by its full path.' };
  await page.getByRole('tab', { name: 'Runner', exact: true }).click();
  await page.getByRole('tab', { name: 'About & updates', exact: true }).click();
  await expect(agents.getByRole('status')).toHaveText('Needs full path');
  await expect(agents).toContainText('sh -lc finds /usr/local/bin/lazybox');

  // Every section fits a phone without sideways scrolling.
  await page.setViewportSize({ width: 390, height: 844 });
  for (const tab of ['Subscriptions', 'Pricing', 'Repositories', 'Agents & workspaces', 'Workspaces', 'Process', 'Notifications', 'Shortcuts', 'Devices', 'Runner', 'About & updates']) {
    await page.getByRole('tab', { name: tab, exact: true }).click();
    await page.waitForTimeout(250);
    assert.ok(await page.locator('.settings-page').evaluate(n => n.scrollWidth <= n.clientWidth), `${tab} scrolls sideways on a phone`);
    await expect(page.getByRole('tab', { name: tab, exact: true })).toBeInViewport();
  }
  assert.deepEqual(errors, []);
  console.log('PASS: pricing table (no overlap, no sideways scroll at 1440/900/480/360px, search, sort, edit, add, confirmed remove), subscription "back at" line, one automatic refresh after a passed reset, About says which lazybox agents reach (fixed / needs full path), keyboard section navigation, phone layout of every section. Fake Runner only.');
} catch (error) { await page?.screenshot({ path: '.cache/settings-polish-failure.png', fullPage: true }); throw error; }
finally { await browser?.close(); await server.close(); await fixture.close(); }
