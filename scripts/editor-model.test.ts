import { test } from 'node:test';
import assert from 'node:assert/strict';
import { activateTab, adjacentTab, closeGroup, closePath, closeTab, editorGroups, emptyEditorLayout, focusedGroup, maxGroups, maxTabs, moveTab, moveTabToGroup, openPaths, openTab, parseEditorLayout, parseQuery, pinTab, resizeSplit, shiftTab, splitGroup, tabHints, visiblePaths } from '../packages/surfaces/src/editor-model.ts';
import { defaultLayout, panels, showCodePanel, type LayoutNode } from '../packages/surfaces/src/workspace-model.ts';

const tabs = (layout: ReturnType<typeof emptyEditorLayout>, index = 0) => editorGroups(layout.root)[index].tabs;

test('opening inserts after the shown tab, activates, remembers, and never duplicates', () => {
 let layout = openTab(emptyEditorLayout(), 'api/a.ts');
 layout = openTab(layout, 'api/b.ts');
 layout = activateTab(layout, 'main', 'api/a.ts');
 layout = openTab(layout, 'api/c.ts');
 assert.deepEqual(tabs(layout), ['api/a.ts', 'api/c.ts', 'api/b.ts']);
 layout = openTab(layout, 'api/b.ts');
 assert.deepEqual(tabs(layout), ['api/a.ts', 'api/c.ts', 'api/b.ts']);
 assert.equal(focusedGroup(layout).active, 'api/b.ts');
 assert.deepEqual(layout.recent.slice(0, 3), ['api/b.ts', 'api/c.ts', 'api/a.ts']);
});

test('a preview tab is replaced by the next preview, kept by a pinned open, an edit or a pin', () => {
 let layout = openTab(emptyEditorLayout(), 'a.ts');
 layout = openTab(layout, 'b.ts', { preview: true });
 layout = openTab(layout, 'c.ts', { preview: true });
 assert.deepEqual(tabs(layout), ['a.ts', 'c.ts']);
 assert.equal(focusedGroup(layout).preview, 'c.ts');
 // Unsaved previews are never replaced.
 const kept = openTab(layout, 'd.ts', { preview: true, keep: path => path === 'c.ts' });
 assert.deepEqual(tabs(kept), ['a.ts', 'c.ts', 'd.ts']);
 layout = openTab(layout, 'c.ts');
 assert.equal(focusedGroup(layout).preview, undefined);
 layout = openTab(layout, 'e.ts', { preview: true });
 layout = pinTab(layout, 'main', 'e.ts');
 layout = openTab(layout, 'f.ts', { preview: true });
 assert.deepEqual(tabs(layout), ['a.ts', 'c.ts', 'e.ts', 'f.ts']);
});

test('the tab limit drops the oldest saved tabs, never the shown or unsaved ones', () => {
 let layout = emptyEditorLayout();
 for (let i = 0; i < maxTabs; i++) layout = openTab(layout, `f${i}.ts`);
 layout = openTab(layout, 'new.ts', { keep: path => path === 'f0.ts' });
 assert.equal(tabs(layout).length, maxTabs);
 assert.deepEqual([tabs(layout).includes('f0.ts'), tabs(layout).includes('f1.ts'), tabs(layout).includes('new.ts')], [true, false, true]);
});

test('closing shows the right-hand neighbour; an emptied group closes unless it is the last', () => {
 let layout = ['a', 'b', 'c'].reduce((value, path) => openTab(value, path), emptyEditorLayout());
 layout = activateTab(layout, 'main', 'b');
 layout = closeTab(layout, 'main', 'b');
 assert.equal(focusedGroup(layout).active, 'c');
 layout = splitGroup(layout, 'main', 'row', 'g2');
 assert.equal(editorGroups(layout.root).length, 2);
 assert.deepEqual(tabs(layout, 1), ['c']);
 assert.equal(layout.focused, 'g2');
 layout = closeTab(layout, 'g2', 'c');
 assert.equal(editorGroups(layout.root).length, 1);
 assert.equal(layout.focused, 'main');
 layout = closeTab(closeTab(layout, 'main', 'a'), 'main', 'c');
 assert.deepEqual(editorGroups(layout.root).map(group => group.tabs), [[]]);
 assert.equal(closeGroup(layout, 'main').root.type, 'group');
});

