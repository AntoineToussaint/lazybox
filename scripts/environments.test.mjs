import { test } from 'node:test';
import assert from 'node:assert/strict';
import { writeFileSync } from 'node:fs';
import { startWorkingDesktop, environmentPaths, releasePath, includeSource, validateEnvironmentRoot, shareSubscriptions, sharedSubscriptionState, freezeRunnerReload, workingTauriConfig, pruneFailedCandidate, sweepFailedCandidates } from './environments.mjs';
import { startDesktopDevServer, desktopDevConfig, stopProcessGroup, spawnProcessGroup } from './desktop-dev.mjs';
import { uiWatcher } from './ui-watch.mjs';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { fileURLToPath } from 'node:url';
import { randomUUID } from 'node:crypto';
import { loadGateCatalog } from './release-gates.mjs';
import { chromium } from '@playwright/test';
import { runnerLaunchEnvironment } from './runner-command.mjs';

test('a terminal launcher does not pass the parent agent account to the Runner', () => {
  const terminal = {
    PATH: '/fixture/bin',
    CODEX_HOME: '/fixture/codex',
    OPENAI_API_KEY: 'fixture-key',
    CLAUDE_CONFIG_DIR: '/fixture/claude',
    CLAUDE_CODE_OAUTH_TOKEN: 'fixture-token',
  };
  assert.deepEqual(runnerLaunchEnvironment(terminal), terminal);
  const agent = { ...terminal, LAZYBOX_V2_SESSION_ID: 'fixture-agent' };
  assert.deepEqual(runnerLaunchEnvironment(agent), { PATH: terminal.PATH, LAZYBOX_V2_SESSION_ID: 'fixture-agent' });
  assert.equal(agent.CODEX_HOME, terminal.CODEX_HOME);
});

test('launcher cleans up a watcher that ignores termination without touching another group', async () => {
  const launch = () => spawn(process.execPath, ['-e', "process.on('SIGTERM',()=>{}); console.log('ready'); setInterval(()=>{},1000)"], { detached: true, stdio: ['ignore', 'pipe', 'pipe'] });
  const owned = launch(), other = launch();
  try {
    await Promise.all([once(owned.stdout, 'data'), once(other.stdout, 'data')]);
    const exited = once(owned, 'exit');
    await stopProcessGroup(owned, 50);
    assert.equal((await exited)[1], 'SIGKILL');
    assert.doesNotThrow(() => process.kill(other.pid, 0));
    await stopProcessGroup(owned, 50);
    await stopProcessGroup(undefined);
  } finally {
    await Promise.all([stopProcessGroup(owned, 50), stopProcessGroup(other, 50)]);
  }
});

test('the terminal-launched Working desktop can be stopped as an owned group', async () => {
  const app = await mkdtemp(join(tmpdir(), 'lazybox-working-client-'));
  const bin = join(app, 'Contents/MacOS');
  await mkdir(bin, { recursive: true });
  await writeFile(join(bin, 'lazybox-v2-desktop'), '#!/bin/sh\nexec sleep 30\n', { mode: 0o755 });
  const desktop = startWorkingDesktop(app, join(app, 'endpoint.json'), process.env);
  try {
    await once(desktop, 'spawn');
    const exited = once(desktop, 'exit');
    await stopProcessGroup(desktop);
    assert.equal(desktop.signalCode, 'SIGTERM');
    await exited;
  } finally {
    desktop.kill();
    await rm(app, { recursive: true, force: true });
  }
});

