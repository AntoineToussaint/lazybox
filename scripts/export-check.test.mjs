import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { check, codeOf, linksOf, makeTargets } from './export-check.mjs';

const makefile = `.DEFAULT_GOAL := help
GO_ROOT := services/runner/code
INSTALL_ENV = PREFIX='$(PREFIX)' FOO='a: b'
.PHONY: help deps install
help:
\t@echo help
deps: node_modules/.package-lock.json
install update:
\tnode x
`;

function repository(readme, contributing = '# Contributing\n') {
  const root = mkdtempSync(join(tmpdir(), 'export-check-'));
  mkdirSync(join(root, 'docs'), { recursive: true });
  mkdirSync(join(root, 'scripts'), { recursive: true });
  writeFileSync(join(root, 'Makefile'), makefile);
  writeFileSync(join(root, 'package.json'), JSON.stringify({ scripts: { typecheck: 'tsc', 'test:client': 'node --test' } }));
  writeFileSync(join(root, 'scripts', 'gates.mjs'), '');
  writeFileSync(join(root, 'docs', 'install.md'), '# Install\n\n## Install from source\n\n`make install`\n');
  writeFileSync(join(root, 'docs', 'getting-started.md'), '# Getting started\n');
  writeFileSync(join(root, 'docs', 'testers.md'), '# Testers\n');
  writeFileSync(join(root, 'README.md'), readme);
  writeFileSync(join(root, 'CONTRIBUTING.md'), contributing);
  writeFileSync(join(root, 'SUPPORT.md'), '# Support\n');
  writeFileSync(join(root, 'SECURITY.md'), '# Security\n');
  return root;
}

test('rule names and .PHONY are targets; variables are not', () => {
  const targets = makeTargets(makefile);
  for (const name of ['help', 'deps', 'install', 'update']) assert.ok(targets.has(name), name);
  assert.ok(!targets.has('INSTALL_ENV') && !targets.has("FOO='a"));
});

test('commands are read from code only, links from prose only', () => {
  const text = 'Run `make dev` or see [x](a.md).\n```sh\nmake install   # [not a link](b.md)\n```\n';
  assert.deepEqual(codeOf(text).map(c => c.text.trim()), ['make dev', 'make install']);
  assert.deepEqual(linksOf(text).map(l => l.target), ['a.md']);
});

test('a README whose commands, paths and links all exist passes', () => {
  const root = repository([
    '# Lazybox', '', '```sh', 'git clone https://github.com/AntoineToussaint/lazybox.git', 'make install PREFIX=~/Applications', '```', '',
    '`npm run test:client`, `node scripts/gates.mjs run ui`, `scripts/gates.mjs`, `~/Library/Logs/Lazybox/runner.log`, `bin/lazybox`.',
    '[Install](docs/install.md#install-from-source) · <img src="docs/install.md"> · [web](https://example.com) · [top](#lazybox)', '',
  ].join('\n'));
  assert.deepEqual(check(root), []);
});

test('each missing command, file, link and anchor is named with its line', () => {
  const root = repository([
    '# Lazybox',
    // Joined so the exporter's own `make -C v2` rewrite leaves this test input intact.
    ['`make -C', 'v2 install` and `make instal`'].join(' '),
    '`npm run test:everything` and `node scripts/missing.mjs`',
    '`scripts/nothing.json`',
    '[gone](docs/gone.md) [anchor](docs/install.md#from-source) [out](../LICENSE)',
    '[remote](https://github.com/AntoineToussaint/lazybox/blob/main/RELEASING.md)',
  ].join('\n'), '# Contributing\n\n```sh\nmake chek   # a typo\n```\n');
  const problems = check(root);
  const expected = [
    'README.md:2: `make -C v2`', 'README.md:2: make instal:', 'README.md:3: npm run test:everything', 'README.md:3: node scripts/missing.mjs',
    'README.md:4: scripts/nothing.json', 'README.md:5: docs/gone.md', 'README.md:5: docs/install.md#from-source: no heading', 'README.md:5: ../LICENSE: leaves',
    'README.md:6: https://github.com/AntoineToussaint/lazybox/blob/main/RELEASING.md', 'CONTRIBUTING.md:4: make chek',
  ];
  for (const fragment of expected) assert.ok(problems.some(p => p.includes(fragment)), `${fragment}\n${problems.join('\n')}`);
  assert.equal(problems.length, expected.length, problems.join('\n'));
});
