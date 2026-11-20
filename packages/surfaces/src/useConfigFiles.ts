import { useEffect, useRef, useState, useSyncExternalStore } from 'react';
import type { ViewStateStore, ZoomHost } from '@lazybox/platform';
import type { ConfigFiles } from '@lazybox/protocol';
import { useRunner } from './RunnerContext';
import { configWritesPending, setConfigWriter, writeConfigSoon, type ConfigWrite, type ConfigWriteResult } from './config-writer';
import { appearanceEdits, appearanceFromFile, appearanceToFile, defaultAppearance, defaultKeys, defaultNotifications, errorCount, fileLabel, firstProblem, keysEdits, keysFromFile, layoutFileName, layoutYaml, notificationsFromFile, parseDoc, sameAppearance, settingsEdits, type ConfigDoc, type ConfigEdit, type KeysValue, type SettingsValue } from './config-files';
import { getAppearance } from './appearance/apply';
import { APPEARANCE_UNSYNCED, saveAppearancePreferences } from './appearance/useAppearance';
import { parseSavedLayouts, SAVED_LAYOUTS_KEY } from './layout-presets';
import { applyKeysFromFile, keyboardPreset, readKeymap, readMenu, readVimPreferences } from './keyboard';
import { useNotificationPreferences } from './useAttention';
import { readPreference, writePreference } from './view-state';
import { settingsError } from './settings-parts';
import { dismissToast, showToast } from './toasts';
import { runAction } from './actions';

/** How often the files are looked at: the Runner rereads them every second. */
export const CONFIG_POLL_MS = 2000;

export type ConfigState = {
  /** The folder holding them, '' before the first answer. */
  directory: string;
  docs: ConfigDoc[];
  revision: number;
  loaded: boolean;
  error: string;
  /** Problems the app found applying a file the Runner accepted (a keymap it refuses), by file name. */
  appProblems: Record<string, string>;
};

let state: ConfigState = { directory: '', docs: [], revision: -1, loaded: false, error: '', appProblems: {} };
const listeners = new Set<() => void>();
const publish = (next: Partial<ConfigState>) => { state = { ...state, ...next }; for (const listener of listeners) listener(); };
const subscribe = (listener: () => void) => { listeners.add(listener); return () => { listeners.delete(listener); }; };
const accept = (files: ConfigFiles) => {
  if (Number(files.revision) === state.revision && state.loaded) return;
  publish({ directory: files.directory, docs: files.files.map(file => parseDoc(file)), revision: Number(files.revision), loaded: true, error: '' });
};

type Update = { name: string; edits?: ConfigEdit[]; content?: string; remove?: boolean };
let updater: ((request: Update) => Promise<ConfigFiles>) | undefined;
/** Writes from this window not yet answered: the file is not applied over them. */
let writing = 0;
/**
 * Changes one config file through the Runner (comments kept; nothing written
 * when the result would not load) and publishes what it then holds. Rejects
 * with a sentence for the person.
 */
export async function updateConfigFile(request: Update): Promise<void> {
  if (!updater) throw new Error('This window is not connected to a Runner.');
  try { accept(await updater(request)); }
  catch (error) { throw new Error(settingsError(error)); }
}

/** Every config file as the Runner last loaded it, live. */
export function useConfigFiles(): ConfigState { return useSyncExternalStore(subscribe, () => state, () => state); }
export function configDocs(): ConfigDoc[] { return state.docs; }
export function configDoc<T>(name: string): ConfigDoc<T> | undefined { return state.docs.find(doc => doc.name === name) as ConfigDoc<T> | undefined; }

/** Whether a file was ever in force here, so its removal means "back to the defaults". */
const SEEN = 'config-files:seen:v1';
const readSeen = (store?: ViewStateStore): Record<string, boolean> => { try { return JSON.parse(readPreference(store, SEEN) || '{}') || {}; } catch { return {}; } };
const markSeen = (store: ViewStateStore | undefined, name: string) => { const seen = readSeen(store); if (!seen[name]) writePreference(store, SEEN, JSON.stringify({ ...seen, [name]: true })); };

/** settings.yaml's appearance section was in force here, so its absence means "back to the defaults". */
const APPEARANCE_SEEN = 'settings#appearance';

/**
 * One per workspace window: keeps the config files loaded (polled, and at
 * once when the window regains focus) and writes the app's changes to them.
 * The main window also applies them: a file saved in an editor changes the
 * keys, notifications and appearance (and the desktop app's zoom) within two
 * seconds, a file removed puts back the defaults, what a person set before
 * files existed becomes their file once, and a file with a mistake is
 * announced with its line (the last good version stays in force).
 */
