// Fixture only: the chat header with the conversation from the report — a long
// first prompt, a branch, a non-default subscription, 26 prompts, 19 finished background
// tasks and one still running, 40.4% of the context, $13.65 and 276 tool calls of which 22 failed.
// At 1440 the title has the first line and everything shows; in a 560px panel
// the title keeps two clean lines and what the second line has no room for
// folds whole into "⋯". Nothing in the header is cut inside a word. A click on
// the title shows the first message; a double-click renames. Screenshots in
// .cache/chat-header-{wide,narrow}-{dark,light}.png. WebKit, the desktop
// app's engine.
import { testViteCache } from './fixtures/vite-cache.mjs';
import { createServer } from './fixtures/vite-cache.mjs';
import react from '@vitejs/plugin-react';
import { webkit, expect } from './fixtures/playwright.mjs';
import { fileURLToPath } from 'node:url';
import { mkdir } from 'node:fs/promises';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';
import { collectPageErrors, drainPageErrors } from './fixtures/page-errors.mjs';

// The prompt from the report, which the header showed cut mid-sentence.
const PROMPT = 'I want to start a plan, we want to look at SOTA but the goal is to write down a design for how Lazybox names conversations and lays out the chat header, then build it with tests.';
// What the Runner shows before a title is generated (titles.Fallback), and the generated one.
const FALLBACK = 'Start a plan, we want to look at SOTA but the goal…';
const GENERATED = 'Conversation titles and chat header layout';

const f = await startRunnerFixture();
f.profiles.push(...['Work subscription', 'Personal'].map((label, i) => ({ id: `claude-${i}`, provider: 'claude', label, managed: true, isolation: 'environment' })));
for (const member of f.collections[0].members) member.spec.branch = 'feature/strongly-tight-collie';
const session = f.sessions[0];
Object.assign(session, { title: FALLBACK, provider: 'claude', model: 'claude-opus-5-5', modelReported: true, profileId: 'claude-0', status: 'done', busy: false,
  agents: [{ id: 'root', status: 'done', turnStartedAtUnixMillis: String(Date.now() - 90_000), turnEndedAtUnixMillis: String(Date.now() - 30_000), lastTurnStatus: 'completed' }] });
const b64 = value => Buffer.from(JSON.stringify(value)).toString('base64');
const channel = f.openEvents(session.id);
const push = (event, at) => channel.push(event.data === undefined ? event : { ...Object.fromEntries(Object.entries(event).filter(([key]) => key !== 'data')), providerDataJson: b64(event.data) }, new Date(at).toISOString());
{
  let at = Date.now() - 3_600_000;
  for (let p = 0; p < 26; p++) {
    push({ type: 'operation_accepted', text: p ? `Follow-up ${p}: keep going with the plan.` : PROMPT, operationId: `op-${p}` }, at += 1000);
    push({ type: 'turn_started' }, at += 100);
    for (let t = 0; t < (p < 16 ? 11 : 10); t++) {
      const id = `tool-${p}-${t}`, failed = p < 22 && t === 0;
      push({ type: 'tool_activity', data: { type: 'tool_call', id, name: t % 2 ? 'Read' : 'Bash', status: failed ? 'failed' : 'completed', input: t % 2 ? { file_path: `src/file-${t}.ts` } : { command: `pnpm test --filter ${t}`, description: 'Run tests' }, output: failed ? 'exit 1' : 'ok' } }, at += 50);
    }
    if (p < 19) push({ type: 'agent_status', agentId: `claude-tool:bg-${p}`, parentAgentId: 'root', nativeId: `task-${p}`, status: 'completed', name: `Research step ${p + 1}`, data: { toolUseId: `bg-${p}`, subagentType: 'Explore' } }, at += 50);
    push({ type: 'message_delta', text: `Step ${p + 1} is done.` }, at += 100);
    push({ type: 'turn_ended', status: 'completed' }, at += 100);
  }
  // One sub-agent still at work in the background: the header's Background work chip counts only active ones (#126).
  push({ type: 'agent_status', agentId: 'claude-tool:bg-live', parentAgentId: 'root', nativeId: 'task-live', status: 'running', name: 'Watch the build', data: { toolUseId: 'bg-live', subagentType: 'general-purpose', backgrounded: true } }, at += 100);
}
// 26 turns × 10–11 calls: 276 tool calls, 22 of them failed.
const telemetry = { groups: [{ scope: { execution: session.id, profile: 'claude-0' }, source: 'gateway', model: 'claude-opus-5-5', records: '276', unpriced: '0', incomplete: '0', nanos: '13650000000', tokens: { input: '1200000', output: '90000' } }],
  contexts: [{ scope: { execution: session.id }, agent: 'root', used: '404000', capacity: '1000000', reserve: '33000', source: 'sdk_summary_estimate', observedAt: new Date().toISOString(), categories: [{ name: 'messages', tokens: '404000', kind: 'used', included: true }] }], recent: [] };

