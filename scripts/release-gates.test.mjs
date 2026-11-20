import { test } from 'node:test';
import { spawnSync } from 'node:child_process';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, readFile, writeFile, rm } from 'node:fs/promises';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';
import { loadGateCatalog, selectGates, shardGates, runGateCatalog, validateReleaseReceipt, executeGate, summarizeGateReport, changedFiles } from './release-gates.mjs';
import { digestTree } from './environments.mjs';

async function fixture(t) {
  const root = await mkdtemp(join(tmpdir(), 'lazybox-gates-'));
  t.after(() => rm(root, { recursive: true, force: true }));
  await mkdir(join(root, 'scripts'));
  const policy = { version: 1, policyFiles: ['Makefile'], gates: [
    { id: 'one', scope: 'core', script: 'test:one' },
    { id: 'two', scope: 'ui', script: 'test:two' },
  ], aliases: [{ script: 'test:alias', target: 'test:two' }], external: [{ script: 'test:live', reason: 'Needs a selected live Runner' }] };
  const pkg = { scripts: { 'test:one': 'node one.mjs', 'test:two': 'node two.mjs', 'test:alias': 'node two.mjs', 'test:live': 'node live.mjs' } };
  const save = async () => {
    await writeFile(join(root, 'scripts/release-gates.json'), JSON.stringify(policy));
    await writeFile(join(root, 'package.json'), JSON.stringify(pkg));
  };
  await save();
  await writeFile(join(root, 'Makefile'), 'check:\n\ttrue\n');
  return { root, policy, pkg, save };
}
const snapshot = async () => 'source-digest';
const pass = async () => ({ status: 'passed', exitCode: 0 });

test('current catalog admits every npm test and includes previously omitted continuity gates', async () => {
  const catalog = await loadGateCatalog(fileURLToPath(new URL('..', import.meta.url)));
  for (const id of ['resume-continuity', 'chat-scroll', 'sidebar-polish', 'navigation-snippets', 'activity-review', 'activity-presentation', 'startup']) assert.ok(catalog.gates.some(gate => gate.id === id), id);
  assert.equal(catalog.external[0].script, 'test:browser');
  assert.equal(selectGates(catalog, 'release').length, catalog.gates.length);
  assert.ok(selectGates(catalog, 'ui').every(gate => gate.scope === 'ui'));
  assert.throws(() => selectGates(catalog, 'quick'), /Choose/);
});

test('unclassified, duplicate, changed aliases and unexplained live exclusions fail admission', async t => {
  const f = await fixture(t);
  f.pkg.scripts['test:new'] = 'node new.mjs'; await f.save();
  await assert.rejects(loadGateCatalog(f.root), /absent/);
  delete f.pkg.scripts['test:new']; f.pkg.scripts['test:alias'] = 'node different.mjs'; await f.save();
  await assert.rejects(loadGateCatalog(f.root), /Invalid test alias/);
  f.pkg.scripts['test:alias'] = f.pkg.scripts['test:two']; f.policy.external[0].reason = ''; await f.save();
  await assert.rejects(loadGateCatalog(f.root), /explicit reason/);
  f.policy.external[0].reason = 'Live'; f.policy.gates.push({ ...f.policy.gates[0] }); await f.save();
  await assert.rejects(loadGateCatalog(f.root), /duplicate gate/);
});

test('script body and gate-policy changes invalidate existing passing evidence', async t => {
  const f = await fixture(t);
  const report = await runGateCatalog(f.root, 'release', { snapshot, execute: pass });
  validateReleaseReceipt(report, await loadGateCatalog(f.root), await snapshot());
  f.pkg.scripts['test:one'] = 'node changed.mjs'; await f.save();
  const changed = await loadGateCatalog(f.root);
  assert.throws(() => validateReleaseReceipt(report, changed, 'source-digest'), /current gate catalog/);
  f.pkg.scripts['test:one'] = 'node one.mjs'; await f.save();
  await writeFile(join(f.root, 'Makefile'), 'check:\n\tfalse\n');
  const changedMake = await loadGateCatalog(f.root);
  assert.throws(() => validateReleaseReceipt(report, changedMake, 'source-digest'), /current gate catalog/);
});

