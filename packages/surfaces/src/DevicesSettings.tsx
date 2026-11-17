import { useCallback, useEffect, useRef, useState } from 'react';
import { ConnectError } from '@lazybox/client-core';
import type { Device, DevicePairing, RemoteAccess } from '@lazybox/protocol';
import { useRunner } from './RunnerContext';
import { DraggableDialog, DialogHeader } from './DraggableDialog';
import { QrCode } from './QrCode';
import { Icon } from './Icons';
import { ageLabel } from './attention';
import './devices.css';

/** What each grant lets a phone do, in the words the person decides with. */
export const grantLabels: Record<string, string> = { read: 'See conversations', respond: 'Reply, answer and approve', start: 'Start conversations' };
const grantHints: Record<string, string> = {
  read: 'What needs you, every conversation and its output, workspace changes.',
  respond: 'Send messages, answer questions, approve or decline requests, interrupt.',
  start: 'Start or continue a conversation in an existing workspace. This runs an agent on this computer.',
};
const never = 'Never: settings, subscriptions, GitHub, terminals, deleting anything.';
/** The grants a new pairing offers. `start` is not offered until the phone has a screen to start a
 * conversation from; a phone already granted it keeps it, and its label above. */
const offered = ['read', 'respond'];
const message = (error: unknown) => ConnectError.from(error).rawMessage || String(error);

/**
 * Settings → Devices: how a phone reaches this Runner (Tailscale), pairing a
 * phone by QR code, and the paired phones with last seen and Revoke.
 */
