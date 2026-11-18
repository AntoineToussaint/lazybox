import { useEffect, useState, useSyncExternalStore } from 'react';
import type { RecognitionEvent, RecognitionSession, SpeechRecognizerHost, SpeechSynthesizerHost, ViewStateStore, VoiceHost } from '@lazybox/platform';
import { PREFERENCE_EVENT, readPreference, writePreference } from '../view-state';
import type { AttentionNotice } from '../attention';
import { showToast } from '../toasts';
import { applyResult, dictationView, emptyDictation, parseVoicePreferences, settleAll, settleDictation, speakableText, type Dictation, type DictationOptions, type DictationView, type VoicePreferences } from './voice-model';
import { webRecognizer, webSynthesizer } from './web-speech';

/**
 * Voice, the live part (VOICE.md): which engines this window has, the
 * dictation in progress (one per window; the desktop host also keeps one
 * microphone for the whole app), where its text goes, and read-aloud.
 *
 * The microphone is open only between a start and its end, and the pill
 * (VoicePill) is on screen for exactly that long.
 */

// ---- Preferences -----------------------------------------------------------

const preferenceName = 'voice:v1';
export function readVoicePreferences(store?: ViewStateStore): VoicePreferences {
  try { return parseVoicePreferences(JSON.parse(readPreference(store, preferenceName) || '{}')); } catch { return parseVoicePreferences({}); }
}
export function saveVoicePreferences(preferences: VoicePreferences, store?: ViewStateStore) {
  writePreference(store, preferenceName, JSON.stringify(preferences));
}
/** The preferences, current across windows (Settings in another window saves them). */
export function useVoicePreferences(store?: ViewStateStore): [VoicePreferences, (next: VoicePreferences) => void] {
  const [preferences, setPreferences] = useState(() => readVoicePreferences(store));
  useEffect(() => {
    const reload = () => setPreferences(readVoicePreferences(store));
    reload();
    window.addEventListener(PREFERENCE_EVENT, reload);
    window.addEventListener('storage', reload);
    return () => { window.removeEventListener(PREFERENCE_EVENT, reload); window.removeEventListener('storage', reload); };
  }, [store]);
  return [preferences, next => { saveVoicePreferences(next, store); setPreferences(next); }];
}

// ---- Engines ---------------------------------------------------------------

let host: VoiceHost = {};
/** The desktop gives its native engines; without them the browser's are used. */
export function configureVoice(next: VoiceHost) { host = next; }
/** Every recognizer this window can use: the system's first. */
export function recognizers(): SpeechRecognizerHost[] {
  return [host.recognizer, webRecognizer()].filter((r): r is SpeechRecognizerHost => !!r);
}
export function chosenRecognizer(preferences: VoicePreferences): SpeechRecognizerHost | undefined {
  const all = recognizers();
  return (preferences.engine === 'auto' ? undefined : all.find(r => r.id === preferences.engine)) || all[0];
}
export function synthesizer(): SpeechSynthesizerHost | undefined { return host.synthesizer || webSynthesizer(); }

// ---- State -------------------------------------------------------------------

export type DictationKind = 'composer' | 'palette';
export type EndReason = 'stop' | 'send' | 'cancel' | 'error' | 'takeover';
/** Where dictated text goes while a dictation lasts. */
export type DictationSink = {
  update(view: DictationView): void;
  /** Over: keep the text ('stop'), keep it and send ('send'), put back what was there ('cancel', 'error'), or leave the text as the person typed it ('takeover'). */
  end(reason: EndReason, view: DictationView): void;
};
/** Something that takes dictation: a message box, the command palette. */
export type DictationTarget = {
  kind: DictationKind;
  element(): HTMLElement | null;
  label?(): string;
  available(): boolean;
  begin(): DictationSink | null;
};
export type VoiceState = {
  phase: 'idle' | 'starting' | 'listening' | 'stopping' | 'error';
  kind?: DictationKind;
  destination?: string;
  mode: 'hold' | 'toggle';
  startedAt: number;
  /** 0–1 from the microphone; undefined for an engine that reports none. */
  level?: number;
  /** Speech heard right now (engines without a level). */
  hearing: boolean;
  text: string;
  onDevice: boolean;
  engine: string;
  error: string;
  errorCode: string;
  /** Reading a reply or a notification aloud. */
  reading: boolean;
};
const idle: VoiceState = { phase: 'idle', mode: 'hold', startedAt: 0, hearing: false, text: '', onDevice: false, engine: '', error: '', errorCode: '', reading: false };
let state: VoiceState = idle;
const listeners = new Set<() => void>();
const set = (change: Partial<VoiceState>) => { state = { ...state, ...change }; for (const listener of listeners) listener(); };
export function voiceState(): VoiceState { return state; }
export function useVoiceState(): VoiceState {
  return useSyncExternalStore(listener => { listeners.add(listener); return () => { listeners.delete(listener); }; }, () => state, () => state);
}
export const dictating = () => state.phase === 'starting' || state.phase === 'listening' || state.phase === 'stopping';

