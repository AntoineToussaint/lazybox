import { selectPanelText } from './panel-selection';
import { recordMaximize } from './training-wheels';
import { forwardRef, useEffect, useImperativeHandle, useLayoutEffect, useRef, useState, type ReactNode } from 'react';
import { createPortal } from 'react-dom';
import { createDockview, type DockviewApi, type DockviewGroupPanel, type DockviewTheme, type IDockviewPanel } from 'dockview';
import 'dockview/dist/styles/dockview.css';
import './dock.css';
import { iconElement } from './Icons';
import { PendingFocus, trackFocus } from './pending-focus';
import { focusWhenShown } from './focus';
import { beginDrag, type DragAxis } from './drag-gesture';
import { reclaimPopoutViews, refreshNativeWebViews } from './native-web-view';
import { EMPTY_CONTENT, fromDockview, groupStates, needsEmptyContent, paramsOf, PANEL_COMPONENT, sameSizes, structureKey, TAB_COMPONENT, toDockview, type Overlay, type PanelParams } from './dock-bridge';
import { clampSidebar, SIDEBAR_MAX, SIDEBAR_MIN } from './PanelLayout';
import { fitWidth, groups, isSidebar, minWidthOf, sidebarAt, sidebarKinds, type Group, type LayoutNode, type Panel, type WorkspacePage } from './workspace-model';
import type { LayoutDirection } from './actions';
import { groupRects, neighbourIn, resizeEdge, resizeStep, shareOf, tabAt } from './dock-keyboard';
import './dock-keyboard.css';
import { attachPopoutTitleBar } from './title-bar';

type Arranged = Pick<WorkspacePage, 'layout' | 'floating' | 'popouts'>;
/** What a group's header actions and menus need to know about it. */
export type GroupView = {
  id: string;
  panels: Panel[];
  active?: Panel;
  location: 'grid' | 'floating' | 'popout';
  maximized: boolean;
  minimized: boolean;
  /** In a column, where minimizing to the tab bar makes sense. */
  canMinimize: boolean;
};
export type MenuItem = { label: string; icon?: string; action(): void; disabled?: boolean; danger?: boolean; separator?: boolean };
export type DockHandle = {
  toggleMaximize(panelID?: string, source?: 'pointer' | 'keys'): void;
  activePanelID(): string | undefined;
  activeGroup(): GroupView | undefined;
  /** Moves the front tab of the active group into a new group beside it; false when it is alone. */
  splitActive(side: 'right' | 'below'): boolean;
  cycleGroups(step: 1 | -1): void;
  /** The next (or previous) tab of the active group; past its last, the next group's first (its last, going back). */
  cycleTabs(step: 1 | -1): void;
  /** Opens the panel's group in its own window (same page process: nothing remounts). */
  popout(panelID: string): Promise<boolean>;
  float(panelID: string): void;
  /** A floating or popped-out group back into the page; a popped-out one goes where it was, and the page comes forward on it. */
  dockBack(groupID: string): Promise<void>;
  /** Brings a popped-out group's window to the front. */
  focusWindow(groupID: string): void;
  /** The popped-out group whose window has the focus, if any. */
  focusedPopout(): string | undefined;
  focusPanel(panelID: string): void;
  /** Whether the dock shows this panel yet (a focus asked for it waits until it does). */
  hasPanel(panelID: string): boolean;
  /** The group a panel is in, as its menus see it. */
  groupOf(panelID: string): GroupView | undefined;
  /** Brings a panel's tab to the front without moving focus into it. */
  focusTab(panelID: string): void;
  // From the keyboard (dock-keyboard.ts). Each shows the focus ring and says what happened to a screen reader.
  /** Focus moves to the neighbouring group; while one is maximized, the zoom moves with it. */
  focusDirection(direction: LayoutDirection): void;
  /** The docked group beside the active one in a direction, if any. */
  neighbour(direction: LayoutDirection): string | undefined;
  /** Moves the active group's edge (tmux style); `repeat` while the key is held, which speeds it up. */
  resize(direction: LayoutDirection, repeat: boolean): void;
  /** Tab `slot` (1–8, 9 the last) of the active group to the front, with the focus. */
  selectTab(slot: number): void;
  /** A panel shown because the person navigated to it: while a group is maximized, the zoom moves to its group. */
  reveal(panelID: string): void;
  /** Says it to a screen reader (politely), and shows the focus ring; `edge` nudges the ring that way (nothing there). */
  announce(text: string, edge?: LayoutDirection): void;
  /** Leaves maximize, if a group is maximized. */
  restore(): void;
};
type Props = {
  page: WorkspacePage;
  hostFor(panel: Panel): HTMLElement;
  titleOf(panel: Panel): string;
  iconOf(panel: Panel): string;
  /** An image a panel shows for itself (a web page's icon), in place of its kind's icon. */
  imageOf?(panel: Panel): string | undefined;
  /** A badge on a panel's tab ("3 findings", a live dot), beside its title. */
  badgeOf?(panel: Panel): { text: string; tone: string; label: string } | undefined;
  canClose(panel: Panel): boolean;
  /** The agent icon shown in place of the generic chat icon. */
  agentOf?(panel: Panel): { icon: string; label: string } | undefined;
  /** A conversation tab's status (working, needs you, failed, done), as a dot before its title. */
  statusOf?(panel: Panel): { state: string; label: string } | undefined;
  /** A tab double-clicked: true when the page handled it (a preview tab kept open); otherwise it maximizes. */
  onTabDoubleClick?(panel: Panel): boolean;
  /** What closing it does, for its × ("Close Changes page" when it is the last panel of a page that goes with it). */
  closeLabel?(panel: Panel, title: string): string;
  sidebarWidth: number;
  /** An explicit navigation still being applied when panels must fit as tabs. */
  requestedPanel: React.RefObject<string>;
  onSidebarWidth(width: number): void;
  /** The person rearranged the dock: drag, drop, resize, tab switch, float, pop out. */
  onArrange(next: Arranged): void;
  /** The page's saved layout is held (it could not be read): it is drawn as it is, and nothing is reported back. */
  locked?: boolean;
  onClose(panel: Panel): void;
  /** A minimized group's tab was clicked. */
  onRestore(groupID: string): void;
  renderActions(group: GroupView, handle: DockHandle): ReactNode;
  /** Right after a group's tabs: its `+` (a new conversation, or any panel opened here). */
  renderAdd?(group: GroupView, handle: DockHandle): ReactNode;
  /** A key pressed in a popped-out group's window (its document is not this one: the page's keys do not see it). */
  onPopoutKey?(event: KeyboardEvent, groupID: string): void;
  onPopoutError(message: string): void;
  popoutUrl: string;
  /** Whether a Web panel can go into a popped-out window with its page: native views that come home (the desktop app). A browser's frame would reload there. */
  webPopouts?: boolean;
  empty: ReactNode;
};

const theme: DockviewTheme = { name: 'lazybox', className: 'dockview-theme-lazybox', colorScheme: 'dark', gap: 0, dndOverlayMounting: 'absolute', dndPanelOverlay: 'group' };
/** A tab bar's height (see dock.css): a minimized group keeps exactly this. */
export const TAB_BAR = 32;
const BIG = Number.MAX_SAFE_INTEGER;
const panelFrom = (id: string, params: unknown): Panel => { const value = (params || {}) as Partial<PanelParams>; return { id, kind: value.kind || 'unknown', binding: value.binding || { mode: 'follow' }, detached: !!value.detached, ...(value.preview ? { preview: true } : {}) }; };
/** A tab's hover text: what it is, and what dragging and double-clicking it do. */
const tabHint = (title: string, preview: boolean) => preview ? `${title} · preview: the next conversation you choose replaces it · double-click to keep it open` : `${title} · drag to move, double-click to maximize`;
const sameArrangement = (a: Arranged, b: Arranged) => structureKey(a) === structureKey(b) && sameSizes(a.layout, b.layout) && [...groupStates(a).entries()].every(([id, group]) => groupStates(b).get(id)?.active === group.active)
  && a.floating.every((value, index) => ['left', 'top', 'right', 'bottom', 'width', 'height'].every(key => Math.abs((value.box[key as 'width'] ?? 0) - (b.floating[index]?.box[key as 'width'] ?? 0)) <= 2));
