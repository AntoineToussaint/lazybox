import { create } from '@bufbuild/protobuf';
import { SessionSchema } from '@lazybox/protocol';
import { mergeConversations } from '../packages/surfaces/src/conversation-model.ts';
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { activatePanel, adoptGeometry, openBesideChat, pageShowing, PANEL_LIMIT, groupOf, allPanels, clonePage, dedupeChats, defaultDock, defaultLayout, detachChat, detachedChats, detachedFor, largestGroup, returnDetached, dockPanel, dockedSide, emptyPage, emptyTarget, groups, insertDocked, mapGroups, mapPagePanels, mapPanels, normalize, panels, parseLayout, redockPopouts, removeFromPage, removePanel, repairLayout, resolveTarget, fitSidebarAgents, fitAgentsEverywhere, sidebarAgentsGroup, retireMonitor, returnChat, routeChat, shownPanels, showCodePanel, showPanel, sidebarAt, sidebarGroupIDs, minimumWidth, fitWidth, splitBeside, type Group, type LayoutNode, type Panel, type WorkspacePage } from '../packages/surfaces/src/workspace-model.ts';
import { applyTemplate, builtinPresets, parseSavedLayouts, restoreTemplate } from '../packages/surfaces/src/layout-presets.ts';

const sidebar = ['column', ['collections', 'agents']];
const kinds = (node: LayoutNode | null): unknown => !node ? null : node.type === 'group' ? node.panels.map(panel => panel.kind).join('+') : [node.direction, node.children.map(kinds)];
const sizes = (node: LayoutNode | null): unknown => !node || node.type === 'group' ? null : [node.sizes, node.children.map(sizes)];
const work = (selection = emptyTarget()) => defaultLayout(selection).pages[0];
const target = (sessionId: string) => ({ collectionId: 'w', sessionId, commandId: '' });
const pinnedChat = (id: string, sessionId: string, detached = false): Panel => ({ id, kind: 'chat', binding: { mode: 'pinned', target: target(sessionId) }, detached });
const follow = (id: string, kind: string): Panel => ({ id, kind, binding: { mode: 'follow' }, detached: false });
let counter = 0; const id = () => `new-${++counter}`;
const reparse = (page: WorkspacePage) => parseLayout(JSON.stringify({ version: 2, selectedPage: page.id, pages: [page] })).pages[0];

test('the default page: workspaces above agents, both in view, the conversation, output minimized below', () => {
  const page = work({ collectionId: 'alpha', sessionId: 'session', commandId: '' });
  assert.deepEqual(kinds(page.layout), ['row', [sidebar, ['column', ['chat', 'console']]]]);
  assert.equal(sidebarAt(page.layout), 'first');
  assert.equal(groups(page.layout).find(group => group.panels[0].kind === 'console')!.minimized, true);
  const restored = parseLayout(JSON.stringify(defaultLayout({ collectionId: 'alpha', sessionId: 'session', commandId: '' })));
  assert.equal(restored.version, 2);
  for (const forbidden of ['token', 'permission', 'profile', 'gateway', 'baseUrl']) assert.equal(JSON.stringify(restored).includes(forbidden), false);
});

test('normalizing drops empty groups, unwraps lone children, merges same-direction splits and keeps shares', () => {
  const a: Group = { type: 'group', id: 'ga', panels: [follow('a', 'chat')], active: 'a' }, b: Group = { ...a, id: 'gb', panels: [follow('b', 'files')], active: 'b' }, c: Group = { ...a, id: 'gc', panels: [follow('c', 'editor')], active: 'c' };
  const nested: LayoutNode = { type: 'split', direction: 'row', sizes: [0.5, 0.5], children: [a, { type: 'split', direction: 'row', sizes: [0.5, 0.5], children: [b, c] }] };
  assert.deepEqual(sizes(normalize(nested)), [[0.5, 0.25, 0.25], [null, null, null]]);
  assert.deepEqual(kinds(normalize({ type: 'split', direction: 'column', sizes: [1], children: [a] })), 'chat');
  assert.equal(normalize({ ...a, panels: [] }), null);
  assert.equal(normalize({ ...a, active: 'missing' })!.type === 'group' && (normalize({ ...a, active: 'missing' }) as Group).active, 'a', 'a missing front tab falls back to the first');
  const removed = removePanel(nested, 'b');
  assert.deepEqual(kinds(removed), ['row', ['chat', 'editor']]);
  assert.deepEqual(sizes(removed), [[0.5, 0.5], [null, null]], 'the neighbour in its split takes the space a closed panel leaves');
  assert.deepEqual(sizes(removePanel({ type: 'split', direction: 'row', sizes: [0.5, 0.3, 0.2], children: [a, b, c] }, 'c')), [[0.625, 0.375], [null, null]], 'siblings share it in proportion');
});