/** The window's preferences, set by its voice layer; the defaults until then. */
let preferences: VoicePreferences = parseVoicePreferences({});
export function setVoicePreferencesContext(next: VoicePreferences) { preferences = next; }
export function currentVoicePreferences(): VoicePreferences { return preferences; }

// ---- Targets -----------------------------------------------------------------

const targets: { target: DictationTarget; focusedAt: number }[] = [];
/**
 * While mounted, a target can take dictation; the one focused last is chosen.
 * Unmounting it (its tab closed) ends a dictation into it, keeping what was
 * said: the microphone never stays on for a message box that is gone.
 */
export function registerDictationTarget(target: DictationTarget): () => void {
  const entry = { target, focusedAt: 0 };
  targets.push(entry);
  const focus = () => { entry.focusedAt = Date.now(); };
  const element = target.element();
  element?.addEventListener('focusin', focus);
  if (element && element.contains(document.activeElement)) focus();
  return () => {
    element?.removeEventListener('focusin', focus); const i = targets.indexOf(entry); if (i >= 0) targets.splice(i, 1);
    if (run && run.target === target && !run.over) end(run, 'stop');
  };
}
/** The palette when it is open; else the message box with focus, the one focused last, or the only one. */
export function resolveTarget(kind?: DictationKind): DictationTarget | undefined {
  const usable = targets.filter(t => t.target.available() && t.target.element()?.isConnected && (!kind || t.target.kind === kind));
  const palette = usable.find(t => t.target.kind === 'palette');
  if (palette) return palette.target;
  const active = document.activeElement;
  const focused = usable.find(t => active && t.target.element()?.contains(active));
  if (focused) return focused.target;
  return [...usable].sort((a, b) => b.focusedAt - a.focusedAt)[0]?.target;
}
export const hasDictationTarget = (kind?: DictationKind) => !!resolveTarget(kind);

// ---- A dictation -------------------------------------------------------------

type Run = {
  id: number;
  /** What it types into: its unmounting ends the dictation. */
  target: DictationTarget;
  sink: DictationSink;
  session?: RecognitionSession;
  dictation: Dictation;
  options: DictationOptions;
  settleTimer?: ReturnType<typeof setTimeout>;
  stopTimer?: ReturnType<typeof setTimeout>;
  over: boolean;
  /** Send once the last words are in (⌘↵ or ↵ while listening). */
  sendWhenDone?: boolean;
};
let run: Run | undefined;
let runs = 0;
/** The dictation key is down (a hold released before the engine started still stops it). */
let keyHeld = false;
export function setDictationKeyHeld(held: boolean) { keyHeld = held; }
/**
 * The dictation key has started a dictation here before. Until it has, the
 * pill says that another app may have taken it: a key swallowed by another
 * app's global shortcut never reaches the window, so nothing can tell.
 */
const keyUsedName = 'lazybox:voice:key-used';
export function voiceKeyUsed(): boolean { try { return localStorage.getItem(keyUsedName) === '1'; } catch { return true; } }
export function markVoiceKeyUsed() { try { localStorage.setItem(keyUsedName, '1'); } catch { /* a convenience */ } }
/** Settings is recording a new voice key: the current one must not start dictation. */
let keysSuspended = false;
export function setVoiceKeysSuspended(suspended: boolean) { keysSuspended = suspended; }
export const voiceKeysSuspended = () => keysSuspended;

/** How long words must stay unchanged before a command at their end applies. */
export const SETTLE_MS = 900;
/** How long a stop waits for the recognizer's final words. */
export const STOP_GRACE_MS = 1500;

