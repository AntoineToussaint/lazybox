/**
 * Lazybox's context menus: one system for every window.
 *
 * Inside the app a right-click (Ctrl+click on a Mac, a long press on touch,
 * Shift+F10 or the context-menu key on the focused element) opens a Lazybox
 * menu for that place, never the browser's or macOS's. The press that opened
 * a menu is never also a click, and releasing it chooses an item only after
 * the pointer moved onto one. Text fields keep the native menu
 * (spelling, paste, emoji); Monaco keeps its own; Alt+right-click shows the
 * native menu in development, for Inspect Element.
 *
 * A menu is built from *sections*, innermost first, separated by lines:
 *
 *   1. "Copy" when the right-click is on a text selection;
 *   2. the section of each element under the pointer that declares one, from
 *      the innermost out (a code block's items, then its message's);
 *   3. when nothing declared anything: "Reload window" (and, in development,
 *      the inspector).
 *
 * ## Declaring a menu (in a component)
 *
 *   <article onContextMenu={contextMenu('message', context => [
 *     { label: 'Copy message', icon: 'copy', run: () => copyText(text) },
 *     'separator',
 *     { label: 'Delete…', danger: true, disabled: busy && 'Wait for the turn to end', run: remove },
 *   ], { message, sessionId })}>
 *
 * The handler does not open anything itself: it adds its section to the event
 * and the window-level listener assembles every section once the event has
 * bubbled, so nested places compose. `useContextMenu(scope, items, detail)` is
 * the same as a hook with a stable handler, for memoized rows.
 *
 * ## Contributing to someone else's menu (the extension point)
 *
 * Any feature can add items to a scope without touching the component that
 * declares it:
 *
 *   const stop = registerContextMenu('message', context =>
 *     context.detail.role === 'agent' ? [{ label: 'Track as artifact…', icon: 'sparkle', run: () => track(context.detail.message) }] : []);
 *
 * or, from a component, `useContextMenuContribution('message', context => […])`.
 * Contributions follow the declaring component's own items in that section.
 * A contributor may return a promise: the menu opens at once and its items
 * join it when they arrive (the artifacts feature lists, from the Runner, the
 * artifacts a message could update).
 * An element can also declare a scope with markup alone,
 * `data-context-menu="artifact"`; contributors then receive the element's
 * `dataset` as `detail`.
 *
 * Scopes declared today, and their `detail`:
 *
 * | scope           | where                         | detail                                                        |
 * |-----------------|-------------------------------|---------------------------------------------------------------|
 * | `message`       | a chat message (artifacts add Track/Update) | `{ message: TranscriptMessage; role: 'agent' \| 'user' \| 'notice'; sessionId: string }` |
 * | `code-block`    | a code block in rich content (a shell block of an agent's adds Run) | `{ text: string; language: string }` |
 * | `inline-code`   | inline code an agent wrote that reads as a command | `{ text: string; command: string }`  |
 * | `file-link`     | a file reference in a message (artifacts add Track) | `{ path: string; line?: number }`       |
 * | `file`          | a Files tree row              | `{ path: string; directory: boolean; workspaceId: string }`  |
 * | `editor-tab`    | an editor tab                 | `{ path: string; group: string; workspaceId: string }`       |
 * | `panel`         | a panel header                | `{ kind: string; id: string }`                                |
 * | `workspace`     | a workspace row               | `{ workspaceId: string }`                                     |
 * | `conversation`  | a conversation row            | `{ sessionId: string; workspaceId: string }`                  |
 * | `changed-file`  | a Changes file row            | `{ member: string; path: string; committed: boolean }`        |
 * | `diff`          | the Changes diff              | `{ member: string; path: string }`                            |
 * | `process`       | a Processes row               | `{ process: HostedProcess }`                                  |
 * | `command`       | a command output console      | `{ commandId: string }`                                       |
 * | `inbox-item`    | an Inbox row                  | `{ sessionId: string; workspaceId: string }`                  |
 * | `terminal`      | a workspace terminal          | `{ terminalId: string; workspaceId: string }`                 |
 * | `terminal-tab`  | a terminal's tab              | `{ terminalId: string; workspaceId: string }`                 |
 * | `turn`          | an agent turn's marker        | `{ turn: number }`                                            |
 * | `diff-line`     | a line of the Changes diff    | `{ member; path; line: number; side: 'old' \| 'new' }`        |
 * | `link`          | any web link (`web-links.ts`) | `{ href: string }`                                            |
 * | `session`       | a chat's header               | `{ sessionId: string }`                                       |
 * | `subagent`      | a sub-agent / background command card, chip or row | `{ agentId: string; kind: 'agent' \| 'command' }` |
 * | `live-bar`      | the running turn's live bar   | `{ sessionId: string; commandId: string }`                    |
 *
 * `openContextMenu(event, items)` opens a menu directly; `copyText` and
 * `showMenuNotice` are the helpers items use for feedback. A window shows
 * menus once `<ContextMenuHost/>` is mounted (`WindowErrorBoundary` does it
 * for every app window); a host app adds native abilities with
 * `configureContextMenus` (the desktop app: Reveal in Finder, the inspector).
 */
