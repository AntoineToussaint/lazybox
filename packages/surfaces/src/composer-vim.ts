/** Modal (Vim) editing for the chat message box.
 *
 * Pure: one key plus the current text and cursor give the next text, cursor
 * and mode, so every command is tested without a browser. The textarea
 * adapter is useComposerVim. Insert mode never goes through here except for
 * Escape: typing, IME composition and paste stay native. */

export type VimMode = 'normal' | 'insert' | 'visual';
export type VimRegister = { text: string; linewise: boolean };
type Snapshot = { text: string; cursor: number };
export type VimState = {
  mode: VimMode;
  /** Keys of an unfinished command, such as ["2", "d"] or ["g"]. */
  pending: string[];
  /** Visual mode: where the selection started, and the end that moves. */
  anchor: number;
  head: number;
  /** The column j and k try to keep; Infinity after $. */
  column: number | null;
  undo: Snapshot[];
  redo: Snapshot[];
  /** The text when the current insert began: one insert is one undo step. */
  insertStart: Snapshot | null;
};
export type VimBuffer = { text: string; cursor: number };
export type VimOutcome = {
  state: VimState;
  text: string;
  cursor: number;
  register: VimRegister;
  /** The key belonged to Vim: the caller cancels the browser's default. */
  handled: boolean;
  /** Enter in normal mode: the caller sends the message. */
  send?: boolean;
};

const UNDO_LIMIT = 200;
export const emptyRegister: VimRegister = { text: '', linewise: false };
export function initialVimState(mode: VimMode = 'insert'): VimState {
  return { mode, pending: [], anchor: 0, head: 0, column: null, undo: [], redo: [], insertStart: null };
}

/** The keys this layer understands, from a KeyboardEvent. Anything else is "". */
export function vimKeyName(event: { key: string; ctrlKey: boolean; metaKey: boolean; altKey: boolean }): string {
  if (event.metaKey || event.altKey) return '';
  if (event.ctrlKey) return event.key === 'r' || event.key === 'R' ? 'Ctrl+r' : event.key === '[' ? 'Escape' : '';
  return event.key;
}

const aliases: Record<string, string> = { ArrowLeft: 'h', ArrowRight: 'l', ArrowDown: 'j', ArrowUp: 'k', Home: '0', End: '$', Backspace: 'h', ' ': 'l', Delete: 'x' };
const motions = new Set(['h', 'j', 'k', 'l', 'w', 'b', 'e', '0', '^', '$', 'G']);

// Text geometry. Offsets are UTF-16 indexes into the text, like a textarea's.
const lineStart = (text: string, i: number) => text.lastIndexOf('\n', i - 1) + 1;
const lineEnd = (text: string, i: number) => { const end = text.indexOf('\n', i); return end < 0 ? text.length : end; };
const firstNonBlank = (text: string, i: number) => { let j = lineStart(text, i); const end = lineEnd(text, i); while (j < end && /[ \t]/.test(text[j])) j++; return j; };
/** In normal mode the cursor sits on a character, never past the end of its line. */
export function clampNormal(text: string, i: number): number {
  i = Math.max(0, Math.min(i, text.length));
  const start = lineStart(text, i), end = lineEnd(text, i);
  return end > start ? Math.min(i, end - 1) : start;
}
const lineIndex = (text: string, i: number) => { let n = 0; for (let j = text.indexOf('\n'); j >= 0 && j < i; j = text.indexOf('\n', j + 1)) n++; return n; };
const lineCount = (text: string) => lineIndex(text, text.length) + 1;
const offsetOfLine = (text: string, line: number) => { let i = 0; for (let n = 0; n < line; n++) { const next = text.indexOf('\n', i); if (next < 0) break; i = next + 1; } return i; };
const atColumn = (text: string, start: number, column: number) => Math.min(start + column, Math.max(start, lineEnd(text, start) - 1));

// Words follow Vim: letters, digits and _ form a word, other punctuation
// another, whitespace separates, and an empty line is a word of its own.
const kind = (ch: string) => /\s/.test(ch) ? 0 : /\w/.test(ch) ? 1 : 2;
function nextWordStart(text: string, i: number): number {
  const n = text.length; if (i >= n) return n;
  let j = i; const k = kind(text[j]);
  if (k) while (j < n && kind(text[j]) === k) j++;
  while (j < n && kind(text[j]) === 0) { if (text[j] === '\n' && text[j + 1] === '\n') return j + 1; j++; }
  return j;
}
function previousWordStart(text: string, i: number): number {
  let j = i - 1;
  while (j > 0 && kind(text[j]) === 0) { if (text[j] === '\n' && text[j - 1] === '\n') return j; j--; }
  if (j <= 0) return 0;
  const k = kind(text[j]); while (j > 0 && kind(text[j - 1]) === k) j--;
  return j;
}
function wordEnd(text: string, i: number): number {
  const n = text.length; let j = i + 1;
  while (j < n && kind(text[j]) === 0) j++;
  if (j >= n) return Math.max(0, n - 1);
  const k = kind(text[j]); while (j + 1 < n && kind(text[j + 1]) === k) j++;
  return j;
}

