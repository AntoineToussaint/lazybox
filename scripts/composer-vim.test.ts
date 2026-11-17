import { test } from 'node:test';
import assert from 'node:assert/strict';
import { initialVimState, vimKey, vimInsertText, vimSelection, vimStatus, vimKeyName, emptyRegister, type VimMode, type VimRegister, type VimState } from '../packages/surfaces/src/composer-vim';

/** Feeds keys like the textarea adapter does. "|" marks the cursor; <Esc>,
 * <CR>, <C-r> and <BS> are special keys; {text} is typed natively in insert mode. */
function run(start: string, keys: string, mode: VimMode = 'normal', register: VimRegister = emptyRegister) {
  let text = start.replace('|', ''), cursor = start.indexOf('|'), state: VimState = initialVimState(mode), sent = 0;
  const handled: boolean[] = [];
  for (const [, special, typed, char] of keys.matchAll(/<([^>]+)>|\{([^}]*)\}|(.)/gs)) {
    if (typed !== undefined) {
      assert.equal(state.mode, 'insert', `typing "${typed}" needs insert mode`);
      // The adapter consults Vim before each native keystroke in insert mode.
      state = vimKey(state, { text, cursor }, typed[0] ?? 'x', register).state;
      text = text.slice(0, cursor) + typed + text.slice(cursor); cursor += typed.length; continue;
    }
    const key = special ? ({ Esc: 'Escape', CR: 'Enter', 'C-r': 'Ctrl+r', BS: 'Backspace', Shift: 'Shift' } as Record<string, string>)[special] : char;
    const out = vimKey(state, { text, cursor }, key, register);
    ({ state, text, cursor, register } = out); handled.push(out.handled); if (out.send) sent++;
  }
  return { text, cursor, mode: state.mode, register, state, sent, handled, marked: text.slice(0, cursor) + '|' + text.slice(cursor) };
}

test('insert commands place the caret where Vim does', () => {
  assert.equal(run('hel|lo', 'i').marked, 'hel|lo');
  assert.equal(run('hel|lo', 'a').marked, 'hell|o');
  assert.equal(run('  hel|lo', 'I').marked, '  |hello');
  assert.equal(run('hel|lo', 'A').marked, 'hello|');
  assert.equal(run('one|\ntwo', 'o').marked, 'one\n|\ntwo');
  assert.equal(run('one\ntw|o', 'O').marked, 'one\n|\ntwo');
  for (const key of 'iaAIoO') assert.equal(run('ab|c', key).mode, 'insert');
});

test('Escape leaves insert mode one character to the left and is never swallowed', () => {
  const out = run('abc|', '<Esc>', 'insert');
  assert.equal(out.marked, 'ab|c'); assert.equal(out.mode, 'normal'); assert.deepEqual(out.handled, [false]);
  assert.equal(run('a\n|b', '<Esc>', 'insert').marked, 'a\n|b');
});

test('motions: h j k l w b e 0 $ ^ gg G, with counts', () => {
  assert.equal(run('ab|cd', 'h').marked, 'a|bcd');
  assert.equal(run('ab|cd', 'l').marked, 'abc|d');
  assert.equal(run('ab|cd', '9l').marked, 'abc|d', 'l stops on the last character');
  assert.equal(run('|one two three four', 'w').marked, 'one |two three four');
  assert.equal(run('|one two three four', '3w').marked, 'one two three |four');
  assert.equal(run('|foo.bar baz', 'w').marked, 'foo|.bar baz', 'punctuation is its own word');
  assert.equal(run('one two thr|ee', 'b').marked, 'one two |three');
  assert.equal(run('one two thr|ee', '2b').marked, 'one |two three');
  assert.equal(run('|one two', 'e').marked, 'on|e two');
  assert.equal(run('  ab|c', '0').marked, '|  abc');
  assert.equal(run('  ab|c', '^').marked, '  |abc');
  assert.equal(run('a|bc\nd', '$').marked, 'ab|c\nd');
  assert.equal(run('one\ntwo\nthr|ee', 'gg').marked, '|one\ntwo\nthree');
  assert.equal(run('o|ne\ntwo\nthree', 'G').marked, 'one\ntwo\n|three');
  assert.equal(run('o|ne\ntwo\nthree', '2G').marked, 'one\n|two\nthree');
  assert.equal(run('abc|d\nxy\nlonger', 'jj').marked, 'abcd\nxy\nlon|ger', 'j keeps the wanted column across a short line');
  assert.equal(run('abcd\nxy\nlon|ger', 'k').marked, 'abcd\nx|y\nlonger');
  assert.equal(run('|one\n\ntwo', 'w').marked, 'one\n|\ntwo', 'an empty line is a word');
  assert.equal(run('|abcd', '2<Shift>l').marked, 'ab|cd', 'modifier keys keep the pending count');
});

