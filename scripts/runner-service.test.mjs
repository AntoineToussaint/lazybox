import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, readFile, rm, chmod, realpath } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { acquireLock, digestTree, environmentPaths } from './environments.mjs';
import { attachClient, controlService, findCodefly, serviceLabel, servicePlan, serviceStatus } from './runner-service.mjs';
import { runnerArguments } from './runner-command.mjs';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { fileURLToPath } from 'node:url';

async function fixture(t) {
  const root = await mkdtemp(join(tmpdir(), 'lazybox-service-'));
  t.after(() => rm(root, { recursive: true, force: true }));
  const paths = environmentPaths(root, 'working'), release = join(root, 'releases', 'candidate');
  const source = join(release, 'source'), app = join(release, 'Lazybox.app');
  for (const dir of [paths.state, join(source, 'services/runner'), join(source, 'resources'), join(source, 'workers/claude-sdk/dist'), app]) await mkdir(dir, { recursive: true });
  await writeFile(join(paths.directory, 'current.json'), JSON.stringify({ id: 'candidate' }));
  await writeFile(join(source, 'services/runner/service.codefly.yaml'), '    hot-reload: false\n');
  await writeFile(join(source, 'resources/lazybox-snippets'), '#!/bin/sh\n', { mode: 0o700 });
  await writeFile(join(source, 'workers/claude-sdk/dist/main.js'), '// fixture worker\n');
  await writeFile(join(app, 'fixture'), 'fixture app');
  const executable = join(root, 'codefly');
  await writeFile(executable, '#!/bin/sh\n', { mode: 0o700 });
  const seal = async () => writeFile(join(release, 'release.json'), JSON.stringify({ version: 1, id: 'candidate', sourceDigest: await digestTree(source), appDigest: await digestTree(app, () => true) }));
  await seal();
  const env = { PATH: '/usr/bin:/bin', GITHUB_TOKEN: 'secret-token', LAZYBOX_V2_CODEFLY: executable };
  const calls = [];
  let status = { ref: { label: serviceLabel(root) }, state: 'not-installed', installed: false, running: false };
  const call = async (binary, args) => {
    calls.push({ binary, args });
    if (args[1] === 'status') return status;
    return { ref: status.ref, action: args[1] };
  };
  return { root, paths, source, app, executable, env, calls, seal, options: { executable, env, call }, setStatus: next => { status = { ...status, ...next }; } };
}

test('plan pins public config and assets without reading or serializing credentials', async t => {
  const f = await fixture(t);
  await writeFile(f.paths.config, 'private config that must not be parsed');
  await writeFile(f.paths.endpoint, 'private endpoint that must not be parsed');
  const plan = await servicePlan(f.root, f.options);
  assert.equal(plan.restart, 'never');
  assert.equal(plan.startAtLogin, false);
  assert.equal(plan.version, (await servicePlan(f.root, f.options)).version);
  assert.deepEqual(Object.keys(plan.publicEnv).sort(), ['GOFLAGS', 'GOMAXPROCS', 'PATH']);
  assert.ok(!JSON.stringify(plan).includes('secret-token'));
  assert.ok(plan.args.includes('--temporary-ports'));
  assert.equal(f.calls.length, 0);
  await writeFile(join(f.source, 'workers/claude-sdk/dist/main.js'), '// changed build');
  assert.notEqual(plan.version, (await servicePlan(f.root, f.options)).version);
});

test('release validation rejects modified source, app and live reload before native mutation', async t => {
  const f = await fixture(t);
  await writeFile(join(f.source, 'unexpected'), 'change');
  await assert.rejects(controlService(f.root, 'install', f.options), /Release is incomplete/);
  assert.equal(f.calls.filter(c => c.args[1] !== 'status').length, 0);
  await f.seal();
  await writeFile(join(f.app, 'fixture'), 'changed app');
  await assert.rejects(servicePlan(f.root, f.options), /Release is incomplete/);
  await writeFile(join(f.source, 'services/runner/service.codefly.yaml'), '    hot-reload: true\n');
  await f.seal();
  await assert.rejects(servicePlan(f.root, f.options), /frozen Codefly/);
});

