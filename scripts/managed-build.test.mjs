import test from 'node:test';
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
  const { listenForDevStop } = await import('./environments.mjs');
  const watcher = await listenForDevStop(lock, {}, () => {
    watcher.close();
    void rm(lock, { recursive: true });
  });
  await writeFile(join(lock, 'stop-' + randomUUID()), '');
  assert.ok(await readFile(join(lock, 'owner.json')));
  await stopDev(lock);
});

test('managed entry point starts Dev twice and stages the selected tree without changing the Working pin', async t => {
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
  f.git(f.remote, 'add', '.');
  f.git(f.remote, 'commit', '-m', 'fixture channel tools');
  const revision = f.git(f.remote, 'rev-parse', 'HEAD');
  const pin = join(f.root, 'working/current.json');
  await mkdir(join(f.root, 'working'));
  await writeFile(pin, 'pinned-before');
  const lines = [], request = { ...f, report: line => lines.push(line) };
  const lock = join(f.root, 'dev/launch.lock');
  try {
    await rebuild(request);
    const first = JSON.parse(await readFile(join(lock, 'control.json')));
    await writeFile(join(f.home, 'ignored-input'), 'local only');
    await rebuild(request);
    const second = JSON.parse(await readFile(join(lock, 'control.json')));
    assert.notEqual(first.request, second.request);
    assert.equal(second.revision, revision);
    assert.equal(lines.filter(line => line === '[Dev] running ' + revision).length, 2);
    assert.equal(lines.filter(line => line === '[Working] staged fixture-candidate ' + revision).length, 2);
    assert.equal(await readFile(pin, 'utf8'), 'pinned-before');
  } finally { await stopDev(lock); }
});