const optionsFor = (p: VoicePreferences, language: string): DictationOptions => ({ language, spokenPunctuation: p.spokenPunctuation, spokenCommands: p.spokenCommands, words: p.words });

/**
 * Starts dictating into `target` (or the target that takes it now). `hold`:
 * until the key is released; `toggle`: until it is pressed again, Stop, or a
 * spoken stop. Reading aloud stops: voice never talks over you.
 */
export async function startDictation({ mode, target, kind }: { mode: 'hold' | 'toggle'; target?: DictationTarget; kind?: DictationKind }): Promise<boolean> {
  if (dictating()) return false;
  const chosen = target || resolveTarget(kind);
  if (!chosen) {
    showToast({ id: 'voice:no-target', tone: 'info', title: kind === 'palette' ? 'Open the command palette to say a command' : 'Open a conversation to dictate', body: 'Dictation types into the message box you are working in.' });
    return false;
  }
  const p = preferences;
  const recognizer = chosenRecognizer(p);
  if (!recognizer) {
    fail('unsupported', 'This browser has no speech recognition. Use Chrome, Edge or Safari, or your keyboard’s dictation.', kind || chosen.kind);
    return false;
  }
  void stopReading();
  const sink = chosen.begin();
  if (!sink) return false;
  const id = ++runs;
  const current: Run = { id, target: chosen, sink, dictation: emptyDictation(), options: optionsFor(p, p.language), over: false };
  run = current;
  set({ ...idle, phase: 'starting', kind: chosen.kind, destination: chosen.label?.(), mode, startedAt: Date.now(), engine: recognizer.label, reading: false });
  try {
    let status = await recognizer.status(p.language).catch(() => undefined);
    if (run !== current) return false;
    // The first dictation asks for what it needs (the system's own prompts) instead of failing.
    if (status && recognizer.requestPermission && (status.microphone === 'not-determined' || status.speech === 'not-determined')) {
      status = await recognizer.requestPermission(p.language);
      const ready = status.microphone === 'granted' && (status.speech === 'granted' || recognizer.id !== 'system');
      if (run !== current) {
        // The prompt took the keyboard (a held key came up): ready for the next press.
        if (ready) showToast({ id: 'voice:ready', tone: 'info', title: 'Voice is ready', body: 'Hold the dictation key and speak.' });
        return false;
      }
      if (!ready) throw new Error(`${status.microphone !== 'granted' ? 'microphone-permission' : 'speech-permission'}: Lazybox needs the microphone and speech recognition to dictate. Allow them in Settings → Voice.`);
    }
    if (status && !current.options.language && status.language) current.options = optionsFor(p, status.language);
    const session = await recognizer.start({ language: p.language, punctuation: p.autoPunctuation, allowServer: p.allowServer }, event => receive(current, event));
    current.session = session;
    if (run !== current || current.over) { void session.cancel().catch(() => {}); return false; }
    set({ phase: 'listening', onDevice: !!status?.onDevice, startedAt: Date.now() });
    // The key came up while the microphone was starting.
    if (state.mode === 'hold' && !keyHeld) void stopDictation();
    return true;
  } catch (error) {
    if (run !== current) return false;
    // The desktop host names the problem first: "microphone-permission: Allow…".
    const text = error instanceof Error ? error.message : String(error);
    const coded = text.match(/^([a-z][a-z-]*): ([\s\S]+)$/);
    end(current, 'error');
    fail(coded?.[1] || 'start', coded?.[2] || text, chosen.kind);
    return false;
  }
}

let errorTimer: ReturnType<typeof setTimeout> | undefined;
/** macOS refuses recognition while Dictation is off, in its own words: say what to do instead. */
export const DICTATION_OFF = 'Voice uses macOS Dictation, which is turned off. Turn it on in System Settings → Keyboard → Dictation (macOS downloads its on-device model the first time), then try again.';
export function voiceProblem(code: string, message: string): { code: string; message: string } {
  if (code === 'dictation-off' || /siri and dictation are disabled|dictation is (disabled|turned off)/i.test(message)) return { code: 'dictation-off', message: DICTATION_OFF };
  return { code, message };
}
export const canOpenVoiceSystemSettings = () => !!host.recognizer?.openPrivacySettings;
/** Opens the system settings page that fixes a voice problem (desktop only). */
export function openVoiceSystemSettings(pane: 'microphone' | 'speech' | 'keyboard'): Promise<void> {
  const open = host.recognizer?.openPrivacySettings;
  return open ? open(pane) : Promise.reject(new Error('System settings open only in the desktop app.'));
}
function fail(rawCode: string, rawMessage: string, kind?: DictationKind) {
  const { code, message } = voiceProblem(rawCode, rawMessage);
  clearTimeout(errorTimer);
  set({ ...idle, phase: 'error', kind, error: message, errorCode: code, reading: state.reading });
  // A problem the person must fix in System Settings stays until they act on it or dismiss it.
  if (code !== 'dictation-off') errorTimer = setTimeout(() => { if (state.phase === 'error') set({ phase: 'idle', error: '', errorCode: '' }); }, 8000);
}
export function dismissVoiceError() { clearTimeout(errorTimer); if (state.phase === 'error') set({ phase: 'idle', error: '', errorCode: '' }); }

