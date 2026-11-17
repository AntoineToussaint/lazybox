// Pairing a phone (REMOTE.md): Settings → Devices turns on the Tailscale
// route, shows a one-time QR code, and follows it until the phone pairs; the
// phone opens the code's link, pairs with a key it generates, and lands in
// its inbox. A used code pairs nothing; an expired one says so and offers a
// new one; closing the dialog withdraws its code; Revoke signs the phone out
// at its next request. The phone never holds or sends the owner's bearer and
// never calls an RPC off the device allowlist. Fake Runner; no agent runs.
import { createServer } from 'vite';
import react from '@vitejs/plugin-react';
import { webkit, chromium, devices, expect } from './fixtures/playwright.mjs';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';
import { testViteCache } from './fixtures/vite-cache.mjs';

const f = await startRunnerFixture();
const proxy = { '/lazybox.bench.v1.RunnerService/': { target: f.url }, '/remote/v1/': { target: f.url } };
const server = await createServer({ ...testViteCache(import.meta.url), configFile: false, root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: { host: '127.0.0.1', port: 0, proxy } });
let browser;
try {
  await server.listen();
  const origin = `http://127.0.0.1:${server.httpServer.address().port}`;
  // The phone reaches the Runner where the fake Tailscale route says it does: this test's server.
  f.remote.phoneUrl = origin;
  browser = await (process.env.TEST_CHROMIUM ? chromium : webkit).launch();
  const desktop = await browser.newPage({ viewport: { width: 1440, height: 900 } });
  const errors = []; desktop.on('pageerror', e => errors.push(String(e)));
  await desktop.goto(`${origin}/?view=workspace&collection=alpha&session=alpha-session#token=${f.token}`);
  await desktop.getByRole('button', { name: 'Settings', exact: true }).click();
  await desktop.getByRole('tab', { name: 'Devices', exact: true }).click();
  const panel = desktop.getByRole('tabpanel', { name: 'Devices' });
  const pairButton = panel.getByRole('button', { name: 'Pair a phone' });

  // Before the route is on, pairing is not offered and the card says what to do.
  const route = panel.locator('.remote-route');
  await expect(route).toContainText('Tailscale is connected as laptop.example.ts.net');
  await expect(route).toContainText('never on the public internet');
  await expect(pairButton).toBeDisabled();
  await expect(panel.locator('.devices-empty')).toContainText('No phones paired yet');
  await route.getByRole('button', { name: 'Turn on' }).click();
  await expect(route).toContainText(`On over Tailscale at ${origin}`);
  assert.deepEqual(f.mutations.filter(m => m.method === 'SetRemoteAccess').map(m => m.body), [{ tailscale: true }]);
  await expect(pairButton).toBeEnabled();
  await desktop.screenshot({ path: '/tmp/lazybox-devices-settings.png' });

  // The code appears at once, with the usual grants, and works once for five minutes.
  await pairButton.click();
  const dialog = desktop.getByRole('dialog', { name: 'Pair a phone' });
  const qr = dialog.getByRole('img', { name: /Pairing code/ });
  await expect(qr).toBeVisible();
  await expect(dialog).toContainText('See conversations · Reply, answer and approve');
  await expect(dialog.getByRole('timer')).toHaveText(/^Works once · expires in 4:5\d$/);
  await expect(dialog).toContainText(`It opens ${new URL(origin).host}`);
  const firstLink = await qr.getAttribute('data-value');
  const firstUrl = new URL(firstLink);
  assert.equal(firstUrl.origin, origin);
  assert.equal(firstUrl.search, '?view=remote');
  const firstOffer = new URLSearchParams(firstUrl.hash.slice(1)).get('pair').split('.')[0];
  assert.ok(!firstUrl.search.includes(f.remote.offers.get(firstOffer).secret), 'the secret rides only in the fragment');

  // Changing what the phone may do issues a new code and withdraws the old one.
  await dialog.getByRole('button', { name: 'Change' }).click();
  await expect(dialog.getByRole('checkbox', { name: /See conversations/ })).toBeDisabled();
  // Starting a conversation is not offered: the phone has no screen for it yet.
  await expect(dialog.getByRole('checkbox', { name: /Start conversations/ })).toHaveCount(0);
  await dialog.getByRole('checkbox', { name: /Reply, answer and approve/ }).uncheck();
  await expect.poll(async () => await qr.getAttribute('data-value')).not.toBe(firstLink);
  await expect.poll(() => f.remote.offers.get(firstOffer).state).toBe('cancelled');
  assert.match(await qr.getAttribute('data-value'), /grants=read$/);
  await dialog.getByRole('checkbox', { name: /Reply, answer and approve/ }).check();
  await expect.poll(async () => await qr.getAttribute('data-value')).toMatch(/grants=read%2Crespond$/);
  await desktop.screenshot({ path: '/tmp/lazybox-pair-dialog.png' });
  const link = await qr.getAttribute('data-value');

  // The phone opens the code's link.
  const iPhone = devices['iPhone 13'];
  const phoneContext = await browser.newContext({ ...iPhone, ...(process.env.TEST_CHROMIUM ? { isMobile: false } : {}) });
  const phone = await phoneContext.newPage();
  const phoneErrors = []; phone.on('pageerror', e => phoneErrors.push(String(e)));
  const phoneBearers = new Set();
  phone.on('request', request => { const value = request.headers().authorization; if (value) phoneBearers.add(value); });
  await phone.goto(link);
  await expect(phone.getByRole('heading', { name: 'Pair this phone' })).toBeVisible();
  assert.equal(await phone.evaluate(() => location.hash), '', 'the secret leaves the address bar and history at once');
  await expect(phone.getByLabel('This phone will be able to')).toHaveText(/See conversations.*Reply, answer and approve/);
  await expect(phone.getByRole('textbox', { name: 'Name this phone' })).toHaveValue('iPhone');
  assert.equal(await phone.evaluate(() => document.documentElement.scrollWidth > innerWidth), false);
  await phone.screenshot({ path: '/tmp/lazybox-phone-pair.png' });
  await phone.getByRole('textbox', { name: 'Name this phone' }).fill('Antoine’s iPhone');
  await phone.getByRole('button', { name: 'Pair this phone' }).click();
  await expect(phone.getByRole('heading', { name: 'Paired' })).toBeVisible();
  const [request] = f.remote.pairRequests;
  assert.ok(request.publicKey && request.signature && !('privateKey' in request), 'only the public key leaves the phone');

  // The computer follows the code to its end and shows the same key fingerprint.
  const paired = desktop.getByRole('dialog', { name: 'Phone paired' });
  await expect(paired).toContainText('Antoine’s iPhone is paired', { timeout: 5000 });
  const fingerprint = (await paired.locator('.mono').innerText()).trim();
  assert.match(fingerprint, /^[0-9A-F]{4} [0-9A-F]{4} [0-9A-F]{4} [0-9A-F]{4}$/);
  await expect(phone.locator('.remote-center .mono')).toHaveText(fingerprint);
  await desktop.screenshot({ path: '/tmp/lazybox-pair-done.png' });
  await phone.getByRole('button', { name: 'Open inbox' }).click();
  await expect(phone.getByRole('heading', { name: 'Inbox' })).toBeVisible();

  // The same code a second time pairs nothing.
  const otherContext = await browser.newContext({ ...iPhone, ...(process.env.TEST_CHROMIUM ? { isMobile: false } : {}) });
  const other = await otherContext.newPage();
  await other.goto(link);
  await other.getByRole('button', { name: 'Pair this phone' }).click();
  await expect(other.getByRole('alert')).toHaveText('this pairing code was already used; show a new one on your computer');
  assert.equal(f.remote.devices.size, 1);
  await otherContext.close();

  // The list shows the phone, what it may do, and that it is active.
  await paired.getByRole('button', { name: 'Done' }).click();
  const row = panel.getByRole('listitem', { name: 'Antoine’s iPhone' });
  await expect(row).toContainText('See conversations');
  await expect(row).toContainText('Reply, answer and approve');
  await expect(row).toContainText('Active now');
  await expect(row).toContainText(fingerprint);

  // An expired code says so, and a new one is one click away; closing withdraws it.
  await pairButton.click();
  const again = desktop.getByRole('dialog', { name: 'Pair a phone' });
  await expect(again.getByRole('img', { name: /Pairing code/ })).toBeVisible();
  const expiring = new URLSearchParams(new URL(await again.getByRole('img', { name: /Pairing code/ }).getAttribute('data-value')).hash.slice(1)).get('pair').split('.')[0];
  f.remote.expire(expiring);
  await expect(again).toContainText('This code expired', { timeout: 5000 });
  await again.getByRole('button', { name: 'Show a new code' }).click();
  await expect(again.getByRole('img', { name: /Pairing code/ })).toBeVisible();
  const replacement = new URLSearchParams(new URL(await again.getByRole('img', { name: /Pairing code/ }).getAttribute('data-value')).hash.slice(1)).get('pair').split('.')[0];
  assert.notEqual(replacement, expiring);
  await again.getByRole('button', { name: 'Close pairing' }).click();
  await expect.poll(() => f.remote.offers.get(replacement).state).toBe('cancelled');

  // Revoke asks once, then the phone is signed out at its next request.
  await row.getByRole('button', { name: 'Revoke Antoine’s iPhone' }).click();
  await row.getByRole('group', { name: 'Revoke Antoine’s iPhone?' }).getByRole('button', { name: 'Revoke' }).click();
  await expect(panel.locator('.devices-empty')).toBeVisible();
  await expect(phone.getByRole('heading', { name: 'This phone was signed out' })).toBeVisible({ timeout: 10_000 });
  await phone.screenshot({ path: '/tmp/lazybox-phone-revoked.png' });
  // Reopening the app does not bring the pairing back.
  await phone.goto(`${origin}/?view=remote`);
  await expect(phone.getByRole('heading', { name: 'Pair this phone with Lazybox' })).toBeVisible();

  // The phone held only device credentials, and asked for nothing a device may not.
  assert.ok([...phoneBearers].length > 0);
  for (const bearer of phoneBearers) assert.match(bearer, /^Bearer lbx-device\./, 'the phone sent a bearer that is not its own');
  assert.ok(![...phoneBearers].some(value => value.includes(f.token)), 'the owner bearer reached the phone');
  assert.deepEqual(f.remote.deviceDenied, [], 'the phone called an RPC off the device allowlist');
  assert.ok(f.remote.deviceCalls.every(call => call.device === null || f.calls.some(c => c.device === call.device)), 'device calls are attributed');
  assert.deepEqual(errors, []);
  assert.deepEqual(phoneErrors, []);
  console.log('PASS remote pairing: route on, one-time QR with grants, new code on change, phone pairs with its own key, matching fingerprint, used code refused, expiry and withdrawal, revoke signs the phone out, device credentials only.');
} finally { await browser?.close(); await server.close(); await f.close(); }
