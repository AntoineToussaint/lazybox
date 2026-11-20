// Assemble one Pages artifact from the live V2 docs, product page and frozen V1 site.
import { execFileSync } from 'node:child_process';
import { cpSync, existsSync, mkdirSync, readdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('../', import.meta.url));
process.chdir(root);
const base = process.env.DOCS_BASE || '/';
if (!/^\/(?:[\w.-]+\/)*$/.test(base) || base.split('/').some(part => part === '..' || part === '.')) {
  throw new Error('DOCS_BASE must be a site path with leading and trailing slashes.');
}
const out = resolve('.cache/site');
const docs = resolve('.cache/docs-site');
const guideBase = base + 'guide/';
execFileSync('npm', ['run', 'docs:check'], { env: { ...process.env, DOCS_BASE: guideBase }, stdio: 'inherit' });
rmSync(out, { recursive: true, force: true });
mkdirSync(out, { recursive: true });
execFileSync('tar', ['-xzf', 'site/legacy.tar.gz', '-C', out]);

function files(dir) {
  return readdirSync(dir, { withFileTypes: true }).flatMap(entry => {
    const path = join(dir, entry.name);
    return entry.isDirectory() ? files(path) : [path];
  });
}

// The archived Starlight release built its internal links at the domain root.
// Its Pagefind UI also takes an explicit site base and bundle directory.
if (base !== '/') {
  for (const path of files(out)) {
    if (path.endsWith('.html')) {
      const html = readFileSync(path, 'utf8').replace(/\b(href|src|poster|srcset)="\/(?!\/)/g, '$1="' + base);
      writeFileSync(path, html);
    }
    if (path.includes('/_astro/Search.') && path.endsWith('.js')) {
      const js = readFileSync(path, 'utf8');
      const options = 'baseUrl:`/`,bundlePath:`/`';
      if (!js.includes(options)) throw new Error('Review the archived Pagefind base options.');
      writeFileSync(path, js.replace(options, 'baseUrl:`' + base + '`,bundlePath:`' + base + '`'));
    }
  }
}
cpSync(docs, join(out, 'guide'), { recursive: true });
cpSync('site/index.html', join(out, 'index.html'));
mkdirSync(join(out, 'product'));
for (const image of ['hero.webp', 'hero-light.webp', 'chat-decisions.webp', 'agent-review.webp']) {
  cpSync(join('docs/images', image), join(out, 'product', image));
}
cpSync('site/style.css', join(out, 'product/style.css'));
cpSync('docs-site/public/logo.svg', join(out, 'product/logo.svg'));

function redirect(path, route) {
  if (existsSync(path)) throw new Error('Route collision at ' + relative(out, path));
  mkdirSync(dirname(path), { recursive: true });
  const href = guideBase + route;
  writeFileSync(path, '<!doctype html><html lang="en"><meta charset="utf-8"><title>Lazybox guides</title>' +
    '<link rel="canonical" href="' + href + '"><script>location.replace(' + JSON.stringify(href) +
    '+location.search+location.hash)</script><meta http-equiv="refresh" content="0;url=' + href +
    '"><a href="' + href + '">Continue to Lazybox guides</a></html>\n');
}
redirect(join(out, 'docs/index.html'), '');
for (const path of files(docs)) {
  const name = relative(docs, path);
  if (name.endsWith('.html') && !['404.html', 'index.html'].includes(name)) {
    redirect(join(out, name), name.slice(0, -5));
  }
}
writeFileSync(join(out, '404.html'), '<!doctype html><html lang="en"><meta charset="utf-8"><title>Page not found · Lazybox</title><h1>Page not found</h1><a href="' + base + '">Lazybox home</a></html>');
// Preserve the sitemap address already indexed for the V1 site.
const siteUrl = new URL(process.env.SITE_URL || base, 'https://lazybox.ai');
if (!siteUrl.pathname.endsWith('/')) siteUrl.pathname += '/';
const canonicalPages = ['','guide/', ...files(docs).map(path => relative(docs, path))
  .filter(name => name.endsWith('.html') && !['404.html', 'index.html'].includes(name))
  .map(name => 'guide/' + name.slice(0, -5)),
  ...files(out).map(path => relative(out, path)).filter(name => /^(v1|docs)\/.*index\.html$/.test(name) && name !== 'docs/index.html')
    .map(name => name.slice(0, -10))];
writeFileSync(join(out, 'sitemap-index.xml'), '<?xml version="1.0" encoding="UTF-8"?><urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">' +
  canonicalPages.map(route => '<url><loc>' + new URL(route, siteUrl).href + '</loc></url>').join('\n') + '</urlset>');
writeFileSync(join(out, 'robots.txt'), 'User-agent: *\nAllow: /\nSitemap: ' + new URL('sitemap-index.xml', siteUrl).href + '\n');
writeFileSync(join(out, 'llms.txt'), '# Lazybox V2\n\n' + [
  ['Homepage', ''], ['V2 guides', 'guide/'], ['Install', 'guide/install'], ['V1 terminal archive', 'v1/']
].map(([label, route]) => '- [' + label + '](' + new URL(route, siteUrl).href + ')').join('\n') + '\n');
execFileSync(process.execPath, ['scripts/docs-site-check.mjs', out], { stdio: 'inherit' });
