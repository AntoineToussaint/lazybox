import { useCallback, useMemo, useSyncExternalStore } from 'react';

/**
 * Pinned conversations on the phone, behind one interface so the store can
 * change without the Inbox changing. The phone shows the computer's pins,
 * which the Runner keeps on each conversation (`runnerPinStore`, shared, and
 * writable for a phone with the respond grant). `localPinStore` is the pins
 * a phone kept on itself before: moved to the Runner once where the phone may
 * pin, otherwise still shown beside the computer's.
 */
export interface PinStore {
  /** Whether these pins are the computer's too. */
  readonly shared: boolean;
  /** Whether the phone may change them. */
  readonly writable: boolean;
  list(): readonly string[];
  set(keys: readonly string[]): void;
  subscribe(listener: () => void): () => void;
}

export const maxPins = 64;

/** Pure: the pins after toggling one; the newest pin goes first. */
export function togglePin(pins: readonly string[], key: string): string[] {
  return pins.includes(key) ? pins.filter(value => value !== key) : [key, ...pins].slice(0, maxPins);
}

export function parsePins(text: string | null): string[] {
  try {
    const value = text ? JSON.parse(text) : [];
    return Array.isArray(value) ? value.filter((key): key is string => typeof key === 'string' && key.length < 256).slice(0, maxPins) : [];
  } catch { return []; }
}

/** This phone's pins, in its own storage, per paired device. */
export function localPinStore(root: string): PinStore {
  const key = `${root}:pins:v1`;
  const listeners = new Set<() => void>();
  let cached: { text: string | null; pins: string[] } | undefined;
  const read = () => {
    let text: string | null = null;
    try { text = localStorage.getItem(key); } catch { /* Private mode: pins live in memory for this visit. */ }
    if (!cached || cached.text !== text) cached = { text, pins: parsePins(text) };
    return cached.pins;
  };
  let memory: string[] | undefined;
  return {
    shared: false, writable: true,
    list: () => memory ?? read(),
    set(keys) {
      const text = JSON.stringify(keys.slice(0, maxPins));
      try { localStorage.setItem(key, text); memory = undefined; } catch { memory = [...keys]; }
      cached = undefined;
      for (const listener of listeners) listener();
    },
    subscribe(listener) {
      listeners.add(listener);
      const storage = (event: StorageEvent) => { if (event.key === key) { cached = undefined; listener(); } };
      window.addEventListener('storage', storage);
      return () => { listeners.delete(listener); window.removeEventListener('storage', storage); };
    },
  };
}

export function usePins(store: PinStore): { pinned: readonly string[]; toggle(key: string): void; shared: boolean; writable: boolean } {
  const pinned = useSyncExternalStore(store.subscribe, store.list);
  const toggle = useCallback((key: string) => { if (store.writable) store.set(togglePin(store.list(), key)); }, [store]);
  return useMemo(() => ({ pinned, toggle, shared: store.shared, writable: store.writable }), [pinned, toggle, store]);
}

/** What a Runner pin store needs from the Runner: pin one conversation, and read again. */
export type PinRunner = { pin(sessionId: string, pinned: boolean): Promise<unknown>; refresh(): void };
/** A conversation as the store sees it: its key (conversationKey), an attempt's id, and when it was pinned (0: not). */
export type PinnedConversation = { key: string; sessionId: string; pinnedAt: number };
export type RunnerPinStore = PinStore & { update(conversations: readonly PinnedConversation[]): void };

/**
 * The computer's pins, kept by the Runner on each conversation
 * (Session.pinned_at): `shared: true`. Writable when the device allowlist lets
 * this phone pin (the respond grant); a read-only phone shows them, with the
 * pins it kept on itself before (`legacy`), and changes neither. Oldest pin
 * first, as on the computer. A change shows at once and is undone if the
 * Runner refuses it.
 */
export function runnerPinStore(runner: PinRunner, { writable, legacy = [] }: { writable: boolean; legacy?: readonly string[] }): RunnerPinStore {
  const listeners = new Set<() => void>();
  let conversations: readonly PinnedConversation[] = [];
  const pending = new Map<string, boolean>();
  let cached: string[] = [], signature = '';
  const compute = () => {
    const pinned = conversations.filter(row => pending.get(row.key) ?? row.pinnedAt > 0)
      .sort((a, b) => (a.pinnedAt || Number.MAX_SAFE_INTEGER) - (b.pinnedAt || Number.MAX_SAFE_INTEGER) || (a.key < b.key ? -1 : 1))
      .map(row => row.key);
    const all = [...new Set([...pinned, ...legacy])].slice(0, maxPins);
    const next = all.join('\n');
    if (next === signature) return;
    signature = next; cached = all;
    for (const listener of listeners) listener();
  };
  return {
    shared: true, writable,
    list: () => cached,
    set(keys) {
      if (!writable) return;
      const current = cached;
      for (const row of conversations) {
        const want = keys.includes(row.key);
        if (want === current.includes(row.key)) continue;
        pending.set(row.key, want);
        void runner.pin(row.sessionId, want).then(() => runner.refresh(), () => { pending.delete(row.key); compute(); });
      }
      compute();
    },
    update(next) {
      conversations = next;
      // A change the Runner now reports is no longer pending.
      for (const [key, want] of pending) { const row = next.find(value => value.key === key); if (!row || (row.pinnedAt > 0) === want) pending.delete(key); }
      compute();
    },
    subscribe(listener) { listeners.add(listener); return () => { listeners.delete(listener); }; },
  };
}
