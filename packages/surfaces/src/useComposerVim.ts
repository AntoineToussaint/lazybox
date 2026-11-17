import { useEffect, useRef, useState, type ClipboardEvent, type KeyboardEvent, type RefObject } from 'react';
import { clampNormal, emptyRegister, initialVimState, vimInsertText, vimKey, vimKeyName, vimSelection, vimStatus, type VimMode, type VimRegister, type VimState } from './composer-vim';

// One unnamed register for every message box in the window, like Vim's.
let register: VimRegister = emptyRegister;

export type ComposerVim = {
  /** Undefined when Vim editing is off: the textarea behaves as before. */
  mode?: VimMode;
  status: string;
  hint: string;
  textareaProps: {
    'data-vim-mode'?: VimMode;
    onKeyDown?(event: KeyboardEvent<HTMLTextAreaElement>): void;
    onPaste?(event: ClipboardEvent<HTMLTextAreaElement>): void;
    onSelect?(): void;
    onFocus?(): void;
  };
};

const hints: Record<VimMode, string> = { normal: 'Enter to send · i to type', insert: 'Esc for normal mode', visual: 'y copy · d delete · c change · Esc to leave' };

/** Vim editing over the chat textarea. Insert mode is the textarea itself:
 * typing, IME composition, paste and the draft all stay native. Normal and
 * visual mode commands write through execCommand so the change reaches
 * React's onChange (and the saved draft) exactly like typing does. Keys in
 * `reserved` (the leader's first key) always reach the keyboard dispatcher. */
export function useComposerVim({ textarea, enabled, value, resetKey, reserved, onSend }: { textarea: RefObject<HTMLTextAreaElement | null>; enabled: boolean; value: string; resetKey: string; reserved: readonly string[]; onSend(): void }): ComposerVim {
  const state = useRef<VimState>(initialVimState());
  const [status, setStatus] = useState(() => ({ mode: state.current.mode, text: vimStatus(state.current) }));
  const written = useRef<string | null>(null), writing = useRef(false), shown = useRef<[number, number] | null>(null);
  const send = useRef(onSend); send.current = onSend;
  const set = (next: VimState) => { state.current = next; setStatus(previous => previous.mode === next.mode && previous.text === vimStatus(next) ? previous : { mode: next.mode, text: vimStatus(next) }); };
  const show = (el: HTMLTextAreaElement, cursor: number) => {
    const [start, end] = vimSelection(state.current, el.value, cursor);
    el.setSelectionRange(start, end, state.current.mode === 'visual' && state.current.head < state.current.anchor ? 'backward' : 'forward');
    shown.current = [start, end];
  };
  const write = (el: HTMLTextAreaElement, next: string) => {
    const previous = el.value; if (previous === next) return;
    written.current = next;
    // Replace only the span that differs: the browser's own undo, scroll and
    // composition state stay intact, and React sees an ordinary input event.
    let start = 0; while (start < previous.length && start < next.length && previous[start] === next[start]) start++;
    let a = previous.length, b = next.length; while (a > start && b > start && previous[a - 1] === next[b - 1]) { a--; b--; }
    el.setSelectionRange(start, a);
    writing.current = true;
    try { if (!(start === b ? document.execCommand('delete') : document.execCommand('insertText', false, next.slice(start, b)))) throw new Error('execCommand unavailable'); }
    catch { /* fall through to the setter below */ }
    finally { writing.current = false; }
    if (el.value !== next) {
      Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value')!.set!.call(el, next);
      el.dispatchEvent(new Event('input', { bubbles: true }));
    }
  };

  useEffect(() => { set(initialVimState()); }, [resetKey, enabled]);
  // A send (or anything else outside Vim) that empties the box starts the next message in insert mode.
  useEffect(() => { if (value === '' && written.current !== '' && state.current.mode !== 'insert') set(initialVimState()); written.current = null; }, [value]);
  // Outside insert mode the textarea takes no typing: Vim's keys are the only edits.
  useEffect(() => {
    const el = textarea.current; if (!enabled || !el) return;
    const guard = (event: InputEvent) => { if (!writing.current && state.current.mode !== 'insert' && !['historyUndo', 'historyRedo', 'insertFromDrop', 'deleteByCut'].includes(event.inputType)) event.preventDefault(); };
    const input = () => { if (!writing.current && state.current.mode === 'visual') { set({ ...state.current, mode: 'normal', pending: [] }); show(el, clampNormal(el.value, el.selectionStart)); } };
    el.addEventListener('beforeinput', guard); el.addEventListener('input', input);
    return () => { el.removeEventListener('beforeinput', guard); el.removeEventListener('input', input); };
  }, [enabled, textarea]);

  if (!enabled) return { status: '', hint: '', textareaProps: {} };
  return {
    mode: status.mode, status: status.text, hint: hints[status.mode],
    textareaProps: {
      'data-vim-mode': status.mode,
      onKeyDown(event) {
        const native = event.nativeEvent, el = event.currentTarget, current = state.current;
        if (native.isComposing || native.keyCode === 229) return;
        const key = vimKeyName(native);
        if (!key) return;
        if (current.mode !== 'insert' && reserved.includes(key)) { if (current.pending.length) set({ ...current, pending: [] }); return; }
        const out = vimKey(current, { text: el.value, cursor: current.mode === 'visual' ? current.head : el.selectionStart }, key, register);
        register = out.register;
        if (out.handled) event.preventDefault();
        if (current.mode === 'insert' && out.state.mode === 'insert') { state.current = out.state; return; }
        write(el, out.text);
        set(out.state);
        show(el, out.cursor);
        if (out.send) send.current();
      },
      onPaste(event) {
        const el = event.currentTarget, current = state.current;
        if (current.mode === 'insert') return;
        event.preventDefault();
        const text = event.clipboardData.getData('text/plain');
        if (!text) return;
        const out = vimInsertText(current, { text: el.value, cursor: current.mode === 'visual' ? current.head : el.selectionStart }, text);
        write(el, out.text); set(out.state); show(el, out.cursor);
      },
      // A click places the normal-mode block; a drag or double-click selects in visual mode.
      onSelect() {
        const el = textarea.current, current = state.current;
        if (!el || current.mode === 'insert' || document.activeElement !== el) return;
        const start = el.selectionStart, end = el.selectionEnd;
        if (shown.current && shown.current[0] === start && shown.current[1] === end) return;
        if (end - start > 1) { set({ ...current, mode: 'visual', pending: [], anchor: start, head: end - 1, column: null }); shown.current = [start, end]; return; }
        set({ ...current, mode: 'normal', pending: [], column: null }); show(el, clampNormal(el.value, start));
      },
      onFocus() {
        const el = textarea.current; if (!el) return;
        if (!el.value) { if (state.current.mode !== 'insert') set({ ...initialVimState(), undo: state.current.undo }); return; }
        if (state.current.mode === 'normal') show(el, clampNormal(el.value, el.selectionStart));
      },
    },
  };
}
