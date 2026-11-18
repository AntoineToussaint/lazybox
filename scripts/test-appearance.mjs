// Appearance, against the fake Runner: the key pages in Dark and in Light
// (screenshots in .cache/appearance-*.png) use only theme tokens for every
// colour on screen and keep body text at 4.5:1 or better; System follows the
// computer; Settings → Appearance changes theme, accent, contrast, chat text,
// density and motion live and keeps them across a reload; a user theme file
// in the Runner's themes folder is listed, applied, hot reloaded when edited,
// and its mistakes are shown in Settings. Fixture only: no provider.
import { createServer } from 'vite';
import react from '@vitejs/plugin-react';
import { chromium, expect } from './fixtures/playwright.mjs';
import { fileURLToPath } from 'node:url';
import { mkdir } from 'node:fs/promises';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';
import { testViteCache } from './fixtures/vite-cache.mjs';
import { seedConversation, visitKeyPages } from './fixtures/appearance-pages.mjs';
import { TOKENS } from '../packages/surfaces/src/appearance/tokens.ts';

const fixture = await startRunnerFixture();
seedConversation(fixture);
const server = await createServer({ ...testViteCache(import.meta.url), configFile: false, logLevel: 'error', root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: fixture.url } } } });
const tokenNames = TOKENS.filter(t => t.kind === 'color').map(t => t.name);

/**
 * Every visible element's text, background and border colour must be a theme
 * token (or a token made translucent). Theme previews and swatches show other
 * themes' colours on purpose and are skipped.
 */
async function untokenizedColours(page) {
  return page.evaluate(names => {
    const probe = document.createElement('i'); document.body.appendChild(probe);
    const rgb = value => { const m = /(?:rgba?\(|color\(srgb\s+)([^)]*)\)/.exec(value); if (!m) return null; const parts = m[1].split(/[\s,/]+/).filter(Boolean).map(Number); const scale = value.startsWith('color(') ? 255 : 1; return { r: Math.round(parts[0] * scale), g: Math.round(parts[1] * scale), b: Math.round(parts[2] * scale), a: parts[3] ?? 1 }; };
    const tokens = new Set(names.map(n => { probe.style.color = `var(--${n})`; const c = rgb(getComputedStyle(probe).color); return c && `${c.r},${c.g},${c.b}`; }));
    probe.remove();
    const near = c => { for (const key of tokens) { const [r, g, b] = key.split(',').map(Number); if (Math.abs(r - c.r) <= 1 && Math.abs(g - c.g) <= 1 && Math.abs(b - c.b) <= 1) return true; } return false; };
    const out = new Set();
    for (const el of document.body.querySelectorAll('*')) {
      if (el.closest('.theme-preview,.accent-swatch,.accent-custom,.token-swatch,.token-override,img,canvas,iframe,.qr-code,svg')) continue;
      const box = el.getBoundingClientRect();
      if (!box.width || !box.height) continue;
      const style = getComputedStyle(el);
      if (style.visibility === 'hidden' || style.display === 'none') continue;
      const checks = [['color', style.color]];
      checks.push(['background', style.backgroundColor]);
      for (const side of ['Top', 'Right', 'Bottom', 'Left']) if (parseFloat(style[`border${side}Width`]) > 0 && style[`border${side}Style`] !== 'none') checks.push(['border', style[`border${side}Color`]]);
      for (const [what, value] of checks) {
        const c = rgb(value);
        if (!c || c.a === 0 || near(c)) continue;
        out.add(`${el.tagName.toLowerCase()}.${[...el.classList].join('.')} ${what} ${value}`);
      }
    }
    return [...out];
  }, tokenNames);
}

