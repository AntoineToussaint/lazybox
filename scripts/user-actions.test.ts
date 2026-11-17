import { test } from 'node:test';
import assert from 'node:assert/strict';
import { create } from '@bufbuild/protobuf';
import { UserActionSchema, UserActionStepSchema, type UserActionStep } from '../packages/protocol/src/gen/api_pb';
import { actionSummary, resolveConversation, resolveWorkspaceAction, runUserAction, stepLabel, type ActionRuntime, type StepOutcome } from '../packages/surfaces/src/user-actions';
import { keyboardPreset, leaderConflicts } from '../packages/surfaces/src/keyboard';

const step = (init: Partial<UserActionStep>) => create(UserActionStepSchema, init);
const action = (steps: Partial<UserActionStep>[]) => create(UserActionSchema, { id: 'a', title: 'A', where: ['menu'], steps: steps.map(step) });

/** A runtime that records every call; outcomes come from the table. */
function fake(outcomes: Record<string, StepOutcome | Error> = {}) {
  const calls: string[] = [];
  const out = (key: string): StepOutcome => { const value = outcomes[key]; if (value instanceof Error) throw value; return value || { ok: true, detail: key }; };
  const runtime: ActionRuntime = {
    async send(target, text, to) { calls.push(`send:${to}:${text}:${target.sessionId}`); if (outcomes['send'] instanceof Error) throw outcomes['send']; return { sessionId: to === 'new' ? 'fresh' : target.sessionId, collectionId: target.collectionId, turnEndedAt: 5 }; },
    async snippet(key) { calls.push(`snippet:${key}`); return `body of ${key}`; },
    async wait(sessionId, until, _timeout, _signal, since) { calls.push(`wait:${sessionId}:${until}:${since ?? '-'}`); return out(`wait:${until}`); },
    openPanel(kind) { calls.push(`panel:${kind}`); },
    openUrl(url) { calls.push(`url:${url}`); },
    openFile(_t, path, line) { calls.push(`file:${path}:${line}`); },
    async runWorkspaceAction(collectionId, a) { calls.push(`run:${collectionId}:${a}`); return out(`run:${a}`); },
    async runCommand(sessionId, c) { calls.push(`command:${sessionId}:${c}`); return out(`command:${c}`); },
    async setPermission(sessionId, mode) { calls.push(`permission:${sessionId}:${mode}`); },
    runAppAction(id, argument, t) { calls.push(`do:${id}:${argument}:${t.sessionId}`); return out(`do:${id}`); },
  };
  return { runtime, calls };
}
const target = { collectionId: 'alpha', sessionId: 's1' };
const run = (a: ReturnType<typeof action>, runtime: ActionRuntime, signal = new AbortController().signal) => runUserAction(a, runtime, target, { signal });

test('steps run in order on the conversation in view', async () => {
  const { runtime, calls } = fake();
  const result = await run(action([{ kind: 'send', text: 'hi', to: 'current' }, { kind: 'open', panel: 'changes' }, { kind: 'permission', permission: 'yolo' }]), runtime);
  assert.equal(result.ok, true);
  assert.deepEqual(calls, ['send:current:hi:s1', 'panel:changes', 'permission:s1:yolo']);
  assert.deepEqual(result.states, ['succeeded', 'succeeded', 'succeeded']);
});

test('a do: step runs an action of the app, then the next step (R5: [do: changes.show, run: test])', async () => {
  const { runtime, calls } = fake({ 'do:layout.cycle': { ok: false, detail: '“Next layout” is not available in the window right now' } });
  const result = await run(action([{ kind: 'do', doAction: 'changes.show' }, { kind: 'run', action: 'test' }, { kind: 'do', doAction: 'settings.open', doArgument: 'keyboard' }]), runtime);
  assert.equal(result.ok, true);
  assert.deepEqual(calls, ['do:changes.show::s1', 'run:alpha:test', 'do:settings.open:keyboard:s1']);
  assert.equal(stepLabel(step({ kind: 'do', doAction: 'changes.show' })), 'Review changes');
  assert.equal(stepLabel(step({ kind: 'do', doAction: 'settings.open', doArgument: 'keyboard' })), 'Open settings (keyboard)');
  const refused = await run(action([{ kind: 'do', doAction: 'layout.cycle' }]), runtime);
  assert.equal(refused.ok, false);
  assert.match(refused.detail, /not available/);
});

test('a failure runs the when: failed branch and skips the succeeded one', async () => {
  const { runtime, calls } = fake({ 'run:test': { ok: false, detail: 'test exited 1' } });
  const result = await run(action([{ kind: 'run', action: 'test' }, { kind: 'send', text: 'fix it', when: 'failed' }, { kind: 'open', panel: 'changes', when: 'succeeded' }]), runtime);
  assert.deepEqual(calls, ['run:alpha:test', 'send:current:fix it:s1']);
  assert.deepEqual(result.states, ['failed', 'succeeded', 'skipped']);
  // The failure was handled: the last step that ran succeeded.
  assert.equal(result.ok, true);
});

test('an unhandled failure stops the action with the step and its reason', async () => {
  const { runtime, calls } = fake({ 'command:npm test': { ok: false, detail: '`npm test` exited 2' } });
  const result = await run(action([{ kind: 'command', command: 'npm test' }, { kind: 'open', panel: 'console' }]), runtime);
  assert.equal(result.ok, false); assert.equal(result.failedStep, 0); assert.equal(result.detail, '`npm test` exited 2');
  assert.deepEqual(result.states, ['failed', 'skipped']);
  assert.deepEqual(calls, ['command:s1:npm test']);
});

