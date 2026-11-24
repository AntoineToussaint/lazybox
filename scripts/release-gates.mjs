import { readFile, mkdir, writeFile, rename, appendFile } from 'node:fs/promises';
import { createHash, randomUUID } from 'node:crypto';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { UNIT_TEST_TIMEOUT_MS, unitTestTimeoutViolations } from './unit-test-timeouts.mjs';

const profiles = new Set(['release', 'core', 'native', 'ui']);
const sha = value => createHash('sha256').update(value).digest('hex');

// A newly added npm test must be admitted, aliased to an admitted test, or
// explicitly classified as a live integration. It cannot silently miss staging.
export async function loadGateCatalog(root) {
  const catalog = JSON.parse(await readFile(join(root, 'scripts/release-gates.json'), 'utf8'));
  const { scripts } = JSON.parse(await readFile(join(root, 'package.json'), 'utf8'));
  if (catalog.version !== 1 || !Array.isArray(catalog.gates) || !catalog.gates.length || !Array.isArray(catalog.external) || !Array.isArray(catalog.aliases) || !Array.isArray(catalog.policyFiles)) throw new Error('Invalid release gate catalog');
  const ids = new Set(), covered = new Set();
  const cover = name => {
    if (!name.startsWith('test:') || !scripts[name] || covered.has(name)) throw new Error(`Missing or duplicate test classification: ${name}`);
    covered.add(name);
  };
  const gates = catalog.gates.map(gate => {
    if (!/^[a-z][a-z0-9-]*$/.test(gate.id) || ids.has(gate.id) || !['core', 'native', 'ui'].includes(gate.scope)) throw new Error('Invalid or duplicate gate');
    ids.add(gate.id);
    let command = gate.command, args = gate.args;
    if (gate.script) {
      if (command || args || gate.covers) throw new Error(`Gate ${gate.id} mixes command and npm forms`);
      cover(gate.script); command = 'npm'; args = ['run', gate.script];
    } else {
      if (typeof command !== 'string' || !command || !Array.isArray(args) || !args.every(arg => typeof arg === 'string') || !Array.isArray(gate.covers)) throw new Error(`Invalid command gate ${gate.id}`);
      gate.covers.forEach(cover);
    }
    const timeoutMs = gate.timeoutMs ?? 180_000;
    if (!Number.isSafeInteger(timeoutMs) || timeoutMs <= 0) throw new Error(`Invalid deadline for ${gate.id}`);
    return { id: gate.id, scope: gate.scope, command, args, timeoutMs };
  });
  for (const item of catalog.external) {
    if (typeof item.reason !== 'string' || !item.reason.trim()) throw new Error('Live gates need an explicit reason');
    cover(item.script);
  }
  for (const item of catalog.aliases) {
    if (!covered.has(item.target) || scripts[item.script] !== scripts[item.target]) throw new Error(`Invalid test alias ${item.script}`);
    cover(item.script);
  }
  for (const name of Object.keys(scripts).filter(name => name.startsWith('test:'))) {
    if (!covered.has(name)) throw new Error(`Test is absent from release gate catalog: ${name}`);
  }
  // Every unit test has a per-test deadline, in every suite (AGENTS.md).
  const unbounded = unitTestTimeoutViolations(root);
  if (unbounded.length) throw new Error(`Every unit test must finish within ${UNIT_TEST_TIMEOUT_MS / 1000} s, enforced per test (scripts/unit-test-timeouts.mjs):\n  ${unbounded.join('\n  ')}`);
  // Script bodies are part of the policy fingerprint, not just their names.
  const commands = Object.fromEntries([...covered].sort().map(name => [name, scripts[name]]));
  const policyFiles = {};
  for (const path of catalog.policyFiles) {
    if (typeof path !== 'string' || path.startsWith('/') || path.split(/[\\/]/).includes('..')) throw new Error('Gate policy files must stay within source');
    policyFiles[path] = sha(await readFile(join(root, path)));
  }
  return { digest: sha(JSON.stringify({ catalog, commands, policyFiles })), gates, external: catalog.external };
}

export function selectGates(catalog, profile, only) {
  if (!profiles.has(profile)) throw new Error('Choose release, core, native or ui');
  const gates = catalog.gates.filter(gate => profile === 'release' || gate.scope === profile);
  if (!only) return gates;
  // A named subset (one CI shard) must name gates this profile owns: a stale
  // or misspelled list fails instead of quietly running nothing.
  const ids = new Set(gates.map(gate => gate.id));
  const unknown = only.filter(id => !ids.has(id));
  if (!only.length || unknown.length || new Set(only).size !== only.length) throw new Error(`Gates outside the ${profile} profile or repeated: ${unknown.join(', ') || only.join(', ')}`);
  return gates.filter(gate => only.includes(gate.id));
}