test('docking moves a panel to an edge of the content area; the sidebar stays at the page edge', () => {
  const layout = work().layout, output = panels(layout).find(panel => panel.kind === 'console')!;
  assert.equal(dockedSide(layout, output.id), 'bottom');
  assert.deepEqual(kinds(dockPanel(layout, output.id, 'right')), ['row', [sidebar, 'chat', 'console']]);
  assert.deepEqual(kinds(dockPanel(layout, output.id, 'left')), ['row', [sidebar, 'console', 'chat']]);
  assert.deepEqual(kinds(dockPanel(layout, output.id, 'top')), ['row', [sidebar, ['column', ['console', 'chat']]]]);
  for (const side of ['left', 'right', 'top', 'bottom'] as const) {
    const docked = dockPanel(layout, output.id, side);
    assert.equal(dockedSide(docked, output.id), side);
    assert.equal(sidebarAt(docked), 'first');
    assert.doesNotThrow(() => reparse({ ...work(), layout: docked }));
  }
  // Content beside a sidebar: a bottom dock spans the content, not the sidebar.
  const wide = insertDocked(layout, follow('t', 'terminal'), 'right');
  assert.deepEqual(kinds(insertDocked(wide, follow('o', 'usage'), 'bottom')), ['row', [sidebar, ['column', [['row', [['column', ['chat', 'console']], 'terminal']], 'usage']]]]);
  // The workspace list itself docks against the whole page.
  const list = panels(layout).find(panel => panel.kind === 'collections')!;
  assert.deepEqual(kinds(dockPanel(layout, list.id, 'right')), ['row', ['agents', ['column', ['chat', 'console']], 'collections']]);
  const single: LayoutNode = { type: 'group', id: 'g', panels: [output], active: output.id };
  assert.equal(dockPanel(single, output.id, 'left'), single, 'the only panel has nowhere to go');
});

test('showing a panel docks it once where its kind goes, and brings it forward when it exists', () => {
  const page = work();
  assert.equal(defaultDock('terminal'), 'right'); assert.equal(defaultDock('console'), 'bottom');
  const opened = showPanel(page, 'terminal', 'right', id);
  assert.equal(opened.added, true);
  assert.deepEqual(kinds(opened.page.layout), ['row', [sidebar, ['column', ['chat', 'console']], 'terminal']]);
  // As a background tab of another group, then shown again: it comes to the front, no duplicate.
  const tabbed = { ...opened.page, layout: splitBeside(removePanel(opened.page.layout, opened.id), groups(opened.page.layout)[1].id, follow(opened.id, 'terminal'), 'right') };
  const again = showPanel(tabbed, 'terminal', 'left', id);
  assert.equal(again.added, false); assert.equal(again.id, opened.id);
  assert.equal(shownPanels(again.page).filter(panel => panel.kind === 'terminal').length, 1);
  const extension = showPanel(page, 'extension', 'right', id);
  const second = showPanel(extension.page, 'extension', 'right', id, true);
  assert.notEqual(second.id, extension.id);
  assert.equal(shownPanels(second.page).filter(panel => panel.kind === 'extension').length, 2);
  // A minimized group is restored.
  const output = panels(page.layout).find(panel => panel.kind === 'console')!;
  assert.equal(groups(showPanel(page, 'console', 'bottom', id).page.layout).find(group => group.panels.includes(output))!.minimized, false);
  assert.deepEqual(kinds(removeFromPage(opened.page, opened.id).layout), kinds(page.layout), 'closing restores the arrangement it was added to');
});

test('Files and the editor pair up: one opens beside the other', () => {
  const page = work(), editor = showPanel(page, 'editor', 'right', id).page;
  const files = showCodePanel(editor, 'files', 'right', id);
  assert.deepEqual(kinds(files.page.layout), ['row', [sidebar, ['column', ['chat', 'console']], 'files', 'editor']]);
  assert.equal(showCodePanel(files.page, 'editor', 'right', id).added, false);
});

test('chats: one panel per conversation, routed, detached into their own window and brought back', () => {
  const base = work(target('s'));
  // The default page's conversation is its preview tab: choosing conversations reuses it, never adds a panel.
  const before = counter;
  let routed = base;
  for (const session of ['s', 's', 'other', 's']) {
    const next = routeChat(routed, target(session), id);
    routed = next.page;
    assert.deepEqual(shownPanels(routed).filter(panel => panel.kind === 'chat').map(panel => panel.id), ['w-work-chat']);
    assert.equal(next.id, 'w-work-chat');
    assert.equal(resolveTarget(shownPanels(routed).find(panel => panel.kind === 'chat')!, routed.selection).sessionId, session);
  }
  assert.equal(counter, before, 'no chat panel is added while the preview tab is there');
  // A regular tab stays on its conversation: another one opens in a preview tab beside it, in its group.
  const regular = mapPagePanels(base, panel => panel.kind === 'chat' ? pinnedChat(panel.id, 's') : panel);
  const beside = routeChat(regular, target('other'), id);
  assert.equal(beside.added, true);
  assert.deepEqual(groupOf(beside.page, beside.id)!.panels.map(panel => [panel.id, resolveTarget(panel, beside.page.selection).sessionId, !!panel.preview]), [['w-work-chat', 's', false], [beside.id, 'other', true]]);
  // A regular tab and the preview tab on the same conversation: it is the regular tab's.
  const both = { ...base, layout: insertDocked(base.layout, pinnedChat('pinned-s', 's'), 'right') };
  const once = routeChat(both, target('s'), id);
  assert.deepEqual(shownPanels(once.page).filter(panel => panel.kind === 'chat').map(panel => panel.id), ['pinned-s']);
  assert.equal(once.id, 'pinned-s');
  // Detach: the only content panel stays as a card; with another content panel it leaves no gap.
  const card = detachChat(base, 'w-work-chat', target('s'));
  assert.equal(panels(card.layout).find(panel => panel.id === 'w-work-chat')!.detached, true);
  assert.equal(detachedFor(card, target('s'))?.id, 'w-work-chat');
  const withEditor = showPanel(base, 'editor', 'right', id).page, away = detachChat(withEditor, 'w-work-chat', target('s'));
  assert.equal(panels(away.layout).some(panel => panel.kind === 'chat'), false);
  assert.deepEqual(away.detached.map(panel => panel.id), ['w-work-chat']);
  assert.doesNotThrow(() => reparse(away));
  const back = returnChat(away, 'w-work-chat', target('s'));
  assert.equal(back.detached.length, 0);
  assert.equal(panels(back.layout).find(panel => panel.id === 'w-work-chat')!.detached, false);
  assert.deepEqual(kinds(returnChat(card, 'w-work-chat', target('s')).layout), kinds(base.layout), 'a card comes back in place');
  assert.equal(panels(returnChat(card, 'w-work-chat', target('s')).layout).find(panel => panel.id === 'w-work-chat')!.detached, false);
  // Brought back where another panel shows that conversation: it goes, rather than duplicating.
  const dup = { ...away, layout: insertDocked(away.layout, follow('f', 'chat'), 'right') };
  assert.equal(allPanels(returnChat(dup, 'w-work-chat', target('s'))).some(panel => panel.id === 'w-work-chat'), false);
});

