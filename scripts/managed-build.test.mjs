import test from 'node:test';
import fs from 'node:fs';
import { syncBuiltinESMExports } from 'node:module';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { randomUUID } from 'node:crypto';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { rebuild, stopDev } from './managed-build.mjs';

async function fixture(t) {
  const root = await mkdtemp(join(tmpdir(), 'managed-build-'));
  t.after(() => rm(root, { recursive: true, force: true }));
  const remote = join(root, 'remote'), home = join(root, 'home');
  const git = (cwd, ...args) => execFileSync('git', ['-c', 'user.name=Test', '-c', 'user.email=test@example.org', ...args], { cwd, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }).trim();
  git(root, 'init', '-b', 'main', remote);
  git(remote, 'commit', '--allow-empty', '-m', 'initial');
  git(root, 'clone', remote, home);
  git(remote, 'commit', '--allow-empty', '-m', 'next');
  return { root, home, git, remote, revision: git(remote, 'rev-parse', 'HEAD') };
}
test('both channels receive the fetched main revision and report independent results', async t => {
  const f = await fixture(t), lines = [];
  const channels = {
    Dev: async ({ revision }) => { assert.equal(revision, f.revision); return 'running'; },
    Working: async ({ env }) => { assert.equal(env.LAZYBOX_BUILD_REVISION, f.revision); return 'staged'; },
  };
  await rebuild({ ...f, channels, report: line => lines.push(line) });
  assert.equal(f.git(f.home, 'rev-parse', 'HEAD'), f.revision);
  assert.ok(lines.includes('[Dev] running ' + f.revision));
  assert.ok(lines.includes('[Working] staged ' + f.revision));
  channels.Dev = async () => { throw new Error('launcher refused'); };
  lines.length = 0;
  await assert.rejects(rebuild({ ...f, channels, report: line => lines.push(line) }), /channels failed/);
  assert.match(lines.join('\n'), /Dev.*failed.*launcher refused/);
  assert.ok(lines.includes('[Working] staged ' + f.revision));
});
test('Working starts the archived staging script without installing dependencies in the archive', async t => {
  const f = await fixture(t);
  await mkdir(join(f.remote, 'scripts'));
  await writeFile(join(f.remote, 'scripts/managed-build.mjs'), '// managed build fixture');
  await writeFile(join(f.remote, 'scripts/environments.mjs'), `
    import assert from 'node:assert/strict';
    import { existsSync } from 'node:fs';
    assert.equal(process.argv[2], 'stage');
    assert.equal(existsSync('node_modules'), false);
    process.send({ id: 'working-candidate', revision: process.env.LAZYBOX_BUILD_REVISION });
  `);
  await writeFile(join(f.remote, 'scripts/activate-working.mjs'), "import assert from 'node:assert/strict'; assert.equal(process.argv[2], 'working-candidate');");
  f.git(f.remote, 'add', 'scripts');
  f.git(f.remote, 'commit', '-m', 'stage fixture');
  const messages = [];
  await rebuild({ ...f, channel: 'working', report: message => messages.push(message) });
  assert.ok(messages.some(message => message.includes('running working-candidate')));
});
test('either standalone action runs only the requested channel', async t => {
  const f = await fixture(t), calls = [];
  const channels = {
    Dev: async ({ revision }) => { calls.push(['dev', revision]); return 'running'; },
    Working: async ({ revision }) => { calls.push(['working', revision]); return 'staged'; },
  };
  for (const channel of ['working', 'dev']) {
    await rebuild({ ...f, channel, channels, report: () => {} });
    assert.deepEqual(calls.splice(0), [[channel, f.revision]]);
  }
  await assert.rejects(rebuild({ ...f, channel: 'invalid', channels }), /Choose a managed build channel/);
});
test('local changes, divergent history and a concurrent request prevent channel work', async t => {
  const f = await fixture(t), before = f.git(f.home, 'rev-parse', 'HEAD');
  let starts = 0;
  const request = { ...f, channels: { Dev: () => starts++, Working: () => starts++ }, report: () => {} };
  await writeFile(join(f.home, 'edit'), 'local');
  await assert.rejects(rebuild(request), /clean checkout/);
  assert.equal(await readFile(join(f.home, 'edit'), 'utf8'), 'local');
  assert.equal(f.git(f.home, 'rev-parse', 'HEAD'), before);
  f.git(f.home, 'add', 'edit');
  f.git(f.home, 'commit', '-m', 'local');
  await assert.rejects(rebuild(request), /local commits/);
  await mkdir(join(f.root, 'managed-build.lock'));
  await assert.rejects(rebuild(request), /already holds/);
  assert.equal(starts, 0);
});
test('running Dev must acknowledge its launcher-specific request and release its lock', async t => {
  const f = await fixture(t), lock = join(f.root, 'launch.lock');
  await mkdir(lock);
  await writeFile(join(lock, 'owner.json'), JSON.stringify({ pid: process.pid }));
  await assert.rejects(stopDev(lock), /no ready control receipt/);
  // Watch notifications are hints, not delivery: suppress them entirely.
  const mockedWatch = t.mock.method(fs, 'watch', () => ({ close() {} }));
  syncBuiltinESMExports();
  let watcher;
  try {
    const { listenForDevStop } = await import('./environments.mjs');
    watcher = await listenForDevStop(lock, {}, () => {
      watcher.close();
      void rm(lock, { recursive: true });
    });
    await writeFile(join(lock, 'stop-' + randomUUID()), '');
    assert.ok(await readFile(join(lock, 'owner.json')));
    await stopDev(lock);
  } finally {
    watcher?.close();
    mockedWatch.mock.restore();
    syncBuiltinESMExports();
  }
});