type Motion = { to: number; linewise?: boolean; inclusive?: boolean; column?: number | null };
function motion(name: string, text: string, c: number, count: number, explicit: boolean, operator: boolean, column: number | null): Motion | null {
  const start = lineStart(text, c), end = lineEnd(text, c);
  const vertical = (lines: number): Motion => {
    const target = Math.max(0, Math.min(lineCount(text) - 1, lineIndex(text, c) + lines));
    const want = column ?? c - start, from = offsetOfLine(text, target);
    return { to: Number.isFinite(want) ? atColumn(text, from, want) : clampNormal(text, lineEnd(text, from)), linewise: true, column: want };
  };
  switch (name) {
    case 'h': return { to: Math.max(start, c - count) };
    case 'l': return { to: Math.min(operator ? end : Math.max(start, end - 1), c + count) };
    case '0': return { to: start };
    case '^': return { to: firstNonBlank(text, c) };
    case '$': { const target = vertical(count - 1).to, last = lineEnd(text, target); return operator ? { to: last } : { to: clampNormal(text, last), column: Infinity }; }
    case 'w': {
      let to = c, from = c;
      for (let n = 0; n < count; n++) { from = to; to = nextWordStart(text, to); }
      // Vim: an operator never carries "w" past the end of the line it ends on.
      if (operator && text[from] !== '\n') { const eol = text.indexOf('\n', from); if (eol >= 0 && eol < to) to = eol; }
      return { to };
    }
    case 'b': { let to = c; for (let n = 0; n < count; n++) to = previousWordStart(text, to); return { to }; }
    case 'e': { let to = c; for (let n = 0; n < count; n++) to = wordEnd(text, to); return { to, inclusive: true }; }
    case 'j': return vertical(count);
    case 'k': return vertical(-count);
    case 'gg': case 'G': {
      const line = explicit ? count - 1 : name === 'gg' ? 0 : lineCount(text) - 1;
      const from = offsetOfLine(text, Math.max(0, Math.min(lineCount(text) - 1, line)));
      return { to: firstNonBlank(text, from), linewise: true };
    }
  }
  return null;
}

type Result = Omit<VimOutcome, 'state' | 'register' | 'handled'> & { state: VimState; register?: VimRegister };
const record = (state: VimState, text: string, cursor: number): VimState => ({ ...state, undo: [...state.undo, { text, cursor }].slice(-UNDO_LIMIT), redo: [] });
const toNormal = (state: VimState): VimState => ({ ...state, mode: 'normal', pending: [], insertStart: null });
const toInsert = (state: VimState, before: Snapshot): VimState => ({ ...state, mode: 'insert', pending: [], column: null, insertStart: before });

/** Applies an operator (d, c, y) to [from, to) or, linewise, to the lines from..to. */
function operate(op: string, state: VimState, text: string, c: number, a: number, b: number, linewise: boolean, cursorAfterYank: number): Result {
  if (linewise) {
    const ls = lineStart(text, Math.min(a, b)), le = lineEnd(text, Math.max(a, b));
    const register = { text: text.slice(ls, le) + '\n', linewise: true };
    if (op === 'y') return { state: { ...state, pending: [] }, text, cursor: clampNormal(text, cursorAfterYank), register };
    if (op === 'c') { const next = text.slice(0, ls) + text.slice(le); return { state: toInsert(state, { text, cursor: c }), text: next, cursor: ls, register }; }
    const [from, to] = le < text.length ? [ls, le + 1] : ls > 0 ? [ls - 1, le] : [0, text.length];
    const next = text.slice(0, from) + text.slice(to);
    return { state: { ...record(state, text, c), pending: [], column: null }, text: next, cursor: firstNonBlank(next, Math.min(from === ls ? ls : lineStart(next, from), next.length)), register };
  }
  const [from, to] = [Math.min(a, b), Math.max(a, b)];
  const register = { text: text.slice(from, to), linewise: false };
  if (op === 'y') return { state: { ...state, pending: [] }, text, cursor: clampNormal(text, cursorAfterYank), register };
  const next = text.slice(0, from) + text.slice(to);
  if (op === 'c') return { state: toInsert(state, { text, cursor: c }), text: next, cursor: from, register };
  if (from === to) return { state: { ...state, pending: [] }, text, cursor: c };
  return { state: { ...record(state, text, c), pending: [], column: null }, text: next, cursor: clampNormal(next, from), register };
}

