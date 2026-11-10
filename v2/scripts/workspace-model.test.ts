import { create } from '@bufbuild/protobuf';
import { SessionSchema } from '@lazybox/protocol';
import { mergeConversations } from '../packages/surfaces/src/conversation-model.ts';
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { retireMonitor, clonePage, defaultLayout, emptyTarget, mapPanels, movePanel, panels, parseLayout, removePanel, resolveTarget, visibleLayout } from '../packages/surfaces/src/workspace-model.ts';

test('one focused default page serializes without authentication or execution options', () => {
  const original = defaultLayout({ collectionId: 'alpha', sessionId: 'session', commandId: '' });
  const restored = parseLayout(JSON.stringify(original));
  assert.equal(restored.pages.length, 1);
  assert.equal(restored.pages[0].layout.type, 'split');
  assert.deepEqual(panels(restored.pages[0].layout).map(panel => panel.kind), ['collections', 'chat', 'console']);
  for (const forbidden of ['token', 'permission', 'profile', 'gateway', 'baseUrl']) assert.equal(JSON.stringify(restored).includes(forbidden), false);
});
test('pin, follow, reorder, removal, and clone preserve record references without sharing view identities', () => {
  const layout = defaultLayout(), page = layout.pages[0];
  const pinned = { collectionId: 'alpha', sessionId: 'session-a', commandId: '' };
  page.layout = mapPanels(page.layout, panel => panel.kind === 'chat' ? { ...panel, binding: { mode: 'pinned', target: pinned } } : panel);
  const chat = panels(page.layout)[1];
  assert.equal(resolveTarget(chat, { ...pinned, sessionId: 'other' }).sessionId, 'session-a');
  assert.equal(resolveTarget(panels(page.layout)[0], pinned), pinned);
  const moved = movePanel(page.layout, chat.id, -1);
  assert.equal(panels(moved)[0].id, chat.id);
  assert.equal(panels(removePanel(moved, chat.id)!).length, 2);
  let counter = 0;
  const copied = clonePage(page, () => `copy-${++counter}`);
  assert.notEqual(copied.id, page.id);
  assert.equal(panels(copied.layout).some(panel => panels(page.layout).some(original => original.id === panel.id)), false);
  assert.deepEqual(resolveTarget(panels(copied.layout)[1], emptyTarget()), pinned);
  assert.doesNotThrow(() => parseLayout(JSON.stringify({ ...layout, pages: [...layout.pages, copied] })));
});
test('invalid versions, oversized data, duplicate ids, unsupported properties, and detached launchers are rejected', () => {
  for (const mutate of [
    (layout: any) => { layout.version = 2; },
    (layout: any) => { layout.token = 'must-not-be-imported'; },
    (layout: any) => { layout.pages[0].selection.commandId = 'missing-session'; },
    (layout: any) => { layout.pages.push(structuredClone(layout.pages[0])); },
    (layout: any) => { layout.pages[0].layout.ratio = 1; },
    (layout: any) => { layout.pages[0].layout.children[0].panel.detached = true; },
  ]) { const layout = defaultLayout(); mutate(layout); assert.throws(() => parseLayout(JSON.stringify(layout))); }
  assert.throws(() => parseLayout('x'.repeat(129 << 10)));
});
test('unknown modules survive a round trip for a placeholder renderer', () => {
  const layout = defaultLayout();
  layout.pages[0].layout = mapPanels(layout.pages[0].layout, panel => ({ ...panel, kind: panel.kind === 'console' ? 'constructor' : panel.kind }));
  assert.equal(panels(parseLayout(JSON.stringify(layout)).pages[0].layout)[2].kind, 'constructor');
});

test('detached panes do not occupy space and docking preserves the saved arrangement', () => {
  const layout = defaultLayout().pages[0].layout;
  const detached = mapPanels(layout, panel => ({ ...panel, detached: panel.kind === 'chat' }));
  const saved = JSON.stringify(detached);
  assert.deepEqual(panels(visibleLayout(detached)!).map(panel => panel.kind), ['collections', 'console']);
  assert.equal(JSON.stringify(detached), saved, 'projection must not change persisted arrangement');
  assert.deepEqual(visibleLayout(mapPanels(detached, panel => ({ ...panel, detached: false }))), layout);
  assert.equal(visibleLayout(mapPanels(layout, panel => ({ ...panel, detached: true }))), null);
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

test('retiring Monitor preserves customized pages and their conversation references',()=>{
 const layout=defaultLayout({collectionId:'w',sessionId:'s',commandId:''});
 const monitor={...structuredClone(layout.pages[0]),id:'monitor',name:'Monitor'};
 layout.pages.push(monitor);layout.selectedPage='monitor';
 const migrated=retireMonitor(layout);assert.equal(migrated.pages.length,1);assert.equal(migrated.selectedPage,'work');assert.equal(migrated.pages[0].selection.sessionId,'s');
 monitor.name='My review';assert.equal(retireMonitor(layout).pages.length,2);
});