import { useEffect, useLayoutEffect, useRef, useState, useSyncExternalStore, type KeyboardEvent as ReactKeyboardEvent, type MouseEvent as ReactMouseEvent } from 'react';
import { createPortal } from 'react-dom';
import { Icon } from './Icons';
import { shortcut as keys } from './workbench';
import { claimPress } from './dismiss';
import { disabledReason, firstEnabled, joinSections, lastEnabled, normalizeMenu, placeMenu, placeSubmenu, rightReleaseChooses, stepFocus, typeahead, dedupeLabels, type MenuEntry, type MenuItem } from './context-menu-model';
import './context-menu.css';

export type { MenuAction, MenuItem } from './context-menu-model';

export type ContextMenuContext<Detail = any> = {
  scope: string;
  /** The element that declared the scope. */
  element: Element;
  /** What was right-clicked (or focused, from the keyboard). */
  target: Element;
  /** What the declaring component passed; an attribute-only scope gets the element's dataset. */
  detail: Detail;
  /** The selected text the right-click was on, or ''. */
  selection: string;
  /** That selection lies inside `element`. */
  selectionInside: boolean;
  /** Opened from the keyboard: the first item takes focus. */
  keyboard: boolean;
};
/** Items now, or later: a promise's items join the open menu when they arrive (an artifact list read from the Runner). */
export type ContextMenuContributor<Detail = any> = (context: ContextMenuContext<Detail>) => MenuItem[] | Promise<MenuItem[] | undefined | void> | undefined | void;

/** What the host app adds. Every field is optional; items that need a missing one are not shown. */
export type ContextMenuPlatform = {
  /** Development build: the inspector item and Alt+right-click for the native menu. */
  development: boolean;
  /** Open the developer tools (a desktop development build). */
  inspect?(): void | Promise<void>;
  /** Show an absolute path in Finder or the platform's file manager. */
  revealInFinder?(path: string): void | Promise<void>;
};
let platform: ContextMenuPlatform = { development: !!import.meta.env?.DEV };
export function configureContextMenus(change: Partial<ContextMenuPlatform>) { platform = { ...platform, ...change }; }
export const contextMenuPlatform = () => platform;

// Contributions ---------------------------------------------------------------

const contributors = new Map<string, Set<ContextMenuContributor>>();
/** Add items to every menu of `scope`. Returns the removal. */
export function registerContextMenu<Detail = any>(scope: string, contributor: ContextMenuContributor<Detail>): () => void {
  let set = contributors.get(scope);
  if (!set) { set = new Set(); contributors.set(scope, set); }
  set.add(contributor);
  return () => { set!.delete(contributor); };
}
/** `registerContextMenu` for as long as a component is mounted; the latest closure is used. */
export function useContextMenuContribution<Detail = any>(scope: string, contributor: ContextMenuContributor<Detail>) {
  const latest = useRef(contributor); latest.current = contributor;
  useEffect(() => registerContextMenu<Detail>(scope, context => latest.current(context)), [scope]);
}

type Section = { scope: string; element: Element; detail: unknown; items?: ContextMenuContributor; label?: string };
const pendingSections = new WeakMap<Event, Section[]>();

/** An `onContextMenu` handler that adds this element's section to the menu. `label` names the menu when this is its innermost section. */
export function contextMenu<Detail = any>(scope: string, items?: ContextMenuContributor<Detail>, detail?: Detail, label?: string) {
  return (event: ReactMouseEvent<Element>) => {
    const list = pendingSections.get(event.nativeEvent) || [];
    list.push({ scope, element: event.currentTarget, detail, items: items as ContextMenuContributor, label });
    pendingSections.set(event.nativeEvent, list);
  };
}
/** `contextMenu` with a handler that stays the same between renders. */
export function useContextMenu<Detail = any>(scope: string, items?: ContextMenuContributor<Detail>, detail?: Detail) {
  const latest = useRef({ items, detail }); latest.current = { items, detail };
  const [handler] = useState(() => (event: ReactMouseEvent<Element>) => contextMenu<Detail>(scope, context => latest.current.items?.(context), latest.current.detail)(event));
  return handler;
}

// Feedback --------------------------------------------------------------------

type Notice = { id: number; text: string };
type OpenMenu = { entries: MenuEntry[]; x: number; y: number; keyboard: boolean; container: Element; restore: Element | null; openedAt: number; label: string; onClose?(): void };
let state: { menu: OpenMenu | null; notice: Notice | null } = { menu: null, notice: null };
const listeners = new Set<() => void>();
const emit = (change: Partial<typeof state>) => { state = { ...state, ...change }; for (const listener of listeners) listener(); };
let noticeTimer: ReturnType<typeof setTimeout> | undefined;