function paste(state: VimState, text: string, c: number, register: VimRegister, before: boolean, count: number): Result {
  if (!register.text) return { state: { ...state, pending: [] }, text, cursor: c };
  const content = register.text.repeat(count), saved = record(state, text, c);
  if (register.linewise) {
    if (before) { const at = lineStart(text, c); const next = text.slice(0, at) + content + text.slice(at); return { state: { ...saved, pending: [] }, text: next, cursor: firstNonBlank(next, at) }; }
    const le = lineEnd(text, c);
    if (le < text.length) { const next = text.slice(0, le + 1) + content + text.slice(le + 1); return { state: { ...saved, pending: [] }, text: next, cursor: firstNonBlank(next, le + 1) }; }
    const next = text + '\n' + content.slice(0, -1);
    return { state: { ...saved, pending: [] }, text: next, cursor: firstNonBlank(next, le + 1) };
  }
  const at = before || lineEnd(text, c) === lineStart(text, c) ? c : c + 1;
  const next = text.slice(0, at) + content + text.slice(at);
  return { state: { ...saved, pending: [] }, text: next, cursor: at + content.length - 1 };
}

/** Text that arrives outside Vim's keys (a native paste) while not inserting. */
export function vimInsertText(state: VimState, buffer: VimBuffer, value: string): Omit<VimOutcome, 'register' | 'handled'> {
  const { text } = buffer;
  if (state.mode === 'visual') {
    const from = Math.min(state.anchor, state.head), to = Math.min(text.length, Math.max(state.anchor, state.head) + 1);
    const next = text.slice(0, from) + value + text.slice(to);
    return { state: toNormal(record(state, text, state.head)), text: next, cursor: clampNormal(next, from + value.length - 1) };
  }
  const c = buffer.cursor, next = text.slice(0, c) + value + text.slice(c);
  return { state: { ...record(state, text, c), pending: [] }, text: next, cursor: clampNormal(next, c + value.length - 1) };
}

/** The selection a textarea shows: a caret while inserting, a one-character
 * block in normal mode, and the inclusive range in visual mode. */
export function vimSelection(state: VimState, text: string, cursor: number): [number, number] {
  if (state.mode === 'visual') return [Math.min(state.anchor, state.head), Math.min(text.length, Math.max(state.anchor, state.head) + 1)];
  if (state.mode === 'normal' && cursor < text.length && text[cursor] !== '\n') return [cursor, cursor + 1];
  return [cursor, cursor];
}

/** The status text for the mode chip, e.g. "NORMAL" or "NORMAL 2d". */
export function vimStatus(state: VimState): string {
  return state.mode.toUpperCase() + (state.pending.length ? ' ' + state.pending.join('') : '');
}

const isDigit = (key: string, sofar: string) => /^[0-9]$/.test(key) && !(key === '0' && !sofar);