test('splits go right or down, keep each group’s tabs, are bounded and resizable', () => {
 let layout = openTab(emptyEditorLayout(), 'a.ts');
 layout = splitGroup(layout, 'main', 'row', 'g2', 'b.ts');
 layout = splitGroup(layout, 'g2', 'column', 'g3');
 assert.equal(layout.root.type, 'split');
 assert.deepEqual(editorGroups(layout.root).map(group => group.tabs), [['a.ts'], ['b.ts'], ['b.ts']]);
 layout = splitGroup(layout, 'g3', 'row', 'g4');
 assert.equal(editorGroups(layout.root).length, maxGroups);
 assert.equal(splitGroup(layout, 'g4', 'row', 'g5'), layout);
 assert.deepEqual(visiblePaths(layout), new Set(['a.ts', 'b.ts']));
 const resized = resizeSplit(layout, [], 0.99);
 assert.equal(resized.root.type === 'split' && resized.root.ratio, 0.85);
 assert.equal(closePath(layout, 'b.ts').root.type, 'group');
});

test('tabs reorder within a group and move between groups', () => {
 let layout = ['a', 'b', 'c'].reduce((value, path) => openTab(value, path), emptyEditorLayout());
 layout = moveTab(layout, 'main', 'c', 0);
 assert.deepEqual(tabs(layout), ['c', 'a', 'b']);
 layout = shiftTab(layout, 'main', 'c', 1);
 assert.deepEqual(tabs(layout), ['a', 'c', 'b']);
 assert.equal(adjacentTab(layout, 'main', 1), 'b');
 assert.equal(adjacentTab(layout, 'main', 2), 'a');
 layout = splitGroup(layout, 'main', 'row', 'g2', 'z');
 layout = moveTabToGroup(layout, 'main', 'a', 'g2', 0);
 assert.deepEqual(tabs(layout), ['c', 'b']);
 assert.deepEqual(tabs(layout, 1), ['a', 'z']);
 assert.equal(layout.focused, 'g2');
 assert.deepEqual(openPaths(layout), new Set(['a', 'b', 'c', 'z']));
});

test('saved layouts round-trip; damaged ones are rejected whole', () => {
 let layout = openTab(emptyEditorLayout(), 'api/a.ts', { preview: true });
 layout = splitGroup(layout, 'main', 'column', 'g2', 'web/b.ts');
 assert.deepEqual(parseEditorLayout(JSON.stringify(layout)), layout);
 for (const damage of [
  (value: any) => { value.version = 2; },
  (value: any) => { value.focused = 'nowhere'; },
  (value: any) => { value.root.children[1].group.id = 'main'; },
  (value: any) => { value.root.children[0].group.active = 'not-a-tab'; },
  (value: any) => { value.root.ratio = 2; },
  (value: any) => { value.root.children[0].group.tabs.push('api/a.ts'); },
  (value: any) => { value.root.children[0].group.tabs.push('bad\0path'); },
 ]) {
  const copy = structuredClone(layout);
  damage(copy);
  assert.throws(() => parseEditorLayout(JSON.stringify(copy)));
 }
});

test('same-named tabs get the shortest distinguishing folder', () => {
 assert.deepEqual(tabHints(['api/README.md', 'web/README.md', 'api/src/main.go']), { 'api/README.md': 'api', 'web/README.md': 'web' });
 assert.deepEqual(tabHints(['a/x/index.ts', 'b/x/index.ts']), { 'a/x/index.ts': 'a/x', 'b/x/index.ts': 'b/x' });
 assert.deepEqual(tabHints(['README.md', 'api/README.md']), { 'README.md': '/', 'api/README.md': 'api' });
});

test('go to file reads name:line', () => {
 assert.deepEqual(parseQuery('routes.ts:42'), { term: 'routes.ts', line: 42 });
 assert.deepEqual(parseQuery(' main.go:3:7 '), { term: 'main.go', line: 3 });
 assert.deepEqual(parseQuery('src/app'), { term: 'src/app', line: undefined });
});

test('files and the editor pair up beside the conversation', () => {
 const page = defaultLayout({ collectionId: 'alpha', sessionId: 's', commandId: '' }).pages[0];
 let n = 0;
 const id = () => `p${++n}`;
 const editor = showCodePanel(page, 'editor', 'right', id);
 const files = showCodePanel(editor.page, 'files', 'right', id);
 const kinds = (node: LayoutNode | null) => panels(node).map(panel => panel.kind);
 assert.deepEqual(kinds(files.page.layout), ['collections', 'agents', 'chat', 'console', 'files', 'editor']);
 // Files sits just left of the editor, in the same row.
 const row = (node: LayoutNode | null): boolean => !!node && node.type === 'split' && (node.direction === 'row' && node.children.some((child, index) => child.type === 'group' && child.panels[0].kind === 'files' && node.children[index + 1]?.type === 'group' && (node.children[index + 1] as Extract<LayoutNode, { type: 'group' }>).panels[0].kind === 'editor') || node.children.some(row));
 assert.ok(row(files.page.layout));
 // Showing one that is already there only brings it forward.
 assert.equal(showCodePanel(files.page, 'files', 'right', id).added, false);
});
