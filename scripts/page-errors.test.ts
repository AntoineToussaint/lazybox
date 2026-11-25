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
const undrained = 'undrained-' + 'page-errors';
const rawMethod = 'page' + 'Errors';

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
  // Playwright's page.pageErrors() returns the same raw errors, after the fact.
  const method = new RegExp(`\\.${rawMethod}\\s*\\(|\\[\\s*['"\`]${rawMethod}['"\`]\\s*\\]`);
  const allowed = new RegExp(`${marker}:[ \\t]*\\S`);
  return lines.flatMap((line, i) => (named.test(joined(line)) || unnamed.test(line) || method.test(joined(line))) &&!allowed.test(line) && !allowed.test(lines[i - 1] ?? '') ? [i + 1] : []);
}

/** Whether a script that opens a page collects its errors through the fixture, or says why not. */
export function collectsPageErrors(text: string): boolean {
  if (!/\.newPage\(/.test(text)) return true;
  return /fixtures\/page-errors\.mjs['"]/.test(text) || new RegExp(`${optOut}:[ \\t]*\\S`).test(text);
}

/**
 * The lines of `text` that open a page (`x = await y.newPage(`) no collector
 * covers: neither collectPageErrors(x, …) before `x` names another page, nor a
 * context collector on the context it came from, nor a marked opt-out on the
 * line or the one above (a page that runs no Lazybox code). A script that opts
 * out as a whole (it does not use the fixture) says so once, at the top.
 */
export function uncollectedPages(text: string): number[] {
  if (!/fixtures\/page-errors\.mjs['"]/.test(text)) return [];
  const lines = text.split('\n');
  const allowed = new RegExp(`${optOut}:[ \\t]*\\S`);
  const opened = [...text.matchAll(/([\w$]+)\s*=\s*(?:await\s+)?([\w$]+)\.newPage\(/g)];
  return opened.flatMap((m, k) => {
    const at = m.index!, line = text.slice(0, at).split('\n').length, page = m[1], from = m[2];
    if (allowed.test(lines[line - 1]) || allowed.test(lines[line - 2] ?? '')) return [];
    // Until the same name opens another page.
    const next = opened.slice(k + 1).find(o => o[1] === page)?.index ?? text.length;
    if (new RegExp(`collectPageErrors\\(\\s*${page.replace(/\$/g, '\\$')}\\s*[,)]`).test(text.slice(at, next))) return [];
    if (from !== 'browser' && new RegExp(`collectContextPageErrors\\(\\s*${from.replace(/\$/g, '\\$')}\\s*[,)]`).test(text)) return [];
    return [line];
  });
}

/**
 * The lines of `text` that install an init script on a page or context no
 * collector covers yet: one that throws would go unheard. The collector must
 * be on the same variable, installed since that variable was last assigned:
 * a context's (`X = … .newContext(`) by collectContextPageErrors(X); a page's
 * by collectPageErrors(page), or by collectContextPageErrors on the context the
 * page came from (`page = … ctx.newPage(`), whose init scripts run first.
 */
export function initScriptsBeforeCollector(text: string): number[] {
  const lines = text.split('\n');
  const assignments = [...text.matchAll(/([\w$]+)\s*=\s*(?:await\s+)?(?:([\w$]+)\.newPage|[\w$.()]*?\.newContext)\(/g)]
    .map(m => ({ name: m[1], index: m.index!, context: m[0].includes('newContext'), from: m[2] }));
  const collectors = [...text.matchAll(/collect(Context)?PageErrors\(\s*([\w$]+)/g)].map(m => ({ context: !!m[1], name: m[2], index: m.index! }));
  const latest = (name: string, before: number) => assignments.filter(a => a.name === name && a.index < before).at(-1);
  const collected = (name: string, context: boolean, after: number, before: number) =>
    collectors.some(c => c.name === name && c.context === context && c.index > after && c.index < before);
  const allowed = new RegExp(`${initOrder}:[ \\t]*\\S`);
  return [...text.matchAll(/([\w$]+)\.addInitScript\(/g)].flatMap(init => {
    const at = init.index!, line = text.slice(0, at).split('\n').length;
    if (allowed.test(lines[line - 1]) || allowed.test(lines[line - 2] ?? '')) return [];
    const target = init[1], assigned = latest(target, at), after = assigned?.index ?? -1;
    if (assigned?.context) return collected(target, true, after, at) ? [] : [line];
    if (collected(target, false, after, at)) return [];
    // A page of a context: that context's collector, installed since the context was made, covers it.
    const source = assigned?.from ? latest(assigned.from, assigned.index) : undefined;
    if (source?.context && collected(source.name, true, source.index, at)) return [];
    // A name the script never assigns (a parameter): a collector of either kind on it.
    if (!assigned && collected(target, true, -1, at)) return [];
    return [line];
  });
}

/**
 * The lines of `text` that assert on an array a collector fills without
 * draining it first (on the same line or the three before): an error the
 * last action raised may still be on its way.
 */
export function undrainedAssertions(text: string): number[] {
  const lines = text.split('\n');
  const names = new Set<string>();
  for (const m of text.matchAll(/(?:const|let|var)\s+([\w$]+)\s*=\s*await\s+collect(?:Context)?PageErrors\(/g)) names.add(m[1]);
  for (const m of text.matchAll(/collect(?:Context)?PageErrors\(\s*[\w$.()]+\s*,\s*([\w$.]+)/g)) names.add(m[1].split('.').at(-1)!);
  if (!names.size) return [];
  const allowed = new RegExp(`${undrained}:[ \\t]*\\S`);
  return lines.flatMap((line, i) => [...line.matchAll(/(?:assert\.\w+|expect)\(\s*([\w$.]+)/g)].flatMap(m => {
    const path = m[1].split('.');
    const at = path.findIndex(part => names.has(part));
    if (at < 0) return [];
    const array = path.slice(0, at + 1).join('.');
    const before = [...lines.slice(Math.max(0, i - 3), i), line.slice(0, m.index)].join('\n');
    if (before.includes(`drainPageErrors(${array})`)) return [];
    if (allowed.test(line) || allowed.test(lines[i - 1] ?? '')) return [];
    return [i + 1];
  }));
}

test('the init-order check finds an init script installed before its collector', () => {
  assert.deepEqual(initScriptsBeforeCollector(`const context = await browser.newContext();\nawait context.addInitScript(stub);\nconst errors = await collectContextPageErrors(context);`), [2]);
  assert.deepEqual(initScriptsBeforeCollector(`const errors = await collectContextPageErrors(context);\nawait context.addInitScript(stub);`), []);
  assert.deepEqual(initScriptsBeforeCollector(`await collectPageErrors(page, errors);\nawait context.addInitScript(stub);`), [2], "a page collector does not cover its context's init scripts");
  assert.deepEqual(initScriptsBeforeCollector(`await collectPageErrors(page, errors);\nawait page.addInitScript(stub);`), []);
  assert.deepEqual(initScriptsBeforeCollector(`// ${initOrder}: the stub cannot throw, and the page is not collected\nawait context.addInitScript(stub);`), []);
  assert.deepEqual(initScriptsBeforeCollector(`const plain = await browser.newContext();\nawait plain.addInitScript(stub);`), [2], 'a context whose name does not say so');
  assert.deepEqual(initScriptsBeforeCollector(`const ctx = await browser.newContext();\nawait collectContextPageErrors(ctx, errors);\nawait ctx.addInitScript(stub);`), []);
  assert.deepEqual(initScriptsBeforeCollector(`const a = await browser.newContext();\nawait collectContextPageErrors(a, errors);\nconst b = await browser.newContext();\nawait b.addInitScript(stub);`), [4], "context a's collector does not cover context b");
  assert.deepEqual(initScriptsBeforeCollector(`const ctx = await browser.newContext();\nawait collectContextPageErrors(ctx, errors);\nconst page = await ctx.newPage();\nawait page.addInitScript(stub);`), [], 'a page of a collected context');
  assert.deepEqual(initScriptsBeforeCollector(`const ctx = await browser.newContext();\nawait collectPageErrors(page, errors);\nawait ctx.addInitScript(stub);`), [3], 'a page collector does not cover a context, whatever its name');
  assert.deepEqual(initScriptsBeforeCollector(`let context = await browser.newContext();\nawait collectContextPageErrors(context, errors);\ncontext = await browser.newContext();\nawait context.addInitScript(stub);`), [4], 'a collector on the context the name held before does not count');
});

test('the page check finds a page opened without a collector', () => {
  const head = "import { collectPageErrors } from './fixtures/page-errors.mjs';\n";
  assert.deepEqual(uncollectedPages(head + 'const page = await browser.newPage();\nawait collectPageErrors(page, errors);'), []);
  assert.deepEqual(uncollectedPages(head + 'const other = await context.newPage();\nawait other.goto(url);'), [2]);
  assert.deepEqual(uncollectedPages(head + 'await collectContextPageErrors(context, errors);\nconst other = await context.newPage();'), []);
  assert.deepEqual(uncollectedPages(head + `// ${optOut}: draws a picture\nconst scratch = await browser.newPage();`), []);
  // A collector on a later page of the same name does not cover the first.
  assert.deepEqual(uncollectedPages(head + 'let page = await browser.newPage();\npage = await browser.newPage();\nawait collectPageErrors(page, errors);'), [2]);
  assert.deepEqual(uncollectedPages('const page = await browser.newPage();'), [], 'a script without the fixture is the whole-file check\'s');
});

test('the drain check finds an assertion on collected errors that was not drained first', () => {
  const collect = 'const errors = await collectPageErrors(page);\n';
  assert.deepEqual(undrainedAssertions(`${collect}await page.click('a');\nassert.deepEqual(errors, []);`), [3]);
  assert.deepEqual(undrainedAssertions(`${collect}await drainPageErrors(errors);\nassert.deepEqual(errors, []);`), []);
  assert.deepEqual(undrainedAssertions(`${collect}await drainPageErrors(errors);assert.deepEqual(errors.filter(e => !/x/.test(e)), []);`), []);
  assert.deepEqual(undrainedAssertions(`${collect}assert.deepEqual(errors.filter(e => !/x/.test(e)), []);`), [2], 'a filtered list');
  assert.deepEqual(undrainedAssertions(`const log = {errors: []};\nawait collectPageErrors(page, log.errors);\nassert.deepEqual(main.log.errors, []);`), [3], 'the array reached through another object');
  assert.deepEqual(undrainedAssertions(`${collect}// ${undrained}: asserts what arrived before the reload\nassert.equal(errors.length, 1);`), []);
  assert.deepEqual(undrainedAssertions(`const other = [];\nassert.deepEqual(other, []);`), [], 'no collector, nothing to say');
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
  assert.deepEqual(rawPageErrorListeners(`const raw = await page.${rawMethod}();\nconst also = await page['${rawMethod}']();`), [1, 2], "Playwright's page.pageErrors()");
  assert.deepEqual(rawPageErrorListeners(`await drainPageErrors(errors);`), [], 'the fixture is not the raw method');
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
  // The fixture's own test times its assertions itself: it polls, and asserts what arrived before a drain on purpose.
  const unsettled = [...files].sort().filter(file => file !== 'scripts/test-page-errors.mjs')
    .flatMap(file => undrainedAssertions(readFileSync(join(v2, file), 'utf8')).map(line => `${file}:${line}`));
  assert.deepEqual(unsettled, [], `await drainPageErrors(errors) before asserting on them, or mark "${undrained}: <why>"`);
  const uncovered = [...files].sort().flatMap(file => uncollectedPages(readFileSync(join(v2, file), 'utf8')).map(line => `${file}:${line}`));
  assert.deepEqual(uncovered, [], `collect every page's errors (collectPageErrors(page, errors) before it navigates), or mark "${optOut}: <why>" on a page that runs no Lazybox code`);
  const early = [...files].sort().flatMap(file => initScriptsBeforeCollector(readFileSync(join(v2, file), 'utf8')).map(line => `${file}:${line}`));
  assert.deepEqual(early, [], `install the collector (collectContextPageErrors for a context) before the init script, or mark "${initOrder}: <why>"`);
});
