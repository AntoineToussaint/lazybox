// No UI gate counts Playwright's raw `pageerror` event. WebKit's protocol
// reports a fetch that a navigation cut, and the app caught, as a JavaScript
// error ("… due to access control checks"), so a gate asserting "no page
// errors" on that event fails at random whenever a reload or goto cuts a poll.
// Gates collect through scripts/fixtures/page-errors.mjs instead, which counts
// only the browser's own uncaught errors and unhandled rejections.
//
// A script with a real reason to hear the raw event (say, asserting what
// WebKit reports) marks the line, or the line above it, with
// `raw-pageerror: <why>`.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync, statSync } from 'node:fs';
import { join, relative } from 'node:path';
import { fileURLToPath } from 'node:url';

const v2 = fileURLToPath(new URL('..', import.meta.url));
const event = 'page' + 'error';
const fixture = 'scripts/fixtures/page-errors.mjs';
const self = 'scripts/page-errors.test.ts';

function sources(folder: string): string[] {
  return readdirSync(folder).flatMap(name => {
    if (name === 'node_modules' || name.startsWith('.')) return [];
    const path = join(folder, name);
    return statSync(path).isDirectory() ? sources(path) : /\.(mjs|cjs|js|ts|mts|tsx)$/.test(name) && !name.endsWith('.d.mts') ? [path] : [];
  });
}

/** The lines of `text` that subscribe to, or wait for, the raw event without a justified exception. */
export function rawPageErrorListeners(text: string): number[] {
  const lines = text.split('\n');
  const uses = new RegExp(`(['"\`])${event}\\1`);
  const allowed = new RegExp(`raw-${event}:\\s*\\S`);
  return lines.flatMap((line, i) => uses.test(line) && !allowed.test(line) && !allowed.test(lines[i - 1] ?? '') ? [i + 1] : []);
}

test('the detector finds raw listeners and honours a justified exception', () => {
  assert.deepEqual(rawPageErrorListeners(`page.on('${event}', e => errors.push(String(e)));`), [1]);
  assert.deepEqual(rawPageErrorListeners(`page.on("${event}", e => {});\nawait page.waitForEvent(\`${event}\`);`), [1, 2]);
  assert.deepEqual(rawPageErrorListeners(`// raw-${event}: asserts what WebKit reports\npage.on('${event}', e => seen.push(e));`), []);
  assert.deepEqual(rawPageErrorListeners(`page.on('${event}', e => seen.push(e)); // raw-${event}: why`), []);
  assert.deepEqual(rawPageErrorListeners(`// raw-${event}:\npage.on('${event}', e => seen.push(e));`), [2], 'an exception needs a reason');
  assert.deepEqual(rawPageErrorListeners(`const errors = await collectPageErrors(page); // not the raw ${event} event`), []);
});

test('UI gates and scenarios count page errors through the fixture, never the raw event', () => {
  const offenders = ['scripts', 'scenarios'].flatMap(folder => sources(join(v2, folder)))
    .map(path => relative(v2, path).split('\\').join('/'))
    .filter(path => path !== fixture && path !== self)
    .flatMap(path => rawPageErrorListeners(readFileSync(join(v2, path), 'utf8')).map(line => `${path}:${line}`));
  assert.deepEqual(offenders, [], `use collectPageErrors or collectContextPageErrors from ${fixture}, or mark a justified exception with "raw-${event}: <why>"`);
});
