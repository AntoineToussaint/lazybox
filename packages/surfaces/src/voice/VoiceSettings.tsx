import { Fragment, useEffect, useState, type ReactNode } from 'react';
import type { RecognizerLanguage, RecognizerStatus, SpeechVoice, ViewStateStore, VoicePermission } from '@lazybox/platform';
import { bindings, canonicalBinding, canonicalKey, chordFromEvent, displayBinding, isLoneModifier, resolveMod, actionDefinition } from '../actions';
import { Icon } from '../Icons';
import { saveKeymap, useKeymap, validateKeymap, type Action } from '../keyboard';
import { SettingsGroup, SettingsHeader, SettingsRow } from '../settings-parts';
import { commandIds, commandWordList, defaultCommandWords, type KeyBehaviour, type ReadReplies, type SpokenCommand, type VoicePreferences } from './voice-model';
import { chosenRecognizer, readAloud, recognizers, setVoiceKeysSuspended, stopReading, synthesizer, useVoicePreferences, useVoiceState } from './voice-session';
import './voice.css';

const permissionText: Record<VoicePermission, string> = { granted: 'Allowed', denied: 'Blocked', restricted: 'Restricted', 'not-determined': 'Not asked yet', unsupported: 'Not available' };
const behaviours: { value: KeyBehaviour; label: string; detail: string }[] = [
  { value: 'both', label: 'Hold to talk, tap to keep listening', detail: 'Hold the key while you speak and release it to finish; a quick tap keeps listening until you press it again (Right ⌥ alone: tap it twice). Recommended.' },
  { value: 'hold', label: 'Hold to talk', detail: 'Listening stops as soon as you release the key.' },
  { value: 'toggle', label: 'Press to start, press again to send', detail: 'Press the key again to finish and send a dictated message. With Right ⌥ alone, tap twice to start and once to send. Say “stop” to keep the draft or press Esc to cancel.' },
];
const commandLabels: Record<SpokenCommand, { label: string; detail: string }> = {
  send: { label: 'Send', detail: 'Sends the message when “Send when I say send” is on; otherwise stops listening so you can press ↵.' },
  newline: { label: 'New line', detail: 'Starts a new line and keeps listening.' },
  cancel: { label: 'Cancel', detail: 'Drops what you just dictated and stops listening.' },
  stop: { label: 'Stop', detail: 'Stops listening and keeps the text.' },
};
const replies: { value: ReadReplies; label: string }[] = [
  { value: 'off', label: 'Off' },
  { value: 'summary', label: 'First paragraph' },
  { value: 'full', label: 'Whole reply' },
];
const samples: Record<string, string> = { fr: 'Voici comment Lazybox lira les réponses de vos agents.', en: 'This is how Lazybox will read your agents’ replies.' };

/**
 * Settings → Voice: how dictation listens (engine, language, permissions,
 * keys), what spoken words do, and read-aloud. The privacy note says
 * "on-device" only when the engine in use confirms it.
 */
