import { useMemo, useRef, useState, type KeyboardEvent as ReactKeyboardEvent } from 'react';
import { Icon } from './Icons';
import { CardHead, editable, Kbd, useChatKeys, useInitialFocus } from './request-parts';
import type { RequestCardProps } from './RequestCards';
import { briefPreview, startRequestOf, workspaceWords } from './conversation-request-model';
import { agentModelText } from './conversation-model';
import { agentTitle } from './providers';
import { useRunner } from './RunnerContext';
import { openRelatedConversation } from './conversation-requests';
import { showConversation } from './review-client';
import { openArtifact } from './artifact-events';
import { requestOrchestrate } from './OrchestratorBadge';
import { humanError } from './human-error';

type RowId = 'start' | 'start-new' | 'edit' | 'deny';
type Row = { id: RowId; label: string; detail?: string; key: string; tone: 'allow' | 'always' | 'deny' };
type Draft = { title: string; brief: string; profile: string; model: string; mode: 'same' | 'new' };

const capital = (text: string) => text.charAt(0).toUpperCase() + text.slice(1);
const permissionWords: Record<string, string> = { yolo: 'YOLO', 'read-only': 'Read only', ask: 'Ask', inherit: 'Ask' };

/**
 * An agent asks to start a conversation, because the person asked it to in
 * this chat (`lazybox conversation new`). The card says who asks, the title,
 * the brief, where it would work (and with how many others) and on which
 * agent; Start opens it as a tab beside this one, Start in a new workspace
 * gives it branches of its own, Edit… changes the title, brief, agent or
 * workspace first, and Deny tells the agent why. Letting this conversation
 * start agents itself is the Orchestrator grant, one link away. On a phone
 * (`remote`) it is Start or Deny, as asked.
 */
