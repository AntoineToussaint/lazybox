// Build/run a separate native application identity against explicitly fake RPC.
// The normal Lazybox app, its saved data, accounts, and Runner stay untouched.
import { execFileSync, spawn } from 'node:child_process';
import { once } from 'node:events';
import { mkdir, writeFile } from 'node:fs/promises';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';

if (process.platform !== 'darwin') throw new Error('This isolated native scenario currently targets macOS');
const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const app = resolve(root, 'apps/desktop');
const env = { ...process.env, CARGO_BUILD_JOBS: process.env.CARGO_BUILD_JOBS || '1' };
const config = JSON.stringify({ productName: 'Lazybox Workspace Check', identifier: 'ai.lazybox.v2.workspacecheck' });
execFileSync('npm', ['run', 'build:ui', '-w', '@lazybox/desktop'], { cwd: root, env, stdio: 'inherit' });
execFileSync(resolve(root, 'node_modules/.bin/tauri'), ['build', '--debug', '--bundles', 'app', '--config', config], { cwd: app, env, stdio: 'inherit' });
const metadata = JSON.parse(execFileSync('cargo', ['metadata', '--no-deps', '--format-version', '1', '--manifest-path', resolve(app, 'src-tauri/Cargo.toml')], { encoding: 'utf8', env }));
const executable = resolve(metadata.target_directory, 'debug/bundle/macos/Lazybox Workspace Check.app/Contents/MacOS/lazybox-v2-desktop');
const fixture = await startRunnerFixture();
await mkdir(resolve(root, '.cache'), { recursive: true });
const evidence = resolve(root, '.cache/workspace-native-evidence.json');
const report = () => writeFile(evidence, JSON.stringify({ fixture: 'fake RPC; no Git, commands, accounts, or model inference', calls: fixture.calls.reduce((counts, call) => ({ ...counts, [call.method]: (counts[call.method] || 0) + 1 }), {}), mutations: fixture.mutations.map(value => value.method), sessions: fixture.sessions.map(value => value.id) }, null, 2));
const timer = setInterval(() => void report(), 1000);
const child = spawn(executable, ['--endpoint', fixture.endpoint, '--session', 'alpha-session'], { env, stdio: 'inherit' });
console.log(`Isolated native scenario: Lazybox Workspace Check. Evidence: ${evidence}`);
for (const signal of ['SIGINT', 'SIGTERM']) process.once(signal, () => child.kill('SIGTERM'));
try { await once(child, 'exit'); }
finally { clearInterval(timer); await report(); await fixture.close(); }