function show(current: Run) {
  const view = dictationView(current.dictation, current.options);
  current.sink.update(view);
  set({ text: view.text });
}

function receive(current: Run, event: RecognitionEvent) {
  if (run !== current || current.over) return;
  switch (event.kind) {
    case 'level': set({ level: Math.max(0, Math.min(1, event.level)) }); return;
    case 'activity': set({ hearing: event.speaking }); return;
    case 'error':
      if (event.code === 'no-speech') { showToast({ id: 'voice:no-speech', tone: 'info', title: 'Nothing was heard', body: 'Check the microphone, then hold the key and speak.' }); return; }
      { const kind = state.kind; end(current, 'error'); fail(event.code, event.message, kind); }
      return;
    case 'end': {
      // The recognizer is done: whatever it last reported counts.
      const step = settleAll(current.dictation, current.options);
      current.dictation = step.dictation;
      end(current, step.command === 'cancel' ? 'cancel' : step.command === 'send' && preferences.autoSend ? 'send' : 'stop', step.command === 'send' && !preferences.autoSend);
      return;
    }
    case 'result': {
      const step = applyResult(current.dictation, event, current.options);
      current.dictation = step.dictation;
      show(current);
      clearTimeout(current.settleTimer);
      if (step.command) { act(current, step.command); return; }
      if (!event.final && current.options.spokenCommands) current.settleTimer = setTimeout(() => settle(current), SETTLE_MS);
      return;
    }
  }
}

function settle(current: Run) {
  if (run !== current || current.over) return;
  const step = settleDictation(current.dictation, current.options);
  if (step.dictation === current.dictation) return;
  current.dictation = step.dictation;
  show(current);
  if (step.command) act(current, step.command);
}

function act(current: Run, command: 'send' | 'stop' | 'cancel') {
  if (command === 'cancel') { end(current, 'cancel'); return; }
  if (command === 'send' && preferences.autoSend) { end(current, 'send'); return; }
  end(current, 'stop', command === 'send');
}

/** Ends a dictation; the engine is cancelled if it still runs (its text is already taken). */
function end(current: Run, reason: EndReason, saidSendWithoutAutoSend = false) {
  if (current.over) return;
  current.over = true;
  if (current.sendWhenDone && reason === 'stop') { reason = 'send'; saidSendWithoutAutoSend = false; }
  clearTimeout(current.settleTimer); clearTimeout(current.stopTimer);
  const view = dictationView(reason === 'cancel' ? current.dictation : settleAll(current.dictation, current.options).dictation, current.options);
  try { current.sink.end(reason, view); } catch (error) { console.error('Dictation target failed', error); }
  void current.session?.cancel().catch(() => {});
  if (run === current) { run = undefined; set({ ...idle, reading: state.reading }); flushAttentionSpeech(); }
  if (saidSendWithoutAutoSend) showToast({ id: 'voice:send-off', tone: 'info', title: 'Press ↵ to send', body: 'Sending by voice is off. Turn on “Send when I say send” in Settings → Voice.' });
}

/** Stop listening and keep the words: the recognizer has a moment to settle them. */
export async function stopDictation(): Promise<void> {
  const current = run;
  if (!current || current.over || state.phase === 'stopping') return;
  if (!current.session) { end(current, 'stop'); return; }
  set({ phase: 'stopping' });
  current.stopTimer = setTimeout(() => end(current, 'stop'), STOP_GRACE_MS);
  try { await current.session.stop(); } catch { end(current, 'stop'); }
}
/**
 * Send what was said (⌘↵ or ↵ while listening): the recognizer settles its
 * last words, then the message goes through the message box's own send,
 * as if ↵ had been pressed after the dictation.
 */