test('installation delegates argv to Codefly, preserves private config and never starts implicitly', async t => {
  const f = await fixture(t);
  await writeFile(f.paths.config, 'preserve-me');
  await controlService(f.root, 'install', f.options);
  assert.equal(await readFile(f.paths.config, 'utf8'), 'preserve-me');
  assert.deepEqual(f.calls.map(c => c.args[1]), ['status', 'install']);
  const args = f.calls[1].args;
  assert.ok(args.includes('--public-arg=--temporary-ports'));
  assert.ok(args.includes('--start-at-login=false'));
  assert.ok(!args.join(' ').includes('secret-token'));
  assert.ok(!args.some(a => /health-(http|tcp)/.test(a)));
});

test('running service and terminal launcher prevent installation', async t => {
  const f = await fixture(t);
  f.setStatus({ installed: true, running: true });
  await assert.rejects(controlService(f.root, 'install', f.options), /Stop the Runner/);
  assert.deepEqual(f.calls.map(c => c.args[1]), ['status']);
  const unlock = await acquireLock(f.paths.lock);
  try { await assert.rejects(controlService(f.root, 'start', f.options), /already running/); }
  finally { await unlock(); }
});

test('start requires exact installed contract and revalidates compiled worker', async t => {
  const f = await fixture(t), plan = await servicePlan(f.root, f.options);
  f.setStatus({ installed: true, version: 'older' });
  await assert.rejects(controlService(f.root, 'start', f.options), /Install the selected/);
  f.setStatus({ version: plan.version });
  await controlService(f.root, 'start', f.options);
  assert.equal(f.calls.at(-1).args[1], 'start');
  await writeFile(join(f.source, 'workers/claude-sdk/dist/main.js'), '// changed');
  await assert.rejects(controlService(f.root, 'start', f.options), /Install the selected/);
});

test('stop/uninstall remain available with missing releases and uninstall requires stopped service', async t => {
  const f = await fixture(t);
  await rm(join(f.root, 'releases'), { recursive: true });
  f.setStatus({ installed: true, running: true, version: 'old-contract' });
  await controlService(f.root, 'stop', f.options);
  assert.equal(f.calls.at(-1).args[1], 'stop');
  await assert.rejects(controlService(f.root, 'uninstall', f.options), /Stop the Runner/);
  f.setStatus({ running: false });
  await controlService(f.root, 'uninstall', f.options);
  assert.deepEqual(f.calls.at(-1).args.slice(-3), ['--version', 'old-contract', '--json']);
});

test('status does not misrepresent native process health as product readiness or expose logs', async t => {
  const f = await fixture(t);
  f.setStatus({ running: true, healthy: true, diagnostics: { recent_logs: ['private log'] } });
  const result = await controlService(f.root, 'status', f.options);
  assert.equal(result.readiness, 'not-probed');
  assert.ok(!JSON.stringify(result).includes('private log'));
  await assert.rejects(serviceStatus(f.root, { ...f.options, call: async () => ({ ref: { label: 'another' }, installed: true, running: true }) }), /does not match/);
});

test('client checks authenticated readiness then detaches without stopping the service', async t => {
  const f = await fixture(t), plan = await servicePlan(f.root, f.options);
  f.setStatus({ installed: true, running: true, version: plan.version });
  await writeFile(f.paths.endpoint, JSON.stringify({ url: 'http://127.0.0.1:12345', token: 'private-bearer' }));
  let launched = 0;
  const options = { ...f.options, fetch: async (url, init) => {
    assert.equal(url.pathname, '/lazybox.bench.v1.RunnerService/Info');
    assert.equal(init.headers.Authorization, 'Bearer private-bearer');
    assert.equal(init.redirect, 'error');
    return { ok: true, json: async () => ({}) };
  }, launch: async (command, args) => { launched++; assert.deepEqual(args, ['--endpoint', f.paths.endpoint]); assert.ok(command.startsWith(f.app)); } };
  await attachClient(f.root, options);
  assert.equal(launched, 1);
  assert.deepEqual(f.calls.map(c => c.args[1]), ['status']);
  await assert.rejects(attachClient(f.root, { ...options, fetch: async () => ({ ok: false }) }), /not ready/);
  assert.equal(launched, 1);
  await writeFile(f.paths.endpoint, JSON.stringify({ url: 'https://example.com', token: 'private-bearer' }));
  await assert.rejects(attachClient(f.root, options), /local Runner/);
});

