import { useEffect, useRef, type MouseEvent as ReactMouseEvent } from 'react';
import type { OpenMode, ViewTarget } from './workspace-model';
import { rowGesture } from './conversation-tabs';
import { isMac } from './actions';

/**
 * What a conversation row can ask of the window it is in, whichever list it
 * sits in (Workspaces, Agents, the Inbox): open the conversation as a tab,
 * beside or below the one on screen, on a page or in a window of its own,
 * close the tabs around its tab, or reveal the Agents panel. The window's
 * Workspace provides them; a surface without one (a detached chat window)
 * has none, and the menus say so instead of offering a dead item.
 */
export type ConversationOpeners = {
  /** A second chat panel showing this conversation, beside the page's conversation (split right). */
  beside(target: ViewTarget): void;
  /** The same, below it (split down). */
  below(target: ViewTarget): void;
  /** As a tab: in the preview tab, a regular tab in front, or one behind it (see OpenMode). */
  open(target: ViewTarget, mode: OpenMode): void;
  /** Opened from another conversation (its review, an orchestrator's agent): a tab right after its parent's, behind it unless `background` is false. */
  related(parent: ViewTarget, target: ViewTarget, options?: { background?: boolean }): void;
  /** On a page of its own, named after it. */
  newPage(target: ViewTarget, title: string): void;
  /** Its tab on the page, if open: how many other tabs its group has, and how many are to its right. */
  tabOf(target: ViewTarget): { others: number; right: number } | undefined;
  /** Close the other tabs of its group, or those to the right of its tab. */
  closeTabs(target: ViewTarget, which: 'others' | 'right'): void;
  /** A conversation deleted: its tabs leave every page, and it is not offered to reopen. */
  forget(sessionId: string): void;
  /** The conversation in its own native window; absent where the host cannot open one. */
  window?(target: ViewTarget): void;
  /** Bring the page's Agents panel to the front; false when the page has none. */
  showAgents(): boolean;
};

let current: ConversationOpeners | undefined;
export const conversationOpeners = () => current;

/**
 * A conversation opened from another one as a tab next to its parent's,
 * behind it (`background`, the default) or in front: a review of a
 * conversation, an agent an orchestrator started. False when this window
 * cannot (no workspace page): the caller shows it its own way.
 */
export function openRelatedConversation(parent: ViewTarget, target: ViewTarget, options: { background?: boolean } = {}): boolean {
  if (!current) return false;
  current.related(parent, target, options);
  return true;
}

/**
 * The pointer gestures of a conversation row, the same in every list: a click
 * shows it in the preview tab (`onOpen`, the list's own way, which also
 * switches workspace or Runner), a double-click keeps it in a tab, ⌘-click
 * and a middle-click open it in a tab behind the one in front.
 */
export function conversationRowGestures(target: ViewTarget, onOpen: () => void) {
  const run = (event: ReactMouseEvent) => {
    const mode = rowGesture({ button: event.button, detail: event.detail, metaKey: event.metaKey, ctrlKey: event.ctrlKey, altKey: event.altKey, type: event.type }, isMac);
    if (!mode) return;
    const openers = current;
    if (mode === 'preview' || !openers) { if (event.type === 'click') onOpen(); return; }
    event.preventDefault();
    // The single click before a double-click showed it already; the double-click keeps it.
    openers.open(target, mode);
  };
  return {
    onClick: run, onDoubleClick: run, onAuxClick: run,
    // A middle-click does not start the browser's scrolling.
    onMouseDown: (event: ReactMouseEvent) => { if (event.button === 1) event.preventDefault(); },
  };
}

/** Provide the openers while mounted (the window's Workspace); the latest closures are used. */
export function useProvideConversationOpeners(openers: ConversationOpeners) {
  const latest = useRef(openers); latest.current = openers;
  useEffect(() => {
    const proxy: ConversationOpeners = {
      beside: target => latest.current.beside(target),
      below: target => latest.current.below(target),
      open: (target, mode) => latest.current.open(target, mode),
      related: (parent, target, options) => latest.current.related(parent, target, options),
      newPage: (target, title) => latest.current.newPage(target, title),
      tabOf: target => latest.current.tabOf(target),
      closeTabs: (target, which) => latest.current.closeTabs(target, which),
      forget: sessionId => latest.current.forget(sessionId),
      showAgents: () => latest.current.showAgents(),
    };
    Object.defineProperty(proxy, 'window', { get: () => latest.current.window, enumerable: true });
    current = proxy;
    return () => { if (current === proxy) current = undefined; };
  }, []);
}

/**
 * The Inbox asked for the Agents panel: the panel takes focus on its first
 * row. A panel shown for the request mounts after it was made, so the request
 * waits briefly for one to take it (as Ask Lazybox's does).
 */
const EVENT = 'lazybox-agents-focus';
let pending = 0;
export function requestAgentsFocus() {
  pending = Date.now();
  window.dispatchEvent(new Event(EVENT));
}
export function useAgentsFocusRequests(onFocus: () => void) {
  const latest = useRef(onFocus); latest.current = onFocus;
  useEffect(() => {
    const take = () => { if (!pending) return; pending = 0; requestAnimationFrame(() => latest.current()); };
    if (pending && Date.now() - pending < 3000) queueMicrotask(take); else pending = 0;
    window.addEventListener(EVENT, take);
    return () => window.removeEventListener(EVENT, take);
  }, []);
}
