import { create } from '@bufbuild/protobuf';
import { CommandReportSchema } from '@lazybox/protocol';
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { RunTracker, appendToDraft, composeRunMessage, formatBytes, isRuntimeRun, isWorkspaceRun, reportSummary, runOrigin, runningLabel } from '../packages/surfaces/src/command-runs.ts';

test('a run is labelled with what its agent received, in the sizes the agent was shown', () => {
  assert.equal(reportSummary(create(CommandReportSchema, { exitCode: 1, agentBytes: 2150n, outputBytes: 1468006n, filter: 'go-test' })), 'Exit 1 · the agent got a 2.1 KiB summary of 1.4 MiB (go-test filter)');
  assert.equal(reportSummary(create(CommandReportSchema, { exitCode: 0, agentBytes: 40n, outputBytes: 12n, filter: 'passthrough' })), 'Exit 0 · the agent got all of it (12 B)');
  assert.equal(reportSummary(create(CommandReportSchema, { exitCode: 0, agentBytes: 5000n, outputBytes: 4096n, filter: 'raw' })), 'Exit 0 · the agent got all of it (4.0 KiB)');
  assert.equal(formatBytes(1023), '1023 B');
});

test('each command opens the console once, and commands that already existed never do', () => {
  const tracker = new RunTracker();
  assert.deepEqual(tracker.next(['old-build', 'old-server']), [], 'the first listing is the baseline');
  assert.deepEqual(tracker.next(['old-build', 'old-server']), []);
  assert.deepEqual(tracker.next(['old-build', 'run-1', 'old-server']), ['run-1']);
  assert.deepEqual(tracker.next(['old-build', 'run-1', 'old-server']), [], 'a run the user closed the console on stays closed');
  assert.deepEqual(tracker.next(['run-2', 'run-1', 'run-3']), ['run-2', 'run-3']);
});

test('a run from a workspace terminal says what an agent would get, and when the exit status is unknown', () => {
  assert.equal(reportSummary(create(CommandReportSchema, { piped: true, agentBytes: 2150n, outputBytes: 1468006n, filter: 'go-test' }), true), 'Piped, exit unknown · an agent would get a 2.1 KiB summary of 1.4 MiB (go-test filter)');
  assert.equal(reportSummary(create(CommandReportSchema, { exitCode: 2, agentBytes: 300n, outputBytes: 9000n, filter: 'passthrough' }), true), 'Exit 2 · an agent would get a 300 B summary of 8.8 KiB');
  assert.ok(isWorkspaceRun({ sessionId: '', collectionId: 'alpha' }));
  assert.ok(!isWorkspaceRun({ sessionId: 'alpha-session', collectionId: '' }));
});

test('Send to agent adds the compact view as fenced output after the draft', () => {
  const view = '[lazybox] go test ./... → exit 1 (2.0s)\n--- FAIL: TestParse\n';
  assert.equal(composeRunMessage({ title: 'go test ./...' }, view), 'Output of `go test ./...` from my terminal:\n```text\n[lazybox] go test ./... → exit 1 (2.0s)\n--- FAIL: TestParse\n```\n');
  // Output that itself contains a fence cannot close the block early.
  assert.ok(composeRunMessage({ title: 'cat README.md' }, 'a\n```\nb\n').startsWith('Output of `cat README.md` from my terminal:\n````text\n'));
  assert.equal(composeRunMessage({ title: 'echo `x`' }, 'x').split('\n')[0], "Output of `echo 'x'` from my terminal:");
  assert.equal(appendToDraft('', 'run'), 'run');
  assert.equal(appendToDraft('  \n', 'run'), 'run');
  assert.equal(appendToDraft('Please fix:\n', 'run'), 'Please fix:\n\nrun');
});

test('actions and setup scripts are labelled as such, never as terminal runs', () => {
  assert.equal(runOrigin({ sessionId: '', collectionId: 'alpha', origin: 'terminal' }), 'from your terminal · ');
  assert.equal(runOrigin({ sessionId: '', collectionId: 'alpha', origin: 'action' }), 'action · ');
  assert.equal(runOrigin({ sessionId: '', collectionId: 'alpha', origin: 'setup' }), 'setup · ');
  assert.equal(runOrigin({ sessionId: 'alpha-session', collectionId: '', origin: '' }), '');
  assert.ok(isRuntimeRun({ origin: 'archive' }) && !isRuntimeRun({ origin: 'terminal' }));
});

test('a running command says where it listens, and when it went on in the background', () => {
  assert.equal(runningLabel({ background: false, ports: [] }), 'Running');
  assert.equal(runningLabel({ background: false, ports: [3000] }), 'Running · port 3000');
  assert.equal(runningLabel({ background: true, ports: [3000] }), 'Running in the background · port 3000');
  assert.equal(runningLabel({ background: true, ports: [] }), 'Running in the background');
});