test('Codefly resolution rejects relative overrides and finds executable on PATH', async t => {
  const f = await fixture(t);
  assert.equal(await findCodefly({ PATH: f.root }), await realpath(f.executable));
  await assert.rejects(findCodefly({ LAZYBOX_V2_CODEFLY: './codefly' }), /absolute executable/);
  await chmod(f.executable, 0o600);
  await assert.rejects(findCodefly({ PATH: f.root }), /Codefly is required/);
  assert.notEqual(serviceLabel(f.root), serviceLabel(join(f.root, 'other')));
});

test('shared Runner command preserves optional subscription and snippet selection', async t => {
  const f = await fixture(t);
  const args = runnerArguments({ paths: f.paths, source: f.source, snippetAdapter: '/tools/snippets', snippetDirectory: '/project', subscriptionState: '/shared/subscriptions' });
  assert.ok(args.includes('runner:LAZYBOX_V2_SUBSCRIPTION_STATE=/shared/subscriptions'));
  assert.ok(args.includes('runner:LAZYBOX_V2_SNIPPET_DIRECTORY=/project'));
  const bare = runnerArguments({ paths: f.paths, source: f.source });
  assert.ok(!bare.some(value => /SUBSCRIPTION|SNIPPET/.test(value)));
});

test('real CLI adapter consumes JSON and preserves argument boundaries without shell execution', async t => {
  const f = await fixture(t), log = join(f.root, 'calls.jsonl');
  // A subprocess fixture exercises the actual execFile boundary; no OS service
  // is installed and no launchctl/systemctl command is run by this suite.
  const program = `#!${process.execPath}\nimport {appendFileSync} from 'node:fs';\nconst args=process.argv.slice(2);\nappendFileSync(${JSON.stringify(log)},JSON.stringify(args)+'\\n');\nconsole.log(JSON.stringify(args[1]==='status'?{ref:{label:args[2]},installed:false,running:false,state:'not-installed'}:{ref:{label:args[2]}}));\n`;
  const executable = join(f.root, 'codefly-fixture.mjs');
  await writeFile(executable, program, { mode: 0o700 });
  await controlService(f.root, 'install', { executable, env: f.env });
  const calls = (await readFile(log, 'utf8')).trim().split('\n').map(JSON.parse);
  assert.deepEqual(calls.map(args => args[1]), ['status', 'install']);
  assert.ok(calls[1].includes(`--public-arg=runner:LAZYBOX_V2_STATE=${f.paths.state}`));
});

test('terminal Working entry point rejects a managed service before building or spawning a Runner', async t => {
  const f = await fixture(t), executable = join(f.root, 'installed-codefly.mjs');
  await writeFile(executable, `#!${process.execPath}\nconst args=process.argv.slice(2); if(args[0]!=='service'||args[1]!=='status')process.exit(9); console.log(JSON.stringify({ref:{label:args[2]},installed:true,running:false}));\n`, { mode: 0o700 });
  await assert.rejects(promisify(execFile)(process.execPath, [fileURLToPath(new URL('./environments.mjs', import.meta.url)), 'working'], {
    env: { ...process.env, LAZYBOX_V2_ENVIRONMENTS: f.root, LAZYBOX_V2_CODEFLY: executable }, timeout: 5000,
  }), error => {
    assert.match(error.stderr, /Working is managed by Codefly/);
    return true;
  });
  await assert.rejects(readFile(join(f.paths.lock, 'owner.json')), { code: 'ENOENT' });
});
