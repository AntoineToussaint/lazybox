// Conversation tabs (workspace-model.ts "Conversations", conversation-tabs.ts):
// the preview tab a single click replaces, regular tabs that stay, tabs opened
// behind the one in front and next to a parent, one tab per conversation, the
// panel limit, closing and reopening, and the saved form.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  chatGroup, closeTab, defaultDock, defaultLayout, emptyTarget, followFrontTab, forgetConversation, groupOf, insertDocked, keepTab, mapPagePanels, moveToNewPage, oldestPreview, openConversation, openRelated, PAGE_LIMIT, PANEL_LIMIT,
  pageName, parseLayout, previewTab, removeFromPage, repairLayout, resolveTarget, routeChat, settleChats, settleLayout, showWorkspaceTabs, shownPanels, tabFor, tabsToClose, type Panel, type ViewTarget, type WorkspacePage,
} from '../packages/surfaces/src/workspace-model.ts';
import { applyTemplate, builtinPresets, templateOf } from '../packages/surfaces/src/layout-presets.ts';
import { CLOSED_LIMIT, dismissedWith, openModeFor, rememberClosed, rowGesture, tabsSettingOn, tabStatus, takeClosed, workspaceConversations } from '../packages/surfaces/src/conversation-tabs.ts';
import type { Session } from '@lazybox/protocol';

const target = (sessionId: string, collectionId = 'w'): ViewTarget => ({ collectionId, sessionId, commandId: '' });
let counter = 0; const id = () => `t-${++counter}`;
const work = (sessionId = 's') => defaultLayout(target(sessionId)).pages[0];
const chats = (page: WorkspacePage) => shownPanels(page).filter(panel => panel.kind === 'chat');
/** Each chat tab of the page: its conversation, and P for the preview tab. */
const tabs = (page: WorkspacePage) => chats(page).map(panel => `${resolveTarget(panel, page.selection).sessionId}${panel.preview ? ' P' : ''}`);
const front = (page: WorkspacePage, panelID: string) => groupOf(page, panelID)!.active === panelID;
const open = (page: WorkspacePage, sessionId: string, mode: 'preview' | 'tab' | 'background', extra = {}) => openConversation(page, target(sessionId), { mode, ...extra }, id);

test('the first conversation reuses a kept workspace starter tab', () => {
  const page = settleChats(defaultLayout(target('')).pages[0], false);
  const starter = chats(page)[0];
  const opened = open(page, 'first', 'tab');
  assert.equal(opened.id, starter.id);
  assert.deepEqual(tabs(opened.page), ['first']);
  assert.deepEqual(tabs(open(opened.page, 'second', 'tab').page), ['first', 'second']);
});

test('a single click shows the preview tab, and the next single click replaces it', () => {
  let page = work('a');
  assert.deepEqual(tabs(page), ['a P'], 'the default page has one chat: its preview tab');
  page = open(page, 'b', 'preview').page;
  assert.deepEqual(tabs(page), ['b P']);
  assert.equal(page.selection.sessionId, 'b', 'what the preview shows is the selected conversation');
  page = open(page, 'c', 'preview').page;
  assert.deepEqual(tabs(page), ['c P'], 'replaced, not stacked');
});

test('a preview tab kept open (double-click, typing, Keep open) is a regular tab; the next click opens a new preview beside it', () => {
  let page = work('a');
  const chat = chats(page)[0].id;
  page = keepTab(page, chat);
  assert.deepEqual(tabs(page), ['a']);
  assert.equal(keepTab(page, chat), page, 'a regular tab stays as it is');
  const opened = open(page, 'b', 'preview');
  assert.equal(opened.added, true);
  assert.deepEqual(tabs(opened.page), ['a', 'b P']);
  assert.equal(groupOf(opened.page, opened.id)!.id, groupOf(opened.page, chat)!.id, 'in the same group');
  assert.ok(front(opened.page, opened.id));
  // Opening one as a tab (double-click) on its preview keeps it there.
  const kept = open(opened.page, 'b', 'tab');
  assert.equal(kept.id, opened.id);
  assert.deepEqual(tabs(kept.page), ['a', 'b']);
  // "Use as preview tab": a regular tab becomes its group's preview tab, on its own conversation; the group keeps one preview.
  const previewed = open(kept.page, 'c', 'preview').page;
  assert.deepEqual(tabs(previewed), ['a', 'b', 'c P']);
  const following = previewTab(previewed, chat);
  assert.deepEqual(tabs(following), ['a P', 'b', 'c']);
  assert.equal(previewTab(following, chat), following, 'already the preview tab');
  assert.deepEqual(tabs(open(following, 'd', 'preview').page), ['d P', 'b', 'c'], 'the next single click replaces it');
});