export function VoiceSettings({ store }: { store?: ViewStateStore }) {
  const [preferences, save] = useVoicePreferences(store);
  const set = (change: Partial<VoicePreferences>) => save({ ...preferences, ...change });
  const recognizer = chosenRecognizer(preferences);
  const all = recognizers();
  const [status, setStatus] = useState<RecognizerStatus | undefined>();
  const [languages, setLanguages] = useState<RecognizerLanguage[]>([]);
  const [voices, setVoices] = useState<SpeechVoice[]>([]);
  const [problem, setProblem] = useState('');
  const [asking, setAsking] = useState(false);
  const voice = useVoiceState();
  const refresh = () => {
    if (!recognizer) return;
    recognizer.status(preferences.language).then(setStatus, error => setProblem(String(error)));
  };
  useEffect(refresh, [recognizer?.id, preferences.language]);
  useEffect(() => { recognizer?.languages().then(setLanguages, () => setLanguages([])); }, [recognizer?.id]);
  useEffect(() => { synthesizer()?.voices().then(setVoices, () => setVoices([])); }, []);
  // Coming back from System Settings: read the permissions again.
  useEffect(() => { window.addEventListener('focus', refresh); return () => window.removeEventListener('focus', refresh); });

  const system = recognizer?.id === 'system';
  const place = system ? 'this Mac' : 'this device';
  const automatic = status?.language ? `Automatic (${languages.find(l => l.code === status.language)?.name || status.language})` : 'Automatic (your system’s language)';
  const ask = async () => {
    if (!recognizer?.requestPermission) return;
    setAsking(true); setProblem('');
    try { setStatus(await recognizer.requestPermission(preferences.language)); } catch (error) { setProblem(String(error)); } finally { setAsking(false); }
  };
  const blocked = (['microphone', 'speech'] as const).filter(pane => status && (status[pane] === 'denied' || status[pane] === 'restricted'));
  const needsAsking = !!status && (status.microphone === 'not-determined' || (system && status.speech === 'not-determined'));

  return <section className="voice-settings" aria-labelledby="voice-settings-title">
    <SettingsHeader id="voice-settings-title" title="Voice" description={<>Dictate messages, say commands, and have replies read to you. Hold <VoiceKey store={store} id="voice.dictate" plain/> and speak; release to finish.</>}/>
    <SettingsGroup title="Dictation">
      {!recognizer ? <p className="settings-error tone-note" role="note">This browser has no speech recognition. Use Chrome, Edge or Safari, or the Lazybox desktop app — or your keyboard’s own dictation key (on iPhone, the microphone on the keyboard).</p> : <>
        <SettingsRow label="Engine" htmlFor="voice-engine" description={system ? 'The Speech framework of macOS, on this Mac.' : 'The speech recognition your browser provides.'}>
          {all.length > 1
            ? <select id="voice-engine" value={preferences.engine} onChange={e => set({ engine: e.target.value as VoicePreferences['engine'] })}>
              <option value="auto">Automatic ({all[0].label})</option>
              {all.map(r => <option key={r.id} value={r.id}>{r.label}</option>)}
            </select>
            : <span>{recognizer.label}</span>}
        </SettingsRow>
        <SettingsRow label="Permissions" description={`Lazybox asks once; ${system ? 'macOS' : 'your browser'} remembers your answer.`}>
          <span className="voice-status">
            <span className="voice-permission" data-state={status?.microphone || 'unsupported'}><Icon name="mic" size={12}/>Microphone: {status ? permissionText[status.microphone] : '…'}</span>
            {system && <span className="voice-permission" data-state={status?.speech || 'unsupported'}><Icon name="text" size={12}/>Speech recognition: {status ? permissionText[status.speech] : '…'}</span>}
            {needsAsking && recognizer.requestPermission && <button type="button" className="primary" disabled={asking} onClick={() => void ask()}>{asking ? 'Waiting for your answer…' : 'Allow…'}</button>}
            {blocked.map(pane => recognizer.openPrivacySettings
              ? <button key={pane} type="button" onClick={() => void recognizer.openPrivacySettings!(pane).catch(error => setProblem(String(error)))}><Icon name="external" size={12}/>Open {pane === 'microphone' ? 'Microphone' : 'Speech Recognition'} settings</button>
              : <small key={pane}>Allow the microphone in your browser’s site settings, then come back.</small>)}
            <button type="button" className="quiet" aria-label="Check the permissions again" title="Check again" onClick={refresh}><Icon name="refresh" size={12}/></button>
          </span>
        </SettingsRow>
        {blocked.length > 0 && system && <p className="settings-error" role="alert">{blocked.includes('microphone') ? 'The microphone is blocked for Lazybox.' : 'Speech recognition is blocked for Lazybox.'} Turn it on in System Settings → Privacy &amp; Security, then come back here.</p>}
        <SettingsRow label="Language" htmlFor="voice-language" description="What you speak. Spoken commands and punctuation work in English and French.">
          <select id="voice-language" value={preferences.language} onChange={e => set({ language: e.target.value })}>
            <option value="">{automatic}</option>
            {languages.map(l => <option key={l.code} value={l.code}>{l.name}{system && l.onDevice === false ? ' · needs Apple’s server' : ''}</option>)}
          </select>
        </SettingsRow>
        {system && <label className="settings-row">
          <span className="settings-row-text"><span className="settings-row-label">Use Apple’s server when needed</span><small>For a language this Mac cannot recognize itself. Off: such a language does not dictate, and audio never leaves this Mac.</small></span>
          <span className="settings-row-control settings-switch"><input type="checkbox" role="switch" checked={preferences.allowServer} onChange={e => set({ allowServer: e.target.checked })}/></span>
        </label>}
        <PrivacyNote status={status} system={system} place={place} allowServer={preferences.allowServer}/>
        {(problem || status?.reason) && <p className="settings-error" role="alert">{problem || status?.reason}</p>}
      </>}
    </SettingsGroup>

    <SettingsGroup title="Keys" description="Both keys work from anywhere in the window, also while you type.">
      <SettingsRow label="Dictate" description={<>Into the message box you are working in. Right ⌥ alone: hold it to talk, tap it twice to keep listening. If ⌥Space does nothing, another app (ChatGPT, Raycast, Alfred) may use it. Pick another key.</>}><VoiceKey store={store} id="voice.dictate"/></SettingsRow>
      <SettingsRow label="Say a command" description="Opens the command palette and runs what you say: “next agent that needs me”, “open changes”, “switch to Beta”."><VoiceKey store={store} id="voice.command"/></SettingsRow>
      <div role="radiogroup" aria-label="How the key works">
        {behaviours.map(choice => <label key={choice.value} className="settings-row">
          <span className="settings-row-text"><span className="settings-row-label">{choice.label}</span><small>{choice.detail}</small></span>
          <span className="settings-row-control"><input type="radio" name="voice-key-behaviour" value={choice.value} checked={preferences.keyBehaviour === choice.value} onChange={() => set({ keyBehaviour: choice.value })}/></span>
        </label>)}
      </div>
    </SettingsGroup>

    <SettingsGroup title="Text">
      <Switch label="Automatic punctuation" detail={system ? 'macOS adds commas, periods and question marks as you speak (macOS 13 and later).' : 'Not offered by browser speech recognition: use spoken punctuation.'} checked={preferences.autoPunctuation} disabled={!system} onChange={autoPunctuation => set({ autoPunctuation })}/>
      <Switch label="Spoken punctuation" detail="Say “comma”, “period”, “question mark”, “new line” — or “virgule”, “point”, “point d’interrogation”, “à la ligne”." checked={preferences.spokenPunctuation} onChange={spokenPunctuation => set({ spokenPunctuation })}/>
    </SettingsGroup>

    <SettingsGroup title="Spoken commands" description="Said at the end of what you dictate, after a short pause. “Don’t send” stays text.">
      <Switch label="Spoken commands" detail="Turn off to dictate these words as text." checked={preferences.spokenCommands} onChange={spokenCommands => set({ spokenCommands })}/>
      <Switch label="Send when I say “send”" detail="Off: saying it only stops listening, and you press ↵ to send. Nothing is ever sent unless you say it or press ↵." checked={preferences.autoSend} disabled={!preferences.spokenCommands} onChange={autoSend => set({ autoSend })}/>
      {commandIds.map(id => <CommandWords key={id} id={id} words={preferences.words[id]} disabled={!preferences.spokenCommands} onChange={list => set({ words: { ...preferences.words, [id]: list } })}/>)}
    </SettingsGroup>

    <SettingsGroup title="Read aloud" description="Never while you dictate. Esc stops reading.">
      {!synthesizer() ? <p className="settings-error tone-note" role="note">This browser cannot read aloud.</p> : <>
        <SettingsRow label="Agent replies" htmlFor="voice-replies" description="When a turn ends in a conversation you have open.">
          <select id="voice-replies" value={preferences.readReplies} onChange={e => set({ readReplies: e.target.value as ReadReplies })}>
            {replies.map(r => <option key={r.value} value={r.value}>{r.label}</option>)}
          </select>
        </SettingsRow>
        <Switch label="Conversations that need you" detail="Reads the “needs you” notification: a question, an approval, a failure." checked={preferences.readNeedsYou} onChange={readNeedsYou => set({ readNeedsYou })}/>
        <SettingsRow label="Voice" htmlFor="voice-voice">
          <select id="voice-voice" value={preferences.voice} onChange={e => set({ voice: e.target.value })}>
            <option value="">System default</option>
            {voices.map(v => <option key={v.id} value={v.id}>{v.name} · {v.language}</option>)}
          </select>
        </SettingsRow>
        <SettingsRow label="Speed" htmlFor="voice-rate">
          <span className="voice-rate">
            <input id="voice-rate" type="range" min="0.5" max="2" step="0.1" value={preferences.rate} onChange={e => set({ rate: Number(e.target.value) })}/>
            <output htmlFor="voice-rate">{preferences.rate.toFixed(1)}×</output>
            {voice.reading
              ? <button type="button" onClick={() => void stopReading()}><Icon name="stop" size={12}/>Stop</button>
              : <button type="button" onClick={() => { const language = voices.find(v => v.id === preferences.voice)?.language || preferences.language || navigator.language; void readAloud(samples[language.slice(0, 2)] || samples.en); }}><Icon name="play" size={12}/>Play a sample</button>}
          </span>
        </SettingsRow>
      </>}
    </SettingsGroup>
  </section>;
}

