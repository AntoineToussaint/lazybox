// Ask Lazybox against the fake Runner: it picks the agent by itself from the
// Runner's availability report (Claude when it can answer, Codex when every
// Claude subscription is at its limit), says which in its header with a menu
// to switch, starts on the first question with no form, and with nothing
// usable lists what each provider needs with the step that fixes it. Entry
// points: Help → Ask Lazybox…, ⌘K, ]] h — each focuses the message box. The
// welcome shows the same check. Fixture only: no agent runs.
import {createServer} from 'vite';
import react from '@vitejs/plugin-react';
import {chromium, expect} from './fixtures/playwright.mjs';
import {fileURLToPath} from 'node:url';
import {mkdir} from 'node:fs/promises';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import {testViteCache} from './fixtures/vite-cache.mjs';
import { collectPageErrors } from './fixtures/page-errors.mjs';

const fixture = await startRunnerFixture();
const server = await createServer({...testViteCache(import.meta.url), configFile: false, root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: {host: '127.0.0.1', port: 0, proxy: {'/lazybox.bench.v1.RunnerService/': {target: fixture.url}}}});
let browser;
try {
  await server.listen(); browser = await chromium.launch({headless: true});
  const context = await browser.newContext({viewport: {width: 1440, height: 900}});
  await context.addInitScript(() => Object.defineProperty(navigator, 'platform', {get: () => 'MacIntel'}));
  const page = await context.newPage(); page.setDefaultTimeout(15000);
  const errors = []; await collectPageErrors(page, errors);
  const origin = `http://127.0.0.1:${server.httpServer.address().port}`;
  await page.goto(`${origin}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`);
  await expect(page.getByRole('combobox', {name: 'Workspace', exact: true})).toHaveValue('alpha');
  await mkdir('.cache', {recursive: true});
  const panel = page.locator('section.help-assistant');
  const header = panel.locator('.help-assistant-header');
  const box = panel.getByRole('textbox', {name: 'Ask Lazybox'});
  const helpMenu = async item => { await page.locator('summary[aria-label="Help"]').click(); await page.getByRole('button', {name: item, exact: true}).click(); };

  // (1) Help → Ask Lazybox…: the panel opens on this page with its message box
  // focused, and picks Claude by itself (the CLI sign-in is available).
  await helpMenu('Ask Lazybox…');
  await expect(panel).toBeVisible();
  await expect(box).toBeFocused();
  await expect(header).toContainText('Help');
  await expect(header).toContainText('using Claude (Claude CLI sign-in)');
  await expect(panel.getByRole('list', {name: 'Suggested questions'}).getByRole('button')).toHaveCount(5);
  await page.screenshot({path: '.cache/help-ready.png'});
  // The switch menu lists every subscription with its state.
  await panel.locator('summary[aria-label="Choose the agent for Ask Lazybox"]').click();
  const menu = page.getByRole('group', {name: 'Choose the agent for Ask Lazybox menu'});
  await expect(menu.getByRole('button', {name: /Claude · Claude CLI sign-in/})).toHaveAttribute('aria-current', 'true');
  await expect(menu.getByRole('button', {name: /Codex · Saved Codex/})).toBeVisible();
  await page.keyboard.press('Escape');

  // A repeated panel operation suggests its shortcut and preserves an existing help draft.
  await box.fill('My existing question');
  await helpMenu('Training Wheels');
  const coach = page.locator('section.training-wheels');
  await coach.getByRole('checkbox', {name: 'Enable coaching'}).check();
  for (let i = 0; i < 4; i++) {
    await page.locator('.dv-groupview:visible .dock-maximize:visible').first().click();
  }
  await expect(coach.getByRole('heading', {name: 'Maximize a panel from the keyboard'})).toBeVisible();
  await coach.getByRole('button', {name: 'Ask Lazybox'}).click();
  const followup = panel.getByRole('textbox', {name: 'Question from Training Wheels'});
  await expect(followup).toHaveValue(/maximize/);
  await followup.fill('How do I change this shortcut?');
  await helpMenu('Training Wheels');
  await coach.getByRole('button', {name: 'Ask Lazybox'}).click();
  await expect(followup).toHaveValue('How do I change this shortcut?');
  await panel.getByRole('button', {name: 'Dismiss', exact: true}).click();
  await expect(box).toHaveValue('My existing question');
  assert.equal(fixture.mutations.filter(m => m.method === 'OpenAssistant').length, 0);

  // The first question starts the assistant: no form, Enter sends it.
  await box.focus();
  const yaml = 'name: Review wide\nlayout:\n  panels: [chat, changes]';
  const proposal = fixture.openEvents('assistant-' + fixture.sessions.length);
  proposal.push({type: 'message_delta', text: ['```lazybox-layout', yaml, '```'].join('\n')});
  proposal.push({type: 'turn_ended'});
  await box.fill('How do I add a second subscription?');
  await page.keyboard.press('Enter');
  await expect.poll(() => fixture.mutations.filter(m => m.method === 'OpenAssistant').length).toBe(1);
  const opened = fixture.mutations.find(m => m.method === 'OpenAssistant');
  assert.equal(opened.body.profileId, 'claude-default');
  assert.equal(opened.body.purpose, 'help');
  // Its usage is attributed to the workspace it is asked from, which the assistant is told; it is not listed as the workspace's work.
  assert.equal(opened.body.collectionId, 'alpha');
  assert.match(opened.body.context, /Alpha feature/);
  assert.match(opened.body.context, /Docked layout[\s\S]*layout:/);
  await expect.poll(() => fixture.mutations.some(m => m.method === 'Apply' && m.body.message?.text === 'How do I add a second subscription?')).toBe(true);
  const sessionId = fixture.sessions.at(-1).id;
  assert.equal(fixture.mutations.find(m => m.method === 'Apply' && m.body.message?.text).body.sessionId, sessionId);
  // The conversation replaces the start, and the header still says who answers.
  await expect(box).toHaveCount(0);
  await expect(panel.locator('.agent-window')).toBeVisible();
  await expect(header).toContainText('using Claude (Claude CLI sign-in)');
  await panel.getByRole('button', {name: 'Review layout'}).click();
  const importer = page.getByRole('dialog', {name: 'Import a layout'});
  await expect(importer.getByRole('textbox', {name: 'Layout YAML'})).toHaveValue(yaml);
  await importer.getByRole('button', {name: 'Cancel'}).click();
  await expect(importer).toHaveCount(0);
  await page.screenshot({path: '.cache/help-conversation.png'});
  // Help is not the workspace's work: the sidebar does not list it.
  await expect(page.getByRole('navigation', {name: 'Workspaces'}).getByText('Conversation alpha-session')).toBeVisible();
  await expect(page.getByRole('navigation', {name: 'Workspaces'})).not.toContainText(sessionId);
  // A new help conversation goes back to the start, focused.
  await panel.getByRole('button', {name: 'New help conversation'}).click();
  await expect(box).toBeFocused();

  // (2) Every Claude subscription at its limit: Ask Lazybox falls back to
  // Codex (the saved subscription first) and says Claude is not ready.
  const backAt = Math.floor(Date.now() / 1000) + 3600;
  // After 23:00 local time the reset is past midnight, and the line names the day.
  const backDay = new Date(backAt * 1000).toDateString() === new Date().toDateString() ? '' : 'tomorrow ';
  fixture.availability.states['claude-default'] = {state: 'limited', resetAt: backAt};
  await panel.locator('summary[aria-label="Choose the agent for Ask Lazybox"]').click();
  await menu.getByRole('button', {name: 'Check again'}).click();
  await expect(header).toContainText('using Codex (Saved Codex)');
  await expect(panel.locator('.help-assistant-fallback')).toHaveText('Claude is not ready, so Codex answers.');
  assert.ok(fixture.availability.refreshes >= 1, 'Check again probes the programs again');
  await panel.locator('summary[aria-label="Choose the agent for Ask Lazybox"]').click();
  await expect(menu.getByRole('button', {name: /Claude · Claude CLI sign-in/})).toContainText(`Limit reached · back ${backDay}at`);
  await page.keyboard.press('Escape');
  await page.screenshot({path: '.cache/help-fallback.png'});
  await panel.getByRole('button', {name: 'How do I run my app from a workspace?'}).click();
  await expect.poll(() => fixture.mutations.filter(m => m.method === 'OpenAssistant').length).toBe(2);
  assert.equal(fixture.mutations.filter(m => m.method === 'OpenAssistant')[1].body.profileId, 'codex-saved');
  await panel.getByRole('button', {name: 'New help conversation'}).click();

  // (3) Nothing usable: Claude Code missing, Codex signed out. A clear list of
  // what each needs, with its fix; never a raw error.
  fixture.availability.claude.installed = false;
  fixture.availability.states['codex-default'] = {state: 'sign_in', detail: 'Not signed in'};
  fixture.availability.states['codex-saved'] = {state: 'sign_in', detail: 'Needs you to sign in again'};
  await panel.locator('summary[aria-label="Choose the agent for Ask Lazybox"]').click();
  await menu.getByRole('button', {name: 'Check again'}).click();
  const empty = panel.getByRole('region', {name: 'No agent is ready to answer'});
  await expect(empty).toBeVisible();
  await expect(header).toContainText('no agent ready');
  await expect(panel.getByRole('alert')).toHaveCount(0);
  const claudeNeed = empty.getByRole('listitem', {name: 'Claude Code'}), codexNeed = empty.getByRole('listitem', {name: 'Codex'});
  await expect(claudeNeed).toContainText('Not installed');
  await expect(claudeNeed.locator('code')).toHaveText('curl -fsSL https://claude.ai/install.sh | bash');
  await expect(codexNeed).toContainText('Installed 0.98.0 · not signed in');
  await page.screenshot({path: '.cache/help-nothing.png'});
  // Install guide: the offline docs, at the section that explains it.
  await claudeNeed.getByRole('button', {name: 'How to install Claude Code'}).click();
  const docs = page.getByRole('dialog', {name: 'Documentation'});
  await expect(docs).toBeVisible();
  await expect(docs.locator('.docs-page h1')).toHaveText('Troubleshooting');
  await expect(docs.locator('#ask-lazybox-says-no-agent-is-ready')).toBeInViewport();
  await docs.getByRole('button', {name: 'Close documentation'}).click();
  // Read the docs: the documentation home.
  await empty.getByRole('button', {name: /Read the docs/}).click();
  await expect(docs.locator('.docs-page h1')).toHaveText('Lazybox documentation');
  // Screenshots in the window's theme: this window is light, so each picture's light twin shows.
  await docs.locator('.docs-page').getByRole('link', {name: 'Getting started', exact: true}).first().click();
  await expect(docs.locator('.docs-page h1')).toHaveText(/Getting started/);
  await expect(docs.locator('.docs-page img').first()).toHaveAttribute('src', /welcome-light\.webp/);
  await docs.getByRole('button', {name: 'Close documentation'}).click();
  // Add a subscription: Settings → Subscriptions.
  await codexNeed.getByRole('button', {name: 'Add a ChatGPT subscription…'}).click();
  await expect(page.getByRole('tab', {name: 'Subscriptions'})).toHaveAttribute('aria-selected', 'true');

  // (4) Entry points: ⌘K "Ask Lazybox" and ]] h bring the panel back from
  // Settings and focus its message box.
  fixture.availability.claude.installed = true;
  delete fixture.availability.states['claude-default'];
  await page.keyboard.press('Meta+k');
  const palette = page.getByRole('dialog', {name: 'Command palette'});
  await palette.getByRole('combobox', {name: 'Search commands'}).fill('ask lazybox');
  await expect(palette.getByRole('option').first()).toContainText('Ask Lazybox');
  await expect(palette.getByRole('option').first().locator('kbd')).toHaveText([']', ']', 'h']);
  await page.keyboard.press('Enter');
  await expect(panel).toBeVisible();
  await panel.locator('summary[aria-label="Choose the agent for Ask Lazybox"]').click();
  await menu.getByRole('button', {name: 'Check again'}).click();
  await expect(header).toContainText('using Claude (Claude CLI sign-in)');
  await page.locator('.workspace-brand').click();
  await expect(box).not.toBeFocused();
  await page.keyboard.type(']]h');
  await expect(box).toBeFocused();
  // The ? overlay lists it too.
  await page.locator('.workspace-brand').click();
  await page.keyboard.press('?');
  const overlay = page.getByRole('dialog', {name: 'Keyboard shortcuts'});
  await expect(overlay).toContainText('Ask Lazybox');
  await page.keyboard.press('Escape');

  // (5) The welcome shows the same check.
  fixture.availability.codex.installed = false;
  await helpMenu('Welcome & getting started');
  const agents = page.getByRole('dialog', {name: 'Welcome to Lazybox'}).getByRole('list', {name: 'Agents on this computer'});
  await expect(agents.getByRole('listitem')).toHaveText(['Claude Code 2.1.311 installed · signed in', 'Codex not installed']);
  await page.screenshot({path: '.cache/help-welcome.png'});
  await page.keyboard.press('Escape');

  await helpMenu('Ask Lazybox…');
  const reply = fixture.openEvents(`assistant-${fixture.sessions.length}`);
  reply.push({type: 'message_delta', text: '[Add subscription](#lazybox-add-subscription)'});
  reply.push({type: 'turn_ended'});
  await box.fill('Add a second subscription');
  await box.press('Enter');
  await panel.getByRole('button', {name: 'Add subscription', exact: true}).click();
  await expect(page.getByRole('dialog', {name: 'Connect a subscription'})).toBeVisible();

  assert.deepEqual(errors, []);
  console.log('PASS: Ask Lazybox picks the agent itself (Claude, then Codex at a limit), names it in the header with a switch menu, starts on the first question, explains what is missing with its fixes and the docs, opens from Help, ⌘K, ]] h and ?, and the welcome shows the same check. Fixture only.');
} finally { await browser?.close(); await server.close(); await fixture.close(); }
