// Fixture only: layout defects a person hits without doing anything unusual.
// A chat squeezed by docked panels keeps its title and a working message box;
// a phone reaches every page tab and keeps the running step on one row; a
// page opened from the header (Changes) closes again; the header fits a window
// snapped to half a screen. WebKit, the desktop engine.
import { testViteCache } from './fixtures/vite-cache.mjs';
import { createServer } from 'vite';
import react from '@vitejs/plugin-react';
import { webkit, expect } from '@playwright/test';
import { fileURLToPath } from 'node:url';
import { mkdir } from 'node:fs/promises';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';

const fixture = await startRunnerFixture();
Object.assign(fixture.sessions[0], { title: 'Ship the front door with a long conversation title', busy: true, status: 'running' });
const server = await createServer({ ...testViteCache(import.meta.url), configFile: false, logLevel: 'error', root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: fixture.url } } } });
let browser;
/** a's box lies within b's, to a pixel. */
const inside = (a, b) => a.x >= b.x - 1 && a.y >= b.y - 1 && a.x + a.width <= b.x + b.width + 1 && a.y + a.height <= b.y + b.height + 1;
try {
  await server.listen(); await mkdir('.cache', { recursive: true });
  browser = await webkit.launch();
  const url = `http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`;
  const errors = [];
  const open = async viewport => { const page = await (await browser.newContext({ viewport })).newPage(); page.setDefaultTimeout(15000); page.on('pageerror', error => errors.push(String(error))); await page.goto(url); await expect(page.getByRole('textbox', { name: 'Message', exact: true }).first()).toBeVisible(); return page; };

  // 1. A chat squeezed between the sidebar and three docked panels.
  {
    const page = await open({ width: 1440, height: 900 });
    await page.getByRole('button', { name: 'Browse files', exact: true }).click();
    await page.getByRole('treeitem', { name: /api/ }).first().click();
    await page.getByRole('treeitem', { name: /README/ }).first().dblclick();
    await expect(page.locator('.panel-editor')).toBeVisible();
    await page.getByRole('button', { name: 'Open terminal', exact: true }).click();
    await expect(page.locator('.panel-terminal')).toBeVisible();
    const chat = page.locator('.panel-chat'), box = await chat.boundingBox();
    assert.ok(box.width < 420, `the chat is narrow in this arrangement (${Math.round(box.width)}px)`);
    // The title is still there to read.
    const title = await chat.locator('.conversation-title-text').boundingBox();
    assert.ok(title.width >= Math.min(140, box.width * 0.5), `the conversation title keeps its room (${Math.round(title.width)}px of ${Math.round(box.width)}px)`);
    // Every header tool is inside the chat, none under a neighbour.
    for (const tool of await chat.locator('.chat-heading-actions button, .chat-heading-actions summary').all()) {
      if (!await tool.isVisible()) continue;
      assert.ok(inside(await tool.boundingBox(), box), `header tool "${await tool.getAttribute('aria-label') || await tool.textContent()}" stays inside the chat`);
    }
    // The message box: Send, Attach and Snippets are whole and inside it.
    const composer = await chat.locator('.composer').boundingBox();
    for (const name of ['Queue follow-up', 'Attach files', 'Snippets']) {
      const control = chat.getByRole('button', { name, exact: true }), bounds = await control.boundingBox();
      assert.ok(bounds && bounds.width >= 24 && bounds.height >= 24, `${name} keeps its size (${bounds && Math.round(bounds.width)}px wide)`);
      assert.ok(inside(bounds, composer), `${name} stays inside the message box`);
    }
    const bar = chat.getByRole('region', { name: 'What the agent is doing' });
    assert.ok(inside(await bar.getByRole('button', { name: 'Interrupt' }).boundingBox(), await bar.boundingBox()), 'Interrupt stays inside the live bar');
    await page.screenshot({ path: '.cache/polish-pass-narrow-chat.png' });
    await page.context().close();
  }

  // 2. A phone: page tabs, the chat header and the live bar.
  {
    const page = await open({ width: 390, height: 844 });
    assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1), 'no sideways page scroll');
    // A page tab that does not fit can be scrolled to: it is never cut with no way to reach it.
    const nav = page.locator('.workspace-header > nav');
    const tabs = await nav.evaluate(node => ({ fits: node.scrollWidth <= node.clientWidth + 1, scrolls: getComputedStyle(node).overflowX === 'auto' }));
    assert.ok(tabs.fits || tabs.scrolls, 'page tabs fit or scroll');
    const settings = nav.getByRole('button', { name: 'Settings', exact: true });
    await settings.scrollIntoViewIfNeeded();
    const navBox = await nav.boundingBox(), settingsBox = await settings.boundingBox();
    assert.ok(settingsBox.x >= navBox.x - 1 && settingsBox.x + settingsBox.width <= navBox.x + navBox.width + 1, 'the Settings tab can be brought fully into view');
    const chat = page.locator('.panel-chat'), box = await chat.boundingBox();
    const title = await chat.locator('.conversation-title-text').boundingBox();
    assert.ok(title.width >= 200, `the conversation title is readable on a phone (${Math.round(title.width)}px)`);
    for (const tool of await chat.locator('.chat-heading-actions button, .chat-heading-actions summary').all()) if (await tool.isVisible()) assert.ok(inside(await tool.boundingBox(), box), 'header tools stay on screen');
    // "Writing… · Interrupt" is one row, not two.
    const bar = chat.getByRole('region', { name: 'What the agent is doing' });
    const step = await bar.locator('.live-activity').boundingBox(), stop = await bar.getByRole('button', { name: 'Interrupt' }).boundingBox();
    assert.ok(Math.abs((step.y + step.height / 2) - (stop.y + stop.height / 2)) < 8, 'the step and Interrupt share a row');
    await page.screenshot({ path: '.cache/polish-pass-phone.png' });
    await page.context().close();
  }

  // 3. A page opened from the header closes again: its only panel's × takes the page with it.
  {
    const page = await open({ width: 1440, height: 900 });
    const pages = page.locator('.workspace-header > nav');
    await page.getByRole('button', { name: 'Review changes', exact: true }).click();
    await expect(pages.getByRole('button', { name: 'Changes', exact: true })).toBeVisible();
    const close = page.getByRole('button', { name: 'Close Changes page', exact: true });
    await expect(close).toBeVisible();
    await close.focus(); await page.keyboard.press('Enter');
    await expect(pages.getByRole('button', { name: 'Changes', exact: true })).toHaveCount(0);
    await expect(pages.getByRole('button', { name: 'Work', exact: true })).toHaveClass(/selected/);
    await expect(page.locator('.panel-chat')).toBeVisible();
    // The last page keeps its conversation: the chat is not closable.
    await expect(page.locator('.panel-chat').getByRole('button', { name: /^Close .* (panel|page)$/ })).toHaveCount(0);
    // And it comes back from the same button.
    await page.getByRole('button', { name: 'Review changes', exact: true }).click();
    await expect(page.getByRole('region', { name: 'Workspace changes' })).toBeVisible();
    await page.context().close();
  }

  // 4. Half a laptop screen: the header fits. Page tabs and the page menu stay on screen, nothing scrolls sideways.
  for (const width of [800, 900, 1024, 1100]) {
    const page = await open({ width, height: 900 });
    assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1), `no sideways page scroll at ${width}px`);
    for (const control of [page.locator('.workspace-header > nav').getByRole('button', { name: 'Work', exact: true }), page.locator('.workspace-header > nav').getByRole('button', { name: 'Settings', exact: true }), page.locator('.workspace-header-actions summary[aria-label="Page actions"]'), page.getByRole('button', { name: 'New conversation', exact: true }).first()]) {
      const bounds = await control.boundingBox();
      assert.ok(bounds && bounds.x >= 0 && bounds.x + bounds.width <= width + 1, `a header control is on screen at ${width}px`);
      // Nothing lies on top of it.
      assert.ok(await control.evaluate(node => { const r = node.getBoundingClientRect(), top = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2); return !!top && (node === top || node.contains(top)); }), `a header control is not covered at ${width}px`);
    }
    if (width === 800) await page.screenshot({ path: '.cache/polish-pass-half-screen.png' });
    await page.context().close();
  }

  // 5. Tooltips are the window's own: whole inside the window however close to the edge their control is,
  //    never the system's (which the desktop web view cuts at the panel edge), and the hover button they
  //    belong to keeps clear of the prompt rail.
  {
    const page = await open({ width: 1440, height: 900 });
    const message = page.locator('.panel-chat .message').filter({ has: page.locator('.message-artifact-button') }).first();
    await message.hover();
    const track = message.locator('.message-artifact-button');
    await expect(track).toBeVisible();
    const frame = await page.locator('.panel-chat .messages-frame').boundingBox(), button = await track.boundingBox();
    assert.ok(button.x + button.width <= frame.x + frame.width - 16, `the Track button keeps clear of the prompt rail (${Math.round(frame.x + frame.width - button.x - button.width)}px from the edge)`);
    const title = await track.getAttribute('title');
    await track.hover();
    const tip = page.getByRole('tooltip');
    await expect(tip).toBeVisible();
    await expect(tip).toHaveText(title);
    assert.equal(await track.getAttribute('title'), null, 'the system tooltip is not shown beside ours');
    const box = await tip.boundingBox();
    assert.ok(box.x >= 0 && box.x + box.width <= 1440 && box.y >= 0 && box.y + box.height <= 900, 'the tooltip is whole inside the window');
    assert.ok(await tip.evaluate(node => node.scrollWidth <= node.clientWidth + 1 && node.scrollHeight <= node.clientHeight + 1), 'the tooltip shows all of its text');
    assert.ok(box.y >= button.y + button.height || box.y + box.height <= button.y, 'the tooltip does not cover its own button');
    // A control at the very right edge of the window: still whole.
    const menu = page.locator('.workspace-header-actions summary[aria-label="Page actions"]');
    await menu.hover();
    await expect(tip).toHaveText('Page actions');
    const edge = await tip.boundingBox();
    assert.ok(edge.x + edge.width <= 1440 - 4, 'a tooltip at the window edge is pulled inside');
    // Moving away puts the title back for the next reader (and for assistive technology).
    await page.mouse.move(700, 450);
    await expect(tip).toBeHidden();
    assert.equal(await track.getAttribute('title'), title);
    await page.context().close();
  }

  // 6. A command you are asked to allow is shown whole at any width: wrapped, never cut or behind a sideways scroll.
  {
    const long = 'rm -rf services/front-door/dist && pnpm --filter front-door build --mode production --reporter=append-only --config ./configs/a/very/deep/path/production.config.ts';
    const beta = fixture.sessions[1];
    const approval = { type: 'approval', requestId: 'approval-long', text: 'Bash', providerDataJson: Buffer.from(JSON.stringify({ tool: 'Bash', input: { command: long, description: 'Clean and rebuild the front door' }, toolUseId: 'tool-long' })).toString('base64') };
    Object.assign(beta, { busy: true, status: 'waiting_approval', pending: [approval] });
    fixture.openEvents(beta.id).push(approval);
    for (const width of [1440, 800, 390]) {
      const page = await (await browser.newContext({ viewport: { width, height: 900 } })).newPage(); page.setDefaultTimeout(15000);
      await page.goto(url.replace('collection=alpha&session=alpha-session', 'collection=beta&session=beta-session'));
      const command = page.locator('.request .approval-what pre').first();
      await expect(command).toContainText('production.config.ts');
      const fit = await command.evaluate(node => ({ wide: node.scrollWidth - node.clientWidth, tall: node.scrollHeight - node.clientHeight, right: node.getBoundingClientRect().right }));
      assert.ok(fit.wide <= 1, `the command is not cut or scrolled sideways at ${width}px (${fit.wide}px hidden)`);
      assert.ok(fit.tall <= 1, `the whole command is on screen at ${width}px`);
      assert.ok(fit.right <= width, `the command stays inside the window at ${width}px`);
      await expect(page.locator('.request .approval-what').getByRole('button', { name: /Copy/ })).toBeVisible();
      // "Request details" is a control: tall enough to hit.
      assert.ok((await page.locator('.request .request-metadata > summary').first().boundingBox()).height >= 24, 'the details disclosure is at least 24px tall');
      if (width === 390) await page.screenshot({ path: '.cache/polish-pass-approval-phone.png' });
      await page.context().close();
    }
  }

  // 7. A question card taller than the window scrolls inside itself; its answer row stays in view.
  {
    const beta = fixture.sessions[1];
    const options = prefix => Array.from({ length: 6 }, (_, i) => ({ label: `${prefix} option ${i + 1}`, description: 'A description long enough to take a line of its own in the card.' }));
    const question = { type: 'question', requestId: 'question-tall', text: 'AskUserQuestion', providerDataJson: Buffer.from(JSON.stringify({ tool: 'AskUserQuestion', toolUseId: 'ask-tall', input: { questions: [{ question: 'Which database should the cache use?', header: 'Database', multiSelect: false, options: options('Database') }, { question: 'Which checks should run before merge?', header: 'Checks', multiSelect: true, options: options('Check') }] } })).toString('base64') };
    Object.assign(beta, { busy: true, status: 'waiting_input', pending: [question] });
    fixture.openEvents(beta.id).push(question);
    const page = await (await browser.newContext({ viewport: { width: 1280, height: 900 } })).newPage(); page.setDefaultTimeout(15000);
    await page.goto(url.replace('collection=alpha&session=alpha-session', 'collection=beta&session=beta-session'));
    const card = page.locator('.request.question-card'), send = card.getByRole('button', { name: 'Send answer' });
    await expect(card).toContainText('Check option 6');
    const messages = await page.locator('.panel-chat .messages').boundingBox(), box = await card.boundingBox();
    assert.ok(box.height <= messages.height + 1, `the card fits the conversation's height (${Math.round(box.height)} of ${Math.round(messages.height)}px)`);
    assert.ok(await card.evaluate(node => node.scrollHeight > node.clientHeight + 20), 'this card has more than fits, and scrolls inside itself');
    // The answer row is on screen at the top of the card's scroll and at its end.
    for (const top of [0, 100000]) {
      await card.evaluate((node, value) => { node.scrollTop = value; }, top);
      const now = await card.boundingBox(), button = await send.boundingBox();
      assert.ok(button.y >= now.y && button.y + button.height <= now.y + now.height + 1 && button.y + button.height <= 900, 'Send answer stays inside the card and the window');
    }
    await expect(card.getByText('Check option 6', { exact: true })).toBeVisible();
    await page.screenshot({ path: '.cache/polish-pass-question.png' });
    await page.context().close();
  }

  // 8. A setup script of several lines keeps its lines in the Actions panel, behind its first line.
  {
    const script = 'set -euo pipefail\nnpm ci\nnpm run build:proto\ncp .env.example .env';
    fixture.runtimeFor('alpha').setups[0].run = script;
    const page = await open({ width: 1440, height: 900 });
    await page.locator('.workspace-header-actions summary[aria-label="Panels"]').click();
    await page.locator('.workspace-header-actions .show-panel', { hasText: 'Actions' }).click();
    const row = page.locator('[data-runtime-row^="setup:"]').first(), disclosure = row.locator('.runtime-script');
    await expect(disclosure.locator('summary')).toContainText('set -euo pipefail');
    await expect(disclosure.locator('summary')).toContainText('4 lines');
    await expect(disclosure.locator('summary')).not.toContainText('npm ci');
    await disclosure.locator('summary').click();
    const pre = disclosure.locator('pre');
    await expect(pre).toBeVisible();
    assert.equal(await pre.evaluate(node => node.textContent), script, 'the script is shown as written');
    assert.equal(await pre.evaluate(node => getComputedStyle(node).whiteSpace), 'pre');
    assert.ok((await pre.boundingBox()).height >= 4 * 14, 'four lines are four lines');
    await page.context().close();
  }

  assert.deepEqual(errors, []);
  console.log('PASS polish pass: a narrow chat keeps its title and a whole message box; a phone reaches every page tab and keeps the step on one row; a page opened from the header closes with its ×; the header fits half a screen. Fixture only.');
} finally { await browser?.close(); await server.close(); await fixture.close(); }
