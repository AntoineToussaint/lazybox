// The dock from the keyboard, the pure part (dock-keyboard.ts): group
// rectangles, neighbour finding, resize math and acceleration, move / swap /
// split, tab slots; and the keymap side (fixed reset chord, platform keys,
// keys the focused editor keeps). The UI is covered by test-layout-keyboard.mjs.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { groupRects, movePanel, neighbourIn, resizeEdge, resizeStep, shareOf, swapGroups, tabAt, type Rect } from '../packages/surfaces/src/dock-keyboard.ts';
import { actionDefinition, defaultKeys, isChordOn } from '../packages/surfaces/src/actions.ts';
import { chordBindings, keyboardPreset, migrateKeymap, ownedByFocus, validateKeymap, withFixedKeys } from '../packages/surfaces/src/keyboard.ts';
import { groups, type Group, type LayoutNode, type Panel, type Split, type WorkspacePage } from '../packages/surfaces/src/workspace-model.ts';

const panel = (id: string, kind = id): Panel => ({ id, kind, binding: { mode: 'follow' }, detached: false });
const group = (id: string, ...members: Panel[]): Group => ({ type: 'group', id, panels: members, active: members[0].id });
const row = (sizes: number[], ...children: LayoutNode[]): Split => ({ type: 'split', direction: 'row', sizes, children });
const column = (sizes: number[], ...children: LayoutNode[]): Split => ({ type: 'split', direction: 'column', sizes, children });
const sidebar = () => group('side', panel('col', 'collections'), panel('agents', 'agents'));
const page = (layout: LayoutNode | null): WorkspacePage => ({ id: 'p', name: 'Work', selection: { collectionId: 'w', sessionId: 's', commandId: '' }, layout, floating: [], popouts: [], detached: [] });
const geometry = { width: 1280, height: 800, sidebarWidth: 280, header: 32 };
// [sidebar | chat over (minimized) output | changes over editor]
const sample = () => row([0.2, 0.4, 0.4], sidebar(), column([0.62, 0.38], group('chat', panel('chat')), { ...group('out', panel('out', 'console')), minimized: true }), column([0.5, 0.5], group('changes', panel('changes')), group('editor', panel('editor'))));

test('group rectangles are what dockview draws: the sidebar at its width, a minimized group its tab bar', () => {
  const rects = groupRects(sample(), geometry);
  assert.deepEqual(rects.get('side'), { x: 0, y: 0, width: 280, height: 800 });
  assert.equal(rects.get('chat')!.x, 280);
  assert.equal(rects.get('out')!.height, 32, 'a minimized group is its tab bar');
  assert.equal(rects.get('chat')!.height + rects.get('out')!.height, 800);
  assert.equal(rects.get('changes')!.x, 780, 'the rest is shared by the flexible shares');
  assert.equal(rects.get('changes')!.width + rects.get('chat')!.width + 280, 1280);
  // A sidebar alone keeps its width beside the empty content area.
  assert.deepEqual(groupRects(sidebar(), geometry).get('side'), { x: 0, y: 0, width: 280, height: 800 });
  assert.equal(groupRects(null, geometry).size, 0);
});

test('the neighbour is the nearest group across that edge, in line with the middle, with no wrapping', () => {
  const rects = groupRects(sample(), geometry);
  assert.equal(neighbourIn(rects, 'chat', 'left'), 'side');
  assert.equal(neighbourIn(rects, 'chat', 'down'), 'out');
  assert.equal(neighbourIn(rects, 'out', 'up'), 'chat');
  assert.equal(neighbourIn(rects, 'side', 'right'), 'chat', 'the group in line with the middle of the sidebar (chat, 0–496) wins over the output');
  assert.equal(neighbourIn(rects, 'chat', 'right'), 'changes', 'beside the chat (0–496): Changes (0–400) shares most');
  assert.equal(neighbourIn(rects, 'out', 'right'), 'editor');
  assert.equal(neighbourIn(rects, 'side', 'left'), undefined, 'no wrapping at the edge');
  assert.equal(neighbourIn(rects, 'editor', 'down'), undefined);
  assert.equal(neighbourIn(rects, 'chat', 'up'), undefined);
  // Coming back: the group the last move came from wins a tie.
  const tall: Map<string, Rect> = new Map([['a', { x: 0, y: 0, width: 100, height: 200 }], ['b', { x: 100, y: 0, width: 100, height: 100 }], ['c', { x: 100, y: 100, width: 100, height: 100 }]]);
  assert.equal(neighbourIn(tall, 'c', 'left'), 'a');
  assert.equal(neighbourIn(tall, 'a', 'right'), 'b', 'equal overlap: the one on the middle line, then the first');
  assert.equal(neighbourIn(tall, 'a', 'right', 'c'), 'c', 'the group the person came from');
  assert.equal(neighbourIn(tall, 'missing', 'right'), undefined);
});