export function DevicesSettings() {
  const { api } = useRunner();
  const [access, setAccess] = useState<RemoteAccess>();
  const [devices, setDevices] = useState<Device[]>();
  const [error, setError] = useState('');
  const [switching, setSwitching] = useState(false);
  const [pairing, setPairing] = useState(false);
  const [confirm, setConfirm] = useState('');
  const [now, setNow] = useState(Date.now);
  const load = useCallback(async (signal?: AbortSignal) => {
    try {
      const [nextAccess, list] = await Promise.all([api.getRemoteAccess({}, { signal, timeoutMs: 15000 }), api.listDevices({}, { signal, timeoutMs: 10000 })]);
      if (signal?.aborted) return;
      setAccess(nextAccess); setDevices(list.devices); setNow(Date.now());
    } catch (e) { if (!signal?.aborted) setError(message(e)); }
  }, [api]);
  useEffect(() => {
    const control = new AbortController();
    void load(control.signal);
    // Last seen moves while the person watches; a phone paired elsewhere appears.
    const timer = setInterval(() => void load(control.signal), 10000);
    return () => { control.abort(); clearInterval(timer); };
  }, [load]);
  const setTailscale = async (on: boolean) => {
    setSwitching(true); setError('');
    try { setAccess(await api.setRemoteAccess({ tailscale: on }, { timeoutMs: 30000 })); }
    catch (e) { setError(message(e)); void load(); }
    finally { setSwitching(false); }
  };
  const revoke = async (device: Device) => {
    setError('');
    try { setDevices((await api.revokeDevice({ deviceId: device.id }, { timeoutMs: 10000 })).devices); setConfirm(''); }
    catch (e) { setError(message(e)); }
  };
  const ts = access?.tailscale;
  const reachable = !!access?.phoneUrl;
  return <section className="devices-settings" aria-labelledby="devices-heading">
    <div className="devices-heading">
      <div><h2 id="devices-heading">Devices</h2><p>Follow and answer your agents from your phone. A paired phone never sees your settings, accounts or GitHub, and you can revoke it at any time.</p></div>
      <button className="primary" disabled={!reachable} title={reachable ? undefined : 'Turn on access from your phone first'} onClick={() => { setError(''); setPairing(true); }}><Icon name="phone"/> Pair a phone</button>
    </div>
    {error && <p className="settings-error" role="alert">{error}</p>}
    <article className="remote-route" aria-labelledby="remote-route-title" data-state={ts?.state || 'loading'}>
      <div className="remote-route-icon"><Icon name="shield" size={20}/></div>
      <div className="remote-route-body">
        <h3 id="remote-route-title">Access from your phone</h3>
        {!access ? <p role="status">Checking how your phone can reach this computer…</p> : <RouteStatus access={access}/>}
        {access?.problem && <p className="remote-problem" role="alert">{access.problem}</p>}
      </div>
      {ts && (ts.enabled && ts.state === 'serving' ? <button disabled={switching} onClick={() => void setTailscale(false)}>{switching ? 'Turning off…' : 'Turn off'}</button>
        : ts.enabled ? <button disabled={switching} onClick={() => void setTailscale(true)}>{switching ? 'Turning on…' : 'Try again'}</button>
        : ts.state === 'available' || ts.state === 'serving' ? <button className="primary" disabled={switching} onClick={() => void setTailscale(true)}>{switching ? 'Turning on…' : 'Turn on'}</button> : null)}
    </article>
    <h3 className="devices-list-title">Paired phones</h3>
    {!devices ? <p className="settings-empty" role="status">Loading paired phones…</p> : !devices.length ? <div className="devices-empty"><Icon name="phone" size={22}/><div><strong>No phones paired yet</strong><p>{reachable ? 'Choose Pair a phone and scan the code with your phone’s camera.' : 'Turn on access from your phone above, then pair it with a QR code.'}</p></div></div>
      : <ul className="device-list" aria-label="Paired phones">{devices.map(device => <li key={device.id} className="device-row" aria-label={device.name}>
        <span className="device-icon"><Icon name="phone" size={18}/></span>
        <div className="device-main">
          <strong>{device.name}</strong>
          <span className="device-meta"><span>Paired {new Date(Number(device.pairedAtUnixMillis)).toLocaleDateString(undefined, { month: 'short', day: 'numeric', year: 'numeric' })}</span><span className="device-seen">{lastSeen(device, now)}</span></span>
          <span className="device-grants">{device.grants.map(grant => <span key={grant} className="grant-chip">{grantLabels[grant] || grant}</span>)}</span>
          <DevicePush device={device}/>
          <small className="mono device-fingerprint" title="Key fingerprint. The phone shows the same one under its name.">{device.fingerprint}</small>
        </div>
        {confirm === device.id ? <div className="device-confirm" role="group" aria-label={`Revoke ${device.name}?`}>
          <span>Sign it out now?</span>
          <button onClick={() => setConfirm('')}>Cancel</button>
          <button className="danger" onClick={() => void revoke(device)}>Revoke</button>
        </div> : <button onClick={() => setConfirm(device.id)} aria-label={`Revoke ${device.name}`}>Revoke</button>}
      </li>)}</ul>}
    {pairing && <PairPhoneDialog phoneUrl={access?.phoneUrl || ''} onClose={() => { setPairing(false); void load(); }}/>}
  </section>;
}

function lastSeen(device: Device, now: number) {
  const at = Number(device.lastSeenAtUnixMillis);
  if (!at) return 'Not seen yet';
  return now - at < 60_000 ? 'Active now' : `Last seen ${ageLabel(at, now)} ago`;
}

function RouteStatus({ access }: { access: RemoteAccess }) {
  const ts = access.tailscale;
  if (!ts) return <p>Remote access is not available on this Runner.</p>;
  if (ts.state === 'serving' && ts.enabled) return <p className="remote-on"><span className="status-dot on" aria-hidden="true"/>On over Tailscale at <span className="mono">{access.phoneUrl}</span>. Only devices signed in to your tailnet can connect, encrypted end to end; it is never on the public internet.</p>;
  if (ts.state === 'available' || (ts.state === 'serving' && !ts.enabled)) return <p>Tailscale is connected as <span className="mono">{ts.dnsName}</span>. Turn this on to reach Lazybox from your phone over your tailnet: encrypted end to end, never on the public internet. Your phone needs Tailscale too.</p>;
  const help = ts.state === 'not_installed' || ts.state === 'not_running' || ts.state === 'needs_login';
  return <p>{ts.detail || 'Tailscale is unavailable.'}{help && <> <a href="https://tailscale.com/download" target="_blank" rel="noreferrer noopener">Get Tailscale <Icon name="external" size={12}/></a></>}</p>;
}

