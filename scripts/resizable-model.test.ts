import { test } from 'node:test';
import assert from 'node:assert/strict';
import type { ViewStateStore } from '../packages/platform/src/index.ts';
import { clampSplit, DEFAULT_SPLIT, dragSplit, keySplit, parseSplit, readSplit, SPLIT_RAIL, splitCss, splitKeys, splitMax, SPLITS, windowScope, writeSplit, type SplitSpec } from '../packages/surfaces/src/resizable-model.ts';

const list: SplitSpec = { name: 'list', side: 'start', min: 200, max: 500, keep: 300, collapsible: true };
const tray: SplitSpec = { name: 'tray', side: 'end', min: 240, max: 560, keep: 320 };
const log: SplitSpec = { name: 'log', side: 'end', axis: 'y', min: 60, max: 600, keep: 120 };

function memory(scope: string, key = `${scope}:panel:p1`): ViewStateStore & { data: Map<string, string> } {
  const data = new Map<string, string>();
  return { key, scope, root: 'runner-a', data, getItem: k => data.get(k) ?? null, setItem: (k, v) => { data.set(k, v); } };
}

test('clamping: never below the minimum, never past the maximum or what leaves the content beside its share', () => {
  assert.equal(clampSplit(list, 50), 200);
  assert.equal(clampSplit(list, 900), 500);
  assert.equal(clampSplit(list, 321.6), 322);
  // 700 px shared with the content, which keeps 300: at most 400.
  assert.equal(splitMax(list, 700), 400);
  assert.equal(clampSplit(list, 480, 700), 400);
  // Too little room for both: the side keeps its minimum (the panel itself is then too narrow).
  assert.equal(splitMax(list, 350), 200);
  assert.equal(splitMax(list), 500);
});

test('dragging: the start side follows the pointer, the end side mirrors it, a collapsible side folds below half its minimum', () => {
  assert.deepEqual(dragSplit(list, DEFAULT_SPLIT, 280, 40, 1200), { size: 320, collapsed: false });
  assert.deepEqual(dragSplit(tray, DEFAULT_SPLIT, 300, 40, 1200), { size: 260, collapsed: false });
  assert.deepEqual(dragSplit(log, DEFAULT_SPLIT, 132, -50, 800), { size: 182, collapsed: false });
  // Past half the minimum: folded, keeping the size it comes back at.
  assert.deepEqual(dragSplit(list, { size: 333, collapsed: false }, 280, -200, 1200), { size: 333, collapsed: true });
  // A side that cannot fold stops at its minimum.
  assert.deepEqual(dragSplit(tray, DEFAULT_SPLIT, 300, 400, 1200), { size: 240, collapsed: false });
  // Dragged back out of the rail.
  assert.deepEqual(dragSplit(list, { size: 333, collapsed: true }, 0, 260, 1200), { size: 260, collapsed: false });
});

test('keys: 16 px a press, 64 with Shift, Home and End, Enter folds; only the arrows of its axis', () => {
  const at = { size: 300, collapsed: false };
  assert.deepEqual(keySplit(list, at, 300, 'ArrowRight', false, 1200), { size: 316, collapsed: false });
  assert.deepEqual(keySplit(list, at, 300, 'ArrowLeft', true, 1200), { size: 236, collapsed: false });
  // The end side grows when its divider moves away from it.
  assert.deepEqual(keySplit(tray, at, 300, 'ArrowLeft', false, 1200), { size: 316, collapsed: false });
  assert.deepEqual(keySplit(log, at, 132, 'ArrowUp', false, 800), { size: 148, collapsed: false });
  assert.equal(keySplit(log, at, 132, 'ArrowLeft', false, 800), null);
  assert.equal(keySplit(list, at, 300, 'ArrowDown', false, 800), null);
  assert.deepEqual(keySplit(list, at, 300, 'Home', false, 1200), { size: 200, collapsed: false });
  assert.deepEqual(keySplit(list, at, 300, 'End', false, 700), { size: 400, collapsed: false });
  assert.deepEqual(keySplit(list, at, 490, 'ArrowRight', true, 1200), { size: 500, collapsed: false });
  // At the minimum, one more step folds a collapsible side; then a step the other way brings it back.
  const folded = keySplit(list, { size: 260, collapsed: false }, 200, 'ArrowLeft', false, 1200);
  assert.deepEqual(folded, { size: 260, collapsed: true });
  assert.deepEqual(keySplit(list, folded!, 0, 'ArrowLeft', false, 1200), folded);
  assert.deepEqual(keySplit(list, folded!, 0, 'ArrowRight', false, 1200), { size: 260, collapsed: false });
  assert.deepEqual(keySplit(tray, at, 240, 'ArrowRight', false, 1200), { size: 240, collapsed: false });
  assert.deepEqual(keySplit(list, at, 300, 'Enter', false), { size: 300, collapsed: true });
  assert.equal(keySplit(tray, at, 300, 'Enter', false), null);
  assert.equal(keySplit(list, at, 300, 'a', false), null);
});