/** Body text below 4.5:1 against what is actually behind it. */
async function lowContrastText(page) {
  return page.evaluate(() => {
    const parse = value => { const m = /(?:rgba?\(|color\(srgb\s+)([^)]*)\)/.exec(value); if (!m) return null; const p = m[1].split(/[\s,/]+/).filter(Boolean).map(Number); const s = value.startsWith('color(') ? 255 : 1; return [p[0] * s, p[1] * s, p[2] * s, p[3] ?? 1]; };
    const lin = v => { v /= 255; return v <= 0.04045 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4; };
    const lum = ([r, g, b]) => 0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b);
    const over = (top, bottom) => [0, 1, 2].map(i => top[i] * top[3] + bottom[i] * (1 - top[3])).concat(1);
    const behind = el => { const layers = []; for (let n = el; n; n = n.parentElement) { const c = parse(getComputedStyle(n).backgroundColor); if (c && c[3] > 0) { layers.push(c); if (c[3] >= 1) break; } } let base = [255, 255, 255, 1]; for (const layer of layers.reverse()) base = over(layer, base); return base; };
    const out = [];
    for (const el of document.querySelectorAll('p, li, td, th, h1, h2, h3, label, .settings-row-label, .conversation-title-text, .workspace-link strong')) {
      const text = [...el.childNodes].filter(n => n.nodeType === 3).map(n => n.textContent.trim()).join('');
      const box = el.getBoundingClientRect();
      if (!text || !box.width || el.closest('[aria-disabled=true],:disabled,.theme-preview,.chat-sample .message_delta code')) continue;
      const style = getComputedStyle(el);
      if (style.visibility === 'hidden' || Number(style.opacity) < 1) continue;
      const fg = parse(style.color); if (!fg) continue;
      const bg = behind(el), color = over(fg, bg);
      const a = lum(color), b = lum(bg), ratio = (Math.max(a, b) + 0.05) / (Math.min(a, b) + 0.05);
      if (ratio < 4.5) out.push(`${el.tagName.toLowerCase()}.${[...el.classList].join('.')} "${text.slice(0, 30)}" ${ratio.toFixed(2)}`);
    }
    return out;
  });
}