test('a review opens as a background tab beside the chat it reviews, once, within the page limit', () => {
  const base = work(target('s'));
  const opened = openBesideChat(base, target('review-1'), 's', id)!;
  assert.ok(opened.added);
  const group = groupOf(opened.page, opened.id)!;
  // In the reviewed chat's group, right after its tab; the chat stays in front, and stays on its conversation.
  assert.deepEqual(group.panels.map(panel => panel.id), ['w-work-chat', opened.id]);
  assert.equal(group.active, 'w-work-chat');
  assert.deepEqual(opened.page.selection, base.selection);
  assert.deepEqual(resolveTarget(group.panels[1], opened.page.selection), target('review-1'));
  assert.doesNotThrow(() => reparse(opened.page));
  // Asked again (a second click, a restart): the tab it has, nothing added.
  const again = openBesideChat(opened.page, target('review-1'), 's', id)!;
  assert.equal(again.added, false); assert.equal(again.id, opened.id); assert.equal(again.page, opened.page);
  // Beside a pinned chat of the reviewed conversation, in that chat's group.
  const beside = { ...base, layout: insertDocked(base.layout, pinnedChat('pinned-a', 'a'), 'right') };
  const next = openBesideChat(beside, target('review-2'), 'a', id)!;
  assert.deepEqual(groupOf(next.page, next.id)!.panels.map(panel => panel.id), ['pinned-a', next.id]);
  // The reviewed conversation is not on the page: beside the page's conversation.
  assert.equal(groupOf(openBesideChat(base, target('review-3'), 'elsewhere', id)!.page, 'w-work-chat')!.panels.length, 2);
  // No chat to join, or a full page: nothing (the caller says so).
  const noChat = { ...base, layout: removeFromPage(base, 'w-work-chat').layout };
  assert.equal(openBesideChat(noChat, target('review-4'), 's', id), undefined);
  let full = base;
  while (allPanels(full).length < PANEL_LIMIT) full = { ...full, layout: insertDocked(full.layout, follow(`x-${allPanels(full).length}`, 'editor'), 'right') };
  assert.equal(openBesideChat(full, target('review-5'), 's', id), undefined);
  // The page that shows a conversation: the selected one first.
  const layout = { version: 2 as const, selectedPage: 'p2', pages: [{ ...base, id: 'p1' }, { ...routeChat(beside, target('t'), id).page, id: 'p2' }] };
  assert.equal(pageShowing(layout, 'a')?.id, 'p2');
  assert.equal(pageShowing(layout, 's')?.id, 'p1');
  assert.equal(pageShowing(layout, 'nowhere'), undefined);
});

