import { test } from 'node:test';
import assert from 'node:assert/strict';
import { arrive, current, emptyHistory, linkHint, nextZoom, originOf, pageLabel, parseHistory, resolveAddress, searchEngine, step, titled, visit, webPage } from '../packages/surfaces/src/web-address.ts';
import { placement } from '../packages/surfaces/src/native-web-view.ts';

const ddg = searchEngine('duckduckgo');

test('the address bar takes an address, with or without its scheme', () => {
  assert.deepEqual(resolveAddress('https://github.com/org/repo'), { url: 'https://github.com/org/repo' });
  assert.deepEqual(resolveAddress('github.com/org/repo'), { url: 'https://github.com/org/repo' });
  assert.deepEqual(resolveAddress('  example.com  '), { url: 'https://example.com/' });
  // This machine is plain http, as a dev server is.
  assert.deepEqual(resolveAddress('localhost:3000/app'), { url: 'http://localhost:3000/app' });
  assert.deepEqual(resolveAddress('127.0.0.1:8080'), { url: 'http://127.0.0.1:8080/' });
  assert.deepEqual(resolveAddress('web.localhost:5173'), { url: 'http://web.localhost:5173/' });
});

test('words are a search with the chosen engine', () => {
  assert.deepEqual(resolveAddress('tauri multiwebview', ddg), { url: 'https://duckduckgo.com/?q=tauri%20multiwebview', search: 'tauri multiwebview' });
  assert.deepEqual(resolveAddress('react'), { url: 'https://www.google.com/search?q=react', search: 'react' });
  assert.equal(searchEngine('nonsense').id, 'google');
});

test('anything but a web page is refused, never searched or opened', () => {
  for (const bad of ['javascript:alert(1)', 'file:///etc/passwd', 'data:text/html,hi', 'vscode://open', 'mailto:a@b.c']) {
    const resolved = resolveAddress(bad);
    assert.ok('error' in resolved, bad);
  }
  assert.ok('error' in resolveAddress('   '));
  // A user name in the address dresses one site up as another.
  assert.ok('error' in resolveAddress('https://bank.example@evil.test/'));
  assert.equal(webPage('https://user:pw@example.com/'), '');
  assert.equal(webPage('http://localhost:3000/x'), 'http://localhost:3000/x');
});

test('the origin is shown with whether the connection is secure', () => {
  assert.deepEqual(originOf('https://github.com/x'), { host: 'github.com', secure: true, local: false });
  assert.deepEqual(originOf('http://example.com/'), { host: 'example.com', secure: false, local: false });
  assert.deepEqual(originOf('http://localhost:3000/'), { host: 'localhost:3000', secure: true, local: true });
  assert.equal(originOf('about:blank'), undefined);
});

test('history: a visit, back and forward, and a redirect landing on its entry', () => {
  let history = visit(emptyHistory(), 'https://a.test/');
  history = arrive(history, 'https://a.test/');
  history = visit(history, 'http://b.test/');
  // b redirected to https: the entry takes the address it landed on.
  history = arrive(history, 'https://b.test/');
  assert.deepEqual(history.entries.map(e => e.url), ['https://a.test/', 'https://b.test/']);
  history = step(history, -1);
  assert.equal(current(history)?.url, 'https://a.test/');
  history = arrive(history, 'https://a.test/');
  assert.equal(history.expect, undefined);
  // Visiting from the middle drops what was ahead, as in any browser.
  history = visit(history, 'https://c.test/');
  assert.deepEqual(history.entries.map(e => e.url), ['https://a.test/', 'https://c.test/']);
  assert.equal(step(history, 5).index, 1);
});

test('a link followed inside the page is a new entry; a reload is not', () => {
  let history = arrive(visit(emptyHistory(), 'https://a.test/'), 'https://a.test/');
  history = arrive(history, 'https://a.test/');
  assert.equal(history.entries.length, 1);
  history = arrive(history, 'https://a.test/next');
  assert.deepEqual(history.entries.map(e => e.url), ['https://a.test/', 'https://a.test/next']);
  history = titled(history, 'Next page');
  assert.equal(pageLabel(current(history)), 'Next page');
  assert.equal(pageLabel({ url: 'https://docs.example.com/x' }), 'docs.example.com');
});