export function finishAndSend(): boolean {
  const current = run;
  if (!current || current.over || state.kind !== 'composer') return false;
  current.sendWhenDone = true;
  void stopDictation();
  return true;
}
/** Stop listening and put back what was there before. */
export function cancelDictation() { if (run) end(run, 'cancel'); }
/** The person typed or sent while dictating: their text stands, the microphone closes. */
export function takeOverDictation() { if (run) end(run, 'takeover'); }
/** Hold → toggle: a tap of the key keeps listening until the next press. */
export function setDictationMode(mode: 'hold' | 'toggle') { if (dictating()) set({ mode }); }
export function toggleDictation(options: { target?: DictationTarget; kind?: DictationKind } = {}) {
  if (dictating()) void stopDictation(); else void startDictation({ mode: 'toggle', ...options });
}

// ---- Read aloud ----------------------------------------------------------------

let reading = 0;
/** Reads text aloud with the person's voice and pace; never while dictating. */
export async function readAloud(text: string): Promise<boolean> {
  const synth = synthesizer();
  if (!synth || !text.trim() || dictating()) return false;
  const token = ++reading;
  set({ reading: true });
  try { await synth.speak(text, { voice: preferences.voice, rate: preferences.rate }); }
  catch (error) { showToast({ id: 'voice:read', tone: 'info', title: 'Could not read aloud', body: error instanceof Error ? error.message : String(error) }); }
  finally { if (token === reading) set({ reading: false }); }
  return true;
}
export async function stopReading(): Promise<void> {
  if (!state.reading) return;
  reading++;
  set({ reading: false });
  await synthesizer()?.stop().catch(() => {});
}

/**
 * Once per thing said across windows: two windows showing the same
 * conversation read its reply once. A convenience, so a full or private
 * storage just reads it here.
 */
export function claimReading(key: string): boolean {
  try {
    const name = `lazybox:voice-read:${key}`;
    if (localStorage.getItem(name)) return false;
    localStorage.setItem(name, String(Date.now()));
    return true;
  } catch { return true; }
}

/** A finished agent reply, read as the person asked (whole, or its first paragraph). */
export function readReply(key: string, markdown: string): boolean {
  if (preferences.readReplies === 'off' || dictating()) return false;
  const text = speakableText(markdown, preferences.readReplies);
  if (!text || !claimReading(key)) return false;
  void readAloud(text);
  return true;
}
type Announcement = { id: string; text: string; pending(): boolean; open(): void };
let queued: Announcement[] = [];
let lastAnnouncement: Announcement | undefined;

/** Queue decisions and blockers. The source checks whether each still needs action. */
export function announceAttention(notices: readonly AttentionNotice[], pending: (notice: AttentionNotice) => boolean, open: (notice: AttentionNotice) => void) {
  if (!preferences.readNeedsYou) return;
  queued = queued.filter(item => item.pending());
  for (const notice of notices) {
    if (!notice.speech || queued.some(item => item.id === notice.speech?.id)) continue;
    queued.push({ id: notice.speech.id, text: notice.speech.text, pending: () => pending(notice), open: () => open(notice) });
  }
  flushAttentionSpeech();
}

function flushAttentionSpeech() {
  if (dictating()) return;
  const batch = queued;
  queued = [];
  if (!preferences.readNeedsYou || !synthesizer()) return;
  const fresh = batch.filter(item => item.pending() && claimReading(item.id));
  const first = fresh.at(-1);
  if (!first) return;
  const line = speakableText(first.text, 'full');
  const text = line.length > 300 ? line.slice(0, 300).replace(/\s+\S*$/, '') + '…' : line;
  lastAnnouncement = { ...first, text: fresh.length > 1 ? `${fresh.length} items need you. ${text}` : text };
  void readAloud(lastAnnouncement.text);
}

export function canRepeatAnnouncement() { return !!lastAnnouncement && !dictating(); }
export function repeatAnnouncement() { if (lastAnnouncement) void readAloud(lastAnnouncement.text); }
export function canOpenAnnouncement() { return !!lastAnnouncement?.pending(); }
export function openAnnouncement() { if (canOpenAnnouncement()) lastAnnouncement?.open(); }
