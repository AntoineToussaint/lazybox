// Fixture only: unconfirmed ownership (RESOURCE_OWNERSHIP.md, "Unconfirmed
// ownership"). A workspace a previous Lazybox left "Owner uncertain" used to
// be a dead end: Delete was disabled with "reconcile the workspace" and
// nothing anywhere could. Now every place that waits for it (Delete
// workspace, Delete conversation, the row's New conversation, the workspace's
// page) shows what is unconfirmed (agent or command, PID, command line,
// running or not, as the Runner checked) and the two ways forward: Stop them
// and continue, or They're already stopped, continue. What the Runner proves
// stopped it clears without asking. All of it by keyboard alone.
// Screenshot: .cache/unconfirmed-ownership.png. Chromium.
import { testViteCache } from './fixtures/vite-cache.mjs';
import { createServer } from './fixtures/vite-cache.mjs';
import react from '@vitejs/plugin-react';
import { chromium, expect } from './fixtures/playwright.mjs';
import { fileURLToPath } from 'node:url';
import { mkdir } from 'node:fs/promises';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';
import { collectPageErrors, drainPageErrors } from './fixtures/page-errors.mjs';

const f = await startRunnerFixture();
const alpha = f.collections.find(c => c.id === 'alpha'), beta = f.collections.find(c => c.id === 'beta');
// Alpha: an agent that still runs and a command Lazybox cannot check.
alpha.ownershipUncertain = true;
f.ownership.set('alpha', { processes: [
  { kind: 'agent', sessionId: 'lost-agent', label: 'Fix the login flow', commandLine: 'node /opt/lazybox/claude-worker.js', pid: 48213, state: 'running', detail: 'It runs, and no Runner supervises it.' },
  { kind: 'command', sessionId: 'lost-agent', commandId: 'lost-command', label: 'npm run dev', commandLine: 'npm run dev -- --port 20000', pid: 0, state: 'unknown', detail: 'Lazybox recorded no process for it, so it cannot check.' },
] });
const since = method => f.mutations.filter(m => m.method === method);
const server = await createServer({ ...testViteCache(import.meta.url), configFile: false, logLevel: 'error', root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: f.url } } } });
let browser;
try {
  await server.listen(); await mkdir('.cache', { recursive: true });
  browser = await chromium.launch({ headless: true });
  const page = await browser.newPage({ viewport: { width: 1400, height: 950 } });
  page.setDefaultTimeout(10_000);
  const errors = []; await collectPageErrors(page, errors);
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace#token=${f.token}`);
  const nav = page.getByRole('navigation', { name: 'Workspaces' });
  // The sidebar's list is polled by more than one reader (every 3 s), so a
  // change to the fixture shows, and settles, within one poll: wait it out.
  const markShown = async () => {
    await expect(nav.getByRole('img', { name: 'Owner uncertain' })).toHaveCount(1);
    await page.waitForTimeout(3500);
    await expect(nav.getByRole('img', { name: 'Owner uncertain' })).toHaveCount(1);
  };
  await expect(nav.getByRole('button', { name: /Alpha feature/ }).first()).toBeVisible();

  // The row's mark says what it means and where the way forward is.
  const mark = nav.getByRole('img', { name: 'Owner uncertain' });
  await expect(mark).toHaveCount(1);
  assert.match(await mark.getAttribute('title'), /can’t confirm stopped.*Settle unconfirmed processes/s);

  // Delete workspace: what is unconfirmed, and both ways forward, inside the dialog.
  await nav.getByLabel('Actions for Alpha feature', { exact: true }).click();
  await page.getByRole('button', { name: 'Delete workspace…', exact: true }).click();
  const remove = page.getByRole('dialog', { name: 'Delete workspace' });
  const panel = remove.getByRole('region', { name: 'Unconfirmed processes' });
  await expect(panel).toBeVisible();
  await expect(panel).toContainText('A previous Lazybox left processes here that it can’t confirm stopped.');
  await expect(panel).toContainText('Deleting this workspace waits until that is settled.');
  const rows = panel.locator('.ownership-process');
  await expect(rows).toHaveCount(2);
  await expect(rows.nth(0)).toContainText('Agent Fix the login flow');
  await expect(rows.nth(0)).toContainText('Running');
  await expect(rows.nth(0)).toContainText('PID 48213 · node /opt/lazybox/claude-worker.js');
  await expect(rows.nth(1)).toContainText('Command npm run dev');
  await expect(rows.nth(1)).toContainText('Can’t check');
  await expect(rows.nth(1)).toContainText('No PID recorded · npm run dev -- --port 20000');
  const stop = panel.getByRole('button', { name: 'Stop them and continue' });
  const stopped = panel.getByRole('button', { name: 'They’re already stopped, continue' });
  await expect(stop).toBeEnabled();
  // Lazybox sees the agent running: the person's word is not offered against it.
  await expect(stopped).toBeDisabled();
  await expect(remove.getByRole('button', { name: 'Delete workspace' })).toBeDisabled();
  // The way forward has the focus; Escape cancels the whole dialog.
  await expect(stop).toBeFocused();
  await page.screenshot({ path: '.cache/unconfirmed-ownership.png' });
  await page.keyboard.press('Escape');
  await expect(remove).not.toBeVisible();
  assert.equal(since('ResolveOwnership').length, 0, 'Cancel settles nothing');

  // Again, by keyboard: Stop stops the running agent; the command it cannot
  // check is said plainly, and then the person's word settles it.
  await nav.getByLabel('Actions for Alpha feature', { exact: true }).focus();
  await page.keyboard.press('Enter');
  await page.getByRole('button', { name: 'Delete workspace…', exact: true }).focus();
  await page.keyboard.press('Enter');
  await expect(stop).toBeFocused();
  await page.keyboard.press('Enter');
  await expect(panel.getByRole('alert')).toContainText('Lazybox cannot check the command "npm run dev"');
  await expect(rows.nth(0)).toContainText('Stopped');
  assert.deepEqual(since('ResolveOwnership').map(m => m.body.stop), [true]);
  await expect(stopped).toBeEnabled();
  await stopped.focus();
  await page.keyboard.press('Enter');
  await expect(panel).toHaveCount(0);
  assert.deepEqual(since('ResolveOwnership').map(m => m.body.stop), [true, undefined]);
  assert.equal(alpha.ownershipUncertain, false);
  assert.match(alpha.lastOwnershipResolution, /You confirmed its processes had stopped/);
  // The dialog goes on as an ordinary deletion.
  await expect(remove.getByText('Workspace contents will be discarded')).toBeVisible();
  await page.keyboard.press('Escape');
  await expect(remove).not.toBeVisible();
  await expect(nav.getByRole('img', { name: 'Owner uncertain' })).toHaveCount(0);

  // What the Runner proves stopped (after a reboot, say) is cleared without
  // asking: New conversation goes straight on.
  beta.ownershipUncertain = true;
  f.ownership.set('beta', { provable: true, processes: [] });
  await markShown();
  await nav.getByRole('button', { name: 'New conversation in Beta release' }).click();
  await expect(page.getByRole('dialog', { name: 'Settle what a previous Lazybox left here' })).toHaveCount(0);
  await expect.poll(() => beta.ownershipUncertain).toBe(false);
  assert.equal(since('ResolveOwnership').length, 2, 'Nothing was asked of the person');
  await expect(nav.getByRole('img', { name: 'Owner uncertain' })).toHaveCount(0);
  const started = page.getByRole('dialog', { name: 'New conversation' });
  await expect(started).toBeVisible();
  await page.keyboard.press('Escape');
  await expect(started).not.toBeVisible();

  // New conversation in a workspace whose agent still runs: the same panel,
  // in a dialog; Stop, then the new conversation goes on.
  beta.ownershipUncertain = true;
  f.ownership.set('beta', { processes: [{ kind: 'agent', sessionId: 'lost-beta', label: 'Release notes', commandLine: 'codex app-server', pid: 51007, state: 'running', detail: 'It runs, and no Runner supervises it.' }] });
  await markShown();
  await nav.getByRole('button', { name: 'New conversation in Beta release' }).press('Enter');
  const settle = page.getByRole('dialog', { name: 'Settle what a previous Lazybox left here' });
  await expect(settle).toBeVisible();
  await expect(settle).toContainText('New conversations and deleting this workspace wait until that is settled.');
  await expect(settle).toContainText('PID 51007 · codex app-server');
  await expect(settle.getByRole('button', { name: 'Stop them and continue' })).toBeFocused();
  await page.keyboard.press('Enter');
  await expect(settle).not.toBeVisible();
  assert.equal(beta.ownershipUncertain, false);
  assert.deepEqual(since('ResolveOwnership').at(-1).body, { collectionId: 'beta', stop: true });
  // The new conversation the person asked for goes on.
  await expect(started).toBeVisible();
  await page.keyboard.press('Escape');
  await expect(started).not.toBeVisible();

  // Delete conversation: a conversation a previous Lazybox left needing recovery.
  const session = f.sessions.find(s => s.id === 'beta-session');
  Object.assign(session, { recoveryRequired: true, closed: true, status: 'interrupted' });
  f.ownership.set('beta-session', { processes: [{ kind: 'command', sessionId: 'beta-session', commandId: 'c1', label: 'go test ./...', commandLine: 'go test ./...', pid: 0, state: 'unknown', detail: 'Lazybox recorded no process for it, so it cannot check.' }] });
  await nav.getByLabel('Actions for Beta release', { exact: true }).click();
  await page.getByRole('button', { name: 'Delete conversation', exact: true }).click();
  const conversation = page.getByRole('dialog', { name: 'Unconfirmed conversation processes' });
  const conversationPanel = conversation.getByRole('region', { name: 'Unconfirmed processes' });
  await expect(conversationPanel).toContainText('Deleting this conversation waits until that is settled.');
  await expect(conversationPanel).toContainText('Command go test ./...');
  // Nothing runs: Stop has nothing to stop; the person's word is the way on.
  await expect(conversationPanel.getByRole('button', { name: 'Stop them and continue' })).toBeDisabled();
  await expect(conversationPanel.getByRole('button', { name: 'They’re already stopped, continue' })).toBeFocused();
  await page.keyboard.press('Enter');
  await expect(conversationPanel).toHaveCount(0);
  await expect(page.getByText('Conversation moved to Recently deleted',{exact:true})).toBeVisible();
  assert.equal(session.recoveryRequired, false);
  await page.keyboard.press('Escape');

  await drainPageErrors(errors);assert.deepEqual(errors, []);
  console.log('Unconfirmed ownership: what is unconfirmed is shown, settled by stopping or by the person’s word, cleared by itself when proved, all by keyboard.');
} finally {
  await browser?.close(); await server.close(); await f.close();
}