test('the CSS value: the default leaves the panel its own, a fold is the rail, a size is clamped to the room', () => {
  assert.equal(splitCss(list, DEFAULT_SPLIT), null);
  assert.equal(splitCss(list, { size: 333, collapsed: false }), '333px');
  assert.equal(splitCss(list, { size: 480, collapsed: false }, 700), '400px');
  assert.equal(splitCss(list, { size: 333, collapsed: true }), `${SPLIT_RAIL}px`);
  // A side that cannot fold ignores a stray fold.
  assert.equal(splitCss(tray, { size: 300, collapsed: true }), '300px');
});

test('persistence: kept for the window (whatever the workspace) and as the last size any window used', () => {
  const panel = memory('runner-a:window-1:workspace:alpha');
  assert.equal(windowScope(panel), 'runner-a:window-1');
  assert.equal(windowScope(memory('runner-a:window-1')), 'runner-a:window-1');
  assert.deepEqual(splitKeys(panel, 'changes-files'), { window: 'runner-a:window-1:split:changes-files', shared: 'runner-a:split.changes-files' });
  assert.deepEqual(readSplit(panel, list), DEFAULT_SPLIT);
  writeSplit(panel, list, { size: 333, collapsed: false });
  // Another workspace of the same window reads it back.
  const other = memory('runner-a:window-1:workspace:beta');
  other.data.set('runner-a:window-1:split:list', panel.data.get('runner-a:window-1:split:list')!);
  assert.deepEqual(readSplit(other, list), { size: 333, collapsed: false });
  // A new window starts from the last size set anywhere; its own then wins.
  const second = memory('runner-a:window-2:workspace:alpha');
  second.data.set('runner-a:split.list', panel.data.get('runner-a:split.list')!);
  assert.deepEqual(readSplit(second, list), { size: 333, collapsed: false });
  second.data.set('runner-a:window-2:split:list', JSON.stringify({ size: 250, collapsed: true }));
  assert.deepEqual(readSplit(second, list), { size: 250, collapsed: true });
  // Reset is remembered too, over the shared size.
  writeSplit(second, list, DEFAULT_SPLIT);
  assert.deepEqual(readSplit(second, list), DEFAULT_SPLIT);
});

test('a saved value is validated: out of range is clamped, junk is ignored, a fold only where it can fold', () => {
  assert.deepEqual(parseSplit(list, '{"size":9999,"collapsed":false}'), { size: 500, collapsed: false });
  assert.deepEqual(parseSplit(list, '{"size":"wide"}'), { size: null, collapsed: false });
  assert.equal(parseSplit(list, 'not json'), null);
  assert.equal(parseSplit(list, '[]'), null);
  assert.deepEqual(parseSplit(tray, '{"size":300,"collapsed":true}'), { size: 300, collapsed: false });
  assert.equal(parseSplit(list, null), null);
});

test('every divider leaves its content a readable width and has a default inside its limits', () => {
  for (const [id, spec] of Object.entries(SPLITS) as [string, SplitSpec][]) {
    assert.ok(spec.min >= 60 && spec.min < spec.max, `${id}: min below max`);
    assert.ok(spec.keep >= 120, `${id}: the content keeps a readable size`);
    assert.match(spec.name, /^[a-z][a-z0-9-]*$/, `${id}: a CSS variable name`);
  }
  assert.equal(new Set(Object.values(SPLITS).map(spec => spec.name)).size, Object.keys(SPLITS).length, 'one name per divider');
});
