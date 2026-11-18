import { useEffect, useId, useRef } from 'react';
import type { ViewStateStore } from '@lazybox/platform';
import { readLaunchChoices } from './view-state';
import { useRunner } from './RunnerContext';
import { agentTitle } from './providers';
import { Icon } from './Icons';
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
  const { info } = useRunner();
  const last = readLaunchChoices(viewState);
  const profile = info?.profiles.find(value => value.id === last.profile);
  const agent = last.provider ? agentTitle(last.provider, info?.profiles) : "";
  const summary = [agent, profile?.label, last.model].filter(Boolean).join(' · ');
  const dialog = useRef<HTMLDialogElement>(null), titleId = useId();
  useEffect(() => { if (open) dialog.current?.showModal(); else dialog.current?.close(); }, [open]);
  return <DraggableDialog ref={dialog} className="new-session-dialog" onCancel={onClose} onClose={onClose} aria-labelledby={titleId}>
    <DialogHeader><h2 id={titleId}>New conversation</h2><button aria-label="Close new conversation" onClick={onClose}>×</button></DialogHeader>
    <p className="conversation-scope">{workspaceID ? <>In <strong>{workspaceName || 'this workspace'}</strong></> : 'Starts in a new workspace without repositories.'}</p>
    {open && !quickStart && <div className="conversation-start-options">
      <button type="button" className="primary quick-start-option" autoFocus onClick={onQuickStart}><Icon name="play" size={15}/>Start with last settings</button>
      <p>{summary || "Reuse your last agent, account, and model."}</p>
      <h3>Customize your start</h3>
    </div>}
    {open && <ChatPanel key={quickStart ? "last" : "custom"} autoStart={quickStart} viewState={viewState} embedded emptyWorkspace={!workspaceID} collectionId={workspaceID} showCommands={false} onSessionChange={onStarted}/>}
  </DraggableDialog>;
}
