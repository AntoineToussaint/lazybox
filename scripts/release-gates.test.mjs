import { test } from 'node:test';
import { spawnSync } from 'node:child_process';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { mkdtemp, mkdir, readFile, writeFile, rm } from 'node:fs/promises';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';
import { loadGateCatalog, selectGates, shardGates, runGateCatalog, validateReleaseReceipt, executeGate, summarizeGateReport, changedFiles } from './release-gates.mjs';
import { digestTree } from './environments.mjs';
import { makefileViolations, nextestViolations, scriptFileViolations, scriptViolations, unitTestTimeoutViolations, workflowViolations } from './unit-test-timeouts.mjs';

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

test('every unit test suite here runs with a 30 s deadline per test', () => {
  const root = fileURLToPath(new URL('..', import.meta.url));
  assert.deepEqual(unitTestTimeoutViolations(root), []);
  const makefile = readFileSync(join(root, 'Makefile'), 'utf8');
  for (const target of ['test', 'test-outlive', 'test-reboot']) assert.match(makefile, new RegExp(`^${target}:[^\\n]*\\n(\\t[^\\n]*\\n)*\\t[^\\n]*\\$\\(GO_TEST\\)`, 'm'), `make ${target} goes through the Go guard`);
  assert.match(makefile, /^GO_TEST := node \$\(abspath scripts\/go-test-guard\.mjs\)$/m);
  assert.match(makefile, /^check-desktop:[^\n]*\n(\t[^\n]*\n)*\t\$\(NEXTEST\) --manifest-path apps\/desktop\/src-tauri\/Cargo\.toml\n\t\$\(NEXTEST\) --manifest-path tools\/lazybox-snippets\/Cargo\.toml$/m);
});

// The browser service's tests launch the agent browser's Chromium; a fresh
// checkout has none, so what runs them installs it first.
test('make check and CI install the agent browser before the browser service tests', () => {
  const root = fileURLToPath(new URL('..', import.meta.url));
  const plan = spawnSync('make', ['-n', '-C', root, 'check-workspaces'], { encoding: 'utf8' });
  assert.equal(plan.status, 0, plan.stderr);
  const install = plan.stdout.indexOf('npm run --silent install:browser'), tests = plan.stdout.indexOf('npm run check:client');
  assert.ok(install >= 0 && install < tests, `make check-workspaces runs:\n${plan.stdout}`);
  const ci = readFileSync(join(root, '.github/workflows/ci.yml'), 'utf8');
  const job = ci.slice(ci.indexOf('\n  typescript:'));
  const ciInstall = job.indexOf('- run: npm run install:browser'), ciTests = job.indexOf('- run: npm run check:client');
  assert.ok(ciInstall >= 0 && ciInstall < ciTests, 'CI\'s typescript job installs the agent browser before npm run check:client');
});