/** A short confirmation ("Copied") near the bottom of the window. */
export function showMenuNotice(text: string) {
  clearTimeout(noticeTimer);
  emit({ notice: { id: Date.now(), text } });
  noticeTimer = setTimeout(() => emit({ notice: null }), 2200);
}

/** Put text on the clipboard and say so. */
export async function copyText(text: string, done = 'Copied') {
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    // Some webviews refuse the async clipboard outside a key or click handler.
    const area = document.createElement('textarea');
    area.value = text; area.setAttribute('readonly', ''); area.style.position = 'fixed'; area.style.opacity = '0';
    document.body.append(area); area.select();
    const copied = document.execCommand('copy'); area.remove();
    if (!copied) { showMenuNotice('Could not copy: the clipboard is unavailable'); return; }
  }
  showMenuNotice(done);
}

// Opening and closing ---------------------------------------------------------

/** Open a menu at the pointer of `at` (preventing its native menu), or at a point. */
export function openContextMenu(at: { clientX: number; clientY: number; preventDefault?(): void } | { x: number; y: number }, items: MenuItem[], { keyboard = false, target, restore: restoreTo, onClose, label = 'Context menu' }: { keyboard?: boolean; target?: Element; /** Where focus goes back on close; the focused element by default. */ restore?: Element | null; /** Called once the menu closes, however it closes. */ onClose?(): void; /** The menu's accessible name. */ label?: string } = {}) {
  const point = 'clientX' in at ? { x: at.clientX, y: at.clientY } : at;
  if ('preventDefault' in at) at.preventDefault?.();
  const entries = normalizeMenu(items);
  if (!entries.length) { closeContextMenu(false); return 0; }
  // One menu at a time: a ⋯ menu that is open closes.
  for (const menu of document.querySelectorAll<HTMLDetailsElement>('details.workspace-actions[open]')) menu.open = false;
  const previous = state.menu;
  const restore = restoreTo !== undefined ? restoreTo : previous?.restore ?? (document.activeElement instanceof HTMLElement && document.activeElement !== document.body ? document.activeElement : null);
  previous?.onClose?.();
  const container = (target && modalDialogOf(target)) || document.body;
  const openedAt = performance.now();
  emit({ menu: { entries, x: point.x, y: point.y, keyboard, container, restore, openedAt, label, onClose } });
  return openedAt;
}
/** Close the open menu; `restore` gives focus back to where it was. */
export function closeContextMenu(restore = true) {
  const menu = state.menu;
  if (!menu) return;
  emit({ menu: null });
  menu.onClose?.();
  if (restore && menu.restore instanceof HTMLElement && menu.restore.isConnected) menu.restore.focus({ preventScroll: true });
}
export const contextMenuOpen = () => !!state.menu;

const modalDialogOf = (element: Element) => {
  const dialog = element.closest('dialog');
  return dialog?.matches(':modal') ? dialog : null;
};

// Assembling a menu from a contextmenu event -----------------------------------

const elementOf = (node: EventTarget | null) => node instanceof Element ? node : node instanceof Node ? node.parentElement : null;
const textInputTypes = new Set(['', 'text', 'search', 'email', 'url', 'tel', 'password', 'number']);
/** Text fields keep the native menu: spelling, paste, emoji. */
export function isEditableTarget(target: Element): boolean {
  const field = target.closest('input, textarea, [contenteditable]');
  if (!field) return false;
  if (field.matches('.xterm-helper-textarea') || field.closest('.monaco-editor')) return false;
  if (field instanceof HTMLInputElement) return textInputTypes.has(field.type);
  if (field instanceof HTMLTextAreaElement) return true;
  return (field as HTMLElement).isContentEditable;
}

/** The selected text, when the pointer is on it (or the menu comes from the keyboard). */
function selectionAt(point: { x: number; y: number } | null): { text: string; range?: Range } {
  const selection = document.getSelection();
  if (!selection || selection.isCollapsed || !selection.rangeCount) return { text: '' };
  const text = selection.toString();
  if (!text.trim()) return { text: '' };
  const range = selection.getRangeAt(0);
  if (!point) return { text, range };
  const slack = 4;
  for (const rect of range.getClientRects()) if (point.x >= rect.left - slack && point.x <= rect.right + slack && point.y >= rect.top - slack && point.y <= rect.bottom + slack) return { text, range };
  return { text: '' };
}

const keyboardEvents = new WeakSet<Event>();
let selectionAtPress: string | null = null;

