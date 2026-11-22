import test from 'node:test';
import assert from 'node:assert/strict';
import { roadmapMissionPlan, agentReference, ago, artifactChip, artifactPaletteEntries, authorShort, diffStats, fromConversation, groupByWorkspace, guessKind, hunks, lineDiff, linkedMarkdownFiles, mostlyNamesFile, slug, suggestedPath, titleFromMarkdown, trackableFile, updatedLine, updatedSinceSeen, type FoundArtifact } from '../packages/surfaces/src/artifact-model.ts';

// Artifacts as the Runner lists them, for the finding helpers.
const made = (id: string, collectionId: string, at: number, extra: Record<string, unknown> = {}) => ({ id, collectionId, title: `Title ${id}`, kind: 'plan', trackedPath: '', originSessionId: '', originConversationId: '', updatedAtUnixMillis: BigInt(at), versions: [{ number: 1 }], ...extra }) as never;
const found = (artifact: never, workspace: string): FoundArtifact => ({ artifact, collectionId: (artifact as { collectionId: string }).collectionId, workspace });

test('an artifact is from a conversation when it began there or any version came from it', () => {
  const here = { sessionId: 's1', conversationId: 'c1' };
  assert.equal(fromConversation(made('a', 'w', 1, { originConversationId: 'c1' }), here), true, 'taken from it');
  assert.equal(fromConversation(made('b', 'w', 1, { originSessionId: 's1' }), here), true, 'an older execution of it, by session');
  assert.equal(fromConversation(made('c', 'w', 1, { originConversationId: 'c9', versions: [{ number: 1 }, { number: 2, source: { kind: 'message', sessionId: 's9', conversationId: 'c1' } }] }), here), true, 'updated from one of its messages');
  assert.equal(fromConversation(made('d', 'w', 1, { versions: [{ number: 1, author: { kind: 'agent', sessionId: 's1' }, source: { kind: 'agent' } }] }), here), true, 'published by its agent');
  assert.equal(fromConversation(made('e', 'w', 1, { originConversationId: 'c9', versions: [{ number: 1, source: { kind: 'file', path: 'PLAN.md' } }] }), here), false, 'a file tracked elsewhere');
  assert.equal(fromConversation(made('f', 'w', 1), { sessionId: '', conversationId: '' }), false, 'no conversation matches nothing');
});

test('all workspaces: grouped by workspace, the one changed last first, each newest first', () => {
  const groups = groupByWorkspace([found(made('a1', 'alpha', 10), 'Alpha'), found(made('b1', 'beta', 30), 'Beta'), found(made('a2', 'alpha', 20), 'Alpha'), found(made('b2', 'beta', 5), 'Beta')]);
  assert.deepEqual(groups.map(g => [g.workspace, g.items.map(i => (i.artifact as { id: string }).id)]), [['Beta', ['b1', 'b2']], ['Alpha', ['a2', 'a1']]]);
  assert.deepEqual(groupByWorkspace([]), []);
});

test('the palette lists every workspace\'s artifacts, newest first, with the workspace in the detail', () => {
  const entries = artifactPaletteEntries([
    found(made('A1', 'alpha', 10, { kind: 'report', versions: [{ number: 1 }, { number: 3 }] }), 'Alpha feature'),
    found(made('B1', 'beta', 20, { trackedPath: 'docs/PLAN.md' }), 'Beta release'),
  ], 'alpha');
  assert.deepEqual(entries.map(e => [e.key, e.title, e.detail, e.here]), [
    ['artifact:beta:B1', 'Title B1', 'Plan · v1 · Beta release', false],
    ['artifact:alpha:A1', 'Title A1', 'Report · v3 · This workspace', true],
  ]);
  // The workspace's name and a tracked file find an artifact too.
  assert.match(entries[0].extra, /Beta release/);
  assert.match(entries[0].extra, /docs\/PLAN\.md/);
  assert.equal(entries[0].icon, 'checklist');
  assert.equal(artifactPaletteEntries([found(made('X', 'w', 1, { title: '' }), 'W')], 'w')[0].title, 'Untitled artifact');
});

test('the header chip counts this conversation\'s artifacts first, the workspace\'s otherwise, and is absent with none', () => {
  const here = { sessionId: 's1', conversationId: 'c1' };
  assert.equal(artifactChip([], here), undefined);
  const mine = made('a', 'w', 1, { originConversationId: 'c1' }), theirs = made('b', 'w', 1, { originConversationId: 'c2' });
  assert.deepEqual(artifactChip([mine, theirs, theirs], here), { count: 1, mine: true, label: '1 artifact', title: '1 artifact from this conversation · 3 in this workspace' });
  assert.deepEqual(artifactChip([theirs, theirs], here), { count: 2, mine: false, label: '2 artifacts', title: '2 artifacts in this workspace' });
  assert.equal(artifactChip([mine], here)?.title, '1 artifact from this conversation');
});