export function useConfigFilesBridge(primary: boolean, viewState?: ViewStateStore, zoom?: ZoomHost) {
  const { api } = useRunner();
  const [notifications, saveNotifications] = useNotificationPreferences();
  const live = useRef({ notifications, saveNotifications, viewState }); live.current = { notifications, saveNotifications, viewState };
  // The desktop app's zoom as last heard (undefined: not known yet, or no zoom here).
  const zoomLevel = useRef<number | undefined>(undefined);
  const [zoomKnown, setZoomKnown] = useState(!zoom);

  // Reads, and the writer every window registers.
  useEffect(() => {
    updater = request => api.updateConfigFile({ name: request.name, edits: request.edits || [], content: request.content || '', remove: !!request.remove }, { timeoutMs: 10_000 });
    const control = new AbortController();
    let busy = false;
    const load = async () => {
      if (busy) return; busy = true;
      try { accept(await api.getConfigFiles({}, { signal: control.signal, timeoutMs: 5000 })); }
      catch (error) { if (!control.signal.aborted) publish({ error: settingsError(error, 'load') }); }
      finally { busy = false; }
    };
    void load();
    const timer = setInterval(() => { if (document.visibilityState !== 'hidden') void load(); }, CONFIG_POLL_MS);
    const focus = () => void load();
    window.addEventListener('focus', focus);
    const editsFor = (request: ConfigWrite, doc: ConfigDoc<unknown> | undefined): ConfigEdit[] => {
      const { viewState: store } = live.current;
      if (request.file === 'keys') return keysEdits({ map: readKeymap(store), menu: readMenu(store), vim: readVimPreferences(store), preset: request.preset }, doc?.exists ? doc.value as KeysValue : undefined);
      const file = doc?.exists ? doc.value as SettingsValue : undefined;
      if ('notifications' in request) return settingsEdits(request.notifications, file);
      // A window that does not know the zoom (a browser) leaves the file's as it is.
      if (request.zoom !== undefined && zoomLevel.current !== undefined) zoomLevel.current = request.zoom;
      const level = request.zoom ?? zoomLevel.current ?? file?.appearance?.zoom;
      return appearanceEdits(appearanceToFile(request.appearance || getAppearance().preferences, level), file?.appearance);
    };
    const write = async (request: ConfigWrite, { quiet }: { quiet: boolean }): Promise<ConfigWriteResult> => {
      const { viewState: store } = live.current;
      const name = request.file;
      const doc = configDoc<unknown>(name);
      // A file that does not load now is not overwritten from the app: its text is the person's.
      if (doc?.exists && doc.error && !doc.stale) return report({ saved: false, message: `${fileLabel(name)} does not load (${doc.error}); fix it in your editor first.` }, quiet);
      const edits = editsFor(request, doc);
      const appearance = request.file === 'settings' && !('notifications' in request);
      const synced = () => { if (appearance && !configWritesPending()) writePreference(store, APPEARANCE_UNSYNCED, ''); };
      if (!edits.length) { synced(); return { saved: true, message: doc?.exists ? `Saved to ${fileLabel(name)}.` : 'Saved.' }; }
      try {
        await updateConfigFile({ name, edits });
        markSeen(store, name);
        if (appearance) { markSeen(store, APPEARANCE_SEEN); synced(); }
        return { saved: true, message: `Saved to ${fileLabel(name)}.` };
      } catch (error) {
        return report({ saved: false, message: `Not saved to ${fileLabel(name)}: ${error instanceof Error ? error.message : String(error)}` }, quiet);
      }
    };
    // One write at a time, each computed from what the file holds after the
    // one before (a reset and its Undo in quick succession both land); while
    // any is pending, the main window does not apply the file over them.
    let chain: Promise<unknown> = Promise.resolve();
    const unregister = setConfigWriter((request, options) => {
      writing++;
      const next = chain.then(() => write(request, options)).finally(() => { writing--; publish({}); });
      chain = next.catch(() => undefined);
      return next;
    });
    return () => { control.abort(); clearInterval(timer); window.removeEventListener('focus', focus); unregister(); if (updater) updater = undefined; };
  }, [api]);

  // Load and mirror the desktop’s saved default. Window shortcuts are local.
  useEffect(() => {
    if (!zoom) { zoomLevel.current = undefined; setZoomKnown(true); return; }
    let alive = true;
    void zoom.get().then(level => { if (alive) { zoomLevel.current = level; setZoomKnown(true); } }, () => { if (alive) setZoomKnown(true); });
    const unsubscribe = zoom.subscribe(level => {
      if (zoomLevel.current === level) return;
      zoomLevel.current = level;
      if (primary) writeConfigSoon('zoom', { file: 'settings', zoom: level });
    });
    return () => { alive = false; unsubscribe(); };
  }, [zoom, primary]);

  // The main window applies the files.
  const applied = useRef(new Map<string, number>());
  const movedLayouts = useRef(false);
  const announced = useRef(new Map<string, string>());
  const files = useConfigFiles();
  useEffect(() => {
    if (!primary || !files.loaded || writing || configWritesPending()) return;
    const { viewState: store, notifications: current, saveNotifications: save } = live.current;
    const seen = readSeen(store);
    const problems: Record<string, string> = {};
    // What the person set before files existed, to move into settings.yaml in one write.
    const moving: ConfigEdit[] = [], moved: string[] = [];
    for (const name of ['keys', 'settings']) {
      const doc = files.docs.find(d => d.name === name);
      if (!doc) continue;
      if (applied.current.get(name) === doc.revision) { if (state.appProblems[name]) problems[name] = state.appProblems[name]; continue; }
      // The appearance waits for the desktop zoom to be known, so moving it into the file keeps it.
      if (name === 'settings' && !zoomKnown) continue;
      applied.current.set(name, doc.revision);
      if (doc.exists && doc.value) {
        if (name === 'keys') {
          const next = keysFromFile(doc.value as KeysValue);
          if (next.problem) problems.keys = `The keys do not apply: ${next.problem} The last keys that worked stay in force.`;
          else applyKeysFromFile(store, next);
        } else {
          const value = doc.value as SettingsValue;
          const next = notificationsFromFile(value);
          if (JSON.stringify(next) !== JSON.stringify(current)) save(next);
          const section = value.appearance || {};
          // A change made here that never reached the file (the window closed first) is newer than the file.
          if (readPreference(store, APPEARANCE_UNSYNCED) === '1') { moving.push(...appearanceEdits(appearanceToFile(getAppearance().preferences, zoomLevel.current ?? section.zoom), section)); moved.push('unsynced'); }
          else if (Object.keys(section).length || seen[APPEARANCE_SEEN]) { applyAppearanceFile(store, section, zoom, zoomLevel); markSeen(store, APPEARANCE_SEEN); }
          else if (!defaultAppearance(getAppearance().preferences, zoomLevel.current)) { moving.push(...appearanceEdits(appearanceToFile(getAppearance().preferences, zoomLevel.current), undefined)); moved.push('appearance'); }
          else markSeen(store, APPEARANCE_SEEN);
        }
        markSeen(store, name);
      } else if (!doc.exists) {
        const keys = { map: readKeymap(store), menu: readMenu(store), vim: readVimPreferences(store) };
        if (name === 'keys') {
          const isDefault = defaultKeys(keys);
          // The file was removed: back to the defaults.
          if (seen.keys) { if (!isDefault) applyKeysFromFile(store, { map: keyboardPreset(), menu: [], vim: { composer: false, editor: false } }); }
          else if (!isDefault) {
            // What the person set before files existed becomes their file, once.
            void updateConfigFile({ name, edits: keysEdits(keys, undefined) }).then(() => {
              markSeen(store, name);
              showToast({ id: 'config-moved-keys', tone: 'info', title: `Your shortcuts are now in ${fileLabel(name)}`, body: 'Edit it in any editor; Lazybox applies it as you save.', timeout: 6000 });
            }, () => { /* Shown when the person next saves. */ });
          }
          continue;
        }
        if (seen.settings) { if (!defaultNotifications(current)) save(notificationsFromFile({ notifications: {} })); }
        else if (!defaultNotifications(current)) { moving.push(...settingsEdits(current, undefined)); moved.push('notifications'); }
        const unsynced = readPreference(store, APPEARANCE_UNSYNCED) === '1';
        if (seen[APPEARANCE_SEEN] && !unsynced) applyAppearanceFile(store, {}, zoom, zoomLevel);
        else if (!defaultAppearance(getAppearance().preferences, zoomLevel.current)) { moving.push(...appearanceEdits(appearanceToFile(getAppearance().preferences, zoomLevel.current), undefined)); moved.push(unsynced ? 'unsynced' : 'appearance'); }
        else if (unsynced) moved.push('unsynced');
      }
    }
    if (moved.includes('unsynced') && !moving.some(edit => edit.path[0] === 'appearance')) writePreference(store, APPEARANCE_UNSYNCED, '');
    if (moving.length) {
      void updateConfigFile({ name: 'settings', edits: moving }).then(() => {
        markSeen(store, 'settings');
        if (moved.includes('appearance') || moved.includes('unsynced')) { markSeen(store, APPEARANCE_SEEN); writePreference(store, APPEARANCE_UNSYNCED, ''); }
        const first = moved.filter(part => part !== 'unsynced');
        if (!first.length) return;
        const what = first.length === 2 ? 'notification and appearance settings' : first[0] === 'appearance' ? 'appearance settings' : 'notification settings';
        showToast({ id: 'config-moved-settings', tone: 'info', title: `Your ${what} are now in ${fileLabel('settings')}`, body: 'Edit it in any editor; Lazybox applies it as you save.', timeout: 6000 });
      }, () => { applied.current.delete('settings'); /* Tried again on the next change; shown when the person next saves. */ });
    }
    if (JSON.stringify(problems) !== JSON.stringify(state.appProblems)) publish({ appProblems: problems });
    // Layouts saved before they were files become files, once.
    const legacy = parseSavedLayouts(readPreference(store, SAVED_LAYOUTS_KEY));
    if (legacy.length && !movedLayouts.current) {
      movedLayouts.current = true;
      void (async () => {
        const taken = files.docs.map(d => d.name);
        for (const layout of legacy) {
          const name = layoutFileName(layout.name, taken);
          taken.push(name);
          await updateConfigFile({ name, content: layoutYaml(layout) });
        }
        writePreference(store, SAVED_LAYOUTS_KEY, '[]');
        showToast({ id: 'config-moved-layouts', tone: 'info', title: `Your saved layouts are now files`, body: `${legacy.length} in the layouts folder beside your other Lazybox files.`, timeout: 6000 });
      })().catch(() => { movedLayouts.current = false; });
    }
    // A file with a new mistake is announced once, with its line.
    for (const doc of files.docs) {
      const count = errorCount(doc), key = count ? `${doc.revision}` : '';
      if (announced.current.get(doc.name) === key) continue;
      const fixed = announced.current.has(doc.name);
      announced.current.set(doc.name, key);
      // Fixed in the editor: the toast about it goes too.
      if (!count) { if (fixed) dismissToast(`config-problem-${doc.name}`); continue; }
      const tab = doc.name === 'keys' ? 'keyboard' : doc.name === 'settings' ? settingsTabOf(doc) : 'power';
      showToast({ id: `config-problem-${doc.name}`, tone: 'failed', title: `${fileLabel(doc.name)}: ${count === 1 ? 'a problem' : `${count} problems`}`, body: `${firstProblem(doc)}${doc.stale ? ' The last version that loaded stays in force.' : ''}`, action: { label: 'Show', run: () => runAction('settings.open', tab) } });
    }
  }, [primary, files, zoomKnown, zoom]);
}

/** The Settings section a settings.yaml problem belongs to. */
function settingsTabOf(doc: ConfigDoc): string {
  const issue = doc.issues.find(i => i.severity === 'error') || doc.issues[0];
  return issue?.path.startsWith('appearance') ? 'appearance' : 'notifications';
}

/** Puts settings.yaml's appearance section in force: what it leaves out is the default. */
function applyAppearanceFile(store: ViewStateStore | undefined, section: SettingsValue['appearance'] & object, zoom: ZoomHost | undefined, level: { current: number | undefined }) {
  const current = getAppearance().preferences;
  const next = appearanceFromFile(section, current);
  if (!sameAppearance(appearanceToFile(current), appearanceToFile(next.preferences))) saveAppearancePreferences(store, next.preferences, { fromFile: true });
  if (zoom && level.current !== undefined && level.current !== next.zoom) {
    level.current = next.zoom;
    void zoom.set(next.zoom).then(set => { level.current = set; }, () => undefined);
  }
}

function report(result: ConfigWriteResult, quiet: boolean): ConfigWriteResult {
  if (!quiet && !result.saved) showToast({ id: 'config-write', tone: 'failed', title: 'Not saved to your config file', body: result.message });
  return result;
}