// Splits a profile into at most `count` disjoint shards that together hold every
// gate exactly once, so CI derives its matrix from this catalog, never a copy.
export function shardGates(gates, count) {
  if (!Number.isSafeInteger(count) || count < 1) throw new Error('Shard count must be a positive integer');
  const shards = Array.from({ length: Math.min(count, gates.length) }, () => []);
  gates.forEach((gate, index) => shards[index % shards.length].push(gate.id));
  return shards;
}

// Each gate owns one child process group, including browser/tool descendants.
// The existing launcher cleanup handles termination; no second supervisor here.
export async function executeGate(gate, { root, signal, env = process.env }) {
  if (signal?.aborted) return { status: 'cancelled', exitCode: null };
  // Loaded here, not at module scope: reading the catalog (`list`, `shards`)
  // must not need installed dependencies, so CI can plan before `npm ci`.
  const { spawnProcessGroup, stopProcessGroup } = await import('./desktop-dev.mjs');
  // Deterministic qualification never inherits a live Runner/provider opt-in,
  // another source tree, a regression toggle, or a different watcher backend.
  const gateEnv = Object.fromEntries(Object.entries(env).filter(([key]) => !key.startsWith('LAZYBOX_V2_') && !key.startsWith('CHOKIDAR_') && !['TEST_SOURCE', 'TEST_WEBKIT', 'REPRO_OLD_BLUR'].includes(key)));
  const child = spawnProcessGroup(gate.command, gate.args, {
    cwd: root, stdio: 'inherit', env: { ...gateEnv, CARGO_BUILD_JOBS: env.CARGO_BUILD_JOBS || '1', GOMAXPROCS: env.GOMAXPROCS || '1', GOFLAGS: `${env.GOFLAGS || '-p=1'} -trimpath` },
  });
  let stopped, termination;
  const stop = reason => {
    if (!stopped) { stopped = reason; termination = stopProcessGroup(child); }
  };
  const abort = () => stop('cancelled');
  const timer = setTimeout(() => stop('timed-out'), gate.timeoutMs);
  signal?.addEventListener('abort', abort, { once: true });
  if (signal?.aborted) abort();
  try {
    const result = await new Promise(resolve => {
      child.once('error', error => resolve({ status: 'failed', exitCode: null, error: error.message }));
      child.once('exit', (code, signal) => resolve({ status: code === 0 ? 'passed' : 'failed', exitCode: code, ...(signal ? { signal } : {}) }));
    });
    await termination;
    return stopped ? { ...result, status: stopped } : result;
  } finally {
    clearTimeout(timer); signal?.removeEventListener('abort', abort);
    // A deadline/cancellation already terminated this group. Do not signal its
    // numeric ID a second time after ownership has ended.
    if (termination) await termination;
    else await stopProcessGroup(child);
  }
}

