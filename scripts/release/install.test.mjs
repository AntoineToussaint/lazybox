import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { appPath, compareVersions, evaluate, linuxRuntimePath, report, requirements, runningCopies, versionIn } from './install.mjs';
import { sourceManifest, sourceVersion, tauriReleaseConfig } from './build.mjs';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');
const need = { go: '1.27.3', node: '24', npm: '11', rust: '1.90', macos: '13.5' };
const everything = { arm64: true, arch: 'arm64', macos: '15.1', clt: true, go: '1.27.3', node: '24.1.0', npm: '11.0.0', rust: '1.90.0', rustup: true, freeGB: 40 };

test('the prerequisites are the versions this checkout pins', async () => {
  const pinned = requirements(root);
  const goMod = await readFile(resolve(root, 'services/runner/code/go.mod'), 'utf8');
  assert.equal(pinned.go, /^go (\S+)$/m.exec(goMod)[1]);
  const pkg = JSON.parse(await readFile(resolve(root, 'package.json'), 'utf8'));
  assert.equal(pinned.node, /\d+(?:\.\d+)*/.exec(pkg.engines.node)[0]);
  assert.equal(pinned.npm, /\d+(?:\.\d+)*/.exec(pkg.engines.npm)[0]);
  assert.match(pinned.rust, /^\d+\.\d+/);
  assert.equal(pinned.macos, '13.5');
});

test('versions compare as numbers, part by part', () => {
  assert.ok(compareVersions('1.100.0', '1.90') > 0, 'not as strings');
  assert.equal(compareVersions('24', '24.0.0'), 0);
  assert.ok(compareVersions('1.27.2', '1.27.3') < 0);
  assert.equal(versionIn('go version go1.27.4 darwin/arm64'.replace(/^go version go/, '')), '1.27.4');
  assert.equal(versionIn('rustc 1.100.0 (0ef1bc386 2026-11-09)'), '1.100.0');
  assert.equal(versionIn(''), '');
});

test('everything present passes, and each missing tool names the command that installs it', () => {
  assert.ok(evaluate(need, everything).every(c => c.ok));
  const missing = evaluate(need, { ...everything, clt: false, go: '', node: '22.3.0', rust: '', rustup: false, freeGB: 2 });
  const failed = Object.fromEntries(missing.filter(c => !c.ok).map(c => [c.name.split(' ')[0], c]));
  assert.deepEqual(Object.keys(failed).sort(), ['8', 'Go', 'Node.js', 'Rust', 'Xcode'].sort());
  assert.equal(failed.Xcode.fix, 'xcode-select --install');
  assert.match(failed.Go.fix, /^brew install go/);
  assert.match(failed.Go.name, /1\.27\.3 or later/);
  assert.match(failed['Node.js'].fix, /^brew upgrade node/, 'an old Node is upgraded, not installed');
  assert.match(failed.Rust.fix, /sh\.rustup\.rs/);
  // An old Rust from rustup is one command.
  assert.equal(evaluate(need, { ...everything, rust: '1.80.0' }).find(c => c.name.startsWith('Rust')).fix, 'rustup update stable');
  assert.equal(evaluate(need, { ...everything, arm64: false, arch: 'x86_64' })[0].ok, false);
  const lines = [];
  assert.equal(report(missing, line => lines.push(line)), false);
  assert.match(lines.at(-1), /then run make install again/);
  assert.ok(lines.some(line => line.includes('✗ Go 1.27.3 or later: not found')));
});

test('PREFIX picks the folder; ~ is the home folder', () => {
  assert.equal(appPath(undefined, '/Users/me'), '/Applications/Lazybox.app');
  assert.equal(appPath('', '/Users/me'), '/Applications/Lazybox.app');
  assert.equal(appPath('~/Applications', '/Users/me'), '/Users/me/Applications/Lazybox.app');
  assert.equal(appPath('/tmp/x', '/Users/me'), '/tmp/x/Lazybox.app');
  assert.throws(() => appPath('Applications', '/Users/me'), /absolute/);
});

test('only the copy being replaced is closed, never another Lazybox', () => {
  const ps = [
    '  101 /Applications/Lazybox.app/Contents/MacOS/lazybox-desktop',
    '  102 /Users/me/Applications/Lazybox.app/Contents/MacOS/lazybox-desktop',
    '  103 /Applications/Lazybox.app/Contents/Resources/runtime/bin/lazybox-runner',
    '  104 /Applications/Lazybox.app.old/Contents/MacOS/lazybox-desktop',
  ].join('\n');
  assert.deepEqual(runningCopies('/Users/me/Applications/Lazybox.app', ps), [102]);
  assert.deepEqual(runningCopies('/Applications/Lazybox.app', ps), [101], 'the Runner is a service, not the app');
});

test('a build from source has no updater, so it can never offer an update', async () => {
  const base = JSON.parse(await readFile(resolve(root, 'packaging/release.tauri.json'), 'utf8'));
  const config = tauriReleaseConfig(base, { version: '0.1.0-pre.1', runtimeDir: '/tmp/runtime', identity: '-', source: true });
  assert.equal(config.plugins.updater, undefined);
  assert.equal(config.bundle.createUpdaterArtifacts, false);
  assert.equal(config.identifier, 'ai.lazybox.app', 'the same app as a downloaded release, so it keeps the same data');
  assert.equal(config.bundle.macOS.signingIdentity, '-');
  // A release still gets its feed and key.
  const release = tauriReleaseConfig(base, { version: '0.1.0', runtimeDir: '/tmp/runtime', identity: '-', pubkey: 'key' });
  assert.equal(release.plugins.updater.pubkey, 'key');
});

test('a build from source carries the newest version the changelog names, and its commit', () => {
  assert.equal(sourceVersion([{ id: '2026-11-18.1' }, { id: '0.1.0-pre.1' }, { id: '0.0.9' }]), '0.1.0-pre.1');
  assert.throws(() => sourceVersion([{ id: '2026-11-18.1' }]), /names no version/);
  assert.deepEqual(sourceManifest({ commit: 'abc', dirty: true, checkout: '/src', date: new Date('2026-11-17T10:00:00.123Z') }), { commit: 'abc', dirty: true, checkout: '/src', builtAt: '2026-11-17T10:00:00Z' });
});

test('Linux needs the Runner build tools and uses a per-user runtime directory', () => {
  const found = { ...everything, glibc: '2.39', tools: { git: true, make: true, tar: true }, clt: false, rust: '', macos: '' };
  assert.ok(evaluate(need, found, 'linux').every(check => check.ok));
  assert.deepEqual(evaluate(need, { ...found, glibc: '' }, 'linux').filter(check => !check.ok).map(check => check.name), ['glibc 2.28 or later']);
  assert.equal(linuxRuntimePath(undefined, '/home/me'), '/home/me/.local/lib/lazybox');
  assert.equal(linuxRuntimePath('~/apps', '/home/me'), '/home/me/apps/lazybox');
  assert.equal(linuxRuntimePath('/opt/apps', '/home/me'), '/opt/apps/lazybox');
  assert.throws(() => linuxRuntimePath('apps', '/home/me'), /absolute/);
});
