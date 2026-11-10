import type { ViewStateStore } from '@lazybox/platform';

export type FileReference = { name: string; size: number; lastModified: number };
export type Draft = { text: string; files: FileReference[]; operationId: string };
export type ConsoleState = { search: string; follow: boolean; scrollTop: number };
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
    files.push({ name: file.name, size: file.size, lastModified: file.lastModified });
  }
  return { text: data.text, files, operationId: typeof data.operationId === 'string' && data.operationId.length <= 128 ? data.operationId : '' };
}
export function writeDraft(store: ViewStateStore | undefined, session: string, draft: Draft): void {
  store?.setItem(`${store.key}:draft:${session || 'new'}`, JSON.stringify(draft));
}
export function readConsoleState(store: ViewStateStore | undefined, command: string): ConsoleState {
  const data = read(store, `console:${command}`);
  return { search: typeof data.search === 'string' ? data.search.slice(0, 4096) : '', follow: data.follow !== false, scrollTop: typeof data.scrollTop === 'number' && Number.isFinite(data.scrollTop) ? Math.max(0, Math.min(data.scrollTop, 1e9)) : 0 };
}
export function writeConsoleState(store: ViewStateStore | undefined, command: string, state: ConsoleState): void {
  store?.setItem(`${store.key}:console:${command}`, JSON.stringify(state));
}

export type LaunchChoices = { provider: string; profile: string; model: string; gateway: string };
export function readLaunchChoices(store?: ViewStateStore): Partial<LaunchChoices> {
  if (!store) return {};
  try {
    const raw = store.getItem(`${store.scope || store.key}:launch-choices`);
    const data = raw && raw.length < 4096 ? JSON.parse(raw) : {};
    if (!data || !['claude', 'codex'].includes(data.provider)) return {};
    return Object.fromEntries(['provider', 'profile', 'model', 'gateway'].filter(k => typeof data[k] === 'string' && data[k].length < 512).map(k => [k, data[k]]));
  } catch { return {}; }
}
export function writeLaunchChoices(store: ViewStateStore | undefined, choices: LaunchChoices): void {
  store?.setItem(`${store.scope || store.key}:launch-choices`, JSON.stringify(choices));
}
