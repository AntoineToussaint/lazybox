import test from 'node:test';
import assert from 'node:assert/strict';
import { sizedSvg } from '../packages/surfaces/src/diagram-svg.ts';

test('a rendered Mermaid SVG gets the natural size of its viewBox', () => {
  const sized = sizedSvg('<svg id="d" width="100%" xmlns="http://www.w3.org/2000/svg" style="max-width: 812.5px;" viewBox="-8 -8 812.5 240"><rect stroke-width="2" width="10" height="10"/></svg>')!;
  assert.equal(sized.width, 812.5); assert.equal(sized.height, 240);
  assert.match(sized.svg, /^<svg width="812.5" height="240" id="d"/);
  assert.doesNotMatch(sized.svg, /width="100%"|max-width/);
  assert.match(sized.svg, /<rect stroke-width="2" width="10" height="10"\/>/, 'only the root element is resized');
  assert.ok(sized.src.startsWith('data:image/svg+xml;charset=utf-8,%3Csvg'));
});

test('fixed pixel dimensions stand in for a missing viewBox; no size is no preview', () => {
  const sized = sizedSvg('<svg width="200px" height="50"><g/></svg>')!;
  assert.deepEqual([sized.width, sized.height], [200, 50]);
  assert.match(sized.svg, /viewBox="0 0 200 50"/);
  assert.equal(sizedSvg('<svg width="100%"><g/></svg>'), undefined);
  assert.equal(sizedSvg('not an svg'), undefined);
});
