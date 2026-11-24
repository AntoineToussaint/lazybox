// Flake detection: repeat tests under the conditions that expose races.
//
//   node scripts/stress.mjs go [--count 50] [--load 15] [--run REGEX] [package...]
//   node scripts/stress.mjs ui [--runs 10] [--parallel 3] [--load 0] suite...
//
// go runs `go test -race` with GOMAXPROCS=1 (one P makes goroutine interleavings
// coarse and late, which is how a missing wait shows up) while --load busy
// loops compete for every core, through scripts/go-test-guard.mjs so each run
// of a test has the 30 s deadline every unit test has. ui runs each npm `test:<suite>` --runs times,
// with --parallel copies of the whole list at once, so suites share the machine
// and anything they share (servers, caches, ports) is contended.
//
// It reports failures per test with the log of each failing run, and exits
// non-zero if any run failed. It does not retry: a test that fails here once is
// flaky until its cause is found.
import { spawn } from 'node:child_process';
import { mkdtemp, writeFile } from 'node:fs/promises';
import { tmpdir, availableParallelism } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const v2 = fileURLToPath(new URL('..', import.meta.url));
const [mode, ...rest] = process.argv.slice(2);
const options = {}, names = [];
for (let i = 0; i < rest.length; i++) {
  if (rest[i].startsWith('--')) options[rest[i].slice(2)] = rest[++i];
  else names.push(rest[i]);
}
if (mode !== 'go' && mode !== 'ui') {
  console.error('usage: stress.mjs go [--count N] [--load N] [--run REGEX] [package...]\n       stress.mjs ui [--runs N] [--parallel N] [--load N] suite...');
  process.exit(2);
}

const run = (command, args, cwd, env = {}) => new Promise(resolve => {
  const child = spawn(command, args, { cwd, env: { ...process.env, ...env }, stdio: ['ignore', 'pipe', 'pipe'] });
  let out = '';
  child.stdout.on('data', d => { out += d; });
  child.stderr.on('data', d => { out += d; });
  child.on('close', code => resolve({ code, out }));
});

// Busy loops in separate processes, so they compete with the tests for cores.
const spinners = Array.from({ length: Number(options.load ?? (mode === 'go' ? availableParallelism() : 0)) },
  () => spawn(process.execPath, ['-e', 'for(;;){}'], { stdio: 'ignore' }));
const stopSpinners = () => { for (const s of spinners) s.kill('SIGKILL'); };
process.on('SIGINT', () => { stopSpinners(); process.exit(130); });

const logs = await mkdtemp(join(tmpdir(), 'lazybox-stress-'));
let failed = 0;
try {
  if (mode === 'go') {
    const count = options.count ?? '50';
    const packages = names.length ? names : ['./internal/...'];
    const args = ['-trimpath', '-race', `-count=${count}`, ...(options.run ? ['-run', options.run] : []), ...packages];
    console.log(`go test ${args.join(' ')}  (GOMAXPROCS=1, ${spinners.length} busy loops, 30 s per test)`);
    // Through the per-test guard, as make test runs them: a run that hangs is
    // stopped at 30 s and counted against its test (--- TIMEOUT), not left to
    // spend the package's -timeout.
    const { code, out } = await run(process.execPath, [join(v2, 'scripts/go-test-guard.mjs'), ...args], join(v2, 'services/runner/code'), { GOMAXPROCS: '1' });
    const failures = out.split('\n').filter(line => line.startsWith('--- FAIL') || line.startsWith('--- TIMEOUT'));
    const byTest = new Map();
    for (const line of failures) {
      const name = line.split(' ')[2];
      byTest.set(name, (byTest.get(name) || 0) + 1);
    }
    for (const [name, n] of byTest) console.log(`FLAKY ${name}: failed ${n} of ${count} runs`);
    if (code !== 0) {
      failed = Math.max(1, failures.length);
      await writeFile(join(logs, 'go.log'), out);
      console.log(`log: ${join(logs, 'go.log')}`);
    }
  } else {
    if (!names.length) throw new Error('name at least one UI suite (the npm test:<suite> script)');
    const runs = Number(options.runs ?? 10), parallel = Number(options.parallel ?? 3);
    console.log(`${names.join(', ')}: ${runs} runs, ${parallel} at a time, ${spinners.length} busy loops`);
    const results = new Map(names.map(name => [name, 0]));
    const copy = async lane => {
      for (let i = 0; i < runs; i++) {
        for (const name of names) {
          const { code, out } = await run('npm', ['run', '-s', `test:${name}`], v2);
          if (code === 0) continue;
          results.set(name, results.get(name) + 1);
          const path = join(logs, `${name}-${lane}-${i}.log`);
          await writeFile(path, out);
          console.log(`FAIL ${name} (lane ${lane}, run ${i}): ${path}`);
        }
      }
    };
    await Promise.all(Array.from({ length: parallel }, (_, lane) => copy(lane)));
    for (const [name, n] of results) {
      console.log(`${n ? 'FLAKY' : 'ok   '} ${name}: failed ${n} of ${runs * parallel} runs`);
      failed += n;
    }
  }
} finally {
  stopSpinners();
}
process.exit(failed ? 1 : 0);
