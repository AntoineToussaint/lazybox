import { useListReorder } from './useListReorder';
import { inOrder } from './workspace-order';
import { showToast } from './toasts';
import { peekProps } from './peek-model';
import { ActionMenu } from './ActionMenu';
import { ActionMenuItems } from './ActionMenuItems';
import { contextMenu } from './ContextMenu';
import { Icon } from './Icons';
import { useEffect, useState } from 'react';
import { Code, ConnectError } from '@lazybox/client-core';
import type { Session } from '@lazybox/protocol';
import { useRunner } from './RunnerContext';
import { sessionStatus } from './agent-status';
import { ConversationProviderIcon, SessionMark } from './LiveIndicator';
import { conversationDetails, conversationName, mergeConversations } from './conversation-model';
import { activityAt, ageLabel } from './attention';
import { archivedAt, isArchivedAway, isPinned, pinnedAt } from './agents-model';
import { InlineConversationRename, useConversationActions } from './ConversationActions';
import type { ViewTarget } from './workspace-model';
import { conversationRowGestures } from './conversation-requests';
import { isMac } from './actions';

/** How a row opens as a tab, on hover (a click shows it in the preview tab). */
const ROW_HINT = `Double-click: keep it in a tab · ${isMac ? '⌘' : 'Ctrl'}-click: open behind`;
import { useUnread } from './useAttention';
import './inbox.css';

/**
 * Conversations, one row each: status mark, title, age. Pinned ones first;
 * archived ones only in History (allWorkspaces), marked. Provider and model
 * are the tooltip. Selecting a conversation attaches a reader; it never
 * starts execution. The row's menu is every conversation's (ConversationActions).
 */
