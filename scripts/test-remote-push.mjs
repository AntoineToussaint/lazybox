// The phone that acts (REMOTE.md, audit brief R7p), against the fake Runner
// in a 390 px mobile viewport (Chromium: service workers and the push
// protocol's test hooks):
// - Inbox rows answer: Allow with Undo before it is sent; a flagged command
//   asks first; Deny with a reason; a question's options as chips (one tap,
//   or several then Send). The answer reaches the Runner from the device, and
//   the computer sees the request answered.
// - Swipes: right pins (Undo), a long left swipe marks read (Undo), the left
//   tray stops a turn after confirming; ⋯ opens every action.
// - Push: the permission flow with a mocked PushManager (the browser's push
//   service is not reachable from a test), the subscription handed to the
//   Runner, per-kind toggles and quiet hours, a test notification; a
//   needs-input push delivered to the service worker with the page closed
//   becomes a notification whose click opens its conversation, where it is
//   approved. An iPhone in a Safari tab is told to add Lazybox to the Home
//   Screen first.
// - A running command's output follows live; offline and pull to refresh.
// - Every phone call carried a device credential and stayed on the allowlist.
// Screenshots: .cache/remote-*.png at 390 px, dark and light.
import { createServer } from 'vite';
import react from '@vitejs/plugin-react';
import { chromium, devices, expect } from './fixtures/playwright.mjs';
import { fileURLToPath } from 'node:url';
import { mkdirSync } from 'node:fs';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';
import { testViteCache } from './fixtures/vite-cache.mjs';

const shots = fileURLToPath(new URL('../.cache/', import.meta.url));
mkdirSync(shots, { recursive: true });
const f = await startRunnerFixture();
const now = Date.now(), ms = value => String(value);
const data = value => Buffer.from(JSON.stringify(value)).toString('base64');
const root = (status, updated, extra = {}) => [{ id: 'root', name: 'Main agent', status, updatedAtUnixMillis: ms(updated), ...extra }];
const base = { profileId: 'claude-default', provider: 'claude', capabilities: { files: true }, pending: [] };
// The fixture's two sessions: one works (and runs a dev server), one has a finished turn.
Object.assign(f.sessions[0], { title: 'Index the API', model: 'claude-fable-5', busy: true, status: 'running', agents: root('running', now - 5_000) });
Object.assign(f.sessions[1], { title: 'Write the docs', model: 'claude-fable-5', status: 'done', agents: root('done', now - 60_000, { turnEndedAtUnixMillis: ms(now - 60_000), lastTurnStatus: 'completed' }), lastMessage: 'The guide is written.' });
f.sessions.push(
  { ...base, id: 'approval-session', conversationId: 'approval-conversation', collectionId: 'beta', title: 'Bump dependencies', model: 'claude-fable-5', status: 'waiting_approval', createdAtUnixMillis: ms(now - 5_000_000), agents: root('waiting_approval', now - 30_000),
    pending: [{ type: 'approval', requestId: 'a-deps', text: 'Bash', providerDataJson: data({ input: { command: 'npm update' } }) }] },
  { ...base, id: 'risky-session', collectionId: 'beta', title: 'Clean the build', model: 'claude-fable-5', status: 'waiting_approval', createdAtUnixMillis: ms(now - 5_100_000), agents: root('waiting_approval', now - 40_000),
    pending: [{ type: 'approval', requestId: 'a-clean', text: 'Bash', providerDataJson: data({ input: { command: 'rm -rf build dist' } }) }] },
  { ...base, id: 'choice-session', collectionId: 'alpha', title: 'Pick a database', model: 'claude-fable-5', status: 'waiting_input', createdAtUnixMillis: ms(now - 5_200_000), agents: root('waiting_input', now - 50_000),
    pending: [{ type: 'question', requestId: 'q-db', providerDataJson: data({ input: { questions: [{ question: 'Which database should the service use?', header: 'Database', options: [{ label: 'Postgres', description: 'Shared, durable' }, { label: 'SQLite', description: 'One file' }] }] } }) }] },
  { ...base, id: 'multi-session', collectionId: 'alpha', title: 'Set up CI', model: 'claude-fable-5', status: 'waiting_input', createdAtUnixMillis: ms(now - 5_300_000), agents: root('waiting_input', now - 60_000),
    pending: [{ type: 'question', requestId: 'q-ci', providerDataJson: data({ input: { questions: [{ question: 'Which checks should CI run?', multiSelect: true, options: [{ label: 'lint' }, { label: 'test' }, { label: 'e2e' }] }] } }) }] },
  { ...base, id: 'review-session', conversationId: 'review-conversation', collectionId: 'alpha', title: 'Refactor the parser', model: 'claude-fable-5', status: 'done', createdAtUnixMillis: ms(now - 3_600_000), agents: root('done', now - 10_000, { turnEndedAtUnixMillis: ms(now - 10_000), lastTurnStatus: 'completed' }), lastMessage: 'I split the parser into three files. **All 42 tests pass.**' },
  { ...base, id: 'failing-session', collectionId: 'beta', title: 'Deploy preview', model: 'claude-fable-5', status: 'failed', fault: 'provider delivery failed; reconcile before continuing', createdAtUnixMillis: ms(now - 7_200_000), agents: root('failed', now - 20_000) },
);
const dev = f.streamCommand(f.sessions[0].id, 'npm run dev', ['npm', 'run', 'dev']);
dev.write('> api@1.4.0 dev\n> vite --port 5173\n\n  VITE v6.2.0  ready in 412 ms\n\n  ➜  Local:   http://localhost:5173/\n');
f.remote.tailscale = { ...f.remote.tailscale, enabled: true, state: 'serving' };
f.remote.resolveOnAnswer = true;