test('a thrown Runner refusal is the step failure, without the code prefix', async () => {
  const { runtime } = fake({ 'run:deploy': new Error('[failed_precondition] Approve api’s scripts first') });
  const result = await run(action([{ kind: 'run', action: 'deploy' }]), runtime);
  assert.equal(result.ok, false); assert.equal(result.detail, 'Approve api’s scripts first');
});

test('send to a new conversation makes it current, and wait follows the turn it started', async () => {
  const { runtime, calls } = fake();
  await run(action([{ kind: 'send', snippet: 'review', to: 'new', wait: true }, { kind: 'wait', until: 'needs-input' }]), runtime);
  assert.deepEqual(calls, ['snippet:review', 'send:new:body of review:s1', 'wait:fresh:idle:5', 'wait:fresh:needs-input:-']);
});

test('steps that need a conversation say so instead of guessing', async () => {
  const { runtime, calls } = fake();
  const result = await runUserAction(action([{ kind: 'command', command: 'ls' }]), runtime, { collectionId: 'alpha', sessionId: '' }, { signal: new AbortController().signal });
  assert.equal(result.ok, false); assert.match(result.detail, /Open a conversation first/); assert.deepEqual(calls, []);
});

test('cancelling stops before the next step', async () => {
  const control = new AbortController();
  const { runtime, calls } = fake();
  runtime.openPanel = kind => { calls.push(`panel:${kind}`); control.abort(); };
  const result = await run(action([{ kind: 'open', panel: 'files' }, { kind: 'send', text: 'never' }]), runtime, control.signal);
  assert.equal(result.cancelled, true); assert.deepEqual(calls, ['panel:files']);
});

test('summaries read as a sentence of steps', () => {
  assert.equal(actionSummary(action([{ kind: 'run', action: 'test' }, { kind: 'send', text: 'fix the failures', when: 'failed' }])), 'Run test → if failed: Send “fix the failures”');
  assert.equal(stepLabel(step({ kind: 'open', file: 'api/main.go', line: 12 })), 'Open api/main.go:12');
  assert.equal(stepLabel(step({ kind: 'send', snippet: 'review', to: 'new' })), 'Send snippet review to a new conversation');
});

test('conversations resolve by id, exact title, then a unique prefix in the workspace first', () => {
  const sessions = [{ id: 'x1', title: 'Fix the tests', collectionId: 'beta' }, { id: 'x2', title: 'Fix the tests', collectionId: 'alpha' }, { id: 'x3', title: 'Release notes', collectionId: 'alpha' }, { id: 'old', title: 'Old', successorExecutionId: 'x3' }];
  assert.equal(resolveConversation(sessions, 'x1')?.id, 'x1');
  assert.equal(resolveConversation(sessions, 'fix the tests', 'alpha')?.id, 'x2');
  assert.equal(resolveConversation(sessions, 'release', 'alpha')?.id, 'x3');
  assert.equal(resolveConversation(sessions, 'nothing'), undefined);
});

test('workspace actions resolve by key or a unique name', () => {
  const actions = [{ key: 'api/test', name: 'test' }, { key: 'web/test', name: 'test' }, { key: 'api/serve', name: 'serve' }];
  assert.equal(resolveWorkspaceAction(actions, 'api/test').action?.key, 'api/test');
  assert.equal(resolveWorkspaceAction(actions, 'serve').action?.key, 'api/serve');
  assert.match(resolveWorkspaceAction(actions, 'test').problem!, /Several repositories/);
  assert.match(resolveWorkspaceAction(actions, 'lint').problem!, /no action “lint”/);
});

test('a leader key taken by a built-in action or a menu entry is reported, not bound', () => {
  const map = keyboardPreset();
  const conflicts = leaderConflicts([{ id: 'one', key: 'w', label: 'One', run() {} }, { id: 'two', key: 'q', label: 'Two', run() {} }, { id: 'three', key: 'z', label: 'Three', run() {} }], map, [{ key: 'q', label: 'Quick', snippet: 'quick', mode: 'send' }]);
  assert.deepEqual(conflicts.map(c => [c.extension.id, c.takenBy]), [['one', 'New workspace'], ['two', 'Quick']]);
});

test('after a handled failure, unconditional steps still run; a failing handler ends the action', async () => {
  let { runtime, calls } = fake({ 'run:test': { ok: false, detail: 'exit 1' } });
  let result = await run(action([{ kind: 'run', action: 'test' }, { kind: 'send', text: 'fix', when: 'failed' }, { kind: 'open', panel: 'console' }]), runtime);
  assert.equal(result.ok, true); assert.equal(result.failedStep, 0); assert.deepEqual(calls, ['run:alpha:test', 'send:current:fix:s1', 'panel:console']);
  ({ runtime, calls } = fake({ 'run:test': { ok: false, detail: 'exit 1' }, send: new Error('the conversation is read-only') }));
  result = await run(action([{ kind: 'run', action: 'test' }, { kind: 'send', text: 'fix', when: 'failed' }, { kind: 'open', panel: 'console' }]), runtime);
  assert.equal(result.ok, false); assert.equal(result.failedStep, 1); assert.equal(result.detail, 'the conversation is read-only');
  assert.deepEqual(result.states, ['failed', 'failed', 'skipped']);
});