export function ConversationList({ collectionId = '', target, onTarget, compact = false, hideWhenEmpty = false, heading, allWorkspaces = false, sessions: supplied, workspaceName, order, arrange, reviewsOf, nestedReviews = false }: { nestedReviews?: boolean; reviewsOf?(author: Session): Session[]; order?: readonly string[]; arrange?(ids: string[]): Promise<void>; sessions?: Session[]; allWorkspaces?: boolean; compact?: boolean; hideWhenEmpty?: boolean; heading?: string; collectionId?: string; target: ViewTarget; onTarget(target: ViewTarget): void; workspaceName?(id: string): string }) {
  const { api, info, revision, error:connectionError } = useRunner();
  const [search, setSearch] = useState('');
  const [deleted,setDeleted]=useState<Set<string>>(()=>new Set());
  const [history, setHistory] = useState<Session[]>([]);
  const [error, setError] = useState('');
  const [pageCount, setPageCount] = useState(1);
  const [more, setMore] = useState(false);
  const [loading,setLoading]=useState(true);
  const [unsupported, setUnsupported] = useState(false);
  const [renaming, setRenaming] = useState('');
  const [now, setNow] = useState(Date.now);
  useEffect(() => { const timer = setInterval(() => setNow(Date.now()), 30_000); return () => clearInterval(timer); }, []);
  const unread = useUnread();
  const actions = useConversationActions({ onOpen: onTarget, onDeleted: gone => {
    setDeleted(previous => new Set([...previous, gone.id])); setHistory(previous => previous.filter(row => row.id !== gone.id));
    if (target.sessionId === gone.id) onTarget({ ...target, sessionId: '', commandId: '' });
  } });
  useEffect(() => {
    if (supplied) {setLoading(false); return;}
    const control = new AbortController();
    let loading = false, unavailable = false;
    setUnsupported(false);setLoading(true);
    const load = async () => {
      if (loading || unavailable) return;
      loading = true;
      try {
        const loaded: Session[] = [];
        let hasMore = false;
        for (let page = 0; page < pageCount; page++) {
          const result = await api.listHistory({ limit: 30, offset: page * 30, collectionId: allWorkspaces ? undefined : collectionId, search: search.trim() }, { signal: control.signal, timeoutMs: 5000 });
          loaded.push(...result.sessions); hasMore = result.hasMore;
          if (!hasMore) break;
        }
        if (!control.signal.aborted) { setHistory(loaded); setMore(hasMore); setError(''); }
      } catch (error) {
        if (control.signal.aborted) return;
        // A desktop update must not require stopping an older Runner's agents.
        // Its live catalog is usable, but it cannot supply durable history.
        if (ConnectError.from(error).code === Code.Unimplemented) {
          unavailable = true; setUnsupported(true); setMore(false);
        } else setError(String(error));
      } finally { loading = false;if(!control.signal.aborted)setLoading(false); }
    };
    void load(); const timer = setInterval(() => void load(), 3000);
    return () => { control.abort(); clearInterval(timer); };
  }, [api, pageCount, collectionId, search, allWorkspaces, revision, supplied]);
  const open = (session: Session) => onTarget({ collectionId: session.collectionId, sessionId: session.id, commandId: '' });
  const matching = (supplied || mergeConversations(history, info?.sessions || [])).filter(session => !deleted.has(session.id) && (allWorkspaces || session.collectionId === collectionId) && (allWorkspaces || !isArchivedAway(session)) && (!search.trim() || session.title.toLocaleLowerCase().includes(search.trim().toLocaleLowerCase())));
  // Pinned first, in the order they were pinned; the rest keep their order.
  const visible = [...matching.filter(isPinned).sort((a, b) => pinnedAt(a) - pinnedAt(b)), ...matching.filter(session => !isPinned(session))];
  const rows = visible.map(session => ({id: session.conversationId || session.id, session}));
  const ordered = inOrder(rows, order);
  const reorder = useListReorder({
    items: [...ordered.filter(row => isPinned(row.session)), ...ordered.filter(row => !isPinned(row.session))],
    name: row => conversationName(row.session), group: row => isPinned(row.session),
    grip: '.conversation-link', arrange,
    onError: body => showToast({ tone: 'failed', title: 'Conversations not reordered', body }),
  });
  if(hideWhenEmpty&&!visible.length&&!search&&!error)return null;
  return <div className="conversation-navigation" data-peek-list>
    {heading&&<div className="explorer-heading"><span>{heading}</span></div>}
    {error && !connectionError && <p className="error" role="alert">Could not load saved conversations.</p>}
    {unsupported && !collectionId && <p className="module-empty">Live conversations · saved history requires a Runner update.</p>}
    {allWorkspaces && <input autoFocus type="search" className="conversation-filter" aria-label="Search all conversation titles" placeholder="Search all conversation titles…" maxLength={120} value={search} onChange={event => { setSearch(event.target.value); setPageCount(1); }}/> }
    <nav {...(arrange ? reorder.listProps : {})} aria-keyshortcuts={arrange ? "Alt+Shift+ArrowUp Alt+Shift+ArrowDown" : undefined} aria-label={allWorkspaces ? 'All conversations' : collectionId ? 'Workspace conversations' : 'Conversations'}>
      {arrange && <span className="sr-only" role="status" aria-live="polite">{reorder.announcement}</span>}
      {reorder.items.flatMap(({session}) => [
        { session, nested: nestedReviews },
        ...(reviewsOf?.(session) || []).filter(child => !deleted.has(child.id)).map(session => ({ session, nested: true })),
      ]).map(({session, nested}) => {
      const dragProps = arrange && !nested ? reorder.itemProps(session.conversationId || session.id) : undefined;
      const marker = unread(session), title = conversationName(session), status = sessionStatus(session), archived = archivedAt(session) > 0, pinned = isPinned(session);
      const items = actions.items(session, { rename: () => setRenaming(session.id) }), at = activityAt(session);
      const details = [allWorkspaces || !compact ? workspaceName?.(session.collectionId) : '', conversationDetails(session, info?.profiles || [])].filter(Boolean).join(' · ');
      return <div {...dragProps} className="conversation-row" data-review-child={nested || undefined} data-unread={marker || undefined} data-pinned={pinned || undefined} data-archived={archived || undefined} key={session.id}
        onContextMenu={contextMenu('conversation', () => items, { sessionId: session.id, workspaceId: session.collectionId }, `Conversation ${title}`)}
        onKeyDown={event => { dragProps?.onKeyDown(event); if (event.key === 'F2' && renaming !== session.id) { event.preventDefault(); setRenaming(session.id); } }}>
        {renaming === session.id
          ? <div className="conversation-renaming"><SessionMark session={session} unread={marker}/><InlineConversationRename session={session} onDone={() => setRenaming('')}/></div>
          : <button className="conversation-link" {...peekProps({ kind: 'conversation', sessionId: session.id, workspaceId: session.collectionId, title: session.title })} aria-current={target.sessionId===session.id?'page':undefined}
              title={[title, `${status} · ${details}`, ROW_HINT].join('\n')}
              {...conversationRowGestures({ collectionId: session.collectionId, sessionId: session.id, commandId: '' }, () => open(session))}>
              <SessionMark session={session} unread={marker} title={marker ? (marker === 'failed' ? 'Failed · unread' : 'Ready for review · unread') : status}/>
              <span className="sr-only">{marker ? (marker === 'failed' ? 'Failed, unread' : 'Ready for review, unread') : status}{pinned ? ', pinned' : ''}{archived ? ', archived' : ''}: </span>
              <ConversationProviderIcon session={session}/>
              <span className="conversation-label">
                <strong>{title}</strong>
                {!compact && <small className="conversation-agent">{details}</small>}
              </span>
              {pinned && <span className="conversation-pinned" aria-hidden="true"><Icon name="pin" size={10}/></span>}
              {archived && <span className="conversation-archived">Archived</span>}
              <time className="conversation-age" dateTime={at ? new Date(at).toISOString() : undefined}>{ageLabel(at, now)}</time>
            </button>}
        <ActionMenu className="conversation-actions" label={`Actions for conversation ${session.title||session.id}`}><ActionMenuItems items={items}/></ActionMenu>
      </div>; })}</nav>
    {!error && !loading && !visible.length && <p className="module-empty">{search ? 'No conversation titles match this search.' : collectionId ? 'No conversations in this workspace yet.' : 'No standalone conversations yet.'}</p>}
    {actions.dialogs}
    {more && <button onClick={() => setPageCount(value => value + 1)}>Show more conversations</button>}
  </div>;
}
