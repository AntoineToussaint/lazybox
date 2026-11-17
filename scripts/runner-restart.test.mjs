// Launcher side of Restart Runner: no Codefly, no Runner. Fake starts stand in
// for `codefly run service runner`; the restart request is the real file contract.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { environmentPaths, restartRequestPath, superviseRunner, takeRestartRequest } from './environments.mjs';
import { runnerArguments } from './runner-command.mjs';

// Each fake run resolves `finished` when the test ends it.
function fakeRunners() {
  const runs = [];
  const start = async () => {
    let end; const finished = new Promise(resolve => { end = resolve; });
    runs.push({ end }); return { finished };
  };
  return { runs, start };
}
const until = async predicate => {
  for (let i = 0; i < 400 && !predicate(); i++) await new Promise(resolve => setTimeout(resolve, 5));
  assert.ok(predicate(), 'condition never held');
};
async function scratch(t) {
  const root = await mkdtemp(join(tmpdir(), 'lazybox-restart-'));
  t.after(() => rm(root, { recursive: true, force: true }));
  return join(root, 'restart-request.json');
}

test('launcher starts the Runner again only for a restart the Runner requested, without restarting the desktop', async t => {
  const request = await scratch(t);
  const { runs, start } = fakeRunners();
  let desktops = 0; const logs = [];
  const supervised = superviseRunner({ start, restartRequest: request, stopping: () => false, onReady: () => { desktops++; }, log: line => logs.push(line) });
  await until(() => runs.length === 1);
  // What the Runner does on RestartRunner (internal/restart): record, then exit.
  // `codefly run` reports its Runner's exit as a failure; the request decides.
  await writeFile(request, JSON.stringify({ version: 1, pid: 1, requestedAt: new Date().toISOString() }));
  runs[0].end({ code: 1 });
  await until(() => runs.length === 2);
  await assert.rejects(readFile(request), { code: 'ENOENT' }, 'a consumed request must not restart a later exit');
  await until(() => logs.length === 2);
  // Any other exit keeps today's behavior: the environment stops with the failure.
  runs[1].end({ code: 1 });
  await assert.rejects(supervised, /Runner exited \(1\)/);
  assert.equal(runs.length, 2);
  assert.equal(desktops, 1, 'a restart must not start a second desktop');
  assert.match(logs.join('\n'), /Agents, commands and terminals keep running/);
});

test('a restart request left by an earlier launch never turns a crash into a restart', async t => {
  const request = await scratch(t);
  await writeFile(request, JSON.stringify({ version: 1 }));
  const { runs, start } = fakeRunners();
  const supervised = superviseRunner({ start, restartRequest: request, stopping: () => false, log: () => {} });
  await until(() => runs.length === 1);
  runs[0].end({ code: null, signal: 'SIGKILL' });
  await assert.rejects(supervised, /Runner exited \(SIGKILL\)/);
  assert.equal(runs.length, 1);
});

test('stopping the environment wins over a pending restart, and a clean exit ends it quietly', async t => {
  const request = await scratch(t);
  let stopping = false;
  const { runs, start } = fakeRunners();
  const supervised = superviseRunner({ start, restartRequest: request, stopping: () => stopping, log: () => {} });
  await until(() => runs.length === 1);
  await writeFile(request, JSON.stringify({ version: 1 }));
  stopping = true; runs[0].end({ code: 1 });
  await supervised;
  assert.equal(runs.length, 1);
  const clean = fakeRunners();
  const quiet = superviseRunner({ start: clean.start, restartRequest: request, stopping: () => false, log: () => {} });
  await until(() => clean.runs.length === 1);
  clean.runs[0].end({ code: 0 });
  await quiet;
  assert.equal(clean.runs.length, 1);
  // A start interrupted by Ctrl+C reports no Runner; supervision just ends.
  await superviseRunner({ start: async () => undefined, restartRequest: request, stopping: () => true });
});

test('a restart whose Runner never becomes ready stops the environment as a first launch would', async t => {
  const request = await scratch(t);
  let starts = 0, desktops = 0;
  const start = async () => {
    if (starts++ === 1) throw new Error('Runner did not become ready; inspect its startup output');
    await writeFile(request, JSON.stringify({ version: 1 }));
    return { finished: Promise.resolve({ code: 1 }) };
  };
  await assert.rejects(superviseRunner({ start, restartRequest: request, stopping: () => false, onReady: () => { desktops++; }, log: () => {} }), /did not become ready/);
  assert.equal(starts, 2);
  assert.equal(desktops, 1);
});

test('restart requests are read once and only in the Runner format', async t => {
  const request = await scratch(t);
  assert.equal(await takeRestartRequest(request), false);
  await writeFile(request, 'not json');
  assert.equal(await takeRestartRequest(request), false);
  await writeFile(request, JSON.stringify({ version: 2 }));
  assert.equal(await takeRestartRequest(request), false);
  await writeFile(request, JSON.stringify({ version: 1 }));
  assert.equal(await takeRestartRequest(request), true);
  assert.equal(await takeRestartRequest(request), false);
});

test('a real process that records a request and exits is started again', async t => {
  const request = await scratch(t);
  let starts = 0;
  // The first child behaves like a Runner asked to restart: it records the
  // request and exits non-zero, as `codefly run` does when its Runner exits.
  const start = async () => {
    const script = starts++ === 0
      ? `require('fs').writeFileSync(${JSON.stringify(request)}, JSON.stringify({version:1,pid:process.pid})); process.exit(1)`
      : 'process.exit(0)';
    const child = spawn(process.execPath, ['-e', script], { stdio: 'ignore' });
    return { finished: once(child, 'exit').then(([code, signal]) => ({ code, signal })) };
  };
  await superviseRunner({ start, restartRequest: request, stopping: () => false, log: () => {} });
  assert.equal(starts, 2);
});

test('only the terminal launcher declares a restart request; the Codefly service does not', () => {
  const paths = environmentPaths('/fixture/environments', 'dev');
  assert.equal(restartRequestPath(paths), '/fixture/environments/dev/restart-request.json');
  assert.notEqual(restartRequestPath(paths), restartRequestPath(environmentPaths('/fixture/environments', 'working')));
  const args = runnerArguments({ paths, source: '/source', restartRequest: restartRequestPath(paths) });
  assert.ok(args.includes('runner:LAZYBOX_V2_RESTART_REQUEST=/fixture/environments/dev/restart-request.json'));
  assert.ok(args.includes('runner:LAZYBOX_V2_KEEP_AGENTS=1'));
  // runner-service.mjs builds its plan without one: nothing would start it again.
  assert.ok(!runnerArguments({ paths, source: '/source' }).some(value => value.includes('RESTART')));
});
