import { Icon } from './Icons';
import type { Panel } from './workspace-model';

/**
 * The Workspaces sidebar keeps one width in pixels for every workspace and
 * window size, so switching workspace never moves it; dragging its edge
 * changes it everywhere (see DockLayout).
 */
export const SIDEBAR_MIN = 200, SIDEBAR_MAX = 640, SIDEBAR_DEFAULT = 280;
export const clampSidebar = (width: number) => Math.round(Math.min(SIDEBAR_MAX, Math.max(SIDEBAR_MIN, width)));

/** On a phone, one panel at a time: this chooses which. */
export function MobilePanelSwitcher({ panels, active, title, onChoose }: { panels: Panel[]; active?: Panel; title(panel: Panel): string; onChoose(panel: Panel): void }) {
  return <nav className="mobile-panel-switcher" aria-label="Workspace panels">
    {panels.map(panel => <button key={panel.id} aria-pressed={panel.id === active?.id} onClick={() => onChoose(panel)}>{title(panel)}</button>)}
  </nav>;
}

/** On a phone, the one shown panel: its stable host element is moved in, not re-rendered. A closable panel gets a reachable ×. */
export function MobilePanel({ host, title, onClose }: { host?: HTMLElement; title: string; onClose?(): void }) {
  return <div className="mobile-panel">
    {onClose && <div className="mobile-panel-bar"><strong>{title}</strong><button className="icon-button" aria-label={`Close ${title} panel`} onClick={onClose}><Icon name="close" size={16}/></button></div>}
    <div className="mobile-panel-body" ref={node => { if (node && host && host.parentNode !== node) node.replaceChildren(host); }}/>
  </div>;
}
