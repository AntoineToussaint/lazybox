import { createContext, useCallback, useEffect, useMemo, useRef, useSyncExternalStore } from 'react';
import type { ViewStateStore } from '@lazybox/platform';
import { ACTIVITY_DETAILS, DEFAULT_ACTIVITY_DETAIL, isActivityDetail, type ActivityDetail } from './tool-lines';
import { readPreference, writePreference } from './view-state';
import { useActions } from './actions';
import { showToast } from './toasts';

/**
 * Activity detail (tool-lines.ts): one default for every conversation,
 * remembered with the app's preferences, and an override per conversation
 * (its ⋯ menu, the command palette, `] ] v`). Every chat of a window follows
 * a change at once.
 */
export const ACTIVITY_LABELS: Record<ActivityDetail, { label: string; detail: string }> = {
  quiet: { label: 'Quiet', detail: 'Only what the agent says, its decisions and its edits. Each turn’s work is one line: “Worked for 2m 10s · 14 steps · 1 failed”.' },
  smart: { label: 'Smart', detail: 'Runs of tool calls between the agent’s words become one summary line you can open. Failures, edits, questions and the plan always show.' },
  verbose: { label: 'Verbose', detail: 'Every tool call on its own line, with its exit code and duration.' },
};
const defaultKey = 'chat.activity-detail';
const overridesKey = 'chat.activity-detail.conversations';
/** Overrides kept: the conversations changed most recently. */
const OVERRIDES = 200;

let revision = 0;
const listeners = new Set<() => void>();
const changed = () => { revision++; for (const listener of listeners) listener(); };
if (typeof window !== 'undefined') window.addEventListener('storage', changed);
function subscribe(listener: () => void) { listeners.add(listener); return () => { listeners.delete(listener); }; }

export function readActivityDefault(store?: ViewStateStore): ActivityDetail {
  try { const value = readPreference(store, defaultKey); return isActivityDetail(value) ? value : DEFAULT_ACTIVITY_DETAIL; } catch { return DEFAULT_ACTIVITY_DETAIL; }
}
function readOverrides(store?: ViewStateStore): [string, ActivityDetail][] {
  try {
    const value = JSON.parse(readPreference(store, overridesKey) || '[]');
    return Array.isArray(value) ? value.filter((pair): pair is [string, ActivityDetail] => Array.isArray(pair) && typeof pair[0] === 'string' && isActivityDetail(pair[1])).slice(0, OVERRIDES) : [];
  } catch { return []; }
}
export function writeActivityDefault(store: ViewStateStore | undefined, detail: ActivityDetail) {
  try { writePreference(store, defaultKey, detail); } catch { /* kept for this window only */ }
  changed();
}
/** A conversation's own detail; `null` follows the default again. */
export function writeActivityOverride(store: ViewStateStore | undefined, conversation: string, detail: ActivityDetail | null) {
  if (!conversation) return;
  const rest = readOverrides(store).filter(([key]) => key !== conversation);
  const next = detail ? [[conversation, detail] as [string, ActivityDetail], ...rest].slice(0, OVERRIDES) : rest;
  try { writePreference(store, overridesKey, JSON.stringify(next)); } catch { /* kept for this window only */ }
  changed();
}

