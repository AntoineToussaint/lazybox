import { useEffect, useRef, useSyncExternalStore } from 'react';
import type { ViewStateStore } from '@lazybox/platform';
import { PREFERENCE_EVENT, readPreference, writePreference } from './view-state';
import { useRunner } from './RunnerContext';
import { useSeen } from './useAttention';
import { conversationKey, deriveAttention, seenAt } from './attention';
import { dismissedWith, openModeFor, rememberClosed, tabStatus, tabsSettingOn, TABS_PREFERENCE, takeClosed, WORKSPACE_TAB_CAP, workspaceConversations, type ClosedTab, type TabStatus } from './conversation-tabs';
import { showToast } from './toasts';
import {
  closeTab, forgetConversation, groupOf, isChatTab, keepTab, moveToNewPage, oldestPreview, openConversation, openRelated, pageShowing, PANEL_LIMIT, resolveTarget, showWorkspaceTabs, shownPanels, tabFor, tabsToClose,
  type OpenMode, type OpenOptions, type Opened, type Panel, type ViewTarget, type WorkspaceLayout, type WorkspacePage,
} from './workspace-model';

/** What the tabs need from the page that holds them (Workspace). */
export type TabHost = {
  /** The page as it is now, and the workspace's pages. */
  livePage(): WorkspacePage;
  layout(): WorkspaceLayout;
  changePage(next: WorkspacePage): boolean;
  commit(next: WorkspaceLayout): boolean;
  /** Closes a panel the way its × does. */
  closePanel(panel: Panel): void;
  /** A tab opened in front: `focus` moves the keyboard there, `reveal` only shows it (a maximized page keeps the zoom on it). */
  shown(panelID: string, how: 'focus' | 'reveal'): void;
  /** The conversation's title, as its tab shows it. */
  title(target: ViewTarget): string;
  /** Said to a screen reader, and shown by the dock's focus ring. */
  announce(text: string): void;
  error(text: string): void;
  /** Which workspace the closed tabs belong to. */
  scope: string;
  /** "Open a workspace's conversations as tabs" (useTabsSetting). */
  tabs: boolean;
  /** Where the conversations whose tab the person closed are kept, per workspace. */
  store?: ViewStateStore;
};

/** "Open a workspace's conversations as tabs", on unless turned off (Settings → Chat), in every window. */
export function useTabsSetting(store?: ViewStateStore): [boolean, (on: boolean) => void] {
  const read = () => { try { return tabsSettingOn(readPreference(store, TABS_PREFERENCE)); } catch { return true; } };
  const on = useSyncExternalStore(listener => { window.addEventListener(PREFERENCE_EVENT, listener); return () => window.removeEventListener(PREFERENCE_EVENT, listener); }, read);
  return [on, value => { try { writePreference(store, TABS_PREFERENCE, value ? 'on' : 'off'); } catch { /* holds for this window */ } }];
}

const newID = () => crypto.randomUUID();

/**
 * Conversation tabs, the window's side of them (the rules are in
 * `workspace-model.ts`): opening a conversation in the preview tab, a tab in
 * front or one behind; a related conversation beside its parent; the tabs
 * closed lately (⇧⌘T); a preview tab kept open once typed in; and each tab's
 * status dot. A full page says so and offers to close the oldest preview tab.
 */
