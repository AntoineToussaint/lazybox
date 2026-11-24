import { useMemo, useRef, useState, type KeyboardEvent as ReactKeyboardEvent } from 'react';
import type { Collection } from '@lazybox/protocol';
import { Icon } from './Icons';
import { humanError } from './human-error';
import { CardHead, editable, Kbd, useChatKeys, useInitialFocus } from './request-parts';
import type { RequestCardProps } from './RequestCards';
import { additionKey, pickerQuery, repositoryRequestOf, sourceLabel, sourceTag } from './repository-request-model';
import { shortPath } from './tool-lines';
import { WorkspaceRepositoryDialog } from './WorkspaceRepositoryDialog';

type Row = { id: 'add' | 'choose' | 'decline'; label: string; detail?: string; key: string; tone: 'allow' | 'always' | 'deny' };

const capital = (text: string) => text.charAt(0).toUpperCase() + text.slice(1);

/**
 * An agent asks to add a repository to its workspace instead of reaching
 * outside it: the best match, the branch it gets, and Add (the workspace's own
 * add path, with its progress), Choose another… (the repository picker, the
 * match preselected) or Decline with a reason the agent is told.
 */
export function RepositoryRequestCard({ request, api, onError, position, agentName = 'the agent' }: RequestCardProps) {
  const view = useMemo(() => repositoryRequestOf(request), [request.providerDataJson, request.requestId]);
  const [chosen, setChosen] = useState(0);
  const [mode, setMode] = useState<'ask' | 'adding' | 'declining' | 'sending' | 'done'>('ask');
  const [progress, setProgress] = useState('');
  const [error, setError] = useState('');
  const [reason, setReason] = useState('');
  const [done, setDone] = useState('');
  const [picker, setPicker] = useState<Collection>();
  const card = useRef<HTMLElement>(null);
  const busy = useRef(false);
  const target = view.candidates[chosen];
  const agent = capital(agentName);
  const rows: Row[] = [
    ...(target ? [{ id: 'add', label: `Add ${target.name}`, detail: target.source === 'github' ? 'Clones it, then adds a worktree to this workspace' : 'Adds a worktree to this workspace', key: 'y', tone: 'allow' } as Row] : []),
    { id: 'choose', label: target ? 'Choose another…' : 'Choose a repository…', detail: 'Pick it from your repositories', key: 'c', tone: 'always' },
    { id: 'decline', label: 'Decline…', detail: `Tell ${agentName} why`, key: 'n', tone: 'deny' },
  ];

  const add = async () => {
    if (!target || busy.current) return;
    busy.current = true; setMode('adding'); setError(''); setProgress('Preparing…');
    try {
      let identity = target.identity;
      if (!identity && target.localPath) {
        // A folder the catalog does not know yet: register it, as "Choose folder…" does.
        setProgress(`Registering ${shortPath(target.localPath)}…`);
        identity = (await api.addLocalRepository({ path: target.localPath, name: '' })).repository?.gitIdentity || '';
        if (!identity) throw Error('Lazybox could not register that folder as a repository.');
      }
      let added = false;
      for await (const update of api.prepareWorkspace({ requestKey: additionKey(view.id, identity), collectionId: view.workspace, name: '', branch: '', repositoryIdentities: [identity], repositoryRequestId: view.id }, { timeoutMs: 600000 })) {
        setProgress(update.message);
        if (update.collection) added = true;
      }
      if (!added) throw Error('Preparation ended before the Runner confirmed the repository. Add again to check the same request.');
      setDone(`Added ${target.name} · ${agent} is told`); setMode('done');
    } catch (e) {
      setError(humanError(e)); setMode('ask');
    } finally { busy.current = false; }
  };
  const decline = async () => {
    if (busy.current) return;
    busy.current = true; setMode('sending'); setError('');
    try {
      await api.declineRepositoryRequest({ requestId: view.id, reason: reason.trim() });
      setDone(`Declined · ${agent} is told`); setMode('done');
    } catch (e) { onError(String(e)); setMode('declining'); }
    finally { busy.current = false; }
  };
  const choose = async () => {
    if (busy.current) return;
    try { setError(''); const { collection } = await api.getCollection({ collectionId: view.workspace }); if (collection) setPicker(collection); }
    catch (e) { setError(String(e)); }
  };
  const activate = (row: Row) => {
    if (mode !== 'ask') return;
    if (row.id === 'add') void add();
    else if (row.id === 'choose') void choose();
    else { setMode('declining'); requestAnimationFrame(() => card.current?.querySelector<HTMLTextAreaElement>('.deny-reason textarea')?.focus()); }
  };
  const onKey = (event: { key: string }) => {
    if (mode !== 'ask') return false;
    const digit = /^[1-9]$/.test(event.key) ? Number(event.key) - 1 : -1;
    const row = digit >= 0 ? rows[digit] : rows.find(r => r.key === event.key.toLowerCase());
    if (!row) return false;
    activate(row); return true;
  };
  useChatKeys(card, onKey);
  useInitialFocus(card);
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
  const others = view.candidates.length > 1;
  const heading = target ? <>{agent} asks to add <strong>{target.name}</strong> to this workspace</> : <>{agent} asks for a repository: <strong>“{view.query}”</strong></>;
  return <article ref={card} tabIndex={-1} className="request repository-request-card" aria-label={`Repository request: ${target ? `add ${target.name}` : view.query}`} onKeyDown={cardKeys}>
    <CardHead eyebrow="Repository request" tool={sourceTag(target)} position={position}/>
    <h3>{heading}</h3>
    {view.reason && <p className="repository-reason"><span>Why</span>{view.reason}</p>}
    {target ? <dl className="repository-facts">
      <div><dt>Repository</dt><dd><Icon name="repository" size={13}/><span><strong>{target.name}</strong><small>{sourceLabel(target)}{target.localPath ? <> · <span className="mono" title={target.localPath}>{shortPath(target.localPath)}</span></> : null}</small></span></dd></div>
      <div><dt>Branch</dt><dd><Icon name="branch" size={13}/><span><code>{view.branch || 'the workspace’s branch'}</code><small>This workspace’s branch</small></span></dd></div>
    </dl> : <p className="repository-nomatch">No repository in your catalog matches. Choose one, or decline.</p>}
    {others && <div className="repository-others" role="group" aria-label="Matches">
      <span>Matches</span>
      {view.candidates.map((c, i) => <button key={c.identity || c.localPath} type="button" className="repository-other" aria-pressed={i === chosen} disabled={mode !== 'ask'} title={sourceLabel(c)} onClick={() => setChosen(i)}>{c.name}</button>)}
    </div>}
    {mode === 'declining' || mode === 'sending' ? <form className="deny-reason" onSubmit={event => { event.preventDefault(); void decline(); }}>
      <label>Why not? <small>Optional. {agent} is told, and continues without it.</small>
        <textarea rows={2} value={reason} placeholder="e.g. Use the vendored copy in third_party/ instead" onChange={e => setReason(e.target.value)} onKeyDown={event => {
          if (event.key === 'Enter' && !event.shiftKey && !event.nativeEvent.isComposing) { event.preventDefault(); void decline(); }
          if (event.key === 'Escape') { event.preventDefault(); setMode('ask'); setReason(''); requestAnimationFrame(() => card.current?.querySelector<HTMLElement>('[data-row=decline]')?.focus()); }
        }}/>
      </label>
      <div className="row"><button type="button" disabled={mode === 'sending'} onClick={() => { setMode('ask'); setReason(''); }}>Back</button><button type="submit" className="danger" disabled={mode === 'sending'}>Decline</button></div>
    </form> : mode === 'done' ? <p className="repository-done" role="status"><Icon name="check" size={13}/>{done}</p>
    : <div className="decision-rows" role="group" aria-label="Your decision">
      {rows.map((row, i) => <button key={row.id} type="button" className="decision-row" data-row={row.id} data-tone={row.tone} aria-label={row.label} aria-keyshortcuts={[String(i + 1), row.key].join(' ')} disabled={mode !== 'ask'} title={row.detail} onClick={() => activate(row)}>
        {keys && <Kbd>{i + 1}</Kbd>}
        <span className="decision-text"><span>{row.label}</span>{row.detail && <small>{row.detail}</small>}</span>
        {keys && <Kbd>{row.key.toUpperCase()}</Kbd>}
      </button>)}
    </div>}
    {mode === 'adding' && <div role="status" className="workspace-preparation repository-progress"><span>{progress}</span><progress aria-label={`Adding ${target?.name || 'repository'}`}/></div>}
    {error && <p className="error" role="alert">{error}</p>}
    {picker && <WorkspaceRepositoryDialog workspace={picker} preselected={target?.identity ? [target.identity] : []} initialQuery={pickerQuery(view)} repositoryRequestId={view.id}
      intro={<>{agent} asked for <strong>{target?.name || `“${view.query}”`}</strong>{view.reason ? <>: “{view.reason}”</> : '.'} Choose the repository to add.</>}
      onClose={() => setPicker(undefined)} onSaved={() => { setPicker(undefined); setDone(`Added · ${agent} is told`); setMode('done'); }}/>}
  </article>;
}
