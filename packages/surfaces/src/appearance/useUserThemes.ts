import { useEffect, useRef, useState } from 'react';
import type { ViewStateStore } from '@lazybox/platform';
import { useRunner } from '../RunnerContext';
import { getAppearance } from './apply';
import { parseThemeDocument, type ThemeFileResult } from './user-themes';
import { saveAppearancePreferences, useAppearance } from './useAppearance';
import type { ThemeDefinition } from './themes';

export type UserThemes = { directory: string; files: ThemeFileResult[]; loaded: boolean; error: string; reload(): void };

/** One file as the Runner listed it, checked against the token catalogue. */
export function checkThemeFile(file: { name: string; json: string; error: string }): ThemeFileResult {
  if (file.error) return { file: file.name, errors: [file.error], warnings: [] };
  try { return parseThemeDocument(file.name, JSON.parse(file.json || 'null')); }
  catch { return { file: file.name, errors: ['The Runner sent a document that is not JSON.'], warnings: [] }; }
}

/**
 * The Runner's themes folder, reread every `intervalMs` while `active`, so a
 * file saved in an editor shows up (or shows its errors) within a moment.
 * A Runner that predates themes answers Unimplemented: that reads as none.
 */
export function useUserThemes(active: boolean, intervalMs = 2000): UserThemes {
  const { api } = useRunner();
  const [state, setState] = useState<Omit<UserThemes, 'reload'>>({ directory: '', files: [], loaded: false, error: '' });
  const [tick, setTick] = useState(0);
  const revision = useRef('');
  useEffect(() => {
    if (!active) return;
    const control = new AbortController();
    let busy = false;
    const read = async () => {
      if (busy) return; busy = true;
      try {
        const folder = await api.listThemes({}, { signal: control.signal, timeoutMs: 5000 });
        if (control.signal.aborted) return;
        if (folder.revision && folder.revision === revision.current) return;
        revision.current = folder.revision;
        setState({ directory: folder.directory, files: folder.files.map(checkThemeFile), loaded: true, error: '' });
      } catch (error) {
        if (!control.signal.aborted) setState(current => ({ ...current, loaded: true, error: /unimplemented/i.test(String(error)) ? 'This Runner does not serve user themes yet. Restart it after updating Lazybox.' : 'Could not read the themes folder from the Runner.' }));
      } finally { busy = false; }
    };
    void read();
    const timer = setInterval(() => void read(), intervalMs);
    return () => { control.abort(); clearInterval(timer); };
  }, [api, active, intervalMs, tick]);
  return { ...state, reload: () => { revision.current = ''; setTick(value => value + 1); } };
}

const same = (a?: ThemeDefinition, b?: ThemeDefinition) => JSON.stringify(a && { ...a, source: undefined }) === JSON.stringify(b && { ...b, source: undefined });

/** Writes the current definition of each user theme in use into the preferences, so every window repaints. */
export function adoptUserThemes(store: ViewStateStore | undefined, files: ThemeFileResult[]): void {
  const preferences = getAppearance().preferences;
  const used = [preferences.darkTheme, preferences.lightTheme].filter(id => id.startsWith('user:'));
  if (!used.length) return;
  const snapshot = { ...(preferences.userThemes || {}) };
  let changed = false;
  for (const id of used) {
    const theme = files.find(f => f.theme?.id === id)?.theme;
    if (theme && !same(theme, snapshot[id])) { snapshot[id] = theme; changed = true; }
  }
  for (const id of Object.keys(snapshot)) if (!used.includes(id)) { delete snapshot[id]; changed = true; }
  if (changed) saveAppearancePreferences(store, { ...preferences, userThemes: snapshot });
}

/**
 * Hot reload for the user themes in use: while one is selected, the folder is
 * reread and an edited file repaints every window. A file with errors keeps
 * the last good version on screen; Settings → Appearance says what is wrong.
 */
export function UserThemeSync({ store }: { store?: ViewStateStore }) {
  const { preferences } = useAppearance();
  const inUse = preferences.darkTheme.startsWith('user:') || preferences.lightTheme.startsWith('user:');
  const themes = useUserThemes(inUse, 3000);
  useEffect(() => { if (themes.loaded) adoptUserThemes(store, themes.files); }, [themes.files, themes.loaded, store]);
  return null;
}
