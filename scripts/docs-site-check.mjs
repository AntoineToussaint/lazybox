#!/usr/bin/env node
// Checks the built documentation site (npm run docs:check, after
// `vitepress build docs-site`). VitePress refuses a link to a missing page;
// this checks what it does not, in the HTML it wrote:
//
//   - every link inside the site reaches a page or a file, and its #fragment
//     names an element of that page (the docs use GitHub's heading anchors,
//     which docs-site/.vitepress/markdown.ts reproduces);
//   - every image and asset a page loads is there;
//   - every link the site makes to a file of this repository on GitHub (an
//     internal note, a schema) names a file that exists here.
//
//   node scripts/docs-site-check.mjs [outDir]      default: .cache/docs-site
import { existsSync, readdirSync, readFileSync, statSync } from 'node:fs';
import { join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const v2 = fileURLToPath(new URL('..', import.meta.url));
const out = resolve(v2, process.argv[2] || '.cache/docs-site');
const base = (process.env.DOCS_BASE || '/').replace(/\/?$/, '/');
// docs-site/.vitepress/config.ts: REPOSITORY.
const SOURCE = /^https:\/\/github\.com\/AntoineToussaint\/lazybox\/(?:blob|tree|edit)\/[^/]+\/([^#?]+)/;

if (!existsSync(join(out, 'index.html'))) {
  console.error(`No site at ${relative(v2, out)}: run npm run docs:build first.`);
  process.exit(1);
}

const htmlFiles = dir => readdirSync(dir).flatMap(name => {
  const path = join(dir, name);
  return statSync(path).isDirectory() ? htmlFiles(path) : name.endsWith('.html') ? [path] : [];
});

const decode = text => text.replace(/&amp;/g, '&').replace(/&quot;/g, '"').replace(/&#39;/g, "'").replace(/&lt;/g, '<').replace(/&gt;/g, '>');
const idCache = new Map();
function idsOf(file) {
  if (!idCache.has(file)) idCache.set(file, new Set([...readFileSync(file, 'utf8').matchAll(/\sid="([^"]*)"/g)].map(match => decode(match[1]))));
  return idCache.get(file);
}

/** The file a site path is served from, with clean URLs: /guides/chat → guides/chat.html. */
function fileFor(path) {
  const plain = decodeURIComponent(path).replace(/^\/+/, '');
  const candidates = plain === '' || plain.endsWith('/') ? [plain + 'index.html'] : [plain, plain + '.html', plain + '/index.html'];
  return candidates.map(candidate => join(out, candidate)).find(candidate => existsSync(candidate) && statSync(candidate).isFile());
}

const problems = [];
let checked = 0;
for (const page of htmlFiles(out)) {
  const name = relative(out, page);
  const html = readFileSync(page, 'utf8');
  for (const match of html.matchAll(/\s(href|src)="([^"]*)"/g)) {
    const [, attribute, raw] = match;
    const url = decode(raw);
    if (!url || url.startsWith('mailto:') || url.startsWith('data:') || url.startsWith('javascript:')) continue;
    const source = SOURCE.exec(url);
    if (source) {
      checked++;
      if (!existsSync(join(v2, decodeURIComponent(source[1])))) problems.push(`${name}: ${url} names a file this repository does not have`);
      continue;
    }
    if (/^[a-z][a-z0-9+.-]*:/i.test(url) || url.startsWith('//')) continue;
    checked++;
    const [pathAndQuery, fragment] = url.split('#', 2);
    const path = pathAndQuery.split('?')[0];
    let target = page;
    if (path) {
      // A relative link resolves against the page's folder (clean URLs keep it: /guides/chat is in /guides/).
      if (path.startsWith('/') && !path.startsWith(base)) { problems.push(`${name}: ${url} is outside the base ${base}`); continue; }
      target = fileFor(path.startsWith('/') ? path.slice(base.length - 1) : '/' + relative(out, resolve(page, '..', path)).split('\\').join('/'));
      if (!target) { problems.push(`${name}: ${attribute}="${url}" — no such page or file`); continue; }
    }
    if (fragment && target.endsWith('.html') && !idsOf(target).has(decodeURIComponent(fragment))) {
      problems.push(`${name}: ${url} — no element #${decodeURIComponent(fragment)} in ${relative(out, target)}`);
    }
  }
}

if (problems.length) {
  console.error(`docs site: ${problems.length} broken link(s):\n  ${problems.join('\n  ')}`);
  process.exit(1);
}
console.log(`docs site: ${checked} links and assets in ${htmlFiles(out).length} pages resolve.`);