test('a unit test without its per-test deadline fails admission, wherever it is started', async t => {
  assert.deepEqual(scriptViolations({ 'test:a': 'node --import tsx --test --test-timeout=30000 a.test.ts', 'test:ui': 'node scripts/test-ui.mjs' }), []);
  assert.match(scriptViolations({ 'test:a': 'npm run build && node --import tsx --test a.test.ts' })[0], /test:a: node --test without --test-timeout=30000/);
  assert.match(scriptViolations({ 'test:a': 'node --test --test-timeout=60000 a.test.mjs' })[0], /without --test-timeout=30000/, 'another deadline is not the rule');
  assert.match(scriptViolations({ 'test:go': 'cd services && go test ./...' })[0], /go test without scripts\/go-test-guard\.mjs/);

  const make = recipe => makefileViolations(`GO_TEST := node $(abspath scripts/go-test-guard.mjs)\nNEXTEST := cargo nextest run --config-file $(abspath .config/nextest.toml) --locked\n.PHONY: test\ntest: deps\n${recipe}\n`);
  assert.deepEqual(make('\tcd code && $(GO_TEST) -timeout 4m ./...\n\t$(NEXTEST) --manifest-path a/Cargo.toml\n\tnode --test --test-timeout=30000 a.test.mjs'), []);
  assert.match(make('\tcd code && go test -timeout 4m ./...')[0], /Makefile test: go test without scripts\/go-test-guard\.mjs/);
  assert.match(make('\tcd code && $(GO_TEST) ./... ; \\\n\tstatus=$$?; node --test a.test.mjs')[0], /node --test without/, 'continued lines are one command line');
  assert.match(make('\tcargo test --manifest-path a/Cargo.toml --locked')[0], /cargo test has no per-test deadline/);
  assert.match(make('\tcargo nextest run --manifest-path a/Cargo.toml')[0], /without --config-file \.config\/nextest\.toml/);
  assert.match(make('\t$(GO_TEST) --limit-ms=60000 ./...')[0], /--limit-ms is for its own tests/);
  assert.deepEqual(makefileViolations('test-real: build-worker\n\tcd code && LAZYBOX_V2_REAL=1 go test -timeout 12m -v ./tests/integration\n'), [], 'real-provider targets are not unit tests');
  for (const bare of ['\tgo -C services/runner/code test ./...', '\tgo -C=services/runner/code test ./...', '\t$$GO test ./...', '\t$(GO) test ./...', '\t/usr/local/go/bin/go test ./...', '\tcd code && GOFLAGS=-p=1 go vet ./... && go test ./...'])
    assert.match(make(bare)[0] || '', /go test without scripts\/go-test-guard\.mjs/, bare);
  assert.deepEqual(make('\tgo -C services/runner/code vet ./...\n\tgo build ./...'), [], 'other go commands are not tests');
  assert.match(make('\tcargo +stable test --locked')[0], /cargo test has no per-test deadline/);
  assert.match(make('\tbash scripts/release/publish-cask_test.sh')[0], /a test file run outside node --test --test-timeout=30000/);
  assert.match(make('\tnode scripts/release/install.test.mjs')[0], /a test file run outside node --test/);

  assert.deepEqual(scriptFileViolations({
    'scripts/stress.mjs': "run(process.execPath, [join(v2, 'scripts/go-test-guard.mjs'), '-race'])",
    'scripts/go-test-guard.mjs': "spawn('go', ['test', '-json', ...args])",
    'scripts/release/build.mjs': "run('go', ['test', '-run', '^TestRelease'])",
    'scripts/activity.test.ts': "assert.equal(title('go test ./...'), 'go test')",
    'scripts/fixtures/page.mjs': "tool('Bash', { command: 'go test ./...' })",
  }), [], 'the guard, a listed non-unit script, test fixtures and quoted commands');
  assert.match(scriptFileViolations({ 'scripts/stress.mjs': "run('go', ['test', '-race', '-count=50'])" })[0], /scripts\/stress\.mjs: spawns go test without/);
  assert.match(scriptFileViolations({ 'scripts/x.mjs': 'spawn("go", ["-C", dir, "test", "./..."])' })[0], /spawns go test without/);
  assert.match(scriptFileViolations({ 'scripts/x.mjs': "spawn('cargo', ['test'])" })[0], /spawns cargo test/);
  assert.match(scriptFileViolations({ 'scripts/x.mjs': "spawn(process.execPath, ['--test', file])" })[0], /spawns node --test without --test-timeout=30000/);
  assert.deepEqual(scriptFileViolations({ 'scripts/x.mjs': "spawn(process.execPath, ['--test', '--test-timeout=30000', file])" }), []);
  assert.match(scriptFileViolations({ 'scripts/check.sh': '#!/bin/sh\n# go test used to run here\ncd code && "$GO" test ./...\n' })[0], /scripts\/check\.sh: go test without/);

  assert.deepEqual(workflowViolations('    steps:\n      # cargo test used to run here\n      - run: make test\n', 'ci.yml'), []);
  assert.match(workflowViolations('      - run: cd services/runner/code && go test ./...\n', 'ci.yml')[0], /ci\.yml: go test without/);
  assert.match(workflowViolations('      - run: node --test scripts/x.test.mjs\n', 'ci.yml')[0], /node --test without/);

  const config = 'nextest-version = { required = "0.9.152" }\n\n[profile.default]\nslow-timeout = { period = "30s", terminate-after = 1 }\n';
  assert.deepEqual(nextestViolations(config, { 'ci.yml': '      NEXTEST_VERSION: "0.9.152"\n' }), []);
  assert.match(nextestViolations(config.replace('30s', '60s'))[0], /slow-timeout = \{ period = "30s", terminate-after = 1 \}/);
  assert.match(nextestViolations(config.replace('terminate-after = 1', 'terminate-after = 2'))[0], /terminate-after = 1/);
  assert.match(nextestViolations(config.replace('[profile.default]', '[profile.ci]'))[0], /\[profile\.default\]/, 'the default profile is the one make runs');
  assert.match(nextestViolations(config, { 'ci.yml': '      NEXTEST_VERSION: "0.9.100"\n' })[0], /installs cargo-nextest 0\.9\.100, \.config\/nextest\.toml requires 0\.9\.152/);
  assert.match(nextestViolations(undefined)[0], /missing/);

  const f = await fixture(t);
  f.pkg.scripts['test:one'] = 'node --test one.test.mjs'; await f.save();
  await assert.rejects(loadGateCatalog(f.root), /must finish within 30 s[\s\S]*test:one: node --test without --test-timeout=30000/);
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