export function useConversationTabs(host: TabHost) {
  const latest = useRef(host); latest.current = host;
  const closed = useRef<ClosedTab[]>([]);
  /** Conversations whose tab the person closed stay closed when the workspace loads again, until opened. */
  const dismissedKey = () => `${host.store?.scope || host.store?.key}:workspace:${latest.current.scope || 'home'}:closed-tabs`;
  const dismiss = (sessionId: string, closing: boolean) => {
    const store = latest.current.store;
    if (!store || !sessionId) return;
    try {
      const text = store.getItem(dismissedKey()), next = dismissedWith(text, sessionId, closing);
      if (JSON.stringify(next) !== (text || '[]')) store.setItem(dismissedKey(), JSON.stringify(next));
    } catch { /* a convenience */ }
  };
  const dismissed = (): Set<string> => { try { return new Set(dismissedWith(latest.current.store?.getItem(dismissedKey()), '', false)); } catch { return new Set(); } };
  const { info } = useRunner();
  const { seen } = useSeen();

  /** A page is full (`host`, the one shown unless said): say so, and offer to close its oldest preview tab to make room. */
  const offerRoom = (target: ViewTarget, retry: () => void, host = latest.current.livePage()) => {
    const h = latest.current, preview = oldestPreview(host);
    const name = h.title(target), previewName = preview ? h.title(resolveTarget(preview, host.selection)) : '';
    // The preview closes where it is: through its × on the page shown, else on its own page.
    const close = (panel: Panel) => {
      const now = latest.current;
      if (host.id === now.livePage().id) { now.closePanel(panel); return; }
      const layout = now.layout();
      now.commit({ ...layout, pages: layout.pages.map(page => page.id === host.id ? closeTab(page, panel.id) : page) });
    };
    showToast({
      id: 'conversation-tabs:full', tone: 'info', timeout: 10000,
      title: `This page already has ${PANEL_LIMIT} panels`,
      body: preview ? `Close the preview tab “${previewName}” to open “${name}”, or close another tab.` : `Close a tab to open “${name}”.`,
      ...(preview ? { button: { label: 'Close preview and open', run: () => { close(preview); retry(); } } } : {}),
    });
  };
  const apply = (result: Opened, target: ViewTarget, front: boolean, retry: () => void, said?: string): string => {
    const h = latest.current;
    if (result.full) { offerRoom(target, retry); return ''; }
    if (!h.changePage(result.page)) return '';
    if (front) h.shown(result.id, 'focus');
    else h.announce(said || (result.added ? `Opened “${h.title(target)}” in a tab behind this one` : `“${h.title(target)}” is already open in a tab`));
    return result.id;
  };

  /** A conversation as a tab; the panel showing it ('' when nothing opened). A preview open only reveals it: the click's list keeps the keyboard. */
  const open = (target: ViewTarget, asked: OpenMode, place: Omit<OpenOptions, 'mode'> = {}): string => {
    const mode = openModeFor(asked, latest.current.tabs);
    dismiss(target.sessionId, false);
    const h = latest.current, before = tabFor(h.livePage(), target), result = openConversation(h.livePage(), target, { mode, ...place }, newID);
    // A single click with tabs on: the tab shows it in view, and the list keeps the keyboard.
    if (asked === 'preview' && mode === 'tab' && !result.full) { if (!h.changePage(result.page)) return ''; h.shown(result.id, before ? 'focus' : 'reveal'); return result.id; }
    if (mode !== 'preview') return apply(result, target, mode === 'tab', () => open(target, asked, place));
    if (result.full) { offerRoom(target, () => open(target, asked, place)); return ''; }
    if (!h.changePage(result.page)) return '';
    // A conversation already in a tab of its own (a review beside the work) comes forward with the focus.
    h.shown(result.id, before && !before.preview ? 'focus' : 'reveal');
    return result.id;
  };
  /** A conversation opened from another (a review, an orchestrator's agent): a tab right after its parent's. */
  const related = (parent: ViewTarget, target: ViewTarget, { background = true }: { background?: boolean } = {}) => {
    dismiss(target.sessionId, false);
    const h = latest.current, layout = h.layout(), live = h.livePage();
    // On the page that shows the parent (this one first), so a review joins the conversation it reviews.
    const host = (parent.sessionId && pageShowing(layout, parent.sessionId)) || live;
    const result = openRelated(host, parent, target, { background }, newID), retry = () => related(parent, target, { background });
    if (host.id === live.id) { apply(result, target, !background, retry, result.added ? `Opened “${h.title(target)}” in a tab next to “${h.title(parent)}”` : undefined); return; }
    if (result.full) { offerRoom(target, retry, host); return; }
    if (h.commit({ ...layout, selectedPage: background ? layout.selectedPage : host.id, pages: layout.pages.map(page => page.id === host.id ? result.page : page) }) && !background) h.shown(result.id, 'focus');
  };
  /** A conversation on a page of its own. */
  const newPage = (target: ViewTarget, title: string) => {
    const h = latest.current, moved = moveToNewPage(h.layout(), h.livePage().id, target, title, newID);
    if ('error' in moved) { h.error(moved.error); return; }
    if (!h.commit(moved.layout)) return;
    const page = moved.layout.pages.find(value => value.id === moved.page), tab = page && tabFor(page, target);
    if (tab) h.shown(tab.id, 'focus');
  };
  /** Close the other tabs of a tab's group, or those to its right; it stays in front. */
  const closeTabs = (panel: Panel, which: 'others' | 'right') => {
    const h = latest.current;
    for (const other of tabsToClose(h.livePage(), panel.id, which)) h.closePanel(other);
  };
  const tabOf = (target: ViewTarget) => {
    const page = latest.current.livePage(), tab = tabFor(page, target);
    return tab ? { panel: tab, others: tabsToClose(page, tab.id, 'others').length, right: tabsToClose(page, tab.id, 'right').length } : undefined;
  };
  /** A chat tab about to close: remembered, so ⇧⌘T puts it back where it was. */
  const remember = (page: WorkspacePage, panel: Panel) => {
    const group = groupOf(page, panel.id);
    if (!isChatTab(panel) || !group) return;
    dismiss(resolveTarget(panel, page.selection).sessionId, true);
    closed.current = rememberClosed(closed.current, { page: `${latest.current.scope}:${page.id}`, target: { ...resolveTarget(panel, page.selection), commandId: '' }, group: group.id, index: group.panels.findIndex(value => value.id === panel.id) });
  };
  /** ⇧⌘T: the last conversation tab closed on this page, back where it was. */
  const reopen = () => {
    const h = latest.current, page = h.livePage();
    const { tab, rest } = takeClosed(closed.current, `${h.scope}:${page.id}`, target => !!tabFor(page, target));
    closed.current = rest;
    if (!tab) { h.announce('No closed conversation tab to reopen'); return; }
    open(tab.target, 'tab', { group: tab.group, index: tab.index });
  };
  /** Whether ⇧⌘T has a tab to bring back on this page (the same test as reopen). */
  const canReopen = () => { const h = latest.current, page = h.livePage(); return !!takeClosed(closed.current, `${h.scope}:${page.id}`, target => !!tabFor(page, target)).tab; };
  /** A conversation that is gone (deleted): its tabs leave every page, and it is not offered again. */
  const forget = (sessionId: string) => {
    if (!sessionId) return;
    closed.current = closed.current.filter(entry => entry.target.sessionId !== sessionId);
    dismiss(sessionId, false);
    const h = latest.current, layout = h.layout(), next = forgetConversation(layout, sessionId);
    if (next !== layout) h.commit(next);
  };
  /** A tab's status dot: working, needs you, failed, done (unread), or idle. */
  const statusOf = (panel: Panel, selection: ViewTarget): TabStatus | undefined => {
    if (!isChatTab(panel)) return undefined;
    const id = resolveTarget(panel, selection).sessionId, session = id ? info?.sessions.find(value => value.id === id) : undefined;
    if (!session) return undefined;
    const attention = deriveAttention(session, seenAt(seen, conversationKey(session)));
    return tabStatus(attention.state, attention.reason);
  };

  // A workspace's open conversations are there as tabs when its page loads (most recent first, up
  // to WORKSPACE_TAB_CAP), once per page in this window; closed ones stay closed (dismiss).
  const filled = useRef(new Set<string>());
  const sessions = info?.sessions;
  const pageID = host.livePage().id;
  useEffect(() => {
    const h = latest.current;
    // Once per page while the setting is on: turned off and on again, it fills the page again.
    if (!h.tabs) { filled.current.clear(); return; }
    if (!sessions || !h.scope) return;
    const key = `${h.scope}:${pageID}`;
    if (filled.current.has(key)) return;
    filled.current.add(key);
    const page = h.livePage(), next = showWorkspaceTabs(page, workspaceConversations(sessions, h.scope, dismissed()), WORKSPACE_TAB_CAP, newID);
    if (next !== page) h.changePage(next);
  }, [sessions, host.scope, pageID, host.tabs]);

  // Typing in a preview tab keeps it open, as editing a previewed file does in an IDE. After the
  // message box has taken the keystroke (bubbling, then a microtask): a render before React's own
  // change handler would put the controlled box back to its old value and lose what was typed.
  useEffect(() => {
    const typed = (event: Event) => {
      const id = event.target instanceof Element ? event.target.closest('[data-panel-id]')?.getAttribute('data-panel-id') : undefined;
      if (!id) return;
      queueMicrotask(() => {
        const h = latest.current, page = h.livePage();
        if (shownPanels(page).some(panel => panel.id === id && panel.preview)) h.changePage(keepTab(page, id));
      });
    };
    document.addEventListener('input', typed);
    return () => document.removeEventListener('input', typed);
  }, []);

  // A conversation the Runner listed and no longer does was deleted (here or elsewhere): its tabs go.
  const listed = useRef<Set<string>>(undefined);
  useEffect(() => {
    if (!sessions) return;
    const now = new Set(sessions.map(session => session.id)), before = listed.current;
    listed.current = now;
    if (before) for (const id of before) if (!now.has(id)) forget(id);
  }, [sessions]);

  return { open, related, newPage, closeTabs, tabOf, remember, reopen, canReopen, statusOf, forget };
}