test('a chat put back from its window returns where it was', () => {
  const chatGroup = (panel: Panel): Group => ({ type: 'group', id: `g-${panel.id}`, panels: [panel], active: panel.id });
  const sidebar: Group = { type: 'group', id: 'g-side', panels: [follow('side', 'collections')], active: 'side' };
  const editor = chatGroup(follow('ed', 'editor')), files = chatGroup(follow('fi', 'files')), output = { ...chatGroup(follow('out', 'console')) };
  const page = (layout: LayoutNode): WorkspacePage => ({ ...emptyPage('p', 'Work', target('s'), layout) });
  const chat = follow('c', 'chat');
  // Beside the sidebar: chat | editor in a row, output below both.
  const layout: LayoutNode = { type: 'split', direction: 'row', sizes: [0.2, 0.8], children: [sidebar,
    { type: 'split', direction: 'column', sizes: [0.7, 0.3], children: [{ type: 'split', direction: 'row', sizes: [0.6, 0.4], children: [chatGroup(chat), editor] }, output] }] };
  const before = page(layout);
  const away = detachChat(before, 'c', target('s'));
  const origin = away.detached[0].origin!;
  assert.deepEqual({ beside: origin.beside, side: origin.side, share: origin.share }, { beside: ['g-ed'], side: 'left', share: 0.6 }, 'remembers its neighbour, side and share');
  assert.doesNotThrow(() => reparse(away), 'the origin is saved with the page');
  assert.deepEqual(reparse(away).detached[0].origin, origin);
  const back = returnChat(away, 'c', target('s'));
  assert.deepEqual(kinds(back.layout), kinds(before.layout), 'back in the same place');
  assert.deepEqual(sizes(back.layout), sizes(before.layout), 'with the same sizes');
  assert.equal(allPanels(back).find(panel => panel.id === 'c')!.origin, undefined, 'the origin is dropped once back');

  // A tab of a group comes back as a tab, at its place, in front.
  const tabs: LayoutNode = { type: 'split', direction: 'row', sizes: [0.2, 0.8], children: [sidebar, { type: 'group', id: 'g-work', panels: [follow('ed', 'editor'), chat, follow('fi', 'files')], active: 'ed' }] };
  const tabbed = returnChat(detachChat(page(tabs), 'c', target('s')), 'c', target('s'));
  const group = groups(tabbed.layout).find(value => value.id === 'g-work')!;
  assert.deepEqual(group.panels.map(panel => panel.id), ['ed', 'c', 'fi']);
  assert.equal(group.active, 'c');

  // Its neighbour moved meanwhile: it goes beside that neighbour wherever it now is.
  const moved = { ...away, layout: { type: 'split', direction: 'row', sizes: [0.2, 0.5, 0.3], children: [sidebar, output, editor] } as LayoutNode };
  assert.deepEqual(kinds(returnChat(moved, 'c', target('s')).layout), ['row', ['collections', 'console', 'chat', 'editor']]);

  // Its neighbours are gone: beside the largest group (never the sidebar or a minimized group).
  const gone = { ...away, layout: { type: 'split', direction: 'row', sizes: [0.2, 0.3, 0.5], children: [sidebar, files, { ...output, minimized: false }] } as LayoutNode };
  assert.deepEqual(kinds(returnChat(gone, 'c', target('s')).layout), ['row', ['collections', 'files', 'console', 'chat']]);
  assert.equal(largestGroup({ type: 'split', direction: 'row', sizes: [0.6, 0.4], children: [sidebar, files] })!.id, 'g-fi', 'the sidebar is never the largest');
  // A page emptied meanwhile takes it whole.
  assert.deepEqual(kinds(returnChat({ ...away, layout: null }, 'c', target('s')).layout), 'chat');

  // From a saved layout: the page that holds it, whichever is selected.
  const saved = { version: 2 as const, selectedPage: 'other', pages: [away, emptyPage('other', 'Other', target('s'), editor)] };
  const returned = returnDetached(saved, 'c')!;
  assert.equal(returned.page, 'p');
  assert.equal(allPanels(returned.layout.pages[0]).find(panel => panel.id === 'c')!.detached, false);
  assert.equal(returnDetached(returned.layout, 'c'), undefined, 'already back: nothing to do');
  assert.deepEqual(detachedChats(saved).map(value => value.panel.id), ['c']);
  // Command output minimized below the chat is minimized again when the chat comes back above it,
  // although it could not stay minimized alone in the row meanwhile.
  const withOutput = page({ type: 'split', direction: 'row', sizes: [0.2, 0.5, 0.3], children: [sidebar, { type: 'split', direction: 'column', sizes: [0.7, 0.3], children: [chatGroup(chat), { ...output, minimized: true }] }, editor] });
  const outputAway = detachChat(withOutput, 'c', target('s'));
  assert.equal(outputAway.detached[0].origin!.minimized, true);
  const unminimized = { ...outputAway, layout: mapGroups(outputAway.layout, group => ({ ...group, minimized: undefined })) };
  const outputBack = returnChat(unminimized, 'c', target('s'));
  assert.deepEqual(kinds(outputBack.layout), kinds(withOutput.layout));
  assert.equal(groups(outputBack.layout).find(group => group.id === 'g-out')!.minimized, true);
  // An origin that does not validate is refused, not guessed at.
  const bad = JSON.parse(JSON.stringify(away)); bad.detached[0].origin.side = 'diagonal';
  assert.throws(() => reparse(bad));
});

test('repair on load: duplicate chats removed, popped-out groups docked back', () => {
  let page = work(target('s'));
  page = { ...page, layout: insertDocked(insertDocked(page.layout, pinnedChat('dup', 's'), 'right'), pinnedChat('other', 't'), 'right') };
  page = { ...page, popouts: [{ type: 'group', id: 'gp', panels: [follow('ed', 'editor')], active: 'ed' }] };
  const repaired = repairLayout({ version: 2, selectedPage: page.id, pages: [page] }).pages[0];
  // The preview tab and a regular tab on the same conversation: the regular tab stays.
  assert.deepEqual(shownPanels(repaired).filter(panel => panel.kind === 'chat').map(panel => panel.id), ['dup', 'other']);
  assert.equal(repaired.popouts.length, 0);
  assert.ok(panels(repaired.layout).some(panel => panel.id === 'ed'), 'a popped-out editor comes back docked');
  assert.deepEqual(redockPopouts(work()), work());
  const clean = defaultLayout(target('s'));
  assert.equal(repairLayout(clean), clean);
  assert.deepEqual(dedupeChats(work(target('s')), target('s')).layout, work(target('s')).layout);
});

