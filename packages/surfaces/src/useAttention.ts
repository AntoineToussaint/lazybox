import { createContext, createElement, useCallback, useContext, useEffect, useMemo, useRef, useState, useSyncExternalStore, type ReactNode } from 'react';
import type { Session } from '@lazybox/protocol';
import type { AttentionHost } from '@lazybox/platform';
import { useRunner } from './RunnerContext';
import { mergeConversations } from './conversation-model';
import { buildInbox, conversationKey, defaultNotificationPreferences, deriveAttention, markSeen, reachesInbox, parseSeen, seenAt, seenMark, type Inbox, type NotificationPreferences, type Seen } from './attention';

/**
 * Attention state is shared by every window of one Runner, live: reading a
 * conversation in a detached chat window marks it seen in the main window's
 * inbox too. It lives in localStorage (one origin for every window of the app,
 * with `storage` events between them) under the Runner authority. A window's
 * ViewStateStore is a per-window snapshot, so it cannot carry this.
 */
const Scope = createContext<{ root: string; host?: AttentionHost }>({ root: '' });
/** The Runner authority attention is kept under, and the host that can notify. */
export function AttentionScope({ root, host, children }: { root?: string; host?: AttentionHost; children: ReactNode }) {
  const value = useMemo(() => ({ root: root || '', host }), [root, host]);
  return createElement(Scope.Provider, { value }, children);
}
function useRoot(): string {
  const { root } = useContext(Scope), { connection } = useRunner();
  return root || connection.baseUrl;
}
/** Where this part of the window keeps its Runner's seen marks and notification choices. */
export const useAttentionRoot = useRoot;
export function useAttentionHost(): AttentionHost | undefined {
  return useContext(Scope).host;
}
function storage(): Storage | undefined {
  try { return globalThis.localStorage; } catch { return undefined; }
}
const changed = 'lazybox-attention';
const seenKey = (root: string) => `${root}:attention:v1`;
const preferencesKey = (root: string) => `${root}:notifications:v1`;
const viewingKey = (root: string) => `${root}:attention:viewing`;

// Every row of the sidebar reads these on each render; reads come from memory,
// and another window's write (a `storage` event) invalidates them. Registered
// at import, so it runs before any component's own `storage` listener.
const cache = new Map<string, string | null>();
window.addEventListener('storage', () => cache.clear());
export function subscribeAttention(listener: () => void) { return subscribe(listener); }
function subscribe(listener: () => void) {
  window.addEventListener('storage', listener); window.addEventListener(changed, listener);
  return () => { window.removeEventListener('storage', listener); window.removeEventListener(changed, listener); };
}
function write(key: string, value: string) {
  cache.set(key, value);
  try { storage()?.setItem(key, value); } catch { /* Quota or a private window: attention stays in memory for this window. */ }
  window.dispatchEvent(new Event(changed));
}
function read(key: string): string | null {
  if (cache.has(key)) return cache.get(key)!;
  let value: string | null = null;
  try { value = storage()?.getItem(key) ?? null; } catch { /* Unavailable storage reads as nothing seen yet. */ }
  cache.set(key, value);
  return value;
}
/** The raw seen record of a Runner's root (another Runner of the window: RunnerDirectory.tsx). */
export function readSeenText(root: string): string | null { return read(seenKey(root)); }
/** Marks conversations of the Runner kept under `root` as seen. */
export function markSeenIn(root: string, entries: { key: string; at: number }[]) {
  const current = loadSeen(root);
  let next = current;
  for (const entry of entries) next = markSeen(next, entry.key, entry.at);
  if (next !== current) write(seenKey(root), JSON.stringify(next));
}
function loadSeen(root: string): Seen {
  const text = read(seenKey(root));
  const seen = parseSeen(text, Date.now());
  if (text === null) write(seenKey(root), JSON.stringify(seen));
  return seen;
}

/** What this Runner's windows have seen, and a way to mark more as seen. */
export function useSeen() {
  const root = useRoot();
  const text = useSyncExternalStore(subscribe, () => read(seenKey(root)));
  const seen = useMemo(() => parseSeen(text, Date.now()), [text]);
  // The first look starts the record: what happened before it counts as seen.
  useEffect(() => { if (text === null) loadSeen(root); }, [root, text]);
  const mark = useCallback((entries: { key: string; at: number }[]) => markSeenIn(root, entries), [root]);
  /** Undoes a mark: a conversation's mark goes back to what it was (undefined: none). */
  const restore = useCallback((key: string, previous: number | undefined) => {
    const current = loadSeen(root);
    const marks = { ...current.marks };
    if (previous === undefined) delete marks[key]; else marks[key] = previous;
    write(seenKey(root), JSON.stringify({ since: current.since, marks }));
  }, [root]);
  return { seen, mark, restore };
}