test('working and development identities, state, endpoints, and locks are distinct', () => {
  const working = environmentPaths('/fixture/environments', 'working');
  const dev = environmentPaths('/fixture/environments', 'dev');
  for (const key of ['directory', 'state', 'endpoint', 'config', 'identifier', 'productName', 'lock']) assert.notEqual(working[key], dev[key], key);
  assert.equal(working.identifier, 'ai.lazybox.v2.working');
  assert.equal(dev.identifier, 'ai.lazybox.v2.dev');
  assert.throws(() => environmentPaths('/fixture', '../working'));
});
test('release identities cannot escape the immutable release directory', () => {
  assert.equal(releasePath('/fixture', 'candidate-1'), '/fixture/releases/candidate-1');
  for (const id of ['', '..', '/tmp/overwrite', '../working', 'a/b']) assert.throws(() => releasePath('/fixture', id));
});
test('source snapshots exclude credentials, runtime state, and mutable build output', () => {
  for (const file of ['.git', '.git/index', '.git/config', '.cache/subscriptions/accounts/secret', 'runner.config.json', '.env', '.env.local', 'services/runner/.cache/env', 'apps/desktop/src-tauri/target/debug/app', 'node_modules/lib/index.js', 'workers/claude-sdk/dist/main.js']) assert.equal(includeSource(file), false, file);
  for (const file of ['services/runner/code/work.go', 'services/runner/code/go.sum', 'package-lock.json', 'workers/claude-sdk/src/main.ts', 'apps/desktop/src-tauri/Cargo.lock', '.env.example']) assert.equal(includeSource(file), true, file);
});

import { mkdtemp, mkdir, writeFile, readFile, rm, rename, symlink, realpath, readdir } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { copySource, digestTree, promote, acquireLock, readValidatedRelease, buildCLI, watchCLI, runnerSourceFingerprint, runnerSourceFile } from './environments.mjs';

test('snapshot inside the source excludes runtime credentials and survives subsequent source edits', async t => {
  const root = await mkdtemp(join(tmpdir(), 'lazybox-environments-'));
  t.after(() => rm(root, { recursive: true, force: true }));
  await mkdir(join(root, '.cache/releases'), { recursive: true });
  await writeFile(join(root, '.cache/credential'), 'private');
  await writeFile(join(root, 'runner.config.json'), 'private');
  await writeFile(join(root, 'main.ts'), 'original');
  const snapshot = join(root, '.cache/releases/source');
  await copySource(root, snapshot);
  const digest = await digestTree(snapshot);
  await writeFile(join(root, 'main.ts'), 'development');
  assert.equal(await readFile(join(snapshot, 'main.ts'), 'utf8'), 'original');
  assert.equal(await digestTree(snapshot), digest);
  for (const path of ['.cache/credential', 'runner.config.json']) await assert.rejects(readFile(join(snapshot, path)), { code: 'ENOENT' });
  await symlink(join(root, '.cache/credential'), join(root, 'linked-secret'));
  await assert.rejects(copySource(root, join(root, '.cache/releases/rejected')), /symlink/);
});

