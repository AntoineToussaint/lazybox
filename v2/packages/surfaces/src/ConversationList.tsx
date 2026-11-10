import { ActionMenu } from './ActionMenu';
import { DeleteConversationDialog } from './DeleteConversationDialog';
import { Icon } from './Icons';
import { useEffect, useState } from 'react';
import { Code, ConnectError } from '@lazybox/client-core';
import type { Session } from '@lazybox/protocol';
import { useRunner } from './RunnerContext';
import { sessionStatus } from './agent-status';
import { conversationDetails, sessionAccountLabel, mergeConversations } from './conversation-model';
import type { ViewTarget } from './workspace-model';

/** Selecting a conversation attaches a reader; it never starts execution. */
export function ConversationList({ collectionId = '', target, onTarget, compact = false, hideWhenEmpty = false, heading, allWorkspaces = false, sessions: supplied }: { sessions?: Session[]; allWorkspaces?: boolean; compact?: boolean; hideWhenEmpty?: boolean; heading?: string; collectionId?: string; target: ViewTarget; onTarget(target: ViewTarget): void }) {
  const { api, info, revision, error:connectionError } = useRunner();
  const [search, setSearch] = useState('');
  const [deleting,setDeleting]=useState<Session>();
  const [deleted,setDeleted]=useState<Set<string>>(()=>new Set());
  const [history, setHistory] = useState<Session[]>([]);
  const [error, setError] = useState('');
  const [pageCount, setPageCount] = useState(1);
  const [more, setMore] = useState(false);
  const [loading,setLoading]=useState(true);
  const [unsupported, setUnsupported] = useState(false);
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
  const visible = (supplied || mergeConversations(history, info?.sessions || [])).filter(session => !deleted.has(session.id) && (allWorkspaces || session.collectionId === collectionId) && (!search.trim() || session.title.toLocaleLowerCase().includes(search.trim().toLocaleLowerCase())));
  if(hideWhenEmpty&&!visible.length&&!search&&!error)return null;
  return <div className="conversation-navigation">
    {heading&&<div className="explorer-heading"><span>{heading}</span></div>}
    {error && !connectionError && <p className="error" role="alert">Could not load saved conversations.</p>}
    {unsupported && !collectionId && <p className="module-empty">Live conversations · saved history requires a Runner update.</p>}
    {allWorkspaces && <input autoFocus type="search" className="conversation-filter" aria-label="Search all conversation titles" placeholder="Search all conversation titles…" maxLength={120} value={search} onChange={event => { setSearch(event.target.value); setPageCount(1); }}/> }
    <nav aria-label={allWorkspaces ? 'All conversations' : collectionId ? 'Workspace conversations' : 'Conversations'}>{visible.map(session => <div className="conversation-row" key={session.id}><button className="conversation-link" aria-current={target.sessionId===session.id?'page':undefined} onClick={() => onTarget({ collectionId: session.collectionId, sessionId: session.id, commandId: '' })}>
      <span className={`session-dot status-${session.closed?'saved':session.status}`} title={sessionStatus(session)}/><span className="conversation-label"><strong>{session.title || 'Untitled conversation'}</strong><small className="conversation-agent" title={conversationDetails(session,info?.profiles||[])+' · '+sessionStatus(session)}>{session.model?`${session.provider==='codex'?'Codex':'Claude'} · ${session.model}`:session.provider==='codex'?'Codex':'Claude'}{info?.profiles.find(profile=>profile.id===session.profileId)?.label&&session.profileId!=='claude-default'&&session.profileId!=='codex-default'?` · ${sessionAccountLabel(session,info?.profiles.find(profile=>profile.id===session.profileId))}`:''}</small><span className={session.closed||['ready','done','stopped'].includes(session.status)?'sr-only':'conversation-state'}>{sessionStatus(session)}</span></span>
    </button><ActionMenu className="conversation-actions" label={`Actions for conversation ${session.title||session.id}`} ><button className="danger" onClick={event=>{event.currentTarget.closest('details')!.open=false;setDeleting(session);}}>Delete conversation…</button></ActionMenu></div>)}</nav>
    {!error && !loading && !visible.length && <p className="module-empty">{search ? 'No conversation titles match this search.' : collectionId ? 'No conversations in this workspace yet.' : 'No standalone conversations yet.'}</p>}
    {deleting&&<DeleteConversationDialog session={deleting} onClose={()=>setDeleting(undefined)} onDeleted={()=>{setDeleted(previous=>new Set([...previous,deleting.id]));setHistory(previous=>previous.filter(row=>row.id!==deleting.id));if(target.sessionId===deleting.id)onTarget({...target,sessionId:'',commandId:''});setDeleting(undefined);}}/>}
    {more && <button onClick={() => setPageCount(value => value + 1)}>Show more conversations</button>}
  </div>;
}
