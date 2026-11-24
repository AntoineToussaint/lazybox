/**
 * Conversation tabs, the parts that are not the page's arrangement (that is
 * `workspace-model.ts`, "Conversations"): what a click on a conversation row
 * asks for, the tabs closed recently (⇧⌘T brings the last one back), and the
 * status dot a conversation's tab shows. Pure, so the rules are tested alone.
 */
import type { Session } from '@lazybox/protocol';
import { activityAt, isListed, type AttentionState } from './attention';
import { isArchivedAway } from './agents-model';
import type { OpenMode, ViewTarget } from './workspace-model';

/**
 * "Open a workspace's conversations as tabs" (on unless the person turned it
 * off, Settings → Chat): every conversation opened is a kept tab and a
 * workspace's open conversations are there as tabs. Off, a single click
 * shows a conversation in the preview tab, which the next one replaces.
 */
export const TABS_PREFERENCE = 'workspace.conversation-tabs';
export const tabsSettingOn = (value: string | null | undefined) => value !== 'off';
/** At most this many conversation tabs are opened for a workspace by themselves; the rest are a click away in the sidebar. */
export const WORKSPACE_TAB_CAP = 6;
/** How an open asks to show a conversation, given the setting: with tabs on, what would be a preview is a kept tab. */
export const openModeFor = (mode: OpenMode, tabs: boolean): OpenMode => tabs && mode === 'preview' ? 'tab' : mode;

/**
 * A workspace's open conversations, most recent first: its work (not Ask
 * Lazybox's), not ended, not archived away, the latest execution of each.
 */
export function workspaceConversations(sessions: readonly Session[], workspaceId: string, dismissed: ReadonlySet<string> = new Set()): ViewTarget[] {
  return sessions
    .filter(session => session.collectionId === workspaceId && isListed(session) && !session.closed && !session.successorExecutionId && !isArchivedAway(session) && !dismissed.has(session.id))
    .sort((a, b) => activityAt(b) - activityAt(a))
    .map(session => ({ collectionId: workspaceId, sessionId: session.id, commandId: '' }));
}
/** The conversations whose tab the person closed (kept per workspace), with one more, or one fewer when it is opened again. */
export function dismissedWith(text: string | null | undefined, sessionId: string, closed: boolean): string[] {
  let list: string[] = [];
  try { const value: unknown = JSON.parse(text || '[]'); if (Array.isArray(value)) list = value.filter((item): item is string => typeof item === 'string'); } catch { /* none */ }
  const rest = list.filter(id => id !== sessionId);
  return (closed ? [...rest, sessionId] : rest).slice(-50);
}

/**
 * What a pointer gesture on a conversation row opens: a single click shows
 * it in the preview tab, a double-click keeps it in a tab, ⌘-click (Ctrl-click
 * off a Mac) and a middle-click open it in a tab behind the one in front.
 * Alt-click is Peek's (peek-model.ts); a right-click is the menu's.
 */
export function rowGesture(event: { button: number; detail?: number; metaKey: boolean; ctrlKey: boolean; altKey: boolean; type: string }, mac: boolean): OpenMode | undefined {
  if (event.altKey) return undefined;
  if (event.type === 'auxclick') return event.button === 1 ? 'background' : undefined;
  if (event.button !== 0) return undefined;
  if (event.type === 'dblclick') return 'tab';
  return (mac ? event.metaKey : event.ctrlKey) ? 'background' : 'preview';
}

/** A conversation tab closed: where it was, so ⇧⌘T puts it back there. */
export type ClosedTab = { page: string; target: ViewTarget; group: string; index: number };
/** At most this many closed tabs are remembered. */
export const CLOSED_LIMIT = 20;
/** The closed tabs with one more, the latest last; the same conversation is remembered once. */
export function rememberClosed(stack: readonly ClosedTab[], closed: ClosedTab): ClosedTab[] {
  const same = (entry: ClosedTab) => entry.page === closed.page && entry.target.sessionId === closed.target.sessionId && entry.target.collectionId === closed.target.collectionId;
  return [...stack.filter(entry => !same(entry)), closed].slice(-CLOSED_LIMIT);
}
/** The latest closed tab of a page that is not open again, and the stack without it (and without those that are open again). */
export function takeClosed(stack: readonly ClosedTab[], page: string, isOpen: (target: ViewTarget) => boolean): { tab?: ClosedTab; rest: ClosedTab[] } {
  const rest = stack.filter(entry => entry.page !== page || !isOpen(entry.target));
  for (let index = rest.length - 1; index >= 0; index--) {
    if (rest[index].page === page) return { tab: rest[index], rest: [...rest.slice(0, index), ...rest.slice(index + 1)] };
  }
  return { rest };
}

/** The dot before a conversation tab's title: working, needs you, failed, done (a turn ended), or idle. */
export type TabStatus = { state: 'working' | 'needs-you' | 'failed' | 'done' | 'idle'; label: string };
export function tabStatus(state: AttentionState, reason: string): TabStatus {
  switch (state) {
    case 'working': return { state: 'working', label: reason || 'Working' };
    case 'needs-input': return { state: 'needs-you', label: reason ? `Needs you: ${reason}` : 'Needs you' };
    case 'failed': return { state: 'failed', label: reason || 'Failed' };
    case 'review': return { state: 'done', label: reason || 'Turn done' };
    default: return { state: 'idle', label: reason || 'Done' };
  }
}