test('promotion validates source and app, pins only the next launch, and leaves both states untouched', async t => {
  const root = await mkdtemp(join(tmpdir(), 'lazybox-promotion-'));
  t.after(() => rm(root, { recursive: true, force: true }));
  for (const channel of ['working', 'dev']) {
    const paths = environmentPaths(root, channel);
    await mkdir(paths.state, { recursive: true });
    await writeFile(join(paths.state, 'history.db'), channel + '-history');
    await writeFile(paths.config, channel + '-private-config');
  }
  const candidate = releasePath(root, 'candidate');
  await mkdir(join(candidate, 'source'), { recursive: true });
  await mkdir(join(candidate, 'source/scripts'));
  await mkdir(join(candidate, 'source/workers/claude-sdk/dist'), { recursive: true });
  await writeFile(join(candidate, 'source/workers/claude-sdk/dist/main.js'), '// validated worker');
  const sourceRoot = fileURLToPath(new URL('..', import.meta.url));
  // The catalog and every file its policy fingerprints, as a staged snapshot holds them.
  const { policyFiles } = JSON.parse(await readFile(join(sourceRoot, 'scripts/release-gates.json'), 'utf8'));
  for (const path of ['package.json', 'scripts/release-gates.json', ...policyFiles]) {
    await mkdir(dirname(join(candidate, 'source', path)), { recursive: true });
    await writeFile(join(candidate, 'source', path), await readFile(join(sourceRoot, path)));
  }
  await mkdir(join(candidate, 'Lazybox.app'));
  await writeFile(join(candidate, 'source/main.ts'), 'validated');
  await writeFile(join(candidate, 'Lazybox.app/binary'), 'validated-app');
  const catalog = await loadGateCatalog(fileURLToPath(new URL('..', import.meta.url)));
  const sourceDigest = await digestTree(join(candidate, 'source'));
  const metadata = { version: 2, id: 'candidate', sourceDigest, appDigest: await digestTree(join(candidate, 'Lazybox.app'), () => true), workerDigest: await digestTree(join(candidate, 'source/workers/claude-sdk/dist'), () => true), gateReceipt: {
    version: 1, profile: 'release', status: 'passed', sourceDigest, finalSourceDigest: sourceDigest, catalogDigest: catalog.digest,
    results: catalog.gates.map(gate => ({ id: gate.id, command: gate.command, args: gate.args, status: 'passed', exitCode: 0, durationMs: 1 })),
  } };
  await writeFile(join(candidate, 'release.json'), JSON.stringify(metadata));
  assert.equal((await readValidatedRelease(root, 'candidate')).metadata.id, 'candidate');
  await promote(root, 'candidate');
  const pointer = join(root, 'working/current.json');
  const pinned = await readFile(pointer, 'utf8');
  assert.equal(JSON.parse(pinned).id, 'candidate');
  for (const invalid of [{ ...metadata, version: 1 }, { ...metadata, gateReceipt: undefined }, { ...metadata, gateReceipt: { ...metadata.gateReceipt, results: metadata.gateReceipt.results.slice(1) } }]) {
    await writeFile(join(candidate, 'release.json'), JSON.stringify(invalid));
    await assert.rejects(promote(root, 'candidate'), /legacy|receipt/);
    assert.equal(await readFile(pointer, 'utf8'), pinned);
  }
  // Working builds its own artifacts. Release gate receipts belong to v2 candidates.
  const { gateReceipt, ...workingBuild } = metadata;
  await writeFile(join(candidate, 'release.json'), JSON.stringify({ ...workingBuild, version: 3 }));
  await promote(root, 'candidate');
  const workingPointer = await readFile(pointer, 'utf8');
  await writeFile(join(candidate, 'source/workers/claude-sdk/dist/main.js'), '// unvalidated worker');
  await assert.rejects(promote(root, 'candidate'), /Compiled worker changed/);
  assert.equal(await readFile(pointer, 'utf8'), workingPointer);
  await writeFile(join(candidate, 'source/workers/claude-sdk/dist/main.js'), '// validated worker');
  await writeFile(join(candidate, 'Lazybox.app/binary'), 'unvalidated-app');
  await assert.rejects(promote(root, 'candidate'), /incomplete|changed/);
  assert.equal(await readFile(pointer, 'utf8'), workingPointer);
  for (const channel of ['working', 'dev']) {
    const paths = environmentPaths(root, channel);
    assert.equal(await readFile(join(paths.state, 'history.db'), 'utf8'), channel + '-history');
    assert.equal(await readFile(paths.config, 'utf8'), channel + '-private-config');
  }
});

test('a second launcher cannot build over a running environment', async t => {
  const root = await mkdtemp(join(tmpdir(), 'lazybox-lock-'));
  t.after(() => rm(root, { recursive: true, force: true }));
  const path = join(root, 'launch.lock');
  const unlock = await acquireLock(path);
  await assert.rejects(acquireLock(path), /already running or building/);
  await unlock();
  await (await acquireLock(path))();
});

test('custom storage cannot recurse into its own source snapshot', () => {
  for (const directory of ['/source/v2', '/source/v2/releases']) assert.throws(() => validateEnvironmentRoot('/source/v2', directory), /outside source/);
  for (const directory of ['/source/v2/.cache/environments', '/data/lazybox', '/source/other']) assert.doesNotThrow(() => validateEnvironmentRoot('/source/v2', directory));
});