test('one tap moves an edge 5 %; held down it starts at 1 % a step and speeds up to 3 %', () => {
  assert.equal(resizeStep(false), 0.05);
  assert.equal(resizeStep(false, 30), 0.05, 'taps never accelerate');
  assert.equal(resizeStep(true, 0), 0.01);
  assert.ok(resizeStep(true, 10) > resizeStep(true, 1));
  assert.equal(resizeStep(true, 100), 0.03);
});

test('resizing moves the nearest edge on the arrow side (it grows), else the opposite one (it shrinks), tmux style', () => {
  const layout = row([0.2, 0.4, 0.4], sidebar(), group('chat', panel('chat')), column([0.5, 0.5], group('changes', panel('changes')), group('editor', panel('editor'))));
  const grown = resizeEdge(layout, 'chat', 'right', 0.05);
  assert.equal(grown.kind, 'layout');
  if (grown.kind !== 'layout') return;
  const sizes = (grown.layout as Split).sizes;
  assert.ok(Math.abs(sizes[1] - 0.44) < 1e-9 && Math.abs(sizes[2] - 0.36) < 1e-9, `chat grows by 5 % of the flexible width, its neighbour gives it: ${sizes}`);
  assert.equal(sizes[0], 0.2, 'the sidebar share is untouched');
  assert.equal(grown.axis, 'width');
  assert.ok(Math.abs(grown.share - 0.55) < 1e-9, 'the share announced is of what the sidebar leaves');
  // The right column has no right edge: its left edge moves right, so it shrinks.
  const shrunk = resizeEdge(layout, 'editor', 'right', 0.05);
  assert.equal(shrunk.kind, 'layout');
  if (shrunk.kind === 'layout') assert.ok(Math.abs((shrunk.layout as Split).sizes[2] - 0.36) < 1e-9);
  // Up and down move the edge inside the column.
  const up = resizeEdge(layout, 'editor', 'up', 0.05);
  assert.equal(up.kind, 'layout');
  if (up.kind === 'layout') { const inner = (up.layout as Split).children[2] as Split; assert.ok(Math.abs(inner.sizes[0] - 0.45) < 1e-9 && Math.abs(inner.sizes[1] - 0.55) < 1e-9); }
  // The chat fills its column: nothing moves up or down.
  assert.deepEqual(resizeEdge(layout, 'chat', 'down', 0.05), { kind: 'none' });
  assert.equal(shareOf(layout, 'chat', 'width'), 0.5);
});

test('the sidebar\'s edge changes its one pixel width, never its share; a minimized group\'s edge does not move', () => {
  const layout = sample();
  assert.deepEqual(resizeEdge(layout, 'side', 'right', 0.05), { kind: 'sidebar', sign: 1 }, 'the sidebar widens');
  assert.deepEqual(resizeEdge(layout, 'side', 'left', 0.05), { kind: 'sidebar', sign: -1 }, 'with no left edge, its right edge moves left: it narrows');
  assert.deepEqual(resizeEdge(layout, 'chat', 'left', 0.05), { kind: 'sidebar', sign: -1 }, 'the chat\'s left edge is the sidebar\'s');
  // A sidebar on the right.
  const right = row([0.8, 0.2], group('chat', panel('chat')), sidebar());
  assert.deepEqual(resizeEdge(right, 'chat', 'right', 0.05), { kind: 'sidebar', sign: -1 }, 'the chat grows into a right-hand sidebar: it narrows');
  // The output is minimized to its tab bar: the chat cannot push into it.
  assert.deepEqual(resizeEdge(layout, 'chat', 'down', 0.05), { kind: 'none' });
});

