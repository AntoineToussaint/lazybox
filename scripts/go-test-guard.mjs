// Runs `go test` with a deadline on every single test (AGENTS.md: every unit
// test finishes within 30 seconds).
//
//   node scripts/go-test-guard.mjs [go test flags] [packages]
//
// go test's -timeout bounds a whole test binary, so one hung test is found only
// when its package's minutes run out, and a slow one never. This runs
// `go test -json` and follows every test in the event stream, subtests and
// parallel tests included. A test's time is the time it runs: a parallel test
// held by t.Parallel() is paused, and its clock with it, as go test reports it.
// A parent's time includes its subtests. When a test has run for longer than
// UNIT_TEST_TIMEOUT_MS, the guard sends its test binary SIGQUIT, so the binary
// prints every goroutine's stack and exits (SIGKILL if it has not exited
// KILL_GRACE_MS later), and the run fails naming the test: the deepest one
// still running under it, since a hung subtest runs its parent out first.
//
// Only tests are timed. Package initialisation, a TestMain's work outside
// m.Run, and the binary's start and exit belong to no test; go test's
// -timeout, which bounds the whole binary, is what stops those.
//
// The output is go test's own text, not JSON: with -v every line, otherwise a
// top-level test's lines (its subtests' included) only when it fails or was
// still running when its binary exited, and the package lines go test prints
// (`ok`, `FAIL`, `?`, build errors). The exit status is go test's, or 1 when a
// test ran over its deadline.
import { spawn, spawnSync } from 'node:child_process';
import { readlinkSync, realpathSync } from 'node:fs';
import { createInterface } from 'node:readline';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { UNIT_TEST_TIMEOUT_MS } from './unit-test-timeouts.mjs';

/** How long a binary sent SIGQUIT has to print its stacks and exit before SIGKILL. */
export const KILL_GRACE_MS = 10_000;

const seconds = ms => `${(ms / 1000).toFixed(1)} s`;
const topLevel = name => name.split('/')[0];
const verboseFlag = /^--?(test\.)?v(=true)?$/;

/**
 * Follows a `go test -json` event stream: what to print, and which tests have
 * run longer than `limitMs`. Pure apart from the clock it is handed, so the
 * tests drive it with recorded events.
 */
export class TestGuard {
  constructor({ limitMs = UNIT_TEST_TIMEOUT_MS, verbose = false } = {}) {
    this.limitMs = limitMs;
    this.verbose = verbose;
    /** `${package}\0${test}` → { package, test, ranMs, since } (since: null while paused). */
    this.running = new Map();
    /** `${package}\0${top-level test}` → the lines go test -v prints for it. */
    this.buffers = new Map();
    /** Every test found over its deadline: { package, test, ranMs }. */
    this.overdue = [];
    this.reported = new Set();
  }

  /** Handles one event at `now` (ms, monotonic); returns the text to print. */
  event(event, now) {
    const { Action: action, Package: pkg, Test: test, Output: output = '' } = event;
    if (action === 'build-output') return output;
    if (!pkg) return '';
    const key = `${pkg}\0${test}`;
    if (test) {
      const clock = this.running.get(key);
      if (action === 'run') this.running.set(key, { package: pkg, test, ranMs: 0, since: now });
      else if (action === 'pause' && clock?.since != null) { clock.ranMs += now - clock.since; clock.since = null; }
      else if (action === 'cont' && clock && clock.since == null) clock.since = now;
      else if (['pass', 'fail', 'skip'].includes(action)) this.running.delete(key);
      if (this.verbose) return action === 'output' ? output : '';
      const owner = `${pkg}\0${topLevel(test)}`;
      if (action === 'output') this.buffers.set(owner, (this.buffers.get(owner) || '') + output);
      if (test.includes('/') || !['pass', 'fail', 'skip'].includes(action)) return '';
      const lines = this.buffers.get(owner) || '';
      this.buffers.delete(owner);
      return action === 'fail' ? lines : '';
    }
    if (action === 'output') {
      if (!this.verbose && output === 'PASS\n') return '';
      // The binary's last line (FAIL, or go test's `ok`/`FAIL pkg`): a test
      // still open here ended with its binary, a hung one included, and its
      // lines (the goroutine dump with them) belong before the verdict.
      return (/^(ok|FAIL)\b/.test(output) ? this.unfinished(pkg) : '') + output;
    }
    if (action !== 'pass' && action !== 'fail' && action !== 'skip') return '';
    for (const [open, clock] of this.running) if (clock.package === pkg) this.running.delete(open);
    return this.unfinished(pkg);
  }

