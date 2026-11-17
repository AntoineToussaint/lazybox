import test from 'node:test';
import assert from 'node:assert/strict';
import { budgetLine, defaultCap, lanesOf, messageThreads, overlapsOf, overlapsOfTask, pendingPlans, planOrder, reviewHeld, ceilingLine, childState, defaultGrant, effectivePermission, grantOf, grantProblem, permissionChoices, profileLabel } from '../packages/surfaces/src/orchestration-model';
import { emptyTranscript, foldTranscript } from '../packages/surfaces/src/transcript-model';
import { systemEventView } from '../packages/surfaces/src/system-events';
import { lineageLabel, nestByLineage } from '../packages/surfaces/src/agents-model';

test('a grant offers at most the conversation’s own permission; inherit counts as ask', () => {
  assert.equal(effectivePermission('inherit'), 'ask');
  assert.equal(effectivePermission(''), 'ask');
  assert.deepEqual(permissionChoices('read-only').map(c => !!c.disabled), [false, true, true]);
  assert.deepEqual(permissionChoices('yolo').map(c => !!c.disabled), [false, false, false]);
  assert.match(permissionChoices('ask')[2].disabled!, /runs at Ask; its agents cannot get more/);
});

test('a grant defaults to the conversation’s own subscriptions and says what is missing', () => {
  const draft = defaultGrant({ permission: 'yolo', profileId: 'claude-default', rotationProfileIds: ['claude-work', 'claude-default'] } as never);
  assert.deepEqual(draft, { permission: 'yolo', profileIds: ['claude-default', 'claude-work'], maxChildren: 3, budgetUsd: 0, planRequired: true });
  assert.equal(grantProblem(draft), undefined);
  assert.match(grantProblem({ ...draft, profileIds: [] })!, /at least one subscription/);
  assert.match(grantProblem({ ...draft, maxChildren: 40 })!, /Between 1 and 16/);
  assert.equal(ceilingLine({ ceiling: { permission: 'ask', profileIds: [], maxChildren: 3, depth: 1, budgetUsd: 20 } } as never), 'Up to 3 at once · Ask or narrower · $20 cap');
  assert.equal(profileLabel([{ id: 'codex-saved', provider: 'codex', label: 'Work' }], 'codex-saved'), 'Codex · Work');
});

test('the live grant a conversation holds, and how its agents read', () => {
  const grant = { id: 'gr', live: true, conversationId: 'orch' }, gone = { id: 'old', live: false, conversationId: 'orch' };
  const missions = [{ id: 'm', grants: [gone, grant] }] as never;
  assert.equal(grantOf(missions, 'orch')?.grant.id, 'gr');
  assert.equal(grantOf(missions, 'other'), undefined);
  const task = (status: string, gate = '') => ({ status, gate }) as never;
  const attempt = (live: string, ended = 0n, outcome = '') => ({ live, endedAtUnixMillis: ended, outcome }) as never;
  assert.deepEqual(childState(task('working'), attempt('running')), { label: 'Working', tone: 'run' });
  assert.deepEqual(childState(task('needs_you', 'reviewed'), attempt('idle')), { label: 'Reported · needs you', tone: 'wait' });
  assert.deepEqual(childState(task('needs_you'), attempt('waiting')), { label: 'Needs you', tone: 'wait' });
  assert.deepEqual(childState(task('ready'), attempt('closed', 5n, 'stopped')), { label: 'Stopped', tone: 'off' });
  assert.deepEqual(childState(task('done'), attempt('idle')), { label: 'Accepted', tone: 'done' });
});

test('the transcript shows another agent’s message as theirs, and the tree’s lines as lines', () => {
  const data = (value: unknown) => new TextEncoder().encode(JSON.stringify(value));
  const records = [
    { cursor: 1n, event: { type: 'operation_accepted', text: 'Do it', turnId: 't1', operationId: 'op1', providerDataJson: data({ files: [] }) } },
    { cursor: 2n, event: { type: 'operation_accepted', text: '[Lazybox] Your agent “Indexer” writes:\n\n> done', turnId: 't2', operationId: 'agent-msg_1', providerDataJson: data({ origin: { kind: 'agent_note', summary: 'Indexer: done', detail: 'your agent', from: 'Indexer', conversation: 'c1', session: 's1' } }) } },
    { cursor: 3n, event: { type: 'agent_spawned', text: 'Started “Indexer” · worker · ask · in this workspace', turnId: '', providerDataJson: data({ conversation: 'c1', session: 's1', mission: 'm' }) } },
  ] as never;
  const folded = foldTranscript(emptyTranscript(), records);
  assert.deepEqual(folded.messages.map(m => m.type), ['operation_accepted', 'operation_accepted', 'lineage']);
  assert.equal(folded.messages[0].origin, undefined, 'the person’s own message has no origin');
  assert.deepEqual(folded.messages[1].origin, { kind: 'agent_note', summary: 'Indexer: done', detail: 'your agent', copy: '', from: 'Indexer', conversation: 'c1', session: 's1' });
  const view = systemEventView(folded.messages[1].origin!, 'Claude');
  assert.equal(view.kind, 'Message from an agent');
  assert.equal(view.line, 'Indexer: done');
  assert.equal(view.sentBy, 'Indexer’s message, as Lazybox gave it to Claude');
  assert.deepEqual(folded.messages[2].lineage, { kind: 'agent_spawned', conversation: 'c1', session: 's1', mission: 'm', direction: '', plan: '' });
});

