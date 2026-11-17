import type { ViewStateStore } from '@lazybox/platform';
import { useAction } from '../actions';
import { getAppearance } from './apply';
import { saveAppearancePreferences, useAppearanceSync } from './useAppearance';

/**
 * Mounted once per window by the host: the window follows the stored
 * appearance live, and the palette's "Switch between light and dark" works.
 */
export function AppearanceSync({ store }: { store?: ViewStateStore }) {
  useAppearanceSync(store);
  useAction('appearance.toggle', () => {
    const { preferences, theme } = getAppearance();
    saveAppearancePreferences(store, { ...preferences, mode: theme.scheme === 'dark' ? 'light' : 'dark' });
  });
  return null;
}
