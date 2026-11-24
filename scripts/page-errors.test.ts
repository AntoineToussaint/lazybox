// Every UI gate counts page errors through scripts/fixtures/page-errors.mjs,
// and none through Playwright's raw error events.
//
// The raw events (`pageerror` on a page, `weberror` on a context) are wrong:
// WebKit's protocol reports a fetch that a navigation cut, and the app caught,
// as a JavaScript error ("… due to access control checks"), so a gate asserting
// "no page errors" on them fails at random whenever a reload or goto cuts a
// poll. The fixture counts only the browser's own uncaught errors and
// unhandled rejections.
//
// Two checks:
// - No script listens to a raw event, by its name, by a name built from parts,
//   or by a name the line does not spell out at all. A script with a real
//   reason (say, asserting what WebKit reports) marks the line, or the one
//   above, with `raw-pageerror: <why>`.
// - Every script that opens a page, of a UI gate or named test-*.mjs, collects
//   its errors through the fixture, or says why not with
//   `page-errors-opt-out: <why>`.
// - Such a script installs its collector before any init script of the same
//   page or context (a context's before its own), or says why not with
//   `init-before-page-errors: <why>`: an init script that threw first would
//   go unheard.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { existsSync, readFileSync, readdirSync, statSync } from 'node:fs';
import { join, relative } from 'node:path';
import { fileURLToPath } from 'node:url';

const v2 = fileURLToPath(new URL('..', import.meta.url));
const events = ['page' + 'error', 'web' + 'error'];
const fixture = 'scripts/fixtures/page-errors.mjs';
const self = 'scripts/page-errors.test.ts';
const marker = 'raw-' + 'page' + 'error';
const optOut = 'page-errors-' + 'opt-out';
const initOrder = 'init-before-' + 'page-errors';

function sources(folder: string): string[] {
  return readdirSync(folder).flatMap(name => {
    if (name === 'node_modules' || name.startsWith('.')) return [];
    const path = join(folder, name);
    return statSync(path).isDirectory() ? sources(path) : /\.(mjs|cjs|js|ts|mts|tsx)$/.test(name) && !name.endsWith('.d.mts') ? [path] : [];
  });
}

