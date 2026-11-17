import { useEffect, useId, useRef } from 'react';
import type { ViewStateStore } from '@lazybox/platform';
import { ChatPanel } from './AgentWindow';
import { DraggableDialog, DialogHeader } from './DraggableDialog';

/** Starting a conversation in the workspace, or in a new one without repositories. */
export function NewConversationDialog({ open, quickStart, workspaceID, workspaceName, viewState, onQuickStart, onClose, onStarted }: {
  open: boolean;
  /** Start right away with the last settings. */
  quickStart: boolean;
  workspaceID: string;
  workspaceName?: string;
  viewState?: ViewStateStore;
  onQuickStart(): void;
  onClose(): void;
  onStarted(sessionId: string): void;
}) {
  const dialog = useRef<HTMLDialogElement>(null), titleId = useId();
  useEffect(() => { if (open) dialog.current?.showModal(); else dialog.current?.close(); }, [open]);
  return <DraggableDialog ref={dialog} className="new-session-dialog" onCancel={onClose} onClose={onClose} aria-labelledby={titleId}>
    <DialogHeader><h2 id={titleId}>New conversation</h2><button aria-label="Close new conversation" onClick={onClose}>×</button></DialogHeader>
    <p className="conversation-scope">{workspaceID ? <>In <strong>{workspaceName || 'this workspace'}</strong></> : 'Starts in a new workspace without repositories.'}</p>
    {open && !quickStart && <button className="quick-start-option" onClick={onQuickStart}>Start with last settings</button>}
    {open && <ChatPanel autoStart={quickStart} viewState={viewState} embedded emptyWorkspace={!workspaceID} collectionId={workspaceID} showCommands={false} onSessionChange={onStarted}/>}
  </DraggableDialog>;
}
