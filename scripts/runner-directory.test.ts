// Several Runners in one window, the pure half (runner-directory.ts): which
// Runners a window lists and in what order, their health, the merged inbox
// (each row keyed and routed to its own Runner), targets that switch Runner,
// and the Agents sections when two Runners share workspace ids.
import { create } from '@bufbuild/protobuf';
import { AgentEventSchema, SessionSchema, type Session } from '@lazybox/protocol';
import type { RunnerListing } from '@lazybox/platform';
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { elsewhere, groupByMachine, healthLabel, isRemote, launchRunner, linkStateText, routeLabel, rttText, runnerStateText, THIS_MACHINE, thisMachineLabel, localTarget, mergeInboxes, orderRunners, OWN_RUNNER, parseRunnerPreferences, rowTarget, runnerChannel, runnerCounts, runnerHealth, runnerLabel, scopedKey, shortPath, splitKey, validRunnerId } from '../packages/surfaces/src/runner-directory.ts';
import { agentSections } from '../packages/surfaces/src/agents-model.ts';
import { parsePeek, peekKey } from '../packages/surfaces/src/peek-model.ts';
import { listingKey } from '../packages/surfaces/src/runner-locator.ts';
import { validTarget } from '../packages/surfaces/src/workspace-model.ts';

function session(id: string, init: Partial<Session> & { updated?: number } = {}): Session {
  const { updated = 1_000, ...rest } = init;
  return create(SessionSchema, { id, conversationId: id, collectionId: 'alpha', title: `Title ${id}`, status: 'ready', createdAtUnixMillis: 1_000n, agents: [{ id: 'root', status: 'done', updatedAtUnixMillis: BigInt(updated) }], ...rest });
}
const question = create(AgentEventSchema, { type: 'question', requestId: 'q1' });
const listing = (id: string, init: Partial<RunnerListing> = {}): RunnerListing => ({ id, name: id, source: 'registered', current: false, state: 'running', url: `http://127.0.0.1:1/${id}`, token: `t-${id}`, ...init });
const online = () => 'online' as const;

test('preferences keep valid ids only, once, and never the window\'s own', () => {
  assert.deepEqual(parseRunnerPreferences(null), { hidden: [], primary: '' });
  assert.deepEqual(parseRunnerPreferences('not json'), { hidden: [], primary: '' });
  assert.deepEqual(parseRunnerPreferences('[]'), { hidden: [], primary: '' });
  assert.deepEqual(parseRunnerPreferences(JSON.stringify({ hidden: ['a', 'a', 'b/c', 7, OWN_RUNNER, 'ok-1.x'], primary: OWN_RUNNER })), { hidden: ['a', 'ok-1.x'], primary: '' });
  assert.deepEqual(parseRunnerPreferences(JSON.stringify({ hidden: [], primary: 'working' })), { hidden: [], primary: 'working' });
  assert.equal(validRunnerId('../x'), false);
  assert.equal(validRunnerId('3f2a9c0d1e4b5a6c'), true);
});

test('health: stopped and invalid come from the listing; the connection says the rest', () => {
  assert.equal(runnerHealth({ state: 'stopped' }, undefined), 'stopped');
  assert.equal(runnerHealth({ state: 'invalid' }, undefined), 'invalid');
  assert.equal(runnerHealth({ state: 'running' }, undefined), 'connecting');
  assert.equal(runnerHealth({ state: 'running' }, { connecting: true, error: '' }), 'connecting');
  assert.equal(runnerHealth({ state: 'running' }, { connecting: false, error: 'unreachable' }), 'offline');
  assert.equal(runnerHealth({ state: 'running' }, { connecting: false, error: 'unreachable', info: {} as never }), 'offline');
  assert.equal(runnerHealth(undefined, { connecting: false, error: '', info: {} as never }), 'online');
  assert.equal(healthLabel.offline, 'Not reachable');
});

