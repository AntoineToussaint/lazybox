import { useCallback, useEffect, useState } from 'react';
import { ConnectError, type DeviceSession } from '@lazybox/client-core';
import type { PushSettings } from '@lazybox/protocol';
import { useRunner } from '../RunnerContext';
import { Icon } from '../Icons';
import { grantLabels } from '../DevicesSettings';
import { notificationOptions } from '../NotificationSettings';
import { currentEnvironment, disablePush, enablePush, localTimeZone, preferencesOf, pushSupport, PushError, quietNow, wirePreferences, type PhonePushPreferences, type PushSupport } from './push';
import { ConfirmSheet } from './PhoneSheets';
import type { PinStore } from './pins';

const message = (error: unknown) => error instanceof PushError ? error.message : ConnectError.from(error).rawMessage || String(error);
const permissionNow = (): NotificationPermission | 'unsupported' => typeof Notification === 'undefined' ? 'unsupported' : Notification.permission;
/** The three kinds the computer's Settings → Notifications offers, with its words. */
const kinds = notificationOptions.filter(option => option.key === 'needsInput' || option.key === 'failed' || option.key === 'review') as { key: 'needsInput' | 'failed' | 'review'; label: string; detail: string }[];

/** This phone's notifications: whether it can have them, whether it does, and changing that. */
export function usePush() {
  const { api } = useRunner();
  const [settings, setSettings] = useState<PushSettings>();
  const [support] = useState<PushSupport>(() => pushSupport(currentEnvironment()));
  const [permission, setPermission] = useState(permissionNow);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [errorKind, setErrorKind] = useState<'denied' | 'unsupported' | 'failed' | ''>('');
  useEffect(() => {
    const control = new AbortController();
    void api.getPushSettings({}, { signal: control.signal, timeoutMs: 15000 }).then(next => { if (!control.signal.aborted) setSettings(next); }, failure => { if (!control.signal.aborted) setError(message(failure)); });
    return () => control.abort();
  }, [api]);
  const run = useCallback(async (work: () => Promise<PushSettings>) => {
    setBusy(true); setError(''); setErrorKind('');
    try { setSettings(await work()); return true; }
    catch (failure) { setError(message(failure)); setErrorKind(failure instanceof PushError ? failure.reason : 'failed'); return false; }
    finally { setBusy(false); setPermission(permissionNow()); }
  }, []);
  const preferences = preferencesOf(settings);
  return {
    settings, support, permission, busy, error, errorKind, preferences,
    enable: () => run(() => enablePush(api, preferences)),
    disable: () => run(() => disablePush(api)),
    save: (next: PhonePushPreferences) => run(() => api.setPushSettings({ preferences: wirePreferences(next) }, { timeoutMs: 15000 })),
    test: async () => {
      setBusy(true); setError('');
      try { const result = await api.sendTestPush({}, { timeoutMs: 20000 }); if (result.push) setSettings(current => ({ ...current!, ...result.push!, applicationServerKey: current?.applicationServerKey || '' })); return true; }
      catch (failure) { setError(message(failure)); return false; }
      finally { setBusy(false); }
    },
  };
}

/** The steps to put Lazybox on an iPhone's Home Screen, which iOS needs before it allows notifications. */
function InstallSteps() {
  return <ol className="remote-install">
    <li>Tap <strong>Share</strong> <span className="remote-share-glyph" aria-hidden="true"><svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round"><path d="M12 3v12"/><path d="M8 7l4-4 4 4"/><path d="M5 12v7a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2v-7"/></svg></span> in Safari.</li>
    <li>Choose <strong>Add to Home Screen</strong>, then <strong>Add</strong>.</li>
    <li>Open Lazybox from the Home Screen and turn notifications on there.</li>
  </ol>;
}

/**
 * A quiet card at the top of the Inbox until notifications are on (or the
 * person says not now): one tap turns them on, or on an iPhone, how to put
 * Lazybox on the Home Screen first.
 */