test('the title is the first heading outside code, else the first prose line', () => {
  assert.equal(titleFromMarkdown('Some intro\n\n# Migration **plan**\n\n## Steps'), 'Migration plan');
  assert.equal(titleFromMarkdown('```md\n# Not this\n```\n## Real [title](http://x)'), 'Real title');
  assert.equal(titleFromMarkdown('Datasource rollout\n===\nbody'), 'Datasource rollout');
  assert.equal(titleFromMarkdown('- Here is the plan:\n- step'), 'Here is the plan');
  assert.equal(titleFromMarkdown('   \n'), 'Untitled');
  assert.equal(titleFromMarkdown('# ' + 'x'.repeat(200)).length, 120);
});

test('a kind is suggested from the title and the shape of the text', () => {
  assert.equal(guessKind('Migration plan'), 'plan');
  assert.equal(guessKind('Security review findings'), 'report');
  assert.equal(guessKind('API design spec'), 'spec');
  assert.equal(guessKind('Meeting notes'), 'notes');
  assert.equal(guessKind('Datasource', '1. Phase one\n2. Phase two'), 'plan');
  assert.equal(guessKind('Hello'), 'other');
});

test('a line diff keeps unchanged lines and numbers both sides', () => {
  const lines = lineDiff('a\nb\nc\nd', 'a\nB\nc\nd\ne');
  assert.deepEqual(lines.map(l => `${l.kind[0]}${l.text}`), ['sa', 'rb', 'aB', 'sc', 'sd', 'ae']);
  assert.deepEqual(diffStats(lines), { added: 2, removed: 1 });
  assert.deepEqual(lines.map(l => [l.before, l.after]), [[1, 1], [2, undefined], [undefined, 2], [3, 3], [4, 4], [undefined, 5]]);
  assert.deepEqual(lineDiff('', 'x').map(l => l.kind), ['add']);
  assert.deepEqual(lineDiff('x', '').map(l => l.kind), ['remove']);
  assert.deepEqual(lineDiff('same', 'same').map(l => l.kind), ['same']);
});

test('a line diff is minimal on interleaved edits and survives its step limit', () => {
  const before = Array.from({ length: 50 }, (_, i) => `line ${i}`);
  const after = before.map((line, i) => i % 10 === 0 ? line + ' changed' : line);
  const lines = lineDiff(before.join('\n'), after.join('\n'));
  assert.deepEqual(diffStats(lines), { added: 5, removed: 5 });
  // Rebuilding both sides from the diff gives the inputs back.
  assert.equal(lines.filter(l => l.kind !== 'add').map(l => l.text).join('\n'), before.join('\n'));
  assert.equal(lines.filter(l => l.kind !== 'remove').map(l => l.text).join('\n'), after.join('\n'));
  const fallback = lineDiff('a\nb\nc', 'x\ny\nz', 1);
  assert.equal(fallback.filter(l => l.kind !== 'add').map(l => l.text).join('\n'), 'a\nb\nc');
  assert.equal(fallback.filter(l => l.kind !== 'remove').map(l => l.text).join('\n'), 'x\ny\nz');
});

test('hunks fold long unchanged runs', () => {
  const before = Array.from({ length: 30 }, (_, i) => String(i)).join('\n');
  const after = before.replace(/^15$/m, 'fifteen');
  const folded = hunks(lineDiff(before, after), 2);
  assert.equal(folded.length, 2);
  assert.equal(folded[0].hiddenBefore, 13);
  assert.deepEqual(folded[0].lines.map(l => l.text), ['13', '14', '15', 'fifteen', '16', '17']);
  assert.equal(folded[1].hiddenBefore, 12);
});

test('times, slugs and paths read naturally', () => {
  const now = Date.UTC(2026, 10, 16, 12);
  assert.equal(ago(now - 10_000, now), 'just now');
  assert.equal(ago(now - 120_000, now), '2 min ago');
  assert.equal(ago(BigInt(now - 3 * 3600_000), now), '3 h ago');
  assert.equal(ago(now - 86400_000, now), '1 day ago');
  assert.equal(ago(0, now), '');
  assert.equal(slug('Datasource: Migration Plan (v2)!'), 'datasource-migration-plan-v2');
  assert.equal(slug('§§§'), 'artifact');
  assert.equal(suggestedPath('api', 'Rollout plan'), 'api/docs/rollout-plan.md');
});

test('the list line says who changed it last', () => {
  const now = Date.UTC(2026, 10, 16, 12);
  const artifact = { versions: [{ number: 1 }, { number: 2, author: { kind: 'file' }, source: { kind: 'file' } }], updatedAtUnixMillis: BigInt(now - 120_000) } as never;
  assert.equal(updatedLine(artifact, now), 'v2 · file changed on disk · 2 min ago');
});

