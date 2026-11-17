import { useEffect, useLayoutEffect, useRef, useState, type CSSProperties, type RefObject } from 'react';
import { mergeDictation } from './voice-model';
import { finishAndSend, registerDictationTarget, startDictation, stopDictation, takeOverDictation, useVoiceState, voiceState, recognizers, type DictationSink, type DictationTarget } from './voice-session';

/** The span of the draft still being recognized, drawn over the message box. */
export type LiveRange = { start: number; end: number };

/**
 * A message box as a dictation target. Dictated words go where the caret
 * was (or at the end), joined with spaces and capitals as text is; the words
 * still being recognized are marked. Esc or "cancel" puts the draft back as
 * it was; typing or sending takes over (the words stay, the microphone
 * closes). The draft is saved when the dictation ends, not on every word.
 */
export function useComposerDictation({ textarea, value, enabled, onPreview, onCommit, onSend }: {
  textarea: RefObject<HTMLTextAreaElement | null>;
  value: string;
  enabled: boolean;
  /** Show this text in the box (while dictating; not saved). */
  onPreview(text: string): void;
  /** The text the dictation leaves: saved like typing. */
  onCommit(text: string): void;
  /** "Send" was said and sending by voice is on. */
  onSend(): void;
}) {
  const latest = useRef({ value, enabled, onPreview, onCommit, onSend }); latest.current = { value, enabled, onPreview, onCommit, onSend };
  const [live, setLive] = useState<LiveRange | null>(null);
  const [active, setActive] = useState(false);
  const pendingSend = useRef<string | null>(null);
  const target = useRef<DictationTarget | null>(null);
  /** Send once the box holds `text` (the chat sends what it shows); now, if it already does. */
  const sendWhenShown = (text: string) => {
    pendingSend.current = text;
    requestAnimationFrame(() => {
      if (pendingSend.current !== text || latest.current.value !== text) return;
      pendingSend.current = null;
      latest.current.onSend();
    });
  };

  useEffect(() => {
    const own: DictationTarget = {
      kind: 'composer',
      element: () => textarea.current,
      available: () => latest.current.enabled && !!textarea.current && !textarea.current.disabled && !textarea.current.readOnly,
      begin(): DictationSink | null {
        const box = textarea.current;
        if (!box) return null;
        const original = latest.current.value;
        const focused = document.activeElement === box;
        // Vim normal mode shows its cursor as a one-letter selection: dictate after it, like `a`.
        const normal = box.dataset.vimMode === 'normal';
        const to = focused ? box.selectionEnd : original.length, from = focused && !normal ? box.selectionStart : to;
        const before = original.slice(0, from), after = original.slice(to);
        box.focus({ preventScroll: true });
        setActive(true);
        const place = (at: number) => requestAnimationFrame(() => { const node = textarea.current; if (node && document.activeElement === node) { node.setSelectionRange(at, at); } });
        // Words were shown in the box: its text, and so its caret, changed.
        let previewed = false;
        return {
          update(view) {
            if (view.text) previewed = true;
            const merged = mergeDictation(before, after, view.text);
            latest.current.onPreview(merged.text);
            setLive(view.liveStart < view.text.length ? { start: merged.start + view.liveStart, end: merged.end } : null);
            place(merged.end);
          },
          end(reason, view) {
            setActive(false); setLive(null);
            if (reason === 'takeover') return;
            // Nothing said: the draft stays exactly as it was (a selection is not replaced by silence).
            if (reason === 'cancel' || reason === 'error' || !view.text) {
              latest.current.onCommit(original);
              // Put the caret back only where words had moved it. Untouched, it stays where the
              // person has it: a later placement would undo the key that ended this dictation
              // (⌥→ after Right ⌥, a slow ⌥ chord) and send the next dictation to the old spot.
              if (previewed) place(to);
              // ⌘↵ with nothing new said still sends what the box holds.
              if (reason === 'send') sendWhenShown(original);
              return;
            }
            const merged = mergeDictation(before, after, view.text);
            latest.current.onCommit(merged.text);
            place(merged.end);
            if (reason === 'send') sendWhenShown(merged.text);
          },
        };
      },
    };
    target.current = own;
    return registerDictationTarget(own);
  }, [textarea]);

  // Send once the box holds the dictated text (the chat sends what it shows).
  useEffect(() => {
    if (pendingSend.current === null || value !== pendingSend.current) return;
    pendingSend.current = null;
    latest.current.onSend();
  }, [value]);

  const state = useVoiceState();
  const listening = active && (state.phase === 'starting' || state.phase === 'listening' || state.phase === 'stopping');
  return {
    /** This box is taking dictation now. */
    active: listening,
    live,
    /** The mic button: start here (toggle), or stop. */
    toggle() {
      if (active && voiceState().phase !== 'idle') { void stopDictation(); return; }
      if (target.current) void startDictation({ mode: 'toggle', target: target.current });
    },
    /** The person typed: their text stands. */
    takeOver() { if (active) takeOverDictation(); },
    /**
     * The person sent (↵, ⌘↵, the Send button) while this box was listening:
     * the last words settle, then the message is sent. False when it was not
     * listening (send as usual).
     */
    finishAndSend() { return active && voiceState().phase !== 'idle' && finishAndSend(); },
    /** Whether this window can dictate at all (a recognizer exists). */
    supported: recognizers().length > 0,
  };
}

const copied = ['fontFamily', 'fontSize', 'fontWeight', 'fontStyle', 'lineHeight', 'letterSpacing', 'wordSpacing', 'textIndent', 'textTransform', 'tabSize', 'paddingTop', 'paddingRight', 'paddingBottom', 'paddingLeft', 'direction', 'textAlign'] as const;

/**
 * The live words, marked over the textarea at the same place: a transparent
 * copy of its text with the live span tinted and underlined, scrolled with it.
 */
export function DictationOverlay({ textarea, text, range }: { textarea: RefObject<HTMLTextAreaElement | null>; text: string; range: LiveRange | null }) {
  const [style, setStyle] = useState<CSSProperties | null>(null);
  const [scroll, setScroll] = useState(0);
  useLayoutEffect(() => {
    const box = textarea.current;
    if (!range || !box) { setStyle(null); return; }
    const computed = getComputedStyle(box);
    const next: CSSProperties = { left: box.offsetLeft + box.clientLeft, top: box.offsetTop + box.clientTop, width: box.clientWidth, height: box.clientHeight };
    for (const property of copied) (next as Record<string, string | number>)[property] = computed[property];
    setStyle(next);
    setScroll(box.scrollTop);
  }, [textarea, text, range?.start, range?.end]);
  useEffect(() => {
    const box = textarea.current;
    if (!range || !box) return;
    const follow = () => setScroll(box.scrollTop);
    box.addEventListener('scroll', follow);
    return () => box.removeEventListener('scroll', follow);
  }, [textarea, !!range]);
  if (!range || !style) return null;
  return <div className="dictation-overlay" aria-hidden="true" style={style}>
    <div style={{ transform: `translateY(${-scroll}px)` }}>
      {text.slice(0, range.start)}<mark data-dictation-live>{text.slice(range.start, range.end)}</mark>{text.slice(range.end)}{text.endsWith('\n') ? ' ' : ''}
    </div>
  </div>;
}
