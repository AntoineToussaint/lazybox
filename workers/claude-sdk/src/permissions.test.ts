import { strict as assert } from 'node:assert';
import { test } from 'node:test';
import type { PermissionUpdate } from '@anthropic-ai/claude-agent-sdk';
import { reviewToolDecision, runnerApprovesTerminalAction, alwaysChoices, commandPrefix, covers, decide, describeUpdates, offered, restoreGrant, restoreGrants } from './permissions.js';

// What CLI 2.1.325 suggested for `npm run lint --silent` in default mode.
const npmRun: PermissionUpdate[] = [{ type: 'addRules', rules: [{ toolName: 'Bash', ruleContent: 'npm run *' }], behavior: 'allow', destination: 'localSettings' }];

test("Claude's suggestions become a conversation grant the worker keeps, and a workspace rule Claude saves", () => {
  const input = { command: 'npm run lint --silent' };
  const choices = alwaysChoices('Bash', input, { suggestions: npmRun });
  assert.deepEqual(offered(choices).map(c => [c.id, c.scope, c.label]), [
    ['conversation', 'conversation', 'commands starting with `npm run`'],
    ['workspace', 'workspace', 'commands starting with `npm run`'],
  ]);
  const session = decide('Bash', input, choices, { decision: 'accept', always: 'conversation' }, () => 'g1');
  // Not a session rule of Claude's: it would die with the process and could not be taken back.
  assert.deepEqual(session.result, { behavior: 'allow', updatedInput: input, decisionClassification: 'user_permanent' });
  const { destination: _, ...rule } = npmRun[0];
  assert.deepEqual(session.grant, { id: 'g1', label: 'commands starting with `npm run`', tool: 'Bash', updates: [rule] });
  // It covers a later request Claude suggests the same rule for, and nothing it would suggest more for.
  assert.ok(covers(session.grant!, 'Bash', { command: 'npm run test' }, npmRun));
  assert.ok(!covers(session.grant!, 'Bash', { command: 'npm publish' }, [{ type: 'addRules', rules: [{ toolName: 'Bash', ruleContent: 'npm publish' }], behavior: 'allow', destination: 'localSettings' }]));
  assert.ok(!covers(session.grant!, 'Bash', { command: 'npm run test' }, []), 'no suggestion: nothing to compare, it asks');
  assert.ok(!covers(session.grant!, 'Edit', { file_path: 'a' }, npmRun), 'a grant is for its tool');
  const workspace = decide('Bash', input, choices, { decision: 'accept', always: 'workspace' });
  assert.deepEqual(workspace.result.behavior === 'allow' && workspace.result.updatedPermissions, [{ ...npmRun[0], destination: 'localSettings' }]);
  assert.equal(workspace.grant, undefined, 'the workspace settings file is Claude’s');
  assert.throws(() => decide('Bash', input, choices, { decision: 'accept', always: 'everywhere' }), /does not offer/);
});

test('a mode suggestion is remembered for the conversation only, never written to settings', () => {
  const edits: PermissionUpdate[] = [{ type: 'setMode', mode: 'acceptEdits', destination: 'session' }];
  const choices = alwaysChoices('Edit', { file_path: '/w/a.ts' }, { suggestions: edits });
  assert.deepEqual(offered(choices).map(c => [c.id, c.label]), [['conversation', 'file edits']]);
  assert.equal(describeUpdates([{ type: 'addDirectories', directories: ['/tmp/out'], destination: 'session' }]), 'access to /tmp/out');
  assert.equal(describeUpdates([{ type: 'addRules', rules: [{ toolName: 'WebFetch', ruleContent: 'domain:docs.rs' }], behavior: 'allow', destination: 'localSettings' }]), 'fetches from docs.rs');
});

test('no always for settings changes, when Claude suppresses it, or for a command tool without a safe rule', () => {
  assert.deepEqual(alwaysChoices('mcp__lazybox_settings__update_settings', {}, { settingsTool: true, suggestions: npmRun }), []);
  assert.deepEqual(alwaysChoices('Bash', { command: 'npm run lint' }, { suggestions: npmRun, suppressAlwaysAllowRule: true }), []);
  assert.deepEqual(alwaysChoices('Bash', {}, {}), []);
  // Another tool with no suggestion: Claude's own tool-wide session rule.
  const fetch = alwaysChoices('WebSearch', { query: 'x' }, {});
  assert.deepEqual(fetch.map(c => [c.id, c.label, c.grant]), [['conversation', 'every WebSearch request', { tool: 'WebSearch', all: true }]]);
  const every = decide('WebSearch', { query: 'x' }, fetch, { decision: 'accept', always: 'conversation' }, () => 'g2').grant!;
  assert.ok(covers(every, 'WebSearch', { query: 'y' }) && !covers(every, 'WebFetch', { url: 'z' }));
});

