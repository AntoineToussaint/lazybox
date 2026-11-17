import { test } from 'node:test';
import assert from 'node:assert/strict';
import type { HostedProcess } from '@lazybox/protocol';
import { backendLabel, formatMemory, formatUptime, groupProcesses, listeningLabel, processState, stopConsequence } from '../packages/surfaces/src/processes-model.ts';

const process = (fields: Partial<HostedProcess>) => ({ id: 'h', kind: 'agent', backend: 'sessionhost', state: 'running', attached: true, hostPid: 1, programPid: 2, uptimeSeconds: 0n, label: 'x', workspaceId: '', workspaceName: '', sessionId: '', conversationTitle: '', provider: '', commandId: '', terminalId: '', attachCommand: '', ...fields }) as HostedProcess;

test('a host no Runner holds is an orphan whatever it runs, and listed last', () => {
  const groups = groupProcesses([process({ id: 'o', kind: 'command', attached: false }), process({ id: 't', kind: 'terminal' }), process({ id: 'a' })]);
  assert.deepEqual(groups.map(g => [g.key, g.processes.map(p => p.id)]), [['agent', ['a']], ['terminal', ['t']], ['orphan', ['o']]]);
});

test('this workspace only keeps its own processes', () => {
  const groups = groupProcesses([process({ id: 'here', workspaceId: 'w' }), process({ id: 'there', workspaceId: 'v' })], 'w');
  assert.deepEqual(groups.flatMap(g => g.processes.map(p => p.id)), ['here']);
});

test('uptime and memory read as people say them', () => {
  assert.equal(formatUptime(90061n), '1d 1h');
  assert.equal(formatUptime(8100n), '2h 15m');
  assert.equal(formatUptime(95), '1m');
  assert.equal(formatUptime(12), '12s');
  assert.equal(formatUptime(0n), '');
  assert.equal(formatMemory(412n * 1048576n), '412 MB');
  assert.equal(formatMemory(1.5 * 2 ** 30), '1.5 GB');
  assert.equal(formatMemory(2048), '2 KB');
});

test('state and stop consequence say what matters', () => {
  assert.deepEqual(processState(process({ attached: false })), { label: 'Orphaned', tone: 'warn' });
  assert.deepEqual(processState(process({ state: 'unreachable' })), { label: 'Not answering', tone: 'warn' });
  assert.match(stopConsequence(process({})), /conversation stays in your history/);
  assert.match(stopConsequence(process({ kind: 'terminal' })), /SIGHUP/);
  assert.match(stopConsequence(process({ attached: false })), /No Runner supervises/);
  assert.match(stopConsequence(process({ kind: 'command', backend: 'run' })), /SIGTERM, then SIGKILL/);
});

test('a lazybox run in the background says what hosts it and where it listens', () => {
  assert.equal(backendLabel('run'), 'lazybox run');
  assert.equal(backendLabel('tmux'), 'tmux');
  assert.equal(backendLabel('sessionhost'), 'session host');
  assert.equal(listeningLabel([3000]), 'listening on :3000');
  assert.equal(listeningLabel([3000, 3001]), 'listening on :3000, :3001');
  assert.equal(listeningLabel([]), '');
});
