import { create } from '@bufbuild/protobuf';
import { ReviewCommentSchema } from '@lazybox/protocol';
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { diffLabel, dispositionLabel, freshnessLabel, groupByFile, isLine, matchesFilter, parseDiff, placeComment, reviewExcerpt, reviewMessage, selectionTarget, type DiffLine } from '../packages/surfaces/src/review-model.ts';

const DIFF = `diff --git a/src/handler.go b/src/handler.go
index 1111111..2222222 100644
--- a/src/handler.go
+++ b/src/handler.go
@@ -1,6 +1,7 @@
 package api

-func Handle() {
+func Handle() error {
+\tlog.Print("handle")
 \treturn
 }

@@ -20,3 +21,3 @@ func other() {
 \ta := 1
-\tb := 2
+\tb := 3
 \treturn
`;
const comment = (fields: Record<string, unknown> = {}) => create(ReviewCommentSchema, { id: 'rc-1', member: 'api', path: 'src/handler.go', side: 'new', startLine: 3, endLine: 4, snippet: ['func Handle() error {', '\tlog.Print("handle")'], body: 'Return the error.', head: 'a'.repeat(40), view: 'uncommitted', disposition: 'open', freshness: 'current', currentStartLine: 3, currentEndLine: 4, ...fields });

test('a unified diff numbers each line on the side it exists on', () => {
  const rows = parseDiff(DIFF);
  const lines = rows.filter(isLine) as DiffLine[];
  assert.equal(rows.filter(r => r.kind === 'meta').length, 4);
  assert.equal(rows.filter(r => r.kind === 'hunk').length, 2);
  assert.deepEqual(lines.slice(0, 5).map(l => [l.type, l.oldLine, l.newLine]), [['context', 1, 1], ['context', 2, 2], ['del', 3, undefined], ['add', undefined, 3], ['add', undefined, 4]]);
  const second = lines.filter(l => l.hunk === 2);
  assert.deepEqual(second.map(l => [l.type, l.oldLine, l.newLine]), [['context', 20, 21], ['del', 21, undefined], ['add', undefined, 22], ['context', 22, 23]]);
  // An untracked file is the Runner's "New file:" preview: every line added.
  const preview = parseDiff('New file: notes.md\n\n# Notes\n\nfirst\n');
  assert.deepEqual(preview.filter(isLine).map(l => [l.type, l.newLine, l.text]), [['add', 1, '# Notes'], ['add', 2, ''], ['add', 3, 'first']]);
});

test('a selection anchors to new-side lines, or to removed lines alone, within one hunk', () => {
  const rows = parseDiff(DIFF);
  const index = (predicate: (l: DiffLine) => boolean) => rows.findIndex(r => isLine(r) && predicate(r));
  const del = index(l => l.type === 'del' && l.oldLine === 3), add2 = index(l => l.newLine === 4), ret = index(l => l.newLine === 5);
  assert.deepEqual(selectionTarget(rows, del, del), { side: 'old', startLine: 3, endLine: 3, snippet: ['func Handle() {'], lastRow: del });
  // A range across a removal quotes the new-side lines only, in file order.
  const across = selectionTarget(rows, ret, del)!;
  assert.equal(across.side, 'new'); assert.equal(across.startLine, 3); assert.equal(across.endLine, 5);
  assert.deepEqual(across.snippet, ['func Handle() error {', '\tlog.Print("handle")', '\treturn']);
  // Dragging into the next hunk stops at the end of this one.
  const far = index(l => l.newLine === 23);
  const clamped = selectionTarget(rows, add2, far)!;
  assert.equal(clamped.startLine, 4); assert.equal(clamped.endLine, 7);
  assert.equal(selectionTarget(rows, 0, 0), undefined, 'a header is not a line');
});

test('saved comments are placed by their quoted lines, nearest where they now are', () => {
  const rows = parseDiff(DIFF);
  const placed = placeComment(rows, comment());
  assert.ok(placed !== undefined && isLine(rows[placed]) && (rows[placed] as DiffLine).newLine === 4);
  // Moved in the working tree but shown in a diff whose numbering differs:
  // the text decides, not the number.
  assert.equal(placeComment(rows, comment({ startLine: 40, currentStartLine: 41, freshness: 'moved' })), placed);
  assert.equal(placeComment(rows, comment({ snippet: ['func Handle() error {', '\treturn'] })), undefined, 'lines that are not consecutive in the file are not a match');
  const removed = placeComment(rows, comment({ side: 'old', startLine: 3, endLine: 3, snippet: ['func Handle() {'] }));
  assert.equal((rows[removed!] as DiffLine).oldLine, 3);
  assert.equal(placeComment(rows, comment({ snippet: ['gone()'] })), undefined);
});