test('saved history is read back defensively', () => {
  const saved = parseHistory(JSON.stringify({ entries: [{ url: 'https://a.test/', title: 'A' }, { url: 'javascript:alert(1)' }, { url: 'https://b.test/' }], index: 9 }));
  assert.deepEqual(saved.entries.map(e => e.url), ['https://a.test/', 'https://b.test/']);
  assert.equal(saved.index, 1);
  assert.deepEqual(parseHistory('not json'), emptyHistory());
  assert.deepEqual(parseHistory(null), emptyHistory());
});

test('zoom steps through the usual levels and stops at the ends', () => {
  assert.equal(nextZoom(1, 1), 1.1);
  assert.equal(nextZoom(1, -1), 0.9);
  assert.equal(nextZoom(2, 1), 2);
  assert.equal(nextZoom(0.5, -1), 0.5);
  assert.equal(nextZoom(1.05, 1), 1.1);
});

test('a link says what ⌥-click does, only where a Web panel can open it', () => {
  assert.match(linkHint('https://a.test/', 'browser', true, '⌥'), /⌥-click to open in Lazybox/);
  assert.match(linkHint('https://a.test/', 'viewer', true), /click to open in your browser/);
  assert.equal(linkHint('https://a.test/', 'browser', false), 'https://a.test/');
  assert.equal(linkHint('mailto:a@b.c', 'browser', true), 'mailto:a@b.c');
});

test('a native view steps aside for menus and dialogs, and stays inside the window', () => {
  const rect = (x: number, y: number, width: number, height: number) => ({ left: x, top: y, right: x + width, bottom: y + height, width, height, x, y, toJSON() {} }) as DOMRect;
  const viewport = { width: 1000, height: 800 };
  assert.deepEqual(placement(rect(500, 40, 500, 760), viewport, [], false), { x: 500, y: 40, width: 500, height: 760 });
  // A menu over the panel hides it; one elsewhere does not.
  assert.equal(placement(rect(500, 40, 500, 760), viewport, [rect(600, 50, 200, 200)], false), undefined);
  assert.ok(placement(rect(500, 40, 500, 760), viewport, [rect(10, 50, 200, 200)], false));
  // A modal dialog hides every view; a collapsed panel has no room.
  assert.equal(placement(rect(500, 40, 500, 760), viewport, [], true), undefined);
  assert.equal(placement(rect(500, 40, 500, 4), viewport, [], false), undefined);
  // Clipped to the window.
  assert.deepEqual(placement(rect(800, 40, 500, 900), viewport, [], false), { x: 800, y: 40, width: 200, height: 760 });
});

test('a native view is placed exactly over the page area, fractions and all, and clipped to its panel', () => {
  const rect = (x: number, y: number, width: number, height: number) => ({ left: x, top: y, right: x + width, bottom: y + height, width, height, x, y, toJSON() {} }) as DOMRect;
  const viewport = { width: 1512, height: 916 };
  // The page area under the address bar on fractional CSS pixels (a sidebar
  // 261.5 wide, the interface's text size): passed on as is, for the host to
  // convert to points with the app page's zoom and snap inward, once (viewer.rs).
  assert.deepEqual(placement(rect(262.5, 113.25, 700.5, 760.5), viewport, [], false), { x: 262.5, y: 113.25, width: 700.5, height: 760.5 });
  // An ancestor that clips (a dock group, a scrolled container) clips the
  // view: it never draws outside its panel.
  const group = { left: 262, top: 80, right: 900, bottom: 700 };
  assert.deepEqual(placement(rect(262.5, 113.25, 700.5, 760.5), viewport, [], false, [group]), { x: 262.5, y: 113.25, width: 637.5, height: 586.75 });
  // Scrolled out of its container: hidden.
  assert.equal(placement(rect(262, 760, 700, 300), viewport, [], false, [group]), undefined);
  // A menu over the part of the panel a clip hides leaves the view on screen; one over the rest does not.
  assert.ok(placement(rect(262.5, 113.25, 700.5, 760.5), viewport, [rect(920, 300, 200, 200)], false, [group]));
  assert.equal(placement(rect(262.5, 113.25, 700.5, 760.5), viewport, [rect(880, 300, 200, 200)], false, [group]), undefined);
});
