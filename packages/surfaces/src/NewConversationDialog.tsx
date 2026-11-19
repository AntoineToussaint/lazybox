import { useEffect, useId, useRef } from 'react';
import type { ViewStateStore } from '@lazybox/platform';
import { conversationDefaults } from './view-state';
import { useRunner } from './RunnerContext';
import { agentTitle } from './providers';
import { Icon } from './Icons';
import type { ConversationCopy } from './NewSessionForm';
import { ChatPanel } from './AgentWindow';
import { DraggableDialog, DialogHeader } from './DraggableDialog';

/** Starting a conversation in the workspace, or in a new one without repositories. */
export function NewConversationDialog({ open, quickStart, copy, workspaceID, workspaceName, viewState, onQuickStart, onClose, onStarted }: {
  open: boolean;
  /** Start right away with the last settings. */
  quickStart: boolean;
  copy?: ConversationCopy;
  workspaceID: string;
  workspaceName?: string;
  viewState?: ViewStateStore;
  onQuickStart(): void;
  onClose(): void;
  onStarted(sessionId: string, collectionId?: string): void;
}) {
  const { info } = useRunner();
  const last = conversationDefaults(viewState, info);
  const profile = info?.profiles.find(value => value.id === last.profile);
  const agent = last.provider ? agentTitle(last.provider, info?.profiles) : "";
  const summary = [agent, profile?.label, last.model].filter(Boolean).join(' · ');
  const dialog = useRef<HTMLDialogElement>(null), titleId = useId();
  useEffect(() => { if (open) dialog.current?.showModal(); else dialog.current?.close(); }, [open]);
  return <DraggableDialog ref={dialog} className="new-session-dialog" onCancel={onClose} onClose={onClose} aria-labelledby={titleId}>
    <DialogHeader><h2 id={titleId}>{copy ? 'Clone conversation' : 'New conversation'}</h2><button aria-label="Close new conversation" onClick={onClose}>×</button></DialogHeader>
    <p className="conversation-scope">{workspaceID ? <>In <strong>{workspaceName || 'this workspace'}</strong></> : 'Starts in a new workspace without repositories.'}</p>
    {open && !quickStart && !copy && <div className="conversation-start-options">
      <button type="button" className="primary quick-start-option" autoFocus onClick={onQuickStart}><Icon name="play" size={15}/>Start with last settings</button>
      <p>{summary || "Reuse your last agent, account, and model."}</p>
    </div>}
    {open && <ChatPanel key={quickStart ? "last" : "custom"} autoStart={quickStart} copy={copy} viewState={viewState} embedded emptyWorkspace={!workspaceID} collectionId={workspaceID} showCommands={false} onSessionChange={onStarted}/>}
  </DraggableDialog>;
}
