import { test } from 'node:test';
import assert from 'node:assert/strict';
import { PASTE_BOX_CHARS, PASTE_BOX_LINES, admitFiles, boxIcon, boxKind, boxSubtitle, deliveryNote, detectLanguage, insertText, isPastedName, languageOfName, lineCount, moveItem, pastedTextFile, pastedTextOf, pdfPageCount, replaceText, restorePastedText, shouldBoxPaste, textSummary } from '../packages/surfaces/src/attachment-boxes.ts';

test('a paste becomes a box past 800 characters or 12 lines, not at them', () => {
  assert.equal(PASTE_BOX_CHARS, 800); assert.equal(PASTE_BOX_LINES, 12);
  assert.equal(shouldBoxPaste('x'.repeat(800)), false);
  assert.equal(shouldBoxPaste('x'.repeat(801)), true);
  assert.equal(shouldBoxPaste(Array(12).fill('a').join('\n')), false);
  assert.equal(shouldBoxPaste(Array(12).fill('a').join('\n') + '\n'), false, 'a final newline is not a thirteenth line');
  assert.equal(shouldBoxPaste(Array(13).fill('a').join('\n')), true);
  assert.equal(shouldBoxPaste('a short sentence'), false);
  assert.equal(lineCount(''), 0); assert.equal(lineCount('a'), 1); assert.equal(lineCount('a\nb\n'), 2); assert.equal(lineCount('a\n\nb'), 3);
});

test('a pasted box is a plain-text file named for when it was pasted, its extension the language', () => {
  const now = new Date(2026, 10, 17, 9, 5, 7);
  const log = 'ERROR something failed\n'.repeat(20);
  const file = pastedTextFile(log, now);
  assert.equal(file.name, 'Pasted text 2026-11-17 at 09.05.07.txt');
  assert.equal(file.type, 'text/plain');
  assert.equal(file.size, new TextEncoder().encode(log).length);
  assert.equal(pastedTextOf(file), log);
  assert.ok(isPastedName(file.name));
  assert.equal(boxKind(file), 'text');
  const code = pastedTextFile('package main\n\nimport "fmt"\n\nfunc main() {\n\tfmt.Println("hi")\n}\n', now);
  assert.equal(code.name, 'Pasted text 2026-11-17 at 09.05.07.go');
  assert.equal(languageOfName(code.name), 'go');
  assert.equal(boxIcon('text', code.name), 'file-code');
  assert.equal(boxIcon('text', file.name), 'file-text');
  // An edit keeps the name; a restored draft knows its text without reading the file.
  const edited = replaceText(code, 'package other\n');
  assert.equal(edited.name, code.name); assert.equal(pastedTextOf(edited), 'package other\n');
  assert.equal(pastedTextOf(restorePastedText(file.name, 'kept', 1)), 'kept');
  assert.equal(pastedTextOf(new File(['x'], 'notes.txt')), undefined, 'a dropped file is not a paste');
});

test('the language of pasted code, only when the signals are clear', () => {
  const lang = (text: string) => detectLanguage(text)?.language ?? null;
  assert.equal(lang('{"a": 1, "b": [true, null]}'), 'json');
  assert.equal(lang('diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n'), 'diff');
  assert.equal(lang('#!/usr/bin/env bash\nset -e\necho hi\n'), 'sh');
  assert.equal(lang('def main():\n    print("hi")\n\nclass Thing(Base):\n    pass\n\nimport os\n'), 'python');
  assert.equal(lang('export interface Box { name: string; size: number; }\nexport type Kind = "a" | "b";\nconst x: string = "y";\nimport { a } from "./a";\n'), 'typescript');
  assert.equal(lang('const a = require("a");\nconst b = 2;\nfunction go() {\n  return a;\n}\n'), 'javascript');
  assert.equal(lang('fn main() {\n    let mut x = 1;\n}\nuse std::io;\nimpl Thing {\n}\n'), 'rust');
  assert.equal(lang('SELECT id, name\nFROM users\nWHERE id = 1\nORDER BY name;\n'), 'sql');
  assert.equal(lang('The deploy failed at 10:31 because the disk was full.\nWe retried twice; it failed the same way.\n'), null, 'prose is plain text');
  assert.equal(lang('2026-11-17T10:31:22Z INFO started\n2026-11-17T10:31:23Z ERROR disk full\n'), null, 'a log is plain text');
  assert.equal(lang(''), null);
});

