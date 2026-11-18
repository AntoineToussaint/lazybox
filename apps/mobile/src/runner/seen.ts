import { useSyncExternalStore } from 'react';
import { markSeen, type Seen } from '@lazybox/surfaces/models';

/**
 * What this phone has seen (the PWA keeps the same marks in localStorage).
 * Everything before the app opened counts as seen, so a first launch does not
 * flood the Inbox with old turns. Kept in memory for now: the next step is the
 * offline cache (MOBILE_CLOUD_HUB.md §2.4), which keeps it with the last Inbox.
 */
let seen: Seen = { since: Date.now(), marks: {} };
const listeners = new Set<() => void>();
const subscribe = (listener: () => void) => { listeners.add(listener); return () => { listeners.delete(listener); }; };

export function useSeen(): Seen { return useSyncExternalStore(subscribe, () => seen); }
export function markRead(key: string, at: number) {
  const next = markSeen(seen, key, at);
  if (next !== seen) { seen = next; listeners.forEach(listener => listener()); }
}
