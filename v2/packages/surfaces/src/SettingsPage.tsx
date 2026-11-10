import {KeyboardSettings} from './KeyboardSettings';
import { DraggableDialog, DialogHeader } from './DraggableDialog';
import { useEffect, useRef, useState, type FormEvent } from 'react';
import type { Profile, SubscriptionEnrollment, UsageWindow } from '@lazybox/protocol';
import type { ViewStateStore } from '@lazybox/platform';
import { useRunner } from './RunnerContext';
import { accountLabel } from './agent-status';
import { Icon } from './Icons';
import './settings.css';
import { RepositoryBrowser } from './RepositoryBrowser';
import { AgentSettings, PricingSettings } from './RunnerSettings';


function bareWindowName(window: UsageWindow) {
  if (window.name.startsWith('model:')) return `Weekly · ${window.name.slice(6)}`;
  if (window.name === 'seven_day_oauth_apps') return 'Weekly · OAuth apps';
  if (window.name === 'seven_day_opus') return 'Weekly · Opus';
  if (window.name === 'seven_day_sonnet') return 'Weekly · Sonnet';
  if (window.durationMinutes === 300n || window.name === 'five_hour') return '5-hour limit';
  if (window.durationMinutes === 10080n || window.name.startsWith('seven_day')) return 'Weekly limit';
  if (window.durationMinutes > 0n) return `${Number(window.durationMinutes) / 60}-hour limit`;
  return window.name.endsWith(':primary') ? 'Primary limit' : window.name.endsWith(':secondary') ? 'Secondary limit' : window.name.replaceAll('_', ' ');
}
function windowName(window: UsageWindow) {
 const bare=bareWindowName(window);
 const bucket=window.name.includes(':') ? window.name.split(':')[0] : '';
 return bucket && bucket!=='codex' && bucket!=='model' ? `${bucket.replaceAll('_',' ')} · ${bare}` : bare;
}
function resetLabel(epoch: bigint) {
  if (epoch <= 0n) return '';
  const remaining = Number(epoch) * 1000 - Date.now();
  if (remaining <= 0) return 'Reported window has ended';
  const minutes = Math.ceil(remaining / 60_000), hours = Math.floor(minutes / 60);
  return hours >= 24 ? `Resets in ${Math.floor(hours / 24)}d ${hours % 24}h` : hours ? `Resets in ${hours}h ${minutes % 60}m` : `Resets in ${minutes}m`;
}
const terminal = (state = '') => ['added', 'failed', 'cancelled', 'expired'].includes(state);
export function SettingsPage({ viewState, openSignIn, copySignInCode, chooseRepositoryFolder }: { chooseRepositoryFolder?(): Promise<string | null>; viewState?: ViewStateStore; openSignIn?(url: string): Promise<void>; copySignInCode?(code: string): Promise<void> }) {
  const { api, info, refresh } = useRunner();
  const [tab, setTab] = useState<'subscriptions' | 'repositories' | 'agents' | 'pricing' | 'keyboard'>('subscriptions');
  const storageKey = `${viewState?.scope || viewState?.key}:subscription-enrollment`;
  const [flowID, setFlowID] = useState(() => { try { return viewState?.getItem(storageKey) || ''; } catch { return ''; } });
  const [restoreRequest, setRestoreRequest] = useState<{ key: string; provider: string } | undefined>(() => { try { const value = JSON.parse(viewState?.getItem(`${storageKey}:request`) || 'null'); return value && typeof value.key === 'string' && ['codex', 'claude'].includes(value.provider) ? value : undefined; } catch { return undefined; } });
  const [flow, setFlow] = useState<SubscriptionEnrollment>();
  const [adding, setAdding] = useState(false);
  const [provider, setProvider] = useState<'claude' | 'codex'>('codex');
  const [label, setLabel] = useState('');
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState('');
  const [copied, setCopied] = useState(false);
  const [copyError, setCopyError] = useState('');
  useEffect(() => { setCopied(false); setCopyError(''); }, [flow?.id, flow?.userCode]);
  const [refreshing, setRefreshing] = useState<ReadonlySet<string>>(() => new Set());
  const usageInFlight = useRef(new Set<string>());
  const [refreshErrors, setRefreshErrors] = useState<Record<string, string>>({});
  const [updated, setUpdated] = useState<Record<string, Profile>>({});
  const dialog = useRef<HTMLDialogElement>(null), labelInput = useRef<HTMLInputElement>(null);
  const attempt = useRef<{ key: string; provider: string; label: string } | undefined>(undefined);
  const inFlight = useRef(false);
  const autoOpen = useRef(false);
  const dismissed = useRef(new Set<string>());
  const lifetime = useRef<AbortController | null>(null);
  useEffect(() => { const controller = new AbortController(); lifetime.current = controller; return () => controller.abort(); }, []);
  const rememberFlow = (id: string) => {
    setFlowID(id);
    try { viewState?.setItem(storageKey, id); } catch { setError('Could not retain this sign-in view. The Runner still owns the request.'); }
  };
  useEffect(() => {
    if (adding) { dialog.current?.showModal(); if (!flowID) labelInput.current?.focus(); }
    else dialog.current?.close();
  }, [adding]);
  useEffect(() => {
    const pending = info?.pendingEnrollments.find(value => !dismissed.current.has(value.id));
    if (!flowID && pending) rememberFlow(pending.id);
  }, [info, flowID]);
  useEffect(() => {
    if (flowID || !restoreRequest) return;
    const controller = new AbortController();
    void api.getSubscriptionEnrollment({ requestKey: restoreRequest.key, provider: restoreRequest.provider }, { signal: controller.signal, timeoutMs: 10000 }).then(next => {
      if (controller.signal.aborted) return;
      setFlow(next);
      if (next.id) rememberFlow(next.id);
      else { setAdding(true); setError(next.error || 'Sign-in request not found.'); }
      setRestoreRequest(undefined);
      try { viewState?.setItem(`${storageKey}:request`, ''); } catch { /* the non-secret request reference can be retried */ }
    }).catch(() => { if (!controller.signal.aborted) setError('Could not recover the previous enrollment. Reconnect before adding another account.'); });
    return () => controller.abort();
  }, [api, flowID, restoreRequest]);
  useEffect(() => {
    if (!flowID) return;
    const controller = new AbortController(); let loading = false, done = false;
    const update = async () => {
      if (loading || done) return; loading = true;
      try {
        const next = await api.getSubscriptionEnrollment({ enrollmentId: flowID }, { signal: controller.signal, timeoutMs: 10000 });
        if (controller.signal.aborted) return;
        setFlow(next); attempt.current = undefined;
        if (terminal(next.state)) { done = true; if (next.state === 'added') refresh(); }
      } catch { if (!controller.signal.aborted) setError('Could not read the sign-in request. It may still be running; retry the connection before adding another account.'); }
      finally { loading = false; }
    };
    void update(); const timer = setInterval(() => void update(), 1500);
    return () => { controller.abort(); clearInterval(timer); };
  }, [api, flowID]);
  const add = async (event: FormEvent) => {
    event.preventDefault(); if (inFlight.current || !label.trim()) return;
    inFlight.current = true; autoOpen.current = true; setSubmitting(true); setError('');
    const old = attempt.current;
    const next = old?.provider === provider && old.label === label ? old : { key: crypto.randomUUID(), provider, label };
    attempt.current = next;
    try { viewState?.setItem(`${storageKey}:request`, JSON.stringify({ key: next.key, provider })); } catch { setError('Could not retain the sign-in request reference.'); }
    try {
      const result = await api.addSubscription({ requestKey: next.key, provider, label: label.trim(), subscriptionToken: '' }, { signal: lifetime.current!.signal, timeoutMs: 30000 });
      if (!lifetime.current?.signal.aborted) { setFlow(result); rememberFlow(result.id); attempt.current = undefined; viewState?.setItem(`${storageKey}:request`, ''); }
    } catch { if (!lifetime.current?.signal.aborted) setError('Could not confirm the subscription request. Retry unchanged to check the same request.'); }
    finally { inFlight.current = false; if (!lifetime.current?.signal.aborted) {setSubmitting(false);if(!dialog.current?.open && attempt.current) {setRestoreRequest({key:attempt.current.key,provider:attempt.current.provider});attempt.current=undefined;}} }
  };
  const close = () => { setAdding(false); if (!inFlight.current && attempt.current) {setRestoreRequest({key:attempt.current.key,provider:attempt.current.provider});attempt.current=undefined;} };
  const clear = () => { setRestoreRequest(undefined); try {viewState?.setItem(`${storageKey}:request`, '');} catch {} dismissed.current.add(flowID); rememberFlow(''); setFlow(undefined); setLabel(''); attempt.current = undefined; setError(''); close(); };
  const cancel = async () => {
    if (!flowID) return;
    try { setFlow(await api.cancelSubscriptionEnrollment({ enrollmentId: flowID }, { signal: lifetime.current!.signal, timeoutMs: 10000 })); }
    catch { setError('Could not confirm cancellation. The sign-in request remains visible.'); }
  };
  const copyCode = async () => {
    if (!flow?.userCode) return;
    setCopied(false); setCopyError('');
    try {
      if (copySignInCode) await copySignInCode(flow.userCode);
      else await navigator.clipboard.writeText(flow.userCode);
      setCopied(true);
    } catch { setCopyError('Could not copy. Select the code and copy it manually.'); }
  };
  const open = async () => {
    if (!flow?.signInUrl) return;
    try {
      if (openSignIn) await openSignIn(flow.signInUrl);
      else { const url = new URL(flow.signInUrl); if (url.protocol !== 'https:' || url.username || url.password) throw new Error(); window.open(url.href, '_blank', 'noopener,noreferrer'); }
    } catch { setError('Could not open the provider sign-in page. Use Open sign-in page to try again.'); }
  };
  useEffect(() => {
    if (flow?.state === 'awaiting_login' && flow.signInUrl && autoOpen.current) {
      autoOpen.current = false;
      // Native hosts can open the system browser after the asynchronous RPC.
      // Browser hosts retain the explicit button to avoid popup blockers.
      if (openSignIn) void open();
    }
  }, [flow?.state, flow?.signInUrl]);
  const updateUsage = async (profile: Profile) => {
    if (usageInFlight.current.has(profile.id)) return;
    usageInFlight.current.add(profile.id);
    setRefreshing(new Set(usageInFlight.current)); setRefreshErrors(value => ({ ...value, [profile.id]: '' }));
    try {
      const next = await api.refreshSubscription({ profileId: profile.id }, { signal: lifetime.current!.signal, timeoutMs: 30000 });
      if (!lifetime.current?.signal.aborted) setUpdated(value => ({ ...value, [next.id]: next }));
    } catch { if (!lifetime.current?.signal.aborted) setRefreshErrors(value => ({ ...value, [profile.id]: 'Usage could not be refreshed. Previous observations are retained.' })); }
    finally { usageInFlight.current.delete(profile.id); if (!lifetime.current?.signal.aborted) setRefreshing(new Set(usageInFlight.current)); }
  };
  const profiles = info?.profiles.filter(profile => profile.managed).map(profile => (updated[profile.id]?.availability?.observedAtUnixMillis || 0n) > (profile.availability?.observedAtUnixMillis || 0n) ? updated[profile.id] : profile) || [];
  const pending = !!flowID && !terminal(flow?.state);
  return <section className="settings-page" aria-labelledby="settings-title">
    <header className="settings-title"><div><span className="settings-eyebrow">YOUR WORKSPACE</span><h1 id="settings-title">Settings</h1></div></header>
    <nav className="settings-tabs" role="tablist" aria-label="Settings sections"><button id="subscriptions-tab" role="tab" aria-selected={tab === 'subscriptions'} onClick={() => setTab('subscriptions')} aria-controls="subscriptions-panel">Subscriptions</button><button id="repositories-tab" role="tab" aria-selected={tab === 'repositories'} aria-controls="repositories-panel" onClick={() => setTab('repositories')}>Repositories</button><button role="tab" id="agents-tab" aria-selected={tab==='agents'} aria-controls="agents-panel" onClick={()=>setTab('agents')}>Agents & workspaces</button><button role="tab" id="pricing-tab" aria-selected={tab==='pricing'} aria-controls="pricing-panel" onClick={()=>setTab('pricing')}>Pricing</button><button role="tab" aria-selected={tab==='keyboard'} onClick={()=>setTab('keyboard')}>Shortcuts</button></nav>
    {tab==='keyboard'&&<KeyboardSettings store={viewState}/>}
    {tab==='agents'&&<section role="tabpanel" id="agents-panel" aria-labelledby="agents-tab"><AgentSettings/></section>}
    {tab==='pricing'&&<section role="tabpanel" id="pricing-panel" aria-labelledby="pricing-tab"><PricingSettings/></section>}
    {tab === 'repositories' && <section role="tabpanel" id="repositories-panel" aria-labelledby="repositories-tab"><RepositoryBrowser chooseRepositoryFolder={chooseRepositoryFolder}/></section>}
    <section hidden={tab !== 'subscriptions'} role="tabpanel" id="subscriptions-panel" aria-labelledby="subscriptions-tab">
      <div className="subscriptions-heading"><div><h2>Subscriptions</h2><p>{info?.sharedSubscriptions ? 'Shared with your other Lazybox environments. Saved across restarts.' : 'Saved across restarts, separate from your local CLI sign-ins.'}</p></div><button className="primary" onClick={() => { setError(''); setAdding(true); }}><Icon name="plus"/> {pending ? 'Continue sign-in' : 'Add subscription'}</button></div>
      {error && <p className="settings-error" role="alert">{error}</p>}
      {pending && !adding && <div className="sign-in-banner"><span>{flow?.label || 'Subscription'} · sign-in pending</span><button onClick={() => setAdding(true)}>Continue sign-in</button></div>}
      {!info && <p className="settings-empty">Loading account profiles…</p>}
      {info && !profiles.length && <div className="settings-empty"><h3>No subscriptions yet</h3><p>Add a Claude or Codex account to get started.</p></div>}
      <div className="subscription-grid">{profiles.map(profile => {
        const usage = profile.availability, windows = usage?.windows || [], codex = profile.provider === 'codex';
        return <article className="subscription-card" key={profile.id} aria-label={`${profile.label || (codex ? 'Current Codex account' : 'Current Claude account')} subscription`}>
          <header><span className={`provider-mark ${profile.provider}`}>{codex ? '›' : '✳'}</span><div><h3>{profile.label || (codex ? 'Current Codex account' : 'Current Claude account')}</h3>{usage?.email && <small className="subscription-email">{usage.email}</small>}<span>{codex ? 'Codex' : 'Claude'}{usage?.plan ? ` · ${usage.plan}` : ''}</span></div><span className={`account-state state-${usage?.state || 'unknown'}`}>{(!usage?.state || usage.state === 'unknown') ? 'Saved' : accountLabel(usage.state)}</span></header>
          <div className="subscription-usage">{windows.length ? windows.map(window => <div className="usage-window" key={window.name}><div><span>{windowName(window)}</span><strong>{Number.isFinite(window.usedPercent) ? `${window.usedPercent.toLocaleString(undefined, { maximumFractionDigits: 1 })}% used` : 'Unavailable'}</strong></div><progress max="100" value={Math.max(0, Math.min(100, window.usedPercent))} aria-label={`${windowName(window)} used`} /><small title={window.resetAt > 0n ? new Date(Number(window.resetAt) * 1000).toLocaleString() : undefined}>{resetLabel(window.resetAt) || 'Reset time not reported'}</small></div>) : <div className="usage-unavailable"><span>Usage not reported yet</span><small>Read current subscription usage from your account.</small></div>}</div>
          {refreshErrors[profile.id] && <p className="settings-error" role="alert">{refreshErrors[profile.id]}</p>}
          <footer><small>{usage && usage.observedAtUnixMillis > 0n ? `Last report ${new Date(Number(usage.observedAtUnixMillis)).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })}` : profile.isolation === 'inherited' ? 'Uses current CLI sign-in' : info?.sharedSubscriptions ? 'Shared subscription' : 'Saved on this Runner'}</small><button disabled={refreshing.has(profile.id)} onClick={() => void updateUsage(profile)}><Icon name="refresh"/>{refreshing.has(profile.id) ? 'Refreshing…' : 'Refresh usage'}</button></footer>
        </article>;
      })}</div>
      <p className="usage-note">Usage is the provider’s reported subscription allowance. Missing data stays unavailable.</p>
    </section>
    <DraggableDialog ref={dialog} className="subscription-dialog" onCancel={() => close()} onClose={() => setAdding(false)} aria-labelledby="add-subscription-title">
      <DialogHeader><h2 id="add-subscription-title">{flowID ? 'Add subscription' : 'Connect a subscription'}</h2><button aria-label="Close subscription dialog" onClick={close}>×</button></DialogHeader>
      {error && <p className="settings-error" role="alert">{error}</p>}
      {!flowID ? <form onSubmit={add}>
        <label>Provider<select aria-label="Provider" value={provider} disabled={submitting} onChange={e => setProvider(e.target.value as 'claude' | 'codex')}><option value="codex">Codex</option><option value="claude">Claude</option></select></label>
        <label>Account name<input ref={labelInput} autoComplete="off" maxLength={64} placeholder="e.g. Personal or Work" value={label} disabled={submitting} onChange={e => setLabel(e.target.value)} required/></label>
        <p>Sign in with {provider === 'claude' ? 'Claude' : 'ChatGPT'} in your browser. Your account will be saved for next time.</p>
        <button className="primary" disabled={submitting || !label.trim()}>{submitting ? 'Connecting…' : `Continue with ${provider === 'claude' ? 'Claude' : 'Codex'}`}<Icon name="external"/></button>
        <small>Your existing CLI sign-ins stay separate.</small>
      </form> : <div className="enrollment-state">
        {flow?.state === 'awaiting_login' ? <>
          <h3>Finish signing in with {flow.provider === 'claude' ? 'Claude' : 'ChatGPT'}</h3>
          <p>{flow.userCode ? 'Enter this code on the sign-in page:' : `Complete sign-in in your browser to connect ${flow.label}. This window will update automatically.`}</p>
          {flow.userCode && <div className="device-code-control"><input className="device-code" aria-label="Sign-in code" readOnly value={flow.userCode} onFocus={event => event.currentTarget.select()}/><button onClick={() => void copyCode()}>{copied ? 'Copied' : 'Copy code'}</button><span className="copy-status" role="status">{copied ? 'Code copied to clipboard' : ''}</span>{copyError && <p role="alert">{copyError}</p>}</div>}
          <button className="primary" onClick={() => void open()}>Open sign-in page <Icon name="external"/></button>
          <small role="status">Waiting for sign-in…</small>
        </> : flow?.state === 'added' ? <><span className="enrollment-check">✓</span><h3>Subscription added</h3><p>{flow.label} is available to sessions on this Runner.</p><button className="primary" onClick={clear}>Done</button></> : flow?.state === 'failed' || flow?.state === 'cancelled' || flow?.state === 'expired' ? <><h3>{flow.state === 'cancelled' ? 'Sign-in cancelled' : flow.state === 'expired' ? 'Sign-in request expired' : 'Could not add subscription'}</h3><p>{flow.error}</p><button onClick={clear}>Close</button></> : <p role="status">{flow?.state === 'cancelling' ? 'Cancelling sign-in…' : 'Preparing sign-in…'}</p>}
        {!terminal(flow?.state) && <button className="cancel-sign-in" disabled={flow?.state === 'cancelling'} onClick={() => void cancel()}>Cancel sign-in</button>}
      </div>}
    </DraggableDialog>
  </section>;
}
