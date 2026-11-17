import { test } from 'node:test';
import assert from 'node:assert/strict';
import { absolutePath, dedupeLabels, fencedCode, firstEnabled, isMenuClick, joinSections, lastEnabled, normalizeMenu, placeMenu, placeSubmenu, placeTooltip, quoteText, rightReleaseChooses, stepFocus, typeahead, type MenuItem } from '../packages/surfaces/src/context-menu-model.ts';

const labels = (items: MenuItem[]) => normalizeMenu(items).map(entry => entry.kind === 'separator' ? '—' : entry.action.label);

test('a menu drops empty items, empty submenus and stray separators', () => {
  const items: MenuItem[] = ['separator', { label: 'Open' }, false, null, '', 'separator', 'separator', { label: 'More', submenu: [false, 'separator'] }, { label: 'Copy' }, 'separator'];
  assert.deepEqual(labels(items), ['Open', '—', 'Copy']);
  const nested = normalizeMenu([{ label: 'Permissions', submenu: [{ label: 'Ask' }, 'separator', { label: 'YOLO' }] }]);
  assert.equal(nested[0].kind === 'action' && nested[0].submenu?.length, 3);
});

test('sections join innermost first with one line between non-empty ones, and a label shows once', () => {
  const joined = joinSections([[{ label: 'Copy' }], [], [false], [{ label: 'Copy code' }], [{ label: 'Copy' }, { label: 'Quote in reply' }]]);
  assert.deepEqual(labels(dedupeLabels(joined)), ['Copy', '—', 'Copy code', '—', 'Quote in reply']);
});

test('a menu opens down-right of the pointer and flips or pins to stay on screen', () => {
  const viewport = { width: 1000, height: 700 }, size = { width: 220, height: 300 };
  // Two pixels clear of the pointer, so the press that opened it is not on it.
  assert.deepEqual(placeMenu({ x: 100, y: 100 }, size, viewport), { left: 102, top: 102 });
  // Near the right and bottom edges: it opens left and up of the pointer, as clear of it.
  assert.deepEqual(placeMenu({ x: 950, y: 650 }, size, viewport), { left: 728, top: 348 });
  // Taller than the space either way: pinned inside the margin, never off screen.
  const tall = placeMenu({ x: 500, y: 350 }, { width: 220, height: 650 }, viewport);
  assert.equal(tall.top, 8 + (700 - 16 - 650));
  const huge = placeMenu({ x: 10, y: 10 }, { width: 1200, height: 900 }, viewport);
  assert.deepEqual(huge, { left: 8, top: 8 });
  for (const at of [{ x: 0, y: 0 }, { x: 999, y: 699 }, { x: 500, y: 699 }]) {
    const place = placeMenu(at, size, viewport);
    assert.ok(place.left >= 8 && place.left + size.width <= 992 && place.top >= 8 && place.top + size.height <= 692, JSON.stringify(at));
  }
});

test('the pointer that opened a menu is never on it, unless the window is too small for the menu either way', () => {
  const viewport = { width: 1000, height: 700 }, size = { width: 240, height: 330 };
  const on = (at: { x: number; y: number }, place: { left: number; top: number }) => at.x >= place.left && at.x <= place.left + size.width && at.y >= place.top && at.y <= place.top + size.height;
  for (const at of [{ x: 100, y: 100 }, { x: 997, y: 697 }, { x: 900, y: 650.4 }, { x: 760.6, y: 371.7 }, { x: 3, y: 690 }, { x: 990, y: 5 }]) {
    assert.equal(on(at, placeMenu(at, size, viewport)), false, JSON.stringify(at));
  }
});

test('a right-button release chooses an item only after the pointer moved there from where the menu opened', () => {
  const origin = { x: 400, y: 300 };
  // A slow right-click: released where it was pressed, wherever the menu is.
  assert.equal(rightReleaseChooses(origin, origin), false);
  assert.equal(rightReleaseChooses(origin, { x: 403, y: 302 }), false, 'a trackpad jitter is not a drag');
  // Press, drag onto the item, release.
  assert.equal(rightReleaseChooses(origin, { x: 420, y: 340 }), true);
  assert.equal(rightReleaseChooses(origin, { x: 390, y: 290 }), true);
});

