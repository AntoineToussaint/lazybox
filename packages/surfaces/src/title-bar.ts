import type { TitleBarHost, TitleBarInset } from '@lazybox/platform';
import './title-bar.css';

/**
 * No separate title bar (the desktop app on macOS): each window's header is
 * its title bar. The host draws the system's window buttons over the
 * header's leading edge; this marks the window (`data-title-bar="overlay"` on
 * `<html>`), gives the header their room (`--title-bar-inset`, at least
 * `--title-bar-height` tall, both in CSS pixels at the window's zoom), drops
 * the room in full screen (`data-full-screen`), and moves the window when an
 * empty part of a header is dragged. A browser, the phone and the web app
 * never configure it: their headers are unchanged.
 *
 * Headers say which of their parts move the window with
 * `data-window-drag-region`, read as Tauri reads its own
 * `data-tauri-drag-region`: `"deep"` for the element and everything in it,
 * `""` for the element itself, `"false"` for nothing in it. Controls inside
 * (buttons, links, fields, tabs, menus, popovers, anything focusable or
 * draggable) never move the window. Not Tauri's attribute: its drag regions
 * always maximize on a double-click, whatever the system setting says.
 */
export const DRAG_REGION = 'data-window-drag-region';

const CONTROLS = new Set(['A', 'BUTTON', 'INPUT', 'SELECT', 'TEXTAREA', 'LABEL', 'SUMMARY', 'DETAILS', 'DIALOG', 'OPTION', 'IFRAME']);
const CONTROL_ROLES = new Set(['button', 'link', 'menuitem', 'menuitemcheckbox', 'menuitemradio', 'tab', 'checkbox', 'radio', 'switch', 'option', 'menu', 'listbox', 'dialog', 'alertdialog', 'textbox', 'combobox', 'searchbox', 'slider', 'spinbutton', 'tooltip', 'separator']);

/** Something a press acts on, not the title bar behind it. */
export function isControl(element: Element): boolean {
  if (CONTROLS.has(element.tagName.toUpperCase())) return true;
  if (element.hasAttribute('popover') || element.getAttribute('draggable') === 'true') return true;
  const editable = element.getAttribute('contenteditable');
  if (editable !== null && editable !== 'false') return true;
  const tabindex = element.getAttribute('tabindex');
  if (tabindex !== null && tabindex !== '-1') return true;
  return CONTROL_ROLES.has(element.getAttribute('role') || '');
}

/**
 * Whether a press whose event path is `path` (innermost first) moves the
 * window. Elements are recognised by shape, not `instanceof`: a popped-out
 * panel window's elements belong to another window's realm.
 */
export function isDragRegion(path: readonly EventTarget[]): boolean {
  for (const target of path) {
    const element = target as Element;
    if ((element as Node).nodeType !== 1 || typeof element.getAttribute !== 'function') continue;
    const value = element.getAttribute(DRAG_REGION);
    if (value === null) {
      if (isControl(element)) return false;
      continue;
    }
    if (value === 'false') return false;
    if (value === 'deep') return true;
    return element === path[0];
  }
  return false;
}

type Configured = { host: TitleBarHost; zoom: number; fullScreen: boolean; popouts: Set<Window> };
let configured: Configured | undefined;
/** Popped-out windows' documents whose presses are already followed. */
const listening = new WeakSet<Document>();

function apply(root: HTMLElement, inset: TitleBarInset, zoom: number | undefined, fullScreen: boolean) {
  root.dataset.titleBar = 'overlay';
  root.toggleAttribute('data-full-screen', fullScreen);
  root.style.setProperty('--title-bar-inset', zoom === undefined ? `calc(${inset.inset}px / var(--popout-page-zoom, 1))` : `${Math.ceil(inset.inset / zoom)}px`);
  root.style.setProperty('--title-bar-height', zoom === undefined ? `calc(${inset.height}px / var(--popout-page-zoom, 1))` : `${inset.height / zoom}px`);
}

/** Presses on a document's headers move the window that shows it. */
function listen(document: Document, popout: boolean): () => void {
  let pressedAt: [number, number] | undefined;
  const down = (event: MouseEvent) => {
    if (event.button !== 0 || event.ctrlKey || !configured || !isDragRegion(event.composedPath())) return;
    // No text selection and no focus change: this press belongs to the window.
    event.preventDefault();
    if (event.detail === 1) { pressedAt = undefined; void configured.host.startDragging(popout).catch(() => {}); }
    // The second press of a double-click acts when it is released where it was
    // pressed, as macOS does (moving away cancels it).
    else if (event.detail === 2) pressedAt = [event.clientX, event.clientY];
  };
  const up = (event: MouseEvent) => {
    const at = pressedAt;
    pressedAt = undefined;
    if (!at || event.button !== 0 || event.detail !== 2 || !configured) return;
    if (event.clientX !== at[0] || event.clientY !== at[1] || !isDragRegion(event.composedPath())) return;
    void configured.host.doubleClick(popout).catch(() => {});
  };
  document.addEventListener('mousedown', down);
  document.addEventListener('mouseup', up);
  return () => { document.removeEventListener('mousedown', down); document.removeEventListener('mouseup', up); };
}

/**
 * The desktop host calls this once per window, before it draws. Returns the
 * undo (tests).
 */
export function configureTitleBar(host: TitleBarHost): () => void {
  const state: Configured = { host, zoom: 1, fullScreen: host.fullScreen, popouts: new Set() };
  configured = state;
  const root = document.documentElement;
  const refresh = () => {
    apply(root, host.window, state.zoom, state.fullScreen);
    for (const popout of state.popouts) {
      if (popout.closed) state.popouts.delete(popout);
      else apply(popout.document.documentElement, host.popout, undefined, !!popout.document.documentElement.hasAttribute('data-full-screen'));
    }
  };
  refresh();
  const stops = [
    listen(document, false),
    host.onFullScreen(fullScreen => { state.fullScreen = fullScreen; refresh(); }),
    host.onPopoutFullScreen?.(fullScreen => {
      // Full screen is entered and left by the window in front.
      for (const popout of state.popouts) {
        if (popout.closed) { state.popouts.delete(popout); continue; }
        if (popout.document.hasFocus()) apply(popout.document.documentElement, host.popout, undefined, fullScreen);
      }
    }),
    host.zoom?.subscribe(level => { state.zoom = level; refresh(); }),
  ];
  void host.zoom?.get().then(level => { state.zoom = level; refresh(); }, () => {});
  return () => {
    for (const stop of stops) stop?.();
    if (configured === state) configured = undefined;
    delete root.dataset.titleBar;
    root.removeAttribute('data-full-screen');
    root.style.removeProperty('--title-bar-inset');
    root.style.removeProperty('--title-bar-height');
  };
}

/**
 * A panel group popped out into a window of its own (DockLayout): the
 * group's tab bar is that window's title bar. Nothing where this window's
 * header is not one.
 */
export function attachPopoutTitleBar(popout: Window | undefined, tabBar: Element | null | undefined) {
  if (!configured || !popout) return;
  const state = configured;
  state.popouts.add(popout);
  const root = popout.document.documentElement;
  root.toggleAttribute('data-popout', true);
  apply(root, state.host.popout, undefined, false);
  if (tabBar) {
    tabBar.setAttribute(DRAG_REGION, 'deep');
    // dockview drags the group by the tab bar's empty part; in its own window
    // that part moves the window instead.
    tabBar.querySelector('.dv-void-container')?.setAttribute(DRAG_REGION, '');
  }
  if (!listening.has(popout.document)) {
    listening.add(popout.document);
    listen(popout.document, true);
  }
}
