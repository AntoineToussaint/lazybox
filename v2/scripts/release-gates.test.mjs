import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, readFile, writeFile, rm } from 'node:fs/promises';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';
import { loadGateCatalog, selectGates, runGateCatalog, validateReleaseReceipt, executeGate } from './release-gates.mjs';

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