test('an edge stops before a side gets thinner than 5 %', () => {
  let layout: LayoutNode = row([0.5, 0.5], group('a', panel('a')), group('b', panel('b')));
  for (let i = 0; i < 40; i++) { const next = resizeEdge(layout, 'a', 'right', 0.05); if (next.kind !== 'layout') break; layout = next.layout; }
  assert.ok(Math.abs((layout as Split).sizes[1] - 0.05) < 1e-9, String((layout as Split).sizes));
  assert.deepEqual(resizeEdge(layout, 'a', 'right', 0.05), { kind: 'none' }, 'at the limit nothing moves');
  assert.deepEqual(resizeEdge(group('a', panel('a')), 'a', 'right', 0.05), { kind: 'none' }, 'a lone group fills the page');
});

test('moving a panel: into the neighbour with tabs beside it, a swap when alone, a split at the edge; never the sidebar', () => {
  const tabs = row([0.2, 0.4, 0.4], sidebar(), group('left', panel('chat'), panel('changes')), group('right', panel('editor')));
  const joined = movePanel(page(tabs), 'changes', 'right', 'right');
  assert.equal(joined?.how, 'joined');
  const right = groups(joined!.page.layout).find(value => value.id === 'right')!;
  assert.deepEqual(right.panels.map(value => value.id), ['editor', 'changes']);
  assert.equal(right.active, 'changes', 'it arrives in front');
  // Alone in its group: the two groups trade places, sizes stay where they are.
  const swapped = movePanel(page(tabs), 'editor', 'left', 'left');
  assert.equal(swapped?.how, 'swapped');
  const order = (swapped!.page.layout as Split).children.map(child => (child as Group).id);
  assert.deepEqual(order, ['side', 'right', 'left']);
  assert.deepEqual((swapped!.page.layout as Split).sizes, [0.2, 0.4, 0.4]);
  // No group above: a tab splits out to that side of its own group.
  const split = movePanel(page(tabs), 'changes', 'up', undefined);
  assert.equal(split?.how, 'split');
  assert.equal(groups(split!.page.layout).length, 4);
  // Alone and at the edge: nothing to do.
  assert.equal(movePanel(page(tabs), 'editor', 'right', undefined), undefined);
  // The sidebar is never a source, nor a target (a tab beside it splits out instead).
  assert.equal(movePanel(page(tabs), 'agents', 'right', 'left'), undefined);
  assert.equal(movePanel(page(tabs), 'chat', 'left', 'side')?.how, 'split');
  assert.equal(movePanel(page(row([0.2, 0.8], sidebar(), group('only', panel('chat')))), 'chat', 'left', 'side'), undefined);
  // A floating group moves with the mouse or its menu.
  const floating = { ...page(tabs), floating: [{ group: group('float', panel('web1', 'web'), panel('web2', 'web')), box: { left: 0, top: 0, width: 200, height: 200 } }] };
  assert.equal(movePanel(floating, 'web1', 'left', undefined), undefined);
});

test('swapping opens a minimized group (it would otherwise travel as a sliver)', () => {
  const layout = column([0.6, 0.4], group('chat', panel('chat')), { ...group('out', panel('out', 'console')), minimized: true });
  const swapped = swapGroups(layout, 'chat', 'out') as Split;
  assert.deepEqual(swapped.children.map(child => (child as Group).id), ['out', 'chat']);
  assert.ok(swapped.children.every(child => !(child as Group).minimized));
  assert.equal(swapGroups(layout, 'chat', 'missing'), layout);
});

test('tab slots: 1–8 by place, 9 the last, nothing past the end', () => {
  assert.equal(tabAt(3, 1), 0);
  assert.equal(tabAt(3, 3), 2);
  assert.equal(tabAt(3, 4), undefined);
  assert.equal(tabAt(3, 9), 2, '9 is the last tab, as in a browser');
  assert.equal(tabAt(12, 9), 11);
  assert.equal(tabAt(0, 1), undefined);
});