  /** The lines of `pkg`'s tests that never reported an end, once. */
  unfinished(pkg) {
    let lines = '';
    for (const [owner, text] of this.buffers) {
      if (!owner.startsWith(`${pkg}\0`)) continue;
      lines += text;
      this.buffers.delete(owner);
    }
    return lines;
  }

  /**
   * The tests that went over their deadline since the last call. A parent's
   * clock includes its subtests, so it crosses first when a subtest hangs: the
   * report names the deepest test still running under it (`test`), with the
   * test whose clock ran out (`over`). Each culprit is reported once.
   */
  check(now) {
    const found = [];
    for (const [key, clock] of this.running) {
      const ranMs = clock.ranMs + (clock.since == null ? 0 : now - clock.since);
      if (ranMs <= this.limitMs || this.reported.has(key)) continue;
      this.reported.add(key);
      const test = this.deepestRunning(clock.package, clock.test);
      const culprit = `${clock.package}\0${test}`;
      if (this.reported.has(`culprit\0${culprit}`)) continue;
      this.reported.add(`culprit\0${culprit}`);
      found.push({ package: clock.package, test, over: clock.test, ranMs });
    }
    this.overdue.push(...found);
    return found;
  }

  /** The most deeply nested test running (not paused) under `test`, or `test` itself. */
  deepestRunning(pkg, test) {
    let deepest = test;
    for (const clock of this.running.values()) {
      if (clock.package !== pkg || clock.since == null || !clock.test.startsWith(`${test}/`)) continue;
      if (clock.test.split('/').length > deepest.split('/').length) deepest = clock.test;
    }
    return deepest;
  }

  /** The closing report: every test that ran over, or nothing. */
  summary() {
    if (!this.overdue.length) return '';
    const lines = this.overdue.map(item => `    ${item.test} (${item.package}): stopped after ${seconds(item.ranMs)}${item.over === item.test ? '' : ` of ${item.over}`}`);
    return `\nFAIL: ${this.overdue.length === 1 ? 'a test' : `${this.overdue.length} tests`} ran over the ${seconds(this.limitMs)} limit on every unit test (AGENTS.md):\n${lines.join('\n')}\n`;
  }
}

/** What the guard prints the moment a test runs over. */
export function timeoutLine(item, limitMs) {
  const over = `${seconds(item.ranMs)}, over the ${seconds(limitMs)} limit`;
  return item.over === item.test
    ? `--- TIMEOUT: ${item.test} (${item.package}) has run for ${over}\n`
    : `--- TIMEOUT: ${item.test} (${item.package}) is still running; ${item.over} has run for ${over}\n`;
}

/** The test binaries the go command at `goPid` is running: [{ pid, path }]. */
export function testBinaries(goPid, list = () => spawnSync('ps', ['-A', '-o', 'pid=,ppid=,args='], { encoding: 'utf8' }).stdout || '') {
  const found = [];
  for (const line of list().split('\n')) {
    const match = /^\s*(\d+)\s+(\d+)\s+(\S+)/.exec(line);
    if (match && Number(match[2]) === goPid && match[3].endsWith('.test')) found.push({ pid: Number(match[1]), path: match[3] });
  }
  return found;
}

/** A process's working directory: /proc on Linux, lsof on macOS. */
function processCwd(pid) {
  try { return readlinkSync(`/proc/${pid}/cwd`); } catch { /* not Linux */ }
  const out = spawnSync('lsof', ['-a', '-p', String(pid), '-d', 'cwd', '-Fn'], { encoding: 'utf8' }).stdout || '';
  return /^n(.+)$/m.exec(out)?.[1];
}