test('standalone Dev restarts and Working activates the staged revision without touching Dev', async t => {
  const f = await fixture(t);
  await mkdir(join(f.remote, 'scripts'));
  await writeFile(join(f.remote, 'scripts/managed-build.mjs'), '// supported');
  await writeFile(join(f.remote, '.gitignore'), 'ignored-input\n');
  await writeFile(join(f.remote, 'Makefile'), 'deps:\n\t@true\n');
  await writeFile(join(f.remote, 'scripts/environments.mjs'), `
    import { mkdir, writeFile, rm, access } from 'node:fs/promises';
    import { watch } from 'node:fs';
    import { join } from 'node:path';
    const root = process.env.LAZYBOX_V2_ENVIRONMENTS;
    const revision = process.env.LAZYBOX_BUILD_REVISION;
    if (process.argv[2] === 'dev') {
      const lock = join(root, 'dev/launch.lock');
      await mkdir(lock);
      await writeFile(join(lock, 'owner.json'), JSON.stringify({ pid: process.pid }));
      const nonce = process.env.LAZYBOX_BUILD_REQUEST;
      const watcher = watch(lock, async (_event, name) => {
        if (name !== 'stop-' + nonce) return;
        watcher.close();
        await rm(lock, { recursive: true });
      });
      await writeFile(join(lock, 'control.json'), JSON.stringify({ nonce, request: nonce, revision }));
    } else {
      try { await access('ignored-input'); throw new Error('untracked file staged'); }
      catch (error) { if (error.code !== 'ENOENT') throw error; }
      process.send({ id: 'fixture-candidate', revision });
    }
  `);
  await writeFile(join(f.remote, 'scripts/activate-working.mjs'), `
    import { writeFileSync } from 'node:fs';
    import { join } from 'node:path';
    writeFileSync(join(process.env.LAZYBOX_V2_ENVIRONMENTS, 'working/current.json'), process.argv[2]);
  `);
  f.git(f.remote, 'add', '.');
  f.git(f.remote, 'commit', '-m', 'fixture channel tools');
  const revision = f.git(f.remote, 'rev-parse', 'HEAD');
  const pin = join(f.root, 'working/current.json');
  await mkdir(join(f.root, 'working'));
  await writeFile(pin, 'pinned-before');
  const lines = [], request = { ...f, report: line => lines.push(line) };
  const lock = join(f.root, 'dev/launch.lock');
  try {
    await rebuild({ ...request, channel: 'dev' });
    const first = JSON.parse(await readFile(join(lock, 'control.json')));
    await writeFile(join(f.home, 'ignored-input'), 'local only');
    await rebuild({ ...request, channel: 'dev' });
    const second = JSON.parse(await readFile(join(lock, 'control.json')));
    assert.notEqual(first.request, second.request);
    assert.equal(second.revision, revision);
    assert.equal(lines.filter(line => line === '[Dev] running ' + revision).length, 2);
    assert.ok(!lines.some(line => line.startsWith('[Working]')));
    lines.length = 0;
    await rebuild({ ...request, channel: 'working' });
    assert.ok(lines.includes('[Working] running fixture-candidate ' + revision));
    assert.ok(!lines.some(line => line.startsWith('[Dev]')));
    assert.deepEqual(JSON.parse(await readFile(join(lock, 'control.json'))), second);
    assert.equal(await readFile(pin, 'utf8'), 'fixture-candidate');
  } finally { await stopDev(lock); }
});

