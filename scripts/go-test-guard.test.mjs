import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { mkdtemp, mkdir, rm, symlink, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { TestGuard, testBinaries, timeoutLine } from './go-test-guard.mjs';
import { UNIT_TEST_TIMEOUT_MS } from './unit-test-timeouts.mjs';

const pkg = 'example.com/m/p';
const at = (guard, now, Action, Test, Output) => guard.event({ Action, Package: pkg, ...(Test ? { Test } : {}), ...(Output ? { Output } : {}) }, now);

test('the deadline is the one every unit test has', () => {
  assert.equal(new TestGuard().limitMs, UNIT_TEST_TIMEOUT_MS);
  assert.equal(UNIT_TEST_TIMEOUT_MS, 30_000);
});

test('a test over its deadline is named once; a passing one is forgotten', () => {
  const guard = new TestGuard({ limitMs: 1000 });
  at(guard, 0, 'run', 'TestQuick');
  at(guard, 0, 'run', 'TestSlow');
  at(guard, 500, 'pass', 'TestQuick');
  assert.deepEqual(guard.check(1000), [], 'exactly at the deadline is still in time');
  assert.deepEqual(guard.check(1001).map(item => [item.test, item.ranMs]), [['TestSlow', 1001]]);
  assert.deepEqual(guard.check(5000), [], 'reported once');
  assert.match(guard.summary(), /TestSlow \(example\.com\/m\/p\): stopped after 1\.0 s/);
});

test('time paused by t.Parallel does not count', () => {
  const guard = new TestGuard({ limitMs: 1000 });
  at(guard, 0, 'run', 'TestParallel');
  at(guard, 100, 'pause', 'TestParallel');
  at(guard, 5000, 'cont', 'TestParallel');
  assert.deepEqual(guard.check(5800), [], '100 ms before the pause and 800 ms after it');
  assert.equal(guard.check(5901)[0].test, 'TestParallel');
});

test('a parent that runs over while a subtest hangs names the deepest running subtest, once', () => {
  const guard = new TestGuard({ limitMs: 1000 });
  at(guard, 0, 'run', 'TestParent');
  at(guard, 600, 'run', 'TestParent/done');
  at(guard, 650, 'pass', 'TestParent/done');
  at(guard, 700, 'run', 'TestParent/waits');
  at(guard, 700, 'run', 'TestParent/waits/hangs');
  at(guard, 700, 'run', 'TestParent/paused');
  at(guard, 700, 'pause', 'TestParent/paused');
  at(guard, 700, 'run', 'TestOther');
  const [item, ...rest] = guard.check(1001);
  assert.deepEqual(rest, [], 'the subtests are not over; TestOther is not either');
  assert.deepEqual(item, { package: pkg, test: 'TestParent/waits/hangs', over: 'TestParent', ranMs: 1001 });
  at(guard, 1100, 'pass', 'TestOther');
  assert.deepEqual(guard.check(1702), [], 'TestParent/waits crossing later names the same subtest: no second report');
  assert.equal(timeoutLine(item, 1000), `--- TIMEOUT: TestParent/waits/hangs (${pkg}) is still running; TestParent has run for 1.0 s, over the 1.0 s limit\n`);
  assert.match(guard.summary(), /TestParent\/waits\/hangs \(example\.com\/m\/p\): stopped after 1\.0 s of TestParent/);
});

test('without -v a test prints only when it fails, its subtests with it, and package lines as go test prints them', () => {
  const guard = new TestGuard();
  const printed = [
    at(guard, 0, 'start'),
    at(guard, 0, 'run', 'TestOk'), at(guard, 0, 'output', 'TestOk', '=== RUN   TestOk\n'), at(guard, 0, 'output', 'TestOk', '--- PASS: TestOk (0.00s)\n'), at(guard, 0, 'pass', 'TestOk'),
    at(guard, 0, 'run', 'TestBad'), at(guard, 0, 'output', 'TestBad', '=== RUN   TestBad\n'),
    at(guard, 0, 'run', 'TestBad/sub'), at(guard, 0, 'output', 'TestBad/sub', '    x_test.go:9: boom\n'), at(guard, 0, 'fail', 'TestBad/sub'),
    at(guard, 0, 'output', 'TestBad', '--- FAIL: TestBad (0.00s)\n'), at(guard, 0, 'fail', 'TestBad'),
    at(guard, 0, 'output', undefined, 'FAIL\n'), at(guard, 0, 'output', undefined, `FAIL\t${pkg}\t0.1s\n`), at(guard, 0, 'fail'),
  ].join('');
  assert.equal(printed, `=== RUN   TestBad\n    x_test.go:9: boom\n--- FAIL: TestBad (0.00s)\nFAIL\nFAIL\t${pkg}\t0.1s\n`);
  const passing = new TestGuard();
  assert.equal([at(passing, 0, 'output', undefined, 'PASS\n'), at(passing, 0, 'output', undefined, `ok  \t${pkg}\t0.1s\n`)].join(''), `ok  \t${pkg}\t0.1s\n`, 'go test hides PASS without -v');
  assert.equal(guard.event({ Action: 'build-output', ImportPath: pkg, Output: '# p\n./x.go:1: oops\n' }, 0), '# p\n./x.go:1: oops\n');
});

test('a test still open when its binary ends prints its lines (the goroutine dump among them) before the verdict', () => {
  const guard = new TestGuard({ limitMs: 1000 });
  at(guard, 0, 'run', 'TestHangs');
  at(guard, 0, 'output', 'TestHangs', '=== RUN   TestHangs\n');
  at(guard, 0, 'output', 'TestHangs', 'SIGQUIT: quit\ngoroutine 7 [sleep]:\n');
  assert.equal(at(guard, 0, 'output', undefined, `FAIL\t${pkg}\t31.0s\n`), `=== RUN   TestHangs\nSIGQUIT: quit\ngoroutine 7 [sleep]:\nFAIL\t${pkg}\t31.0s\n`);
  assert.equal(at(guard, 0, 'fail'), '');
  assert.deepEqual(guard.check(60_000), [], 'the package ended: nothing of it is still running');
});

test('with -v every line prints as it comes', () => {
  const guard = new TestGuard({ verbose: true });
  assert.equal([at(guard, 0, 'run', 'TestOk'), at(guard, 0, 'output', 'TestOk', '=== RUN   TestOk\n'), at(guard, 0, 'pass', 'TestOk'), at(guard, 0, 'output', undefined, 'PASS\n')].join(''), '=== RUN   TestOk\nPASS\n');
});

test('only the go command\'s own test binaries are candidates', () => {
  const ps = () => ' 10 1 /usr/local/go/bin/go test -json ./...\n 11 10 /tmp/go-build1/b001/runner.test -test.v=test2json\n 12 11 /tmp/go-build1/b001/runner.test -sessionhost\n 13 10 /usr/local/go/pkg/tool/compile -o x\n';
  assert.deepEqual(testBinaries(10, ps), [{ pid: 11, path: '/tmp/go-build1/b001/runner.test' }], 'not a session host the binary started, not the compiler');
});

const hasGo = spawnSync('go', ['version']).status === 0;

test('a hung test is stopped with a goroutine dump and named; the run fails and the next package still runs', { skip: !hasGo && 'needs go' }, async t => {
  const dir = await mkdtemp(join(tmpdir(), 'lazybox-go-guard-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  await writeFile(join(dir, 'go.mod'), 'module example.com/guard\n\ngo 1.21\n');
  await writeFile(join(dir, 'hang_test.go'), 'package guard\n\nimport (\n\t"testing"\n\t"time"\n)\n\nfunc TestFine(t *testing.T) {}\n\nfunc TestHangs(t *testing.T) {\n\ttime.Sleep(time.Second)\n\tt.Run("inner", func(t *testing.T) { t.Log("waiting"); time.Sleep(time.Hour) })\n}\n');
  await mkdir(join(dir, 'other'));
  await writeFile(join(dir, 'other', 'other_test.go'), 'package other\n\nimport "testing"\n\nfunc TestOther(t *testing.T) {}\n');
  const guard = fileURLToPath(new URL('go-test-guard.mjs', import.meta.url));
  const child = spawn(process.execPath, [guard, '--limit-ms=1500', '-count=1', '-p=1', './...'], { cwd: dir, env: { ...process.env, GOFLAGS: '' } });
  let out = '';
  child.stdout.on('data', data => { out += data; });
  child.stderr.on('data', data => { out += data; });
  const code = await new Promise(done => child.once('close', done));
  assert.equal(code, 1, out);
  // The parent ran a second before its subtest hung, so its clock runs out first: the subtest is named.
  assert.match(out, /--- TIMEOUT: TestHangs\/inner \(example\.com\/guard\) is still running; TestHangs has run for \d+\.\d s, over the 1\.5 s limit/);
  assert.equal(out.match(/--- TIMEOUT/g).length, 1, out);
  assert.match(out, /sending SIGQUIT to \S+guard\.test \(pid \d+\)/);
  assert.match(out, /=== RUN   TestHangs\/inner\n\s+hang_test\.go:\d+: waiting\n[\s\S]*SIGQUIT: quit[\s\S]*goroutine \d+ .*\[sleep\]/, 'the hung test\'s lines and its stacks');
  assert.match(out, /^FAIL\texample\.com\/guard\t/m);
  assert.match(out, /^ok  \texample\.com\/guard\/other\t/m);
  assert.doesNotMatch(out, /TestFine/, 'a passing test stays quiet');
  assert.doesNotMatch(out, /"Action"/, 'go test\'s text, not its JSON');
  assert.match(out, /FAIL: a test ran over the 1\.5 s limit on every unit test \(AGENTS\.md\):\n    TestHangs\/inner \(example\.com\/guard\): stopped after \d+\.\d s of TestHangs\n/);
});

test('a passing run passes, with go test\'s exit status', { skip: !hasGo && 'needs go' }, async t => {
  const dir = await mkdtemp(join(tmpdir(), 'lazybox-go-guard-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  await writeFile(join(dir, 'go.mod'), 'module example.com/pass\n\ngo 1.21\n');
  await writeFile(join(dir, 'pass_test.go'), 'package pass\n\nimport "testing"\n\nfunc TestFine(t *testing.T) {}\n');
  const guard = fileURLToPath(new URL('go-test-guard.mjs', import.meta.url));
  const run = spawnSync(process.execPath, [guard, '-count=1', './...'], { cwd: dir, encoding: 'utf8', env: { ...process.env, GOFLAGS: '' } });
  assert.equal(run.status, 0, run.stdout + run.stderr);
  assert.match(run.stdout, /^ok  \texample\.com\/pass\t/m);
  const broken = spawnSync(process.execPath, [guard, '-count=1', './nowhere'], { cwd: dir, encoding: 'utf8', env: { ...process.env, GOFLAGS: '' } });
  assert.notEqual(broken.status, 0, 'go test\'s own failure is the guard\'s');
  // Started through a symlink, it still runs (and still fails), rather than exiting 0 having done nothing.
  const linked = join(dir, 'linked-guard.mjs');
  await symlink(guard, linked);
  const viaLink = spawnSync(process.execPath, [linked, '-count=1', './...'], { cwd: dir, encoding: 'utf8', env: { ...process.env, GOFLAGS: '' } });
  assert.equal(viaLink.status, 0, viaLink.stdout + viaLink.stderr);
  assert.match(viaLink.stdout, /^ok  \texample\.com\/pass\t/m);
  assert.notEqual(spawnSync(process.execPath, [linked, '-count=1', './nowhere'], { cwd: dir, encoding: 'utf8', env: { ...process.env, GOFLAGS: '' } }).status, 0);
});