/** Every work conversation this Runner knows, as an inbox. */
export function useInbox(): Inbox & { mark(entries: { key: string; at: number }[]): void } {
  const { info } = useRunner();
  const { seen, mark } = useSeen();
  const sessions = useMemo(() => mergeConversations([], info?.sessions || []), [info?.sessions]);
  return { ...useMemo(() => buildInbox(sessions, seen), [sessions, seen]), mark };
}

/**
 * A sidebar row's marker: something finished or failed there since it was
 * last seen. Needs-input has its own status color, so it is not repeated.
 */
export function useUnread(): (session: Session) => '' | 'review' | 'failed' {
  const { seen } = useSeen();
  return useCallback((session: Session) => {
    if (!reachesInbox(session)) return '';
    const row = deriveAttention(session, seenAt(seen, conversationKey(session)));
    return row.unread && (row.state === 'review' || row.state === 'failed') ? row.state : '';
  }, [seen]);
}

export function useNotificationPreferences():[NotificationPreferences, (next: NotificationPreferences) => void] {
  const root = useRoot();
  const text = useSyncExternalStore(subscribe, () => read(preferencesKey(root)));
  const preferences = useMemo(() => {
    try { const value = JSON.parse(text || '{}'); return Object.fromEntries(Object.entries(defaultNotificationPreferences).map(([key, fallback]) => [key, typeof value[key] === 'boolean' ? value[key] : fallback])) as NotificationPreferences; }
    catch { return defaultNotificationPreferences; }
  }, [text]);
  return [preferences, useCallback((next: NotificationPreferences) => write(preferencesKey(root), JSON.stringify(next)), [root])];
}

const windowId = crypto.randomUUID();
const viewingFresh = 90_000;
/** The conversation a focused window of this Runner is showing, if any. */
export function readViewing(root: string): string {
  try {
    const value = JSON.parse(read(viewingKey(root)) || 'null');
    return value && typeof value.key === 'string' && Date.now() - value.at < viewingFresh ? value.key : '';
  } catch { return ''; }
}
export function useViewingReader(): () => string {
  const root = useRoot();
  return useCallback(() => readViewing(root), [root]);
}

export function attentionWindowFocused() {
  return document.visibilityState === 'visible' && document.hasFocus();
}
/**
 * Whether the panel around a conversation is on screen: false for a chat tab
 * behind another (or in a minimized group), which must not mark its
 * conversation seen. True where no page says otherwise (a chat's own window).
 */
export const PanelShownContext = createContext(true);
/**
 * A conversation on screen in a focused window is seen: its turns stop being
 * "ready for review", and no notification is raised for it. Marks follow the
 * freshest view of the session (this panel's or the Runner index's).
 */
export function useMarkViewed(session: Session | undefined, enabled = true) {
  const root = useRoot();
  const { info } = useRunner();
  const { mark } = useSeen();
  const live = session && info?.sessions.find(value => value.id === session.id);
  const shown = useContext(PanelShownContext);
  const active = enabled && shown && !!session && reachesInbox(session);
  const key = session ? conversationKey(session) : '';
  const at = active ? Math.max(...[session, live].filter((value): value is Session => !!value).map(value => seenMark(deriveAttention(value)))) : 0;
  const latest = useRef({ key, at }); latest.current = { key, at };
  const [focused, setFocused] = useState(attentionWindowFocused);
  useEffect(() => {
    if (!active) return;
    const update = () => setFocused(attentionWindowFocused());
    // Focus may have left while this view was inactive (it listened to nothing then): read it now.
    update();
    window.addEventListener('focus', update); window.addEventListener('blur', update); document.addEventListener('visibilitychange', update);
    return () => { window.removeEventListener('focus', update); window.removeEventListener('blur', update); document.removeEventListener('visibilitychange', update); };
  }, [active]);
  // `focused` can still be the stale value in the commit that makes the view active (the read
  // above lands in the next render), so the window is asked again before marking or publishing:
  // a conversation opened in a window in the background is not "seen".
  useEffect(() => { if (active && focused && at && attentionWindowFocused()) mark([{ key, at }]); }, [active, focused, key, at, mark]);
  useEffect(() => {
    if (!active || !focused || !attentionWindowFocused()) return;
    const publish = () => write(viewingKey(root), JSON.stringify({ key: latest.current.key, window: windowId, at: Date.now() }));
    publish();
    const timer = setInterval(publish, viewingFresh / 3);
    return () => {
      clearInterval(timer);
      try { const value = JSON.parse(read(viewingKey(root)) || 'null'); if (value?.window === windowId) write(viewingKey(root), 'null'); } catch { /* Stale presence expires on its own. */ }
    };
  }, [active, focused, key, root]);
}
