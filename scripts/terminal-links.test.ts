import assert from 'node:assert/strict';
import { test } from 'node:test';
import { linksInRows, trimLink, type TerminalRow } from '../packages/surfaces/src/terminal-links';

/** A logical line as a terminal `cols` wide wraps it: rows of single-cell characters. */
const wrap = (text: string, cols: number): TerminalRow[] => {
  const rows: TerminalRow[] = [];
  for (let i = 0; i < text.length; i += cols) { const part = text.slice(i, i + cols); rows.push({ text: part, cells: [...part].map((_, x) => x) }); }
  return rows;
};

const signIn = 'https://accounts.example.com/o/oauth2/auth?response_type=code&client_id=32555940559.apps.example.com&redirect_uri=https%3A%2F%2Fsdk.example.com%2Fauthcode.html&scope=openid+https%3A%2F%2Fwww.example.com%2Fauth%2Fuserinfo.email+https%3A%2F%2Fwww.example.com%2Fauth%2Fcloud-platform&state=Qx7fZk2bH0&prompt=consent&token_usage=remote&access_type=offline&code_challenge=AbCdEf0123456789_-AbCdEf0123456789_-AbCdEf0&code_challenge_method=S256';

test('a sign-in URL wrapped across rows is found whole, from its first cell to its last', () => {
  const line = '    ' + signIn;
  const links = linksInRows(wrap(line, 80));
  assert.equal(links.length, 1);
  assert.equal(links[0].url, signIn);
  assert.deepEqual(links[0].start, { row: 0, col: 4 });
  const last = line.length - 1;
  assert.deepEqual(links[0].end, { row: Math.floor(last / 80), col: last % 80 });
});

test('sentence punctuation and an unmatched bracket are not part of the address; a matched one is', () => {
  assert.equal(trimLink('https://example.com/page.'), 'https://example.com/page');
  assert.equal(trimLink('https://example.com/a,'), 'https://example.com/a');
  assert.equal(trimLink('https://example.com/a)'), 'https://example.com/a');
  assert.equal(trimLink('https://en.example.org/wiki/Shell_(computing)'), 'https://en.example.org/wiki/Shell_(computing)');
  const [link] = linksInRows(wrap('Open (https://example.com/x?y=1) now.', 200));
  assert.equal(link.url, 'https://example.com/x?y=1');
  assert.deepEqual([link.start.col, link.end.col], [6, 30]);
});

test('several links in a line, a wide character before one, and no link in plain text', () => {
  const text = '界 http://localhost:3000/ and https://example.com';
  const rows: TerminalRow[] = [{ text, cells: [0, ...Array.from({ length: text.length - 1 }, (_, i) => i + 2)] }];
  const links = linksInRows(rows);
  assert.deepEqual(links.map(link => link.url), ['http://localhost:3000/', 'https://example.com']);
  assert.equal(links[0].start.col, 3, 'the wide character takes two cells');
  assert.deepEqual(linksInRows(wrap('no address here: example.com or http:// alone', 40)), []);
});