const server = await createServer({ ...testViteCache(import.meta.url), configFile: false, logLevel: 'error', root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: f.url } } } });
let browser;
try {
  await server.listen(); await mkdir('.cache', { recursive: true });
  browser = await webkit.launch();
  const url = `http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=${session.id}#token=${f.token}`;

  /** Nothing on the header's second line runs past its edge or hides part of itself. */
  const nothingCut = async (header, why) => {
    const report = await header.evaluate(node => {
      const meta = node.querySelector('.chat-heading-meta').getBoundingClientRect(), out = [];
      for (const el of node.querySelectorAll('.chat-heading-meta *')) {
        const box = el.getBoundingClientRect();
        if (!box.width || el.closest('[popover]')) continue;
        if (box.right > meta.right + 1 || box.left < meta.left - 1) out.push(`${el.className || el.tagName} runs past the line: ${Math.round(box.left)}–${Math.round(box.right)} of ${Math.round(meta.left)}–${Math.round(meta.right)}`);
        const style = getComputedStyle(el);
        if (el.children.length === 0 && el.textContent.trim() && style.overflow === 'hidden' && el.scrollWidth > el.clientWidth + 1) out.push(`"${el.textContent.trim()}" is cut (${el.scrollWidth} > ${el.clientWidth})`);
      }
      const about = node.querySelector('.chat-heading-about');
      if (about.scrollWidth > about.clientWidth + 1) out.push(`the line hides ${about.scrollWidth - about.clientWidth}px`);
      return out;
    });
    assert.deepEqual(report, [], `${why}: ${report.join('; ')}`);
  };
  const titleLines = title => title.evaluate(node => { const style = getComputedStyle(node); return Math.round(node.getBoundingClientRect().height / parseFloat(style.lineHeight)); });

  for (const scheme of ['dark', 'light']) {
    const context = await browser.newContext({ viewport: { width: 1440, height: 900 }, colorScheme: scheme });
    const page = await context.newPage(); page.setDefaultTimeout(15000);
    const errors = []; await collectPageErrors(page, errors);
    await page.route('**/GetTelemetry', route => route.fulfill({ json: telemetry }));
    session.title = FALLBACK;
    await page.goto(url);
    const chat = page.locator('.panel-chat'), header = chat.locator('.chat-heading'), title = header.locator('.conversation-title-text');

    // Wide: the fallback title is whole words; the second line says it all.
    await expect(title).toHaveText(FALLBACK);
    await expect(header.locator('.chat-status')).toHaveText('Turn done');
    await expect(header.locator('.chat-meta-branch')).toHaveText('feature/strongly-tight-collie');
    await expect(header.locator('.chat-model')).toHaveText('Claude · Opus 5.5');
    await expect(header.locator('.chat-account')).toHaveText('Work subscription');
    await expect(header.getByRole('button', { name: 'Your prompts' })).toContainText('26');
    await expect(header.getByRole('button', { name: /^Background work: .*\d+ (sub-agents?|commands?)/ })).toBeVisible();
    await expect(header.getByRole('button', { name: 'Context and token cost' })).toContainText('40.4%');
    await expect(header.getByRole('button', { name: 'Context and token cost' })).toContainText('$13.65');
    const tools = header.getByRole('button', { name: 'View tools and activity' });
    await expect(tools.locator('.tool-activity-count')).toContainText('276');
    await expect(tools.locator('.tool-activity-failed')).toHaveText('22 failed');
    await expect(tools).toHaveAttribute('title', /276 tool calls, 22 failed/);
    await expect(header.locator('.chat-details-button'), 'everything fits: no ⋯').toBeHidden();
    assert.equal(await titleLines(title), 1, 'a short title keeps one line');
    await nothingCut(header, `wide ${scheme}`);
    // The title's tooltip has the whole first message.
    assert.ok((await title.getAttribute('title')).includes(PROMPT), 'the first message is in the tooltip');

    // The title the Runner generated arrives; the header follows.
    session.title = GENERATED;
    await expect(title).toHaveText(GENERATED, { timeout: 15000 });
    await page.mouse.move(0, 0);
    await header.screenshot({ path: `.cache/chat-header-wide-${scheme}.png` });
    await page.screenshot({ path: `.cache/chat-header-page-wide-${scheme}.png` });

    // A click shows the first message, whole; Escape closes it.
    await title.click();
    const first = page.getByRole('dialog', { name: 'First message' });
    await expect(first).toBeVisible();
    await expect(first.locator('.first-message-text')).toHaveText(PROMPT);
    {
      const box = await first.boundingBox(), anchor = await title.boundingBox();
      assert.ok(box.y >= anchor.y + anchor.height - 1 && box.y < anchor.y + anchor.height + 20, 'the first message opens under the title');
    }
    if (scheme === 'dark') await page.screenshot({ path: '.cache/chat-header-first-message.png' });
    await page.keyboard.press('Escape');
    await expect(first).toBeHidden();

    // Narrow: the chat in a 560px panel. The window narrows until the panel is.
    let width = 1440;
    for (let i = 0; i < 6; i++) {
      const panel = await chat.evaluate(node => node.getBoundingClientRect().width);
      if (Math.abs(panel - 560) <= 4) break;
      width = Math.round(width - (panel - 560)); await page.setViewportSize({ width, height: 900 });
      await page.waitForTimeout(150);
    }
    const panelWidth = await chat.evaluate(node => Math.round(node.getBoundingClientRect().width));
    assert.ok(Math.abs(panelWidth - 560) <= 6, `the chat panel is ${panelWidth}px wide`);
    // A long name the person gave it: two clean lines, never cut inside a word on the first.
    session.title = 'Conversation titles, the chat header layout and every place a conversation is named in Lazybox';
    await expect(title).toHaveText(session.title, { timeout: 15000 });
    assert.ok(await titleLines(title) <= 2, 'the title takes at most two lines');
    const more = header.locator('.chat-details-button');
    await expect(more, 'what does not fit folds into ⋯').toBeVisible();
    await expect(header.locator('.chat-status')).toBeVisible();
    await page.mouse.move(0, 0);
    await header.screenshot({ path: `.cache/chat-header-narrow-${scheme}.png` });
    await page.screenshot({ path: `.cache/chat-header-page-narrow-${scheme}.png` });
    await nothingCut(header, `narrow ${scheme}`);
    // ⋯ holds the rest, with their words, and they still work.
    await more.click();
    const details = page.getByRole('group', { name: 'Conversation details' });
    await expect(details).toBeVisible();
    const folded = await details.locator('.chat-details-row:visible .chat-details-label').allTextContents();
    assert.ok(folded.length > 0, 'the details list what folded');
    if (folded.includes('Tool calls')) await expect(details.getByRole('button', { name: 'View tools and activity' })).toContainText('276 tool calls');
    if (folded.includes('Prompts')) await expect(details.getByRole('button', { name: 'Your prompts' })).toContainText('26 prompts');
    if (scheme === 'dark') await page.screenshot({ path: '.cache/chat-header-details.png' });
    if (folded.includes('Prompts')) {
      await details.getByRole('button', { name: 'Your prompts' }).click();
      await expect(page.getByRole('dialog', { name: 'Jump to a prompt' })).toBeVisible();
      await page.keyboard.press('Escape');
    }
    await page.keyboard.press('Escape');

    // Double-click renames in place; the person's name is what everyone shows.
    if (scheme === 'dark') {
      await page.setViewportSize({ width: 1440, height: 900 });
      await title.dblclick();
      const box = header.getByRole('textbox', { name: 'Conversation name' });
      await expect(box).toBeFocused();
      await expect(page.getByRole('dialog', { name: 'First message' }), 'a double-click does not open the first message').toBeHidden();
      await box.fill('Header and titles');
      await box.press('Enter');
      await expect(title).toHaveText('Header and titles');
      assert.equal(f.mutations.findLast(m => m.method === 'RenameConversation').body.title, 'Header and titles');
      // The same name in the sidebar row, the Agents panel and the tab.
      await expect(page.locator('.workspace-row').filter({ hasText: 'Header and titles' }).first()).toBeVisible({ timeout: 15000 });
      await expect(page.locator('.dv-tab .dock-tab-title').filter({ hasText: 'Header and titles' })).toHaveCount(1);
      // The ⋯ menu renames too.
      await header.locator('.session-menu > summary').click();
      await page.getByRole('button', { name: 'Rename…' }).click();
      await expect(header.getByRole('textbox', { name: 'Conversation name' })).toBeFocused();
      await page.keyboard.press('Escape');
      await expect(title).toHaveText('Header and titles');

      // Settings → Agents & workspaces turns generated titles off (on by default).
      await page.getByRole('button', { name: 'Settings', exact: true }).click();
      await page.getByRole('tab', { name: 'Agents & workspaces', exact: true }).click();
      const generated = page.getByRole('switch', { name: 'Name conversations after their first turn' });
      await expect(generated).toBeChecked();
      await page.locator('.settings-row').filter({ has: generated }).screenshot({ path: '.cache/chat-header-setting.png' });
      await generated.uncheck();
      await page.getByRole('button', { name: 'Save agent settings' }).click();
      await expect(page.getByRole('status').filter({ hasText: 'Agent settings saved.' })).toBeVisible();
      assert.equal(f.mutations.findLast(m => m.method === 'SaveRunnerSettings').body.plainTitles, true);
    }
    await drainPageErrors(errors);assert.deepEqual(errors, []);
    await context.close();
  }
  console.log('chat header: titles whole, nothing cut, ⋯ at narrow widths; screenshots in .cache/chat-header-*.png');
} finally {
  await browser?.close(); await server.close(); await f.close?.();
}