test('open in new tab: a regular tab right after the one in front, selected; a preview with no conversation yet takes it', () => {
  let page = keepTab(work('a'), chats(work('a'))[0].id);
  page = open(page, 'b', 'tab').page;
  page = open(page, 'c', 'tab').page;
  assert.deepEqual(tabs(page), ['a', 'b', 'c']);
  page = open(page, 'a', 'preview').page;
  page = open(page, 'd', 'tab').page;
  assert.deepEqual(tabs(page), ['a', 'd', 'b', 'c'], 'after the tab in front, as in a browser');
  assert.equal(page.selection.sessionId, 'd');
  // A workspace's start (no conversation yet) in the preview tab: the new conversation takes its place.
  const start = defaultLayout({ collectionId: 'w', sessionId: '', commandId: '' }).pages[0];
  const started = open(start, 'new', 'tab');
  assert.equal(started.added, false);
  assert.deepEqual(tabs(started.page), ['new']);
});

test('a background tab opens behind the one in front; a related conversation next to its parent', () => {
  let page = keepTab(work('a'), chats(work('a'))[0].id);
  page = open(page, 'b', 'tab').page;
  page = open(page, 'a', 'preview').page;
  const behind = open(page, 'c', 'background');
  assert.equal(behind.added, true);
  assert.deepEqual(tabs(behind.page), ['a', 'c', 'b']);
  assert.equal(behind.page.selection.sessionId, 'a', 'the selection stays');
  assert.ok(!front(behind.page, behind.id));
  assert.equal(open(behind.page, 'c', 'background').page, behind.page, 'already open: nothing changes');
  // Related: right after the parent's tab, wherever the parent is, behind it.
  const related = openRelated(behind.page, target('b'), target('review-of-b'), { background: true }, id);
  assert.deepEqual(tabs(related.page), ['a', 'c', 'b', 'review-of-b']);
  assert.equal(related.page.selection.sessionId, 'a');
  // In front when asked; and the parent in another group: the child joins that group.
  const split: WorkspacePage = { ...page, layout: insertDocked(page.layout, { id: 'side', kind: 'chat', binding: { mode: 'pinned', target: target('z') }, detached: false }, 'right') };
  const child = openRelated(split, target('z'), target('child-of-z'), { background: false }, id);
  assert.equal(groupOf(child.page, child.id)!.id, groupOf(child.page, 'side')!.id);
  assert.ok(front(child.page, child.id));
  assert.equal(child.page.selection.sessionId, 'child-of-z');
  // A parent that is not open: a tab behind, in the chat group.
  const orphan = openRelated(page, target('gone'), target('child'), { background: true }, id);
  assert.equal(groupOf(orphan.page, orphan.id)!.id, chatGroup(page)!.id);
});