test('switching workspace adopts the current shares where the arrangement matches', () => {
  const from = defaultLayout(target('a')), to = defaultLayout({ collectionId: 'b', sessionId: '', commandId: '' });
  const resized = { ...from, pages: [{ ...from.pages[0], layout: { ...(from.pages[0].layout as Extract<LayoutNode, { type: 'split' }>), sizes: [0.3, 0.7] } }] };
  const adopted = adoptGeometry(to, resized);
  assert.deepEqual(sizes(adopted.pages[0].layout), sizes(resized.pages[0].layout));
  assert.deepEqual(panels(adopted.pages[0].layout).map(panel => panel.id), panels(to.pages[0].layout).map(panel => panel.id), 'the target keeps its own panels');
  const different = { ...to, pages: [{ ...to.pages[0], layout: insertDocked(to.pages[0].layout, follow('x', 'terminal'), 'right') }] };
  assert.deepEqual(sizes(adoptGeometry(different, resized).pages[0].layout), sizes(different.pages[0].layout), 'a different arrangement keeps its own shares');
  // The sidebar on the other side has the shape of the content column: neither takes the other's shares or minimized output.
  const mirrored = { ...from, pages: [{ ...from.pages[0], layout: { ...(from.pages[0].layout as Extract<LayoutNode, { type: 'split' }>), sizes: [0.6, 0.4], children: [...(from.pages[0].layout as Extract<LayoutNode, { type: 'split' }>).children].reverse() } }] };
  const kept = adoptGeometry(to, mirrored).pages[0].layout as Extract<LayoutNode, { type: 'split' }>;
  assert.deepEqual(sizes(kept.children[1]), sizes((to.pages[0].layout as Extract<LayoutNode, { type: 'split' }>).children[1]));
  assert.equal(groups(kept).find(group => group.panels[0].kind === 'console')!.minimized, true);
});

test('invalid versions, oversized data, duplicate ids, unsupported properties, and detached launchers are rejected', () => {
  for (const mutate of [
    (layout: any) => { layout.version = 3; },
    (layout: any) => { layout.token = 'must-not-be-imported'; },
    (layout: any) => { layout.pages[0].selection.commandId = 'missing-session'; },
    (layout: any) => { layout.pages.push(structuredClone(layout.pages[0])); },
    (layout: any) => { layout.pages[0].layout.sizes = [2, 1]; },
    (layout: any) => { layout.pages[0].layout.sizes = [0.5]; },
    (layout: any) => { layout.pages[0].layout.children[0].children[0].active = 'missing'; },
    (layout: any) => { layout.pages[0].layout.children[0].children[0].panels[0].detached = true; },
    (layout: any) => { layout.pages[0].layout.children[0].children[0].panels.push(structuredClone(layout.pages[0].layout.children[0].children[0].panels[0])); },
    (layout: any) => { layout.pages[0].floating = [{ group: layout.pages[0].layout.children[0].children[0], box: { width: 10, height: 10, left: 0, top: 0 } }]; },
    (layout: any) => { layout.pages[0].detached = [{ id: 'x', kind: 'chat', binding: { mode: 'follow' }, detached: true }]; },
  ]) { const layout = defaultLayout(); mutate(layout); assert.throws(() => parseLayout(JSON.stringify(layout)), String(mutate)); }
  assert.throws(() => parseLayout('x'.repeat(193 << 10)));
  const tooMany = work();
  tooMany.layout = groups(tooMany.layout).reduce<LayoutNode | null>((node) => node, tooMany.layout);
  let node = tooMany.layout;
  for (let i = 0; i < 13; i++) node = insertDocked(node, follow(`extra-${i}`, 'usage'), 'right');
  assert.throws(() => reparse({ ...tooMany, layout: node }), /At most 16 panels/);
});

test('unknown modules and floating groups survive a round trip', () => {
  const page = mapPagePanels(work(), panel => ({ ...panel, kind: panel.kind === 'console' ? 'constructor' : panel.kind }));
  const floating = { ...page, floating: [{ group: { type: 'group' as const, id: 'gf', panels: [follow('f', 'files')], active: 'f' }, box: { left: 10, top: 20, width: 300, height: 200 } }] };
  const restored = reparse(floating);
  assert.ok(shownPanels(restored).some(panel => panel.kind === 'constructor'));
  assert.deepEqual(restored.floating, floating.floating);
});

test('clone and Monitor retirement keep references without sharing view identities', () => {
  const page = mapPagePanels(work(), panel => panel.kind === 'chat' ? pinnedChat(panel.id, 'session-a') : panel);
  const copied = clonePage(page, id);
  assert.notEqual(copied.id, page.id);
  assert.equal(allPanels(copied).some(panel => allPanels(page).some(original => original.id === panel.id)), false);
  assert.deepEqual(resolveTarget(shownPanels(copied).find(panel => panel.kind === 'chat')!, emptyTarget()), target('session-a'));
  assert.doesNotThrow(() => parseLayout(JSON.stringify({ version: 2, selectedPage: page.id, pages: [page, copied] })));
  const layout = defaultLayout(target('s'));
  layout.pages.push({ ...structuredClone(layout.pages[0]), id: 'monitor', name: 'Monitor' }); layout.selectedPage = 'monitor';
  const migrated = retireMonitor(layout);
  assert.equal(migrated.pages.length, 1); assert.equal(migrated.selectedPage, 'work'); assert.equal(migrated.pages[0].selection.sessionId, 's');
});