const overlayOf = (page: Arranged): Overlay => {
  const minimized = new Map<string, number>();
  const visit = (node: LayoutNode | null) => {
    if (node?.type !== 'split') return;
    node.children.forEach((child, index) => { if (node.direction === 'column' && child.type === 'group' && child.minimized) minimized.set(child.id, node.sizes[index]); visit(child); });
  };
  visit(page.layout);
  return { minimized };
};
/** Every group's parent direction in the docked tree. */
const parents = (node: LayoutNode | null, into = new Map<string, 'row' | 'column'>()) => {
  if (node?.type === 'split') for (const child of node.children) { if (child.type === 'group') into.set(child.id, node.direction); parents(child, into); }
  return into;
};

/**
 * The page's sidebar and its side: at an edge of the root row, or alone
 * (beside the empty content area). It is one group, or Workspaces stacked
 * over Agents: `group` is its first (its width is the sidebar's), `ids` all.
 */
const sidebarOf = (layout: LayoutNode | null): { group: Group; ids: string[]; at: 'first' | 'last' } | undefined => {
  const of = (node: LayoutNode, at: 'first' | 'last') => { const all = groups(node); return { group: all[0], ids: all.map(group => group.id), at }; };
  if (layout && isSidebar(layout)) return of(layout, 'first');
  const at = sidebarAt(layout);
  return at && layout?.type === 'split' ? of(layout.children[at === 'first' ? 0 : layout.children.length - 1], at) : undefined;
};

/**
 * The page's dock, drawn by dockview: drag a tab onto any panel's edge to
 * split it or onto its middle to join its tabs, with a live preview; drag
 * dividers; float a group over the page or pop it out into its own window.
 *
 * Each panel's content is its stable host element (see usePanelHosts), handed
 * to dockview as is and rendered with the "always" renderer, so neither a
 * move, a tab switch, nor a rebuild of the arrangement remounts or reparents
 * a panel. The saved arrangement (`page`) is the source of truth: dockview is
 * brought to it in place when it changes, and every rearrangement made in
 * dockview is reported back through `onArrange`.
 */