/**
 * The binary running `pkg`'s tests. go test names it after the package's last
 * path element and runs it in the package's directory, which tells apart two
 * packages with the same last element running at once (-p > 1).
 */
function binaryFor(goPid, pkg, cwd) {
  const name = `${pkg.split('/').at(-1)}.test`;
  const candidates = testBinaries(goPid).filter(binary => binary.path.split('/').at(-1) === name);
  if (candidates.length <= 1) return candidates[0];
  const dir = spawnSync('go', ['list', '-f', '{{.Dir}}', pkg], { cwd, encoding: 'utf8' }).stdout?.trim();
  return candidates.find(binary => processCwd(binary.pid) === dir);
}

async function main() {
  let args = process.argv.slice(2), limitMs = UNIT_TEST_TIMEOUT_MS;
  // Only the guard's own tests shorten the deadline (go-test-guard.test.mjs):
  // they must finish within it themselves. The catalog check refuses this
  // flag in the Makefile, CI and package.json (scripts/unit-test-timeouts.mjs).
  const shorter = /^--limit-ms=(\d+)$/.exec(args[0] || '');
  if (shorter) { limitMs = Math.min(Number(shorter[1]), UNIT_TEST_TIMEOUT_MS); args = args.slice(1); }
  const guard = new TestGuard({ limitMs, verbose: args.some(arg => verboseFlag.test(arg)) });
  const go = spawn('go', ['test', '-json', ...args], { stdio: ['inherit', 'pipe', 'inherit'] });
  const forward = signal => () => go.kill(signal);
  for (const signal of ['SIGINT', 'SIGTERM']) process.on(signal, forward(signal));
  const signalled = new Set(), kills = [];
  const stop = () => {
    for (const item of guard.check(performance.now())) {
      process.stdout.write(timeoutLine(item, guard.limitMs));
      if (signalled.has(item.package)) continue;
      signalled.add(item.package);
      const binary = binaryFor(go.pid, item.package, process.cwd());
      if (!binary) { process.stdout.write(`    its test binary has already exited\n`); continue; }
      process.stdout.write(`    sending SIGQUIT to ${binary.path} (pid ${binary.pid}) for a goroutine dump\n`);
      try { process.kill(binary.pid, 'SIGQUIT'); } catch { continue; }
      // Still the go command's test binary, not a process that reused its pid.
      const kill = () => { if (testBinaries(go.pid).some(child => child.pid === binary.pid)) try { process.kill(binary.pid, 'SIGKILL'); } catch { /* exited */ } };
      kills.push(setTimeout(kill, KILL_GRACE_MS));
    }
  };
  const timer = setInterval(stop, 250);
  const lines = createInterface({ input: go.stdout, crlfDelay: Infinity });
  for await (const line of lines) {
    let event;
    try { event = JSON.parse(line); } catch { process.stdout.write(`${line}\n`); continue; }
    const text = guard.event(event, performance.now());
    if (text) process.stdout.write(text);
  }
  const [code, signal] = await new Promise(done => go.exitCode !== null || go.signalCode !== null ? done([go.exitCode, go.signalCode]) : go.once('close', (c, s) => done([c, s])));
  clearInterval(timer);
  kills.forEach(clearTimeout);
  const summary = guard.summary();
  if (summary) process.stdout.write(summary);
  process.exitCode = summary && !code ? 1 : code ?? (signal ? 1 : 0);
}

// Run as a program, also through a symlink (macOS's /tmp is /private/tmp):
// node resolves the module's own path, not argv[1], so both are compared real.
const real = path => { try { return realpathSync(path); } catch { return resolve(path); } };
if (process.argv[1] && real(process.argv[1]) === real(fileURLToPath(import.meta.url))) main().catch(error => { console.error(`go-test-guard: ${error.message}`); process.exitCode = 1; });
