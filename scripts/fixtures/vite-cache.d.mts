// Types for vite-cache.mjs, for TypeScript callers (scenarios).
/** Vite options that give one UI suite its own dependency cache, named after `scriptUrl`. */
export function testViteCache(scriptUrl: string): { cacheDir: string };