test('Ctrl+click is the context-menu gesture on a Mac only', () => {
  assert.equal(isMenuClick({ button: 0, ctrlKey: true }, true), true);
  assert.equal(isMenuClick({ button: 0, ctrlKey: false }, true), false);
  // Elsewhere Ctrl+click is the browser's "open in a new tab".
  assert.equal(isMenuClick({ button: 0, ctrlKey: true }, false), false);
  assert.equal(isMenuClick({ button: 1, ctrlKey: true }, true), false);
});

test('a submenu opens beside its item, on the left when the right has no room', () => {
  const viewport = { width: 1000, height: 700 }, size = { width: 200, height: 120 };
  assert.deepEqual(placeSubmenu({ left: 100, right: 300, top: 50, bottom: 78 }, size, viewport), { left: 298, top: 45 });
  assert.deepEqual(placeSubmenu({ left: 700, right: 900, top: 50, bottom: 78 }, size, viewport), { left: 502, top: 45 });
  assert.equal(placeSubmenu({ left: 100, right: 300, top: 660, bottom: 688 }, size, viewport).top, 700 - 8 - 120);
});

test('arrows skip separators and disabled items and wrap around', () => {
  const entries = normalizeMenu([{ label: 'Open', disabled: true }, { label: 'Copy' }, 'separator', { label: 'Cut', disabled: 'Read only' }, { label: 'Close' }]);
  assert.equal(firstEnabled(entries), 1);
  assert.equal(lastEnabled(entries), 4);
  assert.equal(stepFocus(entries, 1, 1), 4);
  assert.equal(stepFocus(entries, 4, 1), 1);
  assert.equal(stepFocus(entries, 1, -1), 4);
  assert.equal(stepFocus(normalizeMenu([{ label: 'Only', disabled: true }]), -1, 1), -1);
});

test('typing jumps to the matching item; repeating a letter cycles', () => {
  const entries = normalizeMenu([{ label: 'Copy path' }, { label: 'Copy relative path' }, { label: 'Close' }, { label: 'Close others', disabled: true }, { label: 'Reveal in Files' }]);
  assert.equal(typeahead(entries, 'r', -1), 4);
  assert.equal(typeahead(entries, 'c', -1), 0);
  assert.equal(typeahead(entries, 'c', 0), 1);
  assert.equal(typeahead(entries, 'cc', 1), 2);
  assert.equal(typeahead(entries, 'copy r', 0), 1);
  assert.equal(typeahead(entries, 'close o', 0), -1, 'a disabled item is not chosen');
  assert.equal(typeahead(entries, 'x', 0), -1);
});

test('quoting, fencing and absolute paths', () => {
  assert.equal(quoteText('first\n\nsecond\n'), '> first\n>\n> second\n\n');
  assert.equal(fencedCode('a\n', 'ts'), '```ts\na\n```');
  assert.equal(fencedCode('x\n```\ny'), '````\nx\n```\ny\n````');
  assert.equal(absolutePath('/w/alpha/', 'api/src/main.go'), '/w/alpha/api/src/main.go');
  assert.equal(absolutePath('/w/alpha', '.'), '/w/alpha');
  assert.equal(absolutePath('', 'api/x'), 'api/x');
  assert.equal(absolutePath('/w', '/etc/hosts'), '/etc/hosts');
});

test('a tooltip sits under its element, flips above at the bottom edge, and is always whole inside the window', () => {
  const size = { width: 260, height: 28 }, viewport = { width: 1000, height: 700 };
  // Room everywhere: centred under the element.
  assert.deepEqual(placeTooltip({ left: 400, right: 440, top: 100, bottom: 124 }, size, viewport), { left: 290, top: 130 });
  // At the right edge (the Track button): pulled in, not cut.
  const right = placeTooltip({ left: 960, right: 996, top: 100, bottom: 124 }, size, viewport);
  assert.equal(right.left + size.width, viewport.width - 8);
  // At the left edge.
  assert.equal(placeTooltip({ left: 2, right: 30, top: 100, bottom: 124 }, size, viewport).left, 8);
  // No room below: above the element.
  assert.deepEqual(placeTooltip({ left: 400, right: 440, top: 670, bottom: 694 }, size, viewport), { left: 290, top: 636 });
});