type Phase = { kind: 'loading' } | { kind: 'offer'; offer: DevicePairing } | { kind: 'ended'; offer: DevicePairing } | { kind: 'error'; message: string };

/**
 * The pairing code. It appears at once with the usual grants; changing what
 * the phone may do issues a new code. The code works once, for five minutes;
 * closing the dialog withdraws it.
 */
function PairPhoneDialog({ phoneUrl, onClose }: { phoneUrl: string; onClose(): void }) {
  const { api } = useRunner();
  const dialog = useRef<HTMLDialogElement>(null);
  const [grants, setGrants] = useState<string[]>(['read', 'respond']);
  const [editing, setEditing] = useState(false);
  const [phase, setPhase] = useState<Phase>({ kind: 'loading' });
  const [now, setNow] = useState(Date.now);
  const [copied, setCopied] = useState(false);
  const [round, setRound] = useState(0);
  const current = useRef('');
  useEffect(() => { dialog.current?.showModal(); }, []);
  // A new code for each choice of grants; the previous one is withdrawn.
  useEffect(() => {
    let cancelled = false;
    setPhase({ kind: 'loading' }); setCopied(false);
    void api.beginDevicePairing({ grants }, { timeoutMs: 15000 }).then(offer => {
      if (cancelled) { void api.cancelDevicePairing({ pairingId: offer.id }).catch(() => {}); return; }
      current.current = offer.id; setPhase({ kind: 'offer', offer });
    }, error => { if (!cancelled) setPhase({ kind: 'error', message: message(error) }); });
    return () => {
      cancelled = true;
      const id = current.current; current.current = '';
      if (id) void api.cancelDevicePairing({ pairingId: id }).catch(() => {});
    };
  }, [api, grants.join(','), round]);
  // Follow the code until the phone uses it or it ends.
  const offerId = phase.kind === 'offer' ? phase.offer.id : '';
  useEffect(() => {
    if (!offerId) return;
    const timer = setInterval(() => {
      setNow(Date.now());
      void api.getDevicePairing({ pairingId: offerId }, { timeoutMs: 5000 }).then(status => {
        if (status.state === 'pending' || current.current !== offerId) return;
        current.current = '';
        setPhase(previous => previous.kind === 'offer' ? { kind: 'ended', offer: { ...previous.offer, ...status, url: '' } as DevicePairing } : previous);
      }, () => {});
    }, 1000);
    return () => clearInterval(timer);
  }, [api, offerId]);
  const close = () => { dialog.current?.close(); };
  const toggle = (grant: string, on: boolean) => setGrants(value => offered.filter(g => g === 'read' || (g === grant ? on : value.includes(g))));
  const host = (() => { try { return new URL(phoneUrl).host; } catch { return phoneUrl; } })();
  const remaining = phase.kind === 'offer' ? Math.max(0, Number(phase.offer.expiresAtUnixMillis) - now) : 0;
  const ended = phase.kind === 'ended' ? phase.offer : undefined;
  return <DraggableDialog ref={dialog} className="pair-dialog" aria-labelledby="pair-title" onClose={onClose}>
    <DialogHeader><h2 id="pair-title">{ended?.state === 'paired' ? 'Phone paired' : 'Pair a phone'}</h2><button aria-label="Close pairing" onClick={close}>×</button></DialogHeader>
    {ended?.state === 'paired' && ended.device ? <div className="pair-done">
      <span className="pair-check" aria-hidden="true"><Icon name="check" size={28}/></span>
      <h3>{ended.device.name} is paired</h3>
      <p>Check that your phone shows the same key: <span className="mono">{ended.device.fingerprint}</span></p>
      <p className="pair-grants">It can: {ended.device.grants.map(grant => grantLabels[grant] || grant).join(' · ')}.</p>
      <button className="primary" data-autofocus onClick={close}>Done</button>
    </div> : ended ? <div className="pair-done">
      <h3>{ended.state === 'expired' ? 'This code expired' : ended.state === 'failed' ? 'This code was blocked' : 'This code was cancelled'}</h3>
      <p>{ended.state === 'failed' ? 'Too many wrong attempts used it. Nothing was paired.' : 'Nothing was paired.'}</p>
      <button className="primary" data-autofocus onClick={() => setRound(value => value + 1)}>Show a new code</button>
    </div> : phase.kind === 'error' ? <div className="pair-done"><p className="settings-error" role="alert">{phase.message}</p><button onClick={close}>Close</button></div>
    : <div className="pair-body">
      <div className="pair-code">
        {phase.kind === 'offer' ? <QrCode value={phase.offer.url} label="Pairing code: scan it with your phone's camera"/> : <div className="qr-placeholder" role="status">Making a code…</div>}
      </div>
      <div className="pair-steps">
        <ol>
          <li>Open your phone’s camera and point it at the code.</li>
          <li>Tap the link. It opens <span className="mono">{host}</span>.</li>
          <li>Tap <strong>Pair this phone</strong>.</li>
        </ol>
        <p className="pair-expiry" role="timer" aria-live="off">{phase.kind === 'offer' ? `Works once · expires in ${Math.floor(remaining / 60000)}:${String(Math.floor(remaining / 1000) % 60).padStart(2, '0')}` : ' '}</p>
        <div className="pair-allowed">
          <span>This phone will be able to</span>
          {editing ? <fieldset aria-label="What this phone may do">
            {offered.map(grant => <label key={grant} className="grant-option"><input type="checkbox" checked={grants.includes(grant)} disabled={grant === 'read'} onChange={event => toggle(grant, event.target.checked)}/><span><strong>{grantLabels[grant]}</strong><small>{grantHints[grant]}</small></span></label>)}
            <small>{never}</small>
          </fieldset> : <p>{grants.map(grant => grantLabels[grant]).join(' · ')} <button className="link-button" onClick={() => setEditing(true)}>Change</button></p>}
        </div>
        {phase.kind === 'offer' && <button className="pair-copy" onClick={() => void navigator.clipboard.writeText(phase.offer.url).then(() => setCopied(true), () => {})}><Icon name="copy" size={14}/>{copied ? 'Link copied' : 'Copy link instead'}</button>}
        <small className="pair-waiting" role="status">{phase.kind === 'offer' ? 'Waiting for your phone…' : ''}</small>
      </div>
    </div>}
  </DraggableDialog>;
}

