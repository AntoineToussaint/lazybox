#!/usr/bin/env node
// Installs Lazybox from this clone (docs/install.md#install-from-source):
//
//   make install   [PREFIX=~/Applications] [LINK_DIR=<dir>] [OPEN=0] [SMOKE=1]
//   make uninstall [PREFIX=…] [PURGE=1]
//   make update    git pull, then make install
//   node scripts/release/install.mjs check | install | uninstall
//
// `install` checks what the build needs and says how to get anything
// missing, builds the app from source (scripts/release/build.mjs
// --from-source: ad-hoc signed, no updater, no secrets), puts Lazybox.app in
// PREFIX (/Applications by default), and then does what the app's first-run
// window does, through the same bundled CLI: `lazybox service install` (the
// Runner as a launchd service) and `lazybox link` (the CLI on PATH). Running
// agents keep running across a reinstall: only the Runner restarts.
//
// `uninstall` runs the installed `lazybox uninstall` (the service, the CLI
// links, completions; with PURGE=1 also conversations and settings, after
// asking), then removes the app. Without PURGE your data stays, so a later
// install picks up where you were.
//
// Everything the CLI touches follows HOME and LAZYBOX_SERVICE_LABEL, so a
// qualification run can install into a scratch home under a test label
// without touching a real installation (RELEASING.md).
import { spawnSync } from 'node:child_process';
import { existsSync, readFileSync, statfsSync, accessSync, constants, mkdirSync, renameSync, rmSync } from 'node:fs';
import { homedir } from 'node:os';
import { dirname, join, resolve, isAbsolute } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');

/** The release app's bundle identifier (packaging/release.tauri.json). */
export const APP_ID = 'ai.lazybox.app';
export const APP_NAME = 'Lazybox.app';
/** Free space the first build needs: dependencies, the Rust build and the app. */
export const BUILD_SPACE_GB = 8;
/** What the first build and a rebuild take on an Apple Silicon Mac (measured; RELEASING.md). */
export const FIRST_BUILD = '5 to 10 minutes';
export const REBUILD = 'about 3 minutes';

// ---- what the build needs ----

/** Dotted numeric versions, compared part by part ("1.27.3" < "1.100.0"). */
export function compareVersions(a, b) {
  const pa = String(a).split('.').map(Number), pb = String(b).split('.').map(Number);
  for (let i = 0; i < Math.max(pa.length, pb.length); i++) {
    const d = (pa[i] || 0) - (pb[i] || 0);
    if (d) return d < 0 ? -1 : 1;
  }
  return 0;
}

/** The first x.y[.z] in a tool's --version output. */
export function versionIn(text) {
  return /(\d+\.\d+(?:\.\d+)?)/.exec(String(text || ''))?.[1] || '';
}

/** The versions this checkout pins: go.mod, package.json engines, the desktop crate's rust-version. */
export function requirements(dir = root) {
  const goMod = readFileSync(join(dir, 'services/runner/code/go.mod'), 'utf8');
  const go = /^go\s+(\d+\.\d+(?:\.\d+)?)\s*$/m.exec(goMod)?.[1];
  const pkg = JSON.parse(readFileSync(join(dir, 'package.json'), 'utf8'));
  // engines say ">=24": the lowest version, as written.
  const atLeast = value => /(\d+(?:\.\d+)*)/.exec(value || '')?.[1];
  const cargo = readFileSync(join(dir, 'apps/desktop/src-tauri/Cargo.toml'), 'utf8');
  const rust = /^rust-version\s*=\s*"([^"]+)"/m.exec(cargo)?.[1];
  const macos = JSON.parse(readFileSync(join(dir, 'packaging/release.tauri.json'), 'utf8')).bundle?.macOS?.minimumSystemVersion;
  const need = { go, node: atLeast(pkg.engines?.node), npm: atLeast(pkg.engines?.npm), rust, macos };
  for (const [name, value] of Object.entries(need)) if (!value) throw new Error(`Could not read the ${name} version this checkout needs`);
  return need;
}

/**
 * Each prerequisite with whether this Mac has it and, when not, the command
 * that gets it. `found` is what probe() saw; empty strings mean "not found".
 */
