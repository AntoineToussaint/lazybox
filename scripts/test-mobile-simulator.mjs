// The iPhone app (apps/mobile) end to end on the iOS Simulator, against the
// fixture Runner's device gateway: pair from a link → Inbox → open → Allow →
// reply → Send now behind a running turn (apps/mobile/e2e/pair-approve-send.yaml,
// driven by Maestro). Then checks what the Runner received: only device
// credentials, only allowlisted RPCs, the approval and the messages from the
// paired device, and the live stream opened from a cursor.
//
// Needs: the Release app installed on a booted simulator (apps/mobile/README.md;
// it carries its own bundle, so no Metro, no dev menu and no debugger window
// can open on the desktop during a run), and Maestro (`MAESTRO=/path/to/maestro`,
// Java 17+). The simulator runs headless: `xcrun simctl boot`, no Simulator.app.
//
//   node scripts/test-mobile-simulator.mjs [--appearance dark|light|both] [--flow main|lifecycle|gateway|all]
//
// `lifecycle` (apps/mobile/e2e/stream-lifecycle.yaml) is the streaming
// spike's evidence: a stream idle for LAZYBOX_IDLE_MS (default 3 minutes)
// still delivers, closes in the background, and resumes from its cursor.
//
// `gateway` (apps/mobile/e2e/real-gateway.yaml) pairs with the Runner's real
// device gateway, served on loopback by the Go test TestSimulatorGateway: the
// Runner's own pairing, credentials, allowlist and Connect, with a stand-in
// for execution behind it. Never a Runner of yours.
//
// Screenshots: .cache/mobile/<appearance>/*.png (the others in .cache/mobile/lifecycle and gateway). It never touches a real
// Runner: the fixture is a fake on a loopback port of its own.
import { spawn, spawnSync } from 'node:child_process';
import { mkdir, readdir, rename, rm } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { join } from 'node:path';
import assert from 'node:assert/strict';
import { startPhoneFixture } from './mobile-fixture.mjs';

const root = fileURLToPath(new URL('..', import.meta.url));
const flows = { main: join(root, 'apps/mobile/e2e/pair-approve-send.yaml'), lifecycle: join(root, 'apps/mobile/e2e/stream-lifecycle.yaml'), gateway: join(root, 'apps/mobile/e2e/real-gateway.yaml') };
const IDLE_MS = Number(process.env.LAZYBOX_IDLE_MS || 180_000);
const maestro = process.env.MAESTRO || 'maestro';
const arg = name => { const i = process.argv.indexOf(name); return i > 0 ? process.argv[i + 1] : undefined; };
const appearances = (arg('--appearance') || 'both') === 'both' ? ['dark', 'light'] : [arg('--appearance')];
const which = arg('--flow') || 'all';
const until = async (check, timeout, what) => { const end = Date.now() + timeout; while (!check()) { assert.ok(Date.now() < end, `timed out waiting for ${what}`); await new Promise(resolve => setTimeout(resolve, 200)); } };
const flatten = async out => { for (const entry of await readdir(out, { recursive: true })) if (entry.endsWith('.png') && entry.includes('/')) await rename(join(out, entry), join(out, entry.split('/').at(-1))); };