const proxy = { '/lazybox.bench.v1.RunnerService/': { target: f.url }, '/remote/v1/': { target: f.url } };
const server = await createServer({ ...testViteCache(import.meta.url), configFile: false, root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: { host: '127.0.0.1', port: 0, proxy } });
let browser;
const errors = [];
const applies = (pick) => f.mutations.filter(m => m.method === 'Apply' && pick(m.body));
const owner = async (method, body = {}) => (await fetch(`${f.url}/lazybox.bench.v1.RunnerService/${method}`, { method: 'POST', headers: { authorization: `Bearer ${f.token}`, 'content-type': 'application/json' }, body: JSON.stringify(body) })).json();

// The browser's push service cannot be reached from a test: PushManager is
// mocked with a subscription of the real shape (a fresh P-256 key, a 16-byte
// secret, an FCM endpoint), and the permission prompt answers as the person
// would. Both survive reloads, as they do in a browser.
const mockPush = () => {
  const store = { get: key => localStorage.getItem(`__push:${key}`), set: (key, value) => localStorage.setItem(`__push:${key}`, value) };
  Object.defineProperty(Notification, 'permission', { configurable: true, get: () => store.get('permission') || 'default' });
  Notification.requestPermission = async () => { store.set('asked', String(Number(store.get('asked') || 0) + 1)); const answer = store.get('answer') || 'granted'; store.set('permission', answer); return answer; };
  const b64 = bytes => btoa(String.fromCharCode(...new Uint8Array(bytes))).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
  const unb64 = text => Uint8Array.from(atob(text.replace(/-/g, '+').replace(/_/g, '/')), c => c.charCodeAt(0)).buffer;
  const make = saved => saved && { endpoint: saved.endpoint, options: { userVisibleOnly: true, applicationServerKey: unb64(saved.key) }, toJSON: () => ({ endpoint: saved.endpoint, expirationTime: null, keys: { p256dh: saved.p256dh, auth: saved.auth } }), unsubscribe: async () => { localStorage.removeItem('__push:subscription'); return true; } };
  PushManager.prototype.getSubscription = async function () { return make(JSON.parse(store.get('subscription') || 'null')); };
  PushManager.prototype.subscribe = async function (options) {
    if (Notification.permission !== 'granted') throw new DOMException('Permission denied', 'NotAllowedError');
    const pair = await crypto.subtle.generateKey({ name: 'ECDH', namedCurve: 'P-256' }, true, ['deriveBits']);
    const saved = { endpoint: `https://fcm.googleapis.com/fcm/send/fixture-${b64(crypto.getRandomValues(new Uint8Array(9)))}`, key: b64(options.applicationServerKey), p256dh: b64(await crypto.subtle.exportKey('raw', pair.publicKey)), auth: b64(crypto.getRandomValues(new Uint8Array(16))) };
    store.set('subscription', JSON.stringify(saved)); store.set('subscribed', String(Number(store.get('subscribed') || 0) + 1));
    return make(saved);
  };
};

