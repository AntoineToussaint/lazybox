import { useState } from 'react';
import type { Extension } from '@lazybox/protocol';
import { ConfirmDialog } from './ConfirmDialog';
import { ExtensionApprovalSheet } from './ExtensionApprovalSheet';
import { CLAUDE_SWITCHING_WARNING, CLAUDE_TERMS_URL, describeGrant, describeHost, extensionStateLabel, switchedProviders, widening } from './extension-grants';
import { Icon } from './Icons';
import { SettingsGroup, SettingsHeader } from './settings-parts';
import { useExtensions } from './useExtensions';
import './extensions.css';

/**
 * Settings → Extensions: every extension folder with its state, what it may
 * do, and the switch, review and removal; the bundled examples; where the
 * folder is and how to make one. Approving always goes through the sheet.
 */
export function ExtensionsSettings() {
  const { value, error, change } = useExtensions();
  const [reviewing, setReviewing] = useState<Extension>();
  const [removing, setRemoving] = useState<Extension>();
  const [problem, setProblem] = useState('');
  const [busy, setBusy] = useState('');
  const [copied, setCopied] = useState(false);
  const run = async (id: string, act: Parameters<typeof change>[0]) => {
    setBusy(id); setProblem('');
    const failed = await change(act);
    setBusy('');
    if (failed) setProblem(failed);
  };
  const toggle = (ext: Extension, on: boolean) => {
    const added = widening(ext);
    // Turning on what was never approved, or asks for more, is the sheet's.
    if (on && (ext.state === 'new' || added.grants.length || added.hosts.length)) { setReviewing(ext); return; }
    void run(ext.id, api => api.setExtensionEnabled({ id: ext.id, enabled: on }));
  };
  const add = async (example: string) => {
    setBusy(example); setProblem('');
    let added: Extension | undefined;
    const failed = await change(async api => { const list = await api.installExampleExtension({ example }); added = list.extensions.find(e => e.id === example); return list; });
    setBusy('');
    if (failed) setProblem(failed); else if (added) setReviewing(added);
  };
  const copy = (text: string) => void navigator.clipboard.writeText(text).then(() => { setCopied(true); setTimeout(() => setCopied(false), 1500); }, () => undefined);
  const list = value?.extensions || [];
  return <div className="extensions-settings">
    <SettingsHeader title="Extensions" description={<>Panels you add as a folder of HTML, JavaScript and CSS, and account policies such as switching subscriptions at a limit. Each runs in a sandbox, apart from Lazybox, and can read and do only what you allow here. Show a panel from <strong>Panels → New Extensions panel</strong>.</>}/>
    {(error && !value) && <p className="settings-error" role="alert">{error}</p>}
    {problem && <p className="settings-error" role="alert">{problem}</p>}
    {!value && !error && <p className="settings-empty" role="status">Loading extensions…</p>}
    {value && <>
      <SettingsGroup title="Installed">
        {list.length ? <ul className="extension-cards" aria-label="Your extensions">{list.map(ext => {
          const added = widening(ext), on = ext.state === 'enabled' || ext.state === 'review';
          return <li key={ext.id} className={`extension-card state-${ext.state}`} aria-label={ext.name || ext.id}>
            <span className="extension-mark" aria-hidden="true"><Icon name={ext.state === 'broken' ? 'alert' : ext.panels[0]?.icon || 'puzzle'} size={18}/></span>
            <div className="extension-card-body">
              <div className="extension-card-title"><strong>{ext.name || ext.id}</strong>{ext.version && <small>{ext.version}</small>}<span className={`extension-chip state-${ext.state}`}>{extensionStateLabel[ext.state]}</span></div>
              {ext.description && <p>{ext.description}</p>}
              {ext.state === 'broken' ? <ul className="extension-problems" aria-label={`Problems in ${ext.id}/extension.yaml`}>
                {ext.error && <li>{ext.error}</li>}
                {ext.issues.filter(i => i.severity === 'error').map((i, n) => <li key={n}><span className="where">line {i.line}</span> {i.message}</li>)}
              </ul> : <ul className="extension-summary" aria-label={`What ${ext.name || ext.id} may do`}>
                {ext.grants.map(g => { const d = describeGrant(g); return <li key={g} className={`grant-${d.tone}${added.grants.includes(g) ? ' is-new' : ''}`} title={d.detail}><Icon name={d.icon} size={12}/>{d.title}{added.grants.includes(g) && ext.state !== 'new' ? ' · new' : ''}</li>; })}
                {ext.hosts.map(h => <li key={h} className={`grant-act${added.hosts.includes(h) ? ' is-new' : ''}`} title={h}><Icon name="globe" size={12}/>{describeHost(h).title}</li>)}
                {!ext.grants.length && !ext.hosts.length && <li className="grant-read"><Icon name="lock" size={12}/>Nothing of yours; no network</li>}
              </ul>}
              {on && ext.accountPolicy && switchedProviders(ext).includes('claude') && <p className="extension-caution" role="note"><Icon name="alert" size={13}/> <span>{CLAUDE_SWITCHING_WARNING} <a href={CLAUDE_TERMS_URL} target="_blank" rel="noreferrer">Anthropic’s terms</a></span></p>}
              <small className="extension-path mono" title={ext.path}>{ext.path}</small>
            </div>
            <div className="extension-card-actions">
              {ext.state !== 'broken' && <span className="settings-switch" title={on ? 'Turn off' : 'Turn on'}><input type="checkbox" role="switch" aria-label={`${ext.name || ext.id} on`} checked={on} disabled={busy === ext.id} onChange={event => toggle(ext, event.target.checked)}/></span>}
              {(ext.state === 'new' || ext.state === 'review') && <button type="button" className="primary" onClick={() => setReviewing(ext)}>{ext.state === 'new' ? 'Review' : 'Review what’s new'}</button>}
              {ext.state === 'enabled' && <button type="button" onClick={() => setReviewing(ext)}>Access</button>}
              <button type="button" className="quiet" aria-label={`Remove ${ext.name || ext.id}`} title="Remove" onClick={() => setRemoving(ext)}><Icon name="close" size={13}/></button>
            </div>
          </li>;
        })}</ul> : <p className="settings-empty">No extensions yet. Add an example below, or make your own.</p>}
      </SettingsGroup>
      {value.examples.length > 0 && <SettingsGroup title="Examples" description="Bundled with Lazybox. Adding one copies its folder into yours; you still approve what it may do.">
        <ul className="extension-cards" aria-label="Example extensions">{value.examples.map(x => <li key={x.id} className="extension-card">
          <span className="extension-mark" aria-hidden="true"><Icon name="puzzle" size={18}/></span>
          <div className="extension-card-body"><div className="extension-card-title"><strong>{x.name}</strong></div><p>{x.description}</p></div>
          <div className="extension-card-actions">{x.installed ? <small className="muted">Added</small> : <button type="button" disabled={!!busy} onClick={() => void add(x.id)}>{busy === x.id ? 'Adding…' : 'Add'}</button>}</div>
        </li>)}</ul>
      </SettingsGroup>}
      <SettingsGroup title="Make your own" description="A folder with an extension.yaml and its pages. Lazybox reloads an open panel when you save a file in it.">
        <div className="settings-row"><div className="settings-row-text"><span className="settings-row-label">Extensions folder</span><small>One folder per extension, named by its id.</small></div>
          <div className="settings-row-control extension-folder"><code title={value.directory}>{value.directory}</code><button aria-label="Copy the extensions folder path" title="Copy path" onClick={() => copy(value.directory)}><Icon name="copy" size={13}/>{copied ? ' Copied' : ''}</button></div></div>
        <div className="settings-row is-stacked"><div className="extension-commands">
          <code>lazybox extension new my-panel</code><small>A starting extension, with the bridge’s types for your editor</small>
          <code>lazybox extension validate</code><small>Every extension checked, problems by file:line</small>
          <code>lazybox extension list</code><small>What each may do, and whether it is on</small>
        </div></div>
      </SettingsGroup>
    </>}
    {reviewing && <ExtensionApprovalSheet extension={reviewing} onClose={() => setReviewing(undefined)}/>}
    {removing && <RemoveExtensionDialog extension={removing} onClose={() => setRemoving(undefined)} onRemove={() => run(removing.id, api => api.removeExtension({ id: removing.id })).then(() => setRemoving(undefined))}/>}
  </div>;
}

function RemoveExtensionDialog({ extension, onClose, onRemove }: { extension: Extension; onClose(): void; onRemove(): Promise<void> }) {
  return <ConfirmDialog title={`Remove ${extension.name || extension.id}?`} closeLabel="Keep it" cancelLabel="Keep it" confirm="Remove" working="Removing…" onClose={onClose} onConfirm={onRemove}>
    This deletes its folder, <span className="mono">{extension.path}</span>, and what you allowed it. Its open panels close.
  </ConfirmDialog>;
}