export const DockLayout = forwardRef<DockHandle, Props>(function DockLayout(props, ref) {
  const [intoFocus] = useState(() => new PendingFocus<{ panel: string }>());
  useEffect(() => trackFocus(intoFocus), [intoFocus]);
  const container = useRef<HTMLDivElement>(null);
  const api = useRef<DockviewApi>(undefined);
  const pendingPopouts = useRef(new Map<string, Promise<boolean>>());
  const latest = useRef(props); latest.current = props;
  /** Set while this component changes dockview, so the change is not reported back as the person's. */
  const applying = useRef(0);
  const applied = useRef<Arranged>(undefined);
  /** Each panel's content element in dockview, which holds the panel's host. */
  const contents = useRef(new Map<string, HTMLElement>());
  /** Each tab's close button, whose label follows what closing does. */
  const closes = useRef(new Map<string, HTMLButtonElement>());
  const badges = useRef(new Map<string, HTMLElement>());
  /** A tab's badge as its panel says now: shown with its text and tone, or hidden. */
  const paintBadge = (id: string, panel: Panel) => {
    const element = badges.current.get(id);
    if (!element) return;
    const badge = latest.current.badgeOf?.(panel);
    element.hidden = !badge;
    if (!badge) return;
    if (element.textContent !== badge.text) element.textContent = badge.text;
    if (element.dataset.tone !== badge.tone) element.dataset.tone = badge.tone;
    if (element.title !== badge.label) { element.title = badge.label; element.setAttribute('aria-label', badge.label); }
  };
  /** Each tab, whose preview mark and status dot follow the page without a rebuild. */
  const tabs = useRef(new Map<string, { tab: HTMLElement; status: HTMLElement; paintIcon(panel: Panel): void }>());
  const size = useRef({ width: 0, height: 0 });
  const [actionHosts, setActionHosts] = useState(() => new Map<string, HTMLElement>());
  /** Where each group's `+` is drawn, right after its tabs. */
  const [addHosts, setAddHosts] = useState(() => new Map<string, HTMLElement>());
  const layoutRoot = useRef<HTMLDivElement>(null);
  /** The ring around the active group: shown while the person works the dock from the keyboard, gone at the first click. */
  const [ring, setRing] = useState(false);
  const [bumped, setBumped] = useState<{ edge: LayoutDirection; at: number }>();
  /** What the last keyboard action did, for a screen reader (the zero-width tail makes a repeat heard again). */
  const [said, setSaid] = useState('');
  /** The group the last focus move left, so the opposite arrow comes back to it. */
  const cameFrom = useRef<{ group: string; direction: LayoutDirection }>(undefined);
  /** The element each panel last had the focus on. */
  const lastFocus = useRef(new Map<string, HTMLElement>());
  /** Consecutive held-key resizes, which speed up. */
  const streak = useRef(0);
  /** While a resize key is held, what it did is said once it settles. */
  const held = useRef(0);
  // The first click anywhere puts the ring away: the pointer shows where the person is.
  useEffect(() => {
    const away = () => setRing(false);
    window.addEventListener('pointerdown', away, true);
    return () => { window.removeEventListener('pointerdown', away, true); clearTimeout(held.current); };
  }, []);
  /** Where the empty content area (`EMPTY_CONTENT`) draws the page's empty state. */
  const [emptyHost, setEmptyHost] = useState<HTMLElement>();
  const [, setVersion] = useState(0);
  const bump = () => setVersion(value => value + 1);

  const geometry = () => ({ width: size.current.width, height: size.current.height, sidebarWidth: latest.current.sidebarWidth, header: TAB_BAR, title: latest.current.titleOf });
  /** A tab as its panel is now: a preview tab's title in italics, a conversation's status dot, the hover text. */
  const decorateTab = (id: string, panel: Panel, title: string) => {
    const found = tabs.current.get(id);
    if (!found) return;
    const preview = !!panel.preview, status = latest.current.statusOf?.(panel);
    found.tab.toggleAttribute('data-preview', preview);
    found.status.hidden = !status;
    if (status && found.status.dataset.state !== status.state) found.status.dataset.state = status.state;
    found.status.setAttribute('aria-hidden', 'true');
    found.paintIcon(panel);
    const hint = tabHint(title, preview) + (status ? ` · ${status.label}` : '');
    if (found.tab.title !== hint) found.tab.title = hint;
  };
  const read = (): Arranged | undefined => api.current ? fromDockview(api.current.toJSON(), overlayOf(latest.current.page)) : undefined;
  const silently = (change: () => void) => { applying.current++; try { change(); } finally { applying.current--; } };

  /** Sidebar width, minimized groups, and every other group's limits, from the saved arrangement. */
  const applyConstraints = () => {
    const dock = api.current, page = latest.current.page;
    if (!dock) return;
    const found = sidebarOf(page.layout), sidebar = found?.group;
    const direction = parents(page.layout), states = groupStates(page);
    silently(() => {
      for (const group of dock.groups) {
        const saved = states.get(group.id), minimized = !!saved?.minimized && direction.get(group.id) === 'column';
        // A stacked sidebar (Workspaces over Agents) gets no maximum width: with one, dockview capped the
        // whole grid at it after some rebuilds (a sidebar-right page, then a sidebar-left one;
        // test-sidebar-stability). Its width still stays within SIDEBAR_MAX: a drag past it is clamped
        // (clampSidebar) and set back by this function.
        if (found?.ids.includes(group.id)) group.api.setConstraints({ minimumWidth: SIDEBAR_MIN, maximumWidth: found.ids.length > 1 ? BIG : SIDEBAR_MAX, minimumHeight: 120, maximumHeight: BIG });
        else {
          // No narrower than its widest panel needs (a conversation 360px, Files 200px…).
          const minimumWidth = Math.max(160, ...(saved?.panels || []).map(panel => minWidthOf(panel.kind)));
          if (minimized) group.api.setConstraints({ minimumHeight: TAB_BAR, maximumHeight: TAB_BAR, minimumWidth, maximumWidth: BIG });
          else group.api.setConstraints({ minimumWidth, minimumHeight: 96, maximumWidth: BIG, maximumHeight: BIG });
        }
        group.element.classList.toggle('dock-group-minimized', minimized);
        // Its content is drawn over the page by dockview, outside the group: hide it there too.
        for (const panel of group.panels) contents.current.get(panel.id)?.classList.toggle('dock-content-hidden', minimized);
        group.element.dataset.groupId = group.id;
      }
      if (sidebar) {
        const group = dock.getGroup(sidebar.id);
        if (group && Math.abs((group.api.width ?? 0) - latest.current.sidebarWidth) > 1) group.api.setSize({ width: latest.current.sidebarWidth });
      }
      for (const group of dock.groups) {
        const saved = states.get(group.id);
        if (saved?.minimized && direction.get(group.id) === 'column' && Math.abs(group.api.height - TAB_BAR) > 1) group.api.setSize({ height: TAB_BAR });
      }
    });
  };
  /** A minimized group restored to the share it had. */
  const restoreSizes = (before: Arranged | undefined) => {
    const dock = api.current, page = latest.current.page;
    if (!dock || !before) return;
    const was = groupStates(before), now = groupStates(page), direction = parents(page.layout);
    const visit = (node: LayoutNode | null) => {
      if (node?.type !== 'split') return;
      node.children.forEach((child, index) => {
        if (child.type !== 'group' || !was.get(child.id)?.minimized || now.get(child.id)?.minimized || direction.get(child.id) !== 'column') return;
        const group = dock.getGroup(child.id) as DockviewGroupPanel | undefined, column = group?.element.closest('.dv-split-view-container');
        const extent = column?.getBoundingClientRect().height || size.current.height;
        silently(() => group?.api.setSize({ height: Math.round(extent * node.sizes[index]) }));
      });
      node.children.forEach(visit);
    };
    visit(page.layout);
  };

  /** Brings dockview to the saved arrangement, in place where it can. */
  /** Panels that would be crushed below their minimum width become tabs instead (reported, then drawn). */
  const fits = () => {
    const page = latest.current.page, fitted = fitWidth(page.layout, size.current.width, latest.current.sidebarWidth, latest.current.requestedPanel.current);
    if (fitted === page.layout || latest.current.locked) return true;
    latest.current.onArrange({ layout: fitted, floating: page.floating, popouts: page.popouts });
    return false;
  };
  const reconcile = () => {
    const dock = api.current, target = latest.current.page;
    if (!dock || !size.current.width || !size.current.height) return;
    if (!fits()) return;
    const before = applied.current, current = read()!;
    const popped = current.popouts.length > 0 || target.popouts.length > 0;
    // A maximized group stays maximized through any change of the arrangement (a rebuild, a
    // tab brought forward in another group): only the person, or navigation, moves the zoom.
    const zoomed = dock.hasMaximizedGroup() ? dock.groups.find(group => group.api.isMaximized())?.id : undefined;
    if (structureKey(current) !== structureKey(target) || (!popped && !sameSizes(current.layout, target.layout))) {
      // A rebuild with the same panels keeps their content where it is; a window
      // of its own would close, so then only what changed is done in place.
      if (!popped || !before) {
        // A rebuild hides the panels' overlays for a frame, which drops the focus inside
        // one: whatever had it (a message box being typed in) gets it back once drawn.
        const focused = document.activeElement instanceof HTMLElement && document.activeElement.closest('.dock-panel-content') ? document.activeElement : undefined;
        // The active group (the target of keyboard commands) stays the active one, too.
        const activeGroup = dock.activeGroup?.id;
        silently(() => dock.fromJSON(toDockview(target, geometry()), { reuseExistingPanels: true }));
        silently(() => { const group = activeGroup ? dock.getGroup(activeGroup) : undefined; if (group && dock.activeGroup !== group) group.api.setActive(); });
        if (focused && document.activeElement !== focused) focusWhenShown(focused, { preventScroll: true });
      }
      else incremental(dock, target);
    }
    silently(() => {
      const states = groupStates(target);
      for (const panel of dock.panels) {
        const saved = [...states.values()].flatMap(group => group.panels).find(value => value.id === panel.id);
        if (!saved) continue;
        const params = paramsOf(saved);
        if (JSON.stringify(panel.params) !== JSON.stringify(params)) panel.api.updateParameters(params);
        const title = latest.current.titleOf(saved);
        if (panel.title !== title) panel.api.setTitle(title);
      }
      for (const [id, group] of states) {
        const live = dock.getGroup(id);
        if (live && live.activePanel?.id !== group.active) dock.getPanel(group.active)?.api.setActive();
      }
      if (zoomed && !dock.hasMaximizedGroup()) {
        const group = dock.getGroup(zoomed);
        if (group?.activePanel && group.api.location.type === 'grid' && dock.groups.filter(value => value.api.location.type === 'grid').length > 1) dock.maximizeGroup(group.activePanel);
      }
    });
    // A panel kept by id whose kind changed (an imported layout) shows its new host.
    for (const group of groupStates(target).values()) for (const panel of group.panels) {
      const content = contents.current.get(panel.id), host = latest.current.hostFor(panel);
      if (content && host.parentNode !== content) content.replaceChildren(host);
    }
    emptyContent(dock, target);
    applied.current = target;
    applyConstraints();
    restoreSizes(before);
    decorateSashes();
    bump();
    // Anything dockview could not place exactly is reported back as it now is.
    const after = read();
    if (after && !sameArrangement(after, target)) latest.current.onArrange(after);
  };
  /**
   * The empty content area beside a sidebar left alone: added or removed in
   * place as the page needs it. It has no tab bar and takes no drop (a panel
   * dropped in it would hide under its missing tabs): panels dock beside it.
   */
  const emptyContent = (dock: DockviewApi, target: Arranged) => {
    const placeholder = dock.getPanel(EMPTY_CONTENT), need = needsEmptyContent(target);
    silently(() => {
      if (need && !placeholder) {
        // Right of a sidebar group; a stacked sidebar takes the whole left, so right of everything.
        const found = sidebarOf(target.layout), sidebar = found?.ids.length === 1 ? found.group.id : undefined;
        dock.addPanel({ id: EMPTY_CONTENT, component: PANEL_COMPONENT, tabComponent: TAB_COMPONENT, title: '', position: sidebar && dock.getGroup(sidebar) ? { referenceGroup: sidebar, direction: 'right' } : { direction: 'right' } });
      }
      if (!need && placeholder) dock.removePanel(placeholder);
      const group = dock.getPanel(EMPTY_CONTENT)?.group;
      if (group) { group.header.hidden = true; group.locked = 'no-drop-target'; group.element.classList.add('dock-group-empty-content'); }
    });
  };
  /** Adds and removes panels without a rebuild, while a group is in its own window. */
  const incremental = (dock: DockviewApi, target: Arranged) => {
    const want = new Map([...groupStates(target).values()].flatMap(group => group.panels.map(panel => [panel.id, group] as const)));
    silently(() => {
      for (const panel of [...dock.panels]) if (!want.has(panel.id) && panel.id !== EMPTY_CONTENT) dock.removePanel(panel);
      const have = new Set(dock.panels.map(panel => panel.id));
      for (const [id, group] of want) {
        if (have.has(id)) continue;
        const saved = group.panels.find(panel => panel.id === id)!;
        const options = { id, component: PANEL_COMPONENT, tabComponent: TAB_COMPONENT, title: latest.current.titleOf(saved), params: paramsOf(saved) };
        if (dock.getGroup(group.id)) { dock.addPanel({ ...options, position: { referenceGroup: group.id } }); continue; }
        const near = neighbour(target.layout, group.id, new Set(dock.groups.map(value => value.id)));
        dock.addPanel(near ? { ...options, position: { referenceGroup: near.group, direction: near.direction } } : { ...options, position: { direction: 'right' } });
        have.add(id);
      }
    });
  };

  /** Keyboard and screen reader access to every divider: arrows resize, the sidebar's edge in pixels. */
  const decorateSashes = () => {
    const dock = api.current, root = container.current;
    if (!dock || !root) return;
    const page = latest.current.page, sidebarIDs = sidebarOf(page.layout)?.ids || [];
    // A view is the sidebar when it holds exactly the sidebar's groups (all of a stacked one).
    const isSidebarView = (found: { id: string }[]) => found.length === sidebarIDs.length && found.length > 0 && found.every(group => sidebarIDs.includes(group.id));
    for (const sash of root.querySelectorAll<HTMLElement>('.dv-sash')) {
      const split = sash.closest('.dv-split-view-container'), views = split ? [...split.querySelectorAll<HTMLElement>(':scope > .dv-view-container > .dv-view')] : [];
      const index = [...(sash.parentElement?.children || [])].indexOf(sash);
      const first = views[index], second = views[index + 1];
      if (!first || !second) continue;
      const vertical = split!.classList.contains('dv-horizontal');
      const groupIn = (view: HTMLElement) => dock.groups.filter(group => view.contains(group.element));
      const label = (view: HTMLElement) => groupIn(view).map(group => group.activePanel?.title).filter(Boolean).join(', ') || 'panel';
      const sidebar = isSidebarView(groupIn(first)) ? 'first' : isSidebarView(groupIn(second)) ? 'second' : '';
      const extent = (view: HTMLElement) => vertical ? view.getBoundingClientRect().width : view.getBoundingClientRect().height;
      sash.tabIndex = 0;
      sash.setAttribute('role', 'separator');
      sash.setAttribute('aria-orientation', vertical ? 'vertical' : 'horizontal');
      if (sidebar) {
        sash.setAttribute('aria-label', 'Resize Workspaces sidebar');
        sash.setAttribute('aria-valuemin', String(SIDEBAR_MIN)); sash.setAttribute('aria-valuemax', String(SIDEBAR_MAX)); sash.setAttribute('aria-valuenow', String(latest.current.sidebarWidth));
      } else {
        sash.setAttribute('aria-label', `Resize ${label(first)} and ${label(second)}`);
        sash.setAttribute('aria-valuemin', '5'); sash.setAttribute('aria-valuemax', '95'); sash.setAttribute('aria-valuenow', String(Math.round(100 * extent(first) / Math.max(1, extent(first) + extent(second)))));
      }
      sash.onkeydown = event => {
        const step = vertical ? { ArrowLeft: -1, ArrowRight: 1 }[event.key] : { ArrowUp: -1, ArrowDown: 1 }[event.key];
        if (!step) return;
        event.preventDefault();
        if (sidebar) { latest.current.onSidebarWidth(clampSidebar(latest.current.sidebarWidth + (sidebar === 'first' ? step : -step) * 16)); return; }
        const target = groupIn(first)[0], amount = Math.max(16, Math.round((extent(first) + extent(second)) * 0.02));
        target?.api.setSize(vertical ? { width: Math.round(extent(first) + step * amount) } : { height: Math.round(extent(first) + step * amount) });
        requestAnimationFrame(() => decorateSashes());
      };
    }
  };

  useLayoutEffect(() => {
    const element = container.current!;
    const dock = createDockview(element, {
      theme, dndStrategy: 'pointer', defaultRenderer: 'always', disableAutoResizing: true, popoutUrl: latest.current.popoutUrl,
      groupIdGenerator: () => `g-${crypto.randomUUID().slice(0, 12)}`,
      createComponent: ({ id }) => {
        if (id === EMPTY_CONTENT) {
          const area = document.createElement('div');
          area.className = 'dock-empty-content';
          return { element: area, init: () => setEmptyHost(area), dispose: () => setEmptyHost(host => host === area ? undefined : host) };
        }
        const content = document.createElement('div');
        content.className = 'dock-panel-content';
        content.dataset.dockPanel = id;
        // Working in a panel makes its group the active one (the target of keyboard commands).
        const activate = () => { const panel = api.current?.getPanel(id); if (panel && api.current?.activeGroup !== panel.group) silently(() => panel.group.api.setActive()); };
        content.addEventListener('pointerdown', activate, true);
        // Panels are moved with pointer events; an HTML5 drag over a panel is the
        // panel's own (files into the chat, text into a field) and must not reach
        // dockview's drop targets, which would cancel it.
        for (const type of ['dragenter', 'dragover', 'dragleave', 'drop']) content.addEventListener(type, event => event.stopPropagation());
        content.addEventListener('focusin', activate);
        // Where the person was in it, so coming back by keyboard lands there again.
        content.addEventListener('focusin', event => { if (event.target instanceof HTMLElement) lastFocus.current.set(id, event.target); });
        contents.current.set(id, content);
        return {
          element: content,
          init: parameters => { const host = latest.current.hostFor(panelFrom(id, parameters.params)); if (host.parentNode !== content) content.replaceChildren(host); },
          dispose: () => { if (contents.current.get(id) === content) contents.current.delete(id); },
        };
      },
      createTabComponent: ({ id }) => {
        const tab = document.createElement('div');
        tab.className = 'dock-tab';
        const title = document.createElement('span'); title.className = 'dock-tab-title';
        const close = document.createElement('button'); close.type = 'button'; close.className = 'dock-tab-close';
        // A conversation's status before its title (working, needs you, failed, done); empty for other panels.
        const status = document.createElement('span'); status.className = 'dock-tab-status'; status.hidden = true;
        const disposers: (() => void)[] = [];
        return {
          element: tab,
          init: parameters => {
            const panel = panelFrom(id, parameters.params);
            tab.dataset.panelTab = id; tab.dataset.kind = panel.kind;
            // Right-click: the group's menu, through the shared context menu (Workspace contributes it).
            tab.dataset.contextMenu = 'dock-tab';
            title.textContent = parameters.title;
            let icon: SVGSVGElement | HTMLImageElement | undefined;
            let iconKey = '';
            const paintIcon = (panel: Panel) => {
              const host = latest.current;
              const agent = host.agentOf?.(panel);
              const src = agent ? undefined : host.imageOf?.(panel);
              const name = agent?.icon || host.iconOf(panel);
              const key = JSON.stringify([src, name, agent?.label]);
              if (icon && key === iconKey) return;
              const owner = tab.ownerDocument;
              let next: SVGSVGElement | HTMLImageElement;
              if (src) {
                const image = owner.createElement('img');
                image.className = 'dock-tab-image'; image.src = src; image.alt = '';
                next = image;
              } else next = iconElement(name, 14, owner);
              if (agent) {
                next.removeAttribute('aria-hidden');
                next.setAttribute('role', 'img');
                next.setAttribute('aria-label', agent.label);
              }
              if (icon) icon.replaceWith(next); else tab.insertBefore(next, title);
              icon = next; iconKey = key;
            };
            const badge = document.createElement('span'); badge.className = 'dock-tab-badge'; badge.setAttribute('role', 'status'); badge.hidden = true;
            badges.current.set(id, badge); disposers.push(() => { if (badges.current.get(id) === badge) badges.current.delete(id); });
            tab.replaceChildren(status, title, badge);
            paintBadge(id, panel);
            const name = () => parameters.api.title || latest.current.titleOf(panel);
            tabs.current.set(id, { tab, status, paintIcon });
            decorateTab(id, panel, name());
            if (latest.current.canClose(panel)) {
              close.replaceChildren(iconElement('close', 12));
              close.setAttribute('aria-label', latest.current.closeLabel?.(panel, name()) ?? `Close ${name()} panel`); closes.current.set(id, close); close.title = panel.kind === 'terminal' ? 'Close panel (shells keep running)' : 'Close panel';
              close.onpointerdown = event => event.stopPropagation();
              close.onclick = event => { event.stopPropagation(); latest.current.onClose(panelFrom(id, api.current?.getPanel(id)?.params)); };
              tab.append(close);
            }
            /** The panel as it is now (a preview tab kept open changes its params). */
            const current = () => panelFrom(id, api.current?.getPanel(id)?.params ?? parameters.params);
            const titled = parameters.api.onDidTitleChange(event => { title.textContent = event.title; close.setAttribute('aria-label', latest.current.closeLabel?.(panel, event.title) ?? `Close ${event.title} panel`); decorateTab(id, current(), event.title); });
            disposers.push(() => titled.dispose(), () => { if (tabs.current.get(id)?.tab === tab) tabs.current.delete(id); });
            // A preview tab double-clicked is kept open; any other tab maximizes.
            tab.ondblclick = event => { event.preventDefault(); if (latest.current.onTabDoubleClick?.(current())) return; handle.toggleMaximize(id, 'pointer'); };
            // A middle-click closes a tab, as in a browser.
            tab.onmousedown = event => { if (event.button === 1) event.preventDefault(); };
            tab.onauxclick = event => {
              if (event.button !== 1 || !latest.current.canClose(current())) return;
              event.preventDefault(); event.stopPropagation(); latest.current.onClose(current());
            };
            // A minimized group opens again from its tab.
            tab.onclick = () => { const group = api.current?.getPanel(id)?.group; if (group?.element.classList.contains('dock-group-minimized')) latest.current.onRestore(group.id); };
          },
          dispose: () => { for (const dispose of disposers.splice(0)) dispose(); },
        };
      },
      createRightHeaderActionComponent: group => {
        const actions = document.createElement('div');
        actions.className = 'dock-group-actions';
        actions.dataset.group = group.id;
        return {
          element: actions,
          init: () => setActionHosts(hosts => new Map(hosts).set(group.id, actions)),
          dispose: () => setActionHosts(hosts => { if (hosts.get(group.id) !== actions) return hosts; const next = new Map(hosts); next.delete(group.id); return next; }),
        };
      },
      createLeftHeaderActionComponent: group => {
        const add = document.createElement('div');
        add.className = 'dock-group-add';
        add.dataset.group = group.id;
        return {
          element: add,
          init: () => setAddHosts(hosts => new Map(hosts).set(group.id, add)),
          dispose: () => setAddHosts(hosts => { if (hosts.get(group.id) !== add) return hosts; const next = new Map(hosts); next.delete(group.id); return next; }),
        };
      },
    });
    api.current = dock;
    // A rearrangement is reported on the next frame; a resize once it settles,
    // so dragging a divider does not save on every pointer move.
    let frame = 0, timer = 0;
    const report = (delay: number) => {
      if (applying.current) return;
      clearTimeout(timer); cancelAnimationFrame(frame);
      const run = () => {
        frame = 0; timer = 0;
        const next = read();
        if (!next) return;
        decorateSashes(); bump();
        // The sidebar's edge was dragged: that width becomes everyone's.
        const page = latest.current.page, found = sidebarOf(next.layout);
        if (found) {
          const sidebar = dock.getGroup(found.group.id);
          if (sidebar && Math.abs(sidebar.api.width - latest.current.sidebarWidth) > 1 && sidebarOf(page.layout)?.at === found.at) {
            // Only a divider the person dragged changes it; a drop that squeezed it is undone.
            if (dividerDrag) latest.current.onSidebarWidth(clampSidebar(sidebar.api.width));
            else applyConstraints();
          }
        }
        if (!sameArrangement(next, page)) latest.current.onArrange(next);
      };
      if (delay) timer = window.setTimeout(run, delay); else frame = requestAnimationFrame(run);
    };
    // Whether a divider is being dragged, until its last resize is reported.
    let dividerDrag = false, released = 0;
    // dockview's own drags (a divider, a floating group's edge or title bar, a tab or a group)
    // go through the app's drag gesture, so none of them selects text (drag-gesture.ts).
    let endDrag: (() => void) | undefined;
    const drag = (axis: DragAxis, event?: PointerEvent, handle?: Element) => {
      endDrag?.();
      const cursor = handle ? getComputedStyle(handle).cursor : '';
      endDrag = beginDrag(axis, { pointerId: event?.pointerId, cursor: cursor && cursor !== 'auto' ? cursor : undefined, escape: false, onEnd: () => { endDrag = undefined; } });
    };
    const pressed = (event: PointerEvent) => {
      const target = event.target instanceof Element ? event.target : null;
      dividerDrag = !!target?.closest('.dv-sash'); clearTimeout(released);
      if (event.button !== 0 || !target) return;
      const edge = target.closest('.dv-sash, [class^="dv-resize-handle-"]');
      if (edge) { drag(/^(row|ns|n|s)-resize$/.test(getComputedStyle(edge).cursor) ? 'y' : 'x', event, edge); return; }
      // A floating group moves by its title bar, or anywhere with Shift.
      const floating = target.closest('.dv-resize-container');
      if (floating && !target.closest('button, input, select, textarea, a, [contenteditable="true"]') && (target.closest('.dv-floating-titlebar') || event.shiftKey)) drag('move', event);
    };
    const lifted = () => { endDrag?.(); if (document.documentElement.classList.contains('dock-dragging')) { document.documentElement.classList.remove('dock-dragging'); refreshNativeWebViews(); } if (dividerDrag) { clearTimeout(released); released = window.setTimeout(() => { dividerDrag = false; }, 450); } };
    element.addEventListener('pointerdown', pressed, true);
    // Enter or Space on a minimized group's tab opens it, as a click does.
    const selectFromHeader = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const header = target?.closest?.('.dv-tabs-and-actions-container');
      const id = header?.querySelector<HTMLElement>('.dv-active-tab [data-panel-tab]')?.dataset.panelTab;
      const panel = id && contents.current.get(id)?.querySelector<HTMLElement>('.workspace-panel');
      if (panel) selectPanelText(event, panel);
    };
    const tabKey = (event: KeyboardEvent) => {
      selectFromHeader(event);
      if (event.key !== 'Enter' && event.key !== ' ') return;
      const group = event.target instanceof Element && event.target.matches('.dv-tab') ? event.target.closest<HTMLElement>('.dock-group-minimized') : null;
      if (group?.dataset.groupId) { event.preventDefault(); latest.current.onRestore(group.dataset.groupId); }
    };
    element.addEventListener('keydown', tabKey);
    window.addEventListener('pointerup', lifted, true);
    // A tab or a group dragged is a drag gesture too (a floating group's title bar already began one).
    // Native web views step aside while dragging (native-web-view.ts), and come back after.
    const dragging = (on: boolean) => () => {
      const root = document.documentElement;
      if (root.classList.contains('dock-dragging') === on) return;
      if (!on) endDrag?.(); else if (!endDrag) drag('move');
      root.classList.toggle('dock-dragging', on); refreshNativeWebViews();
    };
    // A Web panel moves into a popped-out window only where its page can move with it (native views that come home).
    const dropRules = dock.onWillDrop(event => {
      const data = event.getData(), moved = data?.panelId ? [dock.getPanel(data.panelId)] : data ? dock.getGroup(data.groupId)?.panels || [] : [];
      if (!latest.current.webPopouts && event.group?.api.location.type === 'popout' && moved.some(panel => (panel?.params as Partial<PanelParams> | undefined)?.kind === 'web')) event.preventDefault();
      // Workspaces and Agents are the sidebar: nothing else joins or splits their groups, and they do not go into the content's.
      const kindOf = (panel: IDockviewPanel | undefined) => (panel?.params as Partial<PanelParams> | undefined)?.kind || '';
      const target = event.group?.panels || [];
      if (target.length && moved.length) {
        const intoSidebar = target.every(panel => sidebarKinds.has(kindOf(panel))), sidebarMoving = moved.some(panel => sidebarKinds.has(kindOf(panel)));
        if (intoSidebar ? moved.some(panel => !sidebarKinds.has(kindOf(panel))) : sidebarMoving) event.preventDefault();
      }
    });
    const popoutFailures = dock.onDidOpenPopoutWindowFail(({ reason, error }) => {
      const message = {
        blocked: 'The host blocked the new panel window.',
        closed: 'The panel window closed before it finished opening.',
        unscriptable: 'Lazybox could not display the panel in the new window.',
        'url-refused': 'Lazybox could not open the panel window address.',
      }[reason];
      latest.current.onPopoutError(error ? `${message} ${error.message}` : message);
    });
    const subscriptions = [popoutFailures, dropRules, dock.onWillDragPanel(dragging(true)), dock.onWillDragGroup(dragging(true)), dock.onDidDrop(dragging(false)), dock.onDidLayoutChange(dragging(false)),
      { dispose: () => { endDrag?.(); element.removeEventListener('keydown', tabKey); element.removeEventListener('pointerdown', pressed, true); window.removeEventListener('pointerup', lifted, true); clearTimeout(released); } },
      dock.onDidLayoutChange(event => report([...event.reasons].every(reason => reason === 'geometry') ? 200 : 0)), dock.onDidMaximizedGroupChange(() => { bump(); refreshNativeWebViews(); }), dock.onDidActiveGroupChange(bump), dock.onDidAddPopoutGroup(event => { bump(); refreshNativeWebViews(); listenInPopout(event.window); attachPopoutTitleBar(event.window, event.group.element.querySelector('.dv-tabs-and-actions-container')); forgetFocus(event.group); }), dock.onDidRemovePopoutGroup(event => { bump(); refreshNativeWebViews(); forgetFocus(event.group); })];
    // A panel that changed windows does not get the focus back where it was: that place belonged to the other window (Monaco keeps the window it was in).
    function forgetFocus(group: DockviewGroupPanel | undefined) { for (const panel of group?.panels || []) lastFocus.current.delete(panel.id); }
    // Keys pressed in a popped-out window go to `onPopoutKey`, with the group that window holds.
    function listenInPopout(popout: Window | undefined) {
      popout?.addEventListener('keydown', event => {
        selectFromHeader(event);
        const group = dock.getPopouts().find(value => value.window === popout)?.group;
        if (group) latest.current.onPopoutKey?.(event, group.id);
      });
    }
    const observer = new ResizeObserver(() => {
      const box = element.getBoundingClientRect(), next = { width: Math.round(box.width), height: Math.round(box.height) };
      if (!next.width || !next.height || (next.width === size.current.width && next.height === size.current.height)) return;
      const first = !size.current.width;
      size.current = next;
      if (first) { reconcile(); return; }
      silently(() => dock.layout(next.width, next.height));
      applyConstraints(); decorateSashes();
      fits();
    });
    observer.observe(element);
    // Measured now, so the first arrangement is in place before panels focus anything in it.
    const box = element.getBoundingClientRect();
    if (box.width && box.height) { size.current = { width: Math.round(box.width), height: Math.round(box.height) }; reconcile(); }
    return () => { observer.disconnect(); cancelAnimationFrame(frame); clearTimeout(timer); for (const subscription of subscriptions) subscription.dispose(); api.current = undefined; applied.current = undefined; size.current = { width: 0, height: 0 }; dock.dispose(); };
  }, []);

  // The saved arrangement changed (a command, another workspace, a preset): bring dockview to it.
  useLayoutEffect(() => { reconcile(); }, [props.page]);
  useLayoutEffect(() => { applyConstraints(); decorateSashes(); }, [props.sidebarWidth]);
  // Titles a panel gives itself (a web page's) change without the arrangement changing.
  useEffect(() => {
    const dock = api.current;
    if (!dock) return;
    silently(() => {
      for (const panel of dock.panels) {
        if (panel.id === EMPTY_CONTENT) continue;
        const saved = panelFrom(panel.id, panel.params), title = latest.current.titleOf(saved);
        if (panel.title !== title) panel.api.setTitle(title);
        paintBadge(panel.id, saved);
        const close = closes.current.get(panel.id), label = latest.current.closeLabel?.(saved, title) ?? `Close ${title} panel`;
        if (close && close.getAttribute('aria-label') !== label) close.setAttribute('aria-label', label);
        decorateTab(panel.id, saved, title);
      }
    });
  });

  const view = (group: DockviewGroupPanel): GroupView => {
    const saved = groupStates(latest.current.page).get(group.id), direction = parents(latest.current.page.layout).get(group.id);
    const members = group.panels.map(panel => panelFrom(panel.id, panel.params));
    return {
      id: group.id, panels: members, active: members.find(panel => panel.id === group.activePanel?.id),
      location: group.api.location.type === 'floating' ? 'floating' : group.api.location.type === 'popout' ? 'popout' : 'grid',
      maximized: group.api.isMaximized(), minimized: !!saved?.minimized && direction === 'column', canMinimize: direction === 'column',
    };
  };
  const panelOf = (panelID?: string): IDockviewPanel | undefined => panelID ? api.current?.getPanel(panelID) : api.current?.activePanel;
  const gridGroups = (dock: DockviewApi) => dock.groups.filter(group => group.api.location.type === 'grid' && !group.panels.some(panel => panel.id === EMPTY_CONTENT));
  /**
   * A panel to the front of its group, and its group the active one. While a
   * group is maximized the zoom moves with it, so navigation never drops the
   * person out of a maximized panel (dockview would restore the layout).
   */
  const activate = (panel: IDockviewPanel) => {
    const dock = api.current!, zoomed = dock.hasMaximizedGroup();
    panel.api.setActive();
    if (zoomed && !dock.hasMaximizedGroup() && panel.group.api.location.type === 'grid' && gridGroups(dock).length > 1) dock.maximizeGroup(panel);
  };
  /** Focus into a panel: where the person last was in it, else its message box, else its first field or control. */
  const focusInto = (panel: IDockviewPanel) => {
    // The dock's own focus request (pending-focus.ts): focus moved elsewhere before the next frame stays there.
    const request = { panel: panel.id };
    intoFocus.ask(request);
    requestAnimationFrame(() => {
      if (!intoFocus.take(request)) return;
      const host = latest.current.hostFor(panelFrom(panel.id, panel.params));
      const last = lastFocus.current.get(panel.id);
      // Not an element of a window that has closed since (a group popped out and put back), and not
      // the code editor's own input: Monaco tracks its focus with the window it was first shown in,
      // which a popped-out editor has left (focusing it from here throws). Its tabs take the focus instead.
      const usable = (element: HTMLElement) => element.isConnected && element.ownerDocument === host.ownerDocument && !!element.ownerDocument.defaultView && host.contains(element) && !element.closest('.monaco-editor') && element.getClientRects().length > 0 && !(element as HTMLButtonElement).disabled;
      if (last && usable(last)) { last.focus({ preventScroll: true }); return; }
      (host.querySelector<HTMLElement>('[data-agent-composer]:not(:disabled)') || host.querySelector<HTMLElement>('textarea:not(:disabled), [tabindex="0"], button:not(:disabled), input:not(:disabled)'))?.focus({ preventScroll: true });
    });
  };
  const titleIn = (group: { activePanel?: { title?: string } }) => group.activePanel?.title || 'panel';
  const announce = (text: string, edge?: LayoutDirection) => {
    setRing(true);
    // Said again even when it is the same words (a second press at the same edge).
    setSaid(previous => previous.replace(/​$/, '') === text ? text + (previous.endsWith('​') ? '' : '​') : text);
    if (edge) setBumped({ edge, at: Date.now() });
  };
  /** Each docked group's place, from the saved arrangement (a maximized group hides the others on screen). */
  const rects = () => groupRects(latest.current.page.layout, { width: size.current.width, height: size.current.height, sidebarWidth: latest.current.sidebarWidth, header: TAB_BAR });
  const words: Record<LayoutDirection, string> = { left: 'to the left', right: 'to the right', up: 'above', down: 'below' };
  /** The next group in turn, in front: its tab in front, or (`byTab`, walking every tab) its first tab, its last going back. */
  const cycle = (step: 1 | -1, byTab: boolean) => {
    const dock = api.current;
    if (!dock || !dock.groups.length) return;
    // While a group is maximized, only the docked groups take turns (the zoom moves along).
    const order = dock.groups.filter(group => !group.panels.some(panel => panel.id === EMPTY_CONTENT) && (!dock.hasMaximizedGroup() || group.api.location.type === 'grid')), index = order.findIndex(group => group === dock.activeGroup);
    const next = order[(index + step + order.length) % order.length];
    if (!next) return;
    const front = byTab && next.panels.length > 1 ? next.panels[step > 0 ? 0 : next.panels.length - 1] : next.activePanel;
    if (front) handle.focusPanel(front.id); else next.api.setActive();
    if (order.length > 1) announce(`${titleIn(next)}, group ${order.indexOf(next) + 1} of ${order.length}${byTab && next.panels.length > 1 ? `, tab ${next.panels.indexOf(front!) + 1} of ${next.panels.length}` : ''}${dock.hasMaximizedGroup() ? ', maximized' : ''}`);
  };
  const handle: DockHandle = {
    toggleMaximize: (panelID, source) => {
      const dock = api.current, panel = panelOf(panelID);
      if (!dock || !panel) return;
      if (dock.hasMaximizedGroup()) {
        dock.exitMaximizedGroup();
      } else {
        if (panel.group.api.location.type !== 'grid' || dock.groups.length <= 1) return;
        dock.maximizeGroup(panel);
      }
      if (source) recordMaximize(source);
    },
    activePanelID: () => api.current?.activePanel?.id,
    activeGroup: () => { const group = api.current?.activeGroup; return group ? view(group) : undefined; },
    splitActive: side => {
      const panel = api.current?.activePanel;
      if (!panel || panel.group.panels.length < 2) return false;
      panel.api.moveTo({ group: panel.group, position: side === 'right' ? 'right' : 'bottom' });
      return true;
    },
    cycleGroups: step => cycle(step, false),
    cycleTabs: step => {
      const group = api.current?.activeGroup, panels = group?.panels || [], index = group?.activePanel ? panels.indexOf(group.activePanel) : -1;
      const next = panels[index + step];
      // Within the group first; past its last tab (its first, going back) the next group takes over.
      if (!group || index < 0 || !next) { cycle(step, true); return; }
      handle.focusPanel(next.id);
      announce(`Tab ${index + step + 1} of ${panels.length}: ${next.title || 'panel'}`);
    },
    popout: async panelID => {
      const pending = pendingPopouts.current.get(panelID);
      if (pending) return pending;
      const dock = api.current, panel = panelOf(panelID);
      if (!dock || !panel) return false;
      if (panel.group.api.location.type === 'popout') {
        panel.api.setActive();
        panel.group.api.getWindow().focus();
        return true;
      }
      const target = panel.group.panels.length > 1 ? panel : panel.group;
      // Its token names the window to the host, so a Web panel's page can move into it (native-web-view.ts).
      const opened = dock.addPopoutGroup(target, { popoutUrl: `${latest.current.popoutUrl}?w=${crypto.randomUUID()}` })
        .finally(() => pendingPopouts.current.delete(panelID));
      pendingPopouts.current.set(panelID, opened);
      return opened;
    },
    float: panelID => {
      const dock = api.current, panel = panelOf(panelID);
      if (!dock || !panel) return;
      const box = container.current!.getBoundingClientRect();
      dock.addFloatingGroup(panel.group.panels.length > 1 ? panel : panel.group, { x: Math.round(box.width * 0.2), y: 48, width: Math.round(box.width * 0.5), height: Math.round(box.height * 0.6) });
    },
    dockBack: async groupID => {
      const dock = api.current, group = dock?.getGroup(groupID);
      if (!dock || !group) return;
      if (group.api.location.type === 'popout') {
        // Closing its window puts the group back where it was (dockview's reference group); the page comes forward on it.
        const front = group.activePanel?.id, popout = group.api.getWindow();
        const back = new Promise<void>(resolve => {
          const finish = () => {
            done.dispose(); clearTimeout(timer);
            // Reported at once, so what is put back next (Put all back) finds this group in place.
            const next = read();
            if (next && !sameArrangement(next, latest.current.page)) latest.current.onArrange(next);
            resolve();
          };
          const done = dock.onDidRemovePopoutGroup(event => { if (event.group === group || event.id === groupID) finish(); });
          // A window that never reports closing does not hold up what comes after it.
          const timer = window.setTimeout(finish, 2000);
        });
        // The pages of its Web panels come home before their window goes.
        await reclaimPopoutViews(popout);
        popout.close();
        window.focus();
        await back;
        if (front) requestAnimationFrame(() => handle.focusPanel(front));
        return;
      }
      // Into the page beside its largest docked group.
      const docked = dock.groups.filter(value => value.api.location.type === 'grid' && value !== group).sort((a, b) => b.api.width * b.api.height - a.api.width * a.api.height)[0];
      if (docked) group.api.moveTo({ group: docked, position: 'right' });
      else group.api.moveTo({ position: 'center' });
    },
    focusWindow: groupID => { const group = api.current?.getGroup(groupID); if (group?.api.location.type === 'popout') group.api.getWindow().focus(); },
    // While this page has the focus no popped-out window does (the palette here does not offer to put one back).
    focusedPopout: () => document.hasFocus() ? undefined : api.current?.getPopouts().find(popout => { try { return popout.window.document.hasFocus(); } catch { return false; } })?.group.id,
    hasPanel: panelID => !!api.current?.getPanel(panelID),
    focusPanel: panelID => {
      const panel = api.current?.getPanel(panelID);
      if (!panel) return;
      activate(panel);
      focusInto(panel);
    },
    groupOf: panelID => { const panel = api.current?.getPanel(panelID); return panel ? view(panel.group) : undefined; },
    focusTab: panelID => { const panel = api.current?.getPanel(panelID); if (panel && panel.group.activePanel !== panel) panel.api.setActive(); },
    neighbour: direction => {
      const from = api.current?.activeGroup;
      if (!from || from.api.location.type !== 'grid') return undefined;
      const back = cameFrom.current && { left: 'right', right: 'left', up: 'down', down: 'up' }[cameFrom.current.direction] === direction ? cameFrom.current.group : undefined;
      return neighbourIn(rects(), from.id, direction, back);
    },
    focusDirection: direction => {
      const dock = api.current, from = dock?.activeGroup;
      if (!dock || !from) return;
      if (from.api.location.type !== 'grid') { announce(`${titleIn(from)} is ${from.api.location.type === 'floating' ? 'floating' : 'in its own window'}: Ctrl+Tab moves between groups`, direction); return; }
      const id = handle.neighbour(direction), next = id ? dock.getGroup(id) : undefined;
      if (!next?.activePanel) { announce(`No panel group ${words[direction]} ${titleIn(from)}`, direction); return; }
      cameFrom.current = { group: from.id, direction };
      // A minimized group opens as it is reached (its tab bar has nothing to focus).
      const saved = groupStates(latest.current.page).get(next.id);
      if (saved?.minimized) latest.current.onRestore(next.id);
      const zoomed = dock.hasMaximizedGroup();
      handle.focusPanel(next.activePanel.id);
      announce(`${titleIn(next)}${next.panels.length > 1 ? `, tab ${next.panels.indexOf(next.activePanel) + 1} of ${next.panels.length}` : ''}${zoomed ? ', maximized' : ''}`);
    },
    resize: (direction, repeat) => {
      const dock = api.current, group = dock?.activeGroup;
      if (!dock || !group) return;
      if (group.api.location.type !== 'grid') { announce(`${titleIn(group)} is not docked: drag its edges to resize it`, direction); return; }
      if (dock.hasMaximizedGroup()) { announce(`${titleIn(group)} is maximized: restore the layout to resize it`, direction); return; }
      streak.current = repeat ? streak.current + 1 : 0;
      const step = resizeStep(repeat, streak.current), page = latest.current.page;
      const result = resizeEdge(page.layout, group.id, direction, step);
      // Held down, the key is announced once it settles, not on every step.
      const say = (text: string, edge?: LayoutDirection) => { clearTimeout(held.current); if (repeat) held.current = window.setTimeout(() => announce(text, edge), 300); else announce(text, edge); setRing(true); };
      if (result.kind === 'sidebar') {
        const width = clampSidebar(latest.current.sidebarWidth + result.sign * Math.max(8, Math.round(step * size.current.width)));
        if (width === latest.current.sidebarWidth) { say(`The sidebar is as ${result.sign > 0 ? 'wide' : 'narrow'} as it goes, ${width} pixels`, direction); return; }
        latest.current.onSidebarWidth(width);
        say(`Workspaces sidebar, ${width} pixels wide`);
        return;
      }
      if (result.kind === 'none') { say(`${titleIn(group)} cannot grow or shrink ${words[direction]}`, direction); return; }
      const before = result.axis === 'width' ? group.api.width : group.api.height;
      if (page.popouts.length) {
        // A group in a window of its own would close in a rebuild: dockview resizes in place (its neighbours share the change).
        const extent = result.axis === 'width' ? group.api.width : group.api.height, total = result.axis === 'width' ? size.current.width : size.current.height;
        const grow = result.share >= (shareOf(page.layout, group.id, result.axis) ?? 0);
        group.api.setSize(result.axis === 'width' ? { width: Math.round(extent + (grow ? 1 : -1) * step * total) } : { height: Math.round(extent + (grow ? 1 : -1) * step * total) });
      } else latest.current.onArrange({ layout: result.layout, floating: page.floating, popouts: page.popouts });
      // Said as it is once laid out: dockview keeps every panel at its minimum size.
      requestAnimationFrame(() => {
        const now = dock.getGroup(group.id), after = now ? (result.axis === 'width' ? now.api.width : now.api.height) : before;
        const total = result.axis === 'width' ? size.current.width : size.current.height;
        if (Math.abs(after - before) < 2) say(`${titleIn(now ?? group)} cannot move that edge further ${words[direction]}`, direction);
        else say(`${titleIn(now ?? group)}, ${Math.round(100 * after / Math.max(1, total))}% of the ${result.axis}`);
      });
    },
    selectTab: slot => {
      const dock = api.current, group = dock?.activeGroup;
      if (!dock || !group) return;
      const index = tabAt(group.panels.length, slot);
      if (index === undefined) { announce(`${titleIn(group)} has ${group.panels.length} tab${group.panels.length === 1 ? '' : 's'}`); return; }
      const panel = group.panels[index];
      if (groupStates(latest.current.page).get(group.id)?.minimized) latest.current.onRestore(group.id);
      handle.focusPanel(panel.id);
      announce(`Tab ${index + 1} of ${group.panels.length}: ${panel.title || 'panel'}`);
    },
    reveal: panelID => {
      const dock = api.current, panel = dock?.getPanel(panelID);
      if (!dock || !panel || !dock.hasMaximizedGroup() || panel.group.api.isMaximized()) return;
      activate(panel);
    },
    announce,
    restore: () => { if (api.current?.hasMaximizedGroup()) api.current.exitMaximizedGroup(); },
  };
  useImperativeHandle(ref, () => handle);

  const dock = api.current, empty = !props.page.layout && !props.page.floating.length && !props.page.popouts.length;
  return <div ref={layoutRoot} className="dock-layout">
    <div ref={container} className="dock-container"/>
    {empty && <div className="dock-empty">{props.empty}</div>}
    {emptyHost && needsEmptyContent(props.page) && createPortal(props.empty, emptyHost)}
    {dock && [...actionHosts].map(([id, element]) => { const group = dock.getGroup(id); return group ? createPortal(props.renderActions(view(group as DockviewGroupPanel), handle), element, id) : null; })}
    {dock && props.renderAdd && [...addHosts].map(([id, element]) => { const group = dock.getGroup(id); return group ? createPortal(props.renderAdd!(view(group as DockviewGroupPanel), handle), element, `add-${id}`) : null; })}
    {dock && ring && <FocusRing dock={dock} root={layoutRoot} bumped={bumped}/>}
    <div className="sr-only" role="status" aria-live="polite" aria-atomic="true" data-testid="dock-announcer">{said}</div>
  </div>;
});