const pushKinds = [['needsInput', 'Needs input'], ['failed', 'Failed'], ['review', 'Ready for review']] as const;

/**
 * A phone's notifications, as the phone set them (REMOTE.md): on or off,
 * which kinds, quiet hours, and a test the owner can send from here. The
 * phone chooses; revoking it ends them.
 */
function DevicePush({ device }: { device: Device }) {
  const { api } = useRunner();
  const [status, setStatus] = useState<{ ok: boolean; text: string }>();
  const [sending, setSending] = useState(false);
  const push = device.push;
  if (!push?.subscribed) return <span className="device-push" data-on="false"><Icon name="bell" size={12}/>Notifications off · the phone turns them on in its settings</span>;
  const p = push.preferences;
  const kinds = pushKinds.filter(([key]) => p?.[key]).map(([, label]) => label);
  const test = async () => {
    setSending(true); setStatus(undefined);
    try { await api.sendTestPush({ deviceId: device.id }, { timeoutMs: 20000 }); setStatus({ ok: true, text: 'Sent' }); }
    catch (e) { setStatus({ ok: false, text: message(e) }); }
    finally { setSending(false); }
  };
  return <span className="device-push" data-on="true">
    <Icon name="bell" size={12}/>
    <span>Notifications on{kinds.length ? ` · ${kinds.join(', ')}` : ' · every kind off'}{p?.quietStart && p.quietEnd ? ` · quiet ${p.quietStart}–${p.quietEnd}` : ''}</span>
    <button type="button" className="device-push-test" disabled={sending} onClick={() => void test()}>{sending ? 'Sending…' : 'Send test'}</button>
    {status && <small role="status" className={status.ok ? 'device-push-ok' : 'device-push-error'}>{status.text}</small>}
    {!status && push.lastError && <small className="device-push-error" role="note">Last delivery failed: {push.lastError}</small>}
  </span>;
}
