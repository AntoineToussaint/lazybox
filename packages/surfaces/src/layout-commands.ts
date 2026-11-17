/**
 * The layout's commands as plain functions, for anything that lists or runs
 * actions (a command palette, context menus, the native menu bar): split,
 * close, maximize, cycle groups, float, pop out, minimize, apply a preset,
 * reset, show any panel. The window's workspace provides them while it is
 * mounted; each call reads the live layout, so a command list can be built
 * once and run later.
 */
export type LayoutCommand = {
  /** Stable: `layout.split`, `layout.preset:review`, `panel.show:terminal`. */
  id: string;
  label: string;
  /** The keymap's shortcut, as stored ("Meta+k Meta+m"), when it has one. */
  keys?: string;
  group: 'Layout' | 'Panels' | 'Layouts';
  enabled(): boolean;
  run(): void;
};

let provider: (() => LayoutCommand[]) | undefined;
/** Called by the workspace view; returns the withdrawal. */
export function provideLayoutCommands(get: () => LayoutCommand[]): () => void {
  provider = get;
  return () => { if (provider === get) provider = undefined; };
}
/** Every layout command this window offers right now (none before a workspace is shown). */
export function layoutCommands(): LayoutCommand[] { return provider?.() ?? []; }
/** Runs one by id; false when it does not exist or is not available now. */
export function runLayoutCommand(id: string): boolean {
  const command = layoutCommands().find(value => value.id === id);
  if (!command || !command.enabled()) return false;
  command.run();
  return true;
}
