// Independent native applications and Runner state, with explicit promotion of
// frozen source/build snapshots. This is the local macOS dogfooding launcher.
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { cp, mkdir, readFile, writeFile, rename, rm, readdir, lstat, realpath } from 'node:fs/promises';
import { resolve, dirname, join, relative, isAbsolute } from 'node:path';
import { fileURLToPath } from 'node:url';
import { randomUUID, createHash } from 'node:crypto';
import { runnerArguments, runnerLaunchEnvironment } from './runner-command.mjs';
import { registerRunner, userActionsFile } from './user-config.mjs';
import { loadGateCatalog, validateReleaseReceipt } from './release-gates.mjs';
import { startDesktopDevServer, desktopDevConfig, stopProcessGroup, spawnProcessGroup } from './desktop-dev.mjs';

export function environmentPaths(root, channel) {
  if (!['dev', 'working'].includes(channel)) throw new Error('Choose dev or working');
  const directory = resolve(root, channel);
  return { directory, state: join(directory, 'state'), config: join(directory, 'runner.config.json'), endpoint: join(directory, 'state', 'endpoint.json'), lock: join(directory, 'launch.lock'), identifier: channel === 'dev' ? 'ai.lazybox.v2.dev' : 'ai.lazybox.v2.working', productName: channel === 'dev' ? 'Lazybox Dev' : 'Lazybox' };
}
export function releasePath(root, id) {
  if (!/^[A-Za-z0-9][A-Za-z0-9._-]{0,99}$/.test(id)) throw new Error('Invalid release ID');
  return resolve(root, 'releases', id);
}
export function validateEnvironmentRoot(source, directory) {
  const rel = relative(resolve(source), resolve(directory));
  if (rel === '' || (!rel.startsWith('../') && rel !== '..' && !rel.startsWith('/') && includeSource(rel))) {
    throw new Error('Environment storage must be outside source or inside an excluded build/state directory such as .cache');
  }
}
const excluded = new Set(['.cache', '.codefly', 'node_modules', 'target', 'dist', 'build', 'bin', 'coverage', '.expo', 'playwright-report', 'test-results']);
export function includeSource(relativePath) {
  return !relativePath.split(/[\\/]/).some(part => excluded.has(part) || part === 'runner.config.json' || part === '.env' || part.startsWith('.env.') && part !== '.env.example' || part.endsWith('.log') || part.endsWith('.tsbuildinfo'));
}
// `files`, when given, receives each file's own digest, so a caller can name what changed between two snapshots.
export async function digestTree(root, filter = includeSource, files) {
  const hash = createHash('sha256');
  const walk = async path => {
    for (const name of (await readdir(path)).sort()) {
      const full = join(path, name), rel = relative(root, full);
      if (!filter(rel)) continue;
      const stat = await lstat(full);
      if (stat.isSymbolicLink()) throw new Error(`Source snapshot contains a symlink: ${rel}`);
      if (stat.isDirectory()) await walk(full);
      else if (stat.isFile()) { const header = JSON.stringify([rel, stat.size, stat.mode & 0o111]), data = await readFile(full); hash.update(header); hash.update(data); files?.set(rel, createHash('sha256').update(header).update(data).digest('hex')); }
    }
  };
  await walk(root); return hash.digest('hex');
}
// Walk explicitly: the default release root is beneath .cache in the source
// tree. Exclude that directory before descending, and never follow symlinks.
export async function copySource(source, destination) {
  await mkdir(destination, { recursive: false });
  const walk = async (from, to) => {
    for (const name of await readdir(from)) {
      const path = join(from, name), rel = relative(source, path);
      if (!includeSource(rel)) continue;
      const stat = await lstat(path);
      if (stat.isSymbolicLink()) throw new Error(`Refusing to snapshot source symlink: ${rel}`);
      if (stat.isDirectory()) { await mkdir(join(to, name)); await walk(path, join(to, name)); }
      else if (stat.isFile()) await cp(path, join(to, name), { errorOnExist: true, force: false });
      else throw new Error(`Refusing non-file source: ${rel}`);
    }
  };
  await walk(source, destination);
}
// A Working snapshot runs without source watchers. Normalize before hashing
// and validating the candidate, never by modifying a running installation.
export async function freezeRunnerReload(source) {
  const path = join(source, 'services/runner/service.codefly.yaml');
  const config = await readFile(path, 'utf8');
  if ((config.match(/^    hot-reload: (true|false)$/gm) || []).length !== 1) throw new Error('Expected one explicit Codefly hot-reload setting');
  await writeFile(path, config.replace(/^    hot-reload: true$/m, '    hot-reload: false'));
}
const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const buildEnv = { ...process.env, CARGO_BUILD_JOBS: process.env.CARGO_BUILD_JOBS || '1', GOMAXPROCS: process.env.GOMAXPROCS || '1', GOFLAGS: process.env.GOFLAGS || '-p=1' };
async function run(command, args, cwd, env = buildEnv) {
  const child = spawn(command, args, { cwd, env, stdio: 'inherit' });
  const [code, signal] = await once(child, 'exit');
  if (code !== 0) throw new Error(`${command} failed (${signal || code})`);
}
async function buildSnippetAdapter(source) {
  // The helper (tools/lazybox-snippets) links V1's config library at a pinned
  // lazybox-v1 revision; no duplicated paths or YAML semantics.
  const target = join(source, '.cache', 'snippet-adapter');
  await run('cargo', ['build', '--locked', '--manifest-path', join(source, 'tools/lazybox-snippets/Cargo.toml')], source, {...buildEnv, CARGO_TARGET_DIR:target});
  return join(target, 'debug', 'lazybox-snippets');
}
// The CLI agents call (`lazybox run`, `lazybox link`) is built from the same
// source as the Runner. It is built beside its final path and renamed over
// it, so an agent never executes a half-written binary.
const goBuildCLI = (source, output) => run('go', ['build', '-o', output, './cmd/lazybox'], join(source, 'services/runner/code'));
export async function buildCLI(source, directory, build = goBuildCLI) {
  await mkdir(directory, { recursive: true });
  const temporary = join(directory, `.lazybox-${randomUUID()}`);
  try {
    await build(source, temporary);
    await rename(temporary, join(directory, 'lazybox'));
  } finally {
    await rm(temporary, { force: true });
  }
  return directory;
}
// What the CLI is built from: the Runner module's Go files (tests aside) and
// its go.mod/go.sum. Codefly rebuilds the Runner when they change.
export function runnerSourceFile(path) {
  return (path.endsWith('.go') && !path.endsWith('_test.go')) || /(^|\/)go\.(mod|sum)$/.test(path);
}
export async function runnerSourceFingerprint(source) {
  const code = join(source, 'services/runner/code'), hash = createHash('sha256');
  const walk = async path => {
    for (const entry of (await readdir(path, { withFileTypes: true })).sort((a, b) => a.name < b.name ? -1 : 1)) {
      if (entry.name.startsWith('.') || entry.name === 'testdata' || entry.name === 'node_modules') continue;
      const full = join(path, entry.name), rel = relative(code, full);
      if (entry.isDirectory()) await walk(full);
      else if (entry.isFile() && runnerSourceFile(rel)) {
        try { const stat = await lstat(full); hash.update(JSON.stringify([rel, stat.size, stat.mtimeMs])); } catch (error) { if (error.code !== 'ENOENT') throw error; }
      }
    }
  };
  await walk(code); return hash.digest('hex');
}
// Dev builds the CLI at launch, and Codefly rebuilds and restarts the Runner
// on every Go change: rebuild the CLI with it, or agents run a CLI older than
// their Runner. The source is polled (directory fs.watch is unreliable on
// macOS, ui-watch.mjs); a change is built once it has settled for debounce
// ms. A failed build keeps the previous CLI and is said; it never stops Dev.
export function watchCLI({ source, directory, since, build = goBuildCLI, interval = 1000, debounce = 1500, log = console.log }) {
  let last = since, changedAt = 0, pending = false, stopped = false, timer, current = Promise.resolve();
  const tick = async () => {
    try {
      const now = await runnerSourceFingerprint(source);
      if (last === undefined) last = now;
      else if (now !== last) { last = now; changedAt = Date.now(); pending = true; }
      if (pending && !stopped && Date.now() - changedAt >= debounce) {
        pending = false;
        await buildCLI(source, directory, build);
        log('Rebuilt the lazybox CLI agents run, from the Runner source that changed.');
      }
    } catch (error) {
      log(`The lazybox CLI agents run was not rebuilt; the previous one stays: ${error.message}`);
    }
  };
  const loop = () => { if (!stopped) timer = setTimeout(() => { current = tick().finally(loop); }, interval); };
  loop();
  return { async close() { stopped = true; clearTimeout(timer); await current; } };
}
async function buildDesktop(source, paths, target) {
  await run('npm', ['run', 'build:ui', '-w', '@lazybox/desktop'], source);
  await run(join(source, 'node_modules/.bin/tauri'), ['build', '--debug', '--bundles', 'app', '--config', JSON.stringify({ identifier: paths.identifier, productName: paths.productName })], join(source, 'apps/desktop'), { ...buildEnv, CARGO_TARGET_DIR: target });
  return join(target, 'debug/bundle/macos', paths.productName + '.app');
}
async function saveJSON(path, value) {
  await mkdir(dirname(path), { recursive: true }); const temp = path + '.' + randomUUID();
  await writeFile(temp, JSON.stringify(value, null, 2), { mode: 0o600, flag: 'wx' });
  await rename(temp, path);
}

