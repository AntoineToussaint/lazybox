import { test } from 'node:test';
import assert from 'node:assert/strict';
import { formatBudget, processCards } from '../packages/surfaces/src/orchestration-process-model.ts';

// A reviewed-team process as the Runner sends it (abridged, same shape).
const reviewed = JSON.stringify({
  name: 'reviewed-team', autonomy: 4,
  budgets: { mission: { usd: 60, wall: '10h' }, task: { usd: 12, turns: 120, retries: 2 } },
  phases: [{ id: 'plan', exit: 'contract' }, { id: 'build', exit: 'all_verified' }],
  gates: { contract: { kind: 'human', show: ['brief', 'task_graph'] }, all_verified: { kind: 'rule', when: "mission.assertions.all(a, a.verdict == 'pass')" } },
  roles: {
    coordinator: { provider: 'claude', model: 'strongest', effort: 'high', permission: 'read_only', scopes: ['goal.*'], triggers: [{ on: 'mission.created' }, { on: ['task.question', 'task.blocked'] }], budget: { usd: 8 } },
    verifier: { provider: 'other_than_worker', permission: 'read_only', scopes: ['verdict.record'] },
    transition: { provider: 'claude', triggers: [{ on: 'timer', every: '30m' }, { on: 'stall', after: '20m' }, { on: 'gate.requested', if: "gate.id == 'x'", mode: 'manual' }] },
  },
  rules: [{}, {}, {}],
});

test('phases read left to right with the gate that ends each', () => {
  const cards = processCards(reviewed)!;
  assert.equal(cards.name, 'reviewed-team');
  assert.equal(cards.budget, '$60 · 10h');
  assert.equal(cards.rules, 3);
  assert.deepEqual(cards.phases, [
    { id: 'plan', exit: 'contract', gate: 'human', detail: 'you check brief, task graph' },
    { id: 'build', exit: 'all_verified', gate: 'rule', detail: "mission.assertions.all(a, a.verdict == 'pass')" },
  ]);
});

test('roles keep file order and say who they are, what they may do and what wakes them', () => {
  const roles = processCards(reviewed)!.roles;
  assert.deepEqual(roles.map(r => r.name), ['coordinator', 'verifier', 'transition']);
  assert.deepEqual(roles[0], { name: 'coordinator', providers: 'claude', model: 'strongest · high', permission: 'Read only', scopes: ['goal.*'], wakes: ['on mission.created', 'on task.question, task.blocked'], budget: '$8' });
  assert.equal(roles[1].providers, "not worker's");
  assert.equal(roles[2].permission, 'Read only', 'an unset permission is read only');
  assert.deepEqual(roles[2].wakes, ['on timer every 30m', 'on stall after 20m', 'offered on gate.requested if …']);
});

test('budgets and bad input', () => {
  assert.equal(formatBudget({ usd: 10, turns: 80, retries: 1 }), '$10 · 80 turns · 1 retry');
  assert.equal(formatBudget(), '');
  assert.equal(processCards('not json'), undefined);
  assert.equal(processCards(''), undefined);
});