// `retryFlaky` (CI's UI shards only, never staging): a UI gate that fails runs
// once more, and passing then is recorded as `flaky` with both attempts, not as
// `passed`. The run passes, so one flake does not fail the build, but the
// receipt, the log and the job summary name it, and a release receipt never
// admits it (validateReleaseReceipt requires every gate `passed`).
export async function runGateCatalog(root, profile, { snapshot, execute = executeGate, signal, onProgress = () => {}, only, retryFlaky = false, failFast = false } = {}) {
  if (retryFlaky && profile !== 'ui') throw new Error('Only UI gates are retried; staging and release runs need a clean first pass');
  const catalog = await loadGateCatalog(root), gates = selectGates(catalog, profile, only);
  const sourceDigest = await snapshot();
  const report = { version: 1, profile, ...(only ? { only } : {}), ...(retryFlaky ? { retryFlaky: true } : {}), catalogDigest: catalog.digest, sourceDigest, startedAt: new Date().toISOString(), platform: process.platform, architecture: process.arch, status: 'running', results: [] };
  const attempt = async gate => {
    const started = performance.now();
    let result;
    try { result = signal?.aborted ? { status: 'not-run', exitCode: null } : await execute(gate, { root, signal }); }
    catch (error) { result = { status: 'failed', exitCode: null, error: error.message }; }
    return { ...result, durationMs: Math.round(performance.now() - started) };
  };
  // Diagnostic runs continue after failures; staging opts into fail-fast.
  // Cancellation leaves explicit not-run records and never a passing receipt.
  let failed = false;
  for (const gate of gates) {
    if (failFast && failed) {
      report.results.push({ id: gate.id, command: gate.command, args: gate.args, status: 'not-run', exitCode: null, durationMs: 0 });
      continue;
    }
    onProgress({ gate, phase: 'start' });
    let result = await attempt(gate);
    // Only a plain failure is retried: a deadline or a cancellation is not a flake.
    if (retryFlaky && gate.scope === 'ui' && result.status === 'failed' && !signal?.aborted) {
      onProgress({ gate, phase: 'retry', result });
      const first = result, second = await attempt(gate);
      const passed = second.status === 'passed' && second.exitCode === 0;
      result = { ...(passed ? { status: 'flaky', exitCode: 0 } : second), durationMs: first.durationMs + second.durationMs, attempts: [first, second] };
    }
    const { durationMs, ...outcome } = result;
    report.results.push({ id: gate.id, command: gate.command, args: gate.args, ...outcome, durationMs });
    failed ||= !['passed', 'flaky'].includes(outcome.status) || outcome.exitCode !== 0;
    onProgress({ gate, phase: 'finish', result: outcome });
  }
  report.finalSourceDigest = await snapshot();
  report.finishedAt = new Date().toISOString();
  const ok = item => (item.status === 'passed' || (retryFlaky && item.status === 'flaky')) && item.exitCode === 0;
  report.status = !signal?.aborted && sourceDigest === report.finalSourceDigest && report.results.every(ok) ? 'passed' : 'failed';
  return report;
}

/** Paths added, removed or changed between two `digestTree` file maps, sorted. */
export function changedFiles(before, after) {
  return [...new Set([...before.keys(), ...after.keys()])].filter(path => before.get(path) !== after.get(path)).sort();
}

