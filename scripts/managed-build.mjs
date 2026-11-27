// Entry point for the Rebuild Dev and Stage Working workspace actions.
import { execFile, spawn } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { once } from 'node:events';
import { mkdir, mkdtemp, open, readFile, realpath, rm, writeFile } from 'node:fs/promises';
import { isAbsolute, join, resolve } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { fileURLToPath } from 'node:url';
import { promisify } from 'node:util';

const exec = promisify(execFile);
async function git(cwd, ...args) {
  return (await exec('git', args, { cwd, env: { ...process.env, GIT_TERMINAL_PROMPT: '0' } })).stdout.trim();
}
async function cleanMain(home) {
  if (await git(home, 'symbolic-ref', '--short', 'HEAD') !== 'main' ||
      await git(home, 'status', '--porcelain', '--untracked-files=all')) {
    throw new Error('Repository home must be a clean checkout of main');
  }
}
export async function selectRevision(home) {
  await cleanMain(home);
  await git(home, 'fetch', '--no-write-fetch-head', 'origin', '+refs/heads/main:refs/remotes/origin/main');
  const revision = await git(home, 'rev-parse', 'refs/remotes/origin/main');
  try { await git(home, 'merge-base', '--is-ancestor', 'HEAD', revision); }
  catch (error) {
    if (error.code === 1) throw new Error('Home main contains local commits and cannot fast-forward to origin/main');
    throw error;
  }
  await cleanMain(home);
  await git(home, 'merge', '--ff-only', '--no-overwrite-ignore', revision);
  if (await git(home, 'rev-parse', 'HEAD') !== revision) throw new Error('Home revision changed during update');
  return revision;
}

