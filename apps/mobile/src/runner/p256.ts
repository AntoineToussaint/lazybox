import { p256 } from '@noble/curves/nist.js';
import { base64url, type DeviceSigner, type KeyProvider } from '@lazybox/client-core';

/**
 * The iPhone app's device key (REMOTE.md): ECDSA P-256 like the PWA's, so the
 * Runner's pairing and sign-in do not change. Its secret lives in the
 * Keychain (store.ts, this-device-only, not in backups) and in this object
 * while the app runs; nothing else in the app can read it. Signatures are
 * deterministic (RFC 6979) and raw r‖s, the form the Runner verifies.
 *
 * Next: a Secure Enclave key (MOBILE_CLOUD_HUB.md spike S2), whose secret
 * never enters the app's memory at all. It plugs in as another KeyProvider.
 */

// DER SubjectPublicKeyInfo for an uncompressed P-256 point: the fixed
// AlgorithmIdentifier (id-ecPublicKey, prime256v1) and the BIT STRING header.
const SPKI_PREFIX = Uint8Array.from([0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00]);

/** Base64url SPKI of a P-256 public key, as WebCrypto exports it. */
export function spkiOf(secret: Uint8Array): string {
  const point = p256.getPublicKey(secret, false);
  const spki = new Uint8Array(SPKI_PREFIX.length + point.length);
  spki.set(SPKI_PREFIX); spki.set(point, SPKI_PREFIX.length);
  return base64url(spki);
}

export class P256Key implements DeviceSigner {
  readonly publicKey: string;
  constructor(private readonly secret: Uint8Array) { this.publicKey = spkiOf(secret); }
  async sign(message: Uint8Array): Promise<Uint8Array> { return p256.sign(message, this.secret); }
  /** For the Keychain only (store.ts). */
  exportSecret(): string { return Array.from(this.secret, byte => byte.toString(16).padStart(2, '0')).join(''); }
  static fromSecret(hex: string): P256Key | undefined {
    if (!/^[0-9a-f]{64}$/.test(hex)) return undefined;
    const secret = Uint8Array.from(hex.match(/../g)!, pair => parseInt(pair, 16));
    return p256.utils.isValidSecretKey(secret) ? new P256Key(secret) : undefined;
  }
}

/** A KeyProvider over the given secure random source (expo-crypto on the phone, node:crypto in tests). */
export function p256Keys(random: (length: number) => Uint8Array): KeyProvider {
  return async () => {
    // 48 random bytes (noble's seed length for P-256), reduced into the curve order without bias (FIPS 186-5 A.2.1).
    const key = new P256Key(p256.utils.randomSecretKey(random(48)));
    return { keys: key, publicKey: key.publicKey };
  };
}
