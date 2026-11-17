import type { ViewStateStore } from '@lazybox/platform';
import { Icon } from './Icons';
import { SplitStoreContext } from './Resizer';
import type { ModuleProps, PanelModule, PanelTitle } from './PanelRegistry';
import type { Panel, ViewTarget } from './workspace-model';
import { PanelShownContext } from './useAttention';

/** What a panel can ask of its page. */
export type PanelActions = {
  close(panel: Panel): void;
  /** Shows a target in this panel: a pinned panel keeps its own, others move the page. */
  retarget(panel: Panel, target: ViewTarget): void;
  showSession(target: ViewTarget): void;
  detach(panel: Panel): void;
  dock(panel: Panel): void;
  /** What the module shows instead of its kind's title (a web page's own title and icon), for its tab. */
  title(panel: Panel, title?: PanelTitle): void;
};

/** What every panel module receives from its page, besides its own target and store. */
export type SharedModuleProps = Omit<ModuleProps, 'target' | 'onTarget' | 'viewState' | 'onOpenChat'>;

/**
 * One panel of a page: its module, rendered once into the panel's stable host.
 * Its title, menu and close button are its tab in the dock (DockLayout).
 */
export function WorkspacePanel({ panel, definition, target, store, module, actions, pending, mobileActive, canDetach, shown = true }: {
  panel: Panel;
  definition?: PanelModule;
  target: ViewTarget;
  store?: ViewStateStore;
  module: SharedModuleProps;
  actions: PanelActions;
  /** A detach or dock is in flight. */
  pending: boolean;
  mobileActive: boolean;
  /** The host can open chats in their own window. */
  canDetach: boolean;
  /** On screen: the tab in front of its group (on a phone, the panel shown). A conversation in a tab behind another is not seen. */
  shown?: boolean;
}) {
  const onTarget = (next: ViewTarget) => actions.retarget(panel, next);
  return <PanelShownContext.Provider value={shown}><section className={`workspace-panel panel-${panel.kind}`} aria-label={definition?.title || panel.kind} data-panel-id={panel.id} data-mobile-active={mobileActive || undefined}>
    <div className="module-body">
      {panel.detached
        ? <DetachedMessage canDetach={canDetach} pending={pending} onOpen={() => actions.detach(panel)} onDock={() => actions.dock(panel)}/>
        : definition
          // The dividers inside the panel keep their sizes in its store.
          ? <SplitStoreContext.Provider value={store}><definition.Component {...module} target={target} viewState={store} panelId={panel.id} onPanelTitle={title => actions.title(panel, title)}
            onTarget={panel.kind === 'collections' ? next => next.sessionId ? actions.showSession(next) : onTarget(next) : onTarget}
            onOpenChat={panel.kind === 'chat' && canDetach && target.sessionId ? () => actions.detach(panel) : undefined}
            onClosePanel={panel.kind === 'terminal' || panel.kind === 'web' ? () => actions.close(panel) : undefined}/></SplitStoreContext.Provider>
          : <p className="module-empty">The “{panel.kind}” module is unavailable. Its configuration has been retained.</p>}
    </div>
  </section></PanelShownContext.Provider>;
}

/** In place of a chat that is open in its own window. */
function DetachedMessage({ canDetach, pending, onOpen, onDock }: { canDetach: boolean; pending: boolean; onOpen(): void; onDock(): void }) {
  return <div className="module-empty detached-message">
    <Icon name="external" size={24}/>
    <h3>Chat is in its own window</h3>
    <p>Your conversation continues there.</p>
    <div className="detached-message-actions">
      <button className="primary" disabled={!canDetach || pending} onClick={onDock}><Icon name="put-back" size={14}/>Put back here</button>
      <button disabled={!canDetach || pending} onClick={onOpen}><Icon name="external" size={14}/>Show its window</button>
    </div>
  </div>;
}
