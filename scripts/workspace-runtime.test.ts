import { create } from '@bufbuild/protobuf';
import { WorkspaceActionResultSchema, WorkspaceActionSchema, WorkspaceRunStateSchema, WorkspaceRuntimeSchema, WorkspaceScriptSourceSchema, WorkspaceSetupSchema } from '@lazybox/protocol';
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { actionName, changedLabel, changedLines, defaultAction, elapsed, needsApproval, previewable, runLabel, setupLabel, stoppedElsewhereNotice } from '../packages/surfaces/src/workspace-runtime.ts';

const action = (member: string, name: string, extra: Record<string, unknown> = {}) => create(WorkspaceActionSchema, { key: `${member}/${name}`, member, name, ...extra });
const state = (value: Record<string, unknown>) => create(WorkspaceRunStateSchema, value);

test('elapsed time reads like a status line', () => {
  const now = 10_000_000;
  assert.equal(elapsed(0, now), '');
  assert.equal(elapsed(now - 12_000, now), '12s');
  assert.equal(elapsed(now - 4 * 60_000, now), '4m');
  assert.equal(elapsed(now - 65 * 60_000, now), '1h 5m');
  assert.equal(elapsed(now - 120 * 60_000, now), '2h');
  assert.equal(elapsed(BigInt(now + 5000), now), '0s', 'a clock ahead of ours never shows a negative time');
});

test('run and setup states say what happened, failures with their exit code', () => {
  assert.deepEqual(runLabel(state({ state: 'running' })), { text: 'Running', tone: 'ok' });
  assert.deepEqual(runLabel(state({ state: 'failed', exitCode: 1 })), { text: 'Failed · exit 1', tone: 'bad' });
  assert.deepEqual(runLabel(undefined), { text: 'Not running', tone: 'idle' });
  assert.equal(setupLabel(create(WorkspaceSetupSchema, { pendingApproval: true })).text, 'Waiting for your approval');
  assert.equal(setupLabel(create(WorkspaceSetupSchema, { runState: state({ state: 'succeeded' }) })).text, 'Done');
});

test('an action is named by its repository only when another repository shares its name', () => {
  const all = [action('api', 'dev'), action('web', 'dev'), action('api', 'test')];
  assert.equal(actionName(all[0], all), 'api · dev');
  assert.equal(actionName(all[2], all), 'test');
});

test('Run starts the Runner’s default, then the first marked default, then the first action', () => {
  const actions = [action('api', 'test'), action('api', 'dev', { default: true })];
  assert.equal(defaultAction(create(WorkspaceRuntimeSchema, { actions, defaultAction: 'api/test' }))?.name, 'test');
  assert.equal(defaultAction(create(WorkspaceRuntimeSchema, { actions }))?.name, 'dev');
  assert.equal(defaultAction(create(WorkspaceRuntimeSchema, { actions: [action('api', 'lint')] }))?.name, 'lint');
  assert.equal(defaultAction(create(WorkspaceRuntimeSchema, {})), undefined);
});

test('approval is asked for never-approved and changed scripts, not for broken or approved files', () => {
  const runtime = create(WorkspaceRuntimeSchema, { sources: [
    create(WorkspaceScriptSourceSchema, { member: 'api', approval: 'approved' }),
    create(WorkspaceScriptSourceSchema, { member: 'web', approval: 'changed' }),
    create(WorkspaceScriptSourceSchema, { member: 'docs', approval: 'required', error: 'line 3: unknown field' }),
    create(WorkspaceScriptSourceSchema, { member: 'tools', approval: 'required' }),
  ] });
  assert.deepEqual(needsApproval(runtime).map(s => s.member), ['web', 'tools']);
});

test('only running actions with a URL can be previewed', () => {
  const runtime = create(WorkspaceRuntimeSchema, { actions: [
    action('api', 'dev', { url: 'http://localhost:20000', runState: state({ state: 'running' }) }),
    action('api', 'test', { runState: state({ state: 'running' }) }),
    action('web', 'dev', { url: 'http://localhost:20001', runState: state({ state: 'stopped' }) }),
  ] });
  assert.deepEqual(previewable(runtime).map(a => a.key), ['api/dev']);
});

test('a non-concurrent start says where it stopped the action', () => {
  const result = (...names: string[]) => create(WorkspaceActionResultSchema, { stoppedElsewhere: names.map(workspaceName => ({ workspaceName, collectionId: workspaceName.toLowerCase(), action: 'api/dev' })) });
  assert.equal(stoppedElsewhereNotice(result(), 'dev'), '');
  assert.equal(stoppedElsewhereNotice(result('Beta release'), 'dev'), 'Stopped dev in Beta release to run it here.');
  assert.equal(stoppedElsewhereNotice(result('A', 'B', 'C'), 'dev'), 'Stopped dev in A, B and C to run it here.');
});

test('changed entries are named and their lines marked in the file', () => {
  assert.equal(changedLabel('action:dev'), 'Action “dev”');
  assert.equal(changedLabel('setup'), 'Setup script');
  const file = [
    '# yaml-language-server: $schema=…', // 1
    'setup: |',                            // 2
    '  npm ci',                            // 3
    '  npm run build',                     // 4
    'files:',                              // 5
    '  - .env',                            // 6
    'actions:',                            // 7
    '  dev:',                              // 8
    '    run: npm run dev',                // 9
    '    default: true',                   // 10
    '',                                    // 11
    '  test:',                             // 12
    '    run: npm test',                   // 13
  ].join('\n');
  assert.deepEqual([...changedLines(file, ['setup'])].sort((a, b) => a - b), [2, 3, 4]);
  assert.deepEqual([...changedLines(file, ['action:dev'])].sort((a, b) => a - b), [8, 9, 10]);
  assert.deepEqual([...changedLines(file, ['action:test', 'files'])].sort((a, b) => a - b), [5, 6, 12, 13]);
  assert.deepEqual([...changedLines(file, ['action:missing', 'archive'])], []);
});