// A version 1 layout as saved by the binary-tree layout: a terminal docked along the
// whole bottom, the output collapsed under the chat, a chat in its own window.
const v1 = '{"version":1,"selectedPage":"work","pages":[{"id":"work","name":"Work","selection":{"collectionId":"alpha","sessionId":"s","commandId":""},"layout":{"type":"split","direction":"column","ratio":0.6,"children":[{"type":"split","direction":"row","ratio":0.22,"children":[{"type":"panel","panel":{"id":"alpha-work-collections","kind":"collections","binding":{"mode":"follow"},"detached":false,"collapsed":false}},{"type":"split","direction":"column","ratio":0.72,"children":[{"type":"panel","panel":{"id":"alpha-work-chat","kind":"chat","binding":{"mode":"follow"},"detached":false,"collapsed":false}},{"type":"panel","panel":{"id":"alpha-work-console","kind":"console","binding":{"mode":"follow"},"detached":false,"collapsed":true}}]}]},{"type":"split","direction":"row","ratio":0.5,"children":[{"type":"panel","panel":{"id":"t1","kind":"terminal","binding":{"mode":"follow"},"detached":false}},{"type":"panel","panel":{"id":"away","kind":"chat","binding":{"mode":"pinned","target":{"collectionId":"alpha","sessionId":"t","commandId":""}},"detached":true}}]}]}},{"id":"review","name":"Changes","selection":{"collectionId":"alpha","sessionId":"","commandId":""},"layout":{"type":"panel","panel":{"id":"c1","kind":"changes","binding":{"mode":"follow"},"detached":false}}}]}';
test('version 1 layouts migrate: same panels, places and sizes; collapsed output minimized; chats in windows kept', () => {
  const layout = parseLayout(v1), page = layout.pages[0];
  assert.equal(layout.version, 2); assert.equal(layout.selectedPage, 'work'); assert.equal(layout.pages.length, 2);
  assert.deepEqual(JSON.stringify(kinds(page.layout)), JSON.stringify(['column', [['row', [sidebar, ['column', ['chat', 'console']]]], 'terminal']]));
  assert.deepEqual(sizes(page.layout), [[0.6, 0.4], [[[0.22, 0.78], [[[0.55, 0.45], [null, null]], [[0.72, 0.28], [null, null]]]], null]]);
  assert.equal(groups(page.layout).find(group => group.panels[0].kind === 'console')!.minimized, true);
  assert.deepEqual(page.detached.map(panel => [panel.id, panel.detached]), [['away', true]], 'the chat in its own window is kept, out of the page');
  assert.ok(panels(page.layout).some(panel => panel.id === 'alpha-work-collections-agents'), 'the sidebar gains the agents list, under the workspaces');
  assert.deepEqual(kinds(layout.pages[1].layout), 'changes');
  // Migrated pages are ordinary version 2 pages from then on.
  assert.deepEqual(parseLayout(JSON.stringify(layout)), layout);
  // Everything in its own window: the first chat stays as a card.
  const allAway = v1.replace('"id":"alpha-work-chat","kind":"chat","binding":{"mode":"follow"},"detached":false', '"id":"alpha-work-chat","kind":"chat","binding":{"mode":"pinned","target":{"collectionId":"alpha","sessionId":"s","commandId":""}},"detached":true').replace('{"id":"t1","kind":"terminal"', '{"id":"t1","kind":"console"');
  const cards = parseLayout(allAway).pages[0];
  assert.equal(panels(cards.layout).find(panel => panel.id === 'alpha-work-chat')?.detached, true);
  // A version 1 layout that was invalid is still refused, never half-read.
  assert.throws(() => parseLayout(v1.replace('"ratio":0.6', '"ratio":1')));
  assert.throws(() => parseLayout(v1.replace('"version":1', '"version":1,"extra":true')));
});

test('a saved page with Agents tabbed behind Workspaces gains the stack; nothing else moves', () => {
  const list = follow('list', 'collections'), agents = follow('agents', 'agents'), chat = follow('chat', 'chat'), terminal = follow('t', 'terminal');
  const tabbed: WorkspacePage = { ...work(), layout: { type: 'split', direction: 'row', sizes: [0.25, 0.5, 0.25], children: [
    { type: 'group', id: 'g-list', panels: [list, agents], active: 'agents' }, { type: 'group', id: 'g-chat', panels: [chat], active: 'chat' }, { type: 'group', id: 'g-t', panels: [terminal], active: 't' }] } };
  const repaired = repairLayout({ version: 2, selectedPage: tabbed.id, pages: [tabbed] }).pages[0];
  assert.deepEqual(kinds(repaired.layout), ['row', [sidebar, 'chat', 'terminal']]);
  assert.deepEqual(sizes(repaired.layout), [[0.25, 0.5, 0.25], [[[0.55, 0.45], [null, null]], null, null]]);
  assert.equal(sidebarAt(repaired.layout), 'first');
  assert.deepEqual(sidebarGroupIDs(repaired.layout), ['g-list', 'g-agents'], 'the list keeps its group; agents gets its own');
  assert.doesNotThrow(() => reparse(repaired));
  // Already stacked, or Agents elsewhere: unchanged.
  const page = work();
  assert.equal(repairLayout({ version: 2, selectedPage: page.id, pages: [page] }).pages[0], page);
  const apart: WorkspacePage = { ...tabbed, layout: { ...tabbed.layout as Extract<LayoutNode, { type: 'split' }>, children: [{ type: 'group', id: 'g-list', panels: [list], active: 'list' }, { type: 'group', id: 'g-chat', panels: [chat, agents], active: 'chat' }, { type: 'group', id: 'g-t', panels: [terminal], active: 't' }] } };
  assert.deepEqual(kinds(repairLayout({ version: 2, selectedPage: apart.id, pages: [apart] }).pages[0].layout), ['row', ['collections', 'chat+agents', 'terminal']]);
  // The stack is the sidebar: one fixed width, never merged into tabs when the window narrows.
  assert.equal(minimumWidth(page.layout, 280), 280 + 360);
  assert.ok(groups(fitWidth(insertDocked(page.layout, follow('e', 'editor'), 'right'), 700, 280)).some(group => group.panels[0].kind === 'agents' && group.panels.length === 1));
});

