import { useEffect, useRef, useState } from 'react';
import type { NotificationPermission } from '@lazybox/platform';
import type { NotificationPreferences } from './attention';
import { useAttentionHost, useNotificationPreferences } from './useAttention';
import './inbox.css';
import { Icon } from './Icons';
import { SettingsGroup, SettingsHeader } from './settings-parts';
import { playAlert, storeSoundFile, SYSTEM_SOUNDS, systemSoundsAvailable, useSoundSettings, type AlertSound, type SoundSettings, type SoundSource } from './sounds';
import { ConfigFileStatus } from './ConfigFileStatus';
import { notificationRefusal, onNotificationRefusal, reportRefusal } from './attention-alerts';
import { writeConfig } from './config-writer';

export const notificationOptions: { key: keyof NotificationPreferences; label: string; detail: string }[] = [
  { key: 'needsInput', label: 'Needs input', detail: 'An agent asks a question or waits for an approval.' },
  { key: 'failed', label: 'Failed', detail: 'A provider failure, a message that could not be delivered, or a turn a Runner restart interrupted.' },
  { key: 'review', label: 'Ready for review', detail: 'An agent finished its turn.' },
  { key: 'badge', label: 'Dock badge', detail: 'Show on the app icon the Inbox count: what needs you, pull requests and tasks included.' },
];
const sounds: { key: 'soundNeedsYou' | 'soundDone'; sound: AlertSound; label: string; detail: string }[] = [
  { key: 'soundNeedsYou', sound: 'request', label: 'Needs you', detail: 'An agent asks a question, waits for an approval, or fails.' },
  { key: 'soundDone', sound: 'done', label: 'Done', detail: 'An agent finished its turn.' },
];

/** Settings → Notifications. Preferences are shared by every window of this Runner. */
export function NotificationSettings() {
  const host = useAttentionHost();
  const [preferences, keep] = useNotificationPreferences();
  // Every change is also written to settings.yaml (POWER.md, "Config files").
  const save = (next: NotificationPreferences) => { keep(next); void writeConfig({ file: 'settings', notifications: next }); };
  const [permission, setPermission] = useState<NotificationPermission>();
  const [status, setStatus] = useState('');
  // Read again when the window comes back: the person may have just changed it in the system's settings.
  useEffect(() => {
    const read = () => void host?.notificationPermission?.().then(setPermission).catch(() => setPermission('unsupported'));
    read();
    window.addEventListener('focus', read);
    return () => window.removeEventListener('focus', read);
  }, [host]);
  const [refusal, setRefusal] = useState(notificationRefusal);
  useEffect(() => onNotificationRefusal(() => setRefusal(notificationRefusal())), []);
  const supported = !!host?.notifyAttention;
  const test = async () => {
    try {
      await host!.notifyAttention!({ key: 'lazybox-test', title: 'Lazybox notifications are on', body: 'You will be told when a conversation needs you.', collectionId: '', sessionId: '' });
      reportRefusal('');
      setStatus(host?.systemSettings ? 'Sent: macOS accepted it.' : 'Sent. If nothing appeared, check this app in your system notification settings.');
    } catch (error) { reportRefusal(error); setStatus(`Could not send: ${String(error).replace(/^Error: /, '')}`); }
  };
  return <section className="notification-settings" aria-labelledby="notification-settings-title">
    <SettingsHeader id="notification-settings-title" title="Notifications" description="Lazybox tells you when a conversation you are not looking at needs you: a small toast while Lazybox is in front, a system notification when it is not. Every conversation, notified or not, is in the Inbox."/>
    <ConfigFileStatus name="settings" what="These settings" section="notifications"/>
    {!supported && <p className="settings-error tone-note" role="note">This window does not send notifications; the main Lazybox window does.</p>}
    {permission === 'denied' && <p className="settings-error" role="alert">{host?.systemSettings ? `Lazybox is not allowed to notify you. Allow it in ${host.systemSettings}, then come back.` : 'Notifications are blocked for this site. Allow them in your browser’s site settings, then come back.'}</p>}
    {permission !== 'denied' && refusal && <p className="settings-error" role="alert">The last notification was not shown: {refusal}{host?.systemSettings ? ` Check ${host.systemSettings}.` : ''}</p>}
    {permission === 'default' && host?.requestNotificationPermission && <p className="settings-error tone-note">{host.systemSettings ? 'macOS asks once before Lazybox can notify you.' : 'Your browser asks before showing notifications.'} <button onClick={() => void host.requestNotificationPermission!().then(setPermission)}>Allow notifications</button></p>}
    <SettingsGroup title="Notify me when">
      {notificationOptions.map(option => <label key={option.key} className="settings-row notification-option">
        <span className="settings-row-text"><span className="settings-row-label">{option.label}</span><small>{option.detail}</small></span>
        <span className="settings-row-control settings-switch"><input type="checkbox" role="switch" checked={preferences[option.key]} onChange={event => save({ ...preferences, [option.key]: event.target.checked })}/></span>
      </label>)}
    </SettingsGroup>
    <SoundSettingsGroup preferences={preferences} save={save}/>
    <div className="notification-test">
      <button disabled={!supported || permission === 'denied'} onClick={() => void test()}><Icon name="bell" size={14}/>Send a test notification</button>
      {status && <small role="status">{status}</small>}
    </div>
  </section>;
}

