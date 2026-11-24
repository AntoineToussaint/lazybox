// Every relative link and anchor in the repository's Markdown resolves: the
// target is a file or folder of the repository (tracked, or new and not
// ignored, so a link into .cache/ or a local build fails), and a #fragment
// names a heading of that file as GitHub anchors it. External links (http,
// mailto) are not fetched.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { existsSync, readFileSync, readdirSync } from 'node:fs';
import { join, posix } from 'node:path';
import { fileURLToPath } from 'node:url';
import { includeSource } from './environments.mjs';

const root = fileURLToPath(new URL('..', import.meta.url));

/** Every file below `folder` a source snapshot copies (environments.mjs `includeSource`). */
function sourceFiles(folder = ''): string[] {
  return readdirSync(join(root, folder), { withFileTypes: true }).flatMap(entry => {
    const path = folder ? `${folder}/${entry.name}` : entry.name;
    if (!includeSource(path)) return [];
    return entry.isDirectory() ? sourceFiles(path) : entry.isFile() ? [path] : [];
  });
}

/** The repository's files: tracked, plus new ones git does not ignore. A staged
 *  source snapshot has no .git; there, the files it copied. */
const files = existsSync(join(root, '.git'))
  ? execFileSync('git', ['ls-files', '-z', '--cached', '--others', '--exclude-standard'], { cwd: root, encoding: 'utf8' })
    .split('\0').filter(path => path && existsSync(join(root, path)))
  : sourceFiles();
const known = new Set(files);
const folders = new Set(files.flatMap(path => {
  const parts = path.split('/');
  return parts.slice(0, -1).map((_, index) => parts.slice(0, index + 1).join('/'));
}));
const inRepository = (path: string) => path === '.' || known.has(path) || folders.has(path.replace(/\/$/, ''));

/** The text with fenced code blocks and inline code blanked, so links inside them are not links. */
function prose(text: string): string {
  return text.replace(/^(```|~~~)[^\n]*\n[\s\S]*?^\1[^\n]*$/gm, block => block.replace(/[^\n]/g, ' '))
    .replace(/`[^`\n]*`/g, span => ' '.repeat(span.length));
}

/** GitHub's heading anchor: lowercase, punctuation dropped, spaces to dashes, duplicates numbered. */
export function anchors(markdown: string): Set<string> {
  const seen = new Map<string, number>(), out = new Set<string>();
  const text = markdown.replace(/^(```|~~~)[^\n]*\n[\s\S]*?^\1[^\n]*$/gm, '');
  for (const match of text.matchAll(/^#{1,6}[ \t]+(.+?)[ \t#]*$/gm)) {
    const plain = match[1]
      .replace(/!\[([^\]]*)\]\([^)]*\)/g, '$1')
      .replace(/\[([^\]]*)\]\([^)]*\)/g, '$1')
      .replace(/<[^>]+>/g, '')
      .replace(/[*`]/g, '');
    const base = plain.trim().toLowerCase().replace(/[^\p{L}\p{M}\p{N}\p{Pc}\- ]/gu, '').replace(/ /g, '-');
    const count = seen.get(base) ?? 0;
    seen.set(base, count + 1);
    out.add(count ? `${base}-${count}` : base);
  }
  for (const match of markdown.matchAll(/<a\s+(?:name|id)="([^"]+)"/g)) out.add(match[1]);
  return out;
}

function links(markdown: string): { target: string; line: number }[] {
  const text = prose(markdown), out: { target: string; line: number }[] = [];
  const lineOf = (index: number) => text.slice(0, index).split('\n').length;
  for (const match of text.matchAll(/\]\(\s*<?([^)\s>]+)>?(?:\s+"[^"]*")?\s*\)/g)) out.push({ target: match[1], line: lineOf(match.index!) });
  for (const match of text.matchAll(/^\s*\[[^\]]+\]:\s*(\S+)/gm)) out.push({ target: match[1], line: lineOf(match.index!) });
  for (const match of text.matchAll(/<(?:img|a|source)\b[^>]*?\s(?:src|href|srcset)="([^"]+)"/g)) out.push({ target: match[1], line: lineOf(match.index!) });
  return out;
}

const pages = files.filter(path => path.endsWith('.md'));

test('the docs exist', () => {
  for (const page of ['README.md', 'docs/README.md', 'docs/getting-started.md', 'docs/reference/keyboard-shortcuts.md', 'docs/internal/README.md']) assert.ok(known.has(page), page);
});

test('every relative link and anchor in the repository\'s Markdown resolves', () => {
  const problems: string[] = [];
  const anchorCache = new Map<string, Set<string>>();
  const anchorsOf = (path: string) => { if (!anchorCache.has(path)) anchorCache.set(path, anchors(readFileSync(join(root, path), 'utf8'))); return anchorCache.get(path)!; };
  for (const page of pages) {
    const markdown = readFileSync(join(root, page), 'utf8');
    for (const { target, line } of links(markdown)) {
      // A scheme (https:, mailto:) is external; a leading / is a route of the docs site.
      if (/^[a-z][a-z0-9+.-]*:/i.test(target) || target.startsWith('/')) continue;
      // `#lazybox-…` is an in-app action the chat renders as a button
      // (RichContent.tsx), written in the assistant's prompts; not an anchor.
      if (target.startsWith('#lazybox-')) continue;
      const where = `${page}:${line} → ${target}`;
      const [pathPart, fragment] = target.split('#', 2) as [string, string | undefined];
      const file = pathPart ? posix.normalize(posix.join(posix.dirname(page), decodeURIComponent(pathPart))) : page;
      if (file.startsWith('../')) { problems.push(`${where}: leaves the repository`); continue; }
      if (!inRepository(file)) { problems.push(`${where}: no such file in the repository`); continue; }
      if (fragment === undefined || fragment === '') continue;
      if (!file.endsWith('.md')) { problems.push(`${where}: an anchor into a file that is not Markdown`); continue; }
      if (!anchorsOf(file).has(decodeURIComponent(fragment).toLowerCase())) problems.push(`${where}: no heading "#${fragment}" in ${file}`);
    }
  }
  assert.deepEqual(problems, []);
});

test('anchors follow GitHub: punctuation dropped, duplicates numbered', () => {
  const found = anchors('# Install (preview)\n## `lazybox` CLI\n## A/B: ⌘K & more\n## Same\n## Same\n```\n# not a heading\n```\n');
  assert.deepEqual([...found], ['install-preview', 'lazybox-cli', 'ab-k--more', 'same', 'same-1']);
});
