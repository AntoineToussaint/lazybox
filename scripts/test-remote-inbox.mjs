// The phone (REMOTE.md), paired with a device credential: its inbox groups
// what needs the person across workspaces; a conversation shows its
// transcript and its pending request; the person approves, answers and
// replies from the phone, and the back gesture returns to the inbox. A
// read-only phone sees everything and can send nothing. Phone widths 320 and
// 390 keep everything on screen. Fake Runner; no agent runs.
import { createServer } from 'vite';
import react from '@vitejs/plugin-react';
import { webkit, chromium, devices, expect } from '@playwright/test';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';
import { testViteCache } from './fixtures/vite-cache.mjs';

const f = await startRunnerFixture();
const now = Date.now(), ms = value => String(value);
const data = value => Buffer.from(JSON.stringify(value)).toString('base64');
const root = (status, updated, extra = {}) => [{ id: 'root', name: 'Main agent', status, updatedAtUnixMillis: ms(updated), ...extra }];
const base = { profileId: 'claude-default', provider: 'claude', capabilities: { files: true }, pending: [] };
Object.assign(f.sessions[0], { title: 'Index the API', model: 'claude-fable-5', busy: true, agents: root('running', now - 5_000) });
Object.assign(f.sessions[1], { title: 'Ship release notes', provider: 'codex', model: 'gpt-codex-6', status: 'waiting_input', agents: root('waiting_input', now - 300_000),
  pending: [{ type: 'question', requestId: 'q-release', providerDataJson: data({ questions: [{ question: 'Which version should the notes cover?' }] }) }] });
f.sessions.push(
  { ...base, id: 'approval-session', collectionId: 'beta', title: 'Bump dependencies', model: 'claude-fable-5', status: 'waiting_approval', createdAtUnixMillis: ms(now - 5_000_000), agents: root('waiting_approval', now - 30_000),
    pending: [{ type: 'approval', requestId: 'a-deps', text: 'Bash', providerDataJson: data({ input: { command: 'npm update' } }) }] },
  { ...base, id: 'failing-session', collectionId: 'beta', title: 'Deploy preview', model: 'claude-fable-5', status: 'failed', fault: 'provider delivery failed; reconcile before continuing', createdAtUnixMillis: ms(now - 7_200_000), agents: root('failed', now - 20_000) },
  { ...base, id: 'review-session', collectionId: 'alpha', title: 'Refactor the parser', model: 'claude-fable-5', status: 'running', busy: true, createdAtUnixMillis: ms(now - 3_600_000), agents: root('running', now - 10_000) },
);
f.remote.tailscale = { ...f.remote.tailscale, enabled: true, state: 'serving' };