function Switch({ label, detail, checked, disabled, onChange }: { label: string; detail: ReactNode; checked: boolean; disabled?: boolean; onChange(value: boolean): void }) {
  return <label className="settings-row">
    <span className="settings-row-text"><span className="settings-row-label">{label}</span><small>{detail}</small></span>
    <span className="settings-row-control settings-switch"><input type="checkbox" role="switch" aria-label={label} checked={checked} disabled={disabled} onChange={e => onChange(e.target.checked)}/></span>
  </label>;
}

/** Where the audio goes, said plainly. "On-device" only when the engine confirmed it. */
function PrivacyNote({ status, system, place, allowServer }: { status?: RecognizerStatus; system: boolean; place: string; allowServer: boolean }) {
  const local = !!status?.onDevice;
  const where = local ? `On-device: audio never leaves ${place}.`
    : system ? (allowServer ? 'This language is recognized by Apple’s server: the audio is sent to Apple while you dictate.' : 'This language is not available on-device on this Mac. Add it in System Settings → Keyboard → Dictation, or allow Apple’s server above.')
    : 'Your browser sends the audio to its speech service while you dictate (Google for Chrome and Edge, Apple for Safari).';
  return <p className={'voice-privacy' + (local ? ' is-local' : '')} role="note" aria-label="Privacy">
    <Icon name={local ? 'lock' : 'cloud'} size={14}/>
    <span><strong>{where}</strong> The microphone is on only while the recording light shows, and no audio is ever saved.</span>
  </p>;
}