test('a box’s kind, icon and the line under its title', () => {
  assert.equal(boxKind({ name: 'shot.png', type: 'image/png' }), 'image');
  assert.equal(boxKind({ name: 'drawing.svg', type: 'image/svg+xml' }), 'text', 'SVG is shown as its source, never rendered');
  assert.equal(boxKind({ name: 'report.pdf', type: 'application/pdf' }), 'pdf');
  assert.equal(boxKind({ name: 'report.PDF', type: '' }), 'pdf');
  assert.equal(boxKind({ name: 'main.go', mediaType: 'application/octet-stream' }), 'text');
  assert.equal(boxKind({ name: 'data.json', type: 'application/json' }), 'text');
  assert.equal(boxKind({ name: 'archive.bin', type: '' }), 'file');
  assert.equal(boxIcon('pdf'), 'file-pdf'); assert.equal(boxIcon('file'), 'file');
  assert.equal(boxSubtitle({ name: 'Pasted text 2026-11-17 at 09.05.07.txt', size: 4000, kind: 'text', lines: 142 }), 'Pasted text · 142 lines');
  assert.equal(boxSubtitle({ name: 'Pasted text 2026-11-17 at 09.05.07.py', size: 4000, kind: 'text', lines: 1 }), 'Pasted Python · 1 line');
  assert.equal(boxSubtitle({ name: 'Pasted text 2026-11-17 at 09.05.07.txt', size: 4000, kind: 'text' }), 'Pasted text · 4 KiB', 'before its lines are counted');
  assert.equal(boxSubtitle({ name: 'notes.txt', size: 3000, kind: 'text', lines: 40 }), '40 lines · 3 KiB');
  assert.equal(boxSubtitle({ name: 'report.pdf', size: 1258291, kind: 'pdf', pages: 12 }), 'PDF · 12 pages · 1.2 MiB');
  assert.equal(boxSubtitle({ name: 'report.pdf', size: 2048, kind: 'pdf' }), 'PDF · 2 KiB');
  assert.equal(boxSubtitle({ name: 'archive.bin', size: 6, kind: 'file' }), '6 B');
  assert.deepEqual(textSummary('\n\n  first   real line  \nsecond\n'), { title: 'first real line', lines: 4 });
  assert.equal(textSummary('x'.repeat(200)).title.length, 80);
});

test('the page count of a PDF, from its page tree', () => {
  const pdf = (body: string) => new TextEncoder().encode(body);
  assert.equal(pdfPageCount(pdf('%PDF-1.7\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n2 0 obj << /Type /Pages /Kids [3 0 R 4 0 R 5 0 R] /Count 3 >> endobj\n3 0 obj << /Type /Page /Parent 2 0 R >> endobj\n')), 3);
  assert.equal(pdfPageCount(pdf('%PDF-1.4\n2 0 obj << /Count 12 /Kids [] /Type /Pages >> endobj\n7 0 obj << /Type /Pages /Parent 2 0 R /Count 5 >> endobj')), 12, 'the root’s count, not a branch’s');
  assert.equal(pdfPageCount(pdf('%PDF-1.4\n3 0 obj << /Type /Page >> endobj 4 0 obj << /Type/Page >> endobj 5 0 obj << /Type /Pages >> endobj')), 2, 'without a count, the pages');
  assert.equal(pdfPageCount(pdf('%PDF-1.5\n(all objects compressed)')), undefined);
  assert.equal(pdfPageCount(pdf('not a pdf /Type /Pages /Count 9')), undefined);
});

test('files over a limit become error boxes that name the limit; the rest are attached in order', () => {
  const MiB = 1 << 20;
  const file = (name: string, size: number) => ({ name, size }) as File;
  const limits = { maxFiles: 4, maxFileBytes: 20 * MiB, maxMessageBytes: 20 * MiB };
  const big = admitFiles([], [file('video.mov', 25 * MiB), file('notes.txt', 100)], limits);
  assert.deepEqual(big.accepted.map(f => f.name), ['notes.txt']);
  assert.deepEqual(big.rejected, [{ name: 'video.mov', size: 25 * MiB, reason: 'Too large: 25 MiB of 20' }]);
  const many = admitFiles([file('a', 1), file('b', 1), file('c', 1)], [file('d', 1), file('e', 1)], limits);
  assert.deepEqual(many.accepted.map(f => f.name), ['d']);
  assert.equal(many.rejected[0].reason, 'Too many: 4 files per message');
  const total = admitFiles([file('a', 15 * MiB)], [file('b', 6 * MiB), file('c', 1 * MiB)], limits);
  assert.deepEqual(total.accepted.map(f => f.name), ['c'], 'a later file that still fits is attached');
  assert.equal(total.rejected[0].reason, 'Over the message limit: 21 MiB of 20');
  // A pasted box is one attachment like any other.
  assert.equal(admitFiles([file('a', 1), file('b', 1), file('c', 1), file('d', 1)], [pastedTextFile('x'.repeat(900))], limits).rejected.length, 1);
  assert.equal(admitFiles([], [file('small', 1500)], { maxFiles: 4, maxFileBytes: 1024, maxMessageBytes: 1500 }).rejected[0].reason, 'Too large: 1.5 KiB of 1');
});

test('a sent box says how the agent received it', () => {
  assert.equal(deliveryNote('image'), 'The agent saw it as an image');
  assert.equal(deliveryNote('text'), 'The agent got its text');
  assert.equal(deliveryNote('file'), 'The agent got a link to it, to open as needed');
  assert.equal(deliveryNote(undefined), '', 'an older Runner did not say');
});

test('boxes move, and inline text goes where the caret is', () => {
  assert.deepEqual(moveItem(['a', 'b', 'c'], 0, 2), ['b', 'c', 'a']);
  assert.deepEqual(moveItem(['a', 'b', 'c'], 2, 0), ['c', 'a', 'b']);
  assert.deepEqual(moveItem(['a', 'b'], 0, 5), ['a', 'b']);
  assert.deepEqual(insertText('Look at  please', 'THIS', 8), { message: 'Look at THIS please', caret: 12 });
  assert.deepEqual(insertText('replace me', 'you', 8, 10), { message: 'replace you', caret: 11 });
  assert.deepEqual(insertText('end', '!'), { message: 'end!', caret: 4 });
});