test('one tab per conversation: opening one already open brings its tab forward, and duplicates go', () => {
  let page = keepTab(work('a'), chats(work('a'))[0].id);
  page = open(page, 'b', 'tab').page;
  const again = open(page, 'a', 'tab');
  assert.equal(again.added, false);
  assert.deepEqual(tabs(again.page), ['a', 'b']);
  assert.ok(front(again.page, again.id));
  assert.equal(again.page.selection.sessionId, 'a');
  assert.equal(tabFor(again.page, target('b'))!.id, chats(again.page)[1].id);
  // The preview and a regular tab on one conversation (an older saved page): the regular one stays.
  const duplicate = mapPagePanels(open(page, 'c', 'preview').page, panel => panel.preview ? { ...panel, binding: { mode: 'pinned', target: target('b') } } : panel);
  assert.deepEqual(tabs(repairLayout({ version: 2, selectedPage: duplicate.id, pages: [duplicate] }).pages[0]), ['a', 'b']);
});

test('limits: a full page opens nothing, and offers its oldest preview tab', () => {
  let page = work('a');
  for (let n = 0; page.layout && shownPanels(page).length < PANEL_LIMIT; n++) page = open(page, `x${n}`, 'tab').page;
  assert.equal(shownPanels(page).length, PANEL_LIMIT);
  const full = open(page, 'one-more', 'tab');
  assert.equal(full.full, true);
  assert.equal(full.page, page);
  assert.equal(open(page, 'one-more', 'background').full, true);
  // A single click still has the preview tab to show it in.
  assert.equal(open(page, 'one-more', 'preview').full, undefined);
  // A tab opened beside a preview showing a conversation leaves the preview as it is: that is the one offered.
  assert.equal(oldestPreview(page)!.id, 'w-work-chat');
  assert.equal(oldestPreview(keepTab(page, 'w-work-chat')), undefined);
});

test('closing a tab hands the front to its neighbour, whose conversation is selected; Close other tabs and to the right', () => {
  let page = keepTab(work('a'), chats(work('a'))[0].id);
  for (const s of ['b', 'c', 'd']) page = open(page, s, 'tab').page;
  page = open(page, 'b', 'tab').page;
  const b = tabFor(page, target('b'))!;
  const closed = closeTab(page, b.id);
  assert.deepEqual(tabs(closed), ['a', 'c', 'd']);
  assert.equal(closed.selection.sessionId, 'c', 'the tab to its right comes forward');
  const last = closeTab(closed, tabFor(closed, target('d'))!.id);
  assert.equal(last.selection.sessionId, 'c', 'a tab behind closes without moving the selection');
  assert.deepEqual(tabsToClose(page, b.id, 'others').map(panel => resolveTarget(panel, page.selection).sessionId), ['a', 'c', 'd']);
  assert.deepEqual(tabsToClose(page, b.id, 'right').map(panel => resolveTarget(panel, page.selection).sessionId), ['c', 'd']);
  assert.deepEqual(tabsToClose(work(), 'w-work-collections', 'others'), [], 'never the sidebar');
});

test('a chat tab brought forward in the dock selects its conversation', () => {
  let page = keepTab(work('a'), chats(work('a'))[0].id);
  page = open(page, 'b', 'background').page;
  const b = tabFor(page, target('b'))!, group = groupOf(page, b.id)!;
  const clicked = { ...page, layout: page.layout && JSON.parse(JSON.stringify(page.layout).replace(`"active":"${group.active}"`, `"active":"${b.id}"`)) };
  assert.equal(followFrontTab(page, clicked).selection.sessionId, 'b');
  assert.equal(followFrontTab(page, page), page, 'nothing brought forward: unchanged');
});

test('move to new page: a page of its own, named after it, in front; the tab leaves the page it was on', () => {
  let page = keepTab(work('a'), chats(work('a'))[0].id);
  page = open(page, 'b', 'tab').page;
  const layout = { version: 2 as const, selectedPage: page.id, pages: [page] };
  const moved = moveToNewPage(layout, page.id, target('b'), 'Review: fix the\nflaky test', id);
  assert.ok(!('error' in moved));
  if ('error' in moved) return;
  assert.equal(moved.layout.selectedPage, moved.page);
  const created = moved.layout.pages.find(value => value.id === moved.page)!;
  assert.equal(created.name, 'Review: fix the flaky test');
  assert.deepEqual(tabs(created), ['b']);
  assert.equal(created.selection.sessionId, 'b');
  assert.deepEqual(tabs(moved.layout.pages[0]), ['a']);
  assert.doesNotThrow(() => parseLayout(JSON.stringify(moved.layout)));
  const crowded = { ...layout, pages: Array.from({ length: PAGE_LIMIT }, (_, n) => ({ ...page, id: `p${n}` })) };
  assert.ok('error' in moveToNewPage(crowded, 'p0', target('b'), 'B', id));
  assert.equal(pageName('x'.repeat(80)).length, 64);
  assert.equal(pageName('  '), 'Conversation');
});