test('names: the launchers\' channels read as Dev, Working and Release; others keep their own', () => {
  assert.equal(runnerLabel({ name: 'dev' }), 'Dev');
  assert.equal(runnerLabel({ name: 'working' }), 'Working');
  assert.equal(runnerLabel({ name: 'lazybox' }), 'Release');
  assert.equal(runnerLabel({ name: 'checkout-two' }), 'Checkout-two');
  assert.equal(runnerLabel({ name: ' ', product: 'Lazybox Lab' }), 'Lazybox Lab');
  assert.equal(runnerLabel({ name: 'build-box', remote: { host: 'build-box', route: 'ssh' } }), 'build-box');
  assert.equal(runnerChannel({ name: 'dev' }), 'Dev');
  assert.equal(runnerChannel({ name: 'mine', channel: 'working' }), 'Working');
  assert.equal(runnerChannel({ name: 'mine' }), '');
  assert.equal(shortPath('/Users/me/src/two/v2/.lazybox/endpoint.json'), '…/.lazybox/endpoint.json');
});

test('order: the primary, then the window\'s own, then running, then the rest; the own listing folds into the window', () => {
  const listings = [listing('zeta'), listing('dev', { current: true }), listing('alpha', { state: 'stopped', url: undefined, token: undefined }), listing('working')];
  const rows = orderRunners(listings, { hidden: ['zeta'], primary: '' }, online);
  assert.deepEqual(rows.map(row => row.id), [OWN_RUNNER, 'working', 'zeta', 'alpha']);
  assert.equal(rows[0].label, 'Dev');
  assert.equal(rows[0].own, true);
  assert.equal(rows[0].primary, true);
  assert.equal(rows.find(row => row.id === 'zeta')!.hidden, true);
  const primary = orderRunners(listings, { hidden: [], primary: 'working' }, online);
  assert.deepEqual(primary.map(row => row.id).slice(0, 2), ['working', OWN_RUNNER]);
  assert.equal(primary.find(row => row.own)!.primary, false);
  // A primary that is no longer listed falls back to the window's own.
  assert.equal(orderRunners(listings, { hidden: [], primary: 'gone' }, online)[0].id, OWN_RUNNER);
  // Without a listing for it, the window's own Runner is still there.
  assert.deepEqual(orderRunners([], { hidden: [], primary: '' }, online, 'This Mac').map(row => [row.id, row.label]), [[OWN_RUNNER, 'This Mac']]);
  // Two checkouts' Dev are told apart by where they were found; an id is listed once.
  const twins = orderRunners([listing('a', { name: 'dev', locator: '/src/one/runners/dev.json' }), listing('b', { name: 'dev', locator: '/src/two/runners/dev.json' }), listing('b')], { hidden: [], primary: '' }, online);
  assert.deepEqual(twins.filter(row => !row.own).map(row => row.label), ['Dev · …/runners/dev.json', 'Dev · …/runners/dev.json']);
  assert.equal(twins.length, 3);
});

test('the window opens on its primary only while that Runner runs and is shown', () => {
  const running = orderRunners([listing('working')], { hidden: [], primary: 'working' }, online);
  assert.equal(launchRunner(running), 'working');
  assert.equal(launchRunner(orderRunners([listing('working', { state: 'stopped' })], { hidden: [], primary: 'working' }, online)), OWN_RUNNER);
  assert.equal(launchRunner(orderRunners([listing('working')], { hidden: ['working'], primary: 'working' }, online)), OWN_RUNNER);
  assert.equal(launchRunner(orderRunners([listing('working')], { hidden: [], primary: '' }, online)), OWN_RUNNER);
});

