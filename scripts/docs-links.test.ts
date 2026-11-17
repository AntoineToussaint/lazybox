// Every relative link and anchor in README.md and docs resolves: the
// file exists, and a #fragment names a heading of that file as GitHub
// anchors it. External links (http, mailto) are not fetched.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { existsSync, readFileSync, readdirSync, statSync } from 'node:fs';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const v2 = fileURLToPath(new URL('..', import.meta.url));

function markdownFiles(folder: string): string[] {
  return readdirSync(folder).flatMap(name => {
    const path = join(folder, name);
    return statSync(path).isDirectory() ? markdownFiles(path) : name.endsWith('.md') ? [path] : [];
  });
}

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

const pages = [join(v2, 'README.md'), ...markdownFiles(join(v2, 'docs'))];

test('the docs exist', () => {
  for (const page of ['README.md', 'docs/README.md', 'docs/getting-started.md', 'docs/reference/keyboard-shortcuts.md', 'docs/internal/README.md']) assert.ok(existsSync(join(v2, page)), page);
});

test('every relative link and anchor in README.md and docs resolves', () => {
  const problems: string[] = [];
  const anchorCache = new Map<string, Set<string>>();
  const anchorsOf = (path: string) => { if (!anchorCache.has(path)) anchorCache.set(path, anchors(readFileSync(path, 'utf8'))); return anchorCache.get(path)!; };
  for (const page of pages) {
    const markdown = readFileSync(page, 'utf8');
    for (const { target, line } of links(markdown)) {
      if (/^[a-z][a-z0-9+.-]*:/i.test(target)) continue;
      const where = `${relative(v2, page)}:${line} → ${target}`;
      const [pathPart, fragment] = target.split('#', 2) as [string, string | undefined];
      const file = pathPart ? resolve(dirname(page), decodeURIComponent(pathPart)) : page;
      // A link that leaves v2/ points into the monorepo root. A frozen release source (and the new
      // repository) has no parent: scripts/export-repo.mjs rewrites such links to its root files or to
      // the pinned V1 repository, so there they count as resolved when the monorepo is absent.
      const outside = relative(v2, file).startsWith('..');
      if (outside && !existsSync(file) && !existsSync(join(v2, '..', 'v2'))) continue;
      if (!existsSync(file)) { problems.push(`${where}: no such file`); continue; }
      if (fragment === undefined || fragment === '') continue;
      if (!file.endsWith('.md')) { problems.push(`${where}: an anchor into a file that is not Markdown`); continue; }
      if (!anchorsOf(file).has(decodeURIComponent(fragment).toLowerCase())) problems.push(`${where}: no heading "#${fragment}" in ${relative(v2, file)}`);
    }
  }
  assert.deepEqual(problems, []);
});

test('anchors follow GitHub: punctuation dropped, duplicates numbered', () => {
  const found = anchors('# Install (preview)\n## `lazybox` CLI\n## A/B: ⌘K & more\n## Same\n## Same\n```\n# not a heading\n```\n');
  assert.deepEqual([...found], ['install-preview', 'lazybox-cli', 'ab-k--more', 'same', 'same-1']);
});
