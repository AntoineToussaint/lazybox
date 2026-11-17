import { test } from 'node:test';
import assert from 'node:assert/strict';
import { parseDiff, isLine, type DiffLine } from '../packages/surfaces/src/review-model.ts';
import { contentKey, contextFolds, escapeHtml, expandGap, fileLines, foldedRows, hunkGaps, hunkRows, markRanges, splitIndexes, splitRows, trailingGap, wordDiff, wordHighlights } from '../packages/surfaces/src/diff-layout.ts';

// A 20-line file; the diff changes line 3 and line 15.
const FILE = Array.from({ length: 20 }, (_, i) => `line ${i + 1}`);
const NEW_FILE = FILE.map((line, i) => i === 2 ? 'line three changed' : i === 14 ? 'line 15 is new' : line);
NEW_FILE.splice(15, 0, 'inserted after 15');
const DIFF = `diff --git a/f.txt b/f.txt
--- a/f.txt
+++ b/f.txt
@@ -1,6 +1,6 @@
 line 1
 line 2
-line 3
+line three changed
 line 4
 line 5
 line 6
@@ -12,7 +12,8 @@ section
 line 12
 line 13
 line 14
-line 15
+line 15 is new
+inserted after 15
 line 16
 line 17
 line 18
`;
const text = (row: { kind: string; text: string } | undefined) => row?.text;

test('side by side pairs removed and added runs, context on both sides', () => {
  const rows = parseDiff(DIFF), split = splitRows(rows);
  const pairs = split.filter(row => row.kind === 'pair').map(row => row.kind === 'pair' ? [text(rows[row.old!]), text(rows[row.new!])] : []);
  assert.deepEqual(pairs.slice(0, 4), [['line 1', 'line 1'], ['line 2', 'line 2'], ['line 3', 'line three changed'], ['line 4', 'line 4']]);
  // Two added lines for one removed: the extra one has no left side.
  assert.deepEqual(pairs.slice(9, 12), [['line 15', 'line 15 is new'], [undefined, 'inserted after 15'], ['line 16', 'line 16']]);
  // Hunk headers span both columns; every line row appears exactly once.
  assert.deepEqual(split.filter(row => row.kind === 'full').map(row => row.kind === 'full' && rows[row.row].kind), ['meta', 'meta', 'meta', 'hunk', 'hunk']);
  const shown = split.flatMap(splitIndexes).sort((a, b) => a - b);
  assert.deepEqual(shown, rows.map((_, i) => i));
});

test('a removed run longer than its replacement leaves right cells empty; adds after context stand alone', () => {
  const rows = parseDiff('@@ -1,4 +1,3 @@\n-a\n-b\n-c\n+x\n ctx\n+y\n');
  const split = splitRows(rows).filter(row => row.kind === 'pair');
  assert.deepEqual(split.map(row => row.kind === 'pair' ? [text(rows[row.old!]), text(rows[row.new!])] : []), [['a', 'x'], ['b', undefined], ['c', undefined], ['ctx', 'ctx'], [undefined, 'y']]);
});

test('the Runner\'s new-file preview is all added lines, on the right', () => {
  const rows = parseDiff('New file: notes.md\n\n# Notes\nsecond\n');
  const split = splitRows(rows);
  assert.equal(split[0].kind, 'full');
  assert.deepEqual(split.slice(1).map(row => row.kind === 'pair' && [row.old, text(rows[row.new!])]), [[undefined, '# Notes'], [undefined, 'second']]);
  assert.equal(hunkGaps('New file: notes.md\n\n# Notes\n').size, 0);
});

test('word diff marks only what changed, as character ranges', () => {
  const diff = wordDiff('func Handle() {', 'func Handle() error {')!;
  assert.deepEqual(diff.old, []);
  assert.deepEqual(diff.new.map(r => 'func Handle() error {'.slice(r.start, r.end)), ['error ']);
  const renamed = wordDiff('const total = price * count;', 'const sum = price * quantity;')!;
  assert.deepEqual(renamed.old.map(r => 'const total = price * count;'.slice(r.start, r.end)), ['total', 'count']);
  assert.deepEqual(renamed.new.map(r => 'const sum = price * quantity;'.slice(r.start, r.end)), ['sum', 'quantity']);
  // Two changed words separated by a space are one mark.
  const joined = wordDiff('return a b;', 'return x y;')!;
  assert.deepEqual(joined.new.map(r => 'return x y;'.slice(r.start, r.end)), ['x y']);
  // Nothing in common worth marking, identical, or too long: no word marks.
  assert.equal(wordDiff('alpha beta gamma', 'completely different words here'), undefined);
  assert.equal(wordDiff('same', 'same'), undefined);
  assert.equal(wordDiff('x'.repeat(2000), 'y'.repeat(2000)), undefined);
});

test('word highlights pair the i-th removed line with the i-th added line', () => {
  const rows = parseDiff(DIFF), marks = wordHighlights(rows);
  const index = (value: string) => rows.findIndex(row => isLine(row) && row.text === value);
  // "line 3" → "line three changed" shares too little to be worth marking.
  assert.equal(marks.has(index('line three changed')), false);
  assert.deepEqual(marks.get(index('line 15 is new'))?.map(r => 'line 15 is new'.slice(r.start, r.end)), [' is new']);
  assert.equal(marks.has(index('inserted after 15')), false);
  assert.equal(marks.has(index('line 1')), false);
});

