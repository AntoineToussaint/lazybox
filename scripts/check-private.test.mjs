// check-private.mjs on a scratch repository, with a stand-in private name
// so the test does not spell out the real ones.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { checkTree, PRIVATE_NAME_PATTERNS } from './check-private.mjs';

const patterns = [/(?<![A-Za-z])Hushco(?![a-z])/];

test('the real patterns are encoded and still compile', () => {
  assert.ok(PRIVATE_NAME_PATTERNS.length > 0);
  for (const pattern of PRIVATE_NAME_PATTERNS) assert.ok(pattern instanceof RegExp);
});

test('the tree check finds a private name, and a compiled executable', t => {
  const root = mkdtempSync(join(tmpdir(), 'check-private-'));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  const git = args => execFileSync('git', args, { cwd: root, env: { ...process.env, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_NOSYSTEM: '1' } });
  git(['init', '-q']);
  writeFileSync(join(root, 'notes.md'), 'line one\nworked at Hushco\n');
  writeFileSync(join(root, 'lazybox'), Buffer.concat([Buffer.from('cffaedfe', 'hex'), Buffer.alloc(64)]));
  writeFileSync(join(root, 'clean.md'), 'nothing here\n');
  git(['add', '-A']);
  const { files, problems } = checkTree(root, { patterns });
  assert.equal(files, 3);
  assert.equal(problems.length, 2);
  assert.match(problems[0], /^lazybox: a compiled executable/);
  assert.equal(problems[1], 'notes.md:2: names something private');
});
