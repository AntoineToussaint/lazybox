// Fixture only: the agent, model and context at the left of the message
// box's controls. They say what the chat header says (the same model words,
// the same context reading, no second poll of usage), colour at 60% and 85%,
// open the model picker and the header's context popover, and give way in a
// narrow box (the model name, then all but the meter) and on a phone (the
// percentage alone). A conversation that has not started shows none of it.
// Screenshots in .cache/composer-info-*.png. WebKit, the desktop app's engine.
import { testViteCache } from './fixtures/vite-cache.mjs';
import { createServer } from 'vite';
import react from '@vitejs/plugin-react';
import { webkit, expect } from './fixtures/playwright.mjs';
import { fileURLToPath } from 'node:url';
import { mkdir } from 'node:fs/promises';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';

const f = await startRunnerFixture();
const session = f.sessions[0];
Object.assign(session, { title: 'Composer info', provider: 'claude', model: 'claude-opus-5-5', modelReported: true, status: 'done', busy: false,
  agents: [{ id: 'root', status: 'done', turnStartedAtUnixMillis: String(Date.now() - 90_000), turnEndedAtUnixMillis: String(Date.now() - 30_000), lastTurnStatus: 'completed' }] });
// The Codex conversation waits for its first message and has no usage sample.
Object.assign(f.sessions[1], { provider: 'codex', profileId: 'codex-default', model: 'gpt-5.4', modelReported: true, status: 'ready', busy: false, agents: [] });
{
  const channel = f.openEvents(session.id);
  let at = Date.now() - 120_000;
  channel.push({ type: 'operation_accepted', text: 'Look at the parser.', operationId: 'op-1' }, new Date(at += 1000).toISOString());
  channel.push({ type: 'turn_started' }, new Date(at += 100).toISOString());
  channel.push({ type: 'message_delta', text: 'The parser is fine.' }, new Date(at += 100).toISOString());
  channel.push({ type: 'turn_ended', status: 'completed' }, new Date(at += 100).toISOString());
}
let used = 404000;
let capacity = '1000000';
const telemetry = () => ({ groups: [{ scope: { execution: session.id, profile: 'claude-default' }, source: 'gateway', model: 'claude-opus-5-5', records: '12', unpriced: '0', incomplete: '0', nanos: '1365000000', tokens: { input: '120000', output: '9000' } }],
  contexts: [{ scope: { execution: session.id }, agent: 'root', used: String(used), capacity, reserve: '33000', source: 'sdk_summary_estimate', observedAt: new Date().toISOString(), categories: [{ name: 'messages', tokens: String(used), kind: 'used', included: true }] }], recent: [] });

