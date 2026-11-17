// A phone's pairing and sign-in against a fake device gateway that checks
// what the real one checks: the one-time secret, the key's proof of
// possession, and a single-use challenge signed with the same key.
import test from 'node:test';
import assert from 'node:assert/strict';
import { createPublicKey, verify, randomBytes } from 'node:crypto';
import { DeviceRequestError, DeviceRevoked, memoryDeviceStore, openDeviceSession, pairDevice, parsePairingFragment } from '../packages/client-core/src/device.ts';

function fakeGateway() {
  const gateway = { secret: 'one-time-secret', used: false, devices: new Map<string, string>(), challenges: new Map<string, string>(), requests: [] as { path: string; body: any }[] };
  const ok = (key: string, message: string, signature: string) => verify('sha256', Buffer.from(message), { key: createPublicKey({ key: Buffer.from(key, 'base64url'), format: 'der', type: 'spki' }), dsaEncoding: 'ieee-p1363' }, Buffer.from(signature, 'base64url'));
  const json = (status: number, value: unknown) => new Response(JSON.stringify(value), { status, headers: { 'Content-Type': 'application/json' } });
  const fetch: typeof globalThis.fetch = async (input, init) => {
    const path = new URL(String(input)).pathname, body = JSON.parse(String(init?.body));
    gateway.requests.push({ path, body });
    assert.equal(new Headers(init?.headers).get('content-type'), 'application/json');
    assert.equal(init?.credentials, 'omit', 'no cookie ever rides along');
    if (path === '/remote/v1/pair') {
      if (body.pairing !== 'offer' || body.secret !== gateway.secret) return json(403, { error: 'this pairing code is not valid' });
      if (gateway.used) return json(403, { error: 'this pairing code was already used' });
      if (!ok(body.publicKey, `lazybox-device-pair-v1\0offer\0${body.publicKey}`, body.signature)) return json(403, { error: 'the device key did not sign this request' });
      gateway.used = true; gateway.devices.set('device-1', body.publicKey);
      return json(200, { device: { id: 'device-1', name: body.name, grants: ['read', 'respond'], fingerprint: 'ABCD 0123 4567 89EF' } });
    }
    if (path === '/remote/v1/challenge') {
      if (!gateway.devices.has(body.device)) return json(401, { error: 'not paired', code: 'revoked' });
      const challenge = randomBytes(32).toString('base64url'); gateway.challenges.set(challenge, body.device);
      return json(200, { challenge, expiresAt: Date.now() + 60_000 });
    }
    if (path === '/remote/v1/session') {
      const owner = gateway.challenges.get(body.challenge); gateway.challenges.delete(body.challenge);
      const key = gateway.devices.get(body.device);
      if (owner !== body.device || !key || !ok(key, `lazybox-device-session-v1\0${body.device}\0${body.challenge}`, body.signature)) return json(401, { error: 'bad signature' });
      return json(200, { token: `lbx-device.${body.device}.1.mac`, expiresAt: Date.now() + 900_000, device: { id: body.device, name: 'Phone', grants: ['read', 'respond'], fingerprint: 'ABCD 0123 4567 89EF' } });
    }
    return json(404, { error: 'unknown' });
  };
  return { gateway, fetch };
}

test('a QR fragment is read without trusting its shape', () => {
  assert.deepEqual(parsePairingFragment('#pair=offer.secret-value&grants=read%2Crespond'), { pairing: 'offer', secret: 'secret-value', grants: ['read', 'respond'] });
  assert.deepEqual(parsePairingFragment('#pair=offer.secret&grants=read,settings,start')?.grants, ['read', 'start']);
  for (const bad of ['', '#', '#pair=', '#pair=offer', '#pair=.secret', '#pair=offer.', '#pair=off/er.secret', '#pair=offer.sec<script>']) assert.equal(parsePairingFragment(bad), undefined, bad);
});

test('a phone pairs with a key it cannot export, then signs in with it', async () => {
  const { gateway, fetch } = fakeGateway();
  const store = memoryDeviceStore();
  const stored = await pairDevice({ baseUrl: 'https://laptop.example.ts.net:8443/', offer: { pairing: 'offer', secret: 'one-time-secret', grants: ['read'] }, name: 'iPhone', store, fetch });
  assert.equal(stored.device.id, 'device-1');
  assert.equal((await store.load())?.device.id, 'device-1');
  assert.equal(stored.keys.privateKey.extractable, false, 'the private key cannot be read out of the phone');
  await assert.rejects(crypto.subtle.exportKey('pkcs8', stored.keys.privateKey));
  assert.ok(!JSON.stringify(gateway.requests).includes('PRIVATE'), 'only the public key is sent');

  const session = await openDeviceSession({ baseUrl: 'https://laptop.example.ts.net:8443', stored, fetch });
  assert.match(session.token, /^lbx-device\./);
  // Each sign-in uses its own challenge.
  await openDeviceSession({ baseUrl: 'https://laptop.example.ts.net:8443', stored, fetch });
  const challenges = gateway.requests.filter(r => r.path === '/remote/v1/session').map(r => r.body.challenge);
  assert.equal(new Set(challenges).size, 2);

  // The same QR code cannot pair a second phone.
  await assert.rejects(pairDevice({ baseUrl: 'https://laptop.example.ts.net:8443', offer: { pairing: 'offer', secret: 'one-time-secret', grants: ['read'] }, name: 'Other', store: memoryDeviceStore(), fetch }), (error: unknown) => error instanceof DeviceRequestError && /already used/.test(error.message));

  // Revoked on the computer: the phone is told so it can say so.
  gateway.devices.clear();
  await assert.rejects(openDeviceSession({ baseUrl: 'https://laptop.example.ts.net:8443', stored, fetch }), DeviceRevoked);
});

test('a wrong secret pairs nothing and stores nothing', async () => {
  const { fetch } = fakeGateway();
  const store = memoryDeviceStore();
  await assert.rejects(pairDevice({ baseUrl: 'https://laptop.example.ts.net', offer: { pairing: 'offer', secret: 'guess', grants: [] }, name: 'x', store, fetch }), DeviceRequestError);
  assert.equal(await store.load(), undefined);
});

test('an unreachable computer is explained, not a raw fetch error', async () => {
  const stored = await pairDevice({ baseUrl: 'https://x', offer: { pairing: 'offer', secret: 'one-time-secret', grants: [] }, name: 'x', store: memoryDeviceStore(), fetch: fakeGateway().fetch });
  await assert.rejects(openDeviceSession({ baseUrl: 'https://x', stored, fetch: async () => { throw new TypeError('Load failed'); } }), (error: unknown) => error instanceof DeviceRequestError && /could not be reached/.test(error.message));
});