export type ActivityDetailState = {
  /** What this conversation shows. */
  detail: ActivityDetail;
  /** The default every conversation without its own follows. */
  fallback: ActivityDetail;
  /** This conversation has its own. */
  overridden: boolean;
  /** For this conversation only; choosing the default clears its own. */
  choose(detail: ActivityDetail | null): void;
  /** Quiet → Smart → Verbose → Quiet, for this conversation. */
  cycle(): ActivityDetail;
};
/** The global default alone (Settings → Chat). */
export function useActivityDefault(store?: ViewStateStore): [ActivityDetail, (detail: ActivityDetail) => void] {
  const version = useSyncExternalStore(subscribe, () => revision);
  const value = useMemo(() => readActivityDefault(store), [store, version]);
  return [value, useCallback((detail: ActivityDetail) => writeActivityDefault(store, detail), [store])];
}
/** A conversation's activity detail and how to change it. */
export function useActivityDetail(store: ViewStateStore | undefined, conversation: string): ActivityDetailState {
  const version = useSyncExternalStore(subscribe, () => revision);
  const { fallback, own } = useMemo(() => ({ fallback: readActivityDefault(store), own: conversation ? readOverrides(store).find(([key]) => key === conversation)?.[1] : undefined }), [store, conversation, version]);
  const detail = own || fallback;
  const latest = useRef({ detail, fallback }); latest.current = { detail, fallback };
  const choose = useCallback((next: ActivityDetail | null) => writeActivityOverride(store, conversation, next && next !== latest.current.fallback ? next : null), [store, conversation]);
  const cycle = useCallback(() => {
    const next = ACTIVITY_DETAILS[(ACTIVITY_DETAILS.indexOf(latest.current.detail) + 1) % ACTIVITY_DETAILS.length];
    choose(next);
    return next;
  }, [choose]);
  return useMemo(() => ({ detail, fallback, overridden: !!own, choose, cycle }), [detail, fallback, own, choose, cycle]);
}

/**
 * Which chat of the window a keyboard or palette action means: the one last
 * clicked or focused, else the one opened last. A chat calls `mark` when the
 * person interacts with it; `current()` says whether it is that chat.
 */
const chats: symbol[] = [];
/**
 * Whether the chat around a component is the one in front (`useCurrentChat`):
 * keys that reach the page itself, not a chat, mean that one. Outside a chat,
 * always.
 */
export const CurrentChatContext = createContext<() => boolean>(() => true);
const touch = (id: symbol) => { const i = chats.indexOf(id); if (i >= 0) chats.splice(i, 1); chats.push(id); };
export function useCurrentChat(): { mark(): void; current(): boolean } {
  const id = useRef(Symbol('chat')).current;
  useEffect(() => { touch(id); return () => { const i = chats.indexOf(id); if (i >= 0) chats.splice(i, 1); }; }, [id]);
  return useMemo(() => ({ mark: () => touch(id), current: () => chats.at(-1) === id }), [id]);
}

/**
 * A chat's activity detail with its actions (the palette's "Activity detail: …",
 * `] ] v` to cycle) acting on it while it is the chat in front. A change says
 * itself in a short toast. Call `mark` when the person interacts with the chat.
 */
export function useConversationActivity(store: ViewStateStore | undefined, conversation: string, enabled: boolean): ActivityDetailState & { mark(): void; /** This is the chat in front, which keyboard and palette actions mean. */ current(): boolean } {
  const state = useActivityDetail(store, conversation);
  const chat = useCurrentChat();
  const latest = useRef(state); latest.current = state;
  const say = (detail: ActivityDetail) => {
    const { fallback } = latest.current;
    showToast({ id: 'activity-detail', tone: 'info', title: `Activity detail: ${ACTIVITY_LABELS[detail].label}`, body: detail === fallback ? 'This conversation follows the default.' : `This conversation only. The default is ${ACTIVITY_LABELS[fallback].label}, in Settings → Chat.` });
  };
  const front = () => enabled && chat.current();
  const pick = (detail: ActivityDetail) => enabled && { run: () => { latest.current.choose(detail); say(detail); }, enabled: () => front() && latest.current.detail !== detail };
  useActions({
    'activity.cycle': enabled && { run: () => say(latest.current.cycle()), enabled: front },
    'activity.quiet': pick('quiet'),
    'activity.smart': pick('smart'),
    'activity.verbose': pick('verbose'),
    'activity.default': enabled && { run: () => { latest.current.choose(null); say(latest.current.fallback); }, enabled: () => front() && latest.current.overridden },
  });
  return useMemo(() => ({ ...state, mark: chat.mark, current: chat.current }), [state, chat]);
}