test('saved and reloaded: regular and preview tabs come back as they were; a chat that followed the page becomes its preview tab', () => {
  let page = keepTab(work('a'), chats(work('a'))[0].id);
  page = open(page, 'b', 'tab').page;
  page = open(page, 'c', 'preview').page;
  const layout = { version: 2 as const, selectedPage: page.id, pages: [page] };
  const reloaded = repairLayout(parseLayout(JSON.stringify(layout)));
  assert.deepEqual(tabs(reloaded.pages[0]), ['a', 'b', 'c P']);
  assert.equal(reloaded.pages[0].selection.sessionId, 'c');
  assert.deepEqual(reloaded, parseLayout(JSON.stringify(layout)), 'loading repairs nothing: no tab is lost or changed');
  assert.deepEqual(chats(reloaded.pages[0]).map(panel => panel.id), chats(page).map(panel => panel.id));
  // Older pages: the chat following the page is the preview tab of what it shows.
  const older = mapPagePanels(work('a'), panel => panel.kind === 'chat' ? { id: panel.id, kind: 'chat', binding: { mode: 'follow' }, detached: false } : panel);
  const once = settleChats(older);
  assert.deepEqual(tabs(once), ['a P']);
  assert.equal(settleChats(once), once, 'settling twice changes nothing');
  const same = defaultLayout(target('a'));
  assert.equal(settleLayout(same), same);
  // Only a chat on the page is a preview tab; a preview mark on anything else is dropped.
  const odd = JSON.parse(JSON.stringify(layout));
  odd.pages[0].layout.children[1].children[1].panels[0].preview = true;
  assert.equal(parseLayout(JSON.stringify(odd)).pages[0].layout && shownPanels(parseLayout(JSON.stringify(odd)).pages[0]).find(panel => panel.kind === 'console')!.preview, undefined);
  const wrong = JSON.parse(JSON.stringify(layout)); wrong.pages[0].layout.children[1].children[0].panels[0].preview = 'yes';
  assert.throws(() => parseLayout(JSON.stringify(wrong)));
});

test('a preset keeps every conversation tab, as tabs of its chat group, with the selected one in front', () => {
  let page = keepTab(work('a'), chats(work('a'))[0].id);
  page = open(page, 'b', 'tab').page;
  page = open(page, 'c', 'background').page;
  const preset = builtinPresets.find(value => value.id !== 'chat-focus' && JSON.stringify(value.layout).includes('chat'))!;
  const applied = applyTemplate(page, preset.layout, id);
  assert.deepEqual(tabs(applied).sort(), ['a', 'b', 'c']);
  const group = groupOf(applied, tabFor(applied, target('b'))!.id)!;
  assert.equal(group.active, tabFor(applied, target('b'))!.id);
  assert.equal(JSON.stringify(templateOf(applied)), JSON.stringify(templateOf(applyTemplate(work('b'), preset.layout, id))), 'its shape is the preset’s: the conversations are not its shape');
});

