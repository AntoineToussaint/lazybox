// UI gates serve the app from a Vite dev server that neither watches the
// checkout nor hot reloads (scripts/fixtures/vite-cache.mjs). With Vite's
// watcher on, an edit in the checkout while a gate ran reloaded the gate's
// page mid-step, and the gate failed as if the app had raced.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, readFile, readdir, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createServer, gateServerConfig } from './fixtures/vite-cache.mjs';

const scripts = fileURLToPath(new URL('.', import.meta.url));

/** What Vite may be imported for in a test script: building and previewing, never its dev server. */
const allowedFromVite = new Set(['build', 'preview']);

/**
 * The imports of Vite in `text` that could reach its own dev server: a named
 * import other than `build`/`preview` (renamed or not), a default or namespace
 * import, a bare side-effect import, a re-export, or a dynamic `import('vite')`.
 */
export function viteServerImports(text: string): string[] {
  const vite = String.raw`(?<q>['"])vite\k<q>`;
  const found: string[] = [];
  for (const match of text.matchAll(new RegExp(String.raw`\b(?:import|export)\s*([^'";]*?)\s*from\s*${vite}`, 'g'))) {
    const clause = match[1].trim();
    const named = /^{([^}]*)}$/.exec(clause);
    const names = named ? named[1].split(',').map(name => name.trim().split(/\s+as\s+/)[0].trim()).filter(Boolean) : [];
    if (!named || names.some(name => !allowedFromVite.has(name))) found.push(match[0]);
  }
  for (const match of text.matchAll(new RegExp(String.raw`\bimport\s+${vite}`, 'g'))) found.push(match[0]);
  for (const match of text.matchAll(new RegExp(String.raw`\bimport\s*\(\s*${vite}\s*\)`, 'g'))) found.push(match[0]);
  return found;
}

test('the detector finds every way to reach Vite\'s own server', () => {
  for (const source of [
    `import { createServer } from 'vite';`,
    `import {build,createServer,preview} from "vite";`,
    `import {createServer as cs} from 'vite';`,
    `import * as vite from 'vite'; await vite.createServer({});`,
    `import vite from 'vite';`,
    `import vite, { build } from 'vite';`,
    `const { createServer } = await import('vite');`,
    `const vite = await import( "vite" );`,
    `import 'vite';`,
    `export { createServer } from 'vite';`,
    `export * from 'vite';`,
  ]) assert.notDeepEqual(viteServerImports(source), [], source);
  for (const source of [
    `import {build,preview} from 'vite';import {createServer} from './fixtures/vite-cache.mjs';`,
    `import { build as bundle } from 'vite';`,
    `import { createServer } from 'node:http';`,
    `import react from '@vitejs/plugin-react';`,
    `import { createServer } from './fixtures/vite-cache.mjs';`,
  ]) assert.deepEqual(viteServerImports(source), [], source);
});

test('every UI test serves through the gate server', async () => {
  const offenders: string[] = [];
  for (const name of (await readdir(scripts)).filter(name => /^test-.*\.mjs$/.test(name))) {
    for (const use of viteServerImports(await readFile(join(scripts, name), 'utf8'))) offenders.push(`${name}: ${use}`);
  }
  assert.deepEqual(offenders, [], 'take createServer from ./fixtures/vite-cache.mjs (only build and preview come from vite)');
});

test('the gate server keeps the test\'s server options and turns watching and hot reload off', () => {
  const config = gateServerConfig({ root: '/app', server: { host: '127.0.0.1', port: 0, proxy: { '/rpc': 'http://127.0.0.1:1' }, hmr: true } });
  assert.equal(config.root, '/app');
  assert.deepEqual(config.server, { host: '127.0.0.1', port: 0, proxy: { '/rpc': 'http://127.0.0.1:1' }, watch: null, hmr: false });
});

test('a gate server neither watches nor hot reloads', async () => {
  const root = await mkdtemp(join(tmpdir(), 'lazybox-gate-server-'));
  await writeFile(join(root, 'index.html'), '<!doctype html><script type="module" src="./main.js"></script>');
  await writeFile(join(root, 'main.js'), 'export {};');
  const server = await createServer({ root, configFile: false, logLevel: 'silent', cacheDir: join(root, '.vite'), server: { host: '127.0.0.1', port: 0 } });
  try {
    assert.equal(server.config.server.hmr, false);
    assert.equal(server.config.server.watch, null);
    assert.deepEqual(Object.keys(server.watcher.getWatched()), [], 'nothing in the checkout is watched');
  } finally {
    await server.close();
    await rm(root, { recursive: true, force: true });
  }
});

test('listening warms the app: every module index.html loads is transformed before a page asks', async () => {
  const root = await mkdtemp(join(tmpdir(), 'lazybox-warm-'));
  try {
    await writeFile(join(root, 'index.html'), '<!doctype html><script type="module" src="./main.js"></script>');
    await writeFile(join(root, 'main.js'), "import { two } from './two.js';\nconsole.log(two);\n");
    await writeFile(join(root, 'two.js'), "import { three } from './three.js';\nexport const two = three + 1;\n");
    await writeFile(join(root, 'three.js'), 'export const three = 3;\n');
    const server = await createServer({ root, configFile: false, logLevel: 'silent', cacheDir: join(root, '.vite'), server: { host: '127.0.0.1', port: 0 } });
    try {
      await server.listen();
      const graph = server.environments.client.moduleGraph;
      for (const url of ['/main.js', '/two.js', '/three.js']) {
        assert.ok((await graph.getModuleByUrl(url))?.transformResult, `${url} transformed before any page asked for it`);
      }
    } finally { await server.close(); }
  } finally { await rm(root, { recursive: true, force: true }); }
});
