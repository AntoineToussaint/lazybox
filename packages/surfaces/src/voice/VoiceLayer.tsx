import { useRunner } from '../RunnerContext';
import { wisprRecognizer } from './wispr-recognizer';
import { useEffect, useLayoutEffect, useMemo, useRef, useState, type CSSProperties, type MouseEvent as ReactMouseEvent, type PointerEvent as ReactPointerEvent } from 'react';
import type { ViewStateStore } from '@lazybox/platform';
import { bindings, canonicalBinding, canonicalKey, chordFromEvent, displayBinding, isChord, isLoneModifier, runAction, useActions } from '../actions';
import { beginDrag, preventMouseDown } from '../drag-gesture';
import { Icon } from '../Icons';
import type { Keymap } from '../keyboard';
import { elapsed } from './voice-model';
import { canOpenAnnouncement, canRepeatAnnouncement, canOpenVoiceSystemSettings, cancelDictation, dictating, dismissVoiceError, markVoiceKeyUsed, openAnnouncement, openVoiceSystemSettings, repeatAnnouncement, setDictationKeyHeld, setDictationMode, setVoicePreferencesContext, startDictation, stopDictation, stopReading, toggleDictation, useVoicePreferences, useVoiceState, voiceKeysSuspended, voiceKeyUsed, voiceState, type DictationKind } from './voice-session';
import './voice.css';

/** How long a press of the dictation key may last and still be a tap (it then keeps listening). */
export const TAP_MS = 350;
/** How long Right ⌥ (or Right ⌘) must be held alone before it talks: a quicker ⌥e or ⌥→ never starts anything. */
export const LONE_HOLD_MS = 180;
/** Two taps of Right ⌥ this close together keep listening. */
export const DOUBLE_TAP_MS = 400;
/** A key pressed this soon after a lone hold started, before any word: a slow ⌥ chord, so the dictation is dropped. */
export const LONE_GRACE_MS = 1000;

/**
 * One per window: the voice actions, the push-to-talk keys, Esc, and the
 * pill. The keys are held, so this layer owns their keydown and keyup; the
 * chord and sequence dispatchers skip them (`hold` in actions.ts).
 */