/** One command's words, comma-separated; saved when you leave the field. */
function CommandWords({ id, words, disabled, onChange }: { id: SpokenCommand; words: string[]; disabled: boolean; onChange(words: string[]): void }) {
  const [text, setText] = useState(words.join(', '));
  useEffect(() => setText(words.join(', ')), [words.join('|')]);
  const commit = () => { const list = commandWordList(text) || []; onChange(list.length ? list : defaultCommandWords[id]); };
  return <SettingsRow label={commandLabels[id].label} htmlFor={`voice-words-${id}`} description={commandLabels[id].detail}>
    <span className="voice-words"><input id={`voice-words-${id}`} value={text} disabled={disabled} spellCheck={false} onChange={e => setText(e.target.value)} onBlur={commit} onKeyDown={e => { if (e.key === 'Enter') { e.preventDefault(); commit(); } }}/></span>
  </SettingsRow>;
}

const modifierNames: Record<string, string> = { AltLeft: 'Left ⌥', ControlLeft: 'Left ⌃', ControlRight: 'Right ⌃', MetaLeft: 'Left ⌘', ShiftLeft: 'Left ⇧', ShiftRight: 'Right ⇧' };

/**
 * A voice key, and a button to record another: press the new chord, or press
 * and release Right ⌥ or Right ⌘ alone; Esc keeps the old one.
 */