test('Working disables Codefly watching in its snapshot without changing development source', async t => {
  const root=await mkdtemp(join(tmpdir(),'lazybox-frozen-reload-'));
  t.after(()=>rm(root,{recursive:true,force:true}));
  await mkdir(join(root,'services/runner'),{recursive:true});
  const path=join(root,'services/runner/service.codefly.yaml');
  await writeFile(path,'spec:\n    hot-reload: true\n    connect-endpoint: true\n');
  await freezeRunnerReload(root);
  assert.equal(await readFile(path,'utf8'),'spec:\n    hot-reload: false\n    connect-endpoint: true\n');
  await freezeRunnerReload(root);
  await writeFile(path,'spec: {}');
  await assert.rejects(freezeRunnerReload(root),/explicit Codefly/);
});

test('desktop development CSP uses only its bound loopback origin', () => {
  const paths=environmentPaths('/fixture','dev');
  const nonce='A'.repeat(24);
  const config=desktopDevConfig(paths,'http://127.0.0.1:49177',nonce);
  assert.equal(config.build.devUrl,'http://127.0.0.1:49177');
  assert.ok(config.app.security.devCsp.includes('ws://127.0.0.1:49177'));
  assert.equal(config.app.security.csp,undefined,'production CSP must not be overridden');
  assert.ok(config.app.security.devCsp.includes(`script-src 'self' 'nonce-${nonce}'`));
  // Frames only from an extension content server's session path, no <base>, no form submission: as the release CSP.
  const directives=config.app.security.devCsp.split(';').map(d=>d.trim());
  for(const directive of ['frame-src http://127.0.0.1:*/x/',"base-uri 'none'","form-action 'none'"]) assert.ok(directives.includes(directive),directive);
  for(const url of ['http://example.com:49177','http://127.0.0.1','http://secret@127.0.0.1:49177','http://127.0.0.1:49177/path']) assert.throws(()=>desktopDevConfig(paths,url,nonce));
  assert.throws(()=>desktopDevConfig(paths,'http://127.0.0.1:49177','invalid'));
});

test('watcher policy has an explicit bounded macOS stopgap and configurable backends', () => {
  assert.deepEqual(uiWatcher({}, 'darwin'), { backend: 'polling', options: { useFsEvents: false, usePolling: true, interval: 250, binaryInterval: 1000 } });
  assert.deepEqual(uiWatcher({}, 'linux'), { backend: 'default', options: {} });
  for (const platform of ['darwin', 'linux']) {
    assert.deepEqual(uiWatcher({ LAZYBOX_V2_UI_WATCHER: 'default' }, platform).options, {});
    assert.equal(uiWatcher({ LAZYBOX_V2_UI_WATCHER: 'polling' }, platform).options.usePolling, true);
    assert.throws(() => uiWatcher({ LAZYBOX_V2_UI_WATCHER: 'typo' }, platform), /must be/);
  }
});

