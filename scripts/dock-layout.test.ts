// The dock's pure parts: the bridge to dockview's serialized form, presets,
// saved layouts and the layout command seam. The UI is covered by test-docking.mjs.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { childPixels, EMPTY_CONTENT, fromDockview, needsEmptyContent, sameSizes, structureKey, toDockview } from '../packages/surfaces/src/dock-bridge.ts';
import { applyTemplate, builtinPresets, parseSavedLayouts, sameTemplate, templateKinds, templateOf } from '../packages/surfaces/src/layout-presets.ts';
import { layoutCommands, provideLayoutCommands, runLayoutCommand } from '../packages/surfaces/src/layout-commands.ts';
import { defaultLayout, fitWidth, groups, insertDocked, minimumWidth, minWidthOf, panels, parseLayout, shownPanels, type Group, type LayoutNode, type Panel, type WorkspacePage } from '../packages/surfaces/src/workspace-model.ts';

const geometry = { width: 1400, height: 800, sidebarWidth: 280, header: 32, title: (panel: Panel) => panel.kind };
const work = () => defaultLayout({ collectionId: 'w', sessionId: 's', commandId: '' }).pages[0];
const overlayOf = (page: WorkspacePage) => {
  const minimized = new Map<string, number>();
  const visit = (node: LayoutNode | null) => { if (node?.type !== 'split') return; node.children.forEach((child, index) => { if (child.type === 'group' && child.minimized && node.direction === 'column') minimized.set(child.id, node.sizes[index]); visit(child); }); };
  visit(page.layout);
  return { minimized };
};
let n = 0; const id = () => `id-${++n}`;

test('a page goes to dockview and back unchanged: groups, tabs, front tab, shares, minimized output', () => {
  let page = work();
  page = { ...page, layout: insertDocked(page.layout, { id: 'ed', kind: 'editor', binding: { mode: 'follow' }, detached: false }, 'right') };
  const json = toDockview(page, geometry);
  assert.equal(json.grid.orientation, 'HORIZONTAL');
  assert.equal(json.grid.root.size, 800, 'the root row is as tall as the page');
  assert.deepEqual(Object.values(json.panels).map(panel => [panel.id, panel.contentComponent, (panel.params as { kind: string }).kind]).sort(), panels(page.layout).map(panel => [panel.id, 'panel', panel.kind]).sort());
  const back = fromDockview(json, overlayOf(page));
  assert.equal(structureKey(back), structureKey(page));
  assert.ok(sameSizes(back.layout, page.layout, 0.01), JSON.stringify([back.layout, page.layout]));
  assert.deepEqual(groups(back.layout).map(group => [group.active, !!group.minimized]), groups(page.layout).map(group => [group.active, !!group.minimized]));
});

test('a page whose docked panels are the sidebar alone keeps the sidebar at its width beside an empty content area, never saved', () => {
  const sidebar = groups(work().layout)[0];
  const page = { ...work(), layout: sidebar, floating: [{ group: groups(work().layout)[1], box: { left: 400, top: 40, width: 500, height: 400 } }] };
  assert.equal(needsEmptyContent(page), true);
  assert.equal(needsEmptyContent(work()), false);
  const json = toDockview(page, geometry);
  const root = json.grid.root as { data: { size: number; data: { id: string; views: string[] } }[] };
  assert.deepEqual(root.data.map(leaf => leaf.size), [280, 1120], 'the sidebar at its width, the empty area the rest');
  assert.deepEqual(root.data[1].data.views, [EMPTY_CONTENT]);
  // On a narrow window the sidebar still leaves the empty area some room.
  const narrow = toDockview(page, { ...geometry, width: 300 }).grid.root as { data: { size: number }[] };
  assert.ok(narrow.data[1].size >= 48);
  const back = fromDockview(json, overlayOf(page));
  assert.equal(structureKey(back), structureKey(page), 'the empty area is not part of the page');
  assert.equal(JSON.stringify(back).includes(EMPTY_CONTENT), false);
  // Workspaces stacked over Agents, alone: the column at the sidebar's width, both groups in it.
  const stacked = { ...page, layout: (work().layout as Extract<LayoutNode, { type: 'split' }>).children[0] };
  assert.equal(needsEmptyContent(stacked), true);
  const column = toDockview(stacked, geometry).grid.root as { data: { type: string; size: number; data: unknown }[] };
  assert.deepEqual(column.data.map(node => [node.type, node.size]), [['branch', 280], ['leaf', 1120]]);
  const both = fromDockview(toDockview(stacked, geometry), overlayOf(stacked));
  assert.equal(structureKey(both), structureKey(stacked));
});

