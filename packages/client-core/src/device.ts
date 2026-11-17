/**
 * A phone's side of remote control (REMOTE.md). Pairing generates an ECDSA
 * P-256 key on the phone whose private half is not extractable: script on the
 * page can use it to sign, but cannot read it out, so a copy of the phone's
 * storage holds no reusable secret. The Runner only ever learns the public
 * key. Every access credential is obtained by signing a fresh single-use
 * challenge with that key and lasts fifteen minutes.
 */

export type DeviceRecord = { id: string; name: string; grants: string[]; fingerprint: string };
/** What the phone keeps: its record and its key pair (the private key non-extractable). */
export type StoredDevice = { device: DeviceRecord; keys: CryptoKeyPair; publicKey: string };
export type DeviceSession = { token: string; expiresAt: number; device: DeviceRecord };

export interface DeviceStore {
  load(): Promise<StoredDevice | undefined>;
  save(value: StoredDevice): Promise<void>;
  clear(): Promise<void>;
}

/** The Runner no longer knows this device: it was revoked, or the Runner's state was reset. */
export class DeviceRevoked extends Error {
  constructor(message = 'This phone was signed out of Lazybox.') { super(message); this.name = 'DeviceRevoked'; }
}
/** The Runner refused a request for a reason worth showing the person. */
export class DeviceRequestError extends Error {
  constructor(message: string, readonly status: number) { super(message); this.name = 'DeviceRequestError'; }
}

export type PairingOffer = { pairing: string; secret: string; grants: string[] };

/** Read a pairing offer from a QR link's fragment (`#pair=<id>.<secret>&grants=read,respond`). */
export function parsePairingFragment(hash: string): PairingOffer | undefined {
  const params = new URLSearchParams(hash.replace(/^#/, ''));
  const value = params.get('pair') || '';
  const dot = value.indexOf('.');
  if (dot <= 0 || dot === value.length - 1) return undefined;
  const pairing = value.slice(0, dot), secret = value.slice(dot + 1);
  if (!/^[\w-]+$/.test(pairing) || !/^[\w-]+$/.test(secret)) return undefined;
  const grants = (params.get('grants') || 'read').split(',').filter(grant => ['read', 'respond', 'start'].includes(grant));
  return { pairing, secret, grants };
}

export function base64url(bytes: ArrayBuffer | Uint8Array): string {
  const view = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
  let text = '';
  for (const byte of view) text += String.fromCharCode(byte);
  return btoa(text).replaceAll('+', '-').replaceAll('/', '_').replace(/=+$/, '');
}

const encoder = new TextEncoder();
async function sign(keys: CryptoKeyPair, message: string): Promise<string> {
  return base64url(await crypto.subtle.sign({ name: 'ECDSA', hash: 'SHA-256' }, keys.privateKey, encoder.encode(message)));
}

type Fetch = typeof globalThis.fetch;
async function post<T>(fetcher: Fetch, baseUrl: string, path: string, body: unknown): Promise<T> {
  let response: Response;
  try {
    response = await fetcher(`${baseUrl.replace(/\/+$/, '')}/remote/v1/${path}`, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body), credentials: 'omit', cache: 'no-store' });
  } catch (error) {
    throw new DeviceRequestError(`Your computer could not be reached. Check that it is awake and connected. (${error instanceof Error ? error.message : String(error)})`, 0);
  }
  const value = await response.json().catch(() => ({})) as { error?: string; code?: string } & T;
  if (response.status === 401 && value.code === 'revoked') throw new DeviceRevoked();
  if (!response.ok) throw new DeviceRequestError(value.error || `The Runner answered ${response.status}`, response.status);
  return value;
}

/**
 * Complete a pairing offer: generate this phone's key, prove possession of it,
 * and keep it. Replaces any earlier pairing stored on this phone.
 */
export async function pairDevice({ baseUrl, offer, name, store, fetch: fetcher = globalThis.fetch.bind(globalThis) }: { baseUrl: string; offer: PairingOffer; name: string; store: DeviceStore; fetch?: Fetch }): Promise<StoredDevice> {
  const keys = await crypto.subtle.generateKey({ name: 'ECDSA', namedCurve: 'P-256' }, false, ['sign', 'verify']) as CryptoKeyPair;
  const publicKey = base64url(await crypto.subtle.exportKey('spki', keys.publicKey));
  const signature = await sign(keys, `lazybox-device-pair-v1\0${offer.pairing}\0${publicKey}`);
  const { device } = await post<{ device: DeviceRecord }>(fetcher, baseUrl, 'pair', { pairing: offer.pairing, secret: offer.secret, name, publicKey, signature });
  const stored = { device, keys, publicKey };
  await store.save(stored);
  return stored;
}

/** Sign in: answer a fresh challenge with the device key for a short-lived access credential. */
export async function openDeviceSession({ baseUrl, stored, fetch: fetcher = globalThis.fetch.bind(globalThis) }: { baseUrl: string; stored: StoredDevice; fetch?: Fetch }): Promise<DeviceSession> {
  const id = stored.device.id;
  const { challenge } = await post<{ challenge: string }>(fetcher, baseUrl, 'challenge', { device: id });
  const signature = await sign(stored.keys, `lazybox-device-session-v1\0${id}\0${challenge}`);
  return await post<DeviceSession>(fetcher, baseUrl, 'session', { device: id, challenge, signature });
}

/** The phone's pairing in IndexedDB, which keeps a CryptoKey without exposing its private bytes. */
export function indexedDBDeviceStore(database = 'lazybox-remote'): DeviceStore {
  const open = () => new Promise<IDBDatabase>((resolve, reject) => {
    const request = indexedDB.open(database, 1);
    request.onupgradeneeded = () => { request.result.createObjectStore('device'); };
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
  const run = async <T>(mode: IDBTransactionMode, action: (store: IDBObjectStore) => IDBRequest<T>) => {
    const db = await open();
    try {
      return await new Promise<T>((resolve, reject) => {
        const request = action(db.transaction('device', mode).objectStore('device'));
        request.onsuccess = () => resolve(request.result);
        request.onerror = () => reject(request.error);
      });
    } finally { db.close(); }
  };
  return {
    async load() { const value = await run<StoredDevice | undefined>('readonly', store => store.get('current')); return value?.device?.id && value.keys?.privateKey ? value : undefined; },
    async save(value) { await run('readwrite', store => store.put(value, 'current')); },
    async clear() { await run('readwrite', store => store.delete('current')); },
  };
}

/** For tests and hosts without IndexedDB. */
export function memoryDeviceStore(initial?: StoredDevice): DeviceStore {
  let value = initial;
  return { async load() { return value; }, async save(next) { value = next; }, async clear() { value = undefined; } };
}