async function enrolledProfiles(state) {
  try {
    const path = join(state, 'accounts', 'profiles.json');
    const info = await lstat(path);
    if (!info.isFile() || info.size > 1 << 20) throw new Error('Invalid managed subscription registry');
    const profiles = JSON.parse(await readFile(path, 'utf8'));
    if (!Array.isArray(profiles) || profiles.some(p => !p.id || !['claude', 'codex'].includes(p.provider) || !isAbsolute(p.home || ''))) throw new Error('Invalid managed subscription registry');
    return profiles;
  } catch (error) { if (error.code === 'ENOENT') return []; throw error; }
}

export async function sharedSubscriptionState(environmentRoot) {
  let value;
  try { value = JSON.parse(await readFile(join(environmentRoot, 'subscriptions.json'), 'utf8')); }
  catch (error) { if (error.code === 'ENOENT') return undefined; throw error; }
  if (value.version !== 1 || !isAbsolute(value.state || '')) throw new Error('Invalid shared subscription store selection');
  return realpath(value.state);
}

export async function checkSubscriptionSelection(environmentRoot, state) {
  for (const channel of ['dev', 'working']) {
    const local = environmentPaths(environmentRoot, channel).state;
    if ((await enrolledProfiles(local)).length && await realpath(local) !== state) {
      throw new Error(`${channel} already has private subscriptions; merge them explicitly before selecting a shared store`);
    }
  }
}