export function evaluate(need, found) {
  const tool = (name, have, minimum, fix, why) => ({
    name: `${name} ${minimum} or later`, why, have: have || 'not found',
    ok: !!have && compareVersions(have, minimum) >= 0, fix: have ? fix.upgrade : fix.install,
  });
  return [
    { name: 'An Apple Silicon Mac', why: 'Lazybox is built for Apple Silicon only', have: found.arm64 ? 'Apple Silicon' : found.arch || 'another processor', ok: !!found.arm64, fix: 'Lazybox does not run on Intel Macs yet.' },
    tool('macOS', found.macos, need.macos, { install: '', upgrade: 'System Settings → General → Software Update' }, 'the app'),
    { name: 'Xcode Command Line Tools', why: 'the compiler, linker, git and codesign', have: found.clt ? 'installed' : 'not found', ok: !!found.clt, fix: 'xcode-select --install' },
    tool('Go', found.go, need.go, { install: 'brew install go   (or download it from https://go.dev/dl/)', upgrade: 'brew upgrade go   (or download it from https://go.dev/dl/)' }, 'the Runner and the lazybox command'),
    tool('Node.js', found.node, need.node, { install: 'brew install node   (or download it from https://nodejs.org)', upgrade: 'brew upgrade node   (or download it from https://nodejs.org)' }, "the app's interface and the Claude worker"),
    tool('npm', found.npm, need.npm, { install: 'npm comes with Node.js: install Node.js first', upgrade: `npm install --global npm@${need.npm}` }, 'the locked JavaScript dependencies'),
    tool('Rust', found.rust, need.rust, {
      install: "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   (then open a new terminal)",
      upgrade: found.rustup ? 'rustup update stable' : "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   (then open a new terminal)",
    }, 'the native app (Tauri)'),
    { name: `${BUILD_SPACE_GB} GB of free disk space`, why: 'dependencies and the native build', have: found.freeGB === undefined ? 'unknown' : `${found.freeGB.toFixed(1)} GB free`, ok: found.freeGB === undefined || found.freeGB >= BUILD_SPACE_GB, fix: 'Free up some space on this disk, then run make install again.' },
  ];
}

function capture(command, args, options = {}) {
  const result = spawnSync(command, args, { encoding: 'utf8', ...options });
  return result.status === 0 ? `${result.stdout || ''}${result.stderr || ''}`.trim() : '';
}

/** What this Mac has. Runs each tool where the build runs it, so rustup and Go pick this checkout's toolchains. */
export function probe(dir = root) {
  const arm64 = capture('/usr/sbin/sysctl', ['-n', 'hw.optional.arm64']) === '1';
  let freeGB;
  try { const s = statfsSync(dir); freeGB = s.bavail * s.bsize / 1e9; } catch { /* unknown */ }
  return {
    arm64, arch: capture('/usr/bin/uname', ['-m']),
    macos: versionIn(capture('/usr/bin/sw_vers', ['-productVersion'])),
    clt: !!capture('/usr/bin/xcode-select', ['-p']),
    go: versionIn(capture('go', ['version'], { cwd: join(dir, 'services/runner/code') }).replace(/^go version go/, '')),
    node: versionIn(process.version),
    npm: versionIn(capture('npm', ['--version'])),
    rust: versionIn(capture('rustc', ['--version'], { cwd: dir })),
    rustup: !!capture('rustup', ['--version']),
    freeGB,
  };
}

/** Prints the check; true when everything is there. */
export function report(checks, print = console.log) {
  print('Checking what the build needs:');
  for (const c of checks) {
    print(`  ${c.ok ? '✓' : '✗'} ${c.name}${c.ok ? ` (${c.have})` : `: ${c.have}`}`);
    if (!c.ok && c.fix) print(`      ${c.fix}`);
  }
  const missing = checks.filter(c => !c.ok);
  if (missing.length) print(`\nInstall what is missing above (${missing.length === 1 ? 'one thing' : `${missing.length} things`}), then run make install again.`);
  return !missing.length;
}

// ---- installing ----

/** Where the app goes: PREFIX, ~ expanded, /Applications by default. */
export function appPath(prefix, home = homedir()) {
  const dir = !prefix ? '/Applications' : prefix === '~' ? home : prefix.startsWith('~/') ? join(home, prefix.slice(2)) : prefix;
  if (!isAbsolute(dir)) throw new Error(`PREFIX must be an absolute folder, not ${prefix}`);
  return join(dir, APP_NAME);
}

/** The processes running this copy of the app (never another copy with the same identity). */
export function runningCopies(app, psOutput) {
  const prefix = join(app, 'Contents', 'MacOS') + '/';
  return psOutput.split('\n').map(line => /^\s*(\d+)\s+(.*)$/.exec(line)).filter(Boolean)
    .filter(([, , command]) => command.startsWith(prefix)).map(([, pid]) => Number(pid));
}

function plistValue(app, key) {
  return capture('/usr/libexec/PlistBuddy', ['-c', `Print :${key}`, join(app, 'Contents', 'Info.plist')]);
}

function readJSON(path) { try { return JSON.parse(readFileSync(path, 'utf8')); } catch { return undefined; } }

