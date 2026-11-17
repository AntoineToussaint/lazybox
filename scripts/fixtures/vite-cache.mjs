import { basename } from 'node:path';
import { fileURLToPath } from 'node:url';

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