/** The run in Markdown for a CI job summary: the outcome, then every gate that failed or was flaky. */
export function summarizeGateReport(report) {
  const flaky = report.results.filter(item => item.status === 'flaky'), failed = report.results.filter(item => item.status !== 'passed' && item.status !== 'flaky');
  const lines = [`### Gates: ${report.profile}${report.only ? ` (${report.only.length} named)` : ''}`, '',
    `**${report.status}**: ${report.results.length} gates, ${report.results.length - flaky.length - failed.length} passed, ${flaky.length} flaky (passed on retry), ${failed.length} failed.`];
  if (flaky.length || failed.length) {
    lines.push('', '| Gate | Result | Attempts |', '| --- | --- | --- |');
    for (const item of [...failed, ...flaky]) lines.push(`| \`${item.id}\` | ${item.status === 'flaky' ? 'flaky (passed on retry)' : item.status} | ${(item.attempts || [item]).map(a => `${a.status} (${Math.round(a.durationMs / 1000)} s)`).join(', ')} |`);
  }
  if (report.sourceChanged) lines.push('', `The source changed during the run, so it failed whatever its gates did. A gate must not write the source tree (use \`.cache\` or a temporary directory): ${report.sourceChanged.slice(0, 20).map(path => `\`${path}\``).join(', ')}${report.sourceChanged.length > 20 ? ` and ${report.sourceChanged.length - 20} more` : ''}.`);
  if (flaky.length) lines.push('', 'A flaky gate failed once and passed when run again. It is a race until its cause is found: reproduce it with `make stress-ui` (RELEASE_GATES.md, "Flake detection").');
  return lines.join('\n') + '\n';
}

export function validateReleaseReceipt(report, catalog, sourceDigest) {
  if (!report || report.version !== 1 || report.profile !== 'release' || report.only !== undefined || report.retryFlaky !== undefined || report.status !== 'passed' || report.catalogDigest !== catalog.digest || report.sourceDigest !== sourceDigest || report.finalSourceDigest !== sourceDigest || report.results?.length !== catalog.gates.length) throw new Error('Release has no complete passing receipt for this source and current gate catalog; stage again');
  for (const [index, gate] of catalog.gates.entries()) {
    const result = report.results[index];
    if (result.id !== gate.id || result.status !== 'passed' || result.exitCode !== 0 || result.command !== gate.command || JSON.stringify(result.args) !== JSON.stringify(gate.args) || !Number.isFinite(result.durationMs) || result.durationMs < 0) throw new Error(`Release gate ${gate.id} is missing, changed or failed`);
  }
}

async function main() {
  const [action, profile = 'release', ...rest] = process.argv.slice(2);
  const usage = 'Use release-gates.mjs list [profile] | shards <profile> <count> [--only <id,...>] | run [profile] [--only <id,...>] [--report <path>] [--retry-flaky] [--fail-fast]';
  const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
  const catalog = await loadGateCatalog(root);
  if (action === 'list' && !rest.length) { for (const gate of selectGates(catalog, profile)) console.log(`${gate.id}: ${gate.command} ${gate.args.join(' ')}`); return; }
  // GitHub Actions matrix: one entry per shard, each naming its gates.
  if (action === 'shards') {
    const [count, flag, selection] = rest;
    if (rest.length !== 1 && !(rest.length === 3 && flag === '--only' && selection)) throw new Error(usage);
    const shards = shardGates(selectGates(catalog, profile, selection?.split(',')), Number(count));
    console.log(JSON.stringify({ include: shards.map((gates, index) => ({ shard: `${index + 1}-of-${shards.length}`, gates: gates.join(',') })) }));
    return;
  }
  if (action !== 'run') throw new Error(usage);
  const options = {};
  for (let index = 0; index < rest.length; index += 2) {
    const [flag, value] = [rest[index], rest[index + 1]];
    if (['--retry-flaky', '--fail-fast'].includes(flag) && !(flag.slice(2) in options)) { options[flag.slice(2)] = true; index--; continue; }
    if (!['--only', '--report'].includes(flag) || !value || value.startsWith('--') || flag.slice(2) in options) throw new Error(usage);
    options[flag.slice(2)] = value;
  }
  const only = options.only?.split(',');
  const { digestTree, includeSource } = await import('./environments.mjs');
  // Each snapshot keeps its files' digests, so a run the source changed under names what changed.
  const snapshots = [], snapshot = () => { const files = new Map(); snapshots.push(files); return digestTree(root, includeSource, files); };
  const controller = new AbortController(), cancel = () => controller.abort();
  process.on('SIGINT', cancel); process.on('SIGTERM', cancel);
  let report;
  try {
    const progress = ({ gate, phase, result }) => console.log(`[${gate.id}] ${phase === 'start' ? 'running' : phase === 'retry' ? `${result.status}; running it once more (--retry-flaky)` : result.status === 'flaky' ? 'flaky (passed on retry)' : result.status}`);
    report = await runGateCatalog(root, profile, { only, retryFlaky: !!options['retry-flaky'], failFast: !!options['fail-fast'], snapshot, signal: controller.signal, onProgress: progress });
    if (report.sourceDigest !== report.finalSourceDigest) report.sourceChanged = changedFiles(snapshots[0], snapshots.at(-1));
    const path = resolve(options.report || join(root, '.cache', `gates-${profile}.json`));
    await mkdir(dirname(path), { recursive: true });
    const temporary = `${path}.${randomUUID()}`;
    await writeFile(temporary, JSON.stringify(report, null, 2), { mode: 0o600, flag: 'wx' });
    await rename(temporary, path);
    const flaky = report.results.filter(item => item.status === 'flaky').map(item => item.id);
    console.log(`${report.status}: ${report.results.length} gates${flaky.length ? `, ${flaky.length} flaky (passed on retry): ${flaky.join(', ')}` : ''}${report.sourceChanged ? ', the source changed during the run' : ''}. Receipt: ${path}`);
    // On GitHub Actions a flaky gate is a warning on the run, and every run a job summary.
    if (process.env.GITHUB_ACTIONS === 'true') for (const id of flaky) console.log(`::warning title=Flaky gate ${id}::${id} failed, then passed on retry. It is a race until its cause is found (RELEASE_GATES.md, "Flake detection").`);
    if (process.env.GITHUB_STEP_SUMMARY) await appendFile(process.env.GITHUB_STEP_SUMMARY, summarizeGateReport(report));
    if (report.sourceChanged) console.error(`Source changed during validation; no release receipt admitted. Changed: ${report.sourceChanged.join(', ') || '(the tree digest only)'}`);
    if (report.status !== 'passed') process.exitCode = 1;
  } finally { process.removeListener('SIGINT', cancel); process.removeListener('SIGTERM', cancel); }
}
if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main().catch(error => { console.error(error.message); process.exitCode = 1; });
