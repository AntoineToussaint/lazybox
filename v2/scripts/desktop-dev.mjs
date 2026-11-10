import { createServer } from 'vite';
import { spawn } from 'node:child_process';
import { createServer as createHTTPServer } from 'node:http';
import { realpath } from 'node:fs/promises';
import { uiWatcher } from './ui-watch.mjs';

// Every owned supervisor gets its own group: a child may signal its entire
// group during shutdown without terminating the launcher before cleanup.
export function spawnProcessGroup(command, args, options) {
  return spawn(command, args, { ...options, detached: true });
}
import { join } from 'node:path';

// Watchers can handle SIGTERM without exiting. Only the group created by this
// launcher is signalled, with a bounded grace period before forced cleanup.
export async function stopProcessGroup(child, graceMs = 1500, kill = (pid, signal) => process.kill(pid, signal)) {
  if (!child?.pid) return;
  const signal = name => { try { kill(-child.pid, name); return true; } catch (error) { if (error.code === 'ESRCH') return false; if (name === 0 && error.code === 'EPERM') return true; throw error; } };
  if (!signal('SIGTERM')) return;
  const deadline = Date.now() + graceMs;
  while (Date.now() < deadline) {
    if (!signal(0)) return;
    await new Promise(resolve => setTimeout(resolve, 25));
  }
  signal('SIGKILL');
}

// Vite owns UI watching and React refresh; Tauri owns native-shell rebuilding.
// Bind first so both HTTP and HMR use the actual OS-assigned port.
export async function startDesktopDevServer(source) {
  const watcher = uiWatcher();
  source = await realpath(source);
  // Scope private paths below this source. A release's *ancestor* may itself
  // be .cache; rejecting that ancestor would reject every UI asset in a stage.
  const sourceGlob = source.replace(/[?*()[\]{}!+@]/g, '\\$&');
  const privatePaths = ['**/.cache/**', '**/.git/**', '**/.env*', '**/runner.config.json', '**/src-tauri/**', '**/*.{pem,crt}'].map(pattern => `${sourceGlob}/${pattern}`);
  // Standalone Vite installs a SIGTERM handler that exits the entire process.
  // Embedding uses middleware mode: this launcher owns HTTP, HMR and shutdown.
  let vite;
  const http = createHTTPServer((request, response) => {
    if (vite) vite.middlewares(request, response);
    else { response.statusCode = 503; response.end('Starting'); }
  });
  const close = async () => {
    await vite?.close();
    http.closeAllConnections();
    await new Promise((resolve, reject) => http.close(error => error && error.code !== 'ERR_SERVER_NOT_RUNNING' ? reject(error) : resolve()));
  };
  try {
    await new Promise((resolve, reject) => { http.once('error', reject); http.listen(0, '127.0.0.1', resolve); });
    const address = http.address();
    if (!address || typeof address === 'string') throw new Error('Desktop development server has no TCP address');
    vite = await createServer({
      root: join(source, 'apps/desktop'),
      configFile: join(source, 'apps/desktop/vite.config.ts'),
      server: {
        // Tauri owns native source/build watching. Do not poll its build tree.
        watch: { ...watcher.options, ignored: [`${sourceGlob}/apps/desktop/src-tauri/**`, `${sourceGlob}/**/.cache/**`] },
        middlewareMode: { server: http }, hmr: { server: http },
        host: '127.0.0.1', port: address.port,
        fs: { strict: true, allow: ['apps/desktop', 'packages', 'node_modules'].map(path => join(source, path)), deny: privatePaths },
      },
    });
    return { server: { httpServer: http, watcher: vite.watcher, close }, url: `http://127.0.0.1:${address.port}`, watchBackend: watcher.backend };
  } catch (error) { await close(); throw error; }
}

export function desktopDevConfig(paths, url) {
  const parsed = new URL(url);
  if (parsed.protocol !== 'http:' || parsed.hostname !== '127.0.0.1' || !parsed.port || parsed.username || parsed.password || parsed.pathname !== '/' || parsed.search || parsed.hash) throw new Error('Expected the resolved loopback development origin');
  return {
    identifier: paths.identifier, productName: paths.productName,
    build: { devUrl: parsed.origin },
    app: { security: { devCsp: `default-src 'self'; connect-src ipc: http://ipc.localhost ${parsed.origin} ws://${parsed.host}; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self' data:; worker-src 'self' blob:` } },
  };
}
