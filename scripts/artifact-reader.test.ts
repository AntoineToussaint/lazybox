import test from 'node:test';
import assert from 'node:assert/strict';
import { blockDiff, cellTone, extractToc, markdownBlocks, parseCallout, plainText, readingMinutes, repeatsTitle, tocOutline, withoutRepeatedTitle } from '../packages/surfaces/src/artifact-reader.ts';
import { designDocV1, designDocV2, reportDoc, reportTitle } from './fixtures/artifact-sample.mjs';

test('the table of contents lists headings outside code, with unique ids and their source lines', () => {
  const toc = extractToc('# Plan\n\nIntro\n\n## Why `now`\n\n```md\n## Not a heading\n```\n\n### [Risks](http://x) and *mitigations*\n\nSetext\n------\n\n## Why now\n~~~\n# no\n~~~\n## Ünïcode & symbols! ##\n');
  assert.deepEqual(toc.map(e => [e.level, e.text, e.id, e.line]), [
    [1, 'Plan', 'plan', 1],
    [2, 'Why now', 'why-now', 5],
    [3, 'Risks and mitigations', 'risks-and-mitigations', 11],
    [2, 'Setext', 'setext', 13],
    [2, 'Why now', 'why-now-1', 16],
    [2, 'Ünïcode & symbols!', 'unicode-symbols', 20],
  ]);
  // A list item followed by dashes is not a setext heading.
  assert.deepEqual(extractToc('- item\n---\n'), []);
});

test('the outline shows the two top section levels, and only when there are enough sections', () => {
  const outline = tocOutline(extractToc(designDocV1));
  assert.equal(outline[0].text, 'Context');
  assert.ok(outline.some(e => e.text === 'Non-goals' && e.level === 3));
  assert.ok(!outline.some(e => e.level === 1), 'the single h1 is the title');
  assert.deepEqual(tocOutline(extractToc('# A\n## B\n## C')), []);
  assert.deepEqual(tocOutline(extractToc('## A\n## B\n## C')).map(e => e.text), ['A', 'B', 'C']);
  assert.deepEqual(tocOutline(extractToc('## A\n#### deep\n## B\n## C')).map(e => e.text), ['A', 'B', 'C']);
});

test('callouts: GitHub alerts and bold labels, with an optional title', () => {
  assert.deepEqual(parseCallout('[!NOTE]\nRead this.'), { kind: 'note', title: '', rest: 'Read this.' });
  assert.deepEqual(parseCallout('[!caution] Careful'), { kind: 'caution', title: '', rest: 'Careful' });
  assert.deepEqual(parseCallout('**Warning** Phase 4 cannot be undone.'), { kind: 'warning', title: '', rest: 'Phase 4 cannot be undone.' });
  assert.deepEqual(parseCallout('**Decision:** reads first.'), { kind: 'decision', title: '', rest: 'reads first.' });
  assert.deepEqual(parseCallout('**Decision: reads first**\nbecause…'), { kind: 'decision', title: 'reads first', rest: 'because…' });
  assert.deepEqual(parseCallout('__Tip__: use flags'), { kind: 'tip', title: '', rest: 'use flags' });
  assert.equal(parseCallout('**Bold opening** of an ordinary quote'), undefined);
  assert.equal(parseCallout('[!UNKNOWN] x'), undefined);
  assert.equal(parseCallout('Just a quote'), undefined);
});

test('plain text, reading time, the repeated title, status cells', () => {
  assert.equal(plainText('**Bold** `code` [link](u) _em_ ~~old~~ snake_case[^1]'), 'Bold code link em old snake_case');
  assert.equal(readingMinutes('word '.repeat(460)), 2);
  assert.equal(readingMinutes(''), 1);
  assert.equal(readingMinutes('```\n' + 'x '.repeat(690) + '\n```'), 1);
  assert.equal(withoutRepeatedTitle('\n# Datasource *migration*\n\nBody', 'datasource migration'), '\n\n\nBody');
  assert.equal(withoutRepeatedTitle('Datasource migration\n===\nBody', 'Datasource migration'), '\n\nBody');
  assert.equal(withoutRepeatedTitle('# Another title\nBody', 'Datasource migration'), '# Another title\nBody');
  // A nearly repeated title shows once too: "(final)", a subtitle, a changed word.
  assert.equal(withoutRepeatedTitle(reportDoc, reportTitle).split('\n')[0], '');
  assert.ok(repeatsTitle('Auth compared: front door vs host', 'Auth compared (final): front door vs host — who wins what'));
  assert.ok(repeatsTitle('Datasource migration plan', 'Datasource migration plan (draft)'));
  assert.ok(repeatsTitle('The datasource migration plan for billing', 'Datasource migration plan for billing'));
  assert.ok(repeatsTitle('Rollout plan: phase one', 'Rollout plan'));
  // A heading that says something else stays.
  assert.ok(!repeatsTitle('Plan', 'Plan for the rollout'));
  assert.ok(!repeatsTitle('Context', 'Datasource migration'));
  assert.ok(!repeatsTitle('Datasource risks', 'Datasource migration'));
  assert.ok(!repeatsTitle('Rollout plan for billing and invoices and the ledger', 'Rollout plan'));
  assert.equal(cellTone(' In progress '), 'info');
  assert.equal(cellTone('Done'), 'ok');
  assert.equal(cellTone('Phase one'), undefined);
});

test('blocks split on blank lines but keep fences, tables and loose lists whole', () => {
  const blocks = markdownBlocks('# T\nPara one\nstill one\n\n```go\na\n\nb\n```\n\n- a\n\n- b\n  more\n\n| x | y |\n| - | - |\n| 1 | 2 |\n\nEnd');
  assert.deepEqual(blocks, ['# T', 'Para one\nstill one', '```go\na\n\nb\n```', '- a\n\n- b\n  more', '| x | y |\n| - | - |\n| 1 | 2 |', 'End']);
});

test('the rendered diff pairs replaced blocks and keeps the rest', () => {
  const changes = blockDiff(designDocV1, designDocV2);
  const changed = changes.filter(c => c.kind === 'changed');
  assert.equal(changed.length, 4, 'rollout table, milestones, risks table, open questions');
  assert.ok(changed.every(c => c.kind === 'changed' && c.before.length === 1 && c.after.length === 1));
  assert.ok(changes.filter(c => c.kind === 'same').length > 20);
  assert.deepEqual(blockDiff('a\n\nb', 'a\n\nb\n\nc'), [{ kind: 'same', before: 'a', after: 'a' }, { kind: 'same', before: 'b', after: 'b' }, { kind: 'changed', before: [], after: ['c'] }]);
  assert.deepEqual(blockDiff('x\\ny', 'x\\ny'), [{ kind: 'same', before: 'x\\ny', after: 'x\\ny' }]);
});