/** The whole menu for a contextmenu event that reached the window: its items now, its label, and items still on their way. */
export function menuForEvent(event: MouseEvent, keyboard: boolean): { items: MenuItem[]; label: string; later: Promise<MenuItem[]> | undefined } {
  const target = elementOf(event.target)!;
  const found = [...(pendingSections.get(event) || [])];
  const ancestors: Element[] = [];
  for (let node: Element | null = target; node; node = node.parentElement) ancestors.push(node);
  for (const node of ancestors) {
    const scope = node.getAttribute('data-context-menu');
    if (scope && !found.some(section => section.element === node && section.scope === scope)) found.push({ scope, element: node, detail: { ...(node as HTMLElement).dataset } });
  }
  const rank = (section: Section) => { const index = ancestors.indexOf(section.element); return index < 0 ? ancestors.length : index; };
  const sections = found.map((section, order) => ({ section, order })).sort((a, b) => rank(a.section) - rank(b.section) || a.order - b.order).map(value => value.section);
  const { text, range } = selectionAt(keyboard ? null : { x: event.clientX, y: event.clientY });
  // Each contribution is its items, or a slot its promise fills.
  const waiting: Promise<void>[] = [];
  const bodies = sections.map(section => {
    const context: ContextMenuContext = { scope: section.scope, element: section.element, target, detail: section.detail, selection: text, selectionInside: !!range && section.element.contains(range.commonAncestorContainer), keyboard };
    return [section.items, ...(contributors.get(section.scope) || [])].map(contribute => {
      const slot: { items: MenuItem[] } = { items: [] };
      try {
        const result = contribute?.(context);
        if (result instanceof Promise) waiting.push(result.then(items => { slot.items = items || []; }, error => console.error('Context menu contribution failed', section.scope, error)));
        else slot.items = result || [];
      } catch (error) { console.error('Context menu contribution failed', section.scope, error); }
      return slot;
    });
  });
  const flat = () => bodies.map(slots => slots.flatMap(slot => slot.items));
  const copy: MenuItem[] = text ? [{ label: 'Copy', icon: 'copy', shortcut: 'Mod+C', run: () => copyText(text) }] : [];
  const anything = waiting.length > 0 || bodies.some(slots => slots.some(slot => slot.items.some(Boolean)));
  const fallback: MenuItem[] = anything ? [] : [{ label: 'Reload window', icon: 'refresh', shortcut: 'Mod+R', run: () => location.reload() }];
  const development: MenuItem[] = platform.development ? [platform.inspect
    ? { label: 'Inspect element', icon: 'search', run: () => platform.inspect!() }
    : { label: 'Inspect element', icon: 'search', shortcut: 'Alt+right-click', disabled: 'Hold Option (Alt) and right-click for the developer menu' }] : [];
  const build = () => dedupeLabels(joinSections([copy, ...flat(), fallback, development]));
  return { items: build(), label: sections[0]?.label || 'Context menu', later: waiting.length ? Promise.all(waiting).then(build) : undefined };
}

function onContextMenu(event: MouseEvent) {
  const keyboard = keyboardEvents.has(event);
  const press = selectionAtPress; selectionAtPress = null;
  // The context-menu key and Shift+F10 also make the browser send its own
  // event after ours: one menu.
  if (!keyboard && event.isTrusted && performance.now() - lastSynthetic < 800 && state.menu) { event.preventDefault(); return; }
  if (event.defaultPrevented) return; // Monaco, or a component that opened its own.
  const target = elementOf(event.target);
  if (!target) return;
  if (platform.development && event.altKey) return;
  if (target.closest('[data-native-context-menu]') || isEditableTarget(target)) return;
  event.preventDefault();
  if (target.closest('.context-menu')) return;
  // WebKit selects the word under a right-click before the event: a menu is
  // about what you selected, not what the click selected for you.
  if (press === '' && !keyboard) document.getSelection()?.removeAllRanges();
  const { items, label, later } = menuForEvent(event, keyboard);
  const opened = openContextMenu({ x: event.clientX, y: event.clientY }, items, { keyboard, target, label });
  // Items that arrive later join the same menu, if it is still open.
  void later?.then(all => { if (state.menu?.openedAt === opened) emit({ menu: { ...state.menu, entries: normalizeMenu(all) } }); });
}

