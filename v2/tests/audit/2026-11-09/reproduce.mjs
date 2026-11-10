// Negative acceptance tests: exit 1 means the audited defects still reproduce.
// Copies the candidate; never mutates running source or the staged snapshot.
import { mkdtemp, readFile, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';
import { copySource, digestTree } from '../../../scripts/environments.mjs';

const directory = fileURLToPath(new URL('.', import.meta.url));
const root = resolve(directory, '../../..');
const candidate = resolve(process.argv[2] || join(root, '.cache/implementation/coordination-2026-11-03/source'));
const digest = await digestTree(candidate);
if (!process.argv[2] && digest !== '998d06e2effb4b98902b39b856639bb6cff41e4e14be5f889ab82ed12e84cc18') {
  throw new Error('The staged snapshot changed. Pass its path explicitly to audit a new candidate.');
}
console.log(`Candidate digest: ${digest}`);
const temporary = await mkdtemp(join(tmpdir(), 'lazybox-audit-reproduction-'));
try {
  const source = join(temporary, 'source');
  await copySource(candidate, source);
  for (const [area, name] of [['runner', 'dependency'], ['transport', 'authority']]) {
    const filename = `audit_${name}_test.go`;
    await writeFile(join(source, 'services/runner/code/internal', area, filename), await readFile(join(directory, filename + '.txt')));
  }
  const result = spawnSync('go', ['test', '-count=1', '-timeout', '90s', './internal/runner', './internal/transport', '-run', '^TestAudit'], {
    cwd: join(source, 'services/runner/code'),
    env: { ...process.env, GOMAXPROCS: '1', GOFLAGS: '-p=1' },
    stdio: 'inherit',
  });
  if (result.error) throw result.error;
  process.exitCode = result.status ?? 1;
} finally {
  await rm(temporary, { recursive: true, force: true });
}