async function run(cwd, command, args, env = process.env) {
  const child = spawn(command, args, { cwd, env, stdio: 'inherit' });
  const [code, signal] = await once(child, 'exit');
  if (code !== 0) throw new Error(`${command} exited with ${signal || code}`);
}
async function exists(path) {
  try { await realpath(path); return true; }
  catch (error) { if (error.code === 'ENOENT') return false; throw error; }
}
async function controlFile(lock) {
  return JSON.parse(await readFile(join(lock, 'control.json'), 'utf8'));
}
export async function stopDev(lock) {
  if (!await exists(lock)) return;
  const { pid } = JSON.parse(await readFile(join(lock, 'owner.json'), 'utf8'));
  if (!Number.isSafeInteger(pid) || pid <= 0) throw new Error('Invalid Dev launcher owner');
  try { process.kill(pid, 0); }
  catch (error) {
    if (error.code === 'ESRCH') return; // The launcher reclaims its own stale lock.
    throw error;
  }
  let control;
  try { control = await controlFile(lock); }
  catch { throw new Error('Dev launcher has no ready control receipt. Its owner must stop it before the first managed rebuild'); }
  if (!/^[a-f0-9-]{36}$/.test(control.nonce)) throw new Error('Invalid Dev control receipt');
  try { await writeFile(join(lock, 'stop-' + control.nonce), '', { flag: 'wx', mode: 0o600 }); }
  catch (error) { if (error.code !== 'EEXIST') throw error; }
  for (let i = 0; i < 300 && await exists(lock); i++) await delay(100);
  if (await exists(lock)) throw new Error('Dev launcher still holds its lock');
}
async function dev({ home, root, revision, env }) {
  const directory = join(root, 'dev'), lock = join(directory, 'launch.lock');
  await mkdir(directory, { recursive: true });
  await cleanMain(home);
  if (await git(home, 'rev-parse', 'HEAD') !== revision) throw new Error('Home moved before Dev launch');
  if (!await exists(join(home, 'scripts/managed-build.mjs'))) throw new Error('Selected main has no managed Dev launcher');
  await stopDev(lock);
  await run(home, 'make', ['deps']);
  const request = randomUUID(), log = join(directory, 'build-' + request + '.log');
  console.log('[Dev] Starting launcher. Build output: ' + log);
  const fd = await open(log, 'wx', 0o600);
  let child;
  try {
    child = spawn(process.execPath, ['scripts/environments.mjs', 'dev'], { cwd: home, env: { ...env, LAZYBOX_BUILD_REQUEST: request }, detached: true, stdio: ['ignore', fd.fd, fd.fd] });
    await once(child, 'spawn');
  } finally { await fd.close(); }
  child.unref();
  const reader = await open(log, 'r');
  try {
    return await waitForDev({ reader, child, log, lock, request, revision, home });
  } finally { await reader.close(); }
}
async function waitForDev({ reader, child, log, lock, request, revision, home }) {
  const buffer = Buffer.alloc(16384);
  for (;;) {
    let bytesRead;
    do {
      ({ bytesRead } = await reader.read(buffer));
      if (bytesRead) process.stdout.write(buffer.subarray(0, bytesRead));
    } while (bytesRead === buffer.length);
    if (child.exitCode !== null || child.signalCode) throw new Error('Dev launcher exited. See ' + log);
    if (await exists(join(lock, 'control.json'))) {
      const receipt = await controlFile(lock);
      if (receipt.request === request && receipt.revision === revision) {
        await cleanMain(home);
        if (await git(home, 'rev-parse', 'HEAD') !== revision) throw new Error('Home moved during Dev build');
        return 'running';
      }
    }
    await delay(500);
  }
}
async function working({ home, root, revision, env }) {
  // Build only tracked bytes from the selected commit, even if home changes.
  const temp = await mkdtemp(join(root, 'build-source-'));
  try {
    const archive = join(temp, 'source.tar'), source = join(temp, 'source');
    await mkdir(source);
    await git(home, 'archive', '--format=tar', '--output', archive, revision);
    await run(source, 'tar', ['-xf', archive]);
    const stage = spawn(process.execPath, ['scripts/environments.mjs', 'stage'], {
      cwd: source, env, stdio: ['ignore', 'inherit', 'inherit', 'ipc'],
    });
    let candidate;
    stage.on('message', receipt => { candidate = receipt; });
    const [code, signal] = await once(stage, 'close');
    if (code !== 0) throw new Error('Staging exited with ' + (signal || code));
    if (!candidate?.id || candidate.revision !== revision) throw new Error('Missing candidate revision receipt');
    return 'staged ' + candidate.id;
  } finally { await rm(temp, { recursive: true, force: true }); }
}
export async function rebuild({ home, root, channel, channels = { Dev: dev, Working: working }, report = console.log }) {
  if (!home || !root || !isAbsolute(home) || !isAbsolute(root)) throw new Error('The Runner must supply the repository home and environment root');
  const selected = Object.entries(channels).filter(([name]) => channel === undefined || name.toLowerCase() === channel);
  if (!selected.length) throw new Error('Choose a managed build channel: dev or working');
  const lock = join(root, 'managed-build.lock');
  // Fail closed after an interrupted build. Two contenders must never both reclaim a stale lock.
  try { await mkdir(lock, { mode: 0o700 }); }
  catch (error) {
    if (error.code === 'EEXIST') throw new Error('A managed build already holds ' + lock);
    throw error;
  }
  try {
    await writeFile(join(lock, 'owner.json'), JSON.stringify({ pid: process.pid, started: new Date().toISOString() }));
    let revision;
    try { revision = await selectRevision(home); }
    catch (error) {
      for (const [name] of selected) report(`[${name}] skipped: ${error.message}`);
      throw error;
    }
    await writeFile(join(lock, 'revision'), revision);
    const env = { ...process.env, LAZYBOX_V2_ENVIRONMENTS: root, LAZYBOX_BUILD_REVISION: revision };
    const outcomes = [];
    let failed = false;
    for (const [name, build] of selected) {
      report(`[${name}] building ${revision}`);
      let outcome;
      try { outcome = `[${name}] ${await build({ home, root, revision, env })} ${revision}`; }
      catch (error) {
        failed = true;
        outcome = `[${name}] failed for ${revision}: ${error.message}`;
      }
      report(outcome);
      outcomes.push(outcome);
    }
    report('Build results\n' + outcomes.join('\n'));
    if (failed) throw new Error('One or more channels failed');
  } finally { await rm(lock, { recursive: true }); }
}
if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  rebuild({ home: process.env.LAZYBOX_REPOSITORY_HOME, root: process.env.LAZYBOX_ENVIRONMENTS, channel: process.argv[2] }).catch(error => {
    console.error(error.message);
    process.exitCode = 1;
  });
}