export function VoiceLayer({ viewState, keymap, onOpenSettings }: { viewState?: ViewStateStore; keymap: Keymap; onOpenSettings?(): void }) {
  const { api } = useRunner();
  const [preferences] = useVoicePreferences(viewState);
  useEffect(() => { setVoicePreferencesContext(preferences); }, [preferences]);
  const state = useVoiceState();
  const latest = useRef({ preferences, keymap, api }); latest.current = { preferences, keymap, api };

  /** Say a command: the palette opens (if it is not), then listens. */
  const listenForCommand = (mode: 'hold' | 'toggle') => {
    if (!document.querySelector('dialog.command-palette[open]')) runAction('palette.open');
    // The palette registers itself as a target once it has rendered.
    let frames = 0;
    const attempt = () => {
      if (dictating()) return;
      if (document.querySelector('dialog.command-palette[open]') || frames > 20) { void startDictation({ mode, kind: 'palette', wispr: wisprRecognizer(latest.current.api) }); return; }
      frames++; requestAnimationFrame(attempt);
    };
    requestAnimationFrame(attempt);
  };

  useActions({
    'voice.dictate': () => toggleDictation(),
    'voice.command': () => { if (dictating()) void stopDictation(); else listenForCommand('toggle'); },
    'voice.repeat': { run: repeatAnnouncement, enabled: canRepeatAnnouncement },
    'voice.open-announcement': { run: openAnnouncement, enabled: canOpenAnnouncement },
    'voice.stop-reading': { run: () => void stopReading(), enabled: () => voiceState().reading },
  });

  // The push-to-talk keys and Esc, before anything else sees them.
  useEffect(() => {
    const keysOf = (id: 'voice.dictate' | 'voice.command') => bindings(latest.current.keymap[id]).map(canonicalBinding);
    let pressed: { kind: DictationKind; at: number; code: string; modifiers: string[]; stops: boolean } | null = null;
    const behaviour = () => latest.current.preferences.keyBehaviour;
    const begin = (kind: DictationKind, mode: 'hold' | 'toggle') => {
      markVoiceKeyUsed();
      // In the palette, either key talks to the palette.
      if (kind === 'palette' || document.querySelector('dialog.command-palette[open]')) listenForCommand(mode);
      else void startDictation({ mode });
    };

    // Right ⌥ (or Right ⌘) alone: held for LONE_HOLD_MS with no other key, it
    // talks; tapped twice, it keeps listening; tapped while listening, it
    // finishes the run. Any other key while it is down (⌥e, ⌥→, ⌥-click) makes it that
    // key's modifier: nothing starts, and the key goes through untouched.
    type Lone = { kind: DictationKind; code: string; at: number; timer?: ReturnType<typeof setTimeout>; started: number; toggled: boolean; stops: boolean };
    let lone: Lone | null = null;
    let lastTap: { code: string; at: number } | null = null;
    const dropLone = () => { if (lone) clearTimeout(lone.timer); lone = null; };
    const loneKind = (event: KeyboardEvent): DictationKind | null => {
      const alone = event.code === 'AltRight' ? event.key === 'Alt' && !event.ctrlKey && !event.metaKey && !event.shiftKey
        : event.code === 'MetaRight' ? event.key === 'Meta' && !event.ctrlKey && !event.altKey && !event.shiftKey : false;
      if (!alone) return null;
      return keysOf('voice.dictate').includes(event.code) ? 'composer' : keysOf('voice.command').includes(event.code) ? 'palette' : null;
    };
    /** Another key (or a click) while the lone modifier is down. */
    const interrupt = () => {
      if (!lone) return;
      if (!lone.started) { dropLone(); lastTap = null; return; }
      // Started a moment ago and nothing said yet: that was a slow ⌥ chord, not speech.
      if (!lone.toggled && Date.now() - lone.started < LONE_GRACE_MS && !voiceState().text) { dropLone(); setDictationKeyHeld(false); cancelDictation(); }
    };

    const keydown = (event: KeyboardEvent) => {
      if (event.isComposing) return;
      if (lone && event.code !== lone.code) interrupt();
      // Two taps are two in a row: any other key between them makes the next one a first tap.
      if (!lone && lastTap && event.code !== lastTap.code) lastTap = null;
      const lonely = !voiceKeysSuspended() && loneKind(event);
      if (lonely) {
        if (event.repeat || lone || pressed || otherDialogOpen()) return;
        const now = Date.now();
        const current: Lone = { kind: lonely, code: event.code, at: now, started: 0, toggled: false, stops: false };
        lone = current;
        // Pressed while listening: released alone, it finishes the run.
        if (dictating()) { current.stops = true; return; }
        if (behaviour() !== 'hold' && lastTap?.code === event.code && now - lastTap.at < DOUBLE_TAP_MS) {
          lastTap = null; current.started = now; current.toggled = true;
          begin(lonely, 'toggle');
          return;
        }
        current.timer = setTimeout(() => {
          if (lone !== current) return;
          current.started = Date.now();
          setDictationKeyHeld(true);
          begin(current.kind, behaviour() === 'toggle' ? 'toggle' : 'hold');
        }, LONE_HOLD_MS);
        return;
      }
      if (event.key === 'Escape') {
        const phase = voiceState().phase;
        if (dictating()) { event.preventDefault(); event.stopImmediatePropagation(); cancelDictation(); return; }
        if (phase === 'error') { event.preventDefault(); event.stopImmediatePropagation(); dismissVoiceError(); return; }
        if (voiceState().reading) { event.preventDefault(); event.stopImmediatePropagation(); void stopReading(); }
        return;
      }
      if (voiceKeysSuspended() || !(event.altKey || event.ctrlKey || event.metaKey || /^F\d+$/.test(event.key))) return;
      const binding = canonicalKey(chordFromEvent(event));
      const kind: DictationKind | null = keysOf('voice.dictate').includes(binding) ? 'composer' : keysOf('voice.command').includes(binding) ? 'palette' : null;
      if (!kind) return;
      // Another dialog has the window (a confirmation, Settings' key recorder…): the key is its own.
      if (otherDialogOpen()) return;
      event.preventDefault(); event.stopImmediatePropagation();
      if (event.repeat || pressed) return;
      const modifiers = binding.split('+').slice(0, -1).map(m => m === 'Ctrl' ? 'Control' : m);
      pressed = { kind, at: Date.now(), code: event.code, modifiers, stops: false };
      setDictationKeyHeld(true);
      // A second press finishes toggle dictation.
      if (dictating()) { pressed.stops = true; void stopDictation(); return; }
      begin(kind, behaviour() === 'toggle' ? 'toggle' : 'hold');
    };
    const release = (held: number, stops: boolean) => {
      setDictationKeyHeld(false);
      if (stops || behaviour() === 'toggle') return;
      if (behaviour() === 'both' && held < TAP_MS) { setDictationMode('toggle'); return; }
      void stopDictation();
    };
    const keyup = (event: KeyboardEvent) => {
      if (lone && event.code === lone.code) {
        const { at, started, toggled, stops } = lone; dropLone();
        const now = Date.now();
        if (stops) { event.preventDefault(); event.stopImmediatePropagation(); if (dictating()) void stopDictation(); return; }
        if (toggled) { event.preventDefault(); event.stopImmediatePropagation(); return; }
        if (started) {
          event.preventDefault(); event.stopImmediatePropagation();
          setDictationKeyHeld(false);
          if (voiceState().mode === 'hold') void stopDictation();
          return;
        }
        // Released before it was a hold: a tap, maybe the first of two.
        lastTap = now - at < TAP_MS ? { code: event.code, at: now } : null;
        return;
      }
      if (!pressed) return;
      const ends = event.code === pressed.code || pressed.modifiers.includes(event.key);
      if (!ends) return;
      event.preventDefault(); event.stopImmediatePropagation();
      const { at, stops } = pressed; pressed = null;
      release(Date.now() - at, stops);
    };
    // The window lost the keyboard with the key down: the keyup will not come.
    const blur = () => {
      if (pressed) { const { at, stops } = pressed; pressed = null; release(Math.max(TAP_MS, Date.now() - at), stops); }
      if (lone) {
        const { started, toggled } = lone; dropLone(); lastTap = null;
        if (started && !toggled) { setDictationKeyHeld(false); if (voiceState().mode === 'hold') void stopDictation(); }
      }
    };
    const pointerdown = () => interrupt();
    window.addEventListener('keydown', keydown, true);
    window.addEventListener('keyup', keyup, true);
    window.addEventListener('blur', blur);
    window.addEventListener('pointerdown', pointerdown, true);
    return () => {
      dropLone();
      window.removeEventListener('keydown', keydown, true); window.removeEventListener('keyup', keyup, true); window.removeEventListener('blur', blur); window.removeEventListener('pointerdown', pointerdown, true);
    };
  }, []);

  // Leaving the window (unmount) never leaves a microphone open.
  useEffect(() => () => { if (dictating()) cancelDictation(); }, []);

  // The palette is a modal dialog, above anything else: it shows its own (inline) pill.
  return state.kind === 'palette' && document.querySelector('dialog.command-palette[open]') ? null : <VoicePill state={state} keymap={keymap} onOpenSettings={onOpenSettings}/>;
}