test('real Vite server watches CSS and refuses runtime-state files', {timeout:15000}, async t => {
  const source=fileURLToPath(new URL('..',import.meta.url));
  const name=`reload-fixture-${randomUUID()}.css`;
  const file=join(source,'apps/desktop',name);
  await writeFile(file,'body { --reload-fixture: before; }');
  t.after(()=>rm(file,{force:true}));
  // The page exists before the server starts, like the stylesheet: an HTML file
  // the watcher first sees as added reloads the page, and that reload, landing
  // whenever the polling backend gets to it, threw away the stylesheet the
  // nested-save check adds to the page.
  const html=join(source,'apps/desktop',`${name}.html`);
  await writeFile(html,`<html><head><link rel="stylesheet" href="/${name}"></head><body>Reload check</body></html>`);
  t.after(()=>rm(html,{force:true}));
  const signalHandlers=process.listeners('SIGTERM');
  const {server,url,nonce}=await startDesktopDevServer(source);
  assert.deepEqual(process.listeners('SIGTERM'),signalHandlers,'embedded Vite must not own launcher shutdown');
  t.after(()=>server.close());
  assert.equal(server.httpServer.address().address,'127.0.0.1');
  assert.match(await (await fetch(`${url}/${name}`)).text(),/before/);
  const denied=await fetch(`${url}/@fs/${join(source,'services/runner/code/work.go')}`);
  assert.equal(denied.status,403,'development source outside UI must not be served');
  const browser=await chromium.launch({headless:true});t.after(()=>browser.close());
  const page=await browser.newPage();
  const devHtml=await (await fetch(`${url}/${name}.html`)).text();
  assert.ok(devHtml.includes(`<script type="module" nonce="${nonce}">`), 'the inline React preamble must carry the launcher nonce');
  const connected=new Promise(resolve=>page.on('websocket',socket=>socket.on('framereceived',frame=>{
    if (typeof frame.payload === 'string' && JSON.parse(frame.payload).type === 'connected') resolve();
  })));
  await page.goto(`${url}/${name}.html`);
  await connected;
  await page.waitForFunction(()=>getComputedStyle(document.body).getPropertyValue('--reload-fixture').trim()==='before');
  const changed=new Promise(resolve=>server.watcher.on('change',path=>{if(path===file)resolve();}));
  await writeFile(file,'body { --reload-fixture: after; }');
  await changed;
  await page.waitForFunction(()=>getComputedStyle(document.body).getPropertyValue('--reload-fixture').trim()==='after');
  // Verify the same bound server updates an already-open browser.
  assert.match(await (await fetch(`${url}/${name}?t=${Date.now()}`)).text(),/after/);
  // Editors often replace a file atomically. New nested directories must be
  // watched too; selecting fs.watch must not trade away ordinary save behavior.
  const nested=join(source,'apps/desktop',`${name}-nested`);
  await mkdir(nested);t.after(()=>rm(nested,{recursive:true,force:true}));
  await writeFile(join(nested,'nested.css'),'body { --nested-fixture: before; }');
  await page.evaluate(href=>{const link=document.createElement('link');link.rel='stylesheet';link.href=href;document.head.appendChild(link);},`${url}/${name}-nested/nested.css`);
  await page.waitForFunction(()=>getComputedStyle(document.body).getPropertyValue('--nested-fixture').trim()==='before');
  await writeFile(join(nested,'save.tmp'),'body { --nested-fixture: after; }');
  await rename(join(nested,'save.tmp'),join(nested,'nested.css'));
  await page.waitForFunction(()=>getComputedStyle(document.body).getPropertyValue('--nested-fixture').trim()==='after');
  // Shared packages are outside Vite's app root. A module can be saved before
  // its new stylesheet: the missing-resolution cache must recover on file add.
  const shared=join(source,'packages',`${name}-shared`);
  await mkdir(shared);t.after(()=>rm(shared,{recursive:true,force:true}));
  await writeFile(join(shared,'probe.js'),"import './late.css'; export const ready = true;");
  const moduleURL=`${url}/@fs/${join(shared,'probe.js')}`;
  assert.ok((await fetch(moduleURL)).status>=400,'prime a missing dependency');
  await writeFile(join(shared,'late.css'),'body { --shared-fixture: ready; }');
  await page.waitForFunction(async url=>{try{await import(`${url}?attempt=${Date.now()}`);return getComputedStyle(document.body).getPropertyValue('--shared-fixture').trim()==='ready';}catch{return false;}},moduleURL,{timeout:5000});
});