test('send to agent gives a fetchable reference and an outline, not the whole text', () => {
  const content = '# Plan\n\nIntro\n\n## Step one\n```\n# not a heading\n```\n## Step two\n' + 'filler\n'.repeat(500);
  const text = agentReference({ id: 'ART1', title: 'Plan', kind: 'plan', versions: [{ number: 3 }] as never }, content);
  assert.match(text, /^Artifact “Plan” \(plan, v3\)\. Read it in full with `lazybox artifact get ART1 --version 3`\./);
  assert.match(text, /Outline:\n- Plan\n {2}- Step one\n {2}- Step two\n/);
  assert.doesNotMatch(text, /not a heading|filler/);
});

test('the list says who made the latest version, and marks one the person has not seen', () => {
  const agent = { number: 2, createdAtUnixMillis: 2000n, author: { kind: 'agent', sessionId: 's' }, source: { kind: 'agent' } } as never;
  const person = { number: 3, createdAtUnixMillis: 3000n, author: { kind: 'person' }, source: { kind: 'edit' } } as never;
  const file = { number: 4, createdAtUnixMillis: 4000n, author: { kind: 'file' }, source: { kind: 'file', path: 'PLAN.md' } } as never;
  assert.equal(authorShort(agent), 'by an agent');
  assert.equal(authorShort(person), 'by you');
  assert.equal(authorShort(file), 'from the file');
  assert.equal(authorShort({ number: 1, source: { kind: 'message' } } as never), 'from chat');
  const of = (version: never) => ({ updatedAtUnixMillis: 1n, versions: [version] });
  assert.equal(updatedSinceSeen(of(agent), undefined), true, 'never opened');
  assert.equal(updatedSinceSeen(of(agent), '2000'), false, 'seen this version');
  assert.equal(updatedSinceSeen(of(agent), '1000'), true, 'a newer version since');
  assert.equal(updatedSinceSeen(of(file), '1000'), true);
  assert.equal(updatedSinceSeen(of(person), undefined), false, 'your own version is never news');
});

test('a message links the Markdown files its links and inline code name, each once, in order', () => {
  assert.deepEqual(linkedMarkdownFiles('Wrote `docs/PLAN.md`.'), ['docs/PLAN.md']);
  assert.deepEqual(linkedMarkdownFiles('See [the plan](docs/PLAN.md:12) and `README.md`, then [it again](docs/PLAN.md#L3).'), ['docs/PLAN.md', 'README.md']);
  assert.deepEqual(linkedMarkdownFiles('`/Users/me/repo/notes/design.markdown:4` beside ``api/SPEC.mdx``'), ['/Users/me/repo/notes/design.markdown', 'api/SPEC.mdx']);
  // Not Markdown, not a file, an image, a web link, or code in a fence: none of these.
  assert.deepEqual(linkedMarkdownFiles('Changed `src/main.go` and `rate.limit`; ![diagram](docs/flow.md) [site](https://example.com/a.md)\n```\ncat docs/HIDDEN.md `x.md`\n```'), []);
});

test('a short message that names a file prefers the file; a long reply or a code block does not', () => {
  assert.equal(mostlyNamesFile('Wrote `docs/PLAN.md`.'), true);
  assert.equal(mostlyNamesFile('Updated the rollout plan in [PLAN.md](docs/PLAN.md) with the new phases.'), true);
  assert.equal(mostlyNamesFile('Wrote docs/PLAN.md'), false);
  assert.equal(mostlyNamesFile('Here is the plan in full. ' + 'Words of prose. '.repeat(12) + 'It is also in `docs/PLAN.md`.'), false);
  assert.equal(mostlyNamesFile('Wrote `docs/PLAN.md`:\n```md\n# Plan\n```'), false);
  assert.equal(trackableFile('docs/PLAN.MD'), true);
  assert.equal(trackableFile('src/main.go'), false);
});

test('mission export carries the definition and pinned evidence, rejects broken dependency graphs', () => {
  const milestone = (key: string, deps = '') => [
    '## Ship ' + key, '- Milestone: ' + key, '- Deliverable: Package', '- Depends on: ' + deps,
    '- Definition of done: Build passes', '- Verification evidence: CI receipt',
  ].join('\n');
  const exportPlan = (body: string) => roadmapMissionPlan(body, 'Ship', 'art', 3);
  const plan = exportPlan(milestone('A') + '\n' + milestone('B', 'A'));
  assert.deepEqual(plan.tasks[1].after, ['A']);
  assert.match(plan.tasks[1].brief, /Build passes[\s\S]*CI receipt[\s\S]*art v3/);
  assert.throws(() => exportPlan(milestone('B', 'A')), /Unknown milestone/);
  assert.throws(() => exportPlan(milestone('A', 'A')), /cycle/);
  assert.throws(() => exportPlan(milestone('A') + '\n' + milestone('A')), /unique/);
  assert.throws(() => exportPlan(milestone('A').replace('Build passes', '')), /definition of done/);
});
