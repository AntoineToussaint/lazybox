import { readFileSync } from 'node:fs';
import { basename, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createServer as createViteServer } from 'vite';

/**
 * Vite options that give one UI test its own dependency cache.
 *
 * Vite keeps a single pre-bundled dependency cache per directory, stamped
 * with a hash of the config that built it. The UI tests use different
 * configs (plugins, roots), so with the default shared directory every test
 * found a cache built by another, re-optimized it on start, and deleted the
 * modules another test running at the same time was loading ("Importing a
 * module script failed", missing elements after a forced reload). A cache
 * per test is never rewritten under another one, and stays warm across that
 * test's own runs.
 */
export function testViteCache(scriptUrl) {
  const name = basename(fileURLToPath(scriptUrl), '.mjs');
  return { cacheDir: fileURLToPath(new URL(`../../node_modules/.vite/tests/${name}`, import.meta.url)) };
}

/**
 * The Vite dev server a UI test serves the app from: Vite's own, minus file
 * watching and hot reload. With Vite's watcher on, any edit in the checkout
 * while a gate runs (another agent's change, a branch switch, a formatter)
 * sent a full reload to the gate's page in the middle of a step: the page
 * came back without the state the step had built, and the gate failed with
 * what looked like a race in the app. Now nothing is pushed to the page: a
 * change mid-run is not picked up for modules already served, but a module
 * first requested after the edit is still compiled from the edited file, so
 * such a run tests a mix of the two. What fails that run is the release
 * runner's own check (`sourceChanged` in scripts/release-gates.mjs, which
 * compares the tree before and after), not a reload in the middle of a gate.
 * Every `scripts/test-*.mjs` takes `createServer` from here, and imports only
 * `build` and `preview` from Vite (scripts/vite-server.test.ts checks it).
 */
export async function createServer(config = {}) {
  const server = await createViteServer(gateServerConfig(config));
  // Listening also warms the app: a gate's first page then loads from transformed modules (warmApp).
  const listen = server.listen.bind(server);
  server.listen = async (...args) => {
    const listening = await listen(...args);
    await warmApp(server);
    return listening;
  };
  return server;
}

/**
 * Transforms the app before a gate's first page asks for it: every module the
 * root's index.html loads, and everything those import, and Vite's
 * pre-bundled dependencies with them. On a loaded machine a cold first load
 * took 6–13 s, past the 10 s a gate gives a page's first step, and failed it
 * for nothing the page did; warmed, it is under a second. The work moves to
 * the server's start, which no page timeout covers. Best effort: a module
 * that cannot be transformed on its own (an asset imported raw) is left to
 * the browser to request as the app does.
 */
export async function warmApp(server) {
  const root = server.config.root;
  let html;
  try { html = readFileSync(join(root, 'index.html'), 'utf8'); } catch { return 0; }
  const env = server.environments.client, seen = new Set();
  let next = [...html.matchAll(/<script\b[^>]*\btype="module"[^>]*\bsrc="([^"]+)"/g)].map(m => m[1]);
  while (next.length) {
    const batch = [...new Set(next)].filter(url => !seen.has(url));
    next = [];
    for (const url of batch) seen.add(url);
    await Promise.all(batch.map(async url => {
      try { await env.transformRequest(url); } catch { return; }
      const mod = await env.moduleGraph.getModuleByUrl(url);
      for (const imported of mod?.importedModules ?? []) if (imported.url && !seen.has(imported.url)) next.push(imported.url);
    }));
  }
  return seen.size;
}

/** `config` with watching and hot reload off, whatever the test set for its server. */
export function gateServerConfig(config = {}) {
  return { ...config, server: { ...config.server, watch: null, hmr: false } };
}