const sourceValue = (source: SoundSource) => source.kind === 'system' ? `system:${source.name}` : source.kind === 'file' ? 'file' : 'builtin';

/**
 * Settings → Notifications → Sounds: for each kind, on or off, which sound
 * (Lazybox's chime, a macOS system sound in the desktop app, or a file of
 * your own), with a preview; one volume; and whether to stay quiet while
 * Lazybox is in front. Changes apply at once and survive a reload.
 */
function SoundSettingsGroup({ preferences, save }: { preferences: NotificationPreferences; save(next: NotificationPreferences): void }) {
  const [settings, saveSounds] = useSoundSettings();
  const [errors, setErrors] = useState<Partial<Record<AlertSound, string>>>({});
  const files = useRef<Partial<Record<AlertSound, HTMLInputElement | null>>>({});
  const system = systemSoundsAvailable();
  const preview = (kind: AlertSound, next: SoundSettings = settings) => playAlert(kind, { force: true, settings: next });
  const choose = (kind: AlertSound, value: string) => {
    if (value === 'choose-file') { files.current[kind]?.click(); return; }
    const source: SoundSource = value.startsWith('system:') ? { kind: 'system', name: value.slice(7) } : value === 'file' ? settings.sources[kind] : { kind: 'builtin' };
    const next = { ...settings, sources: { ...settings.sources, [kind]: source } };
    saveSounds(next); setErrors(e => ({ ...e, [kind]: '' })); preview(kind, next);
  };
  const pickFile = async (kind: AlertSound, file: File | undefined) => {
    if (!file) return;
    const problem = await storeSoundFile(kind, file);
    setErrors(e => ({ ...e, [kind]: problem }));
    if (problem) return;
    const next = { ...settings, sources: { ...settings.sources, [kind]: { kind: 'file' as const, name: file.name } } };
    saveSounds(next); preview(kind, next);
  };
  return <SettingsGroup title="Sounds" description="A short sound from the main window when a conversation needs you or is done. Choose each one, and how loud.">
    {sounds.map(option => {
      const source = settings.sources[option.sound];
      const id = `sound-${option.key}`;
      return <div key={option.key} className="settings-row notification-option sound-option">
        <label className="settings-row-text" htmlFor={id}><span className="settings-row-label">{option.label}</span><small>{option.detail}</small></label>
        <span className="settings-row-control notification-sound-control">
          <select aria-label={`${option.label} sound`} value={sourceValue(source)} onChange={event => choose(option.sound, event.target.value)}>
            <option value="builtin">Lazybox chime</option>
            {system && <optgroup label="macOS">{SYSTEM_SOUNDS.map(name => <option key={name} value={`system:${name}`}>{name}</option>)}</optgroup>}
            {(source.kind === 'system' && !system) && <option value={sourceValue(source)}>{source.name} (desktop app)</option>}
            <optgroup label="Your own">
              {source.kind === 'file' && <option value="file">{source.name}</option>}
              <option value="choose-file">Choose a file…</option>
            </optgroup>
          </select>
          <input ref={node => { files.current[option.sound] = node; }} type="file" accept="audio/*,.aif,.aiff,.wav,.mp3,.m4a,.caf" hidden aria-label={`${option.label} sound file`} onChange={event => { void pickFile(option.sound, event.target.files?.[0]); event.target.value = ''; }}/>
          <button type="button" className="icon-button" aria-label={`Play the ${option.label.toLowerCase()} sound`} title="Play" onClick={() => preview(option.sound)}><Icon name="play" size={14}/></button>
          <span className="settings-switch"><input id={id} type="checkbox" role="switch" aria-label={`${option.label} sound on`} checked={preferences[option.key]} onChange={event => save({ ...preferences, [option.key]: event.target.checked })}/></span>
        </span>
        {errors[option.sound] && <p className="settings-error sound-error" role="alert">{errors[option.sound]}</p>}
      </div>;
    })}
    <div className="settings-row notification-option">
      <label className="settings-row-text" htmlFor="sound-volume"><span className="settings-row-label">Volume</span><small>For every Lazybox sound.</small></label>
      <span className="settings-row-control sound-volume">
        <Icon name="speaker" size={14}/>
        <input id="sound-volume" type="range" min={0} max={100} step={5} value={Math.round(settings.volume * 100)} aria-valuetext={`${Math.round(settings.volume * 100)}%`}
          onChange={event => saveSounds({ ...settings, volume: Number(event.target.value) / 100 })}
          onPointerUp={() => preview(preferences.soundNeedsYou || !preferences.soundDone ? 'request' : 'done')}
          onKeyUp={event => { if (event.key.startsWith('Arrow') || event.key === 'Home' || event.key === 'End') preview(preferences.soundNeedsYou || !preferences.soundDone ? 'request' : 'done'); }}/>
        <output htmlFor="sound-volume">{Math.round(settings.volume * 100)}%</output>
      </span>
    </div>
    <label className="settings-row notification-option">
      <span className="settings-row-text"><span className="settings-row-label">Only when Lazybox is in the background</span><small>Stay quiet while you are looking at Lazybox; the toast is enough.</small></span>
      <span className="settings-row-control settings-switch"><input type="checkbox" role="switch" checked={settings.backgroundOnly} onChange={event => saveSounds({ ...settings, backgroundOnly: event.target.checked })}/></span>
    </label>
  </SettingsGroup>;
}