/** A real touch swipe (Chromium's touch input): pointer and touch events as a finger makes them. */
async function swipe(page, locator, dx, { steps = 12, hold = 0 } = {}) {
  const box = await locator.boundingBox();
  const cdp = await page.context().newCDPSession(page);
  const y = box.y + Math.min(30, box.height / 2), x0 = dx > 0 ? box.x + 30 : box.x + box.width - 30;
  await cdp.send('Input.dispatchTouchEvent', { type: 'touchStart', touchPoints: [{ x: x0, y }] });
  for (let i = 1; i <= steps; i++) { await cdp.send('Input.dispatchTouchEvent', { type: 'touchMove', touchPoints: [{ x: x0 + dx * i / steps, y }] }); await page.waitForTimeout(12); }
  if (hold) await page.waitForTimeout(hold);
  await cdp.send('Input.dispatchTouchEvent', { type: 'touchEnd', touchPoints: [] });
  await cdp.detach();
}
async function pull(page, dy) {
  const cdp = await page.context().newCDPSession(page);
  await cdp.send('Input.dispatchTouchEvent', { type: 'touchStart', touchPoints: [{ x: 195, y: 160 }] });
  for (let i = 1; i <= 14; i++) { await cdp.send('Input.dispatchTouchEvent', { type: 'touchMove', touchPoints: [{ x: 195, y: 160 + dy * i / 14 }] }); await page.waitForTimeout(12); }
  await cdp.send('Input.dispatchTouchEvent', { type: 'touchEnd', touchPoints: [] });
  await cdp.detach();
}

