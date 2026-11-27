import { useMemo, useRef, useState, type KeyboardEvent as ReactKeyboardEvent } from 'react';
import { browserRequestHeading, browserRequestOf, decisionRows, pageOrigin, type DecisionRow } from './browser-model';
import { Icon } from './Icons';
import { CardHead, editable, focusDecision, Kbd, useChatKeys, useInitialFocus } from './request-parts';
import type { RequestCardProps } from './RequestCards';

const capital = (text: string) => text.charAt(0).toUpperCase() + text.slice(1);
const EYEBROW = { site: 'New site', download: 'Download', sign_in: 'Sign-in' } as const;

/**
 * The agent's browsing waits for the person (BROWSER.md): a first visit to a
 * site, a download, or a form that sends a password. Allow it once, for the
 * conversation or always in this workspace (sites), or deny with a reason the
 * agent is told.
 */
export function BrowserRequestCard({ request, api, onError, position, agentName = 'the agent' }: RequestCardProps) {
  const view = useMemo(() => browserRequestOf(request), [request.providerDataJson, request.requestId]);
  const agent = capital(view.agentName || agentName);
  const rows = decisionRows(view, agent);
  const [mode, setMode] = useState<'ask' | 'denying' | 'sending' | 'done'>('ask');
  const [reason, setReason] = useState('');
  const [done, setDone] = useState('');
  const card = useRef<HTMLElement>(null);
  const busy = useRef(false);
  const decide = async (row: Pick<DecisionRow, 'decision'> | { decision: 'browse_freely' }, why = '') => {
    if (busy.current) return;
    busy.current = true; setMode('sending');
    try {
      await api.decideBrowserRequest({ requestId: view.id, decision: row.decision, reason: why });
      setDone(row.decision === 'browse_freely' ? 'Full browser access enabled for this conversation' : row.decision === 'deny' ? `Denied · ${agent} is told` : row.decision === 'workspace_previews' ? 'Allowed registered localhost previews in this workspace' : row.decision === 'workspace' ? `Allowed in this workspace · ${view.host}` : row.decision === 'conversation' ? `Allowed for this conversation · ${view.host}` : 'Allowed once');
      setMode('done');
    } catch (e) { onError(String(e)); setMode(row.decision === 'deny' ? 'denying' : 'ask'); }
    finally { busy.current = false; }
  };
  const activate = (row: DecisionRow) => {
    if (mode !== 'ask') return;
    if (row.decision === 'deny') { setMode('denying'); requestAnimationFrame(() => card.current?.querySelector<HTMLTextAreaElement>('.deny-reason textarea')?.focus()); return; }
    void decide(row);
  };
  const deny = () => void decide(rows.find(r => r.decision === 'deny')!, reason.trim());
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
    if (event.repeat || editable(event.target) || event.metaKey || event.ctrlKey || event.altKey) return;
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      focusDecision(event.currentTarget, event.key);
      event.preventDefault(); return;
    }
    if (onKey(event)) event.preventDefault();
  };
  const keys = !position || position.index === 0;
  const origin = pageOrigin(view.url || view.origin);
  return <article ref={card} tabIndex={-1} className="request browser-request-card" data-kind={view.kind} aria-label={`Browser request: ${browserRequestHeading(view, agent)}`} onKeyDown={cardKeys}>
    <CardHead eyebrow={`Browser · ${EYEBROW[view.kind]}`} tool={origin ? (origin.secure ? 'https' : 'http — not secure') : undefined} position={position}/>
    <h3>{browserRequestHeading(view, agent)}</h3>
    {view.url && <p className="browser-request-url">
      <Icon name={view.kind === 'download' ? 'download' : origin?.secure ? 'lock' : 'unlock'} size={13}/>
      <span className="mono" title={view.url}>{view.url}</span>
    </p>}
    {view.reason && <p className="repository-reason"><span>Why</span>{view.reason}</p>}
    {view.kind === 'site' && view.permission === 'yolo' && <p className="browser-request-note"><Icon name="shield" size={12}/>Full access is on for files and commands. Choose how this conversation can browse.</p>}
    {view.kind === 'sign_in' && <p className="browser-request-note"><Icon name="lock" size={12}/>The form holds a password. {agent} never types passwords; this one was typed into the page. Allowing submits it once.</p>}
    {view.kind === 'download' && <p className="browser-request-note"><Icon name="download" size={12}/>The file is saved for {agent} to read. Nothing is opened or run.</p>}
    {mode === 'denying' || (mode === 'sending' && reason) ? <form className="deny-reason" onSubmit={event => { event.preventDefault(); deny(); }}>
      <label>Why not? <small>Optional. {agent} is told, and can try another way.</small>
        <textarea rows={2} value={reason} placeholder={view.kind === 'site' ? 'e.g. Use the docs in the repository instead' : 'e.g. Not needed'} onChange={e => setReason(e.target.value)} onKeyDown={event => {
          if (event.key === 'Enter' && !event.shiftKey && !event.nativeEvent.isComposing) { event.preventDefault(); deny(); }
          if (event.key === 'Escape') { event.preventDefault(); setMode('ask'); setReason(''); requestAnimationFrame(() => card.current?.querySelector<HTMLElement>('[data-row=deny]')?.focus()); }
        }}/>
      </label>
      <div className="row"><button type="button" disabled={mode === 'sending'} onClick={() => { setMode('ask'); setReason(''); }}>Back</button><button type="submit" className="danger" disabled={mode === 'sending'}>Deny</button></div>
    </form> : mode === 'done' ? <p className="repository-done" role="status"><Icon name="check" size={13}/>{done}</p>
    : <div className="decision-rows" role="group" aria-label="Your decision">
      {rows.map((row, i) => <button key={row.decision} type="button" className="decision-row" data-row={row.decision} data-tone={row.tone} aria-label={row.label} aria-keyshortcuts={[String(i + 1), row.key].join(' ')} disabled={mode !== 'ask'} title={row.detail} onClick={() => activate(row)}>
        {keys && <Kbd>{i + 1}</Kbd>}
        <span className="decision-text"><span>{row.label}</span><small>{row.detail}</small></span>
        {keys && <Kbd>{row.key.toUpperCase()}</Kbd>}
      </button>)}
      {view.kind === 'site' && <button type="button" className="decision-row" disabled={mode !== 'ask'} onClick={() => void decide({ decision: 'browse_freely' })}>
        <span className="decision-text"><span>Allow and enable full browser access</span><small>Open new sites in this conversation without asking. Downloads and sign-ins still ask. Change this in the Agent browser menu.</small></span>
      </button>}
    </div>}
  </article>;
}
