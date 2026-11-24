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

/** Whether `text` takes `createServer` straight from Vite, around the gate server. */
export function importsViteCreateServer(text: string): boolean {
  return [...text.matchAll(/import\s*{([^}]*)}\s*from\s*['"]vite['"]/g)].some(match => match[1].split(',').some(name => name.trim() === 'createServer'));
}

test('the detector finds a createServer taken from Vite', () => {
  assert.equal(importsViteCreateServer(`import { createServer } from 'vite';`), true);
  assert.equal(importsViteCreateServer(`import {build,createServer,preview} from "vite";`), true);
  assert.equal(importsViteCreateServer(`import {build,preview} from 'vite';import {createServer} from './fixtures/vite-cache.mjs';`), false);
  assert.equal(importsViteCreateServer(`import { createServer } from 'node:http';`), false);
});

test('every UI test serves through the gate server', async () => {
  const offenders = [];
  for (const name of (await readdir(scripts)).filter(name => /^test-.*\.mjs$/.test(name))) {
    if (importsViteCreateServer(await readFile(join(scripts, name), 'utf8'))) offenders.push(name);
  }
  assert.deepEqual(offenders, [], 'take createServer from ./fixtures/vite-cache.mjs');
});

test('the gate server keeps the test\'s server options and turns watching and hot reload off', () => {
  const config = gateServerConfig({ root: '/app', server: { host: '127.0.0.1', port: 0, proxy: { '/rpc': 'http://127.0.0.1:1' }, hmr: true } });
  assert.equal(config.root, '/app');
  assert.deepEqual(config.server, { host: '127.0.0.1', port: 0, proxy: { '/rpc': 'http://127.0.0.1:1' }, watch: null, hmr: false });
});

test('a gate server neither watches nor hot reloads', async () => {
  const root = await mkdtemp(join(tmpdir(), 'lazybox-gate-server-'));
  await writeFile(join(root, 'index.html'), '<!doctype html><script type="module" src="/main.js"></script>');
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