test('labels say where a comment was made, whether it still holds and what became of it', () => {
  assert.equal(diffLabel(comment()), 'uncommitted changes on aaaaaaaa');
  assert.equal(diffLabel(comment({ view: 'branch', base: 'main' })), 'branch diff main…aaaaaaaa');
  assert.equal(diffLabel(comment({ view: 'branch', base: 'b'.repeat(40) })), 'branch diff bbbbbbbb…aaaaaaaa');
  assert.equal(freshnessLabel(comment({ freshness: 'moved', currentStartLine: 7, currentEndLine: 8 })), 'Moved to lines 7–8');
  assert.equal(freshnessLabel(comment({ freshness: 'outdated' })), 'Outdated');
  assert.equal(freshnessLabel(comment()), '');
  assert.equal(dispositionLabel(comment({ disposition: 'sent', sentToTitle: 'Fix it' })), 'Sent to “Fix it”');
  assert.equal(dispositionLabel(comment({ disposition: 'resolved', resolvedBy: 'c'.repeat(40) })), 'Resolved by cccccccc');
  assert.ok(matchesFilter(comment(), 'pending') && !matchesFilter(comment({ disposition: 'sent' }), 'pending'));
  assert.ok(matchesFilter(comment({ disposition: 'wont_fix' }), 'settled'));
  const groups = groupByFile([comment({ id: 'b', path: 'z.go', startLine: 1 }), comment({ id: 'c', startLine: 9 }), comment({ id: 'a' })]);
  assert.deepEqual(groups.map(g => [g.file, g.comments.map(c => c.id)]), [['api/src/handler.go', ['a', 'c']], ['api/z.go', ['b']]]);
});

test('a review becomes one message with file:line, the quoted lines and the diff they were read in', () => {
  const text = reviewMessage([
    comment({ id: 'b', path: 'web/app.ts', member: 'web', startLine: 2, endLine: 2, snippet: ['const a = `x`;'], body: 'Use a constant.', view: 'branch', base: 'main' }),
    comment(),
    comment({ id: 'c', freshness: 'moved', startLine: 3, endLine: 4, currentStartLine: 10, currentEndLine: 11, body: 'Also here.' }),
    comment({ id: 'd', freshness: 'outdated', startLine: 30, endLine: 30, snippet: ['old()'], body: 'Was this needed?' }),
    comment({ id: 'e', side: 'old', startLine: 3, endLine: 3, snippet: ['func Handle() {'], body: 'Keep the old name as an alias.' }),
  ]);
  assert.match(text, /^Please address these 5 review comments/);
  assert.match(text, /Comments marked outdated were made on lines that have changed since/);
  assert.match(text, /### 1\. api\/src\/handler\.go:3–4\n\n_uncommitted changes on aaaaaaaa_\n\n```go\nfunc Handle\(\) error \{\n\tlog\.Print\("handle"\)\n```\n\nReturn the error\./);
  assert.match(text, /api\/src\/handler\.go:10–11 \(was lines 3–4 when I commented\)/);
  assert.match(text, /api\/src\/handler\.go — removed line 3 \(in the version before the change\)/);
  assert.match(text, /api\/src\/handler\.go:30\n\n_uncommitted changes on aaaaaaaa · outdated: these lines have changed since_/);
  // Code with backticks is fenced longer, so it cannot close the block.
  assert.match(text, /web\/app\.ts:2\n\n_branch diff main…aaaaaaaa_\n\n```ts\nconst a = `x`;\n```/);
  assert.match(text, /When you are done, say which comments you addressed/);
  assert.match(reviewMessage([comment({ snippet: ['a ``` b'] })]), /````go\na ``` b\n````/);
  assert.match(reviewMessage([comment()]), /^Please address this review comment/);
});

test('a comment excerpt drops Markdown marks, never the characters of a name', () => {
  assert.equal(reviewExcerpt('Raise `CACHE_MAX_ENTRIES` to 500'), 'Raise CACHE_MAX_ENTRIES to 500');
  assert.equal(reviewExcerpt('CACHE_MAX_ENTRIES and snake_case_name stay'), 'CACHE_MAX_ENTRIES and snake_case_name stay');
  assert.equal(reviewExcerpt('This is **really** _not_ *fine*, see __init__.py'), 'This is really not fine, see __init__.py');
  assert.equal(reviewExcerpt('> quoted\n## Heading\n\ntext  with   spaces'), 'quoted Heading text with spaces');
  assert.equal(reviewExcerpt('a*b*c and 2 * 3 * 4'), 'a*b*c and 2 * 3 * 4');
  assert.equal(reviewExcerpt('x'.repeat(200)).length, 140);
});