/** A modal dialog other than the command palette is open: the rest of the window is inert. */
function otherDialogOpen(): boolean {
  return [...document.querySelectorAll('dialog[open]')].some(dialog => !dialog.classList.contains('command-palette') && (() => { try { return dialog.matches(':modal'); } catch { return true; } })());
}

const pillKey = 'lazybox:voice-pill:offset';
function readOffset(): { x: number; y: number } {
  try { const value = JSON.parse(localStorage.getItem(pillKey) || 'null'); if (value && Number.isFinite(value.x) && Number.isFinite(value.y)) return value; } catch { /* a convenience */ }
  return { x: 0, y: 0 };
}

/**
 * While the microphone is open, and while something is read aloud: what is
 * happening (and where the text goes), the level, the time, whether audio
 * stays on this device, and how to stop. Drag it anywhere; it stays there.
 */
export function VoicePill({ state, keymap, onOpenSettings, inline }: { state: ReturnType<typeof useVoiceState>; keymap?: Keymap; onOpenSettings?(): void; inline?: boolean }) {
  const [now, setNow] = useState(Date.now());
  const [offset, setOffset] = useState(readOffset);
  const listening = state.phase === 'starting' || state.phase === 'listening' || state.phase === 'stopping';
  // Just above the message box being dictated into, so it never covers the words; else low in the window.
  const [anchor, setAnchor] = useState<{ bottom: number; left: number } | undefined>();
  useLayoutEffect(() => {
    const box = state.kind === 'composer' ? document.querySelector('textarea.is-dictating')?.closest('form')?.getBoundingClientRect() : undefined;
    setAnchor(box && box.top > 80 ? { bottom: Math.round(window.innerHeight - box.top + 10), left: Math.round(box.left + box.width / 2) } : undefined);
  }, [state.kind, listening]);
  useEffect(() => {
    if (!listening) return;
    const timer = setInterval(() => setNow(Date.now()), 250);
    return () => clearInterval(timer);
  }, [listening]);
  // Started from the mic button and the key never worked here: it may be another app's.
  const keyUnproven = useMemo(() => !voiceKeyUsed(), [state.startedAt]);
  if (!listening && state.phase !== 'error' && !state.reading) return null;

  const movable = (event: { target: EventTarget; button: number }) => !inline && event.button === 0 && !(event.target as HTMLElement).closest('button');
  // Dragged anywhere (a drag gesture: no text selected); Escape puts it back.
  const down = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (!movable(event)) return;
    event.preventDefault();
    const x = event.clientX, y = event.clientY, from = offset;
    let last = from;
    beginDrag('move', {
      pointerId: event.pointerId, capture: event.currentTarget,
      onMove: next => {
        const half = window.innerWidth / 2 - 120;
        last = { x: Math.max(-half, Math.min(half, from.x + next.clientX - x)), y: Math.max(-window.innerHeight + 140, Math.min(60, from.y + next.clientY - y)) };
        setOffset(last);
      },
      onEnd: reason => {
        if (reason === 'escape') { setOffset(from); return; }
        try { localStorage.setItem(pillKey, JSON.stringify(last)); } catch { /* a convenience */ }
      },
    });
  };
  const press = (event: ReactMouseEvent<HTMLDivElement>) => { if (movable(event)) preventMouseDown(event); };
  const style: CSSProperties | undefined = inline ? undefined : { transform: `translate(calc(-50% + ${offset.x}px), ${offset.y}px)`, ...(anchor && listening ? anchor : {}) };
  const key = bindings(keymap?.['voice.dictate'])[0];
  const keyText = key ? displayBinding(key).join(' ') : '';

  if (state.phase === 'error') {
    return <div className={'voice-pill is-error' + (inline ? ' is-inline' : '')} role="alert" style={style} onPointerDown={down} onMouseDown={press}>
      <span className="voice-pill-icon"><Icon name="alert" size={15}/></span>
      <span className="voice-pill-text">{state.error}</span>
      {state.errorCode === 'dictation-off' && canOpenVoiceSystemSettings()
        ? <button type="button" className="voice-pill-button" onClick={() => void openVoiceSystemSettings('keyboard').catch(() => undefined)}>Open Keyboard settings</button>
        : onOpenSettings && <button type="button" className="voice-pill-button" onClick={() => { dismissVoiceError(); onOpenSettings(); }}>Voice settings</button>}
      <button type="button" className="icon-button voice-pill-close" aria-label="Dismiss" onClick={dismissVoiceError}><Icon name="close" size={13}/></button>
    </div>;
  }
  if (!listening) {
    return <div className="voice-pill is-reading" role="status" aria-label="Reading aloud" style={style} onPointerDown={down} onMouseDown={press}>
      <span className="voice-pill-icon"><Icon name="speaker" size={15}/></span>
      <span className="voice-pill-text">Reading aloud</span>
      <kbd>Esc</kbd>
      <button type="button" className="icon-button voice-pill-close" aria-label="Stop reading aloud" title="Stop reading aloud · Esc" onClick={() => void stopReading()}><Icon name="stop" size={13}/></button>
    </div>;
  }
  const what = state.phase === 'starting' ? 'Starting the microphone…' : state.phase === 'stopping' ? 'Finishing…' : state.kind === 'palette' ? 'Say a command' : 'Listening';
  const hint = state.mode === 'hold' ? 'Release to finish' : keyText ? `${key && isLoneModifier(key) ? 'Tap ' : ''}${keyText} to finish` : 'Stop to finish';
  const level = state.level ?? (state.hearing ? 0.6 : 0.12);
  // Sending while listening waits for the last words (Composer, finishAndSend); ⌘↵ is Send now, a plain send while the agent is idle.
  const sendChord = state.kind === 'composer' ? bindings(keymap?.['chat.send-now']).find(isChord) : undefined;
  const tip = !inline && state.kind === 'composer' && keyUnproven && keyText;
  return <div className={'voice-pill' + (inline ? ' is-inline' : '')} data-phase={state.phase} data-kind={state.kind} role="status" aria-label={`${what}. ${state.destination ? state.destination + '. ' : ''}${state.onDevice ? 'On-device.' : ''}`} style={style} onPointerDown={down} onMouseDown={press}>
    <span className="voice-pill-dot" aria-hidden="true"/>
    <span className={'voice-pill-meter' + (state.level === undefined ? ' is-estimated' : '')} aria-hidden="true">
      {[0.55, 0.8, 1, 0.8, 0.55].map((weight, i) => <i key={i} style={{ transform: `scaleY(${Math.max(0.18, Math.min(1, level * weight * 1.4))})` }}/>)}
    </span>
    <span className="voice-pill-text" title={state.destination}>{what}{state.destination && <span className="voice-pill-destination"> · {state.destination}</span>}</span>
    <span className="voice-pill-time" aria-label="Elapsed">{elapsed(now - state.startedAt)}</span>
    {state.phase === 'listening' && <span className={'voice-pill-privacy' + (state.onDevice ? ' is-local' : '')} title={state.onDevice ? 'Recognized on this device: the audio never leaves it.' : `${state.engine}: the audio is sent to its speech service.`}>
      <Icon name={state.onDevice ? 'lock' : 'cloud'} size={12}/>{state.onDevice ? 'On-device' : 'Online'}
    </span>}
    <span className="voice-pill-hint">{hint}{sendChord && <> · <kbd>{displayBinding(sendChord).join(' ')}</kbd> send</>} · <kbd>Esc</kbd> cancel</span>
    <button type="button" className="icon-button voice-pill-stop" aria-label="Stop dictation" title="Stop and keep the text" onClick={() => void stopDictation()} disabled={state.phase !== 'listening'}><Icon name="stop" size={13}/></button>
    <button type="button" className="icon-button voice-pill-close" aria-label="Cancel dictation" title="Cancel · Esc" onClick={cancelDictation}><Icon name="close" size={13}/></button>
    {tip && <span className="voice-pill-tip" role="note">
      <span>If {keyText} does nothing, another app (ChatGPT, Raycast, Alfred) may use it.</span>
      {onOpenSettings && <button type="button" onClick={() => { void stopDictation(); onOpenSettings(); }}>Pick another key</button>}
    </span>}
  </div>;
}