test('a frozen source beneath .cache serves UI assets but denies its private files', async t => {
  const directory = await mkdtemp(join(tmpdir(), 'lazybox-stage-ui-'));
  let server;
  t.after(async () => {
    try { await server?.close(); }
    finally { await rm(directory, { recursive: true, force: true }); }
  });
  const source = join(directory, '.cache', 'release-[candidate]', 'source');
  for (const path of ['apps/desktop', 'packages', 'node_modules']) await mkdir(join(source, path), { recursive: true });
  await writeFile(join(source, 'package.json'), JSON.stringify({ type: 'module' }));
  await writeFile(join(source, 'apps/desktop/vite.config.ts'), 'export default {};');
  await writeFile(join(source, 'apps/desktop/public.css'), 'body { --stage: visible; }');
  const privateFiles = ['.cache/private.txt', '.git/config', '.env', 'runner.config.json', 'src-tauri/private.txt', 'private.pem'];
  for (const path of privateFiles) {
    const file = join(source, 'apps/desktop', path);
    await mkdir(file.slice(0, file.lastIndexOf('/')), { recursive: true });
    await writeFile(file, 'PRIVATE-FIXTURE-CONTENT');
  }
  const started = await startDesktopDevServer(source);
  server = started.server;
  const url = started.url;
  const canonical = await realpath(source);
  const response = await fetch(`${url}/public.css`);
  assert.equal(response.status, 200); assert.match(await response.text(), /visible/);
  for (const path of privateFiles) {
    const response = await fetch(`${url}/@fs/${join(canonical, 'apps/desktop', path)}`);
    assert.equal(response.status, 403, path);
    assert.ok(!(await response.text()).includes('PRIVATE-FIXTURE-CONTENT'));
  }
});

test('sharing selects existing managed homes without moving credentials or mixing channel state', async t => {
  const root = await mkdtemp(join(tmpdir(), 'lazybox-shared-'));
  t.after(() => rm(root, { recursive: true, force: true }));
  const prior = join(root, 'original');
  const home = join(prior, 'accounts', 'claude-one', 'claude');
  await mkdir(home, { recursive: true });
  const profiles = JSON.stringify([{ id: 'claude-one', provider: 'claude', home, nativeLogin: true }]);
  await writeFile(join(prior, 'accounts', 'profiles.json'), profiles);
  await writeFile(join(home, 'fixture-auth'), 'do-not-copy-or-move');
  assert.equal(await sharedSubscriptionState(root), undefined);
  await shareSubscriptions(root, prior);
  const selected = await sharedSubscriptionState(root);
  assert.ok(selected.endsWith('/original'));
  await shareSubscriptions(root, prior); // Idempotent across launcher restarts.
  assert.equal(await readFile(join(prior, 'accounts', 'profiles.json'), 'utf8'), profiles);
  assert.equal(await readFile(join(home, 'fixture-auth'), 'utf8'), 'do-not-copy-or-move');
  for (const channel of ['dev', 'working']) {
    await assert.rejects(readFile(join(environmentPaths(root, channel).state, 'accounts', 'profiles.json')), { code: 'ENOENT' });
  }
  const ownState = environmentPaths(root, 'dev').state;
  await mkdir(join(ownState, 'accounts'), { recursive: true });
  await writeFile(join(ownState, 'accounts', 'profiles.json'), profiles);
  await assert.rejects(shareSubscriptions(root, prior), /private subscriptions/);
  assert.equal(await sharedSubscriptionState(root), selected);
  await assert.rejects(shareSubscriptions(root, 'relative'), /absolute/);
});


test('a Runner supervisor cannot signal the launcher process group on shutdown', async () => {
 const child=spawnProcessGroup(process.execPath,['-e',"process.on('SIGINT',()=>process.kill(0,'SIGTERM'));console.log('ready');setInterval(()=>{},1000)"],{stdio:['ignore','pipe','pipe']});
 try { await once(child.stdout,'data');const exited=once(child,'exit');child.kill('SIGINT');assert.equal((await exited)[1],'SIGTERM'); }
 finally { await stopProcessGroup(child,50); }
});


