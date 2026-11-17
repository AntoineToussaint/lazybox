import { useEffect, useRef, useState, type KeyboardEvent as ReactKeyboardEvent, type ReactNode } from 'react';
import type { ViewStateStore } from '@lazybox/platform';
import { ArtifactsPanel } from './ArtifactsPanel';
import { useArtifactRequestsInWindow } from './artifact-events';
import { Icon } from './Icons';
import { requestNavigation } from './navigate-requests';
import { Resizer } from './Resizer';
import { SPLITS } from './resizable-model';
import { useRunner } from './RunnerContext';
import { useWorkspaceCatalog } from './useWorkspaceCatalog';
import type { ViewTarget } from './workspace-model';
import './artifacts.css';

/**
 * A window with no workspace of its own (a chat put in its own window) shows
 * the artifacts asked for in it beside its content, in that window: an
 * artifact card's Open or Compare, the chat header's "N artifacts", a link
 * the agent published. The pane is the Artifacts panel (list and page, one at
 * a time when narrow), resizable from its edge, closed with × or Escape, and
 * **Open in main window** shows the same artifact in the workspace window.
 * On a narrow window it covers the chat instead of squeezing it.
 */
export function DetachedArtifacts({ viewState, sessionId, onFocusMain, children }: {
  viewState?: ViewStateStore;
  /** The conversation this window shows: Send to agent fills its message box, and the source message is revealed here. */
  sessionId: string;
  /** Brings the main window forward (after asking it to show something). */
  onFocusMain?(): void;
  children: ReactNode;
}) {
  const { api, connection } = useRunner();
  const root = viewState?.root || connection.baseUrl;
  /** The workspace whose artifacts the pane shows, and a new value each time something is asked for. */
  const [shown, setShown] = useState<{ collectionId: string; at: number }>();
  const [current, setCurrent] = useState<{ collectionId: string; artifactId: string }>();
  const pane = useRef<HTMLElement>(null);
  const returnTo = useRef<Element | null>(null);
  useArtifactRequestsInWindow(true, request => {
    if (!pane.current?.contains(document.activeElement)) returnTo.current = document.activeElement;
    setShown({ collectionId: request.collectionId, at: Date.now() });
  });
  // Shown or asked again: the pane takes the focus, so Escape closes it and the keys read it.
  useEffect(() => { if (shown) requestAnimationFrame(() => { if (!pane.current?.contains(document.activeElement)) pane.current?.focus({ preventScroll: true }); }); }, [shown?.at]);
  const { collections } = useWorkspaceCatalog(api);
  const close = () => {
    setShown(undefined); setCurrent(undefined);
    const back = returnTo.current;
    requestAnimationFrame(() => {
      if (back instanceof HTMLElement && back.isConnected) back.focus({ preventScroll: true });
      else document.querySelector<HTMLElement>('[data-agent-composer]')?.focus({ preventScroll: true });
    });
  };
  const openInMain = () => {
    if (!shown) return;
    requestNavigation(root, { collectionId: current?.collectionId || shown.collectionId, sessionId: '', commandId: '', artifactId: current?.artifactId || '' });
    onFocusMain?.();
  };
  // A conversation of another window is shown in the main window; this one's source message is revealed here.
  const showSession = (target: ViewTarget) => { if (target.sessionId !== sessionId) { requestNavigation(root, target); onFocusMain?.(); } };
  const keyDown = (event: ReactKeyboardEvent<HTMLElement>) => {
    if (event.key !== 'Escape' || event.defaultPrevented || event.nativeEvent.isComposing) return;
    const target = event.target as HTMLElement;
    // A dialog of the page (portalled, yet its events come here), an open menu, or a field with text keep their Escape.
    if (target.closest('dialog') || pane.current?.querySelector(':popover-open') || ((target instanceof HTMLInputElement || target instanceof HTMLTextAreaElement) && target.value)) return;
    event.preventDefault(); event.stopPropagation(); close();
  };
  return <div className={`detached-split${shown ? ' has-artifacts' : ''}`}>
    {children}
    {shown && <Resizer split={SPLITS.detachedArtifacts} label="artifacts" store={viewState}/>}
    {shown && <aside ref={pane} className="detached-artifacts" aria-label="Artifacts" tabIndex={-1} onKeyDown={keyDown}>
      <div className="workspace-shell detached-artifacts-shell">
      <header className="detached-artifacts-bar">
        <Icon name="artifact" size={14}/>
        <strong>Artifacts</strong>
        <button type="button" className="detached-artifacts-main" title="Show this artifact in the main window" onClick={openInMain}><Icon name="external" size={13}/><span>Open in main window</span></button>
        <button type="button" className="icon-button" aria-label="Close artifacts" title="Close (Esc)" onClick={close}><Icon name="close" size={14}/></button>
      </header>
      <ArtifactsPanel key={shown.collectionId} workspaceId={shown.collectionId} collections={collections} viewState={viewState}
        target={{ collectionId: shown.collectionId, sessionId, commandId: '' }} onShowSession={showSession}
        onCurrentChange={(collectionId, artifactId) => setCurrent(artifactId ? { collectionId, artifactId } : undefined)}/>
      </div>
    </aside>}
  </div>;
}