test('failures are recorded while later gates still run; incomplete or forged success cannot promote', async t => {
  const f = await fixture(t), seen = [];
  const report = await runGateCatalog(f.root, 'release', { snapshot, execute: async gate => {
    seen.push(gate.id); return { status: gate.id === 'one' ? 'failed' : 'passed', exitCode: gate.id === 'one' ? 1 : 0 };
  } });
  assert.deepEqual(seen, ['one', 'two']); assert.equal(report.status, 'failed');
  const catalog = await loadGateCatalog(f.root);
  assert.throws(() => validateReleaseReceipt(report, catalog, 'source-digest'), /passing receipt/);
  report.status = 'passed';
  assert.throws(() => validateReleaseReceipt(report, catalog, 'source-digest'), /one/);
  const valid = await runGateCatalog(f.root, 'release', { snapshot, execute: pass });
  for (const mutate of [r => r.results.pop(), r => r.results.reverse(), r => r.profile = 'ui', r => r.results[0].args = ['run', 'other'], r => r.results[0].exitCode = 1]) {
    const copy = structuredClone(valid); mutate(copy);
    assert.throws(() => validateReleaseReceipt(copy, catalog, 'source-digest'));
  }
});

test('source mutation and cancellation never produce a release receipt', async t => {
  const f = await fixture(t); let calls = 0;
  const changed = await runGateCatalog(f.root, 'release', { snapshot: async () => ++calls === 1 ? 'before' : 'after', execute: pass });
  assert.equal(changed.status, 'failed');
  const controller = new AbortController(); calls = 0;
  const cancelled = await runGateCatalog(f.root, 'release', { snapshot, signal: controller.signal, execute: async () => { calls++; controller.abort(); return pass(); } });
  assert.equal(calls, 1); assert.equal(cancelled.results[1].status, 'not-run'); assert.equal(cancelled.status, 'failed');
  const partial = await runGateCatalog(f.root, 'ui', { snapshot, execute: pass });
  assert.equal(partial.status, 'passed');
  const catalog = await loadGateCatalog(f.root);
  assert.throws(() => validateReleaseReceipt(partial, catalog, 'source-digest'), /complete passing/);
});

test('executor uses argv, records failures, and cancels its owned process group on deadline', async t => {
  const f = await fixture(t), options = { root: f.root };
  const base = { command: process.execPath, timeoutMs: 5000 };
  assert.equal((await executeGate({ ...base, args: ['-e', 'process.exit(0)'] }, options)).status, 'passed');
  assert.equal((await executeGate({ ...base, args: ['-e', 'process.exit(7)'] }, options)).exitCode, 7);
  assert.equal((await executeGate({ ...base, command: join(f.root, 'missing'), args: [] }, options)).status, 'failed');
  const controller = new AbortController(); controller.abort();
  assert.equal((await executeGate({ ...base, args: ['-e', 'process.exit(99)'] }, { ...options, signal: controller.signal })).status, 'cancelled');
  const pidFile = join(f.root, 'descendant.pid');
  const program = `const {spawn}=require('node:child_process'); const {writeFileSync}=require('node:fs'); const child=spawn(process.execPath,['-e',"process.on('SIGTERM',()=>{});setInterval(()=>{},1000)"],{stdio:'ignore'});writeFileSync(${JSON.stringify(pidFile)},String(child.pid));setInterval(()=>{},1000);`;
  const result = await executeGate({ ...base, args: ['-e', program], timeoutMs: 500 }, options);
  assert.equal(result.status, 'timed-out');
  const pid = Number(await readFile(pidFile, 'utf8'));
  await new Promise(resolve => setTimeout(resolve, 100));
  assert.throws(() => process.kill(pid, 0), { code: 'ESRCH' }, 'timed out gate must not leave its child behind');
});

test('deterministic gates cannot inherit live execution or different source/watcher overrides', async t => {
  const f = await fixture(t);
  const unsafe = { LAZYBOX_V2_REAL: '1', LAZYBOX_V2_ENDPOINT: '/do-not-open', TEST_SOURCE: '/other-source', TEST_WEBKIT: '1', REPRO_OLD_BLUR: '1', CHOKIDAR_USEPOLLING: '1' };
  const code = `const keys=${JSON.stringify(Object.keys(unsafe))};process.exit(keys.some(key=>process.env[key]!==undefined)?9:0);`;
  const result = await executeGate({ command: process.execPath, args: ['-e', code], timeoutMs: 5000 }, { root: f.root, env: { ...process.env, ...unsafe } });
  assert.equal(result.status, 'passed');
});