/** What is at the destination now, in words. */
export function describeInstalled(app) {
  const runtime = join(app, 'Contents', 'Resources', 'runtime');
  const version = existsSync(join(runtime, 'VERSION')) ? readFileSync(join(runtime, 'VERSION'), 'utf8').trim() : '';
  const source = readJSON(join(runtime, 'SOURCE.json'));
  return `Lazybox ${version || '(unknown version)'}${source ? `, built from source${source.commit ? ` at ${source.commit.slice(0, 12)}` : ''}` : ''}`;
}

function run(command, args, options = {}) {
  const result = spawnSync(command, args, { stdio: 'inherit', ...options });
  if (result.error) throw result.error;
  return result.status ?? 1;
}

async function quit(app) {
  const pids = runningCopies(app, capture('/bin/ps', ['-axo', 'pid=,comm=']));
  if (!pids.length) return false;
  console.log(`  Closing the running Lazybox (${app}); agents keep running.`);
  for (const pid of pids) { try { process.kill(pid, 'SIGTERM'); } catch { /* already gone */ } }
  for (let i = 0; i < 50; i++) {
    if (!runningCopies(app, capture('/bin/ps', ['-axo', 'pid=,comm='])).length) return true;
    await new Promise(done => setTimeout(done, 100));
  }
  throw new Error(`Lazybox at ${app} did not quit; quit it, then run make install again`);
}

function writable(dir) {
  try { accessSync(dir, constants.W_OK); return true; } catch { return false; }
}

function cliJSON(cli, args) {
  const result = spawnSync(cli, [...args, '--json'], { encoding: 'utf8' });
  try { return JSON.parse(result.stdout.trim()); } catch { return undefined; }
}

async function install({ prefix, linkDir, open, smoke }) {
  const checks = evaluate(requirements(), probe());
  if (!report(checks)) process.exit(1);
  const app = appPath(prefix);
  const folder = dirname(app);
  if (existsSync(folder) && !writable(folder)) throw new Error(`Cannot write to ${folder}. Install for yourself instead: make install PREFIX=~/Applications`);
  if (existsSync(app) && plistValue(app, 'CFBundleIdentifier') !== APP_ID) throw new Error(`${app} is not Lazybox (its identifier is not ${APP_ID}); move it away or choose another PREFIX`);
  if (linkDir && !isAbsolute(linkDir)) throw new Error('LINK_DIR must be an absolute folder');

  const { builtApp } = await import('./build.mjs');
  const fresh = !existsSync(join(root, '.cache', 'release', 'target', 'release'));
  console.log(`\nBuilding Lazybox from source: ${fresh ? `the first build takes ${FIRST_BUILD}` : `a rebuild takes ${REBUILD}`}. Nothing is signed or published; no secrets are needed.`);
  const started = Date.now();
  if (run(process.execPath, [join(root, 'scripts/release/build.mjs'), '--from-source', ...(smoke ? [] : ['--skip-smoke'])], { cwd: root }) !== 0) throw new Error('The build failed (above). Fix what it says, then run make install again; SUPPORT.md says how to report it.');
  console.log(`  Built in ${Math.round((Date.now() - started) / 60000)} min.`);

  console.log(`\nInstalling ${app}`);
  const replacing = existsSync(app);
  if (replacing) console.log(`  Replacing ${describeInstalled(app)}.`);
  const wasOpen = await quit(app);
  mkdirSync(folder, { recursive: true });
  const incoming = join(folder, '.Lazybox.app.installing'), previous = join(folder, '.Lazybox.app.previous');
  rmSync(incoming, { recursive: true, force: true }); rmSync(previous, { recursive: true, force: true });
  if (run('/usr/bin/ditto', [builtApp, incoming]) !== 0) throw new Error(`Could not copy the app to ${folder}`);
  if (existsSync(app)) renameSync(app, previous);
  renameSync(incoming, app);
  rmSync(previous, { recursive: true, force: true });

  const cli = join(app, 'Contents', 'Resources', 'runtime', 'bin', 'lazybox');
  console.log('\nSetting up the Runner (what the first launch does: lazybox service install)');
  if (run(cli, ['service', 'install']) !== 0) throw new Error(`The Runner service did not start. See \`${cli} service logs\` and \`${cli} doctor\`.`);
  console.log('\nPutting the lazybox command on your PATH (lazybox link)');
  const linked = run(cli, ['link', ...(linkDir ? ['--dir', linkDir] : [])]) === 0;
  if (!linked) console.log(`  The command was not linked (above). Lazybox works without it; run \`${cli} link --dir <a folder on your PATH>\` later.`);

  const status = cliJSON(cli, ['service', 'status']) || {};
  const version = readFileSync(join(app, 'Contents', 'Resources', 'runtime', 'VERSION'), 'utf8').trim();
  console.log(`\n✓ ${describeInstalled(app)} is installed in ${folder}.`);
  console.log(`  Runner: ${status.reachable ? 'running' : 'installed, not answering yet'}${status.log ? ` (log: ${status.log.replace(homedir(), '~')})` : ''}`);
  if (open) {
    if (run('/usr/bin/open', [app]) !== 0) console.log(`  Open ${app} yourself.`);
  }
  console.log(`
Next:
  1. ${open ? 'Lazybox is opening' : `Open ${app}`}${wasOpen ? ' again' : ''}. ${replacing
    ? 'Your workspaces and conversations are where you left them;\n     Help → What\'s new lists what changed.'
    : 'The welcome screen helps you connect\n     Claude Code or Codex (sign in as you usually do) and start a task.'}
  2. In a new terminal, check everything: lazybox doctor
  3. What to try first, and how to report a problem: docs/testers.md
  Later: make update (pull, rebuild, reinstall; your data stays), make uninstall.
  Lazybox ${version}: your data is in ~/Library/Application Support/Lazybox.`);
}