function VoiceKey({ store, id, plain }: { store?: ViewStateStore; id: Action; plain?: boolean }) {
  const keymap = useKeymap(store);
  const [recording, setRecording] = useState(false);
  const [error, setError] = useState('');
  const all = bindings(keymap[id]);
  const keys = (binding: string) => displayBinding(binding).map((key, i) => <kbd key={i}>{key}</kbd>);
  const shown = !all.length ? <em>None</em> : plain ? keys(all[0]) : all.map((binding, i) => <Fragment key={binding}>{i > 0 && <span className="voice-key-or">or</span>}{keys(binding)}</Fragment>);
  useEffect(() => {
    if (!recording) return;
    setVoiceKeysSuspended(true);
    const save = (binding: string) => {
      const next = { ...keymap, [id]: binding };
      const problem = validateKeymap(next);
      if (problem) { setError(problem); return; }
      try { saveKeymap(next, store); setError(''); setRecording(false); } catch (e) { setError(String(e).replace(/^Error:\s*/, '')); }
    };
    // A modifier pressed with nothing else yet: released alone, it is the key.
    let alone: string | null = null;
    const keydown = (event: KeyboardEvent) => {
      if (['Alt', 'Shift', 'Control', 'Meta'].includes(event.key)) {
        alone = [event.altKey, event.ctrlKey, event.metaKey, event.shiftKey].filter(Boolean).length === 1 ? event.code : null;
        return;
      }
      alone = null;
      event.preventDefault(); event.stopImmediatePropagation();
      if (event.key === 'Escape') { setRecording(false); return; }
      save(canonicalKey(chordFromEvent(event)));
    };
    const keyup = (event: KeyboardEvent) => {
      if (!alone || event.code !== alone) return;
      alone = null;
      event.preventDefault(); event.stopImmediatePropagation();
      if (isLoneModifier(event.code)) save(event.code);
      else setError(`${modifierNames[event.code] || event.key} alone is pressed with other keys all the time. Alone, only Right ⌥ or Right ⌘ can be the key.`);
    };
    window.addEventListener('keydown', keydown, true);
    window.addEventListener('keyup', keyup, true);
    return () => { window.removeEventListener('keydown', keydown, true); window.removeEventListener('keyup', keyup, true); setVoiceKeysSuspended(false); };
  }, [recording, keymap, id, store]);
  if (plain) return <span className="voice-key">{shown}</span>;
  const fallback = resolveMod(actionDefinition(id)?.keys || '');
  const isDefault = all.map(canonicalBinding).join(' or ') === bindings(fallback).map(canonicalBinding).join(' or ');
  return <span className="voice-key">
    {recording ? <span role="status">Press the new key, or Right ⌥ alone…</span> : shown}
    <button type="button" className={recording ? 'is-recording' : undefined} onClick={() => { setError(''); setRecording(value => !value); }}>{recording ? 'Cancel' : 'Change…'}</button>
    {!isDefault && !recording && <button type="button" className="quiet" onClick={() => { try { saveKeymap({ ...keymap, [id]: fallback }, store); setError(''); } catch (e) { setError(String(e).replace(/^Error:\s*/, '')); } }}>Reset</button>}
    {error && <small className="settings-error" role="alert">{error}</small>}
  </span>;
}
