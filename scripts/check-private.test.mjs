// check-private.mjs on a scratch repository, with a stand-in private name
// so the test does not spell out the real ones.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { checkCommits, checkTree, PRIVATE_NAME_PATTERNS } from './check-private.mjs';

const patterns = [/(?<![A-Za-z])Hushco(?![a-z])/];

function scratch() {
  const root = mkdtempSync(join(tmpdir(), 'check-private-'));
  const git = (args, env = {}) => execFileSync('git', args, { cwd: root, encoding: 'utf8', env: { ...process.env, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_NOSYSTEM: '1', ...env } }).trim();
  git(['init', '-q', '-b', 'main']);
  const commit = (message, { author = 'dev@example.com', committer = 'dev@example.com' } = {}) => {
    writeFileSync(join(root, 'file.txt'), message);
    git(['add', '-A']);
    git(['commit', '-q', '-m', message], { GIT_AUTHOR_NAME: 'Dev', GIT_AUTHOR_EMAIL: author, GIT_COMMITTER_NAME: 'Dev', GIT_COMMITTER_EMAIL: committer });
    return git(['rev-parse', 'HEAD']);
  };
  return { root, git, commit, done: () => rmSync(root, { recursive: true, force: true }) };
}

test('the real patterns are encoded and still compile', () => {
  assert.ok(PRIVATE_NAME_PATTERNS.length > 0);
  for (const pattern of PRIVATE_NAME_PATTERNS) assert.ok(pattern instanceof RegExp);
});

test('only the commits of the range are checked: an older address in history is not', t => {
  const repo = scratch();
  t.after(repo.done);
  const base = repo.commit('old', { author: 'me@hushco.dev' });
  repo.commit('clean');
  assert.deepEqual(checkCommits(repo.root, `${base}..HEAD`, { patterns: [/hushco/] }), { commits: 1, problems: [] });
});

test('a private author, committer or co-author address in the range fails', t => {
  const repo = scratch();
  t.after(repo.done);
  const base = repo.commit('base');
  const author = repo.commit('by author', { author: 'me@hushco.dev' });
  const committer = repo.commit('by committer', { committer: 'me@hushco.dev' });
  const coauthor = repo.commit('by co-author\n\nCo-authored-by: Me <me@hushco.dev>');
  const { commits, problems } = checkCommits(repo.root, `${base}..HEAD`, { patterns: [/hushco/] });
  assert.equal(commits, 3);
  assert.deepEqual(problems.map(problem => problem.split(':')[0] + ' ' + /its (\S+)/.exec(problem)[1]), [
    `commit ${coauthor.slice(0, 12)} Co-authored-by`,
    `commit ${committer.slice(0, 12)} committer`,
    `commit ${author.slice(0, 12)} author`,
  ]);
});

test('the tree check finds a private name, and a compiled executable', t => {
  const repo = scratch();
  t.after(repo.done);
  writeFileSync(join(repo.root, 'notes.md'), 'line one\nworked at Hushco\n');
  writeFileSync(join(repo.root, 'lazybox'), Buffer.concat([Buffer.from('cffaedfe', 'hex'), Buffer.alloc(64)]));
  repo.git(['add', '-A']);
  const { problems } = checkTree(repo.root, { patterns });
  assert.equal(problems.length, 2);
  assert.match(problems[0], /^lazybox: a compiled executable/);
  assert.equal(problems[1], 'notes.md:2: names something private');
});