/** The lines of `text` that subscribe to, or wait for, a raw error event, or an event it does not name, without a justified exception. */
export function rawPageErrorListeners(text: string): number[] {
  const lines = text.split('\n');
  // 'page' + 'error' reads as 'pageerror'.
  const joined = (line: string) => line.replace(/(['"`])\s*\+\s*['"`]/g, '');
  const named = new RegExp(`['"\`](${events.join('|')})['"\`]`);
  // An event name that is not a string literal could be anything. A process signal is not a page event.
  const unnamed = /(?<!\bprocess)\.(on|once|addListener|prependListener|waitForEvent)\(\s*(?!['"`])[^)\s]/;
  const allowed = new RegExp(`${marker}:[ \\t]*\\S`);
  return lines.flatMap((line, i) => (named.test(joined(line)) || unnamed.test(line)) && !allowed.test(line) && !allowed.test(lines[i - 1] ?? '') ? [i + 1] : []);
}

/** Whether a script that opens a page collects its errors through the fixture, or says why not. */
export function collectsPageErrors(text: string): boolean {
  if (!/\.newPage\(/.test(text)) return true;
  return /fixtures\/page-errors\.mjs['"]/.test(text) || new RegExp(`${optOut}:[ \\t]*\\S`).test(text);
}

/**
 * The lines of `text` that install an init script on a page or context no
 * collector covers yet: one that throws would go unheard. A context's init
 * scripts run before its pages', so only a context collector covers them.
 */
export function initScriptsBeforeCollector(text: string): number[] {
  const lines = text.split('\n');
  const collectors = [...text.matchAll(/collect(Context)?PageErrors\(\s*([\w$]+)/g)];
  const allowed = new RegExp(`${initOrder}:[ \\t]*\\S`);
  return [...text.matchAll(/([\w$]+)\.addInitScript\(/g)].flatMap(init => {
    const line = text.slice(0, init.index).split('\n').length;
    if (allowed.test(lines[line - 1]) || allowed.test(lines[line - 2] ?? '')) return [];
    const target = init[1];
    // A context collector covers its own init scripts and any page's (a context's run first); a page collector, its page's.
    const isContext = /context/i.test(target);
    const covered = collectors.some(c => c.index! < init.index! && (c[1] ? c[2] === target || !isContext : c[2] === target && !isContext));
    return covered ? [] : [line];
  });
}

test('the init-order check finds an init script installed before its collector', () => {
  assert.deepEqual(initScriptsBeforeCollector(`const context = await browser.newContext();\nawait context.addInitScript(stub);\nconst errors = await collectContextPageErrors(context);`), [2]);
  assert.deepEqual(initScriptsBeforeCollector(`const errors = await collectContextPageErrors(context);\nawait context.addInitScript(stub);`), []);
  assert.deepEqual(initScriptsBeforeCollector(`await collectPageErrors(page, errors);\nawait context.addInitScript(stub);`), [2], "a page collector does not cover its context's init scripts");
  assert.deepEqual(initScriptsBeforeCollector(`await collectPageErrors(page, errors);\nawait page.addInitScript(stub);`), []);
  assert.deepEqual(initScriptsBeforeCollector(`// ${initOrder}: the stub cannot throw, and the page is not collected\nawait context.addInitScript(stub);`), []);
});

test('the detector finds raw listeners and honours a justified exception', () => {
  const [page, web] = events;
  assert.deepEqual(rawPageErrorListeners(`page.on('${page}', e => errors.push(String(e)));`), [1]);
  assert.deepEqual(rawPageErrorListeners(`page.on("${page}", e => {});\nawait page.waitForEvent(\`${page}\`);`), [1, 2]);
  assert.deepEqual(rawPageErrorListeners(`context.on('${web}', e => errors.push(String(e.error())));`), [1]);
  assert.deepEqual(rawPageErrorListeners(`page.on('page' + 'error', e => {});\ncontext.on("web"+'error', e => {});`), [1, 2], 'a name built from parts');
  assert.deepEqual(rawPageErrorListeners(`const name = which();\npage.on(name, e => {});\ncontext.once(events[0], e => {});`), [2, 3], 'a name the line does not spell out');
  assert.deepEqual(rawPageErrorListeners(`process.on(signal, () => child.kill(signal));\npage.on('console', m => {});`), [], 'a process signal, a named page event');
  assert.deepEqual(rawPageErrorListeners(`// ${marker}: asserts what WebKit reports\npage.on('${page}', e => seen.push(e));`), []);
  assert.deepEqual(rawPageErrorListeners(`page.on('${page}', e => seen.push(e)); // ${marker}: why`), []);
  assert.deepEqual(rawPageErrorListeners(`// ${marker}:\npage.on('${page}', e => seen.push(e));`), [2], 'an exception needs a reason');
  assert.deepEqual(rawPageErrorListeners(`const errors = await collectPageErrors(page); // not the raw ${page} event`), []);
});

test('the inverse check asks a script that opens a page to collect, or say why not', () => {
  assert.equal(collectsPageErrors(`const page = await browser.newPage();\nawait page.goto(url);`), false);
  assert.equal(collectsPageErrors(`import { collectPageErrors } from './fixtures/page-errors.mjs';\nconst page = await browser.newPage();`), true);
  assert.equal(collectsPageErrors(`// ${optOut}: renders a static image, no app code\nconst page = await browser.newPage();`), true);
  assert.equal(collectsPageErrors(`// ${optOut}:\nconst page = await browser.newPage();`), false, 'an opt-out needs a reason');
  assert.equal(collectsPageErrors(`const server = createServer();`), true, 'no page, nothing to collect');
});

test('UI gates and scenarios count page errors through the fixture, never a raw event', () => {
  const offenders = ['scripts', 'scenarios'].flatMap(folder => sources(join(v2, folder)))
    .map(path => relative(v2, path).split('\\').join('/'))
    .filter(path => path !== fixture && path !== self)
    .flatMap(path => rawPageErrorListeners(readFileSync(join(v2, path), 'utf8')).map(line => `${path}:${line}`));
  assert.deepEqual(offenders, [], `use collectPageErrors or collectContextPageErrors from ${fixture}, or mark a justified exception with "${marker}: <why>"`);
});

test('every script of a UI gate, and every test-*.mjs, that opens a page collects its errors', () => {
  const catalog = JSON.parse(readFileSync(join(v2, 'scripts/release-gates.json'), 'utf8')) as { gates: { id: string; scope: string; script: string }[] };
  const scripts = (JSON.parse(readFileSync(join(v2, 'package.json'), 'utf8')) as { scripts: Record<string, string> }).scripts;
  const files = new Set(readdirSync(join(v2, 'scripts')).filter(name => /^test-.*\.mjs$/.test(name)).map(name => `scripts/${name}`));
  for (const gate of catalog.gates.filter(gate => gate.scope === 'ui')) {
    const command = scripts[gate.script] ?? '';
    for (const [file] of command.matchAll(/(?:scripts|scenarios)\/[\w./-]+\.(?:mjs|ts)/g)) {
      assert.ok(existsSync(join(v2, file)), `${gate.id}: ${file} exists`);
      files.add(file);
    }
  }
  // The scenarios' runner opens the pages its entry point drives.
  files.add('scenarios/lib/runner.ts');
  const missing = [...files].sort().filter(file => !collectsPageErrors(readFileSync(join(v2, file), 'utf8')));
  assert.deepEqual(missing, [], `collect through ${fixture} (collectPageErrors, collectContextPageErrors) and assert none, or mark "${optOut}: <why>"`);
  const early = [...files].sort().flatMap(file => initScriptsBeforeCollector(readFileSync(join(v2, file), 'utf8')).map(line => `${file}:${line}`));
  assert.deepEqual(early, [], `install the collector (collectContextPageErrors for a context) before the init script, or mark "${initOrder}: <why>"`);
});