const run = (command, args, options = {}) => spawnSync(command, args, { encoding: 'utf8', ...options });
// The fixture answers from this process: Maestro must run without blocking its event loop.
const runAsync = (command, args, timeout) => new Promise(resolve => {
  const child = spawn(command, args, { stdio: ['ignore', 'pipe', 'pipe'] });
  let stdout = '';
  child.stdout.on('data', data => { stdout += data; }); child.stderr.on('data', data => { stdout += data; });
  const timer = setTimeout(() => child.kill('SIGTERM'), timeout);
  child.on('close', status => { clearTimeout(timer); resolve({ status, stdout }); });
});
const booted = JSON.parse(run('xcrun', ['simctl', 'list', 'devices', 'booted', '-j']).stdout).devices;
const device = process.env.LAZYBOX_SIMULATOR || Object.values(booted).flat().find(d => /iPhone/.test(d.name))?.udid;
assert.ok(device, 'Boot an iPhone simulator first (xcrun simctl boot <udid>), or set LAZYBOX_SIMULATOR.');
assert.equal(run('xcrun', ['simctl', 'get_app_container', device, 'ai.lazybox.mobile']).status, 0, 'Install the Release app first (apps/mobile/README.md).');
// One flow at a time: a Maestro driver left from the previous flow holds the
// driver's port, and a new one then fails to bind and never starts. Stop it first.
const maestroFlow = async (out, link, flow, timeout) => {
  run('xcrun', ['simctl', 'terminate', device, 'dev.mobile.maestro-driver-iosUITests.xctrunner']);
  await new Promise(resolve => setTimeout(resolve, 2000));
  return runAsync(maestro, ['--device', device, 'test', '--headless', '--test-output-dir', out, '-e', `LINK=${encodeURIComponent(link)}`, flow], timeout);
};

for (const appearance of which === 'all' || which === 'main' ? appearances : []) {
  run('xcrun', ['simctl', 'ui', device, 'appearance', appearance]);
  const phone = await startPhoneFixture();
  const out = join(root, '.cache/mobile', appearance);
  await rm(out, { recursive: true, force: true }); await mkdir(out, { recursive: true });
  try {
    const result = await maestroFlow(out, phone.link, flows.main, 600_000);
    process.stdout.write(result.stdout.split('\n').filter(line => /COMPLETED|FAILED|SKIPPED|Assert|Error/.test(line)).join('\n') + '\n');
    assert.equal(result.status, 0, `the Maestro flow failed (${appearance}); see ${out}`);

    const f = phone.f;
    const [deviceId] = f.remote.devices.keys();
    assert.ok(deviceId, 'the app paired');
    assert.equal(f.remote.pairRequests.length, 1);
    // Every call the app made carried its device credential, and none was refused by the allowlist.
    assert.deepEqual(f.remote.deviceDenied, [], 'the app only called RPCs a device may call');
    const fromApp = f.calls.filter(call => call.device === deviceId);
    assert.ok(fromApp.length > 0 && f.calls.filter(call => !call.device && call.method !== 'BeginDevicePairing').length === 0, 'no owner-bearer call came from the phone');
    const applied = f.mutations.filter(m => m.method === 'Apply' && m.device === deviceId).map(m => m.body);
    assert.deepEqual(applied.filter(b => b.approval).map(b => [b.sessionId, b.approval.requestId, b.approval.decision]), [['deps', 'a-deps', 'DECISION_ACCEPT']]);
    assert.deepEqual(applied.filter(b => b.message).map(b => [b.sessionId, b.message.text]), [['deps', 'Ship it and open a pull request.'], ['index', 'Skip the admin routes for now.']]);
    // Send now: the queued message's own operation id, delivered "now".
    const queued = applied.find(b => b.message?.text === 'Skip the admin routes for now.');
    assert.deepEqual(applied.filter(b => b.deliverQueued).map(b => [b.sessionId, b.deliverQueued.operationId, b.deliverQueued.how]), [['index', queued.operationId, 'now']]);
    // The live conversation streamed: WatchEvents opened through expo/fetch from a cursor the tail read gave it.
    const watches = fromApp.filter(call => call.method === 'WatchEvents' && call.body.sessionId === 'index');
    assert.ok(watches.length >= 1 && watches.every(call => BigInt(call.body.after || 0) > 0n), 'the stream continues from the tail, not from the start');
    console.log(`ok ${appearance}: paired as ${deviceId}; ${fromApp.length} device calls; screenshots in ${out}`);
  } finally { await phone.close(); }
  // Maestro writes screenshots under a dated folder: keep them flat, by step.
  await flatten(out);
}

