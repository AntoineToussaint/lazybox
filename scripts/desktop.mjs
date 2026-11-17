import { spawn, execFileSync } from 'node:child_process';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
if (!process.env.LAZYBOX_V2_ENDPOINT) throw new Error('Use make desktop with the Runner endpoint file');
const manifest = resolve(root, 'apps/desktop/src-tauri/Cargo.toml');
const metadata = JSON.parse(execFileSync('cargo', ['metadata', '--no-deps', '--format-version', '1', '--manifest-path', manifest], { encoding: 'utf8' }));
const executable = process.platform === 'darwin'
  ? resolve(metadata.target_directory, 'debug/bundle/macos/Lazybox V2.app/Contents/MacOS/lazybox-v2-desktop')
  : resolve(metadata.target_directory, 'debug', process.platform === 'win32' ? 'lazybox-v2-desktop.exe' : 'lazybox-v2-desktop');
const launchArgs = ['--endpoint', resolve(process.env.LAZYBOX_V2_ENDPOINT)];
if (process.env.LAZYBOX_V2_PROJECT) launchArgs.push('--project', resolve(process.env.LAZYBOX_V2_PROJECT));
if (process.env.LAZYBOX_V2_SESSION) launchArgs.push('--session', process.env.LAZYBOX_V2_SESSION);
const child = spawn(executable, launchArgs, { stdio: 'inherit', env: { ...process.env, LAZYBOX_V2_ENDPOINT: resolve(process.env.LAZYBOX_V2_ENDPOINT) } });
child.on('error', error => { console.error(error.message); process.exitCode = 1; });
child.on('exit', code => { process.exitCode = code ?? 1; });
for (const signal of ['SIGINT', 'SIGTERM']) process.on(signal, () => child.kill(signal));
