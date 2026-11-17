// Screenshots of every Settings tab and the other full pages, at desktop and
// phone width, for a person to look at: `node scripts/polish-screenshots.mjs
// [label]` writes .cache/polish-<label>-<page>-<width>.png. Fake Runner only.
import { createServer } from 'vite';
import react from '@vitejs/plugin-react';
import { chromium, expect } from '@playwright/test';
import { fileURLToPath } from 'node:url';
import { mkdir } from 'node:fs/promises';
import { startRunnerFixture, defaultModelPrices } from './fixtures/runner-ui.mjs';

const label = process.argv[2] || 'after';
const fixture = await startRunnerFixture();
fixture.runnerSettings.prices = defaultModelPrices();
const now = Math.floor(Date.now() / 1000);
fixture.profiles.push(
  { id: 'claude-work', provider: 'claude', label: 'Work Claude', managed: true, isolation: 'environment', availability: { state: 'exhausted', plan: 'max', email: 'work@example.invalid', resetAt: String(now + 7800), observedAtUnixMillis: String(Date.now()), windows: [{ name: 'five_hour', durationMinutes: '300', usedPercent: 100, resetAt: String(now + 7800) }, { name: 'seven_day', durationMinutes: '10080', usedPercent: 64, resetAt: String(now + 300000) }] } },
);
const server = await createServer({ configFile: false, root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: fixture.url } } } });
let browser;
try {
  await server.listen(); browser = await chromium.launch({ headless: true });
  await mkdir('.cache', { recursive: true });
  const base = `http://127.0.0.1:${server.httpServer.address().port}`;
  for (const [width, height] of [[1440, 1000], [480, 900]]) {
    const page = await browser.newPage({ viewport: { width, height } });
    const shot = name => page.screenshot({ path: `.cache/polish-${label}-${name}-${width}.png`, fullPage: true });
    await page.goto(`${base}/?view=workspace&collection=alpha#token=${fixture.token}`);
    await page.waitForTimeout(800);
    await shot('workspace-home');
    const settings = page.getByRole('button', { name: 'Settings', exact: true });
    if (!await settings.isVisible()) await page.getByRole('button', { name: /navigator|Workspaces/i }).first().click().catch(() => {});
    await settings.click();
    for (const tab of ['Subscriptions', 'Repositories', 'Agents & workspaces', 'Pricing', 'Notifications', 'Shortcuts', 'Process', 'Runner', 'Devices', 'Workspaces']) {
      const button = page.getByRole('tab', { name: tab, exact: true });
      if (!await button.count()) continue;
      await button.click(); await page.waitForTimeout(500);
      await shot(`settings-${tab.toLowerCase().replace(/[^a-z]+/g, '-')}`);
    }
    await page.goto(`${base}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`);
    await expect(page.getByRole('textbox', { name: 'Message', exact: true }).first()).toBeVisible();
    const inbox = page.locator('.inbox-action');
    if (await inbox.isVisible()) { await inbox.click(); await page.waitForTimeout(400); await shot('inbox'); await page.keyboard.press('Escape'); }
    const panelsMenu = page.locator('summary[aria-label="Panels"]');
    if (await panelsMenu.isVisible()) {
      await panelsMenu.click(); await page.getByRole('button', { name: 'Show Processes', exact: true }).click();
      await page.waitForTimeout(500); await shot('processes');
    }
    await page.close();
  }
  console.log(`Wrote .cache/polish-${label}-*.png`);
} finally { await browser?.close(); await server.close(); await fixture.close(); }
