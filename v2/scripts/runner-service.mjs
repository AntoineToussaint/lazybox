// Product configuration for Codefly's native service owner. No launchd/systemd
// definitions, PID registry, restart loop or copied credential store lives here.
import { execFile, spawn } from 'node:child_process';
import { promisify } from 'node:util';
import { access, mkdir, readFile, realpath, writeFile } from 'node:fs/promises';
import { constants } from 'node:fs';
import { createHash } from 'node:crypto';
import { delimiter, dirname, isAbsolute, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { runnerArguments } from './runner-command.mjs';
import { acquireLock, checkSubscriptionSelection, digestTree, environmentPaths, readValidatedRelease, sharedSubscriptionState, validateEnvironmentRoot } from './environments.mjs';

const execute = promisify(execFile);
const sourceRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const hash = value => createHash('sha256').update(value).digest('hex');
const publicStatus = status => ({ label: status.ref.label, version: status.version, state: status.state, installed: status.installed, running: status.running, readiness: 'not-probed' });

export function serviceLabel(environmentRoot) {
  return `ai.lazybox.v2.working.${hash(resolve(environmentRoot)).slice(0, 16)}`;
}

export async function findCodefly(env = process.env) {
  const explicit = env.LAZYBOX_V2_CODEFLY;
  if (explicit && !isAbsolute(explicit)) throw new Error('LAZYBOX_V2_CODEFLY must be an absolute executable path');
  const candidates = explicit ? [explicit] : (env.PATH || '').split(delimiter).filter(isAbsolute).map(path => join(path, 'codefly'));
  for (const path of candidates) {
    try { await access(path, constants.X_OK); return await realpath(path); }
    catch (error) { if (!['ENOENT', 'EACCES', 'ENOTDIR'].includes(error.code)) throw error; }
  }
  throw new Error('Codefly is required; install a CLI with native `codefly service` support or set LAZYBOX_V2_CODEFLY');
}

async function invoke(executable, args) {
  // Capture typed output. Never scrape native supervisor output or forward logs
  // into a public plan/status response. Codefly remains the lifecycle authority.
  const { stdout } = await execute(executable, args, { timeout: 60_000, maxBuffer: 4 * 1024 * 1024 });
  try { return JSON.parse(stdout); }
  catch { throw new Error('Codefly returned an invalid service response; a CLI with `service --json` is required'); }
}

export async function serviceStatus(environmentRoot, { executable, call = invoke } = {}) {
  const binary = executable || await findCodefly();
  const label = serviceLabel(environmentRoot);
  const status = await call(binary, ['service', 'status', label, '--json']);
  if (status.ref?.label !== label || typeof status.installed !== 'boolean' || typeof status.running !== 'boolean') {
    throw new Error('Codefly service status does not match this environment');
  }
  return status;
}

export async function servicePlan(environmentRoot, { env = process.env, executable } = {}) {
  const paths = environmentPaths(environmentRoot, 'working');
  const { id } = JSON.parse(await readFile(join(paths.directory, 'current.json'), 'utf8'));
  const { metadata, source, app } = await readValidatedRelease(environmentRoot, id);
  const service = await readFile(join(source, 'services/runner/service.codefly.yaml'), 'utf8');
  if ((service.match(/^    hot-reload: (true|false)$/gm) || []).length !== 1 || !/^    hot-reload: false$/m.test(service)) {
    throw new Error('A managed Working Runner requires frozen Codefly hot reload');
  }
  const binary = executable || await findCodefly(env);
  const worker = join(source, 'workers/claude-sdk/dist/main.js');
  const snippets = join(source, 'resources/lazybox-snippets');
  await access(worker, constants.R_OK);
  await access(snippets, constants.X_OK);
  const subscriptions = await sharedSubscriptionState(environmentRoot);
  if (subscriptions) await checkSubscriptionSelection(environmentRoot, subscriptions);
  const args = runnerArguments({ paths, source, snippetAdapter: snippets, snippetDirectory: source, subscriptionState: subscriptions });
  // Only these explicitly public values reach the durable service definition.
  // Login tokens and the private endpoint's bearer never become arguments/env.
  const publicEnv = { PATH: (env.PATH || '').split(delimiter).filter(isAbsolute).join(delimiter), GOMAXPROCS: '1', GOFLAGS: '-p=1' };
  if (!publicEnv.PATH) throw new Error('An absolute toolchain PATH is required');
  const contract = { executable: binary, executableDigest: hash(await readFile(binary)), source, args, publicEnv, restart: 'never', startAtLogin: false, sourceDigest: metadata.sourceDigest, appDigest: metadata.appDigest, workerDigest: await digestTree(dirname(worker), () => true) };
  const version = hash(JSON.stringify(contract));
  return { label: serviceLabel(environmentRoot), version, release: id, paths, app, ...contract };
}

export async function controlService(environmentRoot, action, options = {}) {
  if (!['install', 'start', 'stop', 'status', 'uninstall'].includes(action)) throw new Error('Unknown Runner service action');
  const executable = options.executable || await findCodefly(options.env);
  const call = options.call || invoke;
  const paths = environmentPaths(environmentRoot, 'working');
  const unlock = action === 'status' ? async () => {} : await acquireLock(paths.lock);
  try {
    const status = await serviceStatus(environmentRoot, { executable, call });
    if (action === 'status') return publicStatus(status);
    // Recovery commands must work even if the selected release no longer exists.
    if (action === 'stop' || action === 'uninstall') {
      if (!status.installed) return { label: status.ref.label, state: 'not-installed' };
      if (action === 'uninstall' && status.running) throw new Error('Stop the Runner service before uninstalling it');
      const result = await call(executable, ['service', action, status.ref.label, ...(action === 'uninstall' && status.version ? ['--version', status.version] : []), '--json']);
      return action === 'stop' ? publicStatus(result) : result;
    }
    const plan = await servicePlan(environmentRoot, { ...options, executable });
    if (action === 'install') {
      if (status.running) throw new Error('Stop the Runner service before changing its installation');
      await mkdir(paths.state, { recursive: true, mode: 0o700 });
      try { await writeFile(paths.config, JSON.stringify({ profiles: [] }), { flag: 'wx', mode: 0o600 }); }
      catch (error) { if (error.code !== 'EEXIST') throw error; }
      return await call(executable, ['service', 'install', plan.label, '--version', plan.version, '--executable', executable,
        '--working-directory', plan.source, '--restart', plan.restart, '--start-at-login=false',
        ...plan.args.map(value => `--public-arg=${value}`),
        ...Object.entries(plan.publicEnv).map(([key, value]) => `--public-env=${key}=${value}`), '--json']);
    }
    if (!status.installed || status.version !== plan.version) throw new Error('Install the selected Working release before starting it; promotion does not update a service');
    return publicStatus(await call(executable, ['service', 'start', plan.label, '--json']));
  } finally { await unlock(); }
}

export async function attachClient(environmentRoot, options = {}) {
  const plan = await servicePlan(environmentRoot, options);
  const status = await serviceStatus(environmentRoot, { executable: plan.executable, call: options.call });
  if (!status.running || status.version !== plan.version) throw new Error('Start the installed, selected Working release before opening its client');
  // Native supervisor health without a probe is not Runner readiness. Check the
  // private locator through the product's authenticated Info boundary instead.
  const endpoint = JSON.parse(await readFile(plan.paths.endpoint, 'utf8'));
  const url = new URL(endpoint.url);
  if (url.protocol !== 'http:' || !['localhost', '127.0.0.1', '[::1]'].includes(url.hostname) || url.username || url.password) throw new Error('Expected a local Runner endpoint');
  const response = await (options.fetch || fetch)(new URL('/lazybox.bench.v1.RunnerService/Info', url), {
    method: 'POST', headers: { Authorization: `Bearer ${endpoint.token}`, 'Content-Type': 'application/json', 'Connect-Protocol-Version': '1' }, body: '{}', signal: AbortSignal.timeout(5000), redirect: 'error',
  });
  if (!response.ok) throw new Error('Runner is not ready; inspect its Codefly service status and retry');
  const info = await response.json();
  if (plan.args.some(value => value.startsWith('runner:LAZYBOX_V2_SUBSCRIPTION_STATE=')) && !info.sharedSubscriptions) throw new Error('Runner does not support the selected shared subscription store');
  const launch = options.launch || ((command, args) => new Promise((resolve, reject) => {
    const child = spawn(command, args, { stdio: 'inherit' });
    child.once('error', reject);
    child.once('exit', (code, signal) => code === 0 ? resolve() : reject(new Error(`Desktop exited (${signal || code})`)));
  }));
  // Closing a client has no service lifecycle action.
  return launch(join(plan.app, 'Contents/MacOS/lazybox-v2-desktop'), ['--endpoint', plan.paths.endpoint]);
}

async function main() {
  const command = process.argv[2];
  if (!['plan', 'install', 'start', 'stop', 'status', 'uninstall', 'client'].includes(command)) throw new Error('Use runner-service.mjs plan|install|start|stop|status|uninstall|client');
  // Current frozen release contains a macOS app. Linux packaging is a separate
  // roadmap gate, despite Codefly itself supporting both native supervisors.
  if (process.platform !== 'darwin') throw new Error('The current Working release packaging supports macOS');
  const root = resolve(process.env.LAZYBOX_V2_ENVIRONMENTS || join(sourceRoot, '.cache/environments'));
  const canonical = await realpath(root);
  validateEnvironmentRoot(sourceRoot, canonical);
  if (command === 'client') return attachClient(canonical);
  console.log(JSON.stringify(command === 'plan' ? await servicePlan(canonical) : await controlService(canonical, command), null, 2));
}
if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main().catch(error => { console.error(error.message); process.exitCode = 1; });