test('marks survive syntax-highlighting markup and entities', () => {
  const html = '<span class="hljs-keyword">func</span> Handle() &amp;error {';
  // Characters: "func Handle() &error {" — mark "Handle() &e".
  assert.equal(markRanges(html, [{ start: 5, end: 16 }]), '<span class="hljs-keyword">func</span> <mark class="diff-word">Handle() &amp;e</mark>rror {');
  // A range crossing a tag closes and reopens the mark around it.
  assert.equal(markRanges('<span class="k">ab</span>cd', [{ start: 1, end: 3 }]), '<span class="k">a<mark class="diff-word">b</mark></span><mark class="diff-word">c</mark>d');
  assert.equal(markRanges(escapeHtml('a<b'), [{ start: 1, end: 2 }]), 'a<mark class="diff-word">&lt;</mark>b');
  assert.equal(markRanges('plain', []), 'plain');
});

test('long unchanged runs fold, keeping context next to changes', () => {
  const body = ['@@ -1,14 +1,14 @@', '-old', '+new', ...Array.from({ length: 12 }, (_, i) => ` same ${i + 2}`), ' tail'].join('\n');
  const rows = parseDiff(body + '\n');
  const folds = contextFolds(rows);
  // 13 unchanged lines after the change, none after: 3 kept, the last 10 folded.
  assert.deepEqual(folds, [{ id: 'fold:5', startLine: 5, endLine: 14, count: 10 }]);
  const { hidden, starts } = foldedRows(rows, folds, new Set());
  assert.equal(hidden.size, 10);
  assert.deepEqual([...starts.values()].map(f => f.id), ['fold:5']);
  assert.equal(foldedRows(rows, folds, new Set(['fold:5'])).hidden.size, 0);
  // A comment's row is never hidden; short runs never fold.
  const commented = rows.findIndex(row => isLine(row) && row.newLine === 8);
  assert.deepEqual(contextFolds(rows, 3, 4, new Set([commented])), []);
  assert.deepEqual(contextFolds(parseDiff(DIFF)), []);
  // Between two changes, context is kept on both sides.
  const middle = parseDiff(['@@ -1,12 +1,12 @@', '-a', '+b', ...Array.from({ length: 10 }, (_, i) => ` m${i}`), '-c', '+d'].join('\n') + '\n');
  assert.deepEqual(contextFolds(middle).map(f => [f.startLine, f.endLine]), [[5, 8]]);
});

test('gaps between hunks are counted and expand from the file, joining hunks', () => {
  assert.deepEqual([...hunkGaps(DIFF)], [[2, 5]]);
  assert.equal(trailingGap(DIFF, NEW_FILE.length), 21 - 19);
  const joined = expandGap(DIFF, NEW_FILE, 2);
  const rows = parseDiff(joined);
  assert.equal(hunkRows(rows).length, 1);
  assert.match(joined, /^@@ -1,18 \+1,19 @@\n/m);
  const lines = rows.filter(isLine) as DiffLine[];
  // Every line still has the right numbers on both sides.
  for (const line of lines) {
    if (line.type !== 'add') assert.equal(line.text, FILE[line.oldLine! - 1]);
    if (line.type !== 'del') assert.equal(line.text, NEW_FILE[line.newLine! - 1]);
  }
  // The rest of the file.
  const all = parseDiff(expandGap(joined, NEW_FILE, 2)).filter(isLine) as DiffLine[];
  assert.equal(all.at(-1)!.newLine, NEW_FILE.length);
  assert.equal(trailingGap(expandGap(joined, NEW_FILE, 2), NEW_FILE.length), 0);
});

test('expanding before the first hunk, and refusing a file that moved on', () => {
  const late = `@@ -5,3 +5,3 @@\n line 5\n-line 6\n+six\n line 7\n`;
  const file = FILE.slice(0, 10).map((line, i) => i === 5 ? 'six' : line);
  assert.deepEqual([...hunkGaps(late)], [[1, 4]]);
  const expanded = parseDiff(expandGap(late, file, 1)).filter(isLine) as DiffLine[];
  assert.deepEqual(expanded.slice(0, 2).map(l => [l.oldLine, l.newLine, l.text]), [[1, 1, 'line 1'], [2, 2, 'line 2']]);
  assert.throws(() => expandGap(late, file.map(l => l === 'six' ? 'seven' : l), 1), /changed since/);
  // A pure insertion: the empty old side is numbered by the line before it.
  const insertion = `@@ -3,0 +4,1 @@\n+added\n`;
  const withInsert = [...FILE.slice(0, 3), 'added', ...FILE.slice(3)];
  assert.deepEqual([...hunkGaps(insertion)], [[1, 3]]);
  const out = expandGap(insertion, withInsert, 1);
  assert.match(out, /^@@ -1,3 \+1,4 @@/);
  const lines = parseDiff(out).filter(isLine) as DiffLine[];
  assert.deepEqual(lines.map(l => [l.oldLine, l.newLine]), [[1, 1], [2, 2], [3, 3], [undefined, 4]]);
});

test('content keys change with the diff text', () => {
  assert.equal(contentKey('abc'), contentKey('abc'));
  assert.notEqual(contentKey('abc'), contentKey('abd'));
  assert.deepEqual(fileLines('a\nb\n'), ['a', 'b']);
  assert.deepEqual(fileLines(''), []);
});
