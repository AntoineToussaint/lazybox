import assert from 'node:assert/strict';
import test from 'node:test';
import { parseAnsi, stripAnsi } from '../packages/surfaces/src/ansi.tsx';

const E = '\x1b';

test('truecolor, reset and plain text', () => {
  const [line] = parseAnsi(`${E}[38;2;6;182;211m✓ workspace — ready${E}[0m done`);
  assert.equal(line.map(s => s.text).join(''), '✓ workspace — ready done');
  assert.equal(line[0].style.fg, 'rgb(6,182,211)');
  assert.deepEqual(line[1].style, {});
});

test('16 and 256 colours, bold and its reset', () => {
  const [line] = parseAnsi(`${E}[1;31merror${E}[22m plain-red ${E}[38;5;214mamber${E}[39m`);
  assert.equal(line[0].style.bold, true);
  assert.equal(line[0].style.fg, 'var(--ansi-1)');
  assert.equal(line[1].style.bold, undefined);
  assert.equal(line[1].style.fg, 'var(--ansi-1)');
  assert.equal(line[2].style.fg, 'rgb(255,175,0)');
  assert.equal(line[3]?.style.fg, undefined);
});

test('styles carry across lines until reset', () => {
  const lines = parseAnsi(`${E}[32mone\ntwo${E}[0m\nthree`);
  assert.equal(lines[1][0].style.fg, 'var(--ansi-2)');
  assert.deepEqual(lines[2][0].style, {});
});

test('cursor and erase sequences, OSC titles are dropped; carriage return keeps the last redraw', () => {
  assert.equal(stripAnsi(`${E}[2K${E}[1Gloading 10%\rloading 50%\rloading 100%\ndone${E}]0;title\x07!`), 'loading 100%\ndone!');
  assert.equal(stripAnsi('line with trailing CR\r\nnext'), 'line with trailing CR\nnext');
});