let lastSynthetic = -Infinity;
/** Shift+F10 or the context-menu key: a menu for the focused element, at it. */
function onKeyDown(event: KeyboardEvent) {
  if (!((event.key === 'F10' && event.shiftKey && !event.metaKey && !event.ctrlKey && !event.altKey) || event.key === 'ContextMenu')) return;
  const active = document.activeElement instanceof Element && document.activeElement !== document.body ? document.activeElement : null;
  const target = active || elementOf(document.getSelection()?.anchorNode || null) || document.body;
  if (target.closest('.monaco-editor') || isEditableTarget(target) || target.closest('.context-menu')) return;
  event.preventDefault(); event.stopPropagation();
  const rect = target.getBoundingClientRect();
  const width = document.documentElement.clientWidth, height = document.documentElement.clientHeight;
  const x = Math.min(Math.max(rect.left, 0) + Math.min(16, rect.width / 2), width - 8);
  const y = Math.min(Math.max(rect.top, 0) + Math.min(rect.height, 28), height - 8);
  const synthetic = new MouseEvent('contextmenu', { bubbles: true, cancelable: true, composed: true, clientX: x, clientY: y, button: 2, buttons: 0 });
  keyboardEvents.add(synthetic); lastSynthetic = performance.now();
  target.dispatchEvent(synthetic);
}

/** A long press on touch opens the menu where the finger is. */
function longPress() {
  let timer: ReturnType<typeof setTimeout> | undefined, start: { x: number; y: number; target: Element } | undefined, fired = -Infinity;
  const cancel = () => { clearTimeout(timer); timer = undefined; start = undefined; };
  const down = (event: PointerEvent) => {
    // Touch and pen only: a mouse has a right button (and some engines leave pointerType empty).
    if ((event.pointerType !== 'touch' && event.pointerType !== 'pen') || !event.isPrimary) return;
    const target = elementOf(event.target);
    if (!target || isEditableTarget(target) || target.closest('.context-menu, .monaco-editor')) return;
    start = { x: event.clientX, y: event.clientY, target };
    clearTimeout(timer);
    timer = setTimeout(() => {
      if (!start) return;
      const synthetic = new MouseEvent('contextmenu', { bubbles: true, cancelable: true, composed: true, clientX: start.x, clientY: start.y, button: 2 });
      lastSynthetic = fired = performance.now();
      start.target.dispatchEvent(synthetic); cancel();
    }, 550);
  };
  const move = (event: PointerEvent) => { if (start && Math.hypot(event.clientX - start.x, event.clientY - start.y) > 10) cancel(); };
  // The tap that ends a long press is not a click on what is under it.
  const click = (event: MouseEvent) => { if (performance.now() - fired < 700) { event.preventDefault(); event.stopPropagation(); fired = -Infinity; } };
  // A browser with its own long-press menu event wins; ours stands down.
  const native = (event: MouseEvent) => { if (event.isTrusted && timer) cancel(); };
  return { down, move, cancel, click, native };
}

// Rendering -------------------------------------------------------------------

/** Icons the shared set does not have, for menus only. */
const menuIcons: Record<string, string[]> = {
  copy: ['M9 9h10v11H9z', 'M5 15V4h10'],
  quote: ['M5 7h5v5H7c0 2 1 3 3 4', 'M14 7h5v5h-3c0 2 1 3 3 4'],
  link: ['M10 14a4 4 0 0 0 6 0l3-3a4 4 0 0 0-6-6l-1 1', 'M14 10a4 4 0 0 0-6 0l-3 3a4 4 0 0 0 6 6l1-1'],
  paste: ['M8 4h8v3H8z', 'M8 5H5v15h14V5h-3'],
  selectAll: ['M4 4h16v16H4z', 'M8 8h8v8H8z'],
  clear: ['M4 20h16', 'M7 16 17 6', 'M13 6h4v4'],
  stop: ['M7 7h10v10H7z'],
  play: ['M8 5v14l11-7z'],
  reveal: ['M3 7h7l2 2h9v10H3z', 'M12 13h5', 'M15 11l2 2-2 2'],
  close: ['M6 6l12 12', 'M18 6 6 18'],
};
function MenuIcon({ name }: { name?: string }) {
  if (!name) return null;
  if (menuIcons[name]) return <svg width={15} height={15} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">{menuIcons[name].map((d, i) => <path key={i} d={d}/>)}</svg>;
  return <Icon name={name} size={15}/>;
}

const shortcutLabel = (value: string) => keys(value).replace('right-click', isMacLike() ? 'Right-click' : 'right-click');
const isMacLike = () => typeof navigator !== 'undefined' && /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent);

/** Shows the open menu and the copy notice. Mount once per window. */
export function ContextMenuHost() {
  const current = useSyncExternalStore(subscribe, () => state);
  useEffect(() => installListeners(), []);
  const { menu, notice } = current;
  return <>
    {menu && createPortal(<MenuLayer key={menu.openedAt} menu={menu}/>, menu.container)}
    {notice && createPortal(<div className="context-menu-notice" role="status" key={notice.id}>{notice.text}</div>, document.querySelector('dialog:modal') || document.body)}
  </>;
}
const subscribe = (listener: () => void) => { listeners.add(listener); return () => { listeners.delete(listener); }; };

