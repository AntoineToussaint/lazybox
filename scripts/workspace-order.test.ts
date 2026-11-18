import { test } from 'node:test';
import assert from 'node:assert/strict';
import { dropAt, dropSlot, edgeScroll, inOrder, moveBy, movedWords, sameOrder, sameSlot } from '../packages/surfaces/src/workspace-order.ts';

const ids = ['a', 'b', 'c', 'd'];

test('a dragged row lands at the slot the line shows', () => {
  assert.deepEqual(dropAt(ids, 'a', 0), ['a', 'b', 'c', 'd']);
  assert.deepEqual(dropAt(ids, 'a', 2), ['b', 'a', 'c', 'd'], 'between b and c');
  assert.deepEqual(dropAt(ids, 'a', 4), ['b', 'c', 'd', 'a'], 'after the last');
  assert.deepEqual(dropAt(ids, 'd', 0), ['d', 'a', 'b', 'c'], 'before the first');
  assert.deepEqual(dropAt(ids, 'c', 1), ['a', 'c', 'b', 'd']);
  assert.deepEqual(dropAt(ids, 'missing', 1), ids);
  assert.deepEqual(dropAt(ids, 'b', 99), ['a', 'c', 'd', 'b'], 'held at the end');
});

test('a slot just above or below the row itself changes nothing, so no line shows there', () => {
  assert.equal(sameSlot(ids, 'b', 1), true);
  assert.equal(sameSlot(ids, 'b', 2), true);
  assert.equal(sameSlot(ids, 'b', 0), false);
  assert.equal(sameSlot(ids, 'b', 3), false);
  for (const slot of [1, 2]) assert.deepEqual(dropAt(ids, 'b', slot), ids);
});

test('⌥⇧↑ and ⌥⇧↓ move a row one place, held at either end', () => {
  assert.deepEqual(moveBy(ids, 'b', -1), ['b', 'a', 'c', 'd']);
  assert.deepEqual(moveBy(ids, 'b', 1), ['a', 'c', 'b', 'd']);
  assert.deepEqual(moveBy(ids, 'a', -1), ids);
  assert.deepEqual(moveBy(ids, 'd', 1), ids);
  assert.deepEqual(moveBy(ids, 'c', 1), ['a', 'b', 'd', 'c']);
});

test('the pointer picks the slot before the first row whose middle is below it', () => {
  const rows = [{ top: 0, bottom: 40 }, { top: 40, bottom: 80 }, { top: 80, bottom: 200 }];
  assert.equal(dropSlot(rows, -10), 0);
  assert.equal(dropSlot(rows, 19), 0);
  assert.equal(dropSlot(rows, 21), 1);
  assert.equal(dropSlot(rows, 79), 2);
  assert.equal(dropSlot(rows, 139), 2, 'a tall row (its conversations open) splits at its middle');
  assert.equal(dropSlot(rows, 141), 3);
  assert.equal(dropSlot([], 5), 0);
});

test('a move shows at once: rows the order does not name (new ones) first, then the kept order', () => {
  const rows = ['a', 'b', 'c', 'n'].map(id => ({ id }));
  assert.deepEqual(inOrder(rows, ['c', 'a', 'b']).map(r => r.id), ['n', 'c', 'a', 'b']);
  assert.deepEqual(inOrder(rows, ['c', 'gone', 'a']).map(r => r.id), ['b', 'n', 'c', 'a'], 'an unknown id is ignored');
  assert.deepEqual(inOrder(rows, undefined).map(r => r.id), ['a', 'b', 'c', 'n']);
  assert.deepEqual(inOrder(rows, []).map(r => r.id), ['a', 'b', 'c', 'n']);
  assert.equal(sameOrder(['a', 'b'], ['a', 'b']), true);
  assert.equal(sameOrder(['a', 'b'], ['b', 'a']), false);
  assert.equal(sameOrder(['a'], ['a', 'b']), false);
});

test('near an edge the list scrolls, faster closer to it', () => {
  assert.equal(edgeScroll(300, 0, 600), 0);
  assert.ok(edgeScroll(5, 0, 600) < edgeScroll(30, 0, 600) && edgeScroll(30, 0, 600) < 0, 'up, faster at the very top');
  assert.ok(edgeScroll(595, 0, 600) > edgeScroll(570, 0, 600) && edgeScroll(570, 0, 600) > 0, 'down, faster at the very bottom');
  assert.equal(edgeScroll(-50, 0, 600), -16, 'past the edge: the most');
  assert.equal(edgeScroll(50, 0, 100) !== 0, false, 'a short list keeps its middle still');
});

test('a screen reader hears where the row went', () => {
  assert.equal(movedWords('Alpha feature', ['b', 'a', 'c'], 'a'), 'Alpha feature moved to 2 of 3');
});