test('row gestures, the closed tabs remembered, and the status dot', () => {
  const click = { button: 0, metaKey: false, ctrlKey: false, altKey: false, type: 'click' };
  assert.equal(rowGesture(click, true), 'preview');
  assert.equal(rowGesture({ ...click, type: 'dblclick' }, true), 'tab');
  assert.equal(rowGesture({ ...click, metaKey: true }, true), 'background');
  assert.equal(rowGesture({ ...click, ctrlKey: true }, false), 'background');
  assert.equal(rowGesture({ ...click, ctrlKey: true }, true), 'preview', 'on a Mac Ctrl-click is the menu, not a tab');
  assert.equal(rowGesture({ ...click, type: 'auxclick', button: 1 }, true), 'background');
  assert.equal(rowGesture({ ...click, type: 'auxclick', button: 2 }, true), undefined);
  assert.equal(rowGesture({ ...click, altKey: true }, true), undefined, 'Alt-click is Peek');
  let stack = rememberClosed([], { page: 'w:p', target: target('a'), group: 'g', index: 0 });
  stack = rememberClosed(stack, { page: 'w:p', target: target('b'), group: 'g', index: 1 });
  stack = rememberClosed(stack, { page: 'w:other', target: target('c'), group: 'g', index: 0 });
  stack = rememberClosed(stack, { page: 'w:p', target: target('a'), group: 'g', index: 2 });
  assert.deepEqual(stack.map(entry => entry.target.sessionId), ['b', 'c', 'a'], 'the same conversation once, latest last');
  const taken = takeClosed(stack, 'w:p', value => value.sessionId === 'a');
  assert.equal(taken.tab!.target.sessionId, 'b', 'one open again is skipped');
  assert.deepEqual(taken.rest.map(entry => entry.target.sessionId), ['c']);
  assert.equal(takeClosed([], 'w:p', () => false).tab, undefined);
  let many: ReturnType<typeof rememberClosed> = [];
  for (let n = 0; n < 30; n++) many = rememberClosed(many, { page: 'w:p', target: target(`s${n}`), group: 'g', index: 0 });
  assert.equal(many.length, CLOSED_LIMIT);
  assert.deepEqual(tabStatus('needs-input', 'Approval'), { state: 'needs-you', label: 'Needs you: Approval' });
  assert.equal(tabStatus('working', 'Working').state, 'working');
  assert.equal(tabStatus('failed', 'Failed').state, 'failed');
  assert.equal(tabStatus('review', 'Turn ended').state, 'done');
  assert.equal(tabStatus('idle', 'Done').state, 'idle');
});

test('routeChat is a preview open; an empty page gets a chat', () => {
  const empty = { ...work('a'), layout: null };
  const routed = routeChat(empty, target('a'), id);
  assert.equal(routed.added, true);
  assert.deepEqual(tabs(routed.page), ['a P']);
  assert.equal(emptyTarget().sessionId, '');
  const panel: Panel = chats(routed.page)[0];
  assert.equal(panel.binding.mode, 'pinned');
});

test('with tabs on, an older page keeps its conversation as a kept tab, and a review beside it never replaces it', () => {
  // The page as an earlier build saved it: the chat follows the selection.
  const older = mapPagePanels(work('main'), panel => panel.kind === 'chat' ? { id: panel.id, kind: 'chat', binding: { mode: 'follow' }, detached: false } : panel);
  assert.deepEqual(tabs(settleChats(older, false)), ['main'], 'migrated as a kept tab, not a preview');
  // A page saved by the first tabs build (with its preview tab) loads kept too.
  assert.deepEqual(tabs(settleChats(work('main'), false)), ['main']);
  assert.deepEqual(tabs(repairLayout({ version: 2, selectedPage: 'work', pages: [older] }, undefined, false).pages[0]), ['main']);
  assert.deepEqual(tabs(settleLayout({ version: 2, selectedPage: 'work', pages: [work('main')] }, false).pages[0]), ['main']);
  // A related open from a preview parent (tabs off, or an older page): the parent is kept first, then the review joins it.
  const review = openRelated(work('main'), target('main'), target('review'), { background: true }, id);
  assert.deepEqual(tabs(review.page), ['main', 'review']);
  // Showing the review afterwards (its "Open conversation" button) brings its tab forward; the parent stays.
  const shown = open(review.page, 'review', 'preview');
  assert.deepEqual(tabs(shown.page), ['main', 'review']);
  assert.equal(shown.page.selection.sessionId, 'review');
});

