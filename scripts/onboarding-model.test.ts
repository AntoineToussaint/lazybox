import { test } from 'node:test';
import assert from 'node:assert/strict';
import { essentialKeys, keyParts, releaseGroups, welcomeDecision, whatsNewDecision, type Release } from '../packages/surfaces/src/onboarding-model.ts';
import { keyboardPreset } from '../packages/surfaces/src/keyboard.ts';
import { displayBinding } from '../packages/surfaces/src/actions.ts';
import { releaseNotes } from '../packages/surfaces/src/release-notes.ts';

const fresh = { seen: false, workspacesLoaded: true, workspaces: 0, conversationsLoaded: true, conversations: 0 };

test('the welcome shows on a fresh install only, once', () => {
  assert.equal(welcomeDecision(fresh), 'show');
  assert.equal(welcomeDecision({ ...fresh, seen: true }), 'none');
  // An existing user: recorded silently, whichever list answers first.
  assert.equal(welcomeDecision({ ...fresh, workspaces: 2 }), 'record');
  assert.equal(welcomeDecision({ ...fresh, conversations: 1 }), 'record');
  assert.equal(welcomeDecision({ ...fresh, conversationsLoaded: false, workspaces: 1 }), 'record');
  assert.equal(welcomeDecision({ ...fresh, workspacesLoaded: false, conversations: 3 }), 'record');
  // An absent answer is not "empty".
  assert.equal(welcomeDecision({ ...fresh, workspacesLoaded: false }), 'wait');
  assert.equal(welcomeDecision({ ...fresh, conversationsLoaded: false }), 'wait');
  assert.equal(welcomeDecision({ ...fresh, seen: true, workspacesLoaded: false }), 'none');
});

const release = (id: string): Release => ({ id, date: '2026-11-16', title: `Release ${id}`, items: [{ title: 'Item', body: 'Body' }] });
const releases = [release('c'), release('b'), release('a')];

test('What is new: recorded silently the first time, shown once per newer release', () => {
  assert.deepEqual(whatsNewDecision(null, releases), { action: 'record', id: 'c' });
  assert.deepEqual(whatsNewDecision('', releases), { action: 'record', id: 'c' });
  assert.deepEqual(whatsNewDecision('c', releases), { action: 'none' });
  assert.deepEqual(whatsNewDecision('b', releases), { action: 'show', id: 'c', releases: [releases[0]] });
  assert.deepEqual(whatsNewDecision('a', releases), { action: 'show', id: 'c', releases: [releases[0], releases[1]] });
  // An id this build does not know (pruned notes, a downgrade): the newest only.
  assert.deepEqual(whatsNewDecision('zz', releases), { action: 'show', id: 'c', releases: [releases[0]] });
  assert.deepEqual(whatsNewDecision('a', []), { action: 'none' });
});

test('the bundled release notes are well formed, newest first, with unique ids', () => {
  assert.ok(releaseNotes.length > 0);
  const ids = releaseNotes.map(r => r.id);
  assert.equal(new Set(ids).size, ids.length);
  for (const r of releaseNotes) {
    assert.match(r.id, /^[A-Za-z0-9][A-Za-z0-9._-]*$/);
    assert.match(r.date, /^\d{4}-\d{2}-\d{2}$/);
    assert.ok(r.title && r.items.length > 0 && r.items.every(i => i.title && i.body));
  }
  const dates = releaseNotes.map(r => r.date);
  assert.deepEqual(dates, [...dates].sort().reverse());
});

test('a big release shows its items under their groups, in order', () => {
  assert.deepEqual(releaseGroups([{ title: 'a', body: '' }]), [{ group: '', items: [{ title: 'a', body: '' }] }]);
  const items = [{ group: 'Chat', title: 'a', body: '' }, { group: 'Chat', title: 'b', body: '' }, { group: 'Review', title: 'c', body: '' }];
  assert.deepEqual(releaseGroups(items).map(g => [g.group, g.items.map(i => i.title)]), [['Chat', ['a', 'b']], ['Review', ['c']]]);
  // Every item of a grouped release is in a group, so no heading is ever missing.
  for (const release of releaseNotes) {
    if (release.items.some(i => i.group)) assert.ok(release.items.every(i => i.group), release.id);
  }
});

test('the five keys come from the person’s own keymap', () => {
  assert.deepEqual(keyParts(' ] ]  c '), [']', ']', 'c']);
  const vim = essentialKeys(keyboardPreset('vim', true), '⌘P', binding => displayBinding(binding, true));
  assert.equal(vim.length, 5);
  assert.deepEqual(vim.map(k => k.label), ['Leave the agent', 'New task', 'Go to file', 'Inbox', 'Next / previous']);
  assert.deepEqual(vim[0].keys, [[']', ']']]);
  assert.deepEqual(vim[1].keys, [['⌘N']]);
  assert.deepEqual(vim[2].keys, [['⌘P']]);
  assert.deepEqual(vim[4].keys, [['j'], ['k']]);
  const tmux = essentialKeys(keyboardPreset('tmux'), 'Ctrl+P');
  assert.deepEqual(tmux[3].keys, [['Ctrl+b', 'i']]);
  assert.deepEqual(tmux[4].keys, [['Ctrl+b', '↓'], ['Ctrl+b', '↑']]);
  const arrows = essentialKeys(keyboardPreset('arrows'), '⌘P');
  assert.deepEqual(arrows[4].keys, [['↓'], ['↑']]);
});
