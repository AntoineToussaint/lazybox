import { basename } from 'node:path';
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
export function createServer(config = {}) {
  return createViteServer(gateServerConfig(config));
}

/** `config` with watching and hot reload off, whatever the test set for its server. */
export function gateServerConfig(config = {}) {
  return { ...config, server: { ...config.server, watch: null, hmr: false } };
}