test('a plan reads in the order its tasks can start, with the paths two share', () => {
  const task = (key: string, after: string[] = []) => ({ key, after }) as never;
  assert.deepEqual(planOrder([task('rank', ['index']), task('index'), task('docs'), task('ship', ['rank', 'docs'])]).map(({ task, step }) => `${(task as { key: string }).key}:${step}`), ['index:1', 'docs:1', 'rank:2', 'ship:3']);
  const overlaps = [{ path: 'search/rank.go', keys: ['index', 'rank'] }] as never;
  assert.deepEqual(overlapsOf(overlaps, 'index', 'search/'), ['rank']);
  assert.deepEqual(overlapsOf(overlaps, 'rank', 'search/rank.go'), ['index']);
  assert.deepEqual(overlapsOf(overlaps, 'docs', 'docs/'), []);
  assert.equal(defaultCap(8.5, 0), 11);
  assert.equal(defaultCap(8.5, 20), 20);
  assert.equal(defaultCap(0, 0), 0);
  assert.equal(budgetLine({ spentUsd: 9.4, reservedUsd: 3, ceiling: { budgetUsd: 12 } } as never), '$9.40 spent · $3 reserved · $12 cap');
  assert.equal(budgetLine({ spentUsd: 0, reservedUsd: 0, ceiling: { budgetUsd: 0 } } as never), '$0 spent · no cap');
  const missions = [{ canceled: false, plans: [{ id: 'p1', state: 'proposed' }, { id: 'p0', state: 'approved' }] }, { canceled: true, plans: [{ id: 'p2', state: 'proposed' }] }] as never;
  assert.deepEqual(pendingPlans(missions).map(p => p.plan.id), ['p1']);
});

test('the timeline has a lane per actor; messages read as threads; holds and overlaps are said', () => {
  const mission = {
    grants: [{ id: 'g', live: true, conversationId: 'orch', title: 'Search lead', ceiling: { reviewLimit: 2 } }],
    tasks: [{ id: 't1', title: 'Index', attempts: [{ conversationId: 'c1', parentConversationId: 'orch', title: '' }] }, { id: 't2', title: 'Rank results', attempts: [{ conversationId: 'c2', parentConversationId: 'orch', title: 'Rank results' }] }],
    overlaps: [{ taskIds: ['t1', 't2'], paths: ['search/rank.go'], kind: 'files' }],
    awaitingReview: 2,
  } as never;
  const event = (actorKind: string, conversationId = '') => ({ actorKind, conversationId }) as never;
  const lanes = lanesOf(mission, [event('agent', 'stray')]);
  assert.deepEqual(lanes.map(l => `${l.short}:${l.tone}`), ['You:person', 'SL:orchestrator', 'I:agent', 'RR:agent', 'A:agent', 'LB:system']);
  assert.equal(lanes.findIndex(l => l.match(event('human'))), 0);
  assert.equal(lanes.findIndex(l => l.match(event('agent', 'c2'))), 3);
  assert.equal(lanes.findIndex(l => l.match(event('rule'))), 5);
  const message = (seq: number, from: string, to: string, at: number) => ({ id: `m${seq}`, seq: BigInt(seq), fromConversationId: from, toConversationId: to, fromLabel: from, toLabel: to, atUnixMillis: BigInt(at) }) as never;
  const threads = messageThreads([message(3, 'orch', 'c2', 30), message(1, 'orch', 'c1', 10), message(2, 'c1', 'orch', 20)]);
  assert.deepEqual(threads.map(t => [t.a.conversation, t.b.conversation, t.messages.length]), [['orch', 'c2', 1], ['orch', 'c1', 2]]);
  assert.deepEqual(overlapsOfTask(mission, 't2'), [{ other: 'Index', paths: ['search/rank.go'], kind: 'files' }]);
  assert.deepEqual(reviewHeld(mission), { waiting: 2, limit: 2 });
  assert.equal(reviewHeld({ ...(mission as object), awaitingReview: 1 } as never), undefined);
});

test('the Agents list nests an orchestrator’s agents under it, and says who started one shown apart', () => {
  const row = (id: string, parent = '') => ({ key: id, session: { id, conversationId: id, lineage: parent ? { parentConversationId: parent, parentTitle: 'Search lead', missionTitle: 'Ship search', role: 'worker' } : undefined } }) as never;
  const nested = nestByLineage([row('c1', 'orch'), row('other'), row('orch'), row('c2', 'orch'), row('lost', 'gone')]);
  assert.deepEqual(nested.map(({ row, depth }) => `${(row as { key: string }).key}:${depth}`), ['other:0', 'orch:0', 'c1:1', 'c2:1', 'lost:0']);
  assert.equal(lineageLabel((row('c1', 'orch') as { session: never }).session), 'started by Search lead · Ship search');
  assert.equal(lineageLabel((row('orch') as { session: never }).session), '');
});