test('deletes, changes and yanks fill the register', () => {
  let out = run('a|bcd', 'x'); assert.equal(out.marked, 'a|cd'); assert.deepEqual(out.register, { text: 'b', linewise: false });
  assert.equal(run('a|bcd', '2x').marked, 'a|d');
  assert.equal(run('ab|c', '5x').marked, 'a|b', 'x at the end lands on the new last character');
  assert.equal(run('|one two three', 'dw').marked, '|two three');
  assert.equal(run('|one two three', '2dw').marked, '|three');
  assert.equal(run('|one two three', 'd2w').marked, '|three');
  assert.equal(run('one |two\nthree', 'dw').marked, 'one| \nthree', 'dw never joins the next line');
  assert.equal(run('one t|wo three', 'D').marked, 'one |t');
  assert.equal(run('one\nt|wo\nthree', 'dd').marked, 'one\n|three');
  assert.equal(run('one\nt|wo\nthree', '2dd').marked, '|one');
  assert.equal(run('one\nt|wo', 'dd').marked, '|one', 'deleting the last line removes the newline before it');
  out = run('one\nt|wo\nthree', 'yy'); assert.equal(out.text, 'one\ntwo\nthree'); assert.deepEqual(out.register, { text: 'two\n', linewise: true });
  out = run('|one two', 'yw'); assert.deepEqual(out.register, { text: 'one ', linewise: false });
  out = run('|one two', 'cw'); assert.equal(out.marked, '| two'); assert.equal(out.mode, 'insert');
  out = run('one |two three', 'cw{TWO}<Esc>'); assert.equal(out.text, 'one TWO three'); assert.equal(out.mode, 'normal');
  out = run('  one\nt|wo', 'cc'); assert.equal(out.marked, '  one\n|'); assert.equal(out.mode, 'insert');
  out = run('one t|wo', 'C'); assert.equal(out.marked, 'one t|'); assert.equal(out.mode, 'insert');
  assert.equal(run('|one\ntwo\nthree', 'dG').text, '');
  assert.equal(run('one\ntwo\nthr|ee', 'dgg').text, '');
  assert.equal(run('one\nt|wo\nthree', 'dj').text, 'one');
  assert.equal(run('ab|cd', 'd$').marked, 'a|b');
  assert.equal(run('ab|cd', 'd0').marked, '|cd');
  assert.equal(run('ab|cd', 'X').marked, 'a|cd');
});

test('p and P paste characters after or before the cursor, lines below or above', () => {
  assert.equal(run('a|bc', 'xp').marked, 'ac|b');
  assert.equal(run('a|bc', 'xP').marked, 'a|bc');
  assert.equal(run('a|bc', 'yl3p').marked, 'abbb|bc');
  assert.equal(run('o|ne\ntwo', 'yyjp').marked, 'one\ntwo\n|one');
  assert.equal(run('one\nt|wo', 'yykP').marked, '|two\none\ntwo');
  assert.equal(run('one\nt|wo\nthree', 'ddp').marked, 'one\nthree\n|two');
  assert.equal(run('a|b', 'p').marked, 'a|b', 'an empty register pastes nothing');
});