const server = await createServer({ ...testViteCache(import.meta.url), configFile: false, logLevel: 'error', root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: f.url } } } });
let browser;
try {
  await server.listen(); await mkdir('.cache', { recursive: true });
  browser = await webkit.launch();
  const base = `http://127.0.0.1:${server.httpServer.address().port}`;
  const url = id => `${base}/?view=workspace&collection=${id === session.id ? 'alpha' : 'beta'}&session=${id}#token=${f.token}`;
  /** A colour token as the page computes it. */
  const token = (page, name) => page.evaluate(name => { const probe = document.createElement('span'); probe.style.color = `var(--${name})`; document.body.append(probe); const value = getComputedStyle(probe).color; probe.remove(); return value; }, name);
  /** What of the info shows: the agent, the model name, the meter, the percentage. */
  const shown = info => info.evaluate(node => Object.fromEntries(['composer-info-agent', 'composer-info-model-name', 'composer-info-meter', 'composer-info-percent']
    .map(name => [name.replace('composer-info-', ''), !!node.querySelector(`.${name}`)?.getClientRects().length])));
  /** Narrows the message box to `width` CSS pixels, as a slim panel would. */
  const narrow = (composer, width) => composer.evaluate((node, width) => { node.style.width = `${width}px`; node.style.flex = 'none'; }, width);

  for (const scheme of ['dark', 'light']) {
    used = 404000;
    capacity = '1000000';
    const context = await browser.newContext({ viewport: { width: 1440, height: 900 }, colorScheme: scheme });
    const page = await context.newPage(); page.setDefaultTimeout(15000);
    // Leaving a page cuts its in-flight polls, which WebKit reports as page errors ("access control checks") on the page being left: not failures. Anything else is.
    const errors = []; let leaving = 0; page.on('pageerror', e => { const text = String(e); if (leaving && /access control checks/.test(text)) return; errors.push(text); });
    const leave = async to => { leaving++; try { await page.goto(to); } finally { leaving--; } };
    let reads = 0, usageAvailable = true;
    await page.route('**/GetTelemetry', route => { reads++; return route.fulfill({ json: usageAvailable ? telemetry() : { contexts: [], groups: [], recent: [] } }); });
    await page.goto(url(session.id));
    const chat = page.locator('.panel-chat'), header = chat.locator('.chat-heading'), composer = chat.locator('form.composer'), info = composer.locator('.composer-info');
    const row = composer.locator('.row'), contextButton = info.getByRole('button', { name: /^Context .*show context and cost/ }), modelButton = info.locator('.composer-info-model');

    // The header's words and number, first on the row.
    await expect(header.locator('.chat-model')).toHaveText('Claude · Opus 5.5');
    await expect(modelButton).toHaveText('Claude · Opus 5.5');
    await expect(modelButton).toHaveAttribute('title', /claude-opus-5-5/);
    await expect(contextButton).toHaveText('40.4%');
    await expect(header.getByRole('button', { name: 'Context and token cost' })).toContainText('40.4%');
    await expect(contextButton).toHaveAttribute('data-level', 'normal');
    assert.equal(await row.evaluate(node => node.firstElementChild?.className), 'composer-info', 'the info leads the row');
    {
      const r = await row.boundingBox(), i = await info.boundingBox(), send = await composer.locator('.send-button').boundingBox();
      assert.ok(i.x - r.x < 2, `the info starts the row (${i.x} vs ${r.x})`);
      assert.ok(Math.abs((i.y + i.height / 2) - (send.y + send.height / 2)) < 2, 'the info is centred on the row, with Send');
      assert.ok(i.y >= r.y - 1 && i.y + i.height <= r.y + r.height + 1, 'one row: nothing wrapped');
    }
    // One reader of the usage: the header's chip. The message box adds no poll.
    reads = 0; await page.waitForTimeout(3500);
    assert.ok(reads <= 2, `one usage poll every 3 s, not two (${reads} reads in 3.5 s)`);
    await page.mouse.move(0, 0);
    await composer.screenshot({ path: `.cache/composer-info-wide-${scheme}.png` });

    // 60% warns, 85% is danger: here and in the header, in the theme's colours.
    const normal = await contextButton.locator('.composer-info-percent').evaluate(node => getComputedStyle(node).color);
    used = 700000;
    await expect(contextButton).toHaveText('70%', { timeout: 10000 });
    await expect(contextButton).toHaveAttribute('data-level', 'warn');
    await expect(header.locator('.context-mini-bar')).toHaveAttribute('data-level', 'warn');
    assert.equal(await contextButton.locator('.composer-info-percent').evaluate(node => getComputedStyle(node).color), await token(page, 'warn'));
    assert.equal(await contextButton.locator('.composer-info-meter i').evaluate(node => getComputedStyle(node).backgroundColor), await token(page, 'warn'));
    used = 912000;
    await expect(contextButton).toHaveText('91.2%', { timeout: 10000 });
    await expect(contextButton).toHaveAttribute('data-level', 'danger');
    await expect(header.locator('.context-mini-bar')).toHaveAttribute('data-level', 'danger');
    const danger = await contextButton.locator('.composer-info-percent').evaluate(node => getComputedStyle(node).color);
    assert.equal(danger, await token(page, 'danger'));
    assert.notEqual(normal, danger, 'the colour changes with the level');
    await page.mouse.move(0, 0);
    await composer.screenshot({ path: `.cache/composer-info-danger-${scheme}.png` });

    // Codex can report a used count before it reports the capacity.
    capacity = undefined;
    await expect(contextButton).toHaveText('912K tokens', { timeout: 10000 });
    await expect(header.getByRole('button', { name: 'Context and token cost' })).toContainText('912K tokens');
    await expect(contextButton.locator('.composer-info-meter')).toHaveCount(0);
    capacity = '1000000';
    await expect(contextButton).toHaveText('91.2%', { timeout: 10000 });

    // The meter opens the header's context popover, over the message box; a second click closes it.
    await contextButton.click();
    const popover = page.locator('.context-popover:popover-open');
    await expect(popover).toHaveCount(1);
    await expect(popover).toContainText('91.2% used');
    {
      const p = await popover.boundingBox(), c = await contextButton.boundingBox();
      assert.ok(p.y + p.height <= c.y + 1, 'the popover opens above the meter');
      assert.ok(Math.abs(p.x - c.x) < 2 || p.x >= 11, 'aligned with the meter');
    }
    if (scheme === 'dark') await page.screenshot({ path: '.cache/composer-info-popover.png' });
    await contextButton.click();
    await expect(popover).toHaveCount(0);
    // Keyboard: the same, from Tab and Enter.
    await contextButton.focus(); await page.keyboard.press('Enter');
    await expect(popover).toHaveCount(1);
    await page.keyboard.press('Escape');
    await expect(popover).toHaveCount(0);

    // The agent opens the model picker, as the header's chip does.
    const message = composer.getByRole('textbox', { name: 'Message', exact: true });
    await modelButton.click();
    await expect(message).toHaveValue('/model ');
    await expect(composer.locator('.slash-menu')).toBeVisible();
    await page.keyboard.press('Escape');
    await expect(message).toHaveValue('');

    // Narrow boxes keep the model picker and percentage visible.
    assert.deepEqual(await shown(info), { agent: true, 'model-name': true, meter: true, percent: true });
    await narrow(composer, 430);
    await expect.poll(() => shown(info)).toEqual({ agent: true, 'model-name': true, meter: false, percent: true });
    await composer.screenshot({ path: `.cache/composer-info-narrow-${scheme}.png` });
    await narrow(composer, 360);
    await expect(modelButton).toBeVisible();
    await expect(contextButton).toContainText('91.2%');
    await composer.screenshot({ path: `.cache/composer-info-slim-${scheme}.png` });
    {
      const r = await row.boundingBox(), send = await composer.locator('.send-button').boundingBox();
      assert.ok(send.y + send.height <= r.y + r.height + 1 && send.x + send.width <= r.x + r.width + 1, 'Send stays in the row');
    }

    // A phone: the percentage alone.
    await page.setViewportSize({ width: 390, height: 844 });
    await leave(url(session.id));
    await expect(contextButton).toHaveText('91.2%');
    await expect(modelButton).toBeVisible();
    await expect(contextButton.locator('.composer-info-meter')).toBeHidden();
    await composer.screenshot({ path: `.cache/composer-info-phone-${scheme}.png` });
    {
      // One row on a phone, even with a notice chip: the percentage takes Attach's word, never Send's place.
      const i = await info.boundingBox(), send = await composer.locator('.send-button').boundingBox();
      assert.ok(Math.abs((i.y + i.height / 2) - (send.y + send.height / 2)) < 3, 'the percentage and Send share the row');
      assert.ok(i.height >= 32, 'a finger-sized target');
    }

    // A first turn can pick a model. The context slot explains why usage is unavailable.
    await page.setViewportSize({ width: 1440, height: 900 });
    usageAvailable = false;
    await leave(url('beta-session'));
    await expect(page.locator('.panel-chat form.composer')).toBeVisible();
    await expect(page.locator('.panel-chat').getByRole('button', {name: /Codex.*change model/})).toBeVisible();
    await expect(page.locator('.panel-chat .composer-info').getByTitle('Waiting for the provider to report context usage')).toHaveText('Context —');
    assert.deepEqual(errors, []);
    await context.close();
  }
  console.log('composer info: the header\'s model and context, colour at 60/85%, model picker and context popover, narrow and phone; screenshots in .cache/composer-info-*.png');
} finally {
  await browser?.close(); await server.close(); await f.close?.();
}
