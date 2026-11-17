import { useCallback, useEffect, useSyncExternalStore } from 'react';
import type { ViewStateStore } from '@lazybox/platform';
import { PREFERENCE_EVENT, readPreference, writePreference } from '../view-state';
import { APPEARANCE_CACHE_KEY, applyAppearance, getAppearance, readCachedPreferences, subscribeAppearance, type AppearanceState } from './apply';
import { parsePreferences, serializePreferences, type AppearancePreferences } from './preferences';
import { appearanceToFile, sameAppearance } from '../config-files';
import { writeConfigSoon } from '../config-writer';
import './appearance.css';

const NAME = 'appearance:v1';

/** The stored preferences: the host's store when there is one, else the last ones applied. */
export function readAppearancePreferences(store?: ViewStateStore): AppearancePreferences {
  try {
    const stored = store ? readPreference(store, NAME) : null;
    return stored ? parsePreferences(stored) : readCachedPreferences();
  } catch { return readCachedPreferences(); }
}
/**
 * Applies and keeps the preferences on this device (the fast copy every window
 * starts from), and writes what changed to settings.yaml's appearance section
 * (POWER.md, "Config files"); `fromFile` is the file being applied, which is
 * not written back.
 */
export function saveAppearancePreferences(store: ViewStateStore | undefined, preferences: AppearancePreferences, { fromFile = false } = {}): void {
  const before = getAppearance().preferences;
  applyAppearance(preferences);
  if (store) writePreference(store, NAME, serializePreferences(preferences));
  if (fromFile || sameAppearance(appearanceToFile(before), appearanceToFile(preferences))) return;
  // Until the file says it, this device's copy is the newer one: a window
  // closed before the write landed writes it at the next start instead of
  // taking the file's older values (useConfigFiles.ts).
  if (store) writePreference(store, APPEARANCE_UNSYNCED, '1');
  writeConfigSoon('appearance', { file: 'settings', appearance: preferences });
}
/** Set while a change made here has not reached settings.yaml yet. */
export const APPEARANCE_UNSYNCED = 'appearance:unsynced:v1';

/** The window's appearance, live. */
export function useAppearance(): AppearanceState {
  return useSyncExternalStore(subscribeAppearance, getAppearance, getAppearance);
}

/**
 * Keeps this window's appearance in step with the stored preferences:
 * applied at mount and whenever they change here or in another window.
 * Every host mounts it once per window.
 */
export function useAppearanceSync(store?: ViewStateStore): void {
  useEffect(() => {
    const reload = () => {
      const next = readAppearancePreferences(store);
      if (serializePreferences(next) !== serializePreferences(getAppearance().preferences)) applyAppearance(next);
    };
    reload();
    const onStorage = (event: StorageEvent) => { if (!event.key || event.key === APPEARANCE_CACHE_KEY || event.key.endsWith(`:${NAME}`)) reload(); };
    window.addEventListener(PREFERENCE_EVENT, reload);
    window.addEventListener('storage', onStorage);
    return () => { window.removeEventListener(PREFERENCE_EVENT, reload); window.removeEventListener('storage', onStorage); };
  }, [store]);
}

/** Read and change the preferences from Settings; every change applies at once. */
export function useAppearancePreferences(store?: ViewStateStore): [AppearancePreferences, (change: (current: AppearancePreferences) => AppearancePreferences) => void] {
  const { preferences } = useAppearance();
  const update = useCallback((change: (current: AppearancePreferences) => AppearancePreferences) => {
    saveAppearancePreferences(store, change(getAppearance().preferences));
  }, [store]);
  return [preferences, update];
}