test('the sidebar takes its fixed width and a minimized group its tab bar, whatever their share', () => {
  const page = work(), json = toDockview(page, geometry);
  const root = json.grid.root as { data: { size: number; data: unknown }[] };
  assert.equal(root.data[0].size, 280, 'sidebar width in pixels');
  assert.equal(root.data[1].size, 1120, 'the content takes the rest');
  const column = root.data[1].data as { size: number }[];
  assert.deepEqual(column.map(child => child.size), [768, 32], 'the minimized output is its tab bar; the chat takes the rest');
  // A wider window: the sidebar stays, everything else scales.
  const wide = toDockview(page, { ...geometry, width: 2000 }).grid.root as { data: { size: number }[] };
  assert.equal(wide.data[0].size, 280); assert.equal(wide.data[1].size, 1720);
  // Shares fill the split exactly, with nothing below a usable minimum.
  const split = { type: 'split' as const, direction: 'row' as const, sizes: [0.333, 0.333, 0.334], children: [page.layout!, page.layout!, page.layout!] };
  assert.equal(childPixels(split, 1001, false, geometry).reduce((sum, value) => sum + value, 0), 1001);
});

test('what dockview changes reads back: a moved tab, a resize, a floating group, a popped-out group, an unknown panel', () => {
  const page = work(), json = toDockview(page, geometry);
  const root = json.grid.root as { data: any[] };
  // Drag: the output joins the chat's group as a tab, in front.
  const column = root.data[1].data as any[];
  column[0].data.views.push(column[1].data.views[0]); column[0].data.activeView = column[1].data.views[0];
  root.data[1] = column[0]; root.data[1].size = 1120;
  // Resize: the sidebar edge dragged.
  root.data[0].size = 360; root.data[1].size = 1040;
  json.floatingGroups = [{ data: { id: 'gf', views: ['fl'], activeView: 'fl' }, position: { left: 40, top: 50, width: 400, height: 300 } }];
  json.popoutGroups = [{ data: { id: 'gp', views: ['po', 'ghost'], activeView: 'po' }, position: null }];
  json.panels.fl = { id: 'fl', params: { kind: 'files', binding: { mode: 'follow' } } };
  json.panels.po = { id: 'po', params: { kind: 'editor', binding: { mode: 'follow' } } };
  json.panels.ghost = { id: 'ghost' };
  const back = fromDockview(json);
  assert.deepEqual(groups(back.layout).map(group => group.panels.map(panel => panel.kind)), [['collections'], ['agents'], ['chat', 'console']]);
  assert.equal((groups(back.layout)[2] as Group).active, 'w-work-console');
  assert.deepEqual(back.floating, [{ group: { type: 'group', id: 'gf', panels: [{ id: 'fl', kind: 'files', binding: { mode: 'follow' }, detached: false }], active: 'fl' }, box: { left: 40, top: 50, width: 400, height: 300 } }]);
  assert.deepEqual(back.popouts.map(group => group.panels.map(panel => panel.id)), [['po']], 'a panel dockview cannot describe is dropped, not invented');
  assert.ok(Math.abs((back.layout as Extract<LayoutNode, { type: 'split' }>).sizes[0] - 360 / 1400) < 0.001);
  assert.doesNotThrow(() => parseLayout(JSON.stringify({ version: 2, selectedPage: 'work', pages: [{ ...page, ...back }] })));
});

test('size comparison ignores the sidebar share and tolerates rounding', () => {
  const page = work(), layout = page.layout as Extract<LayoutNode, { type: 'split' }>;
  assert.ok(sameSizes(layout, { ...layout, sizes: [0.3, 0.7] }), 'the sidebar is pixels, not a share');
  const column = layout.children[1] as Extract<LayoutNode, { type: 'split' }>;
  const resized = { ...layout, children: [layout.children[0], { ...column, sizes: [0.5, 0.5] }] };
  assert.equal(sameSizes(layout, resized), false);
  assert.ok(sameSizes(layout, { ...layout, children: [layout.children[0], { ...column, sizes: [0.625, 0.375] }] }));
});

test('presets: every built-in is valid, and applying one keeps the panels the page has', () => {
  assert.deepEqual(builtinPresets.map(preset => preset.name), ['Chat focus', 'Chat + Changes', 'Chat + Editor + Terminal', 'Review', 'Everything']);
  const page = work(), chat = shownPanels(page).find(panel => panel.kind === 'chat')!;
  for (const preset of builtinPresets) {
    const applied = applyTemplate(page, preset.layout, id);
    assert.doesNotThrow(() => parseLayout(JSON.stringify({ version: 2, selectedPage: 'work', pages: [applied] })), preset.name);
    assert.deepEqual(shownPanels(applied).map(panel => panel.kind).sort(), templateKinds(preset.layout).sort(), preset.name);
    if (templateKinds(preset.layout).includes('chat')) assert.equal(shownPanels(applied).find(panel => panel.kind === 'chat')!.id, chat.id, `${preset.name} keeps the conversation's panel`);
    assert.ok(sameTemplate(templateOf(applied), preset.layout));
  }
  // Chat focus is the default page.
  assert.ok(sameTemplate(templateOf(page), builtinPresets[0].layout));
  // A page in its own window comes back with the layout; chats in native windows stay where they are.
  const away = { ...page, popouts: [{ type: 'group' as const, id: 'gp', panels: [{ id: 'ed', kind: 'editor', binding: { mode: 'follow' as const }, detached: false }], active: 'ed' }], detached: [{ id: 'x', kind: 'chat', binding: { mode: 'pinned' as const, target: { collectionId: 'w', sessionId: 't', commandId: '' } }, detached: true }] };
  const review = applyTemplate(away, builtinPresets.find(preset => preset.id === 'review')!.layout, id);
  assert.equal(review.popouts.length, 0);
  assert.equal(shownPanels(review).find(panel => panel.kind === 'editor')!.id, 'ed', 'the popped-out editor is reused');
  assert.deepEqual(review.detached, away.detached);
});