test('u undoes one command or one whole insert; Ctrl-r redoes', () => {
  let out = run('one |two three', 'dwx');
  assert.equal(out.text, 'one hree');
  out = run('one |two three', 'dwxu'); assert.equal(out.text, 'one three');
  out = run('one |two three', 'dwxuu'); assert.equal(out.marked, 'one |two three');
  out = run('one |two three', 'dwx2u<C-r>'); assert.equal(out.text, 'one three');
  out = run('one |two three', 'dwx2u2<C-r>'); assert.equal(out.text, 'one hree');
  out = run('ab|c', 'i{XYZ}<Esc>a{!}<Esc>u'); assert.equal(out.text, 'abXYZc', 'each insert is one step');
  out = run('ab|c', 'i{XYZ}<Esc>a{!}<Esc>uu'); assert.equal(out.text, 'abc');
  out = run('ab|c', 'i<Esc>u'); assert.equal(out.text, 'abc'); assert.equal(out.state.undo.length, 0, 'an empty insert records nothing');
  out = run('|', '{hello}<Esc>u', 'insert'); assert.equal(out.text, '', 'typing from the default insert mode is undoable');
  out = run('a|bc', 'xux'); assert.equal(out.state.redo.length, 0, 'a new change clears redo');
});

test('visual mode selects inclusively and y d c act on it', () => {
  let out = run('one |two three', 'v');
  assert.equal(out.mode, 'visual'); assert.deepEqual(vimSelection(out.state, out.text, out.cursor), [4, 5]);
  out = run('one |two three', 've'); assert.deepEqual(vimSelection(out.state, out.text, out.cursor), [4, 7]);
  out = run('one |two three', 'vey'); assert.deepEqual(out.register, { text: 'two', linewise: false }); assert.equal(out.mode, 'normal'); assert.equal(out.marked, 'one |two three');
  out = run('one |two three', 'ved'); assert.equal(out.marked, 'one | three'); assert.equal(out.mode, 'normal');
  out = run('one |two three', 'vec{2}<Esc>'); assert.equal(out.text, 'one 2 three');
  out = run('one tw|o three', 'vb'); assert.deepEqual(vimSelection(out.state, out.text, out.cursor), [4, 7], 'selection extends backwards from the anchor');
  out = run('one |two', 'vl<Esc>'); assert.equal(out.mode, 'normal');
  out = run('one |two', 'vlv'); assert.equal(out.mode, 'normal');
  out = run('one |two three', 'vedu'); assert.equal(out.text, 'one two three');
});

test('Enter in normal mode sends; counts and operators wait with a visible status', () => {
  let out = run('hello|', '<CR>'); assert.equal(out.sent, 1); assert.equal(out.text, 'hello');
  out = run('hello|', '<CR>', 'insert'); assert.equal(out.sent, 0); assert.deepEqual(out.handled, [false], 'insert-mode Enter keeps the regular send/newline handling');
  out = run('a|b', '2d'); assert.equal(vimStatus(out.state), 'NORMAL 2d');
  out = run('a|b', '2d<Esc>'); assert.equal(vimStatus(out.state), 'NORMAL');
  out = run('a|b', 'd<CR>'); assert.equal(out.sent, 0, 'Enter cancels a pending operator rather than sending');
  out = run('ab|c', 'zq'); assert.equal(out.text, 'abc'); assert.deepEqual(out.handled, [true, true], 'unknown letters are swallowed, not typed');
});

test('a native paste while not inserting lands at the cursor or replaces the selection', () => {
  const state = { ...initialVimState('normal') };
  let out = vimInsertText(state, { text: 'abc', cursor: 1 }, 'XY');
  assert.equal(out.text, 'aXYbc'); assert.equal(out.cursor, 2); assert.equal(out.state.undo.length, 1);
  out = vimInsertText({ ...state, mode: 'visual', anchor: 1, head: 2 }, { text: 'abcd', cursor: 2 }, 'Z');
  assert.equal(out.text, 'aZd'); assert.equal(out.state.mode, 'normal');
});

test('only Ctrl-r and Ctrl-[ are Vim chords; Cmd and Alt shortcuts stay with the browser', () => {
  assert.equal(vimKeyName({ key: 'r', ctrlKey: true, metaKey: false, altKey: false }), 'Ctrl+r');
  assert.equal(vimKeyName({ key: '[', ctrlKey: true, metaKey: false, altKey: false }), 'Escape');
  assert.equal(vimKeyName({ key: 'v', ctrlKey: false, metaKey: true, altKey: false }), '');
  assert.equal(vimKeyName({ key: 'b', ctrlKey: true, metaKey: false, altKey: false }), '');
  assert.equal(vimKeyName({ key: 'w', ctrlKey: false, metaKey: false, altKey: false }), 'w');
});
