import { test } from 'node:test';
import assert from 'node:assert/strict';
import { modeOf, nameFromPrompt, NO_CHOICES, parseTaskChoices, rankRepositories, rememberTask, TASK_MODES, taskBranch, taskName, taskProblem } from '../packages/surfaces/src/new-task-model.ts';

test('a task is named from its first message, at a word, capitalized', () => {
  assert.equal(nameFromPrompt('fix the login redirect loop when the session expires after a deploy'), 'Fix the login redirect loop');
  assert.equal(nameFromPrompt('make the cache faster'), 'Make the cache faster');
  assert.equal(nameFromPrompt('  \n\n  add a response cache.\nThen benchmark it'), 'Add a response cache');
  assert.equal(nameFromPrompt('Refactor'), 'Refactor');
  // The first sentence only: a name (and its branch) never runs into the next one.
  assert.equal(nameFromPrompt('Load-test the cache. Run 200 requests against /products and put p95 in the PR.'), 'Load-test the cache');
  assert.equal(taskBranch(taskName({ prompt: 'Load-test the cache. Run 200 requests.', labels: [] })), 'feature/load-test-the-cache');
  assert.equal(nameFromPrompt('Bump undici to 7.6 now'), 'Bump undici to 7.6 now', 'a dot inside a word is no sentence end');
  assert.equal(nameFromPrompt(''), '');
  assert.equal(nameFromPrompt('   \n  '), '');
  // A single long word is cut to the limit rather than dropped.
  assert.equal(nameFromPrompt('x'.repeat(80)).length, 48);
  for (const prompt of ['Why does `make test` hang on CI and only there?', 'Implement R11: the ⌘N sheet, with tests and screenshots…']) {
    const name = nameFromPrompt(prompt);
    assert.ok(name.length <= 48 && name.length > 0, name);
    assert.ok(!/[\s.,;:!?…-]$/.test(name), `no trailing punctuation: ${name}`);
  }
});

test('the workspace name: yours, else the Runner\'s generated one, else the message, else the repositories', () => {
  assert.equal(taskName({ custom: 'Cache work', prompt: 'add a cache', labels: ['team/api'] }), 'Cache work');
  assert.equal(taskName({ custom: '   ', prompt: 'add a cache', labels: ['team/api'] }), 'Add a cache');
  assert.equal(taskName({ prompt: 'add a cache', labels: ['team/api'], generated: 'gentle-copper-otter' }), 'gentle-copper-otter');
  assert.equal(taskName({ prompt: '', labels: ['team/api'] }), 'api');
  assert.equal(taskName({ prompt: '', labels: ['team/api', 'team/web'] }), 'api + web');
  assert.equal(taskName({ prompt: '', labels: [] }), 'New workspace');
});

test('the branch follows the name until it is edited', () => {
  assert.equal(taskBranch('Add a cache'), 'feature/add-a-cache');
  assert.equal(taskBranch('Add a cache', 'fix/cache'), 'fix/cache');
  // Cleared on purpose: the Runner's default branch.
  assert.equal(taskBranch('Add a cache', ''), '');
});

test('remembered choices are read defensively and keep the recent repositories in order', () => {
  assert.deepEqual(parseTaskChoices(null), NO_CHOICES);
  assert.deepEqual(parseTaskChoices('not json'), NO_CHOICES);
  assert.deepEqual(parseTaskChoices('[1,2]'), NO_CHOICES);
  const parsed = parseTaskChoices(JSON.stringify({ identities: ['github:team/api', 7, 'github:team/api', ''], recent: ['a', 'b'], provider: 'codex', profile: 'p1', mode: 'plan' }));
  assert.deepEqual(parsed, { identities: ['github:team/api'], recent: ['a', 'b'], provider: 'codex', profile: 'p1', mode: 'plan' });
  assert.equal(parseTaskChoices(JSON.stringify({ mode: 'sudo' })).mode, 'default');
  const next = rememberTask({ ...NO_CHOICES, recent: ['b', 'c', 'a'] }, { identities: ['a', 'd'], provider: 'claude', profile: 'x', mode: 'ask' });
  assert.deepEqual(next.recent, ['a', 'd', 'b', 'c']);
  assert.deepEqual(next.identities, ['a', 'd']);
  // At most twelve recent repositories.
  const many = rememberTask({ ...NO_CHOICES, recent: Array.from({ length: 20 }, (_, i) => `r${i}`) }, { identities: ['new'], provider: '', profile: '', mode: 'default' });
  assert.equal(many.recent.length, 12);
  assert.equal(many.recent[0], 'new');
});

test('the picker lists recent repositories first, and fuzzy matches when typing', () => {
  const entries = [
    { identity: 'github:team/web', name: 'team/web' },
    { identity: 'github:team/api', name: 'team/api' },
    { identity: 'local:tools', name: 'tools' },
    { identity: 'github:other/api-gateway', name: 'other/api-gateway' },
  ];
  const names = (rows: { entry: { name: string } }[]) => rows.map(row => row.entry.name);
  const ranked = rankRepositories(entries, { query: '', recent: ['local:tools', 'github:team/api'], chosen: new Set() });
  assert.deepEqual(names(ranked), ['tools', 'team/api', 'other/api-gateway', 'team/web']);
  assert.deepEqual(ranked.map(row => row.recent), [true, true, false, false]);
  // Chosen ones are left out.
  assert.deepEqual(names(rankRepositories(entries, { query: '', recent: [], chosen: new Set(['github:team/api']) })), ['other/api-gateway', 'team/web', 'tools']);
  const typed = rankRepositories(entries, { query: 'api', recent: ['github:other/api-gateway'], chosen: new Set() });
  assert.ok(names(typed).includes('team/api') && names(typed).includes('other/api-gateway'));
  assert.ok(!names(typed).includes('team/web'));
  assert.ok(typed[0].positions.length > 0, 'the matched letters are known');
  assert.deepEqual(rankRepositories(entries, { query: 'zzz', recent: [], chosen: new Set() }), []);
});

test('a task starts only with a repository (or none on purpose), an account and a valid branch', () => {
  const ok = { identities: ['github:team/api'], empty: false, profileId: 'claude', name: 'Add a cache', branch: 'feature/add-a-cache', ready: true };
  assert.equal(taskProblem(ok), undefined);
  assert.equal(taskProblem({ ...ok, identities: [] })?.field, 'repositories');
  assert.equal(taskProblem({ ...ok, identities: [], empty: true, branch: '' }), undefined);
  assert.equal(taskProblem({ ...ok, ready: false })?.field, 'agent');
  assert.equal(taskProblem({ ...ok, profileId: '' })?.field, 'agent');
  assert.equal(taskProblem({ ...ok, name: '  ' })?.field, 'name');
  assert.equal(taskProblem({ ...ok, branch: '' }), undefined, 'the Runner\'s default branch');
  for (const bad of ['feature/a b', 'feature/', '/x', 'a..b', 'x.lock', 'a//b', 'x@{1}']) assert.equal(taskProblem({ ...ok, branch: bad })?.field, 'branch', bad);
});

test('modes map to the Runner\'s permission and workflow', () => {
  assert.deepEqual(TASK_MODES.map(mode => mode.id), ['default', 'ask', 'plan', 'read-only', 'yolo']);
  assert.deepEqual([modeOf('plan').permission, modeOf('plan').workflow], ['ask', 'plan']);
  assert.deepEqual([modeOf('yolo').permission, modeOf('yolo').workflow], ['yolo', 'execute']);
  assert.equal(modeOf('nonsense').id, 'default');
});
