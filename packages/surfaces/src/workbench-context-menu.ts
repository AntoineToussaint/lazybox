/**
 * Context menus of the Files tree and the editor tabs (scopes `file` and
 * `editor-tab`, see `ContextMenu.tsx`): the workbench actions those places
 * already have, plus copying a path and, on the desktop, Reveal in Finder.
 */
import { contextMenuPlatform, copyText, showMenuNotice } from './ContextMenu';
import { absolutePath, type MenuItem } from './context-menu-model';
import { canSplit, editorGroups, findGroup } from './editor-model';
import { isDirty, isMac, type Workbench } from './workbench';

const roots = new Map<string, string>();
/** The workspace folder on the Runner's machine, read once per workspace. */
async function workspaceRoot(workbench: Workbench, known = ''): Promise<string> {
  if (known) { roots.set(workbench.workspaceId, known); return known; }
  const cached = roots.get(workbench.workspaceId);
  if (cached) return cached;
  const result = await workbench.api.getCollection({ collectionId: workbench.workspaceId }, { timeoutMs: 8000 });
  const root = result.collection?.root || '';
  if (root) roots.set(workbench.workspaceId, root);
  return root;
}

/** Copy path, Copy relative path, and Reveal in Finder where the host can. */
export function pathItems(workbench: Workbench, path: string, root = ''): MenuItem[] {
  const reveal = contextMenuPlatform().revealInFinder;
  return [
    { label: 'Copy path', icon: 'copy', run: async () => { const base = await workspaceRoot(workbench, root).catch(() => ''); await copyText(base ? absolutePath(base, path) : path, base ? 'Copied path' : 'Copied relative path'); } },
    { label: 'Copy relative path', icon: 'copy', run: () => copyText(path, 'Copied relative path') },
    reveal && { label: isMac ? 'Reveal in Finder' : 'Show in file manager', icon: 'reveal', run: async () => {
      const base = await workspaceRoot(workbench, root);
      if (!base) { showMenuNotice('The workspace folder is unknown'); return; }
      await reveal(absolutePath(base, path));
    } },
  ];
}

type Row = { path: string; directory: boolean };
/** A row of the Files tree. */
export function fileItems(workbench: Workbench, row: Row, root: string): MenuItem[] {
  if (row.directory) {
    const open = workbench.getState().expanded.includes(row.path);
    return [
      { label: open ? 'Collapse folder' : 'Expand folder', icon: open ? 'collapse' : 'chevronRight', run: () => workbench.toggle(row.path) },
      { label: 'Collapse all folders', icon: 'collapse', disabled: !workbench.getState().expanded.length, run: () => workbench.collapseAll() },
      { label: 'Refresh files', icon: 'refresh', run: () => workbench.refreshFolders() },
      'separator',
      ...pathItems(workbench, row.path, root),
    ];
  }
  return [
    { label: 'Open', icon: 'file', shortcut: 'Enter', run: () => workbench.open(row.path, { focus: true }) },
    { label: 'Open to the side', icon: 'split-right', shortcut: 'Mod+Enter', run: () => workbench.open(row.path, { side: 'row', focus: true }) },
    { label: 'Quick look', icon: 'eye', shortcut: 'Alt+Enter', run: () => workbench.host.quickLook(row.path) },
    'separator',
    ...pathItems(workbench, row.path, root),
  ];
}

/** An editor tab: closing in the usual ways, splitting with it, and where it is. */
export function editorTabItems(workbench: Workbench, group: string, path: string): MenuItem[] {
  const state = workbench.getState(), value = findGroup(state.layout, group);
  if (!value) return [];
  const index = value.tabs.indexOf(path), right = value.tabs.slice(index + 1);
  const shownElsewhere = (tab: string) => editorGroups(state.layout.root).some(other => other.id !== group && other.tabs.includes(tab));
  const closable = (tab: string) => !isDirty(state.docs[tab]) || shownElsewhere(tab);
  const split = (direction: 'row' | 'column') => { workbench.activate(group, path); workbench.split(direction, group); };
  return [
    { label: 'Close', icon: 'close', shortcut: 'Alt+W', run: () => { workbench.close(group, path); } },
    { label: 'Close others', disabled: value.tabs.length < 2, run: () => { workbench.activate(group, path); workbench.closeTabs(group, 'others'); } },
    { label: 'Close to the right', disabled: !right.length, run: () => { for (const tab of right) if (closable(tab)) workbench.close(group, tab); } },
    { label: 'Close saved', run: () => workbench.closeTabs(group, 'saved') },
    { label: 'Close all', run: () => workbench.closeTabs(group, 'all') },
    'separator',
    value.preview === path && { label: 'Keep open', icon: 'check', run: () => workbench.pin(group, path) },
    { label: 'Split right', icon: 'split-right', shortcut: 'Mod+\\', disabled: !canSplit(state.layout) && 'Four editor groups are open', run: () => split('row') },
    { label: 'Split down', icon: 'split-down', shortcut: 'Mod+Alt+\\', disabled: !canSplit(state.layout) && 'Four editor groups are open', run: () => split('column') },
    'separator',
    { label: 'Reveal in Files', icon: 'target', run: () => workbench.reveal(path) },
    { label: 'Quick look', icon: 'eye', run: () => workbench.host.quickLook(path) },
    ...pathItems(workbench, path),
  ];
}