test('a permission-denied existence probe is not mistaken for failed termination', async () => {
 let probes=0;const signals=[];
 await stopProcessGroup({pid:123},100,(_pid,signal)=>{signals.push(signal);if(signal===0){const error=new Error('probe');error.code=++probes===1?'EPERM':'ESRCH';throw error;}});
 assert.deepEqual(signals,['SIGTERM',0,0]);
});

test('Dev rebuilds the CLI agents run whenever the Runner source changes, never exposing a partial binary', async t => {
  assert.ok(runnerSourceFile('internal/runner/items.go') && runnerSourceFile('go.mod') && runnerSourceFile('go.sum'));
  assert.ok(!runnerSourceFile('internal/runner/items_test.go') && !runnerSourceFile('README.md'));
  const source = await mkdtemp(join(tmpdir(), 'lazybox-cli-watch-'));
  t.after(() => rm(source, { recursive: true, force: true }));
  const code = join(source, 'services/runner/code'), bin = join(source, 'bin');
  await mkdir(join(code, 'internal'), { recursive: true });
  await writeFile(join(code, 'go.mod'), 'module fixture\n');
  await writeFile(join(code, 'main.go'), 'package main\n');
  let builds = 0, failing = false;
  // A build writes its output in two pieces, as a linker does.
  const build = async (_source, output) => {
    if (failing) throw new Error('go build failed');
    const n = ++builds;
    await writeFile(output, `build ${n} start\n`);
    await new Promise(resolve => setTimeout(resolve, 30));
    await writeFile(output, `build ${n} start\nbuild ${n} end\n`);
  };
  await buildCLI(source, bin, build);
  const cli = join(bin, 'lazybox');
  assert.equal(await readFile(cli, 'utf8'), 'build 1 start\nbuild 1 end\n');
  const since = await runnerSourceFingerprint(source);
  const logs = [];
  const watch = watchCLI({ source, directory: bin, since, build, interval: 20, debounce: 80, log: line => logs.push(line) });
  t.after(() => watch.close());
  // Whoever runs the CLI meanwhile only ever finds a whole build.
  let reading = true, torn = '';
  const reader = (async () => {
    while (reading) {
      const text = await readFile(cli, 'utf8');
      if (!/^build (\d+) start\nbuild \1 end\n$/.test(text)) torn = text;
      await new Promise(resolve => setImmediate(resolve));
    }
  })();
  const settle = ms => new Promise(resolve => setTimeout(resolve, ms));
  const until = async (what, ok) => { for (const end = Date.now() + 5000; !(await ok());) { if (Date.now() > end) assert.fail(what); await settle(10); } };
  // Not what the CLI is built from: nothing to build.
  await writeFile(join(code, 'README.md'), 'notes');
  await writeFile(join(code, 'internal', 'x_test.go'), 'package internal\n');
  await settle(300);
  assert.equal(builds, 1);
  // Issue the burst in one turn. A loaded host can stretch an awaited
  // 20 ms pause past the debounce interval, legitimately starting another build.
  for (let i = 0; i < 4; i++) writeFileSync(join(code, 'internal', 'x.go'), `package internal\n// ${i}\n`);
  await until('the CLI was not rebuilt', async () => (await readFile(cli, 'utf8')).includes('build 2 end'));
  await settle(300);
  assert.equal(builds, 2);
  // go.mod counts too.
  await writeFile(join(code, 'go.mod'), 'module fixture\n\ngo 1.25\n');
  await until('go.mod did not rebuild the CLI', async () => (await readFile(cli, 'utf8')).includes('build 3 end'));
  // A broken build keeps the previous CLI and says so.
  failing = true;
  await writeFile(join(code, 'main.go'), 'package main\nbroken(\n');
  await until('the failure is not said', () => logs.some(line => line.includes('previous one stays')));
  assert.equal(await readFile(cli, 'utf8'), 'build 3 start\nbuild 3 end\n');
  reading = false; await reader;
  assert.equal(torn, '', 'an agent could run a half-written CLI');
  assert.deepEqual((await readdir(bin)).filter(name => name !== 'lazybox'), [], 'no temporary build is left behind');
  await watch.close();
});

