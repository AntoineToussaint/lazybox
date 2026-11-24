import {openWorkspaceChanges} from './fixtures/workspace-changes.mjs';
// Fixture only (R12, R13): chat-side polish and handing work to your own tools.
// - The header's Context meter is absent until the conversation has usage,
//   then shows; /context still opens it before.
// - With a chat on the page, the workspace bar no longer repeats the
//   conversation's title (it says how many there are).
// - At 1024×680 the Review tray goes under the files: the diff keeps its
//   whole height (≥300px) and no comment in it is covered.
// - "Continue in Claude Code…" copies `cd <folder> && claude --resume <id>`;
//   a separately kept subscription says why it cannot.
// - "Open in Terminal": in a browser, the `cd` to paste; with a desktop host
//   (mocked here), the chosen terminal at the workspace's folder, from the
//   palette and from a workspace row; Settings → Agents & workspaces chooses it.
// Screenshots in .cache/handoff-*.png, dark and light. WebKit.
import { testViteCache } from './fixtures/vite-cache.mjs';
import { createServer } from 'vite';
import react from '@vitejs/plugin-react';
import { webkit, expect } from './fixtures/playwright.mjs';
import { fileURLToPath } from 'node:url';
import { mkdir } from 'node:fs/promises';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';

const f = await startRunnerFixture();
const alpha = f.sessions.find(s => s.id === 'alpha-session'), beta = f.sessions.find(s => s.id === 'beta-session');
Object.assign(alpha, { title: 'Add a response cache', provider: 'claude', providerReference: '7c1e2a40-cache', status: 'done', busy: false, model: 'claude-opus-5-5', modelReported: true });
// Beta runs on a subscription Lazybox keeps apart from the person's own CLI sign-in.
Object.assign(beta, { title: 'Release notes', provider: 'codex', profileId: 'codex-saved', providerReference: 'thread-9', status: 'done', busy: false });
const api = f.shipState('alpha', 'api');
api.files = [{ path: 'src/handler.go', indexStatus: ' ', worktreeStatus: 'M' }];
f.diffs.set('alpha/api/src/handler.go', `diff --git a/src/handler.go b/src/handler.go\n--- a/src/handler.go\n+++ b/src/handler.go\n@@ -1,40 +1,41 @@\n${Array.from({ length: 40 }, (_, i) => ` line ${i + 1}`).join('\n')}\n+added\n`);
const usage = { groups: [{ scope: { execution: alpha.id, profile: 'claude-default' }, source: 'gateway', model: 'claude-opus-5-5', records: '4', unpriced: '0', incomplete: '0', nanos: '420000000', tokens: { input: '12000', output: '900' } }],
  contexts: [{ scope: { execution: alpha.id }, agent: 'root', used: '62000', capacity: '200000', reserve: '33000', source: 'sdk_summary_estimate', observedAt: new Date().toISOString(), categories: [{ name: 'messages', tokens: '62000', kind: 'used', included: true }] }], recent: [] };

