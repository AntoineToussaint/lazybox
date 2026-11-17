import type { ViewStateStore } from '@lazybox/platform';
import { isAcp } from './providers';

/** A file of a saved draft. A pasted text box keeps its text, so it comes back after a reload; any other file is attached again. */
export type FileReference = { name: string; size: number; lastModified: number; text?: string };
export type Draft = { text: string; files: FileReference[]; operationId: string };
/** A console's view of one command: its search, whether it follows the end, where it was read, and the sources it hides. */
export type ConsoleState = { search: string; follow: boolean; scrollTop: number; hidden?: string[] };
const blank = (): Draft => ({ text: '', files: [], operationId: '' });

function read(store: ViewStateStore | undefined, key: string): Record<string, unknown> {
  if (!store) return {};
  const text = store.getItem(`${store.key}:${key}`);
  if (!text || text.length > 1 << 20) return {};
  try { const value = JSON.parse(text); return value && typeof value === 'object' && !Array.isArray(value) ? value : {}; } catch { return {}; }
}
export function readDraft(store: ViewStateStore | undefined, session: string): Draft {
  const data = read(store, `draft:${session || 'new'}`);
  if (typeof data.text !== 'string' || !Array.isArray(data.files) || data.files.length > 4) return blank();
  const files: FileReference[] = [];
  for (const file of data.files) {
    if (!file || typeof file.name !== 'string' || !file.name || file.name.length > 255 || !Number.isSafeInteger(file.size) || file.size < 0 || !Number.isSafeInteger(file.lastModified)) return blank();
    if (file.text !== undefined && typeof file.text !== 'string') return blank();
    files.push({ name: file.name, size: file.size, lastModified: file.lastModified, ...(typeof file.text === 'string' ? { text: file.text } : {}) });
  }
  return { text: data.text, files, operationId: typeof data.operationId === 'string' && data.operationId.length <= 128 ? data.operationId : '' };
}
export function writeDraft(store: ViewStateStore | undefined, session: string, draft: Draft): void {
  store?.setItem(`${store.key}:draft:${session || 'new'}`, JSON.stringify(draft));
}
export function readConsoleState(store: ViewStateStore | undefined, command: string): ConsoleState {
  const data = read(store, `console:${command}`);
  const hidden = Array.isArray(data.hidden) ? data.hidden.filter((name): name is string => typeof name === 'string').slice(0, 64) : [];
  return { search: typeof data.search === 'string' ? data.search.slice(0, 4096) : '', follow: data.follow !== false, scrollTop: typeof data.scrollTop === 'number' && Number.isFinite(data.scrollTop) ? Math.max(0, Math.min(data.scrollTop, 1e9)) : 0, ...(hidden.length ? { hidden } : {}) };
}
export function writeConsoleState(store: ViewStateStore | undefined, command: string, state: ConsoleState): void {
  store?.setItem(`${store.key}:console:${command}`, JSON.stringify(state));
}

/** App-wide preferences (keymap, snippet lists) are shared by every window of one Runner.
 * Hosts only accept keys under the Runner authority, so a fixed global name would be rejected. */
export function preferenceKey(store: ViewStateStore, name: string): string {
  return `${store.root || (store.scope || store.key).split(':')[0]}:${name}`;
}
/** Reads a preference, falling back to the fixed key older builds used. */
export function readPreference(store: ViewStateStore | undefined, name: string): string | null {
  if (!store) return localStorage.getItem(`lazybox:${name}`);
  return store.getItem(preferenceKey(store, name)) ?? store.getItem(`lazybox:${name}`);
}
/** Event every window gets when an app-wide preference changes, in it or in another window. */
export const PREFERENCE_EVENT = 'lazybox-preference';
export function writePreference(store: ViewStateStore | undefined, name: string, value: string): void {
  if (store) {
    const key = preferenceKey(store, name);
    store.setItem(key, value);
    store.share?.(key, value);
  } else localStorage.setItem(`lazybox:${name}`, value);
  if (typeof window !== 'undefined') window.dispatchEvent(new CustomEvent(PREFERENCE_EVENT, { detail: { name } }));
}

export type LaunchChoices = { provider: string; profile: string; model: string; gateway: string };
export function readLaunchChoices(store?: ViewStateStore): Partial<LaunchChoices> {
  if (!store) return {};
  try {
    const raw = store.getItem(`${store.scope || store.key}:launch-choices`);
    const data = raw && raw.length < 4096 ? JSON.parse(raw) : {};
    if (!data || !(['claude', 'codex'].includes(data.provider) || isAcp(data.provider))) return {};
    return Object.fromEntries(['provider', 'profile', 'model', 'gateway'].filter(k => typeof data[k] === 'string' && data[k].length < 512).map(k => [k, data[k]]));
  } catch { return {}; }
}
export function writeLaunchChoices(store: ViewStateStore | undefined, choices: LaunchChoices): void {
  store?.setItem(`${store.scope || store.key}:launch-choices`, JSON.stringify(choices));
}