export function PushNudge({ onOpenSettings }: { onOpenSettings(): void }) {
  const push = usePush();
  const key = 'lazybox-remote:push-nudge-dismissed';
  const [dismissed, setDismissed] = useState(() => { try { return localStorage.getItem(key) === '1'; } catch { return false; } });
  const [done, setDone] = useState(false);
  const dismiss = () => { setDismissed(true); try { localStorage.setItem(key, '1'); } catch { /* shown again next visit */ } };
  // The confirmation says its piece, then leaves the Inbox to the conversations.
  useEffect(() => { if (!done) return; const timer = setTimeout(() => setDone(false), 6000); return () => clearTimeout(timer); }, [done]);
  if (done) return <p className="remote-nudge remote-nudge-done" role="status"><Icon name="check" size={15}/>Notifications are on. You will hear when an agent needs you.</p>;
  if (dismissed || !push.settings || push.settings.subscribed || push.permission === 'denied') return null;
  if (push.support.kind !== 'supported' && push.support.kind !== 'install') return null;
  return <aside className="remote-nudge" aria-label="Notifications">
    <span className="remote-nudge-icon"><Icon name="bell" size={18}/></span>
    <div>
      <strong>Know when an agent needs you</strong>
      <p>{push.support.kind === 'install' ? 'Add Lazybox to your Home Screen to get notifications on this iPhone.' : 'A notification when an agent asks a question, waits for approval, or fails — even with this page closed.'}</p>
      {push.error && <p className="remote-error" role="alert">{push.error}</p>}
      <div className="remote-nudge-buttons">
        {push.support.kind === 'install' ? <button type="button" className="primary" onClick={onOpenSettings}>Show me how</button>
          : <button type="button" className="primary" disabled={push.busy} onClick={() => void push.enable().then(ok => ok && setDone(true))}>{push.busy ? 'Turning on…' : 'Turn on notifications'}</button>}
        <button type="button" onClick={dismiss}>Not now</button>
      </div>
    </div>
  </aside>;
}

