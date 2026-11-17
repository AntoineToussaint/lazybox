import { useMemo } from 'react';
import type { ViewStateStore } from '@lazybox/platform';
import { CommandCenter, type CommandNavigator } from './CommandCenter';
import { requestNavigation } from './navigate-requests';
import { useRunner } from './RunnerContext';
import { useWorkspaceCatalog } from './useWorkspaceCatalog';

/**
 * The command palette and navigation keys of a window that shows one chat or
 * one command (a detached window). Going to a conversation or workspace asks
 * the main window to show it, and brings that window forward.
 */
export function DetachedCommands({ viewState, collectionId, sessionId, onFocusMain }: { viewState?: ViewStateStore; collectionId: string; sessionId: string; onFocusMain?(): void }) {
  const { api, connection } = useRunner();
  const { collections } = useWorkspaceCatalog(api);
  const root = viewState?.root || connection.baseUrl;
  const navigator = useMemo<CommandNavigator>(() => ({
    collections, detached: true, current: { collectionId, sessionId, commandId: '' },
    goTo: target => { requestNavigation(root, target); onFocusMain?.(); },
  }), [collections, collectionId, sessionId, root, onFocusMain]);
  return <CommandCenter viewState={viewState} navigator={navigator}/>;
}