if (which === 'lifecycle' || which === 'all') {
  run('xcrun', ['simctl', 'ui', device, 'appearance', 'dark']);
  const phone = await startPhoneFixture();
  const out = join(root, '.cache/mobile/lifecycle');
  await rm(out, { recursive: true, force: true }); await mkdir(out, { recursive: true });
  try {
    const flow = maestroFlow(out, phone.link, flows.lifecycle, IDLE_MS + 600_000);
    const index = () => phone.streams.filter(s => s.session === 'index');
    const opens = () => index().filter(s => s.kind === 'open');
    const count = () => index().filter(s => s.kind === 'count').at(-1)?.count ?? 0;
    // The Runner's side: silence for IDLE_MS on an open stream (heartbeats only), then a word.
    await until(() => count() === 1, 120_000, 'the live conversation to open its stream');
    const idleFrom = Date.now();
    await new Promise(resolve => setTimeout(resolve, IDLE_MS));
    assert.equal(opens().length, 1, 'the idle stream stayed the same stream: no reconnect while idle');
    phone.say('\n\nStill here after a quiet stretch.');
    const idleFor = Date.now() - idleFrom;
    // The app goes to the background: its stream ends; what is written meanwhile waits for it.
    await until(() => count() === 0, 120_000, 'the stream to close in the background');
    const closedAt = Date.now();
    phone.say('\n\nWritten while you were away.');
    await until(() => opens().length >= 2 && count() === 1, 120_000, 'the stream to reopen in the foreground');
    const reopened = opens().at(-1);
    const result = await flow;
    process.stdout.write(result.stdout.split('\n').filter(line => /COMPLETED|FAILED|SKIPPED|Assert|Error/.test(line)).join('\n') + '\n');
    assert.equal(result.status, 0, `the lifecycle flow failed; see ${out}`);
    const [first] = opens();
    assert.ok(first.after > 0n && reopened.after > first.after, 'the stream resumed from its cursor, past what it had read');
    console.log(`ok lifecycle: one stream idle ${Math.round(idleFor / 1000)} s then delivered; closed in the background; reopened after ${first.after} → ${reopened.after}, ${Math.round((reopened.at - closedAt) / 1000)} s after it closed`);
  } finally { await phone.close(); }
  await flatten(out);
}

if (which === 'gateway' || which === 'all') {
  run('xcrun', ['simctl', 'ui', device, 'appearance', 'dark']);
  const out = join(root, '.cache/mobile/gateway');
  await rm(out, { recursive: true, force: true }); await mkdir(out, { recursive: true });
  const state = join(out, 'gateway.json');
  const go = new Promise(resolve => {
    const child = spawn('go', ['test', './internal/remote/', '-run', '^TestSimulatorGateway$', '-count=1', '-timeout=10m', '-v'], { cwd: join(root, 'services/runner/code'), env: { ...process.env, LAZYBOX_SIMULATOR_GATEWAY: state, LAZYBOX_SIMULATOR_SECONDS: '300' }, stdio: ['ignore', 'pipe', 'pipe'] });
    let text = ''; child.stdout.on('data', d => { text += d; }); child.stderr.on('data', d => { text += d; });
    child.on('close', status => resolve({ status, text }));
  });
  const { readFile } = await import('node:fs/promises');
  // The Go test compiles first, then writes the link once it serves.
  let link = '';
  for (const end = Date.now() + 300_000; !link; await new Promise(resolve => setTimeout(resolve, 300))) {
    assert.ok(Date.now() < end, 'timed out waiting for the Go test to serve the gateway');
    link = await readFile(state, 'utf8').then(text => JSON.parse(text).link, () => '');
  }
  const result = await maestroFlow(out, link, flows.gateway, 400_000);
  process.stdout.write(result.stdout.split('\n').filter(line => /COMPLETED|FAILED|SKIPPED|Assert|Error/.test(line)).join('\n') + '\n');
  const verdict = await go;
  process.stdout.write(verdict.text.split('\n').filter(line => /simulator_test|PASS|FAIL|ok /.test(line)).join('\n') + '\n');
  assert.equal(result.status, 0, `the gateway flow failed; see ${out}`);
  assert.equal(verdict.status, 0, 'the Go side of the gateway test failed');
  await flatten(out);
  console.log('ok gateway: paired with the Runner\'s real device gateway and allowed a request through it');
}