let browser;
try {
  await server.listen();
  await mkdir('.cache', { recursive: true });
  browser = await chromium.launch({ headless: true });
  const base = `http://127.0.0.1:${server.httpServer.address().port}`;
  const errors = [];
  const audit = async (page, scheme, name) => {
    await page.screenshot({ path: `.cache/appearance-${scheme}-${name}.png` });
    assert.equal(await page.evaluate(() => document.documentElement.dataset.scheme), scheme, `${name} is ${scheme}`);
    assert.deepEqual(await untokenizedColours(page), [], `${scheme} ${name}: every colour is a theme token`);
    assert.deepEqual(await lowContrastText(page), [], `${scheme} ${name}: body text is 4.5:1 or better`);
  };

  // System follows the computer: dark, then light, with nothing chosen.
  const dark = await browser.newContext({ viewport: { width: 1440, height: 900 }, colorScheme: 'dark' });
  let page = await dark.newPage(); page.setDefaultTimeout(15000); page.on('pageerror', e => errors.push(String(e)));
  await visitKeyPages(page, base, fixture.token, name => audit(page, 'dark', name), { settingsTabs: ['Subscriptions', 'Appearance'], artifactId: fixture.designDoc.id });
  assert.equal(await page.evaluate(() => getComputedStyle(document.documentElement).getPropertyValue('--surface').trim()), '#1d1e24', 'Dark keeps its canvas');
  // The audits are not vacuous: a literal colour and faint body text are caught.
  await page.evaluate(() => { const p = document.createElement('p'); p.id = 'audit-control'; p.textContent = 'control text'; p.style.cssText = 'color:#2a2b2f;position:fixed;top:0;left:0;z-index:9;background:var(--surface)'; document.body.appendChild(p); });
  assert.ok((await untokenizedColours(page)).some(line => line.includes('#2a2b2f') || line.includes('42, 43, 47')), 'a literal colour is reported');
  assert.ok((await lowContrastText(page)).some(line => line.includes('control text')), 'faint body text is reported');
  await page.evaluate(() => document.getElementById('audit-control').remove());
  await dark.close();

  const light = await browser.newContext({ viewport: { width: 1440, height: 900 }, colorScheme: 'light' });
  page = await light.newPage(); page.setDefaultTimeout(15000); page.on('pageerror', e => errors.push(String(e)));
  await visitKeyPages(page, base, fixture.token, name => audit(page, 'light', name), { settingsTabs: ['Subscriptions', 'Appearance'], artifactId: fixture.designDoc.id });
  const css = name => page.evaluate(n => getComputedStyle(document.documentElement).getPropertyValue(`--${n}`).trim(), name);
  assert.equal(await css('surface'), '#fafafc');

  // Settings → Appearance: a fixed mode overrides the system, live and kept.
  const panel = page.locator('.appearance-settings');
  await panel.getByRole('radiogroup', { name: 'Appearance', exact: true }).getByRole('radio', { name: 'Dark', exact: true }).click();
  await expect(page.locator('html')).toHaveAttribute('data-scheme', 'dark');
  await panel.getByRole('radio', { name: /^Dark high contrast/ }).click();
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark-high-contrast');
  assert.equal(await css('surface'), '#08080a');
  await panel.getByRole('radio', { name: 'Coral', exact: true }).click();
  assert.equal(await css('accent'), '#ef9a86');
  assert.notEqual(await css('accent-bg'), '#2b2839', 'the accent family follows the accent');
  await panel.getByRole('radiogroup', { name: 'Appearance', exact: true }).getByRole('radio', { name: 'Light', exact: true }).click();
  await panel.getByRole('radio', { name: /^Solarized light/ }).click();
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'solarized-light');
  assert.equal(await css('surface'), '#fdf6e3');

  // Minimum contrast: muted text moves, surfaces never do.
  const muted = await css('text-muted');
  await panel.getByLabel('Minimum contrast').selectOption('7');
  await expect.poll(() => css('text-muted')).not.toBe(muted);
  assert.equal(await css('surface'), '#fdf6e3');

  // Chat text lives in Settings → Chat, beside Activity detail: one Chat section.
  await page.getByRole('tab', { name: 'Chat', exact: true }).click();
  const chat = page.getByRole('tabpanel', { name: 'Chat' });
  await expect(chat.getByRole('radiogroup', { name: 'Activity detail' })).toBeVisible();
  const sample = chat.locator('.chat-sample .rich-content p').first();
  await chat.getByRole('radiogroup', { name: 'Line spacing' }).getByRole('radio', { name: 'Compact', exact: true }).click();
  await chat.getByLabel('appearance chat size value').fill('16');
  await expect.poll(() => sample.evaluate(p => [getComputedStyle(p).fontSize, getComputedStyle(p).lineHeight])).toEqual(['16px', '21.6px']);
  await chat.getByLabel('Line width').selectOption('80');
  assert.equal(await css('chat-max-width'), '80ch');
  assert.deepEqual(await lowContrastText(page), [], 'Settings → Chat keeps body text readable');
  await page.screenshot({ path: '.cache/appearance-settings-chat.png' });
  await page.getByRole('tab', { name: 'Appearance', exact: true }).click();
  await panel.getByRole('radiogroup', { name: 'Density' }).getByRole('radio', { name: 'Relaxed', exact: true }).click();
  await expect(page.locator('html')).toHaveAttribute('data-density', 'relaxed');
  assert.equal(await css('chat-line-height'), '1.7', 'density brings its chat spacing');
  await panel.getByRole('radiogroup', { name: 'Motion' }).getByRole('radio', { name: 'Reduce', exact: true }).click();
  await expect(page.locator('html')).toHaveAttribute('data-motion', 'reduce');
  await panel.getByLabel('appearance ui size value').fill('14');
  assert.equal(await css('text-scale'), String(14 / 13));
  await page.screenshot({ path: '.cache/appearance-settings-changed.png', fullPage: true });

  // Kept across a reload; the conversation reads with the new chat text.
  await page.goto(`${base}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`);
  await page.getByRole('button', { name: 'Work', exact: true }).click();
  await expect(page.getByRole('heading', { name: 'Parser fix' })).toBeVisible();
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'solarized-light');
  assert.equal(await css('accent').then(v => v !== '#ef9a86'), true, 'the accent is held to 7:1 on a light theme');
  assert.equal(await page.locator('.rich-content p').first().evaluate(p => getComputedStyle(p).fontSize), '16px');
  assert.ok(await page.locator('.messages-content').first().evaluate(n => n.getBoundingClientRect().width < 900), 'chat lines are capped');
  assert.deepEqual(await lowContrastText(page), [], 'Solarized at 7:1 keeps body text readable');
  await page.screenshot({ path: '.cache/appearance-solarized-conversation.png' });

  // A user theme: listed, applied, hot reloaded, and its mistakes shown.
  const theme = (accent, extra = {}) => JSON.stringify({ name: 'Midnight', appearance: 'dark', tokens: { bg: '#05060a', surface: '#0b0d14', 'surface-raised': '#121522', line: '#252a3a', text: '#e8ecf5', 'text-muted': '#8b93a7', accent, ...extra } });
  fixture.themeFolder.files.push(
    { name: 'midnight.toml', format: 'toml', json: theme('#5eb1ff'), error: '', modifiedUnixMillis: String(Date.now()) },
    { name: 'typo.json', format: 'json', json: JSON.stringify({ name: 'Typo', appearance: 'dark', tokens: { acent: '#fff' } }), error: '', modifiedUnixMillis: String(Date.now()) },
    { name: 'broken.toml', format: 'toml', json: '', error: 'line 3, column 9: expected "=" after a key', modifiedUnixMillis: String(Date.now()) },
  );
  await page.getByRole('button', { name: 'Settings', exact: true }).click();
  await page.getByRole('tab', { name: 'Appearance', exact: true }).click();
  await panel.getByRole('button', { name: 'Reset all' }).click();
  await panel.getByLabel('Minimum contrast').selectOption('4.5');
  await expect(panel).toContainText('/fixture/state/themes');
  const midnight = panel.getByRole('radio', { name: /^Midnight midnight\.toml/ });
  await expect(midnight).toBeVisible();
  await expect(panel.locator('.theme-problems', { hasText: 'typo.json' })).toContainText('tokens.acent: there is no such token (did you mean "accent"?)');
  await expect(panel.locator('.theme-problems', { hasText: 'broken.toml' })).toContainText('line 3, column 9');
  await panel.getByRole('radiogroup', { name: 'Appearance', exact: true }).getByRole('radio', { name: 'Dark', exact: true }).click();
  await midnight.click();
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'user:midnight');
  assert.equal(await css('surface'), '#0b0d14');
  assert.equal(await css('accent'), '#5eb1ff');
  // Saved in an editor: the window repaints without a reload.
  fixture.themeFolder.files[0] = { ...fixture.themeFolder.files[0], json: theme('#22c4a1'), modifiedUnixMillis: String(Date.now()) };
  await expect.poll(() => css('accent'), { timeout: 10000 }).toBe('#22c4a1');
  // A mistake while editing keeps the last good version and says what is wrong.
  fixture.themeFolder.files[0] = { ...fixture.themeFolder.files[0], json: theme('#22c4a1', { surface: 'navy' }) };
  await expect(panel.locator('.theme-problems', { hasText: 'midnight.toml' })).toContainText('tokens.surface: "navy" is not a colour', { timeout: 10000 });
  assert.equal(await css('surface'), '#0b0d14');
  await expect(midnight).toContainText('last good version');
  await expect(midnight).toHaveAttribute('aria-checked', 'true');
  assert.deepEqual(await untokenizedColours(page), [], 'a user theme reaches every colour too');
  await page.screenshot({ path: '.cache/appearance-user-theme.png' });

  // A new theme starts from the current colours, as a file to edit.
  await panel.getByRole('button', { name: 'New theme from current' }).click();
  await expect(panel.locator('.appearance-status')).toContainText('/fixture/state/themes/my-dark-theme.toml');
  const created = fixture.mutations.find(m => m.method === 'CreateTheme');
  assert.match(created.body.content, /^name = "My dark theme"$/m);
  assert.match(created.body.content, /^accent = "#22c4a1"$/m);

  await panel.getByRole('button', { name: 'Delete theme midnight.toml' }).click();
  await page.getByRole('dialog', { name: 'Delete theme?' }).getByRole('button', { name: 'Delete theme', exact: true }).click();
  await expect(midnight).toHaveCount(0);
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark');
  assert.equal(fixture.mutations.find(m => m.method === 'DeleteTheme').body.file, 'midnight.toml');

  // The token reference lists every token with its current value.
  await panel.getByRole('button', { name: /Token reference/ }).click();
  await panel.getByLabel('Filter tokens').fill('diff-add');
  await expect(panel.locator('.token-group tr')).toHaveCount(3);
  await panel.getByRole('button', { name: 'Reset all' }).click();
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'light');
  // The command palette switches light and dark from anywhere.
  await page.keyboard.press('Meta+k');
  await page.keyboard.type('Switch between light and dark');
  await page.keyboard.press('Enter');
  await expect(page.locator('html')).toHaveAttribute('data-scheme', 'dark');
  await light.close();

  // settings.yaml (POWER.md, "Config files"): Settings → Appearance writes its
  // section at the right paths; a save in an editor applies live within the
  // poll; a mistake is shown by line in Appearance (not in Notifications);
  // removing a value puts its default back; a change made just before the
  // window goes away is not lost to the file's older value. Dark and light.
  const settingsWrites = () => fixture.mutations.filter(m => m.method === 'UpdateConfigFile' && m.body.name === 'settings');
  for (const scheme of ['dark', 'light']) {
    fixture.config.setFile('settings', { value: { notifications: {}, appearance: {} } });
    const context = await browser.newContext({ viewport: { width: 1440, height: 900 }, colorScheme: scheme });
    page = await context.newPage(); page.setDefaultTimeout(15000); page.on('pageerror', e => errors.push(String(e)));
    const css = name => page.evaluate(n => getComputedStyle(document.documentElement).getPropertyValue(`--${n}`).trim(), name);
    await page.goto(`${base}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`);
    await expect(page.getByRole('heading', { name: 'Parser fix' })).toBeVisible();
    await page.getByRole('button', { name: 'Settings', exact: true }).click();
    await page.getByRole('tab', { name: 'Appearance', exact: true }).click();
    const panel = page.locator('.appearance-settings').first();
    const status = panel.locator('.config-file');
    await expect(status).toContainText('Your appearance settings are kept in settings.yaml');
    await expect(status.locator('code')).toHaveText('/fixture/config/lazybox/settings.yaml');

    // A click writes one value at its path; the file then says it.
    const before = settingsWrites().length;
    await panel.getByRole('radio', { name: 'Teal', exact: true }).click();
    await expect.poll(() => settingsWrites().slice(before).map(m => m.body.edits)).toEqual([[{ path: ['appearance', 'accent'], json: '"#6fcfc4"' }]]);
    assert.equal(fixture.config.value('settings').appearance.accent, '#6fcfc4');
    // A dragged slider is one write, of where it stopped.
    const dragged = settingsWrites().length;
    for (const size of ['14', '15', '16']) await panel.getByLabel('appearance code size value').fill(size);
    await expect.poll(() => fixture.config.value('settings').appearance.codeFontSize).toBe(16);
    assert.equal(settingsWrites().length - dragged, 1, 'one write for a burst of changes');

    // Saved in an editor: applied live, no reload.
    const started = Date.now();
    fixture.config.setFile('settings', { value: { notifications: {}, appearance: { accent: '#e2b765', density: 'compact', chatLineHeight: 1.4, mode: scheme === 'dark' ? 'light' : 'dark' } } });
    await expect(page.locator('html')).toHaveAttribute('data-density', 'compact', { timeout: 4000 });
    assert.ok(Date.now() - started < 4000, 'applied within the poll');
    await expect(page.locator('html')).toHaveAttribute('data-scheme', scheme === 'dark' ? 'light' : 'dark');
    await expect(panel.getByRole('radio', { name: 'Amber', exact: true })).toHaveAttribute('aria-checked', 'true');
    assert.equal(await css('chat-line-height'), '1.4');
    await expect(panel.getByLabel('appearance code size value')).toHaveValue('12', { timeout: 1000 }).catch(() => undefined);
    assert.equal(await panel.getByLabel('appearance code size value').inputValue(), '12', 'what the file leaves out is the default');
    assert.equal(settingsWrites().length - dragged, 1, 'applying the file writes nothing back');

    // A mistake: by line in Settings → Appearance and in a toast; Notifications does not show it.
    fixture.config.setFile('settings', { value: { notifications: {}, appearance: { accent: '#e2b765', density: 'compact', mode: scheme } }, issues: [{ path: 'appearance.acent', line: 6, message: 'unknown setting "acent" (did you mean "accent"?)', severity: 'error' }] });
    await expect(page.locator('html')).toHaveAttribute('data-scheme', scheme, { timeout: 4000 });
    const problems = page.getByRole('list', { name: 'Problems in settings.yaml' });
    await expect(problems).toContainText('line 6');
    await expect(problems).toContainText('did you mean "accent"?');
    await expect(page.getByText('settings.yaml: a problem')).toBeVisible();
    await panel.evaluate(n => { for (let p = n.parentElement; p; p = p.parentElement) if (/auto|scroll/.test(getComputedStyle(p).overflowY)) p.scrollTop = 0; window.scrollTo(0, 0); });
    await page.screenshot({ path: `.cache/appearance-file-${scheme}.png` });
    await page.getByRole('tab', { name: 'Notifications', exact: true }).click();
    await expect(page.getByText('settings.yaml', { exact: true })).toBeVisible();
    await expect(page.getByRole('list', { name: 'Problems in settings.yaml' })).toHaveCount(0);
    await page.getByRole('tab', { name: 'Appearance', exact: true }).click();

    // Removed from the file: back to the defaults (the theme's accent, comfortable rows).
    fixture.config.setFile('settings', { value: { notifications: {}, appearance: { mode: scheme } } });
    await expect(page.locator('html')).toHaveAttribute('data-density', 'comfortable', { timeout: 4000 });
    await expect(panel.getByRole('radio', { name: 'Theme accent' })).toHaveAttribute('aria-checked', 'true');
    await expect(page.getByText('settings.yaml: a problem'), 'a fixed mistake takes its toast with it').toHaveCount(0);
    await panel.evaluate(n => { for (let p = n.parentElement; p; p = p.parentElement) if (/auto|scroll/.test(getComputedStyle(p).overflowY)) p.scrollTop = 0; window.scrollTo(0, 0); });
    await page.screenshot({ path: `.cache/appearance-file-defaults-${scheme}.png` });

    // A change, then the window reloads at once: the change wins over the file's older value.
    await panel.getByRole('radio', { name: 'Rose', exact: true }).click();
    await page.reload();
    await page.getByRole('button', { name: 'Settings', exact: true }).click();
    await page.getByRole('tab', { name: 'Appearance', exact: true }).click();
    await expect.poll(() => fixture.config.value('settings').appearance.accent, { timeout: 6000 }).toBe('#ea9ac0');
    await expect(panel.getByRole('radio', { name: 'Rose', exact: true })).toHaveAttribute('aria-checked', 'true');
    await context.close();
  }
  fixture.config.setFile('settings', { value: { notifications: {}, appearance: {} } });

  assert.deepEqual(errors, []);
  console.log('PASS: dark and light key pages use only theme tokens with body text ≥ 4.5:1; System follows the computer; theme, accent, contrast, chat text, density, motion and text size apply live and persist; user theme files are listed, applied, hot reloaded and their errors shown; New theme from current. Fixture only.');
} finally { await browser?.close(); await server.close(); await fixture.close(); }