let installed = 0;
function installListeners() {
  if (installed++) return () => { installed--; };
  const press = longPress();
  // The press that opened a context menu is not also a click: on a Mac,
  // Ctrl+click sends `contextmenu` and then a `click` (WebKit), which would
  // open the link or press the button the menu is about.
  let menuPress = false;
  const pressMenu = (event: MouseEvent) => { if (event.isTrusted && !keyboardEvents.has(event)) menuPress = true; };
  const swallowClick = (event: MouseEvent) => {
    if (!menuPress) return;
    menuPress = false;
    if (event.button !== 0 || event.detail === 0) return; // a keyboard click is its own gesture
    event.preventDefault(); event.stopImmediatePropagation();
  };
  const pointerDown = (event: PointerEvent) => {
    menuPress = false;
    if (event.button === 2 && event.pointerType === 'mouse') selectionAtPress = document.getSelection()?.toString() || '';
    // Outside a menu, a press closes it and goes on to what it hit, but closes
    // nothing else: a dialog or popover under the menu stays open.
    if (state.menu && !(event.target instanceof Element && event.target.closest('.context-menu'))) { claimPress(event); closeContextMenu(false); }
    press.down(event);
  };
  // Scrolling the page closes a menu; content that scrolls by itself (a
  // streaming answer following its end) does not.
  const scroll = (event: Event) => { if (state.menu && !(event.target instanceof Element && event.target.closest('.context-menu'))) closeContextMenu(false); };
  const blur = () => closeContextMenu(false);
  const native = (event: MouseEvent) => press.native(event);
  window.addEventListener('contextmenu', native, true);
  window.addEventListener('contextmenu', pressMenu, true);
  window.addEventListener('contextmenu', onContextMenu);
  window.addEventListener('keydown', onKeyDown, true);
  window.addEventListener('pointerdown', pointerDown, true);
  window.addEventListener('pointermove', press.move, true);
  window.addEventListener('pointerup', press.cancel, true);
  window.addEventListener('pointercancel', press.cancel, true);
  window.addEventListener('click', press.click, true);
  window.addEventListener('click', swallowClick, true);
  window.addEventListener('wheel', scroll, { capture: true, passive: true });
  window.addEventListener('touchmove', scroll, { capture: true, passive: true });
  window.addEventListener('resize', blur);
  window.addEventListener('blur', blur);
  return () => {
    installed--;
    window.removeEventListener('contextmenu', native, true);
    window.removeEventListener('contextmenu', pressMenu, true);
    window.removeEventListener('contextmenu', onContextMenu);
    window.removeEventListener('keydown', onKeyDown, true);
    window.removeEventListener('pointerdown', pointerDown, true);
    window.removeEventListener('pointermove', press.move, true);
    window.removeEventListener('pointerup', press.cancel, true);
    window.removeEventListener('pointercancel', press.cancel, true);
    window.removeEventListener('click', press.click, true);
    window.removeEventListener('click', swallowClick, true);
    window.removeEventListener('wheel', scroll, true);
    window.removeEventListener('touchmove', scroll, true);
    window.removeEventListener('resize', blur);
    window.removeEventListener('blur', blur);
    press.cancel();
  };
}

/** The top-layer surface that holds the menu and its submenus. */
function MenuLayer({ menu }: { menu: OpenMenu }) {
  const layer = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    const node = layer.current!;
    try { if (!node.matches(':popover-open')) node.showPopover(); } catch { /* without popover support it is a fixed layer */ }
    return () => { try { node.hidePopover(); } catch { /* already hidden */ } };
  }, []);
  const run = (entry: Extract<MenuEntry, { kind: 'action' }>) => {
    // Focus goes back first, so an item that moves it (into the composer) wins.
    closeContextMenu(true);
    void (async () => { try { await entry.action.run?.(); } catch (error) { showMenuNotice(`${entry.action.label} failed: ${error instanceof Error ? error.message : String(error)}`); } })();
  };
  return <div ref={layer} className="context-menu-layer" popover="manual">
    <Menu entries={menu.entries} at={{ x: menu.x, y: menu.y }} origin={{ x: menu.x, y: menu.y }} keyboard={menu.keyboard} depth={0} label={menu.label} onRun={run} onClose={() => closeContextMenu(true)}/>
  </div>;
}

type MenuProps = {
  entries: MenuEntry[];
  /** The pointer, for the root menu. */
  at?: { x: number; y: number };
  /** The item a submenu belongs to. */
  anchor?: DOMRect;
  /** Where the root menu opened: a right-button release counts only away from it. */
  origin: { x: number; y: number };
  keyboard: boolean;
  depth: number;
  label?: string;
  onRun(entry: Extract<MenuEntry, { kind: 'action' }>): void;
  /** Close this menu: the whole menu at the root, back to the parent item in a submenu. */
  onClose(): void;
};