test('managed Dev waits for its own native desktop receipt and detects an exited build', async t => {
  const { waitForDesktop } = await import('./environments.mjs');
  const root = await mkdtemp(join(tmpdir(), 'lazybox-desktop-ready-'));
  t.after(() => rm(root, { recursive: true, force: true }));
  const receipt = join(root, 'desktop-ready');
  const desktop = { pid: 1, exitCode: null };
  let done = false;
  const waiting = waitForDesktop(receipt, desktop, () => false).then(() => { done = true; });
  await new Promise(resolve => setTimeout(resolve, 20));
  assert.equal(done, false);
  await writeFile(receipt, 'ready');
  await waiting;
  await assert.rejects(waitForDesktop(receipt, { ...desktop, exitCode: 1 }, () => false), /did not finish/);
});

// Regression: Working's app was only linker-signed (its executable, not its
// bundle), so macOS had no signed identity to allow notifications for.
test('a Working build signs its bundle ad hoc, as a release build does', () => {
  const paths = environmentPaths('/tmp/lazybox-environments', 'working');
  const config = workingTauriConfig(paths);
  assert.equal(config.identifier, paths.identifier);
  assert.equal(config.productName, paths.productName);
  assert.equal(config.bundle.macOS.signingIdentity, '-');
});

// Regression: each failed stage left its whole source, dependencies and app
// (about 9 GB) under releases/, until the disk was full.
test('a failed stage keeps only its gates.json', async () => {
  const { mkdtemp, mkdir: makeDir, writeFile: write, readdir: list } = await import('node:fs/promises');
  const { tmpdir } = await import('node:os');
  const { join: joinPath } = await import('node:path');
  const destination = await mkdtemp(joinPath(tmpdir(), 'lazybox-candidate-'));
  await write(joinPath(destination, 'gates.json'), '{"status":"failed"}');
  await makeDir(joinPath(destination, 'source', 'node_modules'), { recursive: true });
  await write(joinPath(destination, 'source', 'node_modules', 'big'), 'x');
  await makeDir(joinPath(destination, 'Lazybox.app'));
  await pruneFailedCandidate(destination);
  assert.deepEqual(await list(destination), ['gates.json']);
});

// An interrupted stage skips its own cleanup, and failed candidates from
// before the cleanup existed stay: the next stage sweeps them, never the
// pinned release or the one before it, never one that passed.
test('a stage sweeps what earlier stages left, never a release that can run', async () => {
  const { mkdtemp, mkdir: makeDir, writeFile: write, readdir: list } = await import('node:fs/promises');
  const { tmpdir } = await import('node:os');
  const { join: joinPath } = await import('node:path');
  const root = await mkdtemp(joinPath(tmpdir(), 'lazybox-environments-'));
  const candidate = async (id, passed) => {
    const dir = joinPath(root, 'releases', id);
    await makeDir(joinPath(dir, 'source'), { recursive: true });
    await write(joinPath(dir, 'gates.json'), '{}');
    if (passed) await write(joinPath(dir, 'release.json'), '{}');
    return dir;
  };
  const failed = await candidate('failed', false), interrupted = await candidate('interrupted', false);
  const passed = await candidate('passed', true), pinnedOld = await candidate('pinned-old', false), previous = await candidate('previous', false);
  await makeDir(joinPath(root, 'working'), { recursive: true });
  await write(joinPath(root, 'working', 'current.json'), JSON.stringify({ id: 'pinned-old', previous: 'previous' }));
  await sweepFailedCandidates(root);
  assert.deepEqual(await list(failed), ['gates.json']);
  assert.deepEqual(await list(interrupted), ['gates.json']);
  for (const kept of [passed, pinnedOld, previous]) assert.ok((await list(kept)).includes('source'), kept);
  await sweepFailedCandidates(joinPath(root, 'nowhere'));
});
