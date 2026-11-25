import { Component, type ErrorInfo, type ReactNode } from 'react';
import { ContextMenuHost } from './ContextMenu';

const reloadKey = 'lazybox:window-error-reload';

/**
 * A window must never be left blank by a render error. In development a live
 * code reload can leave modules from two versions side by side (for example a
 * new RunnerContext beside an old Workspace); reloading once fixes that, so the
 * window does it by itself, at most once every 10 seconds. Otherwise the error
 * is shown with a Reload button and its details.
 */
export class WindowErrorBoundary extends Component<{ children: ReactNode; development?: boolean }, { error?: Error }> {
 state: { error?: Error } = {};
 static getDerivedStateFromError(error: Error) { return { error }; }
 componentDidCatch(error: Error, info: ErrorInfo) {
  console.error('Window error', error, info.componentStack);
  if (!this.props.development) return;
  try {
   const last = Number(sessionStorage.getItem(reloadKey) || 0);
   if (Date.now() - last < 10000) return;
   sessionStorage.setItem(reloadKey, String(Date.now()));
  } catch { return; }
  window.setTimeout(() => window.location.reload(), 300);
 }
 render() {
  const { error } = this.state;
  // Every window shows Lazybox context menus, its error screen included.
  if (!error) return <>{this.props.children}<ContextMenuHost/></>;
  return <><ContextMenuHost/><div className="window-error" role="alert">
   <h2>This window hit an error</h2>
   <p>Your agents and commands keep running on the Runner. Reloading the window reconnects to them.</p>
   <div className="window-error-actions">
    <button className="primary" onClick={() => window.location.reload()}>Reload window</button>
    <button onClick={() => this.setState({ error: undefined })}>Try to continue</button>
   </div>
   <p>{error.message || String(error)}</p>
   <details><summary>Details</summary><pre>{String(error.stack || error.message)}</pre></details>
  </div></>;
 }
}
