import { useEffect, useRef, useState } from 'react';
import type { Extension } from '@lazybox/protocol';
import { DraggableDialog, DialogHeader } from './DraggableDialog';
import { Icon } from './Icons';
import { CLAUDE_SWITCHING_WARNING, CLAUDE_TERMS_URL, describeGrant, describeHost, extensionNever, switchedProviders, widening } from './extension-grants';
import { useExtensions } from './useExtensions';
import './extensions.css';

/**
 * The approval sheet (EXTENSIONS.md, "Installing"): exactly what an
 * extension may read and do, and where it may connect, in plain words, with
 * what an update adds marked New. Approving sends back exactly the list shown;
 * the Runner refuses it if the manifest changed meanwhile, and the sheet then
 * shows the new version instead of approving something unseen.
 */
export function ExtensionApprovalSheet({ extension, onClose, onApproved }: { extension: Extension; onClose(): void; onApproved?(extension: Extension): void }) {
  const { value, change } = useExtensions();
  const live = value?.extensions.find(e => e.id === extension.id) || extension;
  const dialog = useRef<HTMLDialogElement>(null), first = useRef<HTMLButtonElement>(null);
  const [shown, setShown] = useState(live);
  const [replaced, setReplaced] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  // Not now (or Done) first: allowing is a deliberate click, never a stray Enter.
  useEffect(() => { dialog.current?.showModal(); first.current?.focus(); }, []);
  // A manifest that changes while the sheet is open is announced, never swapped silently.
  useEffect(() => {
    if (live.grants.join() !== shown.grants.join() || live.hosts.join() !== shown.hosts.join() || live.state === 'broken') { setShown(live); setReplaced(true); }
  }, [live.grants.join(), live.hosts.join(), live.state]);
  const added = widening(shown);
  const update = shown.state === 'review' || (shown.state === 'disabled' && (added.grants.length > 0 || added.hosts.length > 0));
  // Approved and on: the sheet only says what it may do.
  const viewing = shown.state === 'enabled';
  const name = shown.name || shown.id;
  const panels = shown.panels.map(p => p.title).join(', ');
  const approve = async () => {
    setBusy(true); setError(''); setReplaced(false);
    const failed = await change(api => api.approveExtension({ id: shown.id, grants: shown.grants, hosts: shown.hosts }));
    setBusy(false);
    if (failed) { setError(failed); return; }
    onApproved?.(shown); onClose();
  };
  const turnOff = async () => {
    setBusy(true);
    const failed = await change(api => api.setExtensionEnabled({ id: shown.id, enabled: false }));
    setBusy(false);
    if (failed) setError(failed); else onClose();
  };
  const workflow = shown.grants.some(grant => grant === 'answer:preferences' || grant === 'review:archives');
  const grants = shown.grants.map(describeGrant);
  return <DraggableDialog ref={dialog} className="extension-approval" aria-labelledby="extension-approval-title" aria-describedby="extension-approval-summary"
    onCancel={event => { if (busy) event.preventDefault(); else onClose(); }}>
    <DialogHeader>
      <span className="extension-mark" aria-hidden="true"><Icon name={shown.panels[0]?.icon || 'puzzle'} size={20}/></span>
      <div className="extension-approval-title">
        <h2 id="extension-approval-title">{viewing ? `What ${name} may do` : update ? `${name} asks for more` : `Allow ${name}?`}</h2>
        <p className="extension-byline">{[shown.version && `Version ${shown.version}`, shown.author && `by ${shown.author}`].filter(Boolean).join(' · ') || 'An extension in your config folder'}</p>
      </div>
      <button type="button" aria-label="Close without allowing" disabled={busy} onClick={onClose}>×</button>
    </DialogHeader>
    <p id="extension-approval-summary" className="extension-approval-summary">
      {shown.description ? <>{shown.description} </> : null}
      {shown.panels.length ? <>It adds {shown.panels.length === 1 ? 'a panel' : `${shown.panels.length} panels`} ({panels}) that runs in a sandbox, apart from Lazybox.</>
        : workflow ? <>Lazybox runs this workflow on the Runner.</> : <>It has no panel and runs no code: Lazybox follows its account policy.</>}
    </p>
    {shown.accountPolicy && <section aria-labelledby="extension-policy" className="extension-policy">
      <h3 id="extension-policy">At a limit</h3>
      <p>{shown.accountPolicy.switchOnLimit
        ? <>Lazybox moves the conversation to {shown.accountPolicy.accounts.length ? <>the first of <strong>{shown.accountPolicy.accounts.join(', ')}</strong> that can take it</> : 'another of your signed-in accounts of the same provider'}. Without it, you choose on the limit card.</>
        : <>It switches nothing: its policy has switchOnLimit off.</>}</p>
      {switchedProviders(shown).includes('claude') && <p className="extension-caution" role="note"><Icon name="alert" size={13}/> <span>{CLAUDE_SWITCHING_WARNING} <a href={CLAUDE_TERMS_URL} target="_blank" rel="noreferrer">Anthropic’s terms</a></span></p>}
    </section>}
    {update && <p className="extension-approval-update" role="note"><Icon name="alert" size={14}/> An update asks for more than you allowed. Until you allow it, it keeps running with what you allowed before. New items are marked.</p>}
    {replaced && <p className="extension-approval-update" role="alert"><Icon name="refresh" size={14}/> Its manifest changed while you were reading. This is what it asks for now.</p>}
    <section aria-labelledby="extension-can">
      <h3 id="extension-can">It can</h3>
      {grants.length ? <ul className="extension-grants" aria-label="What it can do">{grants.map(g => <li key={g.grant} className={`grant-${g.tone}${added.grants.includes(g.grant) && update ? ' is-new' : ''}`}>
        <Icon name={g.icon} size={15}/>
        <span><strong>{g.title}</strong><small>{g.detail}</small></span>
        {added.grants.includes(g.grant) && update && <em className="extension-new">New</em>}
        <code title="The grant in extension.yaml">{g.grant}</code>
      </li>)}</ul> : <p className="extension-none">Only show its own page. It reads nothing of yours and does nothing in the window.</p>}
    </section>
    <section aria-labelledby="extension-network">
      <h3 id="extension-network">It can connect to</h3>
      {shown.hosts.length ? <ul className="extension-grants" aria-label="Where it can connect">{shown.hosts.map(describeHost).map(h => <li key={h.host} className={`grant-act${added.hosts.includes(h.host) && update ? ' is-new' : ''}`}>
        <Icon name="globe" size={15}/>
        <span><strong>{h.title}</strong><small>{h.detail}</small></span>
        {added.hosts.includes(h.host) && update && <em className="extension-new">New</em>}
        <code>{h.host}</code>
      </li>)}</ul> : <p className="extension-none"><Icon name="lock" size={13}/> {workflow ? 'The Runner uses your configured agent account for analysis.' : 'Nothing: it cannot use the network.'}</p>}
      {shown.hosts.length > 0 && (shown.grants.some(g => g.startsWith('read:'))) && <p className="extension-caution"><Icon name="alert" size={13}/> What it can see, it could send to these sites.</p>}
    </section>
    {!workflow && <details className="extension-never">
      <summary><Icon name="shield" size={14}/> It can never…</summary>
      <ul>{extensionNever.map(line => <li key={line}>{line}</li>)}</ul>
    </details>}
    <p className="extension-path" title={shown.path}><Icon name="folder" size={12}/> <span className="mono">{shown.path}</span></p>
    {error && <p className="settings-error" role="alert">{error}</p>}
    {viewing ? <footer>
      <button type="button" disabled={busy} onClick={() => void turnOff()}>Turn it off</button>
      <button ref={first} type="button" className="primary" data-autofocus onClick={onClose}>Done</button>
    </footer> : <footer>
      <button ref={first} type="button" data-autofocus disabled={busy} onClick={onClose}>Not now</button>
      <button type="button" className="primary" disabled={busy || shown.state === 'broken'} onClick={() => void approve()}>{busy ? 'Allowing…' : update ? 'Allow and keep it on' : 'Allow and turn on'}</button>
    </footer>}
  </DraggableDialog>;
}