test('Agents is the window\'s: closed or folded, it stays so on every page, new workspace and old layout until brought back', () => {
  const page = work({ collectionId: 'alpha', sessionId: 's', commandId: '' });
  const changes: WorkspacePage = { ...emptyPage('changes', 'Changes', page.selection, { type: 'group', id: 'g-c', panels: [follow('c', 'changes')], active: 'c' }) };
  const all = { version: 2 as const, selectedPage: page.id, pages: [page, changes] };
  // Closed: off every page; Workspaces takes the sidebar, whose share of the row (its fixed width) does not move.
  const closed = fitAgentsEverywhere(all, 'closed');
  assert.deepEqual(kinds(closed.pages[0].layout), ['row', ['collections', ['column', ['chat', 'console']]]]);
  assert.deepEqual((closed.pages[0].layout as Extract<LayoutNode, { type: 'split' }>).sizes, [0.2, 0.8]);
  assert.equal(sidebarAt(closed.pages[0].layout), 'first');
  assert.equal(closed.pages[1], changes, 'a page without Agents is left alone');
  assert.equal(fitAgentsEverywhere(closed, 'closed'), closed, 'already closed: the same object');
  assert.doesNotThrow(() => parseLayout(JSON.stringify(closed)));
  // A new workspace, a saved page loaded again, an old version 1 layout: none brings it back.
  assert.equal(allPanels(defaultLayout(page.selection, 'closed').pages[0]).some(panel => panel.kind === 'agents'), false);
  assert.equal(allPanels(repairLayout(all, 'closed').pages[0]).some(panel => panel.kind === 'agents'), false);
  assert.equal(parseLayout(v1, 'closed').pages.some(value => allPanels(value).some(panel => panel.kind === 'agents')), false);
  assert.equal(parseLayout(v1).pages.some(value => allPanels(value).some(panel => panel.kind === 'agents')), true, 'without a choice, migration adds it as before');
  // Tabbed behind Workspaces in an old save: stacked, then closed.
  const tabbed: WorkspacePage = { ...page, layout: { type: 'split', direction: 'row', sizes: [0.25, 0.75], children: [{ type: 'group', id: 'g-list', panels: [follow('list', 'collections'), follow('ag', 'agents')], active: 'ag' }, { type: 'group', id: 'g-chat', panels: [follow('chat', 'chat')], active: 'chat' }] } };
  assert.deepEqual(kinds(repairLayout({ version: 2, selectedPage: page.id, pages: [tabbed] }, 'closed').pages[0].layout), ['row', ['collections', 'chat']]);
  // Brought back: under Workspaces on every page whose sidebar has them, with its share; a page without a sidebar stays as it is.
  const shown = fitAgentsEverywhere(closed, 'shown');
  assert.deepEqual(kinds(shown.pages[0].layout), ['row', [sidebar, ['column', ['chat', 'console']]]]);
  assert.deepEqual(sizes(shown.pages[0].layout), [[0.2, 0.8], [[[0.55, 0.45], [null, null]], [[0.62, 0.38], [null, null]]]]);
  assert.equal(shown.pages[1], changes);
  assert.doesNotThrow(() => parseLayout(JSON.stringify(shown)));
  // Without a choice, saved pages are as they were.
  assert.equal(fitSidebarAgents(closed.pages[0], undefined), closed.pages[0]);
  assert.equal(fitSidebarAgents(page, 'shown'), page);
  // Folded: folded on every page (and in a new workspace), unfolded when shown again.
  const folded = fitAgentsEverywhere(all, 'folded');
  assert.equal(sidebarAgentsGroup(folded.pages[0].layout)?.minimized, true);
  assert.equal(sidebarAgentsGroup(defaultLayout(page.selection, 'folded').pages[0].layout)?.minimized, true);
  assert.equal(sidebarAgentsGroup(fitAgentsEverywhere(folded, 'shown').pages[0].layout)?.minimized, undefined);
  assert.equal(sidebarAgentsGroup(fitSidebarAgents(closed.pages[0], 'folded').layout)?.minimized, true, 'brought back folded');
  // The sidebar's Agents group is found only where it can fold: stacked under Workspaces.
  assert.equal(sidebarAgentsGroup(page.layout)?.panels[0].kind, 'agents');
  assert.equal(sidebarAgentsGroup(closed.pages[0].layout), undefined);
  // An id already taken on the page is not reused.
  const clash: WorkspacePage = { ...closed.pages[0], detached: [pinnedChat(`${panels(closed.pages[0].layout)[0].id}-agents`, 's', true)] };
  const back = fitSidebarAgents(clash, 'shown');
  assert.equal(new Set(allPanels(back).map(panel => panel.id)).size, allPanels(back).length);
});