/** Handles one key. `key` is a KeyboardEvent key, or a name from vimKeyName. */
export function vimKey(state: VimState, buffer: VimBuffer, key: string, register: VimRegister = emptyRegister): VimOutcome {
  const { text } = buffer;
  const done = (result: Result, handled = true): VimOutcome => ({ register, handled, ...result });
  if (state.mode === 'insert') {
    if (key !== 'Escape') {
      // The first key of an insert that no command started marks its undo step.
      const start = state.insertStart ?? { text, cursor: buffer.cursor };
      return done({ state: state.insertStart ? state : { ...state, insertStart: start }, text, cursor: buffer.cursor }, false);
    }
    const start = state.insertStart;
    const saved = start && start.text !== text ? record(state, start.text, start.cursor) : state;
    return done({ state: toNormal({ ...saved, column: null }), text, cursor: clampNormal(text, Math.max(lineStart(text, buffer.cursor), buffer.cursor - 1)) }, false);
  }
  key = aliases[key] ?? key;
  const c = state.mode === 'visual' ? state.head : clampNormal(text, buffer.cursor);
  // Modifier keys, Tab, function keys and IME keys are not Vim's to take.
  if (key.length !== 1 && key !== 'Escape' && key !== 'Enter' && key !== 'Ctrl+r') return done({ state, text, cursor: c }, false);
  if (key === 'Escape') return done({ state: toNormal(state), text, cursor: c }, false);
  if (key === 'Enter') return state.mode === 'normal' && !state.pending.length ? done({ state, text, cursor: c, send: true }) : done({ state: { ...state, pending: [] }, text, cursor: c });

  const tokens = [...state.pending, key];
  let i = 0, count = '', operator = '', operatorCount = '';
  while (i < tokens.length && isDigit(tokens[i], count)) count += tokens[i++];
  if (state.mode === 'normal' && i < tokens.length && 'dcy'.includes(tokens[i]) && tokens[i].length === 1) {
    operator = tokens[i++];
    while (i < tokens.length && isDigit(tokens[i], operatorCount)) operatorCount += tokens[i++];
  }
  const rest = tokens.slice(i).join('');
  const explicit = !!(count || operatorCount), n = (Number(count) || 1) * (Number(operatorCount) || 1);
  const wait = (): VimOutcome => done({ state: { ...state, pending: tokens }, text, cursor: c });
  const reset = (): VimOutcome => done({ state: { ...state, pending: [] }, text, cursor: c });
  if (!rest || rest === 'g') return wait();
  const name = rest === 'gg' ? 'gg' : rest;
  const isMotion = motions.has(name) || name === 'gg';

  if (state.mode === 'visual') {
    if (isMotion) {
      const m = motion(name, text, c, n, explicit, false, state.column)!;
      const head = clampNormal(text, m.to);
      return done({ state: { ...state, pending: [], head, column: m.column === undefined ? null : m.column }, text, cursor: head });
    }
    const from = Math.min(state.anchor, state.head), to = Math.min(text.length, Math.max(state.anchor, state.head) + 1);
    if (name === 'v') return done({ state: toNormal(state), text, cursor: c });
    if (name === 'y') { const result = operate('y', state, text, c, from, to, false, from); return done({ ...result, state: toNormal(result.state) }); }
    if (name === 'd' || name === 'x') { const result = operate('d', state, text, c, from, to, false, from); return done({ ...result, state: toNormal(result.state) }); }
    if (name === 'c' || name === 's') return done(operate('c', state, text, c, from, to, false, from));
    return reset();
  }

  if (operator) {
    if (name === operator) {
      const target = Math.min(lineCount(text) - 1, lineIndex(text, c) + n - 1);
      return done(operate(operator, state, text, c, c, offsetOfLine(text, target), true, c));
    }
    if (!isMotion) return reset();
    // "cw" on a word changes to its end, like "ce", and keeps the space after it.
    const m = operator === 'c' && name === 'w' && kind(text[c] ?? ' ') !== 0 ? motion('e', text, c, n, explicit, true, null)! : motion(name, text, c, n, explicit, true, state.column)!;
    if (m.linewise) return done(operate(operator, state, text, c, c, m.to, true, Math.min(c, m.to)));
    const to = m.inclusive ? Math.min(text.length, m.to + 1) : m.to;
    return done(operate(operator, state, text, c, c, to, false, Math.min(c, m.to)));
  }

  if (isMotion) {
    const m = motion(name, text, c, n, explicit, false, state.column)!;
    return done({ state: { ...state, pending: [], column: m.column === undefined ? null : m.column }, text, cursor: clampNormal(text, m.to) });
  }
  const before = { text, cursor: c }, start = lineStart(text, c), end = lineEnd(text, c);
  switch (name) {
    case 'i': return done({ state: toInsert(state, before), text, cursor: c });
    case 'a': return done({ state: toInsert(state, before), text, cursor: end > start ? c + 1 : c });
    case 'A': return done({ state: toInsert(state, before), text, cursor: end });
    case 'I': return done({ state: toInsert(state, before), text, cursor: firstNonBlank(text, c) });
    case 'o': return done({ state: toInsert(state, before), text: text.slice(0, end) + '\n' + text.slice(end), cursor: end + 1 });
    case 'O': return done({ state: toInsert(state, before), text: text.slice(0, start) + '\n' + text.slice(start), cursor: start });
    case 'v': return done({ state: { ...state, mode: 'visual', pending: [], anchor: c, head: c }, text, cursor: c });
    case 'x': return end > start ? done(operate('d', state, text, c, c, Math.min(end, c + n), false, c)) : reset();
    case 'X': return c > start ? done(operate('d', state, text, c, Math.max(start, c - n), c, false, c)) : reset();
    case 'D': case 'C': { const m = motion('$', text, c, n, explicit, true, null)!; return done(operate(name === 'D' ? 'd' : 'c', state, text, c, c, m.to, false, c)); }
    case 'p': case 'P': return done(paste(state, text, c, register, name === 'P', n));
    case 'u': case 'Ctrl+r': {
      let next = state, current = { text, cursor: c };
      for (let k = 0; k < n; k++) {
        const from = name === 'u' ? next.undo : next.redo;
        const snapshot = from.at(-1); if (!snapshot) break;
        next = name === 'u' ? { ...next, undo: from.slice(0, -1), redo: [...next.redo, current] } : { ...next, redo: from.slice(0, -1), undo: [...next.undo, current] };
        current = snapshot;
      }
      return done({ state: { ...next, pending: [], column: null }, text: current.text, cursor: clampNormal(current.text, current.cursor) });
    }
  }
  return reset();
}
