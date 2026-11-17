// The docs' screenshots can always be made again: every image README.md and
// docs reference is on disk and is one that `npm run docs:screenshots`
// (scripts/docs-shots.mjs) writes; every file in docs/images is one of them;
// and every shot is used by a page, so none is taken for nothing.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { existsSync, readdirSync, readFileSync, statSync } from 'node:fs';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { allOutputs, shots, shotOutputs } from './docs-shots.mjs';

const v2 = fileURLToPath(new URL('..', import.meta.url));
const images = join(v2, 'docs', 'images');

function markdownFiles(folder: string): string[] {
  return readdirSync(folder).flatMap(name => {
    const path = join(folder, name);
    return statSync(path).isDirectory() ? markdownFiles(path) : name.endsWith('.md') ? [path] : [];
  });
}
/** The text with fenced code blocks blanked, so an image inside one is not an image. */
const prose = (text: string) => text.replace(/^(```|~~~)[^\n]*\n[\s\S]*?^\1[^\n]*$/gm, block => block.replace(/[^\n]/g, ' '));

/** Every image a page shows: Markdown images and <img>/<source> in HTML, resolved to a path under v2. */
function referenced(): { page: string; image: string }[] {
  const pages = [join(v2, 'README.md'), ...markdownFiles(join(v2, 'docs'))];
  return pages.flatMap(page => {
    const text = prose(readFileSync(page, 'utf8')), targets: string[] = [];
    for (const match of text.matchAll(/!\[[^\]]*\]\(\s*<?([^)\s>]+)>?(?:\s+"[^"]*")?\s*\)/g)) targets.push(match[1]);
    for (const match of text.matchAll(/<(?:img|source)\b[^>]*?\s(?:src|srcset)="([^"]+)"/g)) targets.push(match[1]);
    return targets.filter(target => !/^[a-z]+:/i.test(target)).map(target => ({ page: relative(v2, page), image: relative(v2, resolve(dirname(page), target.split('#')[0])) }));
  });
}

test('every image the docs reference exists', () => {
  const missing = referenced().filter(({ image }) => !existsSync(join(v2, image))).map(({ page, image }) => `${page}: ${image}`);
  assert.deepEqual(missing, [], 'run `npm run docs:screenshots` (or fix the path)');
});

test('every screenshot the docs reference is one docs:screenshots takes', () => {
  const outputs = new Set(allOutputs());
  const strays = referenced().filter(({ image }) => image.startsWith('docs/images/') && !outputs.has(image.slice('docs/images/'.length)))
    .map(({ page, image }) => `${page}: ${image}`);
  assert.deepEqual(strays, [], 'add a shot for it in scripts/docs-shots.mjs: a picture nobody can take again goes stale');
});

test('every file in docs/images is taken by docs:screenshots, and every one it takes is there', () => {
  const outputs = new Set(allOutputs()), files = new Set(readdirSync(images));
  assert.deepEqual([...files].filter(file => !outputs.has(file)), [], 'an image no shot writes cannot be made again');
  assert.deepEqual([...outputs].filter(file => !files.has(file)), [], 'run `npm run docs:screenshots` for the missing ones');
});

test('every shot is shown by a page', () => {
  const shown = new Set(referenced().map(({ image }) => image));
  const unused = Object.keys(shots).flatMap(name => shotOutputs(name).filter(file => !file.endsWith('-light.webp')))
    .filter(file => !shown.has(`docs/images/${file}`));
  assert.deepEqual(unused, [], 'reference it from the guide it illustrates, or drop the shot');
});

test('the images stay light enough to ship in the app', () => {
  const heavy = readdirSync(images).filter(file => statSync(join(images, file)).size > 200 * 1024);
  assert.deepEqual(heavy, [], 'a screenshot over 200 KB: lower its quality or size in docs-screenshots.mjs');
});