test('the shards command selects only the named UI gates', () => {
  const script = fileURLToPath(new URL('./release-gates.mjs', import.meta.url));
  const shards = (...args) => spawnSync(process.execPath, [script, 'shards', 'ui', '6', ...args], { encoding: 'utf8' });
  const selected = shards('--only', 'workspace,voice');
  assert.equal(selected.status, 0, selected.stderr);
  assert.deepEqual(JSON.parse(selected.stdout).include.map(shard => shard.gates), ['workspace', 'voice']);
  const invalid = shards('--only', 'workspace,missing');
  assert.equal(invalid.status, 1);
  assert.match(invalid.stderr, /outside the ui profile.*missing/);
});

test('CI shards cover every gate of a profile exactly once and run only named gates', async t => {
  const catalog = await loadGateCatalog(fileURLToPath(new URL('..', import.meta.url)));
  const ui = selectGates(catalog, 'ui').map(gate => gate.id);
  const shards = shardGates(selectGates(catalog, 'ui'), 3);
  assert.equal(shards.length, 3);
  assert.deepEqual(shards.flat().sort(), [...ui].sort());
  assert.ok(shards.every(shard => shard.length >= Math.floor(ui.length / 3)));
  assert.equal(shardGates(selectGates(catalog, 'native'), 4).length, 1, 'never an empty shard');
  assert.throws(() => shardGates(ui, 0), /positive/);
  assert.deepEqual(selectGates(catalog, 'ui', [ui[1], ui[0]]).map(gate => gate.id), [ui[0], ui[1]], 'catalog order');
  assert.throws(() => selectGates(catalog, 'ui', ['core']), /outside the ui profile/);
  assert.throws(() => selectGates(catalog, 'ui', ['no-such-gate']), /no-such-gate/);
  assert.throws(() => selectGates(catalog, 'ui', [ui[0], ui[0]]), /repeated/);
  assert.throws(() => selectGates(catalog, 'ui', []), /outside/);
  const f = await fixture(t), seen = [];
  const subset = await runGateCatalog(f.root, 'release', { snapshot, only: ['two'], execute: async gate => { seen.push(gate.id); return pass(); } });
  assert.deepEqual(seen, ['two']); assert.deepEqual(subset.only, ['two']); assert.equal(subset.status, 'passed');
  const fixtureCatalog = await loadGateCatalog(f.root);
  assert.throws(() => validateReleaseReceipt(subset, fixtureCatalog, 'source-digest'), /complete passing/, 'a shard can never promote');
});