export function ConversationRequestCard({ request, api, session, onError, position, agentName = 'the agent', remote = false }: RequestCardProps & { remote?: boolean }) {
  const view = useMemo(() => startRequestOf(request), [request.providerDataJson, request.requestId]);
  const { info } = useRunner();
  const profiles = info?.profiles || [];
  const asked: Draft = { title: view.title, brief: view.brief, profile: view.profile, model: view.model, mode: view.mode };
  const [panel, setPanel] = useState<'rows' | 'edit' | 'deny' | 'done'>('rows');
  const [sending, setSending] = useState(false);
  const [draft, setDraft] = useState<Draft>(asked);
  const [reason, setReason] = useState('');
  const [done, setDone] = useState('');
  const [error, setError] = useState('');
  const card = useRef<HTMLElement>(null);
  const busy = useRef(false);
  const agent = capital(agentName);
  const canBranch = !!view.workspace;
  const rows: Row[] = [
    { id: 'start', label: 'Start', detail: view.mode === 'new' ? 'In a new workspace, as asked' : 'In this workspace, as asked', key: 'y', tone: 'allow' },
    ...(!remote && canBranch && view.mode === 'same' ? [{ id: 'start-new', label: 'Start in a new workspace', detail: 'Its own branches, so it does not edit the same files', key: 'w', tone: 'always' } as Row] : []),
    ...(!remote ? [{ id: 'edit', label: 'Edit…', detail: 'Change the title, brief, agent or workspace first', key: 'e', tone: 'always' } as Row] : []),
    { id: 'deny', label: 'Deny…', detail: `Tell ${agentName} why`, key: 'n', tone: 'deny' },
  ];

  const answer = async (body: { decision: 'start' | 'decline'; reason?: string } & Partial<Draft>, after: (startedTitle: string, target: { collectionId: string; sessionId: string }) => void) => {
    if (busy.current) return;
    busy.current = true; setSending(true); setError('');
    // Only what the person changed is sent: empty keeps what the agent asked for.
    const changed = (key: keyof Draft) => body[key] !== undefined && String(body[key]).trim() !== String(asked[key]).trim() ? String(body[key]).trim() : '';
    try {
      const r = await api.answerConversationStart({ requestId: view.id, decision: body.decision, reason: body.reason || '', title: changed('title'), brief: changed('brief'), workspace: changed('mode'), profileId: changed('profile'), model: changed('model') });
      after(r.startedTitle, { collectionId: r.startedCollectionId, sessionId: r.startedSessionId });
      setPanel('done');
    } catch (e) {
      if (body.decision === 'decline') onError(humanError(e)); else setError(humanError(e));
    } finally { busy.current = false; setSending(false); }
  };
  const start = (edits: Partial<Draft> = {}) => answer({ decision: 'start', ...edits }, (title, target) => {
    setDone(`Started ${title || view.title} · ${agent} is told`);
    // The new conversation opens as a tab beside this one (or in its own workspace).
    if (remote || !target.sessionId) return;
    const opened = target.collectionId === view.workspace && openRelatedConversation({ collectionId: view.workspace, sessionId: session, commandId: '' }, { ...target, commandId: '' });
    if (!opened) showConversation({ collectionId: target.collectionId, sessionId: target.sessionId, anywhere: true });
  });
  const deny = () => answer({ decision: 'decline', reason: reason.trim() }, () => setDone(`Declined · ${agent} is told`));
  const focusLater = (selector: string) => requestAnimationFrame(() => card.current?.querySelector<HTMLElement>(selector)?.focus());
  const back = (focus: RowId) => { setPanel('rows'); setError(''); focusLater(`[data-row=${focus}]`); };
  const activate = (row: Row) => {
    if (panel !== 'rows' || sending) return;
    if (row.id === 'start') void start();
    else if (row.id === 'start-new') void start({ mode: 'new' });
    else if (row.id === 'edit') { setDraft(asked); setPanel('edit'); focusLater('.start-edit input[name=title]'); }
    else { setPanel('deny'); focusLater('.deny-reason textarea'); }
  };
  const onKey = (event: { key: string }) => {
    if (panel !== 'rows') return false;
    const digit = /^[1-9]$/.test(event.key) ? Number(event.key) - 1 : -1;
    const row = digit >= 0 ? rows[digit] : rows.find(r => r.key === event.key.toLowerCase());
    if (!row) return false;
    activate(row); return true;
  };
  useChatKeys(card, onKey);
  useInitialFocus(card, '[data-row=start]');
  const cardKeys = (event: ReactKeyboardEvent<HTMLElement>) => {
    if (editable(event.target) || event.metaKey || event.ctrlKey || event.altKey) return;
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      const buttons = [...card.current!.querySelectorAll<HTMLButtonElement>('.decision-row:not(:disabled)')];
      const at = buttons.indexOf(document.activeElement as HTMLButtonElement);
      buttons[(at + (event.key === 'ArrowDown' ? 1 : -1) + buttons.length) % buttons.length]?.focus();
      event.preventDefault(); return;
    }
    if (onKey(event)) event.preventDefault();
  };
  const keys = !position || position.index === 0;
  const where = workspaceWords(view);
  const agentText = agentModelText(agentTitle(view.provider, profiles, 'Agent'), view.model);
  const preview = briefPreview(view.brief);
  const editProfiles = profiles.some(p => p.id === view.profile) ? profiles : [{ id: view.profile, provider: view.provider, label: '' }, ...profiles];
  const submitEdit = () => { if (draft.title.trim()) void start(draft); };
  return <article ref={card} className="request start-request-card" aria-label={`Start a conversation: ${view.title}`} onKeyDown={cardKeys}>
    <CardHead eyebrow="Start a conversation" tool={agentTitle(view.provider, profiles, 'Agent')} position={position}/>
    <h3>{agent} wants to start a conversation: <strong>{view.title}</strong></h3>
    {view.reason && <p className="repository-reason"><span>Why</span>{view.reason}</p>}
    {view.brief && <div className="start-brief"><p className="start-brief-preview">{preview.text}</p>
      {preview.more && <details className="start-brief-full"><summary>Full brief</summary><pre>{view.brief}</pre></details>}</div>}
    <dl className="repository-facts start-facts">
      {view.artifact && <div data-fact="brief"><dt>Brief</dt><dd><Icon name="artifact" size={13}/><span><strong>{view.artifact.title}</strong><small>Artifact · version {view.artifact.version || 1} ·{' '}
        <button type="button" className="link-button start-open-artifact" aria-label={`Open the artifact ${view.artifact.title}`} onClick={() => openArtifact({ collectionId: view.workspace, artifactId: view.artifact!.id, version: view.artifact!.version || undefined })}>Open</button></small></span></dd></div>}
      <div data-fact="workspace" data-shared={(view.mode === 'same' && view.sharedWith > 1) || undefined}><dt>Workspace</dt><dd><Icon name={view.mode === 'new' ? 'branch' : 'folder'} size={13}/><span><strong>{where.label}</strong><small>{where.detail}</small></span></dd></div>
      <div data-fact="agent"><dt>Agent</dt><dd><Icon name="agent" size={13}/><span><strong>{agentText}</strong><small>{view.permission ? `${permissionWords[view.permission] || view.permission} permission, like this conversation` : 'This conversation’s permission'}</small></span></dd></div>
    </dl>
    {panel === 'edit' ? <form className="start-edit" aria-label="Edit before starting" onSubmit={event => { event.preventDefault(); submitEdit(); }}
      onKeyDown={event => {
        if ((event.metaKey || event.ctrlKey) && event.key === 'Enter') { event.preventDefault(); submitEdit(); }
        if (event.key === 'Escape') { event.preventDefault(); event.stopPropagation(); back('edit'); }
      }}>
      <label>Title<input name="title" value={draft.title} maxLength={200} required autoComplete="off" onChange={e => setDraft({ ...draft, title: e.target.value })}/></label>
      <label>Brief<textarea name="brief" rows={5} value={draft.brief} placeholder={view.artifact ? `The artifact “${view.artifact.title}” is the brief` : 'What to do, done when, boundaries'} onChange={e => setDraft({ ...draft, brief: e.target.value })}/></label>
      <div className="start-edit-row">
        <label>Agent<select name="profile" value={draft.profile} onChange={e => { const next = editProfiles.find(p => p.id === e.target.value); setDraft({ ...draft, profile: e.target.value, model: next?.provider === view.provider ? view.model : '' }); }}>
          {editProfiles.map(p => <option key={p.id} value={p.id}>{agentTitle(p.provider, profiles, p.id)}{p.label ? ` · ${p.label}` : ''}</option>)}
        </select></label>
        <label>Model<input name="model" value={draft.model} placeholder="Its default" autoComplete="off" onChange={e => setDraft({ ...draft, model: e.target.value })}/></label>
      </div>
      <fieldset className="start-edit-workspace"><legend>Workspace</legend>
        <label><input type="radio" name="mode" value="same" checked={draft.mode === 'same'} onChange={() => setDraft({ ...draft, mode: 'same' })}/>This one{view.sharedWith > 1 ? ` (shared with ${view.sharedWith} running conversations)` : ''}</label>
        <label><input type="radio" name="mode" value="new" disabled={!canBranch} checked={draft.mode === 'new'} onChange={() => setDraft({ ...draft, mode: 'new' })}/>A new workspace (its own branches)</label>
      </fieldset>
      <div className="row"><small className="start-edit-hint" aria-hidden="true">⌘↵ to start · Esc to go back</small><button type="button" disabled={sending} onClick={() => back('edit')}>Back</button><button type="submit" className="primary" disabled={sending || !draft.title.trim()}>Start</button></div>
    </form>
    : panel === 'deny' ? <form className="deny-reason" onSubmit={event => { event.preventDefault(); void deny(); }}>
      <label>Why not? <small>Optional. {agent} is told, and does not start it another way.</small>
        <textarea rows={2} value={reason} placeholder="e.g. Do it here instead, once the tests pass" onChange={e => setReason(e.target.value)} onKeyDown={event => {
          if (event.key === 'Enter' && !event.shiftKey && !event.nativeEvent.isComposing) { event.preventDefault(); void deny(); }
          if (event.key === 'Escape') { event.preventDefault(); setReason(''); back('deny'); }
        }}/>
      </label>
      <div className="row"><button type="button" disabled={sending} onClick={() => { setReason(''); back('deny'); }}>Back</button><button type="submit" className="danger" disabled={sending}>Deny</button></div>
    </form>
    : panel === 'done' ? <p className="repository-done" role="status"><Icon name="check" size={13}/>{done}</p>
    : <div className="decision-rows" role="group" aria-label="Your decision">
      {rows.map((row, i) => <button key={row.id} type="button" className="decision-row" data-row={row.id} data-tone={row.tone} aria-label={row.label} aria-keyshortcuts={[String(i + 1), row.key].join(' ')} disabled={sending} title={row.detail} onClick={() => activate(row)}>
        {keys && <Kbd>{i + 1}</Kbd>}
        <span className="decision-text"><span>{row.label}</span>{row.detail && <small>{row.detail}</small>}</span>
        {keys && <Kbd>{row.key.toUpperCase()}</Kbd>}
      </button>)}
    </div>}
    {!remote && panel === 'rows' && <button type="button" className="link-button start-grant" onClick={() => requestOrchestrate(session)}>
      <Icon name="shield" size={12}/>Let this conversation start agents (orchestrator grant)…</button>}
    {sending && <small role="status" className="request-sending">Sending…</small>}
    {error && <p className="error" role="alert">{error}</p>}
  </article>;
}