test('Working activation replaces a running launcher, waits for its desktop and restores the pin after a failed launch', async t => {
  const { activateWorking } = await import('./activate-working.mjs');
  const { digestTree } = await import('./environments.mjs');
  const { chmod } = await import('node:fs/promises');
  const root = await mkdtemp(join(tmpdir(), 'working-activation-'));
  const working = join(root, 'working'), lock = join(working, 'launch.lock');
  const fake = join(root, 'codefly');
  await writeFile(fake, '#!/bin/sh\nprintf \'{"ref":{"label":"%s"},"installed":false,"running":false}\\n\' "$3"\n');
  await chmod(fake, 0o700);
  const priorCodefly = process.env.LAZYBOX_V2_CODEFLY, priorRevision = process.env.LAZYBOX_BUILD_REVISION;
  process.env.LAZYBOX_V2_CODEFLY = fake;
  process.env.LAZYBOX_BUILD_REVISION = 'fixture-revision';
  t.after(async () => {
    if (priorCodefly === undefined) delete process.env.LAZYBOX_V2_CODEFLY; else process.env.LAZYBOX_V2_CODEFLY = priorCodefly;
    if (priorRevision === undefined) delete process.env.LAZYBOX_BUILD_REVISION; else process.env.LAZYBOX_BUILD_REVISION = priorRevision;
    try { process.kill(JSON.parse(await readFile(join(lock, 'owner.json'))).pid, 'SIGINT'); } catch (error) { if (error.code !== 'ENOENT' && error.code !== 'ESRCH') throw error; }
    await rm(root, { recursive: true, force: true });
  });
  for (const id of ['first', 'second', 'broken']) {
    const dir = join(root, 'releases', id), source = join(dir, 'source'), app = join(dir, 'Lazybox.app');
    const worker = join(source, 'workers/claude-sdk/dist');
    await mkdir(join(source, 'scripts'), { recursive: true });
    await mkdir(app);
    await mkdir(worker, { recursive: true });
    await writeFile(join(source, 'scripts/environments.mjs'), `
      import { mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
      import { join } from 'node:path';
      const dir = join(process.env.LAZYBOX_V2_ENVIRONMENTS, 'working');
      const pin = JSON.parse(readFileSync(join(dir, 'current.json')));
      if (pin.id === 'broken') process.exit(1);
      const lock = join(dir, 'launch.lock');
      mkdirSync(lock);
      writeFileSync(join(lock, 'owner.json'), JSON.stringify({ pid: process.pid }));
      process.once('SIGINT', () => { rmSync(lock, { recursive: true, force: true }); process.exit(); });
      if (process.env.LAZYBOX_WORKING_DESKTOP_READY) writeFileSync(process.env.LAZYBOX_WORKING_DESKTOP_READY, 'ready');
      setInterval(() => {}, 1000);
    `);
    await writeFile(join(dir, 'release.json'), JSON.stringify({ id, version: 3, revision: 'fixture-revision',
      sourceDigest: await digestTree(source), appDigest: await digestTree(app, () => true), workerDigest: await digestTree(worker, () => true) }));
  }
  const owner = async () => JSON.parse(await readFile(join(lock, 'owner.json'))).pid;
  await activateWorking(root, 'first');
  const first = await owner();
  await activateWorking(root, 'second');
  assert.notEqual(await owner(), first);
  assert.equal(JSON.parse(await readFile(join(working, 'current.json'))).id, 'second');
  await assert.rejects(activateWorking(root, 'broken'), /activation failed/);
  assert.equal(JSON.parse(await readFile(join(working, 'current.json'))).id, 'second');
  for (let i = 0; i < 100 && !fs.existsSync(join(lock, 'owner.json')); i++) await new Promise(resolve => setTimeout(resolve, 20));
  assert.ok(await owner());
});