const proxy = { '/lazybox.bench.v1.RunnerService/': { target: f.url }, '/remote/v1/': { target: f.url } };
const server = await createServer({ ...testViteCache(import.meta.url), configFile: false, root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: { host: '127.0.0.1', port: 0, proxy } });
let browser;
try {
  await server.listen();
  const origin = `http://127.0.0.1:${server.httpServer.address().port}`;
  f.remote.phoneUrl = origin;
  browser = await (process.env.TEST_CHROMIUM ? chromium : webkit).launch();
  // The owner shows a code on the computer (as Settings → Devices does).
  const offer = async grants => {
    const response = await fetch(`${f.url}/lazybox.bench.v1.RunnerService/BeginDevicePairing`, { method: 'POST', headers: { authorization: `Bearer ${f.token}`, 'content-type': 'application/json' }, body: JSON.stringify({ grants }) });
    return (await response.json()).url;
  };
  const errors = [];
  const pairPhone = async (grants, name) => {
    const context = await browser.newContext({ ...devices['iPhone 13'], ...(process.env.TEST_CHROMIUM ? { isMobile: false } : {}) });
    const page = await context.newPage(); page.on('pageerror', e => errors.push(String(e)));
    await page.goto(await offer(grants));
    await page.getByRole('textbox', { name: 'Name this phone' }).fill(name);
    await page.getByRole('button', { name: 'Pair this phone' }).click();
    await page.getByRole('button', { name: 'Open inbox' }).click();
    return page;
  };
  const phone = await pairPhone(['respond'], 'iPhone');

  // The inbox: what needs the person first, across workspaces, newest first.
  await expect(phone.getByRole('heading', { name: 'Inbox' })).toBeVisible();
  // A turn that ends while the phone is open is ready for review.
  await expect(phone.locator('.group-working .remote-row')).toHaveCount(2);
  Object.assign(f.sessions.find(s => s.id === 'review-session'), { status: 'done', busy: false, agents: root('done', Date.now(), { turnEndedAtUnixMillis: ms(Date.now()), lastTurnStatus: 'completed' }), lastMessage: 'I split the parser into three files.\n\n**All 42 tests pass.**' });
  await expect(phone.locator('.group-review .remote-row')).toHaveCount(1, { timeout: 10_000 });
  await expect(phone.locator('.remote-bar-title [role=status]')).toHaveText('2 waiting for you');
  await expect.poll(() => phone.title()).toBe('(2) Lazybox');
  const groups = () => phone.locator('.remote-group h2').allInnerTexts().then(v => v.map(t => t.replace(/\s+/g, ' ').trim()));
  assert.deepEqual(await groups(), ['NEEDS INPUT 2', 'FAILED 1', 'READY FOR REVIEW 1', 'WORKING 1']);
  assert.deepEqual(await phone.locator('.group-needs-input .remote-row strong').allInnerTexts(), ['Bump dependencies', 'Ship release notes']);
  const approvalRow = phone.locator('.remote-row').filter({ hasText: 'Bump dependencies' });
  await expect(approvalRow).toContainText('Beta release');
  await expect(approvalRow.locator('.remote-row-line')).toHaveText('Approval');
  await expect(approvalRow.locator('.remote-row-what')).toHaveText('npm update');
  await expect(approvalRow.getByRole('button', { name: 'Allow' })).toBeVisible();
  await expect(phone.locator('.remote-row').filter({ hasText: 'Deploy preview' }).locator('.remote-row-line')).toContainText('A message could not be delivered');
  for (const width of [320, 390]) {
    await phone.setViewportSize({ width, height: 700 });
    assert.equal(await phone.evaluate(() => document.documentElement.scrollWidth > innerWidth), false, `no sideways scrolling at ${width}`);
    for (const box of await phone.locator('.remote-row-open').evaluateAll(rows => rows.map(row => row.getBoundingClientRect().height))) assert.ok(box >= 44, 'rows are thumb-sized');
  }
  await phone.setViewportSize({ width: 390, height: 844 });
  await phone.screenshot({ path: '/tmp/lazybox-phone-inbox.png' });

  // Approve from the phone.
  await approvalRow.locator('.remote-row-open').click();
  await expect(phone.getByRole('heading', { name: 'Bump dependencies' })).toBeVisible();
  assert.equal(new URL(phone.url()).searchParams.get('session'), 'approval-session');
  const approval = phone.locator('.request').filter({ hasText: 'Approval required' });
  await expect(approval).toContainText('npm update');
  await expect(phone.locator('.message').first()).toContainText('Retained conversation for approval-session');
  const composer = phone.getByRole('textbox', { name: 'Message' });
  const composerBox = await composer.boundingBox();
  assert.ok(composerBox.y + composerBox.height <= 844, 'the composer stays on screen');
  await phone.screenshot({ path: '/tmp/lazybox-phone-approval.png' });
  await approval.getByRole('button', { name: 'Allow once' }).click();
  await expect.poll(() => f.mutations.filter(m => m.method === 'Apply' && m.body.approval).map(m => [m.body.sessionId, m.body.approval.requestId, m.body.approval.decision, m.device])).toEqual([['approval-session', 'a-deps', 'DECISION_ACCEPT', [...f.remote.devices.keys()][0]]]);
  // The phone's back gesture returns to the inbox.
  await phone.goBack();
  await expect(phone.getByRole('heading', { name: 'Inbox' })).toBeVisible();

  // Answer a question.
  await phone.locator('.remote-row').filter({ hasText: 'Ship release notes' }).locator('.remote-row-open').click();
  const question = phone.locator('.request').filter({ hasText: 'Agent question' });
  await question.getByRole('textbox').fill('2.4.0');
  await question.getByRole('button', { name: 'Send answer' }).click();
  await expect.poll(() => f.mutations.filter(m => m.method === 'Apply' && m.body.answer).map(m => m.body.answer.answers)).toEqual([{ 'Which version should the notes cover?': { values: ['2.4.0'] } }]);
  await phone.getByRole('button', { name: 'Back to inbox' }).click();
  await expect(phone.getByRole('heading', { name: 'Inbox' })).toBeVisible();

  // Reply to a finished turn; opening it marks it seen.
  await phone.locator('.remote-row').filter({ hasText: 'Refactor the parser' }).locator('.remote-row-open').click();
  await composer.fill('Ship it, then open a PR.');
  await phone.getByRole('button', { name: 'Send message' }).click();
  await expect(composer).toHaveValue('');
  await expect.poll(() => f.mutations.filter(m => m.method === 'Apply' && m.body.message).map(m => [m.body.sessionId, m.body.message.text])).toEqual([['review-session', 'Ship it, then open a PR.']]);
  await phone.screenshot({ path: '/tmp/lazybox-phone-conversation.png' });
  await phone.getByRole('button', { name: 'Back to inbox' }).click();
  await expect(phone.locator('.group-review')).toHaveCount(0);

  // A working agent can be interrupted from the phone.
  await phone.locator('.remote-row').filter({ hasText: 'Index the API' }).locator('.remote-row-open').click();
  await phone.getByRole('button', { name: 'Interrupt' }).click();
  await expect.poll(() => f.mutations.filter(m => m.method === 'Apply' && m.body.interrupt).length).toBe(1);
  await phone.getByRole('button', { name: 'Back to inbox' }).click();

  // A read-only phone sees the same inbox and request, and can send nothing.
  const reader = await pairPhone([], 'Old phone');
  await reader.locator('.remote-row').filter({ hasText: 'Ship release notes' }).locator('.remote-row-open').click();
  await expect(reader.locator('.remote-composer')).toHaveText('This phone can read conversations only. Pair it again to reply.');
  await expect(reader.locator('.request')).toHaveCount(0);
  await expect(reader.locator('.remote-transcript')).toContainText('This phone can only read; answer on your computer.');
  await expect(reader.getByRole('textbox', { name: 'Message' })).toHaveCount(0);

  // Every phone call carried a device credential and stayed on the device allowlist.
  assert.deepEqual(f.remote.deviceDenied, []);
  const phoneCalls = f.calls.filter(c => c.device);
  assert.ok(phoneCalls.length > 0);
  assert.deepEqual([...new Set(phoneCalls.map(c => c.method))].sort().filter(m => !['Apply', 'GetPushSettings', 'GetSession', 'Info', 'ListCollections', 'ListCommands', 'ListLinks', 'ReadSessionTail', 'WatchEvents'].includes(m)), [], 'the phone used an RPC it does not need');
  assert.deepEqual(f.mutations.filter(m => m.method === 'Apply' && !m.device), [], 'every reply came from a paired phone');
  assert.deepEqual(errors, []);
  console.log('PASS remote inbox: grouped attention across workspaces, approve, answer, reply, interrupt, back gesture, read-only phone, 320/390 widths, device allowlist only.');
} finally { await browser?.close(); await server.close(); await f.close(); }