try {
  await server.listen();
  const origin = `http://127.0.0.1:${server.httpServer.address().port}`;
  f.remote.phoneUrl = origin;
  // The full Chromium in its new headless mode: the headless shell has no notifications.
  browser = await chromium.launch({ channel: 'chromium' });
  const phoneContext = async (colorScheme, userAgent = devices['Pixel 7'].userAgent) => {
    const context = await browser.newContext({ ...devices['Pixel 7'], userAgent, viewport: { width: 390, height: 844 }, colorScheme });
    await context.grantPermissions(['notifications'], { origin });
    await context.addInitScript(mockPush);
    return context;
  };
  const pairPhone = async (context, name, grants = ['respond']) => {
    const page = await context.newPage(); page.on('pageerror', e => errors.push(String(e)));
    await page.goto((await owner('BeginDevicePairing', { grants })).url);
    await page.getByRole('textbox', { name: 'Name this phone' }).fill(name);
    await page.getByRole('button', { name: 'Pair this phone' }).click();
    await page.getByRole('button', { name: 'Open inbox' }).click();
    await expect(page.getByRole('heading', { name: 'Inbox' })).toBeVisible();
    return page;
  };
  const context = await phoneContext('dark');
  let phone = await pairPhone(context, 'Pixel');
  const deviceId = [...f.remote.devices.keys()].at(-1);
  const row = title => phone.locator('.remote-row').filter({ has: phone.locator('.remote-row-title strong', { hasText: title }) });

  // ── Push: one tap from the Inbox asks the phone, subscribes with the Runner's key, tells the Runner.
  const nudge = phone.getByRole('complementary', { name: 'Notifications' });
  await expect(nudge).toContainText('Know when an agent needs you');
  await phone.screenshot({ path: `${shots}remote-inbox-dark.png`, animations: 'disabled' });
  await nudge.getByRole('button', { name: 'Turn on notifications' }).click();
  await expect(phone.getByText('Notifications are on. You will hear when an agent needs you.')).toBeVisible();
  assert.equal(await phone.evaluate(() => localStorage.getItem('__push:asked')), '1', 'asked once, inside the tap');
  const subscribed = f.mutations.filter(m => m.method === 'SetPushSettings' && m.body.subscription);
  assert.equal(subscribed.length, 1);
  assert.equal(subscribed[0].device, deviceId);
  assert.match(subscribed[0].body.subscription.endpoint, /^https:\/\/fcm\.googleapis\.com\/fcm\/send\//);
  assert.equal(Buffer.from(subscribed[0].body.subscription.p256dh, 'base64url').length, 65);
  assert.equal(Buffer.from(subscribed[0].body.subscription.auth, 'base64url').length, 16);
  assert.deepEqual({ review: false, quietStart: '', quietEnd: '', ...subscribed[0].body.preferences, timeZone: undefined }, { needsInput: true, failed: true, review: false, quietStart: '', quietEnd: '', timeZone: undefined }, 'the computer’s defaults');
  assert.equal(await phone.evaluate(() => JSON.parse(localStorage.getItem('__push:subscription')).key), f.remote.vapid, 'subscribed with the Runner’s VAPID key');
  assert.equal((await owner('ListDevices')).devices.find(d => d.id === deviceId).push.subscribed, true, 'the computer sees it on');

  // ── Rows answer. Allow waits for Undo, then goes from the device; the computer sees it answered.
  const deps = row('Bump dependencies');
  await expect(deps.locator('.remote-row-what')).toHaveText('npm update');
  await deps.getByRole('button', { name: 'Allow', exact: true }).click();
  const snackbar = phone.locator('.remote-snackbar');
  await expect(snackbar).toContainText('Allowed · Bump dependencies');
  await expect(deps.getByRole('status')).toContainText('Allowing…');
  await phone.screenshot({ path: `${shots}remote-row-undo-dark.png`, animations: 'disabled' });
  await snackbar.getByRole('button', { name: 'Undo' }).click();
  await expect(deps.getByRole('button', { name: 'Allow', exact: true })).toBeVisible();
  await phone.waitForTimeout(5000);
  assert.deepEqual(applies(b => b.approval?.requestId === 'a-deps'), [], 'Undo sent nothing');
  await deps.getByRole('button', { name: 'Allow', exact: true }).click();
  await expect.poll(() => applies(b => b.approval?.requestId === 'a-deps').map(m => [m.body.approval.decision, m.device]), { timeout: 10_000 }).toEqual([['DECISION_ACCEPT', deviceId]]);
  const desktop = await owner('Info');
  assert.deepEqual(desktop.sessions.find(s => s.id === 'approval-session').pending || [], [], 'the computer sees the request answered');
  await expect(phone.locator('.group-needs-input .remote-row').filter({ hasText: 'Bump dependencies' })).toHaveCount(0, { timeout: 10_000 });

  // A flagged command asks first; Cancel sends nothing. Deny takes a reason.
  const risky = row('Clean the build');
  await expect(risky.locator('.remote-risks')).toContainText('Deletes files recursively');
  await risky.getByRole('button', { name: 'Allow…' }).click();
  const confirm = phone.getByRole('dialog', { name: 'Allow this?' });
  await expect(confirm).toContainText('rm -rf build dist');
  await expect(confirm.getByRole('button', { name: 'Cancel' })).toBeFocused();
  await phone.screenshot({ path: `${shots}remote-confirm-dark.png`, animations: 'disabled' });
  await confirm.getByRole('button', { name: 'Cancel' }).click();
  await risky.getByRole('button', { name: 'Deny' }).click();
  const denySheet = phone.getByRole('dialog', { name: 'Deny this request?' });
  await denySheet.getByRole('textbox').fill('Only remove dist');
  await denySheet.getByRole('button', { name: 'Deny', exact: true }).click();
  await expect.poll(() => applies(b => b.approval?.requestId === 'a-clean').map(m => [m.body.approval.decision, m.body.approval.reason])).toEqual([['DECISION_DECLINE', 'Only remove dist']]);

  // A question's options are chips: one tap answers (with Undo); several, then Send.
  const db = row('Pick a database');
  await expect(db.getByRole('radiogroup', { name: 'Which database should the service use?' })).toBeVisible();
  await db.getByRole('radio', { name: 'SQLite' }).click();
  await expect(snackbar).toContainText('Answered “SQLite”');
  await expect.poll(() => applies(b => b.answer?.requestId === 'q-db').map(m => m.body.answer.answers), { timeout: 10_000 }).toEqual([{ 'Which database should the service use?': { values: ['SQLite'] } }]);
  const ci = row('Set up CI');
  await ci.getByRole('checkbox', { name: 'lint' }).click();
  await ci.getByRole('checkbox', { name: 'e2e' }).click();
  await ci.getByRole('button', { name: 'Send' }).click();
  await expect.poll(() => applies(b => b.answer?.requestId === 'q-ci').map(m => m.body.answer.answers), { timeout: 10_000 }).toEqual([{ 'Which checks should CI run?': { values: ['lint', 'e2e'] } }]);

  // ── Swipes. Right pins (and Undo unpins); the pin keeps it together below what needs you.
  const parser = row('Refactor the parser');
  await swipe(phone, parser, 220);
  await expect(phone.locator('.group-pinned .remote-row')).toContainText('Refactor the parser');
  await expect(snackbar).toContainText('Pinned · Refactor the parser');
  await snackbar.getByRole('button', { name: 'Undo' }).click();
  await expect(phone.locator('.group-pinned')).toHaveCount(0);
  await swipe(phone, row('Refactor the parser'), 220);
  await expect(phone.locator('.group-pinned .remote-row')).toHaveCount(1);
  // The pin is the Runner's: the computer has it too.
  await expect.poll(() => Number(f.sessions.find(s => s.title === 'Refactor the parser')?.pinnedAtUnixMillis || 0)).toBeGreaterThan(0);
  // A long left swipe marks read; Undo puts it back.
  // A turn that ends while the phone is open is unread.
  Object.assign(f.sessions[1], { agents: root('done', Date.now(), { turnEndedAtUnixMillis: ms(Date.now()), lastTurnStatus: 'completed' }) });
  await expect(row('Write the docs')).toHaveAttribute('data-unread', 'true', { timeout: 10_000 });
  await swipe(phone, row('Write the docs'), -330);
  await expect(phone.locator('.group-idle .remote-row').filter({ hasText: 'Write the docs' })).toHaveCount(1);
  await expect(snackbar).toContainText('Marked read · Write the docs');
  await snackbar.getByRole('button', { name: 'Undo' }).click();
  await expect(phone.locator('.group-review .remote-row').filter({ hasText: 'Write the docs' })).toHaveCount(1);
  // A short left swipe opens the tray; Stop asks, then interrupts.
  const working = row('Index the API');
  await swipe(phone, working, -120);
  await expect(working).toHaveAttribute('data-open', 'true');
  await phone.screenshot({ path: `${shots}remote-swipe-tray-dark.png`, animations: 'disabled' });
  await working.locator('.remote-swipe-tray').getByRole('button', { name: 'Stop' }).click();
  const stopSheet = phone.getByRole('dialog', { name: 'Stop this turn?' });
  await stopSheet.getByRole('button', { name: 'Stop', exact: true }).click();
  await expect.poll(() => applies(b => b.interrupt).map(m => [m.body.sessionId, m.device])).toEqual([[f.sessions[0].id, deviceId]]);
  // ⋯ opens every action, keyboard and screen reader included.
  await row('Deploy preview').getByRole('button', { name: 'Actions for Deploy preview' }).click();
  const sheet = phone.getByRole('dialog', { name: 'Actions' });
  await expect(sheet.getByRole('button')).toHaveText(['Open conversation', 'Pin', 'Cancel']);
  // It opens on Cancel, not on its first action, and Esc gives focus back to the ⋯ that opened it.
  await expect(sheet.getByRole('button', { name: 'Cancel' })).toBeFocused();
  await phone.screenshot({ path: `${shots}remote-action-sheet-dark.png`, animations: 'disabled' });
  await phone.keyboard.press('Escape');
  await expect(sheet).toHaveCount(0);
  await expect(row('Deploy preview').getByRole('button', { name: 'Actions for Deploy preview' })).toBeFocused();
  for (const box of await phone.locator('.remote-row button:visible').evaluateAll(nodes => nodes.map(node => node.getBoundingClientRect().height))) assert.ok(box >= 40, `row targets are thumb-sized (${box})`);
  assert.equal(await phone.evaluate(() => document.documentElement.scrollWidth > innerWidth), false, 'no sideways scrolling at 390');

  // ── Pull to refresh asks the computer now; offline says so.
  await phone.evaluate(() => scrollTo(0, 0));
  const infos = f.calls.filter(c => c.method === 'Info' && c.device).length;
  await pull(phone, 260);
  await expect.poll(() => f.calls.filter(c => c.method === 'Info' && c.device).length).toBeGreaterThan(infos);
  await context.setOffline(true);
  await phone.evaluate(() => dispatchEvent(new Event('offline')));
  await expect(phone.getByRole('status').filter({ hasText: 'This phone is offline' })).toBeVisible();
  await context.setOffline(false);
  await phone.evaluate(() => dispatchEvent(new Event('online')));
  await expect(phone.getByRole('status').filter({ hasText: 'This phone is offline' })).toHaveCount(0);

  // ── A running command's output, live, on the phone.
  await row('Index the API').locator('.remote-row-open').click();
  const strip = phone.getByRole('navigation', { name: 'Commands' });
  await strip.getByRole('button', { name: /npm run dev/ }).click();
  await expect(phone.getByRole('heading', { name: 'npm run dev' })).toBeVisible();
  await expect(phone.locator('.remote-console')).toContainText('ready in 412 ms');
  dev.write('  ➜  page reload src/App.tsx\n');
  await expect(phone.locator('.remote-console')).toContainText('page reload src/App.tsx');
  await expect(phone.locator('.console-stop')).toHaveCount(0);
  await phone.screenshot({ path: `${shots}remote-command-dark.png`, animations: 'disabled' });
  await phone.getByRole('button', { name: 'Back to conversation' }).click();
  await phone.getByRole('button', { name: 'Back to inbox' }).click();

  // ── Settings: the computer's kinds, quiet hours, a test notification.
  await phone.getByRole('button', { name: 'Settings' }).click();
  await expect(phone.getByRole('heading', { name: 'Settings' })).toBeVisible();
  await expect(phone.getByText('On for this phone')).toBeVisible();
  await phone.getByRole('switch', { name: /Ready for review/ }).click();
  await expect.poll(() => f.mutations.filter(m => m.method === 'SetPushSettings').at(-1)?.body.preferences?.review).toBe(true);
  await phone.getByRole('switch', { name: /Pause at night/ }).click();
  await expect.poll(() => f.mutations.filter(m => m.method === 'SetPushSettings').at(-1)?.body.preferences).toMatchObject({ quietStart: '22:00', quietEnd: '07:00' });
  assert.ok(f.mutations.filter(m => m.method === 'SetPushSettings').at(-1).body.preferences.timeZone, 'quiet hours carry the phone’s time zone');
  await phone.getByRole('button', { name: 'Send a test' }).click();
  await expect(phone.getByText('Sent. It should arrive within a few seconds.')).toBeVisible();
  assert.deepEqual(f.remote.pushes.at(-1), { device: deviceId, endpoint: subscribed[0].body.subscription.endpoint, payload: { title: 'Notifications work', kind: 'test', url: '/?view=remote', tag: 'test' } });
  await phone.screenshot({ path: `${shots}remote-settings-dark.png`, fullPage: true, animations: 'disabled' });
  f.remote.pushFailure = 'This phone’s notification subscription ended; turn notifications on again on the phone.';
  await phone.getByRole('button', { name: 'Send a test' }).click();
  await expect(phone.getByRole('alert')).toContainText('subscription ended');
  f.remote.pushFailure = '';
  await phone.getByRole('button', { name: 'Back to inbox' }).click();

  // ── A needs-input push, with the page closed: the service worker shows it, and its tap opens the conversation.
  const registrations = new Map();
  const blank = await context.newPage();
  const cdp = await context.newCDPSession(blank);
  cdp.on('ServiceWorker.workerRegistrationUpdated', event => { for (const r of event.registrations) registrations.set(r.scopeURL, r.registrationId); });
  await cdp.send('ServiceWorker.enable');
  await expect.poll(() => registrations.get(`${origin}/`)).toBeTruthy();
  Object.assign(f.sessions.find(s => s.id === 'approval-session'), { status: 'waiting_approval', busy: false, pending: [{ type: 'approval', requestId: 'a-lock', text: 'Bash', providerDataJson: data({ input: { command: 'npm install --package-lock-only' } }) }] });
  const [sent] = f.remote.notify('approval', f.sessions.find(s => s.id === 'approval-session'));
  assert.deepEqual(sent, { title: 'Bump dependencies', kind: 'approval', url: '/?view=remote&session=approval-session', tag: 'approval-conversation' }, 'title, kind and link only');
  await phone.close();
  await cdp.send('ServiceWorker.deliverPushMessage', { origin: `${origin}/`, registrationId: registrations.get(`${origin}/`), data: JSON.stringify(sent) });
  phone = await context.newPage(); phone.on('pageerror', e => errors.push(String(e)));
  // This page's device credential lasts seconds, so it renews while the steps below run.
  let signIns = 0;
  await phone.route('**/remote/v1/session', async route => { const response = await route.fetch(); const body = await response.json(); body.expiresAt = Date.now() + 90_000 + 5_500; signIns++; await route.fulfill({ response, json: body }); });
  const pushWritesBefore = f.mutations.filter(m => m.method === 'SetPushSettings').length;
  await phone.goto(`${origin}/?view=remote`);
  await expect(phone.getByRole('heading', { name: 'Inbox' })).toBeVisible();
  const shown = () => phone.evaluate(() => navigator.serviceWorker.getRegistration('/').then(r => r.getNotifications()).then(list => list.map(n => ({ title: n.title, body: n.body, tag: n.tag, url: n.data?.url }))));
  await expect.poll(shown).toEqual([{ title: 'Approval needed', body: 'Bump dependencies', tag: 'approval-conversation', url: '/?view=remote&session=approval-session' }]);
  const [worker] = context.serviceWorkers();
  await worker.evaluate(() => self.registration.getNotifications().then(([n]) => self.dispatchEvent(new NotificationEvent('notificationclick', { notification: n }))));
  await expect(phone.getByRole('heading', { name: 'Bump dependencies' })).toBeVisible();
  assert.equal(new URL(phone.url()).searchParams.get('session'), 'approval-session');
  // Approved on the page the notification opened, with the computer's card; the banner leaves.
  await phone.locator('.request').getByRole('button', { name: 'Allow once' }).click();
  // It uses the network, so the phone asks first, as a row does.
  await expect(phone.getByRole('dialog', { name: 'Allow this?' })).toContainText('Uses the network');
  await phone.screenshot({ path: `${shots}remote-conversation-confirm-dark.png`, animations: 'disabled' });
  await phone.getByRole('dialog', { name: 'Allow this?' }).getByRole('button', { name: 'Allow', exact: true }).click();
  await expect.poll(() => applies(b => b.approval?.requestId === 'a-lock').map(m => m.device)).toEqual([deviceId]);
  await expect.poll(shown).toEqual([]);
  await phone.screenshot({ path: `${shots}remote-conversation-dark.png`, animations: 'disabled' });
  // A flagged request on that page asks first too.
  Object.assign(f.sessions.find(s => s.id === 'approval-session'), { status: 'waiting_approval', pending: [{ type: 'approval', requestId: 'a-force', text: 'Bash', providerDataJson: data({ input: { command: 'git push --force origin main' } }) }] });
  await phone.locator('.request').getByRole('button', { name: 'Allow once' }).click({ timeout: 10_000 });
  await expect(phone.getByRole('dialog', { name: 'Allow this?' })).toContainText('Can discard work');
  // The sheet opens on Cancel (a stray Enter allows nothing), and closing it gives focus back to Allow once.
  await expect(phone.getByRole('dialog', { name: 'Allow this?' }).getByRole('button', { name: 'Cancel' })).toBeFocused();
  await phone.getByRole('dialog', { name: 'Allow this?' }).getByRole('button', { name: 'Cancel' }).click();
  await expect(phone.locator('.request').getByRole('button', { name: 'Allow once' })).toBeFocused();
  await phone.waitForTimeout(500);
  assert.deepEqual(applies(b => b.approval?.requestId === 'a-force'), [], 'Cancel sent nothing');
  await phone.locator('.request').getByRole('button', { name: 'Allow once' }).click();
  await phone.getByRole('dialog', { name: 'Allow this?' }).getByRole('button', { name: 'Allow', exact: true }).click();
  await expect.poll(() => applies(b => b.approval?.requestId === 'a-force').length).toBe(1);
  await phone.getByRole('button', { name: 'Back to inbox' }).click();
  // The page re-synced its push subscription once, however often its credential renewed, and sent
  // the subscription only: the preferences saved in Settings are not written back over.
  await expect.poll(() => signIns, { timeout: 20_000 }).toBeGreaterThanOrEqual(3);
  const resyncs = f.mutations.filter(m => m.method === 'SetPushSettings').slice(pushWritesBefore);
  assert.equal(resyncs.length, 1, `one re-sync for the page, not one per credential (${resyncs.length})`);
  assert.ok(resyncs[0].body.subscription?.endpoint, 'the re-sync hands over the subscription');
  assert.equal(resyncs[0].body.preferences, undefined, 'the re-sync leaves the preferences as they are');
  await phone.unroute('**/remote/v1/session');

  // ── Light, at 390, with requests waiting again.
  Object.assign(f.sessions.find(s => s.id === 'risky-session'), { status: 'waiting_approval', busy: false, pending: [{ type: 'approval', requestId: 'a-clean-2', text: 'Bash', providerDataJson: data({ input: { command: 'rm -rf dist' } }) }] });
  Object.assign(f.sessions.find(s => s.id === 'choice-session'), { status: 'waiting_input', busy: false, pending: [{ type: 'question', requestId: 'q-db-2', providerDataJson: data({ input: { questions: [{ question: 'Run the migrations now?', options: [{ label: 'Yes' }, { label: 'After review' }] }] } }) }] });
  const light = await phoneContext('light');
  const lightPhone = await pairPhone(light, 'Pixel light');
  await lightPhone.screenshot({ path: `${shots}remote-inbox-light.png`, animations: 'disabled' });
  await lightPhone.getByRole('button', { name: 'Settings' }).click();
  await lightPhone.screenshot({ path: `${shots}remote-settings-light.png`, fullPage: true, animations: 'disabled' });
  await lightPhone.getByRole('button', { name: 'Back to inbox' }).click();
  await lightPhone.locator('.remote-row').filter({ hasText: 'Deploy preview' }).getByRole('button', { name: 'Actions for Deploy preview' }).click();
  await lightPhone.screenshot({ path: `${shots}remote-action-sheet-light.png`, animations: 'disabled' });
  await light.close();

  // ── An iPhone in a Safari tab: notifications need the Home Screen first, and the phone says how.
  const iphone = await phoneContext('dark', 'Mozilla/5.0 (iPhone; CPU iPhone OS 18_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.1 Mobile/15E148 Safari/604.1');
  const ios = await pairPhone(iphone, 'iPhone');
  await expect(ios.getByRole('complementary', { name: 'Notifications' })).toContainText('Add Lazybox to your Home Screen');
  await ios.getByRole('button', { name: 'Show me how' }).click();
  await expect(ios.locator('.remote-install')).toContainText('Add to Home Screen');
  await expect(ios.getByRole('button', { name: 'Turn on notifications' })).toHaveCount(0);
  await ios.screenshot({ path: `${shots}remote-ios-install-dark.png`, fullPage: true, animations: 'disabled' });
  await iphone.close();

  // ── Revoking the phone on the computer ends its notifications with it.
  await owner('RevokeDevice', { deviceId });
  assert.deepEqual(f.remote.notify('approval', f.sessions.find(s => s.id === 'approval-session')).length, 0);
  await phone.reload();
  await expect(phone.getByRole('heading', { name: 'This phone was signed out' })).toBeVisible();

  // Every phone call carried a device credential and stayed on the device allowlist.
  assert.deepEqual(f.remote.deviceDenied, []);
  const used = [...new Set(f.calls.filter(c => c.device).map(c => c.method))].sort();
  assert.deepEqual(used.filter(m => !['Apply', 'GetPushSettings', 'GetSession', 'Info', 'ListCollections', 'ListCommands', 'ListLinks', 'PinConversation', 'ReadSessionTail', 'SendTestPush', 'SetPushSettings', 'WatchEvents', 'WatchOutput'].includes(m)), [], 'the phone used an RPC it does not need');
  assert.deepEqual(errors, []);
  console.log('PASS remote push: row approvals with Undo, flagged commands confirmed, deny with reason, question chips, swipe pin/read/stop, action sheet, pull to refresh, offline, live command output, push permission and subscription, per-kind toggles, quiet hours, test push, closed-page push → notification → conversation → approve, iPhone Home Screen guidance, revoke; screenshots in .cache/remote-*.png.');
} finally { await browser?.close(); await server.close(); await f.close(); }