test('keys: ⌃1–9 for tabs on a Mac (⌘1–9 stay workspaces), Alt+1–9 elsewhere; every preset stays conflict-free', () => {
  const mac = keyboardPreset('vim', true), pc = keyboardPreset('vim', false);
  assert.equal(mac['layout.tab.2'], 'Ctrl+2');
  assert.equal(mac['workspace.select.2'], 'Meta+2');
  assert.equal(pc['layout.tab.2'], 'Alt+2');
  assert.equal(pc['workspace.select.2'], 'Ctrl+2');
  assert.equal(mac['layout.focus-left'], 'Meta+Alt+ArrowLeft');
  assert.equal(pc['layout.focus-left'], 'Ctrl+Alt+ArrowLeft');
  assert.equal(mac['layout.resize-up'], 'Ctrl+Meta+ArrowUp');
  assert.equal(pc['layout.resize-up'], 'Alt+Shift+ArrowUp');
  assert.equal(mac['layout.move-down'], 'Meta+Alt+Shift+ArrowDown');
  for (const preset of ['vim', 'tmux', 'arrows']) for (const platform of [true, false]) assert.equal(validateKeymap(keyboardPreset(preset, platform)), '', `${preset} mac=${platform}`);
  // Off a Mac, Alt with a digit or an arrow is a chord (it types nothing there); on a Mac ⌥1 types ¡.
  assert.equal(isChordOn('Alt+2', false), true);
  assert.equal(isChordOn('Alt+Shift+ArrowUp', false), true);
  assert.equal(isChordOn('Alt+2', true), false);
  assert.equal(isChordOn('Alt+Space', false), false, 'push-to-talk stays a held key');
  assert.equal(isChordOn('Alt+x', false), false);
  assert.equal(defaultKeys(actionDefinition('layout.tab.9')!, false), 'Alt+9');
  assert.ok(actionDefinition('layout.resize-left')?.repeat, 'resizing repeats while held');
});

test('the reset chord cannot be rebound, nor taken by another action', () => {
  const mac = keyboardPreset('vim', true);
  assert.equal(mac['layout.reset-all'], 'Ctrl+Alt+Shift+Escape');
  const tampered = withFixedKeys({ ...mac, 'layout.reset-all': '', 'inbox.open': '] ] i or Ctrl+Alt+Shift+Escape' }, true);
  assert.equal(tampered['layout.reset-all'], 'Ctrl+Alt+Shift+Escape', 'always its default');
  assert.equal(tampered['inbox.open'], '] ] i', 'another action loses it');
  // A saved keymap from any version comes back with it.
  assert.equal(migrateKeymap({ ...mac, 'layout.reset-all': 'Meta+r', __version: 6 }, [], true)['layout.reset-all'], 'Ctrl+Alt+Shift+Escape');
  assert.equal(chordBindings(mac).get('Ctrl+Alt+Shift+Escape'), 'layout.reset-all');
});

test('a keymap saved before the panel-group keys gains them, unless the person uses those keys', () => {
  const saved = Object.fromEntries(Object.entries(keyboardPreset('vim', true)).filter(([id]) => !id.startsWith('layout.focus') && !id.startsWith('layout.tab') && id !== 'layout.add' && id !== 'layout.reset-all'));
  const map = migrateKeymap({ ...saved, 'snippets.open': '] ] s or Ctrl+3', __version: 5 }, [], true);
  assert.equal(map['layout.focus-right'], 'Meta+Alt+ArrowRight');
  assert.equal(map['layout.tab.1'], 'Ctrl+1');
  assert.equal(map['layout.tab.3'], '', 'a key the person gave another action stays theirs');
  assert.equal(map['layout.add'], 'Meta+t');
  assert.equal(validateKeymap(map), '');
  assert.equal(migrateKeymap({ ...saved, 'files.find': '] ] f or Meta+t', __version: 5 }, [], true)['layout.add'], '', 'a chord the person gave another action stays theirs');
});

test('the focused editor keeps its own chords: tabs with ⌥⌘← / ⌥⌘→, cursors with ⌥⌘↑ / ⌥⌘↓', () => {
  const inside = (...selectors: string[]) => ({ closest: (selector: string) => selectors.includes(selector) ? {} as Element : null });
  assert.equal(ownedByFocus('Meta+Alt+ArrowLeft', inside('.editor-area')), true);
  assert.equal(ownedByFocus('Meta+Alt+Shift+ArrowRight', inside('.editor-area')), true);
  assert.equal(ownedByFocus('Meta+Alt+ArrowUp', inside('.editor-area')), false, 'up and down are the layout\'s outside the code itself');
  assert.equal(ownedByFocus('Meta+Alt+ArrowUp', inside('.editor-area', '.monaco-editor')), true);
  assert.equal(ownedByFocus('Alt+Shift+ArrowDown', inside('.monaco-editor')), true, 'copy line, off a Mac');
  assert.equal(ownedByFocus('Ctrl+Meta+ArrowLeft', inside('.editor-area')), false, 'resizing is the layout\'s everywhere');
  assert.equal(ownedByFocus('Meta+Alt+ArrowLeft', inside()), false);
  assert.equal(ownedByFocus('Meta+Alt+ArrowLeft', null), false);
});
