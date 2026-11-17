import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join, relative } from 'node:path';
import { headingSlug, orderPages, plain, resolveDocImage, resolveDocLink, searchDocs, sectionOf, themedDocImage, titleOf, type DocPage } from '../packages/surfaces/src/docs-model';

const known = (paths: string[]) => (path: string) => paths.includes(path);

test('links between pages and anchors stay in the viewer', () => {
  const pages = known(['README.md', 'getting-started.md', 'guides/chat.md', 'reference/cli.md']);
  assert.deepEqual(resolveDocLink('README.md', 'guides/chat.md', pages), { path: 'guides/chat.md', anchor: '' });
  assert.deepEqual(resolveDocLink('guides/chat.md', '../reference/cli.md#lazybox-run', pages), { path: 'reference/cli.md', anchor: 'lazybox-run' });
  assert.deepEqual(resolveDocLink('guides/chat.md', '#answer-a-question', pages), { path: 'guides/chat.md', anchor: 'answer-a-question' });
  assert.deepEqual(resolveDocLink('guides/chat.md', '../', pages), { path: 'README.md', anchor: '' });
  // Leaving the docs (a source file, the web) is not a page, unless the app bundles it.
  assert.equal(resolveDocLink('README.md', '../INSTALL.md', pages), undefined);
  assert.deepEqual(resolveDocLink('getting-started.md', '../INSTALL.md#unsigned-builds', known(['../INSTALL.md'])), { path: '../INSTALL.md', anchor: 'unsigned-builds' });
  assert.equal(resolveDocLink('../INSTALL.md', 'SESSION_HOSTS.md', known(['../INSTALL.md'])), undefined);
  assert.equal(resolveDocLink('README.md', 'https://example.com/x.md', pages), undefined);
  assert.equal(resolveDocLink('README.md', 'missing.md', pages), undefined);
  assert.equal(resolveDocImage('guides/chat.md', '../images/hero.webp'), 'images/hero.webp');
  assert.equal(resolveDocImage('README.md', 'https://example.com/a.png'), undefined);
});

test('pages read in order, titled by their first heading', () => {
  const page = (path: string, title: string): DocPage => ({ path, title, section: sectionOf(path) });
  const ordered = orderPages([page('faq.md', 'FAQ'), page('guides/b.md', 'B'), page('concepts.md', 'Concepts'), page('README.md', 'Docs'), page('reference/cli.md', 'CLI'), page('guides/a.md', 'A'), page('getting-started.md', 'Start')]);
  assert.deepEqual(ordered.map(p => p.path), ['README.md', 'getting-started.md', 'concepts.md', 'guides/a.md', 'guides/b.md', 'reference/cli.md', 'faq.md']);
  assert.equal(titleOf('guides/chat.md', '<!-- x -->\n# Work in the *chat*\n\nText'), 'Work in the chat');
  assert.equal(titleOf('guides/run-your-app.md', 'no heading'), 'Run your app');
});

test('search finds every word, titles first, with the line it matched', () => {
  const pages: DocPage[] = [
    { path: 'a.md', title: 'Keyboard shortcuts', section: 'Reference', text: '# Keyboard shortcuts\nEvery key.' },
    { path: 'b.md', title: 'Chat', section: 'Guides', text: '# Chat\nUse the [keyboard](a.md) to send **shortcuts**.' },
    { path: 'c.md', title: 'Other', section: 'Help', text: 'nothing' },
  ];
  const hits = searchDocs(pages, 'keyboard shortcuts');
  assert.deepEqual(hits.map(h => h.page.path), ['a.md', 'b.md']);
  assert.equal(hits[1].snippet, 'Use the keyboard to send shortcuts.');
  assert.deepEqual(searchDocs(pages, '  '), []);
  assert.equal(plain('![x](y.png) see [the guide](g.md) `code`'), 'see the guide code');
});

test('heading anchors match the links the docs use', () => {
  assert.equal(headingSlug('Install (preview)'), 'install-preview');
  assert.equal(headingSlug('The keys that matter'), 'the-keys-that-matter');
  assert.equal(headingSlug('`lazybox run` & log'), 'lazybox-run--log');
});

// The viewer resolves every link between the bundled pages to a page, and an
// anchor to a heading its own slugs produce (docs-links.test.ts checks the files).
test('the bundled docs have no broken internal links', () => {
  const root = join(import.meta.dirname, '..', 'docs');
  const files: string[] = [];
  const walk = (dir: string) => { for (const name of readdirSync(dir)) { const full = join(dir, name); if (statSync(full).isDirectory()) { if (name !== 'internal') walk(full); } else if (name.endsWith('.md')) files.push(relative(root, full)); } };
  walk(root);
  const texts = new Map(files.map(f => [f, readFileSync(join(root, f), 'utf8')]));
  const anchors = new Map([...texts].map(([f, t]) => [f, new Set([...t.matchAll(/^#{1,4}\s+(.+?)\s*$/gm)].map(m => headingSlug(m[1].replace(/[*_`]|\[([^\]]*)\]\([^)]*\)/g, '$1'))))]));
  const broken: string[] = [];
  for (const [file, text] of texts) {
    for (const [, href] of text.matchAll(/\]\(([^)\s]+)\)/g)) {
      if (/^[a-z]+:/i.test(href) || /\.(png|webp|jpe?g|gif|svg)$/i.test(href)) continue;
      const target = resolveDocLink(file, href, path => texts.has(path));
      if (!target) continue; // leaves the docs: opened outside the viewer
      if (target.anchor && !anchors.get(target.path)!.has(target.anchor)) broken.push(`${file} → ${href}`);
    }
  }
  assert.deepEqual(broken, []);
});

test('a screenshot shows in the window’s theme: its light twin in the light theme, when there is one', () => {
  const has = known(['images/hero.webp', 'images/hero-light.webp', 'images/welcome.webp']);
  assert.equal(themedDocImage('images/hero.webp', 'light', has), 'images/hero-light.webp');
  assert.equal(themedDocImage('images/hero.webp', 'dark', has), 'images/hero.webp');
  assert.equal(themedDocImage('images/welcome.webp', 'light', has), 'images/welcome.webp', 'no twin: the picture as referenced');
  assert.equal(themedDocImage('images/hero-light.webp', 'light', has), 'images/hero-light.webp', 'a light picture referenced as such stays');
});
