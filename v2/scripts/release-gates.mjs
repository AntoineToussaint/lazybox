import { readFile, mkdir, writeFile, rename } from 'node:fs/promises';
import { createHash, randomUUID } from 'node:crypto';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnProcessGroup, stopProcessGroup } from './desktop-dev.mjs';

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
  // Script bodies are part of the policy fingerprint, not just their names.
  const commands = Object.fromEntries([...covered].sort().map(name => [name, scripts[name]]));
  const policyFiles = {};
  for (const path of catalog.policyFiles) {
    if (typeof path !== 'string' || path.startsWith('/') || path.split(/[\\/]/).includes('..')) throw new Error('Gate policy files must stay within source');
    policyFiles[path] = sha(await readFile(join(root, path)));
  }
  return { digest: sha(JSON.stringify({ catalog, commands, policyFiles })), gates, external: catalog.external };
}

export function selectGates(catalog, profile) {
  if (!profiles.has(profile)) throw new Error('Choose release, core, native or ui');
  return catalog.gates.filter(gate => profile === 'release' || gate.scope === profile);
}

// Each gate owns one child process group, including browser/tool descendants.
// The existing launcher cleanup handles termination; no second supervisor here.
export async function executeGate(gate, { root, signal, env = process.env }) {
  if (signal?.aborted) return { status: 'cancelled', exitCode: null };
  // Deterministic qualification never inherits a live Runner/provider opt-in,
  // another source tree, a regression toggle, or a different watcher backend.
  const gateEnv = Object.fromEntries(Object.entries(env).filter(([key]) => !key.startsWith('LAZYBOX_V2_') && !key.startsWith('CHOKIDAR_') && !['TEST_SOURCE', 'TEST_WEBKIT', 'REPRO_OLD_BLUR'].includes(key)));
  const child = spawnProcessGroup(gate.command, gate.args, {
    cwd: root, stdio: 'inherit', env: { ...gateEnv, CARGO_BUILD_JOBS: env.CARGO_BUILD_JOBS || '1', GOMAXPROCS: env.GOMAXPROCS || '1', GOFLAGS: env.GOFLAGS || '-p=1' },
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

export async function runGateCatalog(root, profile, { snapshot, execute = executeGate, signal, onProgress = () => {} } = {}) {
  const catalog = await loadGateCatalog(root), gates = selectGates(catalog, profile);
  const sourceDigest = await snapshot();
  const report = { version: 1, profile, catalogDigest: catalog.digest, sourceDigest, startedAt: new Date().toISOString(), platform: process.platform, architecture: process.arch, status: 'running', results: [] };
  // Run the remaining gates after a failure to give one complete diagnostic.
  // Cancellation leaves explicit not-run records and never a passing receipt.
  for (const gate of gates) {
    const started = performance.now();
    onProgress({ gate, phase: 'start' });
    let result;
    try { result = signal?.aborted ? { status: 'not-run', exitCode: null } : await execute(gate, { root, signal }); }
    catch (error) { result = { status: 'failed', exitCode: null, error: error.message }; }
    report.results.push({ id: gate.id, command: gate.command, args: gate.args, ...result, durationMs: Math.round(performance.now() - started) });
    onProgress({ gate, phase: 'finish', result });
  }
  report.finalSourceDigest = await snapshot();
  report.finishedAt = new Date().toISOString();
  report.status = !signal?.aborted && sourceDigest === report.finalSourceDigest && report.results.every(item => item.status === 'passed' && item.exitCode === 0) ? 'passed' : 'failed';
  return report;
}

export function validateReleaseReceipt(report, catalog, sourceDigest) {
  if (!report || report.version !== 1 || report.profile !== 'release' || report.status !== 'passed' || report.catalogDigest !== catalog.digest || report.sourceDigest !== sourceDigest || report.finalSourceDigest !== sourceDigest || report.results?.length !== catalog.gates.length) throw new Error('Release has no complete passing receipt for this source and current gate catalog; stage again');
  for (const [index, gate] of catalog.gates.entries()) {
    const result = report.results[index];
    if (result.id !== gate.id || result.status !== 'passed' || result.exitCode !== 0 || result.command !== gate.command || JSON.stringify(result.args) !== JSON.stringify(gate.args) || !Number.isFinite(result.durationMs) || result.durationMs < 0) throw new Error(`Release gate ${gate.id} is missing, changed or failed`);
  }
}

async function main() {
  const [action, profile = 'release', ...args] = process.argv.slice(2);
  if (!['list', 'run'].includes(action) || args.length && (args.length !== 2 || args[0] !== '--report')) throw new Error('Use release-gates.mjs list|run [release|core|native|ui] [--report <path>]');
  const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
  const catalog = await loadGateCatalog(root);
  if (action === 'list') { for (const gate of selectGates(catalog, profile)) console.log(`${gate.id}: ${gate.command} ${gate.args.join(' ')}`); return; }
  const { digestTree } = await import('./environments.mjs');
  const controller = new AbortController(), cancel = () => controller.abort();
  process.on('SIGINT', cancel); process.on('SIGTERM', cancel);
  let report;
  try {
    report = await runGateCatalog(root, profile, { snapshot: () => digestTree(root), signal: controller.signal, onProgress: ({ gate, phase, result }) => console.log(`[${gate.id}] ${phase === 'start' ? 'running' : result.status}`) });
    const path = resolve(args[1] || join(root, '.cache', `gates-${profile}.json`));
    await mkdir(dirname(path), { recursive: true });
    const temporary = `${path}.${randomUUID()}`;
    await writeFile(temporary, JSON.stringify(report, null, 2), { mode: 0o600, flag: 'wx' });
    await rename(temporary, path);
    console.log(`${report.status}: ${report.results.length} gates. Receipt: ${path}`);
    if (report.sourceDigest !== report.finalSourceDigest) console.error('Source changed during validation; no release receipt admitted.');
    if (report.status !== 'passed') process.exitCode = 1;
  } finally { process.removeListener('SIGINT', cancel); process.removeListener('SIGTERM', cancel); }
}
if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main().catch(error => { console.error(error.message); process.exitCode = 1; });
