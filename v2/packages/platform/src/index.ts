/** Navigation belongs to a particular native window, not to its execution. */
export type SurfaceTarget = { sessionId: string; commandId: string; commandsOpen: boolean; collectionId?: string; panelId?: string };
export type ChatTarget = { panelId: string; collectionId: string; sessionId: string };
export type WorkspaceHost = { reconnect?(): Promise<void>; openWorkspace?(collectionId: string): Promise<void>; chooseRepositoryFolder?(): Promise<string | null>; openSignIn?(url: string): Promise<void>; copySignInCode?(code: string): Promise<void>; openChat(target: ChatTarget): Promise<void>; dockChat(panelId: string): Promise<void> };
export type ViewStateStore = {
  key: string;
  /** Resolved authority scope shared by panels moved between native windows. */
  scope?: string;
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
};
export type SurfaceHost = {
  saveTarget(target: SurfaceTarget): Promise<void>;
  reopenAgent(): Promise<void>;
  reopenClosed(): Promise<void>;
  reconnect(): Promise<void>;
};