async function uninstall({ prefix, purge }) {
  const app = appPath(prefix);
  const cli = join(app, 'Contents', 'Resources', 'runtime', 'bin', 'lazybox');
  if (!existsSync(cli)) {
    console.log(`No Lazybox at ${app}${prefix ? '' : ' (pass PREFIX= if you installed it elsewhere)'}.`);
    return;
  }
  if (plistValue(app, 'CFBundleIdentifier') !== APP_ID) throw new Error(`${app} is not Lazybox; nothing was removed`);
  // The CLI lists what goes; the app goes too, so its own "drag the app to
  // the Trash" advice is not shown (--json), and PURGE asks here.
  const plan = cliJSON(cli, ['uninstall', '--dry-run', ...(purge ? ['--remove-data'] : [])]);
  if (!plan) throw new Error(`\`${cli} uninstall --dry-run\` could not say what it would remove; nothing was removed`);
  console.log('Removing:');
  for (const r of [...(plan.remove || []), { what: 'the app', path: app }]) console.log(`  - ${r.what} ${r.path.replace(homedir(), '~')}`);
  if (purge) {
    if (!process.stdin.isTTY) throw new Error('PURGE=1 deletes your conversations and workspaces; run it from a terminal to confirm');
    const answer = await new Promise(done => {
      process.stdout.write('This deletes your conversations, subscriptions and workspaces for good, including uncommitted work in them. Type delete to continue: ');
      process.stdin.once('data', data => { process.stdin.pause(); done(String(data).trim()); });
      process.stdin.once('end', () => done(''));
    });
    if (answer !== 'delete') { console.log('Nothing was removed.'); return; }
  }
  await quit(app);
  const result = spawnSync(cli, ['uninstall', '--json', ...(purge ? ['--remove-data', '--yes'] : [])], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'inherit'] });
  if (result.status !== 0) throw new Error('lazybox uninstall did not finish (above); the app was left in place');
  rmSync(app, { recursive: true, force: true });
  let kept = '';
  try { const report = JSON.parse(result.stdout); const live = report.keptRunning || {}; const total = Object.values(live).reduce((sum, n) => sum + (typeof n === 'number' ? n : 0), 0); if (total) kept = `\n  Still running: ${live.agents || 0} agents, ${live.commands || 0} commands, ${live.terminals || 0} terminals. They stop by themselves within a day, or are picked up again if you install before then.`; } catch { /* the summary is enough */ }
  console.log(`✓ Lazybox is uninstalled: the app, its Runner service and the lazybox command.${purge ? ' Your data is deleted.' : ' Your conversations, subscriptions and settings are kept; make install brings them back, make uninstall PURGE=1 deletes them.'}${kept}`);
}

async function main() {
  const [command = 'check'] = process.argv.slice(2);
  const env = process.env;
  const flag = (value, fallback) => value === undefined || value === '' ? fallback : !['0', 'no', 'false'].includes(value.toLowerCase());
  if (command === 'check') process.exit(report(evaluate(requirements(), probe())) ? 0 : 1);
  else if (command === 'install') await install({ prefix: env.PREFIX, linkDir: env.LINK_DIR, open: flag(env.OPEN, true), smoke: flag(env.SMOKE, false) });
  else if (command === 'uninstall') await uninstall({ prefix: env.PREFIX, purge: flag(env.PURGE, false) });
  else throw new Error(`Unknown command ${command}: check, install or uninstall`);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main().catch(error => { console.error(`\n✗ ${error.message}`); process.exitCode = 1; });