function Menu({ entries, at, anchor, origin, keyboard, depth, label, onRun, onClose }: MenuProps) {
  const element = useRef<HTMLDivElement>(null);
  const [active, setActive] = useState(() => keyboard ? firstEnabled(entries) : -1);
  const [open, setOpen] = useState<{ index: number; keyboard: boolean; rect: DOMRect }>();
  const [placed, setPlaced] = useState(false);
  const typed = useRef({ text: '', at: 0 });
  const pressed = useRef(-1), released = useRef(0);
  const hover = useRef<ReturnType<typeof setTimeout>>(undefined);
  useEffect(() => () => clearTimeout(hover.current), []);
  useLayoutEffect(() => {
    const node = element.current!;
    // The layer must be in the top layer before the menu can be measured
    // (a closed popover has no size); a child's effect runs before its parent's.
    const layer = node.closest<HTMLElement>('[popover]');
    if (layer && !layer.matches(':popover-open')) try { layer.showPopover(); } catch { /* a fixed layer without popover support */ }
    const size = { width: node.offsetWidth, height: node.offsetHeight };
    const viewport = { width: document.documentElement.clientWidth, height: document.documentElement.clientHeight };
    const place = anchor ? placeSubmenu(anchor, size, viewport) : placeMenu(at!, size, viewport);
    node.style.left = `${place.left}px`; node.style.top = `${place.top}px`;
    setPlaced(true);
  }, [entries.length]);
  // The keyboard and the pointer move the same highlight; focus follows it.
  useEffect(() => {
    if (!placed) return;
    const node = element.current!;
    const item = active >= 0 ? node.querySelector<HTMLElement>(`[data-index="${active}"]`) : null;
    (item || node).focus({ preventScroll: true });
    item?.scrollIntoView({ block: 'nearest' });
  }, [active, placed]);
  const itemRect = (index: number) => element.current!.querySelector<HTMLElement>(`[data-index="${index}"]`)!.getBoundingClientRect();
  const openSubmenu = (index: number, byKeyboard: boolean) => {
    const entry = entries[index];
    if (entry?.kind !== 'action' || !entry.submenu || entry.action.disabled) return;
    setOpen({ index, keyboard: byKeyboard, rect: itemRect(index) });
  };
  const activate = (index: number, byKeyboard: boolean) => {
    const entry = entries[index];
    if (entry?.kind !== 'action' || entry.action.disabled) return;
    if (entry.submenu) { setActive(index); openSubmenu(index, byKeyboard); return; }
    onRun(entry);
  };
  const keydown = (event: ReactKeyboardEvent) => {
    if (event.target !== element.current && !(event.target instanceof Element && event.target.parentElement === element.current)) return;
    const move = (to: number) => { event.preventDefault(); event.stopPropagation(); if (to >= 0) { setActive(to); setOpen(undefined); } };
    switch (event.key) {
      case 'ArrowDown': move(stepFocus(entries, active, 1)); return;
      case 'ArrowUp': move(stepFocus(entries, active, -1)); return;
      case 'Home': case 'PageUp': move(firstEnabled(entries)); return;
      case 'End': case 'PageDown': move(lastEnabled(entries)); return;
      case 'ArrowRight': event.preventDefault(); event.stopPropagation(); if (active >= 0) openSubmenu(active, true); return;
      case 'ArrowLeft': event.preventDefault(); event.stopPropagation(); if (depth > 0) onClose(); return;
      case 'Escape': event.preventDefault(); event.stopPropagation(); if (depth > 0) onClose(); else closeContextMenu(true); return;
      case 'Tab': event.preventDefault(); event.stopPropagation(); closeContextMenu(true); return;
      case 'Enter': case ' ':
        event.preventDefault(); event.stopPropagation();
        if (event.key === ' ' && typed.current.text && performance.now() - typed.current.at < 600) { find(event.key); return; }
        if (active >= 0) activate(active, true);
        return;
    }
    if (event.key.length === 1 && !event.metaKey && !event.ctrlKey && !event.altKey) { event.preventDefault(); event.stopPropagation(); find(event.key); }
  };
  const find = (key: string) => {
    const now = performance.now();
    typed.current = { text: (now - typed.current.at < 600 ? typed.current.text : '') + key, at: now };
    const index = typeahead(entries, typed.current.text, active);
    if (index >= 0) { setActive(index); setOpen(undefined); }
  };
  const child = open && entries[open.index]?.kind === 'action' ? entries[open.index] as Extract<MenuEntry, { kind: 'action' }> : undefined;
  return <>
    <div ref={element} className="context-menu" role="menu" tabIndex={-1} aria-label={label || 'Context menu'} aria-orientation="vertical" data-depth={depth} style={{ visibility: placed ? 'visible' : 'hidden' }}
      onKeyDown={keydown} onContextMenu={event => event.preventDefault()}>
      {entries.map((entry, index) => {
        if (entry.kind === 'separator') return <div key={index} role="separator" className="context-menu-separator"/>;
        const { action } = entry, reason = disabledReason(action);
        return <div key={index} data-index={index} tabIndex={-1}
          role={action.checked !== undefined ? 'menuitemcheckbox' : 'menuitem'} aria-checked={action.checked} aria-disabled={action.disabled ? true : undefined}
          aria-haspopup={entry.submenu ? 'menu' : undefined} aria-expanded={entry.submenu ? open?.index === index : undefined}
          aria-label={action.label} title={reason} aria-description={reason || (action.shortcut ? shortcutLabel(action.shortcut) : undefined)}
          className={'context-menu-item' + (action.danger ? ' danger' : '') + (active === index ? ' is-active' : '')}
          onPointerMove={() => {
            if (active !== index) setActive(action.disabled ? -1 : index);
            clearTimeout(hover.current);
            if (entry.submenu && open?.index !== index) hover.current = setTimeout(() => openSubmenu(index, false), 120);
            else if (!entry.submenu && open) hover.current = setTimeout(() => setOpen(undefined), 200);
          }}
          onPointerDown={event => { if (event.button === 0) pressed.current = index; }}
          onPointerUp={event => {
            // An item acts on release, as in a native menu: a left press and
            // release on it, or a right press that opened the menu, moved here
            // and released. Releasing a right-click where it opened the menu
            // (held a moment, as a trackpad click often is) chooses nothing.
            const left = event.button === 0 && pressed.current === index;
            pressed.current = -1;
            const right = event.button === 2 && rightReleaseChooses(origin, { x: event.clientX, y: event.clientY });
            if (left || right) { released.current = performance.now(); activate(index, false); }
          }}
          // Clicks that come without a pointer (assistive technology, scripts).
          onClick={() => { if (performance.now() - released.current > 500) activate(index, false); }}>
          <span className="context-menu-icon">{action.checked ? <Icon name="check" size={14}/> : <MenuIcon name={action.icon}/>}</span>
          <span className="context-menu-label">{action.label}</span>
          {action.shortcut ? <kbd className="context-menu-shortcut">{shortcutLabel(action.shortcut)}</kbd> : action.note && <small className="context-menu-shortcut">{action.note}</small>}
          {entry.submenu && <span className="context-menu-more" aria-hidden="true"><Icon name="chevronRight" size={13}/></span>}
        </div>;
      })}
    </div>
    {child?.submenu && open && <Menu key={open.index} entries={child.submenu} anchor={open.rect} origin={origin} keyboard={open.keyboard} depth={depth + 1} label={child.action.label} onRun={onRun}
      onClose={() => { const index = open.index; setOpen(undefined); setActive(index); requestAnimationFrame(() => element.current?.querySelector<HTMLElement>(`[data-index="${index}"]`)?.focus({ preventScroll: true })); }}/>}
  </>;
}

