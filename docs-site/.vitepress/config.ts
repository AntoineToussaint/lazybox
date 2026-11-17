// The Lazybox V2 documentation site: npm run docs:dev, docs:build, docs:preview,
// docs:check; deployed by the standalone repository's Docs workflow (RELEASING.md,
// "Publish the docs").
//
// It renders the docs in place, never a copy: srcDir is v2 itself; every file
// but docs/ (without docs/internal), CHANGELOG.md and this folder's home page
// is excluded; and rewrites give the pages short URLs (docs/guides/chat.md →
// /guides/chat, docs/README.md → /overview, CHANGELOG.md → /changelog).
import { readdirSync, statSync } from 'node:fs';
import { join, relative } from 'node:path';
import { fileURLToPath } from 'node:url';
import { defineConfig } from 'vitepress';
import { githubSlug, lazyboxMarkdown, type SiteSources } from './markdown.ts';
import { missingFromSidebar, route, sidebar } from './sidebar.ts';

const v2 = fileURLToPath(new URL('../../', import.meta.url));
const docs = join(v2, 'docs');
const site = 'docs-site';

const REPOSITORY = 'https://github.com/AntoineToussaint/lazybox';
const V1_REPOSITORY = 'https://github.com/AntoineToussaint/lazybox-v1';
/** GitHub Pages serves a project site under /<repository>/: the deploy workflow sets DOCS_BASE from configure-pages. */
const base = (process.env.DOCS_BASE || '/').replace(/\/?$/, '/');
/** The branch or tag that links to repository files name: the release tag when one is built. "Edit this page" always edits main. */
const ref = process.env.DOCS_SOURCE_REF || 'main';

const markdownFiles = (folder: string): string[] => readdirSync(folder).flatMap(name => {
  const path = join(folder, name);
  return statSync(path).isDirectory() ? markdownFiles(path) : name.endsWith('.md') ? [path] : [];
});

/** Published Markdown, relative to v2: the user docs (not docs/internal), the changelog and the home page. */
const isPage = (path: string) => (path.startsWith('docs/') && path.endsWith('.md') && !path.startsWith('docs/internal/'))
  || path === 'CHANGELOG.md' || path === `${site}/index.md`;

// Everything else under v2 stays out of the page scan.
const srcExclude = [
  ...readdirSync(v2).filter(name => !['docs', site, 'CHANGELOG.md'].includes(name))
    .map(name => statSync(join(v2, name)).isDirectory() ? `${name}/**` : name),
  'docs/internal/**',
  '**/node_modules/**',
];

const docPages = markdownFiles(docs).map(path => relative(docs, path).split('\\').join('/')).filter(path => !path.startsWith('internal/'));
const unlisted = missingFromSidebar(docPages);
if (unlisted.length) throw new Error(`Add these pages to docs-site/.vitepress/sidebar.ts: ${unlisted.join(', ')}`);

const sources: SiteSources = { root: v2, isPage, repository: REPOSITORY, ref, outsideRepository: V1_REPOSITORY };

export default defineConfig({
  title: 'Lazybox',
  titleTemplate: ':title · Lazybox docs',
  description: 'One window for all your coding agents, across all your workspaces, and the place you check their work before it lands.',
  lang: 'en-US',
  base,
  cleanUrls: true,
  srcDir: '..',
  srcExclude,
  rewrites(page) {
    if (page === `${site}/index.md`) return 'index.md';
    if (page === 'CHANGELOG.md') return 'changelog.md';
    if (page === 'docs/README.md') return 'overview.md';
    return page.startsWith('docs/') ? page.slice('docs/'.length) : page;
  },
  outDir: '../.cache/docs-site',
  cacheDir: '../.cache/docs-site-vite',
  // Dead links fail the build (npm run docs:check also checks every anchor in the output).
  ignoreDeadLinks: false,
  head: [
    ['link', { rel: 'icon', type: 'image/svg+xml', href: `${base}logo.svg` }],
    ['meta', { name: 'theme-color', content: '#121317' }],
  ],
  markdown: {
    anchor: { slugify: githubSlug },
    // The docs hold no Vue components; CommonMark's own HTML rules apply, as on GitHub, so a
    // line that starts with a placeholder (`<conversation>`) stays in its paragraph.
    component: false,
    image: { lazyLoad: true },
    theme: { light: 'github-light', dark: 'github-dark' },
    config: md => lazyboxMarkdown(md, sources),
  },
  vite: {
    publicDir: `${site}/public`,
    // The docs reference screenshots, never import code: no dependency scan of the whole v2 tree.
    optimizeDeps: { entries: [] },
    server: { fs: { allow: [v2] } },
  },
  themeConfig: {
    logo: { src: '/logo.svg', alt: '' },
    siteTitle: 'Lazybox',
    nav: [
      { text: 'Get started', link: route('getting-started.md'), activeMatch: '^/(overview|getting-started|install|concepts)' },
      { text: 'Guides', link: route('guides/chat.md'), activeMatch: '^/guides/' },
      { text: 'Reference', link: route('reference/keyboard-shortcuts.md'), activeMatch: '^/reference/' },
      { text: 'Changelog', link: '/changelog' },
    ],
    sidebar: sidebar(docs),
    outline: { level: [2, 3], label: 'On this page' },
    search: { provider: 'local' },
    socialLinks: [{ icon: 'github', link: REPOSITORY, ariaLabel: 'Lazybox on GitHub' }],
    editLink: { pattern: `${REPOSITORY}/edit/main/:path`, text: 'Edit this page on GitHub' },
    externalLinkIcon: true,
    footer: {
      message: 'Lazybox V2 is a preview. Looking for V1, the terminal PR inbox? <a href="https://lazybox.ai/docs/">lazybox.ai/docs</a>.',
    },
  },
});