/**
 * The ring around the active group while the dock is worked from the
 * keyboard. It glides to the group focus moved to, follows a resize, and
 * nudges toward an edge that has nothing beyond it.
 */
function FocusRing({ dock, root, bumped }: { dock: DockviewApi; root: React.RefObject<HTMLDivElement | null>; bumped?: { edge: LayoutDirection; at: number } }) {
  const ring = useRef<HTMLDivElement>(null);
  const [edge, setEdge] = useState<LayoutDirection>();
  useEffect(() => {
    if (!bumped || Date.now() - bumped.at > 400) return;
    // Off first, so a second nudge the same way plays again.
    setEdge(undefined);
    const frame = requestAnimationFrame(() => setEdge(bumped.edge));
    const timer = setTimeout(() => setEdge(undefined), 300);
    return () => { cancelAnimationFrame(frame); clearTimeout(timer); };
  }, [bumped]);
  useLayoutEffect(() => {
    const place = () => {
      const group = dock.activeGroup, element = ring.current, box = root.current?.getBoundingClientRect();
      if (!element || !box) return;
      if (!group || group.api.location.type === 'popout' || group.panels.some(panel => panel.id === EMPTY_CONTENT)) { element.hidden = true; return; }
      const rect = group.element.getBoundingClientRect();
      element.hidden = !rect.width || !rect.height;
      Object.assign(element.style, { left: `${rect.left - box.left}px`, top: `${rect.top - box.top}px`, width: `${rect.width}px`, height: `${rect.height}px` });
    };
    place();
    // dockview lays out after this render (a rebuild, a resize): measured again once it has.
    const frame = requestAnimationFrame(place);
    const subscriptions = [dock.onDidLayoutChange(() => requestAnimationFrame(place)), dock.onDidActiveGroupChange(() => requestAnimationFrame(place)), dock.onDidMaximizedGroupChange(() => requestAnimationFrame(place))];
    window.addEventListener('resize', place);
    return () => { cancelAnimationFrame(frame); window.removeEventListener('resize', place); for (const subscription of subscriptions) subscription.dispose(); };
  });
  return <div ref={ring} className="dock-focus-ring" data-bump={edge} aria-hidden="true"/>;
}

/** The group nearest a new one in the saved tree that dockview already has, and which side of it the new one goes. */
function neighbour(root: LayoutNode | null, groupID: string, existing: Set<string>): { group: string; direction: 'left' | 'right' | 'above' | 'below' } | undefined {
  let found: { group: string; direction: 'left' | 'right' | 'above' | 'below' } | undefined;
  const visit = (node: LayoutNode | null) => {
    if (found || node?.type !== 'split') return;
    const index = node.children.findIndex(child => child.type === 'group' && child.id === groupID);
    if (index >= 0) {
      const row = node.direction === 'row', previous = node.children[index - 1], next = node.children[index + 1];
      const pick = (child: LayoutNode | undefined, last: boolean) => { const all = groups(child || null).filter(group => existing.has(group.id)); return last ? all[all.length - 1] : all[0]; };
      const before = pick(previous, true), after = pick(next, false);
      if (before) found = { group: before.id, direction: row ? 'right' : 'below' };
      else if (after) found = { group: after.id, direction: row ? 'left' : 'above' };
      return;
    }
    node.children.forEach(visit);
  };
  visit(root);
  return found;
}