test('the merged inbox: most urgent first across Runners, other Runners\' rows keyed under them', () => {
  const dev = [session('alpha-session', { status: 'running', busy: true, updated: 5_000 }), session('beta-session', { status: 'waiting_input', pending: [question], updated: 2_000 })];
  const working = [session('alpha-session', { status: 'waiting_input', pending: [question], updated: 9_000 }), session('old', { updated: 100 })];
  const seen = { since: 10_000, marks: {} };
  const inbox = mergeInboxes([{ runner: OWN_RUNNER, sessions: dev, seen }, { runner: 'working', sessions: working, seen }], OWN_RUNNER);
  assert.deepEqual(inbox.rows.map(row => [row.runner, row.key, row.state]), [
    ['working', scopedKey('working', 'alpha-session'), 'needs-input'],
    [OWN_RUNNER, 'beta-session', 'needs-input'],
    [OWN_RUNNER, 'alpha-session', 'working'],
    ['working', scopedKey('working', 'old'), 'idle'],
  ]);
  assert.equal(new Set(inbox.rows.map(row => row.key)).size, 4, 'one conversation id on two Runners is two rows');
  assert.equal(inbox.counts['needs-input'], 2);
  assert.equal(inbox.byRunner.working['needs-input'], 1);
  assert.deepEqual(runnerCounts(inbox.byRunner[OWN_RUNNER]), { waiting: 1, review: 0 });
  assert.deepEqual(runnerCounts(undefined), { waiting: 0, review: 0 });
  assert.deepEqual(inbox.groups.map(group => [group.state, group.rows.length]), [['needs-input', 2], ['working', 1], ['idle', 1]]);
  // Seen marks are each Runner's own: marking Dev's alpha-session does not mark Working's.
  const ended = [session('alpha-session', { agents: [{ id: 'root', status: 'done', updatedAtUnixMillis: 9_000n, turnEndedAtUnixMillis: 9_000n }] as never })];
  const marked = mergeInboxes([{ runner: OWN_RUNNER, sessions: ended, seen: { since: 0, marks: { 'alpha-session': 9_000 } } }, { runner: 'working', sessions: ended, seen: { since: 0, marks: {} } }], OWN_RUNNER);
  assert.deepEqual(marked.rows.map(row => [row.runner, row.state]).sort(), [[OWN_RUNNER, 'idle'], ['working', 'review']]);
  // From the other Runner's page, Dev's rows are the scoped ones.
  const there = mergeInboxes([{ runner: OWN_RUNNER, sessions: dev, seen }, { runner: 'working', sessions: working, seen }], 'working');
  assert.equal(there.rows[0].key, 'alpha-session');
  assert.equal(there.rows[1].key, scopedKey(OWN_RUNNER, 'beta-session'));
});

test('keys and targets route to the Runner they name; layouts never store one', () => {
  assert.deepEqual(splitKey(scopedKey('working', 'abc'), OWN_RUNNER), { runner: 'working', key: 'abc' });
  assert.deepEqual(splitKey('abc', 'working'), { runner: 'working', key: 'abc' });
  const target = rowTarget({ runner: 'working', session: session('s1', { collectionId: 'beta' }) });
  assert.deepEqual(target, { collectionId: 'beta', sessionId: 's1', commandId: '', runner: 'working' });
  assert.equal(elsewhere(target, OWN_RUNNER), true);
  assert.equal(elsewhere(target, 'working'), false);
  assert.equal(elsewhere({ collectionId: 'a', sessionId: '', commandId: '' }, 'working'), false);
  assert.equal(validTarget(target), false, 'a target naming a Runner is not a layout target');
  assert.equal(validTarget(localTarget(target)), true);
});

test('Agents: two Runners\' workspaces with one id are two sections; by Runner groups by Runner', () => {
  const seen = { since: 0, marks: {} };
  const inbox = mergeInboxes([{ runner: OWN_RUNNER, sessions: [session('a1'), session('a2', { collectionId: 'beta' })], seen }, { runner: 'working', sessions: [session('w1')], seen }], OWN_RUNNER);
  const byWorkspace = agentSections(inbox, 'workspace', row => `${row.runner}/${row.session.collectionId}`);
  assert.deepEqual(byWorkspace.map(section => section.title).sort(), [`${OWN_RUNNER}/alpha`, `${OWN_RUNNER}/beta`, 'working/alpha']);
  const byRunner = agentSections(inbox, 'runner', () => '', '', row => row.runner === 'working' ? 'Working' : 'Dev');
  assert.deepEqual(byRunner.map(section => [section.title, section.rows.length]), [['Dev', 2], ['Working', 1]]);
});

