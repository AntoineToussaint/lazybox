/**
 * The in-app documentation (DocsViewer.tsx): the pure parts. Pages are the
 * Markdown files of docs, bundled with the app, so they read offline and
 * need no public site; paths are relative to that folder ("guides/chat.md").
 */
/** GitHub's heading anchor: lower case, punctuation dropped, spaces to dashes. */
export function headingSlug(text: string) {
  return text.trim().toLowerCase().replace(/[^\p{L}\p{N}\s_-]/gu, '').replace(/\s/g, '-');
}

export type DocPage ={ path: string; title: string; section: string; text?: string };
export type DocTarget = { path: string; anchor: string };

const SECTIONS: [RegExp, string][] = [
  [/^(README|getting-started|concepts|\.\.\/INSTALL)\.md$/, 'Start'],
  [/^guides\//, 'Guides'],
  [/^reference\//, 'Reference'],
];
/** Where a page sits in the viewer's list. */
export function sectionOf(path: string) {
  return SECTIONS.find(([pattern]) => pattern.test(path))?.[1] || 'Help';
}
const ORDER = ['Start', 'Guides', 'Reference', 'Help'];
const FIRST = ['README.md', 'getting-started.md', 'install.md', 'concepts.md'];

/** The first heading, or the file name. */
export function titleOf(path: string, text: string) {
  const heading = /^#\s+(.+?)\s*#*\s*$/m.exec(text)?.[1];
  if (heading) return heading.replace(/[*_`]/g, '');
  const name = path.split('/').pop()!.replace(/\.md$/, '').replace(/[-_]/g, ' ');
  return name.charAt(0).toUpperCase() + name.slice(1);
}

/** Pages in reading order: the start pages first, then each section by title. */
export function orderPages(pages: DocPage[]) {
  return [...pages].sort((a, b) => ORDER.indexOf(a.section) - ORDER.indexOf(b.section)
    || (FIRST.indexOf(a.path) + 1 || 99) - (FIRST.indexOf(b.path) + 1 || 99)
    || a.title.localeCompare(b.title));
}

/**
 * A link inside a page, resolved against it: another page (and an anchor in
 * it), an anchor in the same page, or undefined for a link that leaves the
 * documentation (a page outside docs/, a source file).
 */
export function resolveDocLink(from: string, href: string, known: (path: string) => boolean): DocTarget | undefined {
  const [rawPath, rawAnchor = ''] = href.split('#', 2);
  const anchor = decodeURIComponent(rawAnchor);
  if (!rawPath) return { path: from, anchor };
  if (/^[a-z][a-z0-9+.-]*:/i.test(rawPath) || rawPath.startsWith('/')) return undefined;
  const parts = from.split('/').slice(0, -1);
  for (const part of decodeURIComponent(rawPath).split('/')) {
    if (part === '' || part === '.') continue;
    // Above docs/ the path keeps its "..".
    if (part === '..' && parts.length && parts[parts.length - 1] !== '..') parts.pop(); else parts.push(part);
  }
  let path = parts.join('/');
  if (!path.endsWith('.md')) path = path ? `${path.replace(/\/$/, '')}/README.md` : 'README.md';
  return known(path) ? { path, anchor } : undefined;
}

/** An image path in a page, relative to docs/. */
export function resolveDocImage(from: string, src: string) {
  if (/^[a-z][a-z0-9+.-]*:/i.test(src) || src.startsWith('/')) return undefined;
  const parts = from.split('/').slice(0, -1);
  for (const part of src.split('/')) {
    if (part === '' || part === '.') continue;
    if (part === '..') { if (!parts.length) return undefined; parts.pop(); } else parts.push(part);
  }
  return parts.join('/');
}

/**
 * The picture to show for a docs image in this theme. The docs reference the
 * dark screenshot ("images/hero.webp"); in the light theme its light twin
 * ("images/hero-light.webp", written beside it by docs:screenshots) is shown
 * when there is one, so a light window does not read dark pictures.
 */
export function themedDocImage(path: string, scheme: 'dark' | 'light', exists: (path: string) => boolean): string {
  if (scheme !== 'light') return path;
  const light = path.replace(/(\.[a-z0-9]+)$/i, '-light$1');
  return light !== path && !/-light\.[a-z0-9]+$/i.test(path) && exists(light) ? light : path;
}

export type SearchHit ={ page: DocPage; snippet: string; score: number };
/**
 * Pages that contain every word of the query, title matches first, each with
 * the line it matched on. Case-insensitive; Markdown syntax is ignored.
 */
export function searchDocs(pages: DocPage[], query: string, limit = 20): SearchHit[] {
  const words = query.toLowerCase().split(/\s+/).filter(Boolean);
  if (!words.length) return [];
  const hits: SearchHit[] = [];
  for (const page of pages) {
    const title = page.title.toLowerCase(), text = (page.text || '').toLowerCase();
    if (!words.every(w => title.includes(w) || text.includes(w))) continue;
    const score = words.reduce((sum, w) => sum + (title.includes(w) ? 10 : 0) + Math.min(5, text.split(w).length - 1), 0);
    const line = (page.text || '').split('\n').find(l => words.some(w => l.toLowerCase().includes(w)) && !/^\s*(#|```|\|?\s*-{3})/.test(l)) || '';
    hits.push({ page, score, snippet: plain(line).slice(0, 160) });
  }
  return hits.sort((a, b) => b.score - a.score || a.page.title.localeCompare(b.page.title)).slice(0, limit);
}

/** A Markdown line as text: links to their words, emphasis and code marks dropped. */
export function plain(line: string) {
  return line.replace(/!\[[^\]]*\]\([^)]*\)/g, '').replace(/\[([^\]]+)\]\([^)]*\)/g, '$1').replace(/[*_`>#|]/g, '').replace(/\s+/g, ' ').trim();
}
