import { test } from 'node:test';
import assert from 'node:assert/strict';
import { PREVIEWABLE, fitWithin, formatBytes, isLocalImage, pastedFiles, placePreview } from '../packages/surfaces/src/attachment-images.ts';

const viewport = { width: 1280, height: 800 };
const rect = (left: number, top: number, size = 56) => ({ left, top, right: left + size, bottom: top + size });

test('an image fits a box at its own shape and is never enlarged', () => {
  assert.deepEqual(fitWithin(1600, 900, { width: 754, height: 440 }), { width: 754, height: 424 });
  assert.deepEqual(fitWithin(300, 600, { width: 754, height: 440 }), { width: 220, height: 440 });
  assert.deepEqual(fitWithin(40, 20, { width: 754, height: 440 }), { width: 40, height: 20 });
  assert.deepEqual(fitWithin(0, 0, { width: 754, height: 440 }), { width: 0, height: 0 });
});

test('the preview goes above a composer thumbnail, inside 60% of the window', () => {
  const at = placePreview(rect(320, 560), { width: 1600, height: 900 }, viewport);
  assert.equal(at.side, 'above');
  assert.ok(at.width + 14 <= viewport.width * 0.6 + 0.5 && at.height + 40 <= viewport.height * 0.6 + 0.5);
  assert.ok(Math.abs(at.width / at.height - 16 / 9) < 0.01);
  assert.ok(at.top + at.height + 40 <= 560, 'it ends above the thumbnail');
  assert.ok(at.left >= 8 && at.left + at.width + 14 <= viewport.width - 8, 'inside the window');
});

test('without room above or below it goes beside the thumbnail, never over it', () => {
  const thumb = rect(330, 340, 88);
  const at = placePreview(thumb, { width: 1600, height: 900 }, viewport);
  assert.equal(at.side, 'right');
  assert.ok(at.left >= thumb.right, 'right of the thumbnail');
  const nearRight = placePreview(rect(1100, 340, 88), { width: 1600, height: 900 }, viewport);
  assert.equal(nearRight.side, 'left');
  assert.ok(nearRight.left + nearRight.width + 14 <= 1100);
});

test('where it fits nowhere whole, it takes the side with most room, smaller', () => {
  const small = { width: 400, height: 300 };
  const at = placePreview(rect(150, 120, 88), { width: 1600, height: 900 }, small);
  assert.ok(at.width > 0 && at.height > 0);
  assert.ok(at.left >= 8 && at.top >= 8 && at.left + Math.max(180, at.width + 14) <= small.width - 8 + 0.5);
});

test('a tiny image is shown at its size in a card wide enough for its name', () => {
  const at = placePreview(rect(320, 560), { width: 16, height: 16 }, viewport);
  assert.deepEqual([at.width, at.height], [16, 16]);
});

test('sizes read the way a person says them', () => {
  assert.equal(formatBytes(5), '5 B');
  assert.equal(formatBytes(2048), '2 KiB');
  assert.equal(formatBytes(1153433), '1.1 MiB');
});

test('which images are shown', () => {
  for (const type of ['image/png', 'image/jpeg', 'image/gif', 'image/webp']) assert.ok(PREVIEWABLE.test(type), type);
  for (const type of ['image/svg+xml', 'text/plain', '', 'application/pdf']) assert.ok(!PREVIEWABLE.test(type), type);
  assert.ok(isLocalImage({ type: 'image/avif' }));
  assert.ok(!isLocalImage({ type: 'image/svg+xml' }) && !isLocalImage({ type: 'text/plain' }));
});

const clipboard = (files: File[], text = '') => ({ files: files as unknown as FileList, getData: (type: string) => type === 'text/plain' ? text : '' });
const at = new Date(2026, 10, 16, 9, 5, 7);

test('a pasted screenshot gets a readable name; a text paste attaches nothing', () => {
  const [named] = pastedFiles(clipboard([new File(['x'], 'image.png', { type: 'image/png' })]), at);
  assert.equal(named.name, 'Pasted image 2026-11-16 at 09.05.07.png');
  assert.equal(named.type, 'image/png');
  const two = pastedFiles(clipboard([new File(['a'], 'image.png', { type: 'image/png' }), new File(['b'], 'image.png', { type: 'image/png' })]), at);
  assert.deepEqual(two.map(file => file.name), ['Pasted image 2026-11-16 at 09.05.07.png', 'Pasted image 2026-11-16 at 09.05.07 (2).png']);
  assert.deepEqual(pastedFiles(clipboard([], 'just text'), at), []);
});

test('a file copied in Finder attaches under its own name; a spreadsheet copy stays text', () => {
  const report = new File(['%PDF'], 'report.pdf', { type: 'application/pdf' });
  assert.deepEqual(pastedFiles(clipboard([report], 'report.pdf'), at).map(file => file.name), ['report.pdf']);
  // A spreadsheet puts a picture of the cells beside their text: the text wins.
  assert.deepEqual(pastedFiles(clipboard([new File(['x'], 'image.png', { type: 'image/png' })], 'a\tb\n1\t2'), at), []);
});
