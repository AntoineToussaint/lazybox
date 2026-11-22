import { useMemo, useRef } from 'react';
import type { Collection } from '@lazybox/protocol';
import { useActions } from './actions';
import type { CommandNavigator } from './CommandCenter';
import { showMissionsPage, useNavigationRequests } from './navigate-requests';
import { openArtifact } from './artifact-events';
import type { PanelModule } from './PanelRegistry';
import type { Workbench } from './workbench';
import type { ViewTarget } from './workspace-model';
import type { RunnerTarget } from './runner-directory';

type Commands = {
  root: string;
  /** The main window (not a window fixed to one workspace): it takes other windows' requests and has Settings. */
  main: boolean;
  canShowMissions: boolean;
  missionOpen: boolean;
  collections: Collection[];
  selection: ViewTarget;
  workspaceId: string;
  workbench?: Workbench;
  registry: Readonly<Record<string, PanelModule>>;
  /** Panels in windows of their own, each of which the palette can put back. */
  windows?: { id: string; title: string; icon: string; putBack(): void }[];
  canStartConversation: boolean;
  terminalReady: boolean;
  canOpenWorkspaceWindow: boolean;
  showSession(target: ViewTarget): void;
  selectWorkspace(target: ViewTarget): void;
  showPanel(kind: string): void;
  showRun(target: ViewTarget): void;
  openSettings(tab: string): void;
  closeSettings(): void;
  newConversation(): void;
  openWorkspaceWindow(): void;
  toggleEditing(): void;
  toggleConfiguring(): void;
};

/** Panels the palette does not offer: the page is made of them. */
const pageOnly = new Set(['collections', 'chat']);

/**
 * The Workspace's actions in the registry (what its header buttons and page
 * menu do), and the navigator the command palette and navigation keys use.
 */
export function useWorkspaceCommands(commands: Commands): CommandNavigator {
  const latest = useRef(commands); latest.current = commands;
  const c = () => latest.current;
  useActions({
    'settings.open': commands.main && { run: tab => c().openSettings(typeof tab === 'string' ? tab : 'subscriptions') },
    'conversation.new': { run: () => c().newConversation(), enabled: () => c().canStartConversation },
    'changes.show': { run: () => c().showPanel('changes'), enabled: () => !!c().workspaceId },
    'missions.open': { run: () => c().showPanel('missions'), enabled: () => c().canShowMissions },
    'missions.page': { run: () => showMissionsPage(), enabled: () => c().canShowMissions },
    'pulls.open': { run: () => c().showPanel('pulls'), enabled: () => !c().missionOpen && !!c().registry.pulls },
    'terminal.open':{ run: () => c().showPanel('terminal'), enabled: () => c().terminalReady },
    'workspace.window': { run: () => c().openWorkspaceWindow(), enabled: () => c().canOpenWorkspaceWindow },
    'page.customize': { run: () => c().toggleEditing(), enabled: () => !c().missionOpen },
    'layout.configure': { run: () => c().toggleConfiguring(), enabled: () => !c().missionOpen },
  });
  // A detached chat window asks this window to show a conversation, or an artifact (its Open in main window).
  useNavigationRequests(commands.root, commands.main, target => {
    if (target.commandId) { latest.current.closeSettings(); latest.current.showRun(target); return; }
    if (target.artifactId === undefined) { goTo(target); return; }
    const request = { collectionId: target.collectionId, artifactId: target.artifactId };
    latest.current.closeSettings();
    if (target.collectionId === latest.current.workspaceId) { openArtifact(request); return; }
    // Asked for first, so the workspace shows its Artifacts panel once it is switched to.
    openArtifact(request, { whenShown: true });
    goTo({ collectionId: target.collectionId, sessionId: '', commandId: '' });
  });
  const goTo = (target: ViewTarget) => {
    const { showSession, selectWorkspace, closeSettings, workspaceId } = latest.current;
    closeSettings();
    if (target.sessionId) showSession(target);
    // A workspace of another Runner may share this one's id: naming its Runner always goes there.
    else if (target.collectionId !== workspaceId || (target as RunnerTarget).runner) selectWorkspace(target);
  };
  // The palette's list changes when a window comes or goes, not on every render.
  const windowsKey = (commands.windows || []).map(item => `${item.id}:${item.title}`).join('|');
  const panels = useMemo(() => Object.entries(commands.registry).filter(([kind]) => !pageOnly.has(kind)).map(([kind, module]) => ({ kind, title: module.title, icon: module.icon || 'file' })), [commands.registry]);
  return useMemo(() => ({
    collections: commands.collections, current: commands.selection, workbench: commands.workbench, panels: commands.missionOpen ? [] : panels, windows: commands.windows,
    goTo, showPanel: (kind: string) => latest.current.showPanel(kind),
    openSettings: commands.main ? (tab: string) => latest.current.openSettings(tab) : undefined,
  }), [commands.collections, commands.selection, commands.workbench, panels, commands.main, commands.missionOpen, windowsKey]);
}
