// `@lazybox/surfaces/models` is what the iPhone app (React Native) shares with
// the desktop and the phone web app. Every module it reaches must stay free of
// React, the DOM and CSS, or the native bundle breaks (or quietly grows a
// browser dependency). This walks the entry's imports and names the first
// module that is not.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { existsSync, readFileSync, statSync } from 'node:fs';
import { dirname, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('..', import.meta.url));
const entry = resolve(root, 'packages/surfaces/src/models.ts');
/** Packages the shared models may import: the wire types, the Runner client, protobuf and Connect. */
const allowed = new Set(['@lazybox/protocol', '@lazybox/client-core', '@bufbuild/protobuf', '@connectrpc/connect']);
const browserGlobals = /(?<![\w.$])(document|window|localStorage|sessionStorage|navigator|indexedDB|HTMLElement|requestAnimationFrame)\s*[.(\[]/;

function closure(start: string) {
  const files = new Map<string, string[]>(), external = new Map<string, string>();
  const visit = (file: string) => {
    if (files.has(file)) return;
    const source = readFileSync(file, 'utf8');
    const local: string[] = [];
    files.set(file, local);
    // Type-only imports vanish at build time; they may name anything.
    for (const match of source.matchAll(/^\s*(?:import|export)\s+(?!type\b)[^'"]*?from\s+['"]([^'"]+)['"]|^\s*import\s+['"]([^'"]+)['"]/gm)) {
      const spec = match[1] || match[2];
      if (!spec.startsWith('.')) { if (!external.has(spec)) external.set(spec, file); continue; }
      const base = resolve(dirname(file), spec);
      const found = [base, `${base}.ts`, `${base}.tsx`, `${base}/index.ts`].find(path => existsSync(path) && statSync(path).isFile());
      assert.ok(found, `${relative(root, file)} imports ${spec}, which does not resolve`);
      local.push(found); visit(found);
    }
  };
  visit(start);
  return { files, external };
}

test('the shared models import no React, DOM or CSS, and use no browser globals', () => {
  const { files, external } = closure(entry);
  assert.ok(files.size > 10, 'the walk found the models');
  for (const [spec, from] of external) {
    const pkg = spec.startsWith('@') ? spec.split('/').slice(0, 2).join('/') : spec.split('/')[0];
    assert.ok(allowed.has(pkg), `${relative(root, from)} imports ${spec}: the shared models may import only ${[...allowed].join(', ')}`);
  }
  for (const file of files.keys()) {
    assert.ok(file.endsWith('.ts'), `${relative(root, file)} is a component file (.tsx) in the shared models`);
    // Comments and string literals say "window" in prose ("the context window."); only code counts.
    const code = readFileSync(file, 'utf8').replace(/\/\*[\s\S]*?\*\/|(?<![:'"`])\/\/.*$/gm, '').replace(/'(?:\\.|[^'\\\n])*'|"(?:\\.|[^"\\\n])*"|`(?:\\.|[^`\\])*`/g, "''");
    const hit = browserGlobals.exec(code);
    assert.equal(hit, null, `${relative(root, file)} uses the browser global ${hit?.[1]}`);
  }
});

test('the package exposes the models as their own entry', () => {
  const pkg = JSON.parse(readFileSync(resolve(root, 'packages/surfaces/package.json'), 'utf8'));
  assert.equal(pkg.exports['./models'], './src/models.ts');
});