/** The phone's settings: notifications (with the computer's kinds), quiet hours, a test, and this phone. */
export function PhoneSettings({ device, pins, onBack, onForget }: { device: DeviceSession['device']; pins: PinStore; onBack(): void; onForget(): void }) {
  const { api } = useRunner();
  const push = usePush();
  const [status, setStatus] = useState('');
  const [forgetting, setForgetting] = useState(false);
  const p = push.preferences;
  const on = !!push.settings?.subscribed && push.permission === 'granted';
  const quiet = !!(p.quietStart && p.quietEnd);
  const save = (next: PhonePushPreferences) => { setStatus(''); void push.save(next); };
  return <main className="remote-settings" aria-labelledby="remote-settings-title">
    <header className="remote-bar">
      <button className="remote-icon-button" aria-label="Back to inbox" onClick={onBack}><Icon name="back" size={20}/></button>
      <div className="remote-bar-title"><h1 id="remote-settings-title">Settings</h1><span>{device.name}</span></div>
    </header>
    <section className="remote-section" aria-labelledby="remote-notifications">
      <h2 id="remote-notifications">Notifications</h2>
      <div className="remote-card">
        <div className="remote-card-head">
          <span className="remote-nudge-icon" data-on={on || undefined}><Icon name="bell" size={18}/></span>
          <div>
            <strong>{on ? 'On for this phone' : 'Off'}</strong>
            <p>{on ? `Your computer tells this phone when an agent needs you${push.settings?.service ? `, through ${serviceName(push.settings.service)}` : ''}. Only the conversation’s title and what happened are sent — never code or messages.`
              : 'Get a notification when an agent asks a question, waits for approval, or fails, even with Lazybox closed.'}</p>
          </div>
        </div>
        {!push.settings && !push.error && <p className="remote-hint" role="status">Checking…</p>}
        {push.support.kind === 'install' && !on && <><p className="remote-hint">iPhone notifications need Lazybox on the Home Screen (iOS 16.4 or later).</p><InstallSteps/></>}
        {push.support.kind === 'old-ios' && <p className="remote-hint">This iPhone runs iOS {push.support.version}. Notifications from web apps need iOS 16.4 or later; update iOS in Settings → General → Software Update.</p>}
        {push.support.kind === 'unsupported' && <p className="remote-hint">This browser cannot receive notifications. On Android, open this page in Chrome; on an iPhone, in Safari, then add it to the Home Screen.</p>}
        {push.permission === 'denied' && <p className="remote-error" role="alert">Notifications are blocked for Lazybox. {/iPhone|iPad/.test(navigator.userAgent) ? 'On the iPhone: Settings → Notifications → Lazybox → Allow Notifications.' : 'In Chrome: ⋮ → Settings → Site settings → Notifications, then allow this site.'} Then come back here.</p>}
        {push.error && push.errorKind !== 'denied' && <p className="remote-error" role="alert">{push.error}</p>}
        {push.settings?.lastError && on && <p className="remote-error" role="note">The last notification could not be delivered: {push.settings.lastError}</p>}
        {push.support.kind === 'supported' && push.permission !== 'denied' && push.settings && (on
          ? <div className="remote-card-buttons"><button type="button" disabled={push.busy} onClick={() => { setStatus(''); void push.test().then(ok => ok && setStatus('Sent. It should arrive within a few seconds.')); }}><Icon name="bell" size={15}/>Send a test</button><button type="button" disabled={push.busy} onClick={() => void push.disable()}>Turn off</button></div>
          : <button type="button" className="primary remote-wide" disabled={push.busy} onClick={() => void push.enable()}>{push.busy ? 'Turning on…' : 'Turn on notifications'}</button>)}
        {status && <p className="remote-ok" role="status"><Icon name="check" size={14}/>{status}</p>}
      </div>
      {push.settings && <>
        <h3>Notify me when</h3>
        <div className="remote-card remote-switches">
          {kinds.map(kind => <label key={kind.key} className="remote-switch">
            <span><strong>{kind.label}</strong><small>{kind.detail}</small></span>
            <input type="checkbox" role="switch" checked={p[kind.key]} disabled={push.busy} onChange={event => save({ ...p, [kind.key]: event.target.checked })}/>
          </label>)}
        </div>
        <p className="remote-footnote">The same kinds as Settings → Notifications on your computer; each device chooses its own.</p>
        <h3>Quiet hours</h3>
        <div className="remote-card remote-switches">
          <label className="remote-switch">
            <span><strong>Pause at night</strong><small>{quiet ? `Nothing is sent from ${p.quietStart} to ${p.quietEnd}${quietNow(p, new Date()) ? ' — quiet now' : ''}. The Inbox still has everything.` : 'Pause notifications at night. The Inbox still has everything.'}</small></span>
            <input type="checkbox" role="switch" checked={quiet} disabled={push.busy} onChange={event => save(event.target.checked ? { ...p, quietStart: '22:00', quietEnd: '07:00', timeZone: localTimeZone() } : { ...p, quietStart: '', quietEnd: '' })}/>
          </label>
          {quiet && <div className="remote-times">
            <label>From<input type="time" value={p.quietStart} disabled={push.busy} onChange={event => event.target.value && save({ ...p, quietStart: event.target.value, timeZone: localTimeZone() })}/></label>
            <label>To<input type="time" value={p.quietEnd} disabled={push.busy} onChange={event => event.target.value && save({ ...p, quietEnd: event.target.value, timeZone: localTimeZone() })}/></label>
          </div>}
        </div>
        {quiet && p.timeZone && <p className="remote-footnote">In this phone’s time zone, {p.timeZone.replaceAll('_', ' ')}.</p>}
      </>}
    </section>
    <section className="remote-section" aria-labelledby="remote-this-phone">
      <h2 id="remote-this-phone">This phone</h2>
      <div className="remote-card remote-device">
        <strong>{device.name}</strong>
        <span className="mono" title="Key fingerprint. Settings → Devices on your computer shows the same one.">{device.fingerprint}</span>
        <span>{device.grants.map(grant => grantLabels[grant] || grant).join(' · ')}</span>
        <small>{pins.shared ? (pins.writable ? 'Pins are shared with your computer.' : 'Your computer’s pins show here; this phone cannot change them.') : 'Pins are kept on this phone.'} Read marks are this phone’s own.</small>
      </div>
      <button type="button" className="remote-wide remote-danger" onClick={() => setForgetting(true)}>Forget pairing on this phone</button>
      <p className="remote-footnote">Also revoke it on your computer (Settings → Devices): that stops its access and its notifications.</p>
    </section>
    {forgetting && <ConfirmSheet title="Forget this pairing?" detail="This phone deletes its key and stops showing your agents. To use it again, pair it from your computer." confirm="Forget" onClose={() => setForgetting(false)} onConfirm={() => void disablePush(api).catch(() => undefined).finally(onForget)}/>}
  </main>;
}

function serviceName(host: string): string {
  if (host.endsWith('push.apple.com')) return 'Apple';
  if (host.endsWith('googleapis.com')) return 'Google';
  if (host.endsWith('mozilla.com')) return 'Mozilla';
  if (host.endsWith('windows.com')) return 'Microsoft';
  return host;
}
