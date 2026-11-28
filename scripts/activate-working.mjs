// Activate a completed candidate from the managed workspace action. The new
// launcher must outlive both this action and its temporary source archive.
import { execFile, spawn } from 'node:child_process';
import { once } from 'node:events';
import { mkdir, mkdtemp, open, readFile, rename, rm, stat, writeFile } from 'node:fs/promises';
import { join, resolve } from 'node:path';
import { promisify } from 'node:util';
import { setTimeout as delay } from 'node:timers/promises';
import { fileURLToPath } from 'node:url';
import { environmentPaths, promote, readValidatedRelease, waitForDesktop } from './environments.mjs';
import { attachClient, controlService, serviceStatus } from './runner-service.mjs';

const exec = promisify(execFile);
async function optionalJSON(path) {
  try { return JSON.parse(await readFile(path, 'utf8')); }
  catch (error) { if (error.code !== 'ENOENT') throw error; }
}
async function stopLauncher(lock) {
  const ownerPath = join(lock, 'owner.json');
  const owner = await optionalJSON(ownerPath);
  if (!owner) return false;
  const { pid } = owner;
  if (!Number.isSafeInteger(pid) || pid <= 0) throw new Error('Invalid Working launcher owner');
  try { process.kill(pid, 0); } catch (error) { if (error.code === 'ESRCH') return false; throw error; }
  // Older Working launchers already handle SIGINT and release their lock.
  // Check the command and PID age before using a persistent owner's PID.
  const { stdout } = await exec('ps', ['-ww', '-p', String(pid), '-o', 'lstart=', '-o', 'command='], { env: { ...process.env, LC_ALL: 'C' } });
  const match = /^(.{24})\s+.*\bnode\s+(?:.*\/)?scripts\/environments\.mjs working$/.exec(stdout.trim());
  if (!match || !(Date.parse(match[1]) <= (await stat(ownerPath)).mtimeMs)) throw new Error('Working launcher no longer matches its owner receipt');
  process.kill(pid, 'SIGINT');
  for (let i = 0; i < 300; i++) {
    if (!(await optionalJSON(ownerPath))) return true;
    await delay(100);
  }
  throw new Error('Timed out stopping the previous Working launcher');
}
async function stopClients(root, paths) {
  // Clients can outlive their original launcher. Match the exact bundle and
  // endpoint, including clients opened from an older pin of this environment.
  const { stdout } = await exec('ps', ['-Aww', '-o', 'pid=', '-o', 'command='], { maxBuffer: 16 * 1024 * 1024 });
  const prefix = join(root, 'releases') + '/';
  const suffix = '/Lazybox.app/Contents/MacOS/lazybox-v2-desktop --endpoint ' + paths.endpoint;
  const stopping = new Set();
  for (const line of stdout.split('\n')) {
    const match = /^\s*(\d+)\s+(.+)$/.exec(line);
    if (!match || !match[2].startsWith(prefix) || !match[2].endsWith(suffix)) continue;
    if (!/^[A-Za-z0-9._-]+$/.test(match[2].slice(prefix.length, -suffix.length))) continue;
    const pid = Number(match[1]);
    try { process.kill(pid, 'SIGTERM'); stopping.add(pid); }
    catch (error) { if (error.code !== 'ESRCH') throw error; }
  }
  for (let i = 0; stopping.size && i < 100; i++) {
    await delay(100);
    for (const pid of stopping) {
      try { process.kill(pid, 0); }
      catch (error) { if (error.code !== 'ESRCH') throw error; stopping.delete(pid); }
    }
  }
  if (stopping.size) throw new Error('Previous Working desktop is still open');
}
async function detached(command, args, cwd, env, log) {
  const fd = await open(log, 'a', 0o600);
  try {
    const child = spawn(command, args, { cwd, env, detached: true, stdio: ['ignore', fd.fd, fd.fd] });
    await once(child, 'spawn');
    child.unref();
    return child;
  } finally { await fd.close(); }
}

async function startWorking(root, paths, source, installed, directory, waitForWindow = true) {
  const log = join(directory, 'launch.log'), receipt = join(directory, 'desktop-ready');
  const env = { ...process.env, LAZYBOX_V2_ENVIRONMENTS: root };
  if (waitForWindow) env.LAZYBOX_WORKING_DESKTOP_READY = receipt;
  let launcher;
  try {
    if (installed) {
      const oldEndpoint = await optionalJSON(paths.endpoint);
      await controlService(root, 'install');
      await controlService(root, 'start');
      const deadline = Date.now() + 180_000;
      for (;;) {
        const endpoint = await optionalJSON(paths.endpoint);
        if (endpoint?.token && endpoint.token !== oldEndpoint?.token) {
          let probed = false;
          try {
            await attachClient(root, { launch: async (binary, args) => {
              probed = true;
              launcher = await detached(binary, args, source, { ...env, LAZYBOX_DEV_DESKTOP_READY: receipt }, log);
            } });
            break;
          } catch (error) { if (probed || Date.now() >= deadline) throw error; }
        }
        if (Date.now() >= deadline) throw new Error('New Working service did not become ready');
        await delay(500);
      }
    } else {
      launcher = await detached(process.execPath, ['scripts/environments.mjs', 'working'], source, env, log);
    }
    if (waitForWindow) await waitForDesktop(receipt, launcher, () => false);
  } catch (error) {
    launcher?.kill('SIGTERM');
    throw error;
  }
}

export async function activateWorking(root, id) {
  const paths = environmentPaths(root, 'working');
  const { source, metadata } = await readValidatedRelease(root, id);
  if (!metadata.revision || metadata.revision !== process.env.LAZYBOX_BUILD_REVISION) throw new Error('Working candidate revision changed before activation');
  const service = await serviceStatus(root);
  const pin = join(paths.directory, 'current.json'), previous = await optionalJSON(pin);
  await mkdir(paths.directory, { recursive: true });
  const directory = await mkdtemp(join(paths.directory, 'update-'));
  console.log('Starting Working from ' + id + '. Launcher output: ' + join(directory, 'launch.log'));
  let stopped = false, promoted = false;
  try {
    if (service.running) {
      await controlService(root, 'stop');
      stopped = true;
    }
    if (!service.installed) stopped = await stopLauncher(paths.lock);
    await stopClients(root, paths);
    await promote(root, id);
    promoted = true;
    await startWorking(root, paths, source, service.installed, directory);
  } catch (error) {
    try {
      if (promoted) {
        if (service.installed && (await serviceStatus(root)).running) await controlService(root, 'stop');
        if (!service.installed) await stopLauncher(paths.lock);
        await stopClients(root, paths);
        if (previous) {
          const restore = join(directory, 'previous.json');
          await writeFile(restore, JSON.stringify(previous), { mode: 0o600 });
          await rename(restore, pin);
        } else await rm(pin, { force: true });
      }
      if (previous && stopped) await startWorking(root, paths, source, service.installed, directory, false);
      else if (previous && promoted && service.installed) await controlService(root, 'install');
    } catch (rollback) {
      throw new Error(`Working activation failed: ${error.message}. Recovery failed: ${rollback.message}. See ${directory}`);
    }
    throw new Error(`Working activation failed: ${error.message}. See ${directory}`, { cause: error });
  }
}
if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  activateWorking(process.env.LAZYBOX_V2_ENVIRONMENTS, process.argv[2]).catch(error => { console.error(error.message); process.exitCode = 1; });
}