test("a workspace's open conversations show as kept tabs, most recent first, up to the cap; a page without a chat is left alone", () => {
  const page = keepTab(work('a'), 'w-work-chat');
  const filled = showWorkspaceTabs(page, ['b', 'a', 'c', 'd'].map(s => target(s)), 3, id);
  assert.deepEqual(tabs(filled), ['a', 'b', 'c'], 'the open one stays first, the others follow, three at most');
  assert.equal(filled.selection.sessionId, 'a', 'the conversation in front stays in front');
  assert.ok(front(filled, 'w-work-chat'));
  assert.equal(showWorkspaceTabs(filled, [target('a'), target('b')], 6, id), filled, 'all there: unchanged');
  // A page still on the workspace's start: its chat takes the most recent conversation.
  const start = keepTab(defaultLayout({ collectionId: 'w', sessionId: '', commandId: '' }).pages[0], 'w-work-chat');
  const started = showWorkspaceTabs(start, [target('x'), target('y')], 6, id);
  assert.deepEqual(tabs(started), ['x', 'y']);
  assert.equal(started.selection.sessionId, 'x');
  const empty = { ...work('a'), layout: removeFromPage(work('a'), 'w-work-chat').layout };
  assert.equal(showWorkspaceTabs(empty, [target('b')], 6, id), empty);
});

test('a deleted conversation leaves every page; the sidebar docks left', () => {
  let page = keepTab(work('a'), 'w-work-chat');
  page = open(page, 'b', 'tab').page;
  const other = { ...open(keepTab(work('b'), 'w-work-chat'), 'c', 'tab').page, id: 'more' };
  const layout = { version: 2 as const, selectedPage: page.id, pages: [page, other] };
  const gone = forgetConversation(layout, 'b');
  assert.deepEqual(gone.pages.map(value => tabs(value)), [['a'], ['c']]);
  assert.equal(gone.pages[0].selection.sessionId, 'a', 'the tab beside it comes forward, selected');
  assert.equal(forgetConversation(gone, 'b'), gone, 'nothing left: unchanged');
  assert.equal(forgetConversation(gone, ''), gone);
  assert.equal(defaultDock('collections'), 'left');
  assert.equal(defaultDock('agents'), 'left');
  assert.equal(defaultDock('terminal'), 'right');
});

test('which conversations a workspace opens as tabs, the closed ones remembered, and how an open reads with tabs on', () => {
  const at = (n: number) => String(1_700_000_000_000 + n * 1000);
  const session = (sessionId: string, extra: Record<string, unknown> = {}) => ({ id: sessionId, collectionId: 'w', purpose: '', closed: false, successorExecutionId: '', pending: [], status: 'idle', archivedAtUnixMillis: '0', createdAtUnixMillis: at(1), lastActivityAtUnixMillis: at(1), agents: [], ...extra }) as unknown as Session;
  const list = [
    session('old'), session('new', { lastActivityAtUnixMillis: at(9), createdAtUnixMillis: at(9) }),
    session('ended', { closed: true }), session('help', { purpose: 'help' }), session('before', { successorExecutionId: 'new' }),
    session('elsewhere', { collectionId: 'other' }), session('closed-tab'),
  ];
  assert.deepEqual(workspaceConversations(list, 'w', new Set(['closed-tab'])).map(t => t.sessionId), ['new', 'old']);
  assert.deepEqual(dismissedWith(null, 'a', true), ['a']);
  assert.deepEqual(dismissedWith('["a","b"]', 'a', false), ['b']);
  assert.deepEqual(dismissedWith('not json', 'c', true), ['c']);
  assert.equal(openModeFor('preview', true), 'tab');
  assert.equal(openModeFor('preview', false), 'preview');
  assert.equal(openModeFor('background', true), 'background');
  assert.equal(tabsSettingOn(null), true, 'on unless turned off');
  assert.equal(tabsSettingOn('off'), false);
});
