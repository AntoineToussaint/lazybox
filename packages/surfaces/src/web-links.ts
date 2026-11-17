import { useEffect, useSyncExternalStore, type MouseEvent as ReactMouseEvent } from 'react';
import type { ViewStateStore } from '@lazybox/platform';
import { contextMenu, copyText, registerContextMenu } from './ContextMenu';
import { isMenuClick, type MenuItem } from './context-menu-model';
import { readPreference, writePreference } from './view-state';
import { linkHint, searchEngine, webPage, type LinkTarget, type SearchEngine } from './web-address';
import { isMac } from './workbench';

/**
 * Where a web link goes when a person clicks it, in every window: their
 * browser by default, the Lazybox Web panel with ⌥ (Alt), and the other way
 * round when Settings → Web & links says so. A right-click on any web link
 * offers both (the `link` context-menu scope). The app's own page never
 * navigates away.
 */
export type { LinkTarget } from './web-address';
export type LinkPreferences = { target: LinkTarget; engine: SearchEngine };
export type ViewerRequest = { url: string; newPanel: boolean };
type Viewer = (request: ViewerRequest) => boolean;

let preferences: LinkPreferences = { target: 'browser', engine: searchEngine(undefined) };
let openExternal: ((url: string) => Promise<void>) | undefined;
const viewers: Viewer[] = [];
const listeners = new Set<() => void>();
const emit = () => { for (const listener of listeners) listener(); };
const subscribe = (listener: () => void) => { listeners.add(listener); return () => { listeners.delete(listener); }; };

/** Opens a link in the person's browser: the desktop host's default browser, else a new tab. */
export async function openInBrowser(url: string) {
  if (openExternal) return openExternal(url);
  window.open(url, '_blank', 'noopener,noreferrer');
}
/** Opens a web page in this window's Web panel; false when no workspace page can show one. */
export function openInViewer(url: string, newPanel = false): boolean {
  const page = webPage(url);
  if (!page) return false;
  for (let i = viewers.length - 1; i >= 0; i--) if (viewers[i]({ url: page, newPanel })) return true;
  return false;
}
export const canOpenInViewer = () => viewers.length > 0;
/**
 * A server a command started, previewed: its local page in this window's Web
 * panel, else the person's browser. No workspace action needed (the "Preview
 * :<port>" buttons of the live bar and the command console).
 */
export async function openPreview(port: number) {
  const url = `http://localhost:${port}/`;
  if (!openInViewer(url)) await openInBrowser(url);
}

/** A workspace page offers its Web panel to links while it is mounted. */
export function useWebViewer(viewer: Viewer) {
  useEffect(() => {
    const entry: Viewer = request => viewer(request);
    viewers.push(entry); emit();
    return () => { const index = viewers.indexOf(entry); if (index >= 0) viewers.splice(index, 1); emit(); };
  });
}

const preferenceNames = { target: 'web.links', engine: 'web.search' } as const;
export function readLinkPreferences(store?: ViewStateStore): LinkPreferences {
  let target: LinkTarget = 'browser', engine = searchEngine(undefined);
  try { target = readPreference(store, preferenceNames.target) === 'viewer' ? 'viewer' : 'browser'; engine = searchEngine(readPreference(store, preferenceNames.engine)); } catch { /* defaults */ }
  return { target, engine };
}
/** Saves and applies the link preferences for this window. */
export function saveLinkPreferences(store: ViewStateStore | undefined, next: LinkPreferences) {
  writePreference(store, preferenceNames.target, next.target);
  writePreference(store, preferenceNames.engine, next.engine.id);
  preferences = next; emit();
}
/** The window's link preferences, read from `store` (when given) and kept current. */
export function useLinkPreferences(store?: ViewStateStore): LinkPreferences {
  useEffect(() => { if (!store) return; const read = readLinkPreferences(store); if (read.target !== preferences.target || read.engine.id !== preferences.engine.id) { preferences = read; emit(); } }, [store]);
  return useSyncExternalStore(subscribe, () => preferences);
}
export const linkPreferences = () => preferences;

const externalLink = (anchor: HTMLAnchorElement) => {
  const href = anchor.getAttribute('href') || '';
  if (!/^(https?:|mailto:)/i.test(href)) return '';
  try { const url = new URL(anchor.href); return url.origin === location.origin && url.protocol !== 'mailto:' ? '' : url.href; } catch { return ''; }
};
const anchorOf = (target: EventTarget | null) => (target instanceof Element ? target.closest('a[href]') : null) as HTMLAnchorElement | null;
/** Links inside `[data-link-routing="own"]` (the command console) open the way that view says; the right-click menu still applies. */
const ownRouting = (anchor: Element) => !!anchor.closest('[data-link-routing="own"]');