const server = await createServer({ ...testViteCache(import.meta.url), configFile: false, logLevel: 'error', root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: f.url } } } });
let browser;
try {
  await server.listen(); await mkdir('.cache', { recursive: true });
  browser = await webkit.launch();
  const base = `http://127.0.0.1:${server.httpServer.address().port}/?view=workspace`;
  // What the page copied, without the system clipboard.
  const clipboard = () => { window.__copied = []; Object.defineProperty(navigator, 'clipboard', { configurable: true, value: { writeText: async text => { window.__copied.push(text); }, readText: async () => window.__copied.at(-1) || '', write: async () => {}, read: async () => [] } }); };
  const copied = page => page.evaluate(() => window.__copied.at(-1));
  /** The page's own module (same Vite URL, so the same instance): to stand in for the desktop host. */
  const moduleUrl = (page, file) => page.evaluate(name => performance.getEntriesByType('resource').map(e => e.name).find(n => n.includes(`/${name}`)), file);

  for (const scheme of ['dark', 'light']) {
    const context = await browser.newContext({ viewport: { width: 1440, height: 900 }, colorScheme: scheme });
    await context.addInitScript(clipboard);
    const page = await context.newPage(); page.setDefaultTimeout(15000);
    // Leaving a page cuts its in-flight polls, which WebKit reports as page errors ("access control checks") on the page being left: not failures. Anything else is.
    const errors = []; let leaving = 0; page.on('pageerror', e => { const text = String(e); if (leaving && /access control checks/.test(text)) return; errors.push(text); });
    const leave = async to => { leaving++; try { await page.goto(to); } finally { leaving--; } };
    let reported = false;
    await page.route('**/GetTelemetry', route => route.fulfill({ json: reported ? usage : { groups: [], contexts: [], recent: [] } }));
    await page.goto(`${base}&collection=alpha&session=${alpha.id}#token=${f.token}`);
    const chat = page.locator('.panel-chat'), header = chat.locator('.chat-heading');
    await expect(header.locator('.conversation-title-text')).toHaveText('Add a response cache');

    // R12 · No usage yet: no meter at all — not an empty grey bar.
    await page.waitForTimeout(500);
    await expect(header.getByRole('button', { name: 'Context and token cost' })).toBeHidden();
    assert.equal(await header.locator('.context-mini-bar').evaluateAll(bars => bars.filter(b => b.getClientRects().length).length), 0, 'no visible meter');
    // /context still opens it, and it says what it knows.
    await page.keyboard.press('Meta+k'); await page.keyboard.type('Show context and token cost'); await page.keyboard.press('Enter');
    const popover = page.locator('.context-popover:popover-open');
    await expect(popover).toContainText('Context usage not reported yet');
    await page.keyboard.press('Escape');
    await expect(popover).toHaveCount(0);
    // R12 · The workspace bar does not repeat the title the chat header shows.
    const bar = page.locator('.workspace-context');
    await expect(bar.locator('.session-switcher-face')).toHaveText(/\d+ conversations?/);
    assert.ok(!(await bar.innerText()).includes('Add a response cache'), 'the title is not repeated above the chat');
    await page.mouse.move(0, 0);
    await page.screenshot({ path: `.cache/handoff-header-before-usage-${scheme}.png`, clip: { x: 0, y: 0, width: 1440, height: 260 } });
    // Usage arrives: the meter shows, with its number.
    reported = true;
    await expect(header.getByRole('button', { name: 'Context and token cost' })).toContainText('31%', { timeout: 10000 });
    await page.screenshot({ path: `.cache/handoff-header-with-usage-${scheme}.png`, clip: { x: 0, y: 0, width: 1440, height: 260 } });

    // R12 · Continue in Claude Code…: the provider's own resume command, in the conversation's folder.
    await header.getByLabel('Session actions', { exact: true }).click();
    const menu = page.getByRole('group', { name: 'Session actions menu' });
    const resume = menu.getByRole('button', { name: 'Continue in Claude Code…' });
    await expect(resume).toBeEnabled();
    // No debug facts: the direct gateway and an account the CLI does not report go unsaid.
    await expect(menu).not.toContainText('Gateway');
    await expect(menu).not.toContainText('Account not reported');
    if (scheme === 'dark') await page.screenshot({ path: '.cache/handoff-session-menu-dark.png' });
    await resume.click();
    await expect.poll(() => copied(page)).toBe(`cd /fixture/collections/alpha && claude --resume 7c1e2a40-cache`);
    await expect(page.getByText(/Copied — paste it in your terminal/)).toBeVisible();
    // The same from the header's right-click.
    await header.locator('.conversation-title-text').click({ button: 'right' });
    await expect(page.getByRole('menuitem', { name: 'Continue in Claude Code…' })).toBeVisible();
    await expect(page.getByRole('menuitem', { name: /Open in Terminal/ })).toBeVisible();
    await page.keyboard.press('Escape');

    // R13 · In a browser, Open in Terminal hands over the cd to paste.
    await page.keyboard.press('Meta+k'); await page.keyboard.type('Open in Terminal'); await page.keyboard.press('Enter');
    const toast = page.locator('.toast').filter({ hasText: 'paste it in your terminal' }).last();
    await expect(toast).toContainText('cd /fixture/collections/alpha');
    await expect.poll(() => copied(page)).toBe('cd /fixture/collections/alpha');
    if (scheme === 'dark') await page.screenshot({ path: '.cache/handoff-terminal-web-dark.png' });

    // R12 · A conversation on a separate subscription says why it cannot continue in the CLI.
    await page.locator('.workspace-row', { hasText: 'Beta release' }).click({ button: 'right' });
    await expect(page.getByRole('menuitem', { name: /Open in Terminal/ })).toBeVisible();
    await page.keyboard.press('Escape');
    await leave(`${base}&collection=beta&session=${beta.id}#token=${f.token}`);
    await expect(header.locator('.conversation-title-text')).toHaveText('Release notes');
    await header.getByLabel('Session actions', { exact: true }).click();
    const codex = page.getByRole('group', { name: 'Session actions menu' }).getByRole('button', { name: 'Continue in Codex…' });
    await expect(codex).toBeDisabled();
    await expect(codex).toHaveAttribute('title', /Saved Codex.*apart/);
    await page.keyboard.press('Escape');

    if (scheme === 'dark') {
      // R13 · With a desktop host (here a stand-in): the chosen terminal, at the folder.
      await leave(`${base}&collection=alpha&session=${alpha.id}#token=${f.token}`);
      await expect(header.locator('.conversation-title-text')).toHaveText('Add a response cache');
      const handoff = await moduleUrl(page, 'handoff.tsx');
      assert.ok(handoff, 'the handoff module is loaded');
      // A stand-in desktop host: it keeps the custom command line itself and
      // saves one only when its (here simulated) native confirmation allows it.
      const configure = (installed, failure = '') => page.evaluate(async ([url, installed, failure]) => {
        const m = await import(url);
        m.configureExternalTerminal({
          installed: async () => installed,
          open: async request => { if (failure) throw new Error(failure); window.__opened.push(request); return { path: request.path }; },
          customCommand: async () => window.__saved,
          setCustomCommand: async command => { window.__asked.push(command); if (!window.__allow) throw new Error('Your terminal command was not saved'); window.__saved = command.trim(); return window.__saved; },
        });
      }, [handoff, installed, failure]);
      await page.evaluate(() => { window.__opened = []; window.__asked = []; window.__saved = ''; window.__allow = false; });
      await configure(['iterm', 'terminal']);
      await page.keyboard.press('Meta+k'); await page.keyboard.type('Open in Terminal'); await page.keyboard.press('Enter');
      await expect.poll(() => page.evaluate(() => window.__opened.at(-1))).toEqual({ path: '/fixture/collections/alpha', app: 'iterm' });
      // From a workspace row: that workspace's folder.
      await page.locator('.workspace-row', { hasText: 'Beta release' }).click({ button: 'right' });
      await page.getByRole('menuitem', { name: 'Open in Terminal' }).click();
      await expect.poll(() => page.evaluate(() => window.__opened.at(-1)?.path)).toBe('/fixture/collections/beta');
      // From a Files row: a file's folder.
      await page.getByRole('button', { name: 'Browse files', exact: true }).click();
      const readme = page.getByRole('tree', { name: 'Workspace files' }).locator('[data-path="api/README.md"]');
      await expect(readme).toBeVisible();
      await readme.click({ button: 'right' });
      await page.getByRole('menuitem', { name: 'Open in Terminal' }).click();
      await expect.poll(() => page.evaluate(() => window.__opened.at(-1)?.path)).toBe('/fixture/collections/alpha/api');
      // A failure says so, with the cd to fall back on.
      await configure(['iterm'], 'iTerm is not installed');
      await page.keyboard.press('Meta+k'); await page.keyboard.type('Open in Terminal'); await page.keyboard.press('Enter');
      await expect(page.locator('.toast').filter({ hasText: 'Could not open iTerm' })).toContainText('iTerm is not installed');
      await configure(['iterm', 'terminal']);
      // Settings → Workspaces → Your terminal: installed ones, a custom command.
      await page.getByRole('button', { name: 'Settings', exact: true }).click();
      await page.getByRole('tab', { name: 'Agents & workspaces', exact: true }).click();
      const choice = page.getByRole('combobox', { name: 'Terminal app' });
      await expect(choice.locator('option:checked')).toHaveText('Automatic · iTerm');
      await expect(choice.locator('option', { hasText: 'Ghostty' })).toHaveText('Ghostty (not installed)');
      await choice.selectOption('custom');
      const command = page.getByRole('textbox', { name: 'Command' });
      const save = page.getByRole('button', { name: 'Save', exact: true });
      await expect(command).toHaveValue('');
      await command.fill('alacritty --working-directory {dir}');
      // The host's confirmation refused: nothing is kept, and it says so.
      await save.click();
      await expect(page.getByRole('alert').filter({ hasText: 'Your terminal command was not saved' })).toBeVisible();
      assert.equal(await page.evaluate(() => window.__saved), '');
      // Allowed: the host keeps it.
      await page.evaluate(() => { window.__allow = true; });
      await save.click();
      await expect(page.getByRole('status').filter({ hasText: 'Saved.' })).toBeVisible();
      await expect(save).toBeDisabled();
      assert.deepEqual(await page.evaluate(() => window.__asked), ['alacritty --working-directory {dir}', 'alacritty --working-directory {dir}']);
      await page.mouse.move(0, 0);
      await page.screenshot({ path: '.cache/handoff-terminal-settings-dark.png' });
      await page.getByRole('button', { name: 'Work', exact: true }).click();
      await page.keyboard.press('Meta+k'); await page.keyboard.type('Open in Terminal'); await page.keyboard.press('Enter');
      // The request names the folder and "custom", never the command line: the host runs its own.
      await expect.poll(() => page.evaluate(() => window.__opened.at(-1))).toEqual({ path: '/fixture/collections/alpha', app: 'custom' });
      // A command line an earlier version kept in the page's storage is never run, only offered to save the same way.
      await page.evaluate(() => { window.__saved = ''; window.__asked = []; localStorage.setItem('lazybox:external-terminal', JSON.stringify({ app: 'custom', command: 'kitty --directory {dir}' })); });
      await page.keyboard.press('Meta+k'); await page.keyboard.type('Open in Terminal'); await page.keyboard.press('Enter');
      await expect(page.locator('.toast').filter({ hasText: 'Your terminal command is empty' })).toBeVisible();
      await page.getByRole('button', { name: 'Settings', exact: true }).click();
      await page.getByRole('tab', { name: 'Agents & workspaces', exact: true }).click();
      await expect(page.getByRole('textbox', { name: 'Command' })).toHaveValue('kitty --directory {dir}');
      assert.deepEqual(await page.evaluate(() => window.__asked), [], 'nothing is saved without the person');
      await page.getByRole('button', { name: 'Work', exact: true }).click();
    }

    // R12 · At 1024×680 the Review tray goes under the files; the diff keeps its height and width.
    await leave(`${base}&collection=alpha&session=${alpha.id}#token=${f.token}`);
    await page.setViewportSize({ width: 1024, height: 680 });
    await openWorkspaceChanges(page);
    const changes = page.getByRole('region', { name: 'Workspace changes' });
    await expect(changes.getByRole('grid', { name: 'File diff' })).toContainText('added');
    await changes.getByRole('button', { name: /^Review comments/ }).click();
    const tray = changes.getByRole('complementary', { name: 'Review' });
    await expect(tray).toBeVisible();
    await page.waitForTimeout(250);
    const diffHeight = await changes.locator('.diff-view').evaluate(node => node.getBoundingClientRect().height);
    assert.ok(diffHeight >= 300, `the diff keeps at least 300px beside the tray at 1024×680 (${Math.round(diffHeight)}px)`);
    const [diffBox, trayBox] = [await changes.locator('.diff-view').boundingBox(), await tray.boundingBox()];
    assert.ok(trayBox.x + trayBox.width <= diffBox.x + 1, 'the tray covers no part of the diff');
    await page.mouse.move(0, 0);
    await page.screenshot({ path: `.cache/handoff-review-1024-${scheme}.png` });
    // Escape closes it.
    await tray.getByRole('button', { name: /^All/ }).focus();
    await page.keyboard.press('Escape');
    await expect(tray).toHaveCount(0);
    assert.deepEqual(errors, [], `no page errors (${scheme})`);
    await context.close();
  }
  console.log('PASS: context meter hidden until usage, no repeated title, Review tray keeps the diff ≥300px at 1024, Continue in Claude Code / Codex…, Open in Terminal (browser cd, desktop host, settings). Fixture only.');
} finally { await browser?.close(); await server.close(); await f.close(); }