test('saved layouts are validated: bad entries are dropped, never trusted', () => {
  const good = { id: 'mine', name: 'Mine', description: '', layout: templateOf(work())! };
  assert.deepEqual(parseSavedLayouts(JSON.stringify([good])), [good]);
  for (const bad of [{ ...good, id: '../x' }, { ...good, name: '' }, { ...good, layout: { type: 'group', kinds: ['Bad Kind'] } }, { ...good, layout: { type: 'split', direction: 'row', sizes: [1], children: [{ type: 'group', kinds: ['chat'] }] } }, null, 'x']) {
    assert.deepEqual(parseSavedLayouts(JSON.stringify([bad])), []);
  }
  assert.deepEqual(parseSavedLayouts('not json'), []);
  assert.deepEqual(parseSavedLayouts(null), []);
});

test('layout commands are plain functions a palette can list and run', () => {
  assert.deepEqual(layoutCommands(), []);
  let ran = 0;
  const withdraw = provideLayoutCommands(() => [
    { id: 'layout.maximize', label: 'Maximize', group: 'Layout', enabled: () => true, run: () => { ran++; } },
    { id: 'layout.close', label: 'Close', group: 'Layout', enabled: () => false, run: () => { ran += 10; } },
  ]);
  assert.deepEqual(layoutCommands().map(command => command.id), ['layout.maximize', 'layout.close']);
  assert.equal(runLayoutCommand('layout.maximize'), true);
  assert.equal(runLayoutCommand('layout.close'), false, 'an unavailable command does not run');
  assert.equal(runLayoutCommand('missing'), false);
  assert.equal(ran, 1);
  withdraw();
  assert.deepEqual(layoutCommands(), []);
});

test('panels are never crushed: too narrow, the rightmost side-by-side panels become tabs, the newest in front', () => {
  const follow = (id: string, kind: string): Panel => ({ id, kind, binding: { mode: 'follow' }, detached: false });
  assert.equal(minWidthOf('chat'), 360); assert.equal(minWidthOf('files'), 200); assert.equal(minWidthOf('anything'), 220);
  let layout: LayoutNode | null = work().layout;
  assert.equal(minimumWidth(layout, 280), 280 + 360, 'the sidebar takes its width; stacked panels the widest');
  for (const [id, kind] of [['files', 'files'], ['editor', 'editor'], ['terminal', 'terminal']]) layout = insertDocked(layout, follow(id, kind), 'right');
  assert.equal(minimumWidth(layout, 280), 280 + 360 + 200 + 320 + 280);
  assert.equal(fitWidth(layout, 2000, 280), layout, 'room enough: unchanged');
  const fitted = fitWidth(layout, 1200, 280);
  assert.ok(minimumWidth(fitted, 280) <= 1200);
  const merged = groups(fitted).find(group => group.panels.some(panel => panel.id === 'terminal'))!;
  assert.deepEqual(merged.panels.map(panel => panel.id), ['editor', 'terminal'], 'the editor and terminal share a group');
  assert.equal(merged.active, 'terminal', 'the one that was on the right stays in front');
  assert.deepEqual(panels(fitted).map(panel => panel.id).sort(), panels(layout).map(panel => panel.id).sort(), 'no panel is lost');
  assert.ok(groups(fitted).some(group => group.panels.length === 1 && group.panels[0].kind === 'collections') && groups(fitted).some(group => group.panels.length === 1 && group.panels[0].kind === 'agents'), 'the sidebar is never merged');
  // Narrower still: more tabs, down to the sidebar beside the conversation's column.
  const narrow = fitWidth(layout, 700, 280);
  assert.ok(minimumWidth(narrow, 280) <= 700);
  assert.equal(narrow?.type === 'split' && narrow.children.length, 2);
  assert.equal(panels(narrow).length, panels(layout).length);
  assert.doesNotThrow(() => parseLayout(JSON.stringify({ version: 2, selectedPage: 'work', pages: [{ ...work(), layout: narrow }] })));
});