/** What a right-click on a web link offers, the default first. */
export function linkMenuItems(href: string): MenuItem[] {
  const page = webPage(href);
  if (!page) return [];
  const report = (error: unknown) => console.error('Could not open link', error);
  const alt = 'Alt+Click';
  const viewerFirst = preferences.target === 'viewer';
  const noViewer = canOpenInViewer() ? false : 'Open a workspace window to show pages in Lazybox';
  const inLazybox: MenuItem = { label: 'Open in Lazybox', icon: 'globe', shortcut: viewerFirst ? undefined : alt, note: viewerFirst ? 'Click' : undefined, disabled: noViewer, run: () => { if (!openInViewer(page)) void openInBrowser(page).catch(report); } };
  const inBrowser: MenuItem = { label: 'Open in your browser', icon: 'external', shortcut: viewerFirst ? alt : undefined, note: viewerFirst ? undefined : 'Click', run: () => void openInBrowser(page).catch(report) };
  return [
    ...(viewerFirst ? [inLazybox, inBrowser] : [inBrowser, inLazybox]),
    { label: 'Open in a new Lazybox panel', icon: 'plus', disabled: noViewer, run: () => { if (!openInViewer(page, true)) void openInBrowser(page).catch(report); } },
    'separator',
    { label: 'Copy link', icon: 'link', run: () => copyText(page, 'Copied link') },
  ];
}

let installed: (() => void) | undefined;
/**
 * Routes this window's link clicks. Call once per window; `external` opens a
 * link in the system browser (the desktop host). Without it a link opens in
 * a new tab. Returns the uninstall.
 */
export function installLinkRouting(external?: (url: string) => Promise<void>): () => void {
  installed?.();
  openExternal = external;
  const report = (error: unknown) => console.error('Could not open link', error);
  // The link a right-click (or Ctrl+click) opened the menu on, until the next press.
  let menuAnchor: Element | null = null;
  const press = () => { menuAnchor = null; };
  const click = (event: MouseEvent) => {
    if (event.defaultPrevented || event.button !== 0) return;
    const anchor = anchorOf(event.target);
    const url = anchor && externalLink(anchor);
    if (!url) return;
    // Ctrl+click on a Mac is a right-click: its menu opens, the link does not
    // (WebKit follows the menu with this click). Here too for links that
    // route themselves (the command console).
    const menuClick = isMenuClick(event, isMac) || (anchor === menuAnchor && event.detail > 0);
    menuAnchor = null;
    if (menuClick) { event.preventDefault(); event.stopPropagation(); return; }
    if (ownRouting(anchor)) return;
    const page = webPage(url);
    // ⌘-click (Ctrl-click off a Mac) and Shift-click are the browser's own "new tab / window" gestures.
    const explicit = event.metaKey || event.ctrlKey || event.shiftKey;
    const inViewer = !explicit && !!page && (preferences.target === 'viewer') !== event.altKey;
    if (inViewer && openInViewer(page)) { event.preventDefault(); return; }
    // Without a host, a link that opens a new tab (or a mail app) already does;
    // others are kept from replacing the app.
    if (!openExternal && ((anchor!.target === '_blank' && !event.altKey) || !page)) return;
    event.preventDefault();
    void openInBrowser(url).catch(report);
  };
  // Every web link, wherever it is rendered, is a `link` section of the context menu.
  const contextmenu = (event: MouseEvent) => {
    const anchor = anchorOf(event.target);
    const url = anchor && externalLink(anchor);
    if (!url) return;
    if (event.isTrusted) menuAnchor = anchor;
    if (!webPage(url)) return;
    let host = url; try { host = new URL(url).host; } catch { /* the address itself */ }
    contextMenu('link', undefined, { href: url }, `Link to ${host}`)({ nativeEvent: event, currentTarget: anchor } as unknown as ReactMouseEvent<Element>);
  };
  const hover = (event: MouseEvent) => {
    const anchor = anchorOf(event.target);
    const url = anchor && externalLink(anchor);
    if (!anchor || !url || (anchor.title && !anchor.dataset.linkHint) || ownRouting(anchor)) return;
    const hint = linkHint(url, preferences.target, canOpenInViewer(), isMac ? '⌥' : 'Alt');
    if (anchor.title !== hint) { anchor.title = hint; anchor.dataset.linkHint = '1'; }
  };
  const stopMenu = registerContextMenu<{ href?: string }>('link', context => linkMenuItems(context.detail?.href || ''));
  document.addEventListener('click', click, true);
  document.addEventListener('contextmenu', contextmenu, true);
  document.addEventListener('mouseover', hover, true);
  document.addEventListener('pointerdown', press, true);
  installed = () => { stopMenu(); document.removeEventListener('click', click, true); document.removeEventListener('contextmenu', contextmenu, true); document.removeEventListener('mouseover', hover, true); document.removeEventListener('pointerdown', press, true); installed = undefined; };
  return installed;
}
