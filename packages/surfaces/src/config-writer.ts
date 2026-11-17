/**
 * Where a change made in the app goes to the person's config files (POWER.md,
 * "Config files"). Settings and the keyboard functions call `writeConfig`
 * after they change what the app holds; the window's config bridge
 * (useConfigFiles.ts) registers the writer that turns the change into edits of
 * keys.yaml or settings.yaml. Without a bridge (a test, no Runner), nothing is
 * written and the app keeps the change as before.
 */
import type { NotificationPreferences } from './attention';
import type { AppearancePreferences } from './appearance/preferences';

export type ConfigWrite =
  /** keys.yaml from the keymap, ]] menu and Vim choice the app now holds; preset when the person picked one. */
  | { file: 'keys'; preset?: string }
  /** settings.yaml from these notification preferences. */
  | { file: 'settings'; notifications: NotificationPreferences }
  /** settings.yaml's appearance section from these preferences; zoom (the desktop app's) when it changed. */
  | { file: 'settings'; appearance?: AppearancePreferences; zoom?: number };
/** saved: the file says it now; message: what to tell the person ("Saved to keys.yaml", or why not). */
export type ConfigWriteResult = { saved: boolean; message: string };
type Writer = (request: ConfigWrite, options: { quiet: boolean }) => Promise<ConfigWriteResult>;

let writer: Writer | undefined;
export function setConfigWriter(next: Writer): () => void {
  writer = next;
  return () => { if (writer === next) writer = undefined; };
}
/** Writes the change to its file. Never rejects; quiet leaves telling the person to the caller. */
export function writeConfig(request: ConfigWrite, options: { quiet?: boolean } = {}): Promise<ConfigWriteResult> {
  return writer ? writer(request, { quiet: !!options.quiet }).catch(error => ({ saved: false, message: String(error) })) : Promise.resolve({ saved: false, message: '' });
}

/**
 * A change that comes in bursts (a slider dragged, the zoom keys held): only
 * the last one in `ms` is written. Until it is, the main window does not apply
 * the file over it (configWritesPending), so nothing jumps back mid-drag.
 */
const soon = new Map<string, { timer: ReturnType<typeof setTimeout>; request: ConfigWrite }>();
export function writeConfigSoon(key: string, request: ConfigWrite, ms = 400): void {
  if (!writer) return;
  const pending = soon.get(key);
  if (pending) clearTimeout(pending.timer);
  soon.set(key, { request, timer: setTimeout(() => { const entry = soon.get(key); soon.delete(key); if (entry) void writeConfig(entry.request); }, ms) });
}
export const configWritesPending = () => soon.size > 0;
// A window going away sends what it still holds, as far as it gets.
if (typeof window !== 'undefined') window.addEventListener('pagehide', () => {
  for (const [key, entry] of soon) { clearTimeout(entry.timer); soon.delete(key); void writeConfig(entry.request); }
});