test('CI retries a failed UI gate once and reports it flaky; staging never retries and a flaky gate never promotes', async t => {
  const f = await fixture(t);
  f.policy.gates.push({ id: 'three', scope: 'ui', script: 'test:three' }, { id: 'four', scope: 'ui', script: 'test:four' });
  f.pkg.scripts['test:three'] = 'node three.mjs'; f.pkg.scripts['test:four'] = 'node four.mjs'; await f.save();
  // two: fails, then passes (a flake). three: fails twice. four: runs out of time (never retried).
  const runs = {}, outcome = { two: ['failed', 'passed'], three: ['failed', 'failed'], four: ['timed-out'] };
  const execute = async gate => { const n = runs[gate.id] = (runs[gate.id] || 0) + 1, status = outcome[gate.id]?.[n - 1] || 'passed'; return { status, exitCode: status === 'passed' ? 0 : status === 'failed' ? 1 : null }; };
  const report = await runGateCatalog(f.root, 'ui', { snapshot, execute, retryFlaky: true, only: ['two', 'three', 'four'] });
  assert.deepEqual(runs, { two: 2, three: 2, four: 1 });
  const [two, three, four] = report.results;
  assert.equal(two.status, 'flaky'); assert.equal(two.exitCode, 0);
  assert.deepEqual(two.attempts.map(a => a.status), ['failed', 'passed']);
  assert.equal(three.status, 'failed'); assert.equal(three.exitCode, 1); assert.equal(three.attempts.length, 2);
  assert.equal(four.status, 'timed-out'); assert.equal(four.attempts, undefined);
  assert.equal(report.status, 'failed'); assert.equal(report.retryFlaky, true);
  // Only the flake: the run passes, and says so.
  for (const key of Object.keys(runs)) delete runs[key];
  const flaky = await runGateCatalog(f.root, 'ui', { snapshot, execute, retryFlaky: true, only: ['two'] });
  assert.equal(flaky.status, 'passed'); assert.equal(flaky.results[0].status, 'flaky');
  const summary = summarizeGateReport(flaky);
  assert.match(summary, /1 flaky \(passed on retry\)/); assert.match(summary, /`two` \| flaky \(passed on retry\) \| failed \(\d+ s\), passed \(\d+ s\)/);
  assert.match(summarizeGateReport(report), /`three` \| failed/);
  // Without the option a failure is a failure, and staging's release profile refuses it outright.
  for (const key of Object.keys(runs)) delete runs[key];
  const strict = await runGateCatalog(f.root, 'ui', { snapshot, execute, only: ['two'] });
  assert.deepEqual(runs, { two: 1 }); assert.equal(strict.status, 'failed'); assert.equal(strict.results[0].attempts, undefined);
  await assert.rejects(runGateCatalog(f.root, 'release', { snapshot, execute: pass, retryFlaky: true }), /clean first pass/);
  // A flaky result, or a retrying run, can never stand in for a release receipt.
  const catalog = await loadGateCatalog(f.root);
  const release = await runGateCatalog(f.root, 'release', { snapshot, execute: pass });
  validateReleaseReceipt(release, catalog, 'source-digest');
  const forged = structuredClone(release); forged.results[1] = { ...forged.results[1], status: 'flaky' };
  assert.throws(() => validateReleaseReceipt(forged, catalog, 'source-digest'), /two/);
  const retried = structuredClone(release); retried.retryFlaky = true;
  assert.throws(() => validateReleaseReceipt(retried, catalog, 'source-digest'), /complete passing/);
});

test('a run the source changed under names the files that changed', async t => {
  const root = await mkdtemp(join(tmpdir(), 'lazybox-gates-'));
  t.after(() => rm(root, { recursive: true, force: true }));
  await mkdir(join(root, 'docs')); await mkdir(join(root, '.cache'));
  await writeFile(join(root, 'kept.md'), 'same'); await writeFile(join(root, 'docs/shot.png'), 'old');
  const before = new Map(), after = new Map();
  const first = await digestTree(root, undefined, before);
  await writeFile(join(root, 'docs/shot.png'), 'new'); await writeFile(join(root, 'added.ts'), 'x'); await writeFile(join(root, '.cache/ignored.png'), 'x');
  const second = await digestTree(root, undefined, after);
  assert.notEqual(first, second);
  assert.deepEqual(changedFiles(before, after), ['added.ts', 'docs/shot.png']);
  const report = { profile: 'ui', status: 'failed', results: [{ id: 'one', status: 'passed', exitCode: 0, durationMs: 1 }], sourceChanged: ['docs/shot.png'] };
  assert.match(summarizeGateReport(report), /source changed during the run[^]*`docs\/shot\.png`/);
});

test('fail-fast staging stops after failure and cannot promote skipped gates', async t => {
  const f = await fixture(t), seen = [];
  const report = await runGateCatalog(f.root, 'release', {
    snapshot, failFast: true, execute: async gate => {
      seen.push(gate.id);
      return { status: 'failed', exitCode: 1 };
    },
  });
  assert.deepEqual(seen, ['one']);
  assert.equal(report.results[1].status, 'not-run');
  assert.equal(report.status, 'failed');
  const catalog = await loadGateCatalog(f.root);
  assert.throws(() => validateReleaseReceipt(report, catalog, 'source-digest'));
});

test('fail-fast still runs every gate on success and produces a promotable receipt', async t => {
  const f = await fixture(t), seen = [];
  const report = await runGateCatalog(f.root, 'release', {
    snapshot, failFast: true, execute: async gate => {
      seen.push(gate.id);
      return pass();
    },
  });
  assert.deepEqual(seen, ['one', 'two']);
  const catalog = await loadGateCatalog(f.root);
  validateReleaseReceipt(report, catalog, 'source-digest');
});