// The first artifact menus' interface --------------------------------------------

/** An item in the shape the artifact menus were written with. */
export type ContextMenuItem = { label: string; icon?: string; hint?: string; disabled?: boolean; danger?: boolean; separatorBefore?: boolean; run(): void };
export type ContextMenuState = { x: number; y: number; items: ContextMenuItem[]; label: string; returnFocus?: HTMLElement | null };

/**
 * Shows `menu` as a Lazybox context menu while mounted, for code that keeps
 * its own menu state (the Artifacts panel). New code declares a section with
 * `contextMenu()` or contributes to a scope instead.
 */
export function ContextMenu({ menu, onClose }: { menu: ContextMenuState; onClose(): void }) {
  const close = useRef(onClose); close.current = onClose;
  useEffect(() => {
    let open = true;
    const items: MenuItem[] = menu.items.flatMap(item => [item.separatorBefore ? 'separator' as const : null, { label: item.label, icon: item.icon, note: item.hint, disabled: item.disabled, danger: item.danger, run: item.run }]);
    openContextMenu({ x: menu.x, y: menu.y }, items, { label: menu.label, keyboard: !!menu.returnFocus && document.activeElement === menu.returnFocus, restore: menu.returnFocus ?? undefined, onClose: () => { if (open) { open = false; close.current(); } } });
    return () => { if (open) { open = false; closeContextMenu(false); } };
  }, [menu]);
  return null;
}

/** Where a context menu opens: the pointer, or below the element for the keyboard. */
export function menuPoint(event: { clientX: number; clientY: number; currentTarget: Element; target: EventTarget }): { x: number; y: number } {
  if (event.clientX || event.clientY) return { x: event.clientX, y: event.clientY };
  const bounds = (event.target instanceof Element ? event.target : event.currentTarget).getBoundingClientRect();
  return { x: bounds.left + 12, y: bounds.top + Math.min(bounds.height, 28) };
}
