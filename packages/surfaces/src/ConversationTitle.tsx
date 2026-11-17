import { useEffect, useId, useRef, useState, type FormEvent } from 'react';
import type { RunnerClient } from '@lazybox/client-core';
import type { Session } from '@lazybox/protocol';
import type { branchSummary } from './conversation-model';
import { Icon } from './Icons';
import { showMenuNotice } from './ContextMenu';
import { humanError } from './human-error';

type Branch = ReturnType<typeof branchSummary>;

/** Asks the header showing `sessionId` to rename it in place (the ⋯ menu's Rename…). */
export const RENAME_EVENT = 'lazybox:rename-conversation';
export function requestRename(sessionId: string): boolean {
  return !window.dispatchEvent(new CustomEvent(RENAME_EVENT, { detail: { sessionId }, cancelable: true }));
}

/** How long a click waits to be sure it is not the first of a double-click. */
const DOUBLE_CLICK_MS = 260;

/**
 * The conversation's title: up to two lines, the whole title and the first
 * message in its tooltip. A click shows the first message; a double-click (or
 * F2, or the pencil) renames it in place: Enter saves, Escape puts it back.
 * A title that is only the workspace folder's (generated) name is no title:
 * the header leads with the branch until the conversation gets a real one.
 */
export function ConversationTitle({ api, session, sessionID, realTitle, suggestedTitle, firstMessage, firstMessageLoading = false, branch, onRenamed, onError }: {
  api: RunnerClient;
  session?: Session;
  sessionID: string;
  realTitle: string;
  suggestedTitle: string;
  /** The conversation's first message, whole: the tooltip and the "First message" popover. */
  firstMessage: string;
  /** The conversation's start is still being read: no first message to show yet. */
  firstMessageLoading?: boolean;
  branch: Branch;
  onRenamed(session: Session | undefined): void;
  onError(message: string): void;
}) {
  const [renaming, setRenaming] = useState(false);
  const [conversationName, setConversationName] = useState('');
  const [savingName, setSavingName] = useState(false);
  const popoverId = useId(), popover = useRef<HTMLDivElement>(null), titleButton = useRef<HTMLButtonElement>(null);
  const clickTimer = useRef<ReturnType<typeof setTimeout>>(undefined);
  const startRename = () => {
    if (!session) return;
    clearTimeout(clickTimer.current); popover.current?.hidePopover?.();
    setConversationName(realTitle || suggestedTitle); setRenaming(true);
  };
  const latestStart = useRef(startRename); latestStart.current = startRename;
  useEffect(() => {
    const rename = (event: Event) => { if ((event as CustomEvent<{ sessionId: string }>).detail?.sessionId !== sessionID) return; event.preventDefault(); latestStart.current(); };
    window.addEventListener(RENAME_EVENT, rename);
    return () => { window.removeEventListener(RENAME_EVENT, rename); clearTimeout(clickTimer.current); };
  }, [sessionID]);
  const saveName = async (event: FormEvent) => {
    event.preventDefault(); const title = conversationName.trim();
    if (savingName || !title) return;
    if (title === session?.title) { setRenaming(false); return; }
    setSavingName(true);
    try { const result = await api.renameConversation({ sessionId: sessionID, title }); onRenamed(result.session); setRenaming(false); }
    catch (error) { onError(humanError(error)); } finally { setSavingName(false); }
  };
  const showFirstMessage = () => {
    const anchor = titleButton.current?.getBoundingClientRect(), panel = popover.current;
    if (!anchor || !panel) return;
    panel.style.left = `${Math.max(8, Math.min(anchor.left, window.innerWidth - 448))}px`;
    panel.style.top = `${Math.max(8, Math.min(anchor.bottom + 6, window.innerHeight - 200))}px`;
    panel.showPopover?.();
  };

  if (renaming) return <form className="conversation-rename" onSubmit={saveName}>
    <input autoFocus required maxLength={120} aria-label="Conversation name" value={conversationName} disabled={savingName}
      onFocus={event => event.target.select()}
      onChange={event => setConversationName(event.target.value)}
      onKeyDown={event => { if (event.key === 'Escape' && !savingName) { event.preventDefault(); event.stopPropagation(); setRenaming(false); } }}/>
    <button className="icon-button" aria-label="Save conversation name" title="Save (Enter)" disabled={savingName || !conversationName.trim()}><Icon name="check" size={14}/></button>
    <button type="button" className="icon-button" aria-label="Cancel renaming" title="Cancel (Esc)" disabled={savingName} onClick={() => setRenaming(false)}><Icon name="close" size={14}/></button>
  </form>;

  const opening = firstMessage.trim();
  const tooltip = [realTitle, opening && opening !== realTitle ? `First message: ${opening.length > 600 ? opening.slice(0, 600).trimEnd() + '…' : opening}` : '', firstMessageLoading ? 'Double-click to rename' : 'Click for the first message · double-click to rename'].filter(Boolean).join('\n\n');
  return <div className="conversation-title" data-untitled={realTitle ? undefined : true}>
    {realTitle
      ? <h2><button ref={titleButton} type="button" className="conversation-title-text" title={tooltip} aria-haspopup="dialog" disabled={!session}
        onClick={event => { clearTimeout(clickTimer.current); if (event.detail > 1) return; if (event.detail === 0) { showFirstMessage(); return; } clickTimer.current = setTimeout(showFirstMessage, DOUBLE_CLICK_MS); }}
        onDoubleClick={startRename}
        onKeyDown={event => { if (event.key === 'F2') { event.preventDefault(); startRename(); } }}>{realTitle}</button></h2>
      : <h2 className="chat-branch-title">{branch
        ? <span className="chat-branch" title={branch.title}><Icon name="branch" size={13}/><span className="chat-branch-name">{branch.name}</span>{branch.detail && <small>{branch.detail}</small>}</span>
        : <span className="chat-untitled">New conversation</span>}</h2>}
    {realTitle
      ? <button type="button" className="icon-button conversation-rename-button" aria-label="Rename conversation" title="Rename (double-click the title, F2)" disabled={!session} onClick={startRename}><Icon name="pencil" size={13}/></button>
      : <button type="button" className="conversation-name-it" aria-label="Rename conversation" title={suggestedTitle ? `Name it “${suggestedTitle}”` : 'Give this conversation a name'} disabled={!session} onClick={startRename}><Icon name="pencil" size={12}/><span>Name it</span></button>}
    {realTitle && !firstMessageLoading && <div ref={popover} id={popoverId} popover="auto" className="first-message" role="dialog" aria-label="First message">
      <header><strong>First message</strong><button type="button" className="icon-button" aria-label="Close" title="Close (Esc)" onClick={() => popover.current?.hidePopover?.()}><Icon name="close" size={13}/></button></header>
      {opening ? <p className="first-message-text">{opening}</p> : <p className="first-message-empty">Nothing was said yet.</p>}
      <footer>
        {opening && <button type="button" onClick={() => { void navigator.clipboard?.writeText(opening).then(() => showMenuNotice('First message copied'), () => onError('Could not copy the first message')); }}><Icon name="copy" size={12}/>Copy</button>}
        <button type="button" onClick={startRename}><Icon name="pencil" size={12}/>Rename…</button>
      </footer>
    </div>}
  </div>;
}