test('every built-in layout stacks Workspaces above Agents; a saved one gains the stack', () => {
  for (const preset of builtinPresets) {
    const left = preset.layout.type === 'split' ? preset.layout.children[0] : preset.layout;
    assert.deepEqual(left, { type: 'split', direction: 'column', sizes: [0.55, 0.45], children: [{ type: 'group', kinds: ['collections'] }, { type: 'group', kinds: ['agents'] }] }, preset.name);
    const applied = applyTemplate(work(), preset.layout, id);
    assert.equal(sidebarAt(applied.layout), 'first', preset.name);
  }
  const saved = parseSavedLayouts(JSON.stringify([{ id: 'mine', name: 'Mine', description: '', layout: { type: 'split', direction: 'row', sizes: [0.3, 0.7], children: [{ type: 'group', kinds: ['agents', 'collections'] }, { type: 'group', kinds: ['chat', 'changes'] }] } }]));
  assert.deepEqual(saved[0].layout, { type: 'split', direction: 'row', sizes: [0.3, 0.7], children: [{ type: 'split', direction: 'column', sizes: [0.55, 0.45], children: [{ type: 'group', kinds: ['collections'] }, { type: 'group', kinds: ['agents'] }] }, { type: 'group', kinds: ['chat', 'changes'] }] });
});

test('activating a panel restores its group and brings it to the front', () => {
  const page = work(), output = panels(page.layout).find(panel => panel.kind === 'console')!;
  const active = activatePanel(page, output.id);
  assert.equal(groups(active.layout).find(group => group.panels.includes(output))!.minimized, false);
  assert.deepEqual(mapPanels(page.layout!, panel => panel), page.layout);
  assert.deepEqual(emptyPage('p', 'P', emptyTarget(), null).layout, null);
});

test('conversation order remains stable across response order and activity changes', () => {
  const a = create(SessionSchema, { id: 'a', createdAtUnixMillis: 100n, title: 'Saved title' });
  const b = create(SessionSchema, { id: 'b', createdAtUnixMillis: 200n });
  const c = create(SessionSchema, { id: 'c', createdAtUnixMillis: 200n });
  const snapshots = [a, b, c];
  assert.deepEqual(mergeConversations([], snapshots).map(s => s.id), ['b', 'c', 'a']);
  assert.deepEqual(mergeConversations([], [...snapshots].reverse()).map(s => s.id), ['b', 'c', 'a']);
  const updated = mergeConversations(snapshots, [create(SessionSchema, { id: 'a', status: 'running' })]);
  assert.deepEqual(updated.map(s => s.id), ['b', 'c', 'a']);
  assert.equal(updated[2].title, 'Saved title');
  assert.equal(updated[2].createdAtUnixMillis, 100n);
  assert.equal(updated[2].status, 'running');
  const legacy = ['z', 'a', 'm'].map(id => create(SessionSchema, { id }));
  assert.deepEqual(mergeConversations([], legacy).map(s => s.id), ['a', 'm', 'z']);
  assert.deepEqual(mergeConversations([], legacy.reverse()).map(s => s.id), ['a', 'm', 'z']);
});

test('continuations replace predecessor rows without changing conversation order', () => {
 const first=create(SessionSchema,{id:'first',conversationId:'first',createdAtUnixMillis:100n});
 const other=create(SessionSchema,{id:'other',conversationId:'other',createdAtUnixMillis:200n});
 const next=create(SessionSchema,{id:'next',conversationId:'first',previousExecutionId:'first',createdAtUnixMillis:100n});
 assert.deepEqual(mergeConversations([next,other],[first,next,other]).map(s=>s.id),['other','next']);
});


test('restoring a workspace retires Missions in tabs, floating groups, popouts and dedicated pages', () => {
  const group = (id: string, kinds: string[]): Group => ({
    id, type: 'group', panels: kinds.map(kind => follow(id + kind, kind)), active: id + kinds[0],
  });
  const mixed = emptyPage('mixed', 'Custom', target('active'), group('docked', ['missions', 'files']));
  mixed.floating.push({ group: group('floating', ['missions']), box: { width: 300, height: 200 } });
  mixed.popouts.push(group('popped', ['missions', 'changes']));
  const dedicated = emptyPage('older', 'Missions', target('active'), group('board', ['missions']));
  const saved = { version: 2 as const, selectedPage: dedicated.id, pages: [dedicated, mixed] };
  const restored = repairLayout(saved);
  assert.equal(restored.selectedPage, mixed.id);
  assert.deepEqual(restored.pages.map(page => page.name), ['Custom']);
  assert.deepEqual(allPanels(restored.pages[0]).map(panel => panel.kind), ['files', 'changes']);
  const fallback = repairLayout({ ...saved, pages: [dedicated] });
  assert.deepEqual(fallback, repairLayout(defaultLayout(target('active'))));
});

test('old layout presets retain the other panels and their relative shares', () => {
  const node = restoreTemplate({ type: 'split', direction: 'row', sizes: [0.25, 0.5, 0.25], children: [
    { type: 'group', kinds: ['missions', 'files'] }, { type: 'group', kinds: ['missions'] }, { type: 'group', kinds: ['changes'] },
  ]});
  assert.deepEqual(node, { type: 'split', direction: 'row', sizes: [0.5, 0.5], children: [
    { type: 'group', kinds: ['files'] }, { type: 'group', kinds: ['changes'] },
  ]});
  assert.deepEqual(restoreTemplate({ type: 'group', kinds: ['missions'] }), builtinPresets[0].layout);
});
