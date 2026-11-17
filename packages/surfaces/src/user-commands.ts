import { useSyncExternalStore } from 'react';
import type { ActionTarget } from './user-actions';

/**
 * The person's actions as commands any menu can list: the ]] menu, the "Run
 * an action…" picker, the workspace and conversation menus.
 *
 * The built-in registry (actions.ts) is a fixed list of ids, so the person's
 * actions, which change with their file, live in this small store beside it:
 * the command palette appends `userPaletteItems()`, PowerLayer contributes
 * `userMenuItems()` to the shared context menus ("Your actions") and binds the
 * ]] keys through `setLeaderExtensions`.
 */
export type UserCommand = {
  id: string;
  label: string;
  description: string;
  icon: string;
  /** The key after the ]] leader, or "". */
  key: string;
  /** palette, menu, conversation, workspace. */
  where: string[];
  /** One line about the steps. */
  summary: string;
  /** Runs in the given workspace and conversation, or the ones in view. */
  run(target?: ActionTarget): void;
};

let commands: UserCommand[] = [];
const listeners = new Set<() => void>();

export const userCommands = {
  list(): readonly UserCommand[] { return commands; },
  find(id: string): UserCommand | undefined { return commands.find(c => c.id === id); },
  set(next: UserCommand[]) { commands = next; for (const listener of listeners) listener(); },
  subscribe(listener: () => void): () => void { listeners.add(listener); return () => { listeners.delete(listener); }; },
};

/**
 * Context-menu items in the shape of the shared menu model (`MenuAction` in
 * context-menu-model.ts upstream: label, icon, shortcut, run), so a menu
 * contributor appends `...userMenuItems('workspace', target)`.
 */
export type UserMenuItem = { label: string; icon: string; shortcut?: string; run(): void };
export function userMenuItems(where: 'workspace' | 'conversation', target: ActionTarget): UserMenuItem[] {
  return commands.filter(c => c.where.includes(where)).map(c => ({ label: c.label, icon: c.icon, shortcut: c.key && c.where.includes('menu') ? `] ] ${c.key}` : undefined, run: () => c.run(target) }));
}

/**
 * Command-palette rows in the shape of the palette's items (CommandPalette.tsx
 * upstream: key, kind 'action', title, detail, extra, keys, run), for the
 * actions offered in the palette. The palette maps `icon` through its Icon.
 */
export type UserPaletteItem = { key: string; kind: 'action'; title: string; detail: string; extra: string; icon: string; keys: string[]; run(): void };
export function userPaletteItems(): UserPaletteItem[] {
  return commands.filter(c => c.where.includes('palette')).map(c => ({ key: `user-action:${c.id}`, kind: 'action', title: c.label, detail: 'Your action', extra: `${c.id} ${c.description} ${c.summary}`, icon: c.icon, keys: c.key && c.where.includes('menu') ? [`] ] ${c.key}`] : [], run: () => c.run() }));
}

/** The commands offered in one place (where), live as the file changes. */
export function useUserCommands(where?: string): readonly UserCommand[] {
  const all = useSyncExternalStore(userCommands.subscribe, userCommands.list, userCommands.list);
  return where ? all.filter(c => c.where.includes(where)) : all;
}
