import * as SecureStore from 'expo-secure-store';
import { getRandomBytes } from 'expo-crypto';
import type { DeviceRecord, DeviceStore, StoredDevice } from '@lazybox/client-core';
import { P256Key, p256Keys } from './p256';

/** This phone's pairing with one Runner: the gateway it reaches, its device record and its key. */
export type Pairing = { baseUrl: string; stored: StoredDevice };

type Saved = { v: 1; baseUrl: string; device: DeviceRecord; secret: string };

const KEY = 'lazybox.pairing';
/**
 * In the Keychain, readable only while the phone is unlocked and never in a
 * backup or on another device: a restored phone pairs again. (Notification
 * actions that sign while locked will need AFTER_FIRST_UNLOCK_THIS_DEVICE_ONLY;
 * see MOBILE_CLOUD_HUB.md §2.3.)
 */
const OPTIONS: SecureStore.SecureStoreOptions = { keychainAccessible: SecureStore.WHEN_UNLOCKED_THIS_DEVICE_ONLY };

export const keyProvider = p256Keys(length => getRandomBytes(length));

export async function loadPairing(): Promise<Pairing | undefined> {
  const text = await SecureStore.getItemAsync(KEY, OPTIONS).catch(() => null);
  if (!text) return undefined;
  try {
    const saved = JSON.parse(text) as Saved;
    const key = saved.v === 1 && typeof saved.baseUrl === 'string' && saved.device?.id ? P256Key.fromSecret(saved.secret) : undefined;
    return key && { baseUrl: saved.baseUrl, stored: { device: saved.device, keys: key, publicKey: key.publicKey } };
  } catch { return undefined; }
}

export async function clearPairing(): Promise<void> {
  await SecureStore.deleteItemAsync(KEY, OPTIONS);
}

/** The DeviceStore `pairDevice` saves into, for a pairing with the Runner at `baseUrl`. */
export function pairingStore(baseUrl: string): DeviceStore {
  return {
    load: async () => (await loadPairing())?.stored,
    async save(value) {
      if (!(value.keys instanceof P256Key)) throw new Error('This phone keeps only its own Keychain key.');
      const saved: Saved = { v: 1, baseUrl, device: value.device, secret: value.keys.exportSecret() };
      await SecureStore.setItemAsync(KEY, JSON.stringify(saved), OPTIONS);
    },
    clear: clearPairing,
  };
}