// Select existing managed storage without copying credentials or relocating
// provider homes. Native login storage can be tied to its exact directory.
export async function shareSubscriptions(environmentRoot, state) {
  if (!isAbsolute(state || '')) throw new Error('Supply an absolute existing subscription state directory');
  const canonical = await realpath(state);
  const profiles = await enrolledProfiles(canonical);
  if (!profiles.length) throw new Error('The selected state contains no managed subscriptions');
  const unlock = await acquireLock(join(environmentRoot, 'subscriptions.lock'));
  try {
    const previous = await sharedSubscriptionState(environmentRoot);
    if (previous && previous !== canonical) throw new Error('A different shared subscription store is already selected');
    await checkSubscriptionSelection(environmentRoot, canonical);
    await saveJSON(join(environmentRoot, 'subscriptions.json'), { version: 1, state: canonical });
    console.log(`Working and Dev will use the same ${profiles.length} saved subscriptions on their next launch. Credentials were not copied or moved.`);
  } finally { await unlock(); }
}
export async function acquireLock(path) {
  await mkdir(dirname(path), { recursive: true });
  try { await mkdir(path); }
  catch (error) {
    if (error.code !== 'EEXIST') throw error;
    let pid;
    try { pid = JSON.parse(await readFile(join(path, 'owner.json'), 'utf8')).pid; }
    catch { throw new Error(`Environment lock is incomplete; inspect ${path}`); }
    if (!Number.isSafeInteger(pid) || pid <= 0) throw new Error('Invalid environment owner');
    try { process.kill(pid, 0); throw new Error('This environment is already running or building. Stop its launcher before updating it.'); }
    catch (error) { if (error.code !== 'ESRCH') throw error; }
    await rm(path, { recursive: true }); await mkdir(path);
  }
  await writeFile(join(path, 'owner.json'), JSON.stringify({ pid: process.pid }), { flag: 'wx', mode: 0o600 });
  return () => rm(path, { recursive: true });
}
async function stage(environmentRoot) {
  const unlock = await acquireLock(join(environmentRoot, 'stage.lock'));
  try {
    await stageCandidate(environmentRoot);
  } finally { await unlock(); }
}
async function stageCandidate(environmentRoot) {
  const id = new Date().toISOString().replaceAll(/[:.]/g, '-') + '-' + randomUUID().slice(0, 8);
  const destination = releasePath(environmentRoot, id), source = join(destination, 'source');
  await mkdir(destination, { recursive: true });
  console.log(`Staging ${id}; current working installation is unchanged.`);
  const snippetAdapter=await buildSnippetAdapter(root);
  await copySource(root, source);
  await mkdir(join(source,'resources'),{recursive:true});
  await cp(snippetAdapter,join(source,'resources','lazybox-snippets'));
  await buildCLI(source, join(source, 'resources', 'bin'));
  await freezeRunnerReload(source);
  const before = await digestTree(source);
  await run('npm', ['ci', '--ignore-scripts', '--no-audit', '--no-fund'], source);
  const receiptPath = join(destination, 'gates.json');
  await run(process.execPath, ['scripts/release-gates.mjs', 'run', 'release', '--report', receiptPath], source, { ...buildEnv, CARGO_TARGET_DIR: join(environmentRoot, 'build', 'working') });
  const gateReceipt = JSON.parse(await readFile(receiptPath, 'utf8'));
  validateReleaseReceipt(gateReceipt, await loadGateCatalog(source), before);
  const app = await buildDesktop(source, environmentPaths(environmentRoot, 'working'), join(environmentRoot, 'build', 'working'));
  // The phone app the Runner's device gateway serves (REMOTE.md).
  await run('npm', ['run', 'build:web'], source);
  await cp(app, join(destination, 'Lazybox.app'), { recursive: true, force: false, errorOnExist: true });
  const after = await digestTree(source);
  if (before !== after) throw new Error('Validation modified the source snapshot; stage again after reviewing generated changes');
  await saveJSON(join(destination, 'release.json'), { version: 2, id, sourceDigest: after, appDigest: await digestTree(join(destination, 'Lazybox.app'), () => true), workerDigest: await digestTree(join(source, 'workers/claude-sdk/dist'), () => true), validatedAt: new Date().toISOString(), gateReceipt, liveGitHubVerified: false });
  console.log(`Candidate ready: ${id}\nPromote explicitly with: make promote RELEASE=${id}`);
}
// Promotion and both launch paths share artifact validation. A mutable compiled
// worker is execution code too, even though dist is excluded from source hashes.
export async function readValidatedRelease(environmentRoot, id) {
  const directory = releasePath(environmentRoot, id);
  const source = join(directory, 'source'), app = join(directory, 'Lazybox.app');
  const metadata = JSON.parse(await readFile(join(directory, 'release.json'), 'utf8'));
  if (![1, 2].includes(metadata.version) || metadata.id !== id || await digestTree(source) !== metadata.sourceDigest || await digestTree(app, () => true) !== metadata.appDigest) throw new Error('Release is incomplete or changed after validation; stage again');
  if (metadata.version === 2) {
    validateReleaseReceipt(metadata.gateReceipt, await loadGateCatalog(source), metadata.sourceDigest);
    if (metadata.workerDigest !== await digestTree(join(source, 'workers/claude-sdk/dist'), () => true)) throw new Error('Compiled worker changed after validation; stage again');
  }
  return { metadata, source, app };
}
export async function promote(environmentRoot, id) {
  const { metadata } = await readValidatedRelease(environmentRoot, id);
  if (metadata.version !== 2) throw new Error('Release uses legacy validation; stage again');
  validateReleaseReceipt(metadata.gateReceipt, await loadGateCatalog(root), metadata.sourceDigest);
  const currentPath = join(environmentRoot, 'working', 'current.json');
  let previous; try { previous = JSON.parse(await readFile(currentPath, 'utf8')).id; } catch (error) { if (error.code !== 'ENOENT') throw error; }
  await saveJSON(currentPath, { id, previous, promotedAt: new Date().toISOString() });
  console.log(`Working release pinned to ${id}. No processes, credentials, or databases were changed. Restart working explicitly to adopt it.`);
}
// The Runner's restart contract (services/runner/code/internal/restart): before
// it exits for a restart the person asked for in the app, it records a request
// here. Its presence after an exit, and only then, means "start it again".
export function restartRequestPath(paths) { return join(paths.directory, 'restart-request.json'); }
export async function takeRestartRequest(path) {
  let data;
  try { data = await readFile(path, 'utf8'); }
  catch (error) { if (error.code === 'ENOENT') return false; throw error; }
  await rm(path, { force: true });
  try { return JSON.parse(data).version === 1; } catch { return false; }
}
// Keep one Runner serving for the life of the environment. start() launches a
// Runner and resolves once it serves ({ finished }), or undefined when the
// environment is stopping. A requested restart starts the next Runner without
// touching the desktop or watchers; any other exit ends the environment as
// before: a failure throws, a clean exit returns.
export async function superviseRunner({ start, restartRequest, stopping, onReady = () => {}, log = console.log }) {
  for (let restarts = 0; ; restarts++) {
    // A request left by an earlier launch must never turn a later crash into a restart.
    await rm(restartRequest, { force: true });
    const run = await start();
    if (!run) return;
    if (restarts === 0) await onReady(); else log('Runner restarted. Agents, commands and terminals were re-attached.');
    const result = await run.finished;
    if (stopping()) return;
    if (await takeRestartRequest(restartRequest)) {
      log('Restart requested from the app: starting the Runner again. Agents, commands and terminals keep running.');
      continue;
    }
    if (result.code !== 0) throw result.failure || new Error(`Runner exited (${result.signal || result.code})`);
    return;
  }
}
async function launch(environmentRoot, channel) {
  const paths = environmentPaths(environmentRoot, channel);
  const unlock = await acquireLock(paths.lock);
  let runner, desktop, worker, ui, cliWatch, finished, stopping = false;
  const stop = () => { if (stopping) return; stopping = true; runner?.kill('SIGINT'); };
  try {
    const subscriptionState = await sharedSubscriptionState(environmentRoot);
    if (subscriptionState) await checkSubscriptionSelection(environmentRoot, subscriptionState);
    let source = root, app, snippetAdapter, cliDir;
    if (channel === 'working') {
      const { serviceStatus } = await import('./runner-service.mjs');
      if ((await serviceStatus(environmentRoot)).installed) throw new Error('Working is managed by Codefly. Use make working-client to attach, or runner-service-stop/uninstall before using the terminal launcher.');
      let id; try { ({ id } = JSON.parse(await readFile(join(paths.directory, 'current.json'), 'utf8'))); }
      catch { throw new Error('No working release is pinned. Run make stage, then make promote RELEASE=<id>.'); }
      ({ source, app } = await readValidatedRelease(environmentRoot, id));
      snippetAdapter=join(source,'resources','lazybox-snippets');
      cliDir=join(source,'resources','bin');
    } else {
      snippetAdapter=await buildSnippetAdapter(source);
      const cliSource = await runnerSourceFingerprint(source);
      cliDir=await buildCLI(source, join(paths.directory, 'bin'));
      cliWatch = watchCLI({ source, directory: cliDir, since: cliSource });
      await run('make', ['deps', 'build-worker', 'build-web'], source);
      ui = await startDesktopDevServer(source);
      worker = spawn(join(source, 'node_modules/.bin/tsc'), ['-p', 'workers/claude-sdk', '--watch', '--preserveWatchOutput'], { cwd: source, env: buildEnv, stdio: 'inherit', detached: true });
      worker.on('error', error => { console.error(`Worker watcher failed: ${error.message}`); stop(); });
    }
    await mkdir(paths.state, { recursive: true });
    try { await writeFile(paths.config, JSON.stringify({ profiles: [] }, null, 2), { flag: 'wx', mode: 0o600 }); } catch (error) { if (error.code !== 'EEXIST') throw error; }
    // Both roots are resolved from the selected environment. Codefly allocates
    // independent ports; neither channel borrows another channel's endpoint.
    const restartRequest = restartRequestPath(paths);
    const args = runnerArguments({ paths, source, snippetAdapter, snippetDirectory: source, subscriptionState, cliDir, restartRequest, actionsFile: userActionsFile() });
    const runnerEnv = runnerLaunchEnvironment(buildEnv);
    process.on('SIGINT', stop); process.on('SIGTERM', stop);
    // One Runner process: spawn it and wait until its new endpoint answers Info.
    const start = async () => {
      let previousToken = ''; try { previousToken = JSON.parse(await readFile(paths.endpoint, 'utf8')).token; } catch { /* first launch */ }
      runner = spawnProcessGroup('codefly', args, { cwd: source, env: runnerEnv, stdio: 'inherit' });
      let exited = false, failure;
      finished = new Promise(resolve => {
        runner.once('exit', (code, signal) => { exited = true; resolve({ code, signal, failure }); });
        runner.once('error', error => { failure = error; exited = true; resolve({ code: 1, failure }); });
      });
      if (stopping) runner.kill('SIGINT');
      const deadline = Date.now() + 180_000;
      let ready = false;
      while (Date.now() < deadline && !exited && !stopping) {
        try {
          const endpoint = JSON.parse(await readFile(paths.endpoint, 'utf8'));
          if (endpoint.token !== previousToken) {
            const response = await fetch(endpoint.url + '/lazybox.bench.v1.RunnerService/Info', { method: 'POST', headers: { Authorization: `Bearer ${endpoint.token}`, 'Content-Type': 'application/json', 'Connect-Protocol-Version': '1' }, body: '{}', signal: AbortSignal.timeout(1000) });
            if (response.ok) {
              const info = await response.json();
              if (subscriptionState && !info.sharedSubscriptions) { failure = new Error('This release does not support the selected shared subscription store. Stage and promote an updated release.'); break; }
              ready = true; break;
            }
          }
        } catch { /* startup is not yet ready */ }
        await new Promise(resolve => setTimeout(resolve, 300));
      }
      if (stopping) return undefined;
      if (!ready) throw failure || new Error('Runner did not become ready; inspect its startup output');
      return { finished };
    };
    // The desktop and watchers start once and outlive any requested restart.
    const onReady = () => {
      // `lazybox` in any terminal finds this Runner (lazybox runners / --runner).
      registerRunner(channel, paths).catch(error => console.error(`The lazybox CLI will not find this Runner by name: ${error.message}`));
      desktop =channel === 'dev'
        ? spawn(join(source, 'node_modules/.bin/tauri'), ['dev', '--config', JSON.stringify(desktopDevConfig(paths, ui.url, ui.nonce)), '--', '--', '--endpoint', paths.endpoint], { cwd: join(source, 'apps/desktop'), env: { ...buildEnv, CARGO_TARGET_DIR: join(environmentRoot, 'build', 'dev') }, stdio: 'inherit', detached: true })
        : spawn(join(app, 'Contents/MacOS/lazybox-v2-desktop'), ['--endpoint', paths.endpoint], { env: buildEnv, stdio: 'inherit' });
      desktop.on('error', error => console.error(`Desktop launch failed: ${error.message}`));
      console.log(`${paths.productName} is running. State: ${paths.state}\nClosing the desktop leaves this Runner alive. Ctrl+C here stops this environment's Runner; Restart Runner in the app starts it again here.`);
      if (ui) console.log(`Auto reload: Vite refreshes the UI (${ui.watchBackend} watcher), Tauri rebuilds Rust, Codefly rebuilds/restarts Go, and the lazybox CLI agents run is rebuilt with it. A Runner restart does not stop Dev agents or their managed commands: the restarted Runner re-attaches to their session hosts.`);
    };
    await superviseRunner({ start, restartRequest, stopping: () => stopping, onReady });
  } finally {
    await Promise.all([stopProcessGroup(channel === 'dev' ? desktop : undefined), stopProcessGroup(worker), cliWatch?.close()]);
    await ui?.server.close();
    runner?.kill('SIGINT');
    if (finished) await finished;
    process.removeListener('SIGINT', stop); process.removeListener('SIGTERM', stop); await unlock();
  }
}
async function main() {
  if (process.platform !== 'darwin') throw new Error('The channel launcher currently supports macOS');
  const environmentRoot = resolve(process.env.LAZYBOX_V2_ENVIRONMENTS || join(root, '.cache', 'environments'));
  await mkdir(environmentRoot, { recursive: true });
  const canonical = await realpath(environmentRoot);
  validateEnvironmentRoot(root, canonical);
  const command = process.argv[2];
  if (command === 'stage') return stage(canonical);
  if (command === 'promote') return promote(canonical, process.argv[3] || '');
  if (command === 'share-subscriptions') return shareSubscriptions(canonical, process.argv[3] || '');
  if (['dev', 'working'].includes(command)) return launch(canonical, command);
  if (command === 'status') {
    for (const channel of ['working', 'dev']) { const paths = environmentPaths(canonical, channel); let release = channel === 'dev' ? 'current source' : 'not pinned'; if (channel === 'working') { try { release = JSON.parse(await readFile(join(paths.directory, 'current.json'), 'utf8')).id; } catch {} } console.log(`${channel}: ${release}\n  state: ${paths.state}\n  app: ${paths.identifier}`); } return;
  }
  throw new Error('Use environments.mjs dev|working|stage|promote <release>|share-subscriptions <state>|status');
}
if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main().catch(error => { console.error(error.message); process.exitCode = 1; });
