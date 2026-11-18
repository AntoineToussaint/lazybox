import type { ViewStateStore } from '@lazybox/platform';
import { ActionMenu } from './ActionMenu';
import { bindings, displayBinding, useAction } from './actions';
import { Icon } from './Icons';
import { useKeymap } from './keyboard';

/** "Put back in main window (⇧⌘D)": a label with the person's first key for an action, for a tooltip. */
export function withKeys(label: string, keys: string | undefined): string {
  const first = bindings(keys)[0];
  return first ? `${label} (${displayBinding(first).join(' ')})` : label;
}

/**
 * A detached window's own controls: one quiet row. A chat's window leads with
 * the way back, **Put back in main window** (⇧⌘D, the Window menu, the
 * palette), which returns the chat to where it was in the main window and
 * closes this one; closing the window does the same. A command console has
 * no place to go back to, so it leads with the main window instead. Showing
 * the main window, reopening a closed window and reconnecting (rarely needed:
 * the window follows Runner restarts) live in ⋯.
 */
export function WindowControls({ applicationName, viewState, onPutBack, onWorkspace, onReopenClosed, onReconnect }: { applicationName: string; viewState?: ViewStateStore; onPutBack?(): void; onWorkspace(): void; onReopenClosed?(): void; onReconnect?(): void }) {
  const keymap = useKeymap(viewState);
  useAction('window.put-back', () => onPutBack?.(), () => !!onPutBack);
  // The window's title bar in the desktop app (title-bar.ts): its empty parts move the window.
  return <nav className="native-toolbar window-controls" aria-label="Window controls" data-window-drag-region="deep">
    {onPutBack
      ? <button type="button" className="put-back-button" title={withKeys('Put this chat back where it was in the main window, and close this window', keymap['window.put-back'])} onClick={onPutBack}>
        <Icon name="put-back" size={14}/><span>Put back in main window</span>
      </button>
      : <button type="button" className="icon-button window-back" aria-label="Show main window" title="Show the main window" onClick={onWorkspace}><Icon name="back" size={15}/></button>}
    <span className="application-name">{applicationName}</span>
    <ActionMenu className="window-menu" label="Window actions">
      {onPutBack && <button onClick={onWorkspace}><Icon name="external"/><span>Show main window</span></button>}
      {onReopenClosed && <button onClick={onReopenClosed}><Icon name="history"/><span>Reopen closed window</span></button>}
      {onReconnect && <button onClick={onReconnect}><Icon name="refresh"/><span>Reconnect this window</span></button>}
    </ActionMenu>
  </nav>;
}
