import { useEffect } from 'react';
import type { ViewStateStore } from '@lazybox/platform';
import { nativeMenuItems, runMenuAction, type NativeMenuItem } from './actions';
import { useKeymap } from './keyboard';

export { runMenuAction, type NativeMenuItem };

/**
 * Keeps the desktop app's menu bar in step with the action registry and the
 * person's keys: it is rebuilt whenever the keymap changes, so the menu shows
 * the shortcuts that actually work.
 */
export function useNativeActionMenu(viewState: ViewStateStore | undefined, publish: ((items: NativeMenuItem[]) => Promise<void>) | undefined) {
  const keymap = useKeymap(viewState);
  useEffect(() => {
    if (publish) void publish(nativeMenuItems(keymap)).catch(() => { /* the menu keeps its previous items */ });
  }, [keymap, publish]);
}