test('peek: a conversation names its Runner, and one id on two Runners is two items', () => {
  const item = parsePeek(JSON.stringify({ kind: 'conversation', sessionId: 's1', runner: 'working' }));
  assert.deepEqual(item, { kind: 'conversation', sessionId: 's1', runner: 'working' });
  assert.notEqual(peekKey(item!), peekKey({ kind: 'conversation', sessionId: 's1' }));
});

test('a listing that says the same thing is the same value (no rerender on every poll)', () => {
  assert.equal(listingKey([listing('a')]), listingKey([{ ...listing('a') }]));
  assert.notEqual(listingKey([listing('a')]), listingKey([listing('a', { token: 'rotated' })]));
  // A remote Runner's link changing state (or its round trip) is a new value.
  const remote = (state: 'connected' | 'reconnecting', rttMs = 40) => listing('box', { remote: { host: 'build-box', route: 'ssh', machine: 'build-box', state, rttMs } });
  assert.equal(listingKey([remote('connected')]), listingKey([remote('connected')]));
  assert.notEqual(listingKey([remote('connected')]), listingKey([remote('reconnecting')]));
  assert.notEqual(listingKey([remote('connected', 40)]), listingKey([remote('connected', 400)]));
});

test('Runners on other computers: the link in words, and the sidebar grouped by computer', () => {
  const remote = (name: string, extra: Partial<NonNullable<RunnerListing['remote']>> = {}, state: RunnerListing['state'] = 'running') =>
    listing(name, { state, remote: { host: name, route: 'ssh', machine: name, state: 'connected', rttMs: 30, ...extra } });
  assert.equal(linkStateText(listing('dev')), '');
  assert.equal(linkStateText(remote('box')), '');
  assert.equal(linkStateText(remote('box', { state: 'reconnecting' })), 'Reconnecting…');
  assert.equal(linkStateText(remote('box', { state: 'refused', code: 'unknown-key' })), 'Access revoked');
  assert.equal(linkStateText(remote('box', { state: 'refused', code: 'wrong-runner' })), 'Not its Runner');
  // The link agent is not serving yet: the app is starting it.
  assert.equal(linkStateText(remote('box', {}, 'stopped')), 'Connecting…');
  assert.equal(runnerStateText({ health: 'offline', listing: remote('box', { state: 'reconnecting' }) }), 'Reconnecting…');
  assert.equal(runnerStateText({ health: 'offline', listing: listing('working') }), healthLabel.offline);
  assert.equal(isRemote(remote('box')), true);
  assert.equal(isRemote(listing('dev')), false);
  assert.equal(rttText(41.6), '42 ms');
  assert.equal(rttText(undefined), '');
  assert.equal(routeLabel('ssh'), 'SSH');

  const rows = [{ listing: listing('dev') }, { listing: remote('zeta', { machine: 'zeta' }) }, { listing: listing('working') }, { listing: remote('alpha-gpu', { machine: 'alpha', state: 'reconnecting', rttMs: 500 }) }];
  const machines = groupByMachine(rows, 'This Mac');
  assert.deepEqual(machines.map(m => [m.label, m.remote, m.rows.length]), [['This Mac', false, 2], ['alpha', true, 1], ['zeta', true, 1]]);
  assert.deepEqual([machines[1].route, machines[1].rttMs, machines[1].state], ['ssh', 500, 'Reconnecting…']);
  assert.equal(machines[0].id, THIS_MACHINE);
  // Only remote ones: no empty "This Mac".
  assert.deepEqual(groupByMachine([{ listing: remote('box') }]).map(m => m.label), ['box']);
  assert.equal(thisMachineLabel('MacIntel'), 'This Mac');
  assert.equal(thisMachineLabel('Linux x86_64'), 'This computer');
});