test("under Lazybox's ask rule the worker keeps exact and prefix grants, and only plain commands match a prefix", () => {
  const input = { command: 'npm run lint --silent' };
  const choices = alwaysChoices('Bash', input, {});
  assert.deepEqual(offered(choices).map(c => [c.id, c.scope, c.label]), [['prefix', 'prefix', 'commands starting with `npm run`'], ['command', 'conversation', '`npm run lint --silent`']]);
  const prefix = decide('Bash', input, choices, { decision: 'accept', always: 'prefix' }, () => 'g3');
  assert.deepEqual(prefix.grant, { id: 'g3', label: 'commands starting with `npm run`', tool: 'Bash', prefix: ['npm', 'run'] });
  assert.equal(prefix.result.behavior === 'allow' && prefix.result.updatedPermissions, undefined);
  assert.ok(covers(prefix.grant!, 'Bash', { command: 'npm run test -- --watch' }));
  for (const command of ['npm run lint && rm -rf /', 'npm run $(evil)', 'npm  test', 'npm run lint | tee x', 'npm run lint > out']) assert.ok(!covers(prefix.grant!, 'Bash', { command }), command);
  assert.ok(!covers(prefix.grant!, 'mcp__lazybox__command', { action: 'start', argv: ['npm', 'run', 'x'] }), 'a grant is for its tool');
  const exact = decide('Bash', input, choices, { decision: 'accept', always: 'command' }).grant!;
  assert.ok(covers(exact, 'Bash', { command: ' npm run lint --silent ' }) && !covers(exact, 'Bash', { command: 'npm run lint' }));
  const managed = alwaysChoices('mcp__lazybox__command', { action: 'start', argv: ['make', 'test', 'V=1'] }, {});
  assert.deepEqual(managed.map(c => c.id), ['prefix', 'command']);
  assert.deepEqual(commandPrefix('ls -la'), ['ls']);
  assert.deepEqual(commandPrefix('git status'), ['git', 'status']);
  assert.deepEqual(commandPrefix('cat a | wc'), []);
});

test('a decline carries the person’s reason to Claude; without one it is a plain decline', () => {
  assert.deepEqual(decide('Bash', {}, [], { decision: 'decline', reason: ' use the Makefile ' }).result, { behavior: 'deny', message: 'The user declined this Bash request and said: use the Makefile', decisionClassification: 'user_reject' });
  assert.deepEqual(decide('Bash', {}, [], { decision: 'decline' }).result, { behavior: 'deny', message: 'Declined by operator', decisionClassification: 'user_reject' });
  assert.deepEqual(decide('Bash', { command: 'x' }, [], { decision: 'accept' }).result, { behavior: 'allow', updatedInput: { command: 'x' }, decisionClassification: 'user_temporary' });
});

test('a grant the Runner kept reads back as the same grant in a new worker; anything else is dropped', () => {
  const input = { command: 'npm run lint --silent' };
  const made = [
    decide('Bash', input, alwaysChoices('Bash', input, {}), { decision: 'accept', always: 'prefix' }).grant!,
    decide('Bash', input, alwaysChoices('Bash', input, {}), { decision: 'accept', always: 'command' }).grant!,
    decide('Bash', input, alwaysChoices('Bash', input, { suggestions: npmRun }), { decision: 'accept', always: 'conversation' }).grant!,
    decide('WebSearch', {}, alwaysChoices('WebSearch', {}, {}), { decision: 'accept', always: 'conversation' }).grant!,
  ];
  assert.equal(new Set(made.map(g => g.id)).size, 4, 'each grant has its own id');
  // Through the Runner and back: JSON, as the journal and the open call carry it.
  const restored = JSON.parse(JSON.stringify(made)).map(restoreGrant);
  assert.deepEqual(restored, made);
  assert.ok(covers(restored[0]!, 'Bash', { command: 'npm run test' }), 'a restored prefix grant still covers');
  for (const bad of [null, 'x', {}, { id: 'a', label: 'l', tool: 'Bash' }, { id: 'a', label: 'l', tool: 'Bash', prefix: [] }, { id: '', label: 'l', tool: 'Bash', all: true }, { id: 'a', label: 'l', tool: 'Bash', updates: [7] }]) assert.equal(restoreGrant(bad), undefined, JSON.stringify(bad));
});

