// How the site renders docs as written, with no copy and no rewrite of the
// Markdown: the app's docs viewer reads the same files, so they stay the one
// source. Three differences between a page on GitHub (or in the app) and on
// the site are handled here, at render time:
//
//   - Heading anchors follow GitHub's rules, so the `#fragment` links the docs
//     already use (and scripts/docs-links.test.ts checks) land on the site too.
//   - A link that leaves the published pages (an internal design note, a JSON
//     Schema, a source file) points at that file in the repository on GitHub.
//   - A screenshot shows its light twin (`hero.webp` → `hero-light.webp`,
//     written beside it by docs:screenshots) in the light theme, as the app's
//     docs viewer does (themedDocImage in packages/surfaces/src/docs-model.ts).
import { existsSync, statSync } from 'node:fs';
import { dirname, relative, resolve } from 'node:path';
import type MarkdownIt from 'markdown-it';

/** GitHub's heading anchor: lower case, punctuation dropped, spaces to dashes (anchors() in scripts/docs-links.test.ts). */
export function githubSlug(text: string): string {
  return text.trim().toLowerCase().replace(/[^\p{L}\p{M}\p{N}\p{Pc}\- ]/gu, '').replace(/ /g, '-');
}

export type SiteSources = {
  /** The v2 folder: the site's srcDir, and the root of the standalone repository. */
  root: string;
  /** Whether a file (relative to root) is a page of the site. */
  isPage: (path: string) => boolean;
  /** Where repository files are browsed: https://github.com/<owner>/<repo>. */
  repository: string;
  /** The branch or tag source links name. */
  ref: string;
  /** Where links that leave v2/ (the monorepo's V1 files) go. */
  outsideRepository: string;
};

const SCHEME = /^[a-z][a-z0-9+.-]*:/i;

type Env = { path?: string; realPath?: string };
const sourceOf = (env: Env) => env.realPath ?? env.path ?? '';

/** The repository URL for a relative link from a page, or undefined when the link stays on the site. */
export function sourceLink(sources: SiteSources, page: string, href: string): string | undefined {
  if (!href || href.startsWith('#') || href.startsWith('/') || SCHEME.test(href)) return undefined;
  const [pathPart, ...rest] = href.split('#');
  if (!pathPart) return undefined;
  const fragment = rest.length ? '#' + rest.join('#') : '';
  const target = resolve(dirname(page), decodeURIComponent(pathPart));
  const inRoot = relative(sources.root, target).split('\\').join('/');
  if (inRoot.startsWith('../')) {
    // Only in the monorepo: the standalone export already rewrote these to absolute URLs.
    const inMonorepo = relative(resolve(sources.root, '..'), target).split('\\').join('/');
    return `${sources.outsideRepository}/blob/main/${encodeURI(inMonorepo)}${fragment}`;
  }
  if (sources.isPage(inRoot)) return undefined;
  const kind = existsSync(target) && statSync(target).isDirectory() ? 'tree' : 'blob';
  return `${sources.repository}/${kind}/${sources.ref}/${encodeURI(inRoot)}${fragment}`;
}

/** The light screenshot beside a dark one, as the docs reference it, or undefined when there is none. */
export function lightTwin(page: string, src: string): string | undefined {
  if (!src || SCHEME.test(src) || src.startsWith('/') || /-light\.[a-z0-9]+$/i.test(src)) return undefined;
  const light = src.replace(/(\.[a-z0-9]+)$/i, '-light$1');
  if (light === src) return undefined;
  return existsSync(resolve(dirname(page), decodeURIComponent(light.replace(/^\.\//, '')))) ? light : undefined;
}

/** HTML elements a page may write as HTML. Anything else in angle brackets is prose ("<date and time>"). */
const HTML_TAGS = new Set(('a abbr address article aside b bdi bdo blockquote br caption cite code col colgroup dd del details dfn div dl dt em '
  + 'figcaption figure footer h1 h2 h3 h4 h5 h6 header hr i img ins kbd li main mark nav ol p picture pre q s samp section small source span '
  + 'strong sub summary sup table tbody td tfoot th thead time tr u ul var video wbr').split(' '));
const tagOf = (html: string) => /^<\/?([A-Za-z][\w-]*)/.exec(html)?.[1]?.toLowerCase();
const escapeHtml = (text: string) => text.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');

export function lazyboxMarkdown(md: MarkdownIt, sources: SiteSources) {
  // The page is compiled as a Vue template, where a placeholder written in angle brackets would be an
  // unclosed element. On GitHub and in the app it reads as text; here too.
  md.core.ruler.after('inline', 'lazybox-placeholders', state => {
    for (const token of state.tokens) {
      if (token.type === 'html_block') {
        const tag = tagOf(token.content);
        if (tag && !HTML_TAGS.has(tag)) token.content = `<p>${escapeHtml(token.content.trim())}</p>\n`;
      }
      for (const child of token.children ?? []) {
        if (child.type !== 'html_inline') continue;
        const tag = tagOf(child.content);
        if (tag && !HTML_TAGS.has(tag)) child.type = 'text';
      }
    }
  });

  // Before VitePress renders links: one that leaves the site becomes an external link to the repository.
  md.core.ruler.after('inline', 'lazybox-source-links', state => {
    const page = sourceOf(state.env as Env);
    for (const token of state.tokens) {
      for (const child of token.children ?? []) {
        if (child.type !== 'link_open') continue;
        const url = sourceLink(sources, page, child.attrGet('href') ?? '');
        if (url) child.attrSet('href', url);
      }
    }
  });

  // A screenshot with a light twin renders twice; the theme's CSS shows the one for the current theme.
  const image = md.renderer.rules.image!;
  md.renderer.rules.image = (tokens, index, options, env, self) => {
    const token = tokens[index];
    const src = token.attrGet('src') ?? '';
    const light = lightTwin(sourceOf(env as Env), src);
    if (!light) return image(tokens, index, options, env, self);
    const original = token.attrs ? token.attrs.map(attr => [...attr] as [string, string]) : null;
    token.attrJoin('class', 'lb-theme-dark');
    const dark = image(tokens, index, options, env, self);
    token.attrs = original ? original.map(attr => [...attr] as [string, string]) : null;
    token.attrSet('src', light);
    token.attrJoin('class', 'lb-theme-light');
    const lit = image(tokens, index, options, env, self);
    token.attrs = original;
    return dark + lit;
  };
}