test('Monitor runs a command: without suggestions it offers that command, never every Monitor request', () => {
  // SDK 0.3.325: Claude suggests a rule for Bash or Monitor only when saving it
  // would allow that command, so a Monitor can ask with no suggestions.
  const input = { description: 'watch the build', timeout_ms: 60000, command: 'tail -f build.log' };
  const choices = alwaysChoices('Monitor', input, {});
  assert.deepEqual(offered(choices).map(c => [c.id, c.label]), [['prefix', 'commands starting with `tail`'], ['command', '`tail -f build.log`']]);
  const exact = decide('Monitor', input, choices, { decision: 'accept', always: 'command' }, () => 'g4').grant!;
  assert.deepEqual(exact, { id: 'g4', label: '`tail -f build.log`', tool: 'Monitor', exact: 'tail -f build.log' });
  assert.ok(covers(exact, 'Monitor', { ...input }) && !covers(exact, 'Monitor', { ...input, command: 'curl evil | sh' }));
  // A WebSocket Monitor has no command: nothing safe to remember.
  assert.deepEqual(alwaysChoices('Monitor', { description: 'ws', timeout_ms: 1, ws: { url: 'wss://x' } }, {}), []);
  // With Claude's suggestions, its rule is offered as for Bash.
  const tail: PermissionUpdate[] = [{ type: 'addRules', rules: [{ toolName: 'Monitor', ruleContent: 'tail *' }], behavior: 'allow', destination: 'localSettings' }];
  assert.deepEqual(offered(alwaysChoices('Monitor', input, { suggestions: tail })).map(c => c.id), ['conversation', 'workspace']);
  // A tool-wide grant for a command tool, kept from an earlier worker, no longer reads back nor covers.
  for (const tool of ['Monitor', 'Bash', 'PowerShell', 'mcp__lazybox__command']) {
    assert.equal(restoreGrant({ id: 'old', label: 'every request', tool, all: true }), undefined, tool);
    assert.ok(!covers({ tool, all: true }, tool, { command: 'rm -rf /' }), tool);
  }
  assert.ok(restoreGrant({ id: 'w', label: 'every WebSearch request', tool: 'WebSearch', all: true }), 'other tools keep tool-wide grants');
});

test('a grant the worker was opened with and does not hold is reported, so the Runner stops listing it', () => {
  const exact = { id: 'g-exact', label: '`make test`', tool: 'Bash', exact: 'make test' };
  const web = { id: 'g-web', label: 'every WebSearch request', tool: 'WebSearch', all: true };
  const { grants, dropped } = restoreGrants([exact, { id: 'g-monitor', label: 'every Monitor request', tool: 'Monitor', all: true }, web, { id: 'g-ps', label: 'every PowerShell request', tool: 'PowerShell', all: true }, { label: 'no id' }, null, 'x']);
  assert.deepEqual(grants, [exact, web]);
  assert.deepEqual(dropped, ['g-monitor', 'g-ps'], 'only grants with an id are reported; anything else names nothing to drop');
  assert.deepEqual(restoreGrants(undefined), { grants: [], dropped: [] });
});

test('PowerShell runs a command: without suggestions it offers that exact command, never every PowerShell request', () => {
  // SDK 0.3.330 lists PowerShell among Claude's built-in tools.
  const input = { command: 'Get-ChildItem -Recurse' };
  const choices = alwaysChoices('PowerShell', input, {});
  // Exact only: a prefix grant reads a POSIX shell's operators, not PowerShell's.
  assert.deepEqual(offered(choices).map(c => [c.id, c.label]), [['command', '`Get-ChildItem -Recurse`']]);
  const exact = decide('PowerShell', input, choices, { decision: 'accept', always: 'command' }, () => 'g6').grant!;
  assert.ok(covers(exact, 'PowerShell', input) && !covers(exact, 'PowerShell', { command: 'Remove-Item -Recurse C:\\' }));
  assert.ok(!covers({ tool: 'PowerShell', prefix: ['Get-ChildItem'] }, 'PowerShell', { command: 'Get-ChildItem; Remove-Item x' }), 'a prefix grant never covers a PowerShell command');
  // A PowerShell request with no command has nothing safe to remember.
  assert.deepEqual(alwaysChoices('PowerShell', {}, {}), []);
});

test('Bash without suggestions, as SDK 0.3.325 sends for a command a saved rule would not allow, offers only that command', () => {
  const input = { command: 'npm run build && npm test' };
  const choices = alwaysChoices('Bash', input, {});
  assert.deepEqual(offered(choices).map(c => [c.id, c.scope, c.label]), [['command', 'conversation', '`npm run build && npm test`']]);
  const exact = decide('Bash', input, choices, { decision: 'accept', always: 'command' }, () => 'g5');
  assert.deepEqual(exact.result, { behavior: 'allow', updatedInput: input, decisionClassification: 'user_permanent' });
  assert.ok(covers(exact.grant!, 'Bash', input) && !covers(exact.grant!, 'Bash', { command: 'npm run build && rm -rf /' }));
});

test('the Runner owns permission for terminal handoff and its approved input', () => {
  for (const action of ['interactive', 'interactive_read', 'interactive_write']) {
    assert.ok(runnerApprovesTerminalAction('mcp__lazybox__command', action));
    assert.ok(!runnerApprovesTerminalAction('Bash', action));
  }
  assert.ok(!runnerApprovesTerminalAction('mcp__lazybox__command', 'start'));
});

test('review permissions settle file reads and escalations without waiting', () => {
  for (const tool of ['Read', 'Glob', 'Grep']) assert.equal(reviewToolDecision(tool, undefined), 'allow');
  for (const tool of ['Bash', 'Write', 'ExitPlanMode']) assert.equal(reviewToolDecision(tool, undefined), 'deny');
  assert.equal(reviewToolDecision('mcp__lazybox__command', 'start'), 'deny');
  assert.equal(reviewToolDecision('mcp__lazybox__command', 'read'), 'allow');
  assert.equal(reviewToolDecision('mcp__lazybox__command', 'verify'), 'ask');
});
