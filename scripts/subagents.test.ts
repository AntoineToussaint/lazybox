import { create } from '@bufbuild/protobuf';
import { JournalEventSchema, type JournalEvent } from '@lazybox/protocol';
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { emptyTranscript, foldTranscript, type Transcript } from '../packages/surfaces/src/transcript-model.ts';
import { describeGroup, describeTool } from '../packages/surfaces/src/tool-lines.ts';
import { SUBAGENT_LIMIT, actionLabel, agentElapsed, ancestry, backgroundCommands, canStopSubagent, commandTitle, currentAction, panelAgent, endsWithTurn, finishedWork, hiddenFromWork, mergedStatus, resultPreview, statusWord, stepRows, subagentLabel, subagentTree, tokenLabel, toolTotal, type Subagent } from '../packages/surfaces/src/subagent-model.ts';
import { noteStops } from '../packages/surfaces/src/tool-stops.ts';

let cursor = 0n;
const T0 = Date.parse('2026-11-16T10:00:00Z');
type Init = { type: string; agentId?: string; parentAgentId?: string; nativeId?: string; name?: string; agentKind?: string; status?: string; text?: string; turnId?: string; data?: unknown };
const record = (at: number, { data, ...event }: Init): JournalEvent => create(JournalEventSchema, {
  cursor: ++cursor, capturedAt: new Date(T0 + at).toISOString(),
  event: { ...event, providerDataJson: data === undefined ? new Uint8Array() : new TextEncoder().encode(JSON.stringify(data)) },
});
const fold = (state: Transcript, ...records: JournalEvent[]) => foldTranscript(state, records);
const call = (id: string, name: string, status: string, extra: Record<string, unknown> = {}) => ({ type: 'tool_call', id, name, status, ...extra });

/** What the Claude worker journals for a main agent that delegates to one sub-agent. */
function claudeRun() {
  cursor = 0n;
  const child = 'claude-tool:toolu_1';
  let t = emptyTranscript();
  t = fold(t,
    record(0, { type: 'operation_accepted', text: 'Find the flaky test' }),
    record(10, { type: 'message_delta', text: 'I will ask an explorer.' }),
    record(20, { type: 'tool_activity', data: call('toolu_1', 'Task', 'running', { input: { description: 'Find flaky tests', prompt: 'Look through the test suite.\nReport the flaky ones.', subagent_type: 'Explore' } }) }),
    record(30, { type: 'agent_status', agentId: child, parentAgentId: 'root', nativeId: 'task-1', status: 'running', name: 'Find flaky tests', agentKind: 'local_agent', data: { toolUseId: 'toolu_1', subagentType: 'Explore', prompt: 'Look through the test suite.\nReport the flaky ones.' } }),
  );
  return { t, child };
}

test('a sub-agent card sits where its parent spawned it, with its task and type', () => {
  const { t, child } = claudeRun();
  assert.deepEqual(t.messages.map(m => m.type), ['operation_accepted', 'message_delta', 'subagent']);
  assert.equal(t.messages[2].agentId, child);
  const agent = t.subagents.byId[child];
  assert.equal(agent.name, 'Find flaky tests');
  assert.equal(agent.type, 'Explore');
  assert.equal(agent.nativeId, 'task-1');
  assert.equal(agent.status, 'running');
  assert.match(agent.prompt, /Report the flaky ones/);
  assert.equal(agent.anchor, `subagent:${child}`);
  // The main agent's next words come after the card, as a new message.
  const after = fold(t, record(40, { type: 'message_delta', text: 'Meanwhile…' }));
  assert.deepEqual(after.messages.map(m => m.type), ['operation_accepted', 'message_delta', 'subagent', 'message_delta']);
});

test('its own tool calls and prose are attributed to it, never to the main transcript', () => {
  let { t, child } = claudeRun();
  t = fold(t,
    record(1000, { type: 'tool_activity', agentId: child, data: call('c1', 'Read', 'running', { input: { file_path: 'platform/bin/devbox' } }) }),
  );
  assert.equal(currentAction(t.subagents.byId[child])?.text, 'Reading platform/bin/devbox');
  assert.equal(currentAction(t.subagents.byId[child])?.since, T0 + 1000);
  t = fold(t,
    record(1500, { type: 'tool_activity', agentId: child, data: call('c1', undefined as never, 'completed', { output: 'file body' }) }),
    record(1600, { type: 'message_delta', agentId: child, parentAgentId: 'root', text: 'Two tests ' }),
    record(1700, { type: 'message_delta', agentId: child, parentAgentId: 'root', text: 'look flaky.' }),
    record(1800, { type: 'tool_activity', agentId: child, data: call('c2', 'Bash', 'running', { input: { command: 'go test ./... -run TestLogin -count 20', description: 'Run the login test' } }) }),
  );
  const agent = t.subagents.byId[child];
  assert.deepEqual(agent.items.map(i => i.kind), ['tool', 'message', 'tool']);
  assert.equal(agent.items[1].kind === 'message' && agent.items[1].text, 'Two tests look flaky.');
  assert.equal(agent.items[0].kind === 'tool' && agent.items[0].call.status, 'Completed');
  assert.equal(agent.toolCount, 2);
  assert.equal(currentAction(agent)?.text, 'Running go test ./... -run TestLogin -count 20');
  assert.ok(!t.messages.some(m => m.text.includes('look flaky')), 'a sub-agent\'s prose stays out of the main transcript');
});

test('progress, tokens and the result it hands back', () => {
  let { t, child } = claudeRun();
  t = fold(t, record(5000, { type: 'agent_status', agentId: child, status: 'running', data: { toolUseId: 'toolu_1', usage: { totalTokens: 48_200, toolUses: 14, durationMs: 4900 }, summary: 'Analyzing the login test' } }));
  let agent = t.subagents.byId[child];
  assert.equal(agent.tokens, 48_200);
  assert.equal(toolTotal(agent), 14);
  assert.equal(agent.progress, 'Analyzing the login test');
  assert.equal(agentElapsed(agent, T0 + 65_020), 65_000, 'it runs from the call that spawned it');
  t = fold(t,
    record(70_000, { type: 'tool_activity', data: { type: 'tool_call', id: 'toolu_1', status: 'completed', output: [{ type: 'text', text: '## Findings\n- TestLogin races on the clock.' }, { type: 'text', text: 'agentId: a1b2c3 (use to resume)' }] } }),
    record(70_010, { type: 'agent_status', agentId: child, status: 'done', data: { toolUseId: 'toolu_1', summary: 'Found one flaky test', usage: { totalTokens: 61_000, toolUses: 23, durationMs: 69_900 } } }),
  );
  agent = t.subagents.byId[child];
  assert.equal(agent.status, 'done');
  assert.equal(agent.result, '## Findings\n- TestLogin races on the clock.');
  assert.equal(resultPreview(agent.result), 'Findings · TestLogin races on the clock.');
  assert.equal(agent.summary, 'Found one flaky test');
  assert.equal(agent.progress, '');
  assert.equal(agentElapsed(agent, T0 + 999_999), 69_900, 'a finished sub-agent reports the provider\'s duration');
  assert.equal(currentAction(agent), undefined);
});

test('a nested sub-agent lives in its parent\'s timeline, not as a second card', () => {
  let { t, child } = claudeRun();
  const grandchild = 'claude-tool:toolu_2';
  t = fold(t,
    record(2000, { type: 'tool_activity', agentId: child, data: call('toolu_2', 'Task', 'running', { input: { description: 'Read CI logs', prompt: 'Read the logs', subagent_type: 'general-purpose' } }) }),
    record(2100, { type: 'agent_status', agentId: grandchild, parentAgentId: child, nativeId: 'task-2', status: 'running', name: 'Read CI logs', data: { toolUseId: 'toolu_2' } }),
    record(2200, { type: 'tool_activity', agentId: grandchild, data: call('g1', 'WebFetch', 'running', { input: { url: 'https://ci.example.com/run/42' } }) }),
  );
  assert.equal(t.messages.filter(m => m.type === 'subagent').length, 1);
  const parent = t.subagents.byId[child], nested = t.subagents.byId[grandchild];
  assert.deepEqual(parent.children, [grandchild]);
  assert.equal(parent.items.at(-1)?.kind, 'agent');
  assert.equal(nested.depth, 1);
  assert.equal(nested.anchor, '');
  assert.equal(currentAction(nested)?.text, 'Fetching ci.example.com/run/42');
  assert.deepEqual(subagentTree(t.subagents).map(n => [n.agent.id, n.depth]), [[child, 0], [grandchild, 1]]);
  assert.deepEqual(ancestry(t.subagents, grandchild), [child, grandchild]);
});

test('a failed sub-agent says why; an ended session stops the rest', () => {
  let { t, child } = claudeRun();
  t = fold(t,
    record(3000, { type: 'tool_activity', agentId: child, data: call('c9', 'Bash', 'running', { input: { command: 'make test' } }) }),
    record(3100, { type: 'agent_status', agentId: child, status: 'failed', data: { toolUseId: 'toolu_1', error: 'Tool budget exceeded' } }),
  );
  const failed = t.subagents.byId[child];
  assert.equal(failed.status, 'failed');
  assert.equal(failed.error, 'Tool budget exceeded');
  assert.equal(failed.items[0].kind === 'tool' && failed.items[0].call.status, 'Result not reported', 'a missing result is never inferred to be success');
  const other = 'claude-tool:toolu_3';
  t = fold(t,
    record(4000, { type: 'agent_status', agentId: other, status: 'running', name: 'Second' }),
    record(5000, { type: 'session_closed' }),
  );
  assert.equal(t.subagents.byId[other].status, 'stopped');
  assert.equal(t.subagents.byId[child].status, 'failed');
});

test('the Runner\'s word that a sub-agent ended wins over a journal that still says running', () => {
  const { t, child } = claudeRun();
  const agent = t.subagents.byId[child];
  assert.equal(mergedStatus(agent, { [child]: 'stopped' }), 'stopped');
  assert.equal(mergedStatus(agent, { [child]: 'running' }), 'running');
  assert.equal(mergedStatus(agent, {}), 'running');
});

test('Codex sub-threads get a card from their status and keep their own messages', () => {
  cursor = 0n;
  const thread = 'codex:thread-b';
  let t = fold(emptyTranscript(),
    record(0, { type: 'operation_accepted', text: 'Review the change' }),
    record(10, { type: 'agent_status', agentId: thread, parentAgentId: 'root', nativeId: 'thread-b', agentKind: 'agent', status: 'running', name: 'Review the Go side', data: { toolUseId: 'item-7', prompt: 'Review the Go side' } }),
    record(20, { type: 'turn_started', agentId: thread, parentAgentId: 'root', turnId: 'tb1' }),
    record(30, { type: 'tool_activity', agentId: thread, data: call('cmd-1', 'Shell command', 'running', { input: ['go', 'vet', './...'] }) }),
    record(40, { type: 'message_delta', agentId: thread, text: 'Checking.', data: { itemId: 'progress' } }),
    record(41, { type: 'message_delta', agentId: thread, text: 'Looks ', data: { itemId: 'answer' } }),
    record(42, { type: 'message_delta', agentId: thread, text: 'fine.', data: { itemId: 'answer' } }),
  );
  const agent = t.subagents.byId[thread];
  assert.equal(t.messages.at(-1)?.agentId, thread);
  assert.equal(agent.type, '', 'a generic agent kind is no type');
  assert.deepEqual(agent.items.flatMap(item => item.kind === 'message' ? [item.text] : []), ['Checking.', 'Looks fine.']);
  t = fold(t, record(50, { type: 'turn_ended', agentId: thread, status: 'completed' }));
  assert.equal(t.subagents.byId[thread].status, 'done');
});

test('a fold only replaces the sub-agents it touched', () => {
  let { t, child } = claudeRun();
  t = fold(t, record(100, { type: 'agent_status', agentId: 'claude-tool:other', status: 'running', name: 'Other' }));
  const before = t.subagents.byId[child], otherBefore = t.subagents.byId['claude-tool:other'];
  const next = fold(t, record(200, { type: 'message_delta', agentId: 'claude-tool:other', text: 'hi' }));
  assert.equal(next.subagents.byId[child], before, 'an untouched card keeps its identity, so it does not re-render');
  assert.notEqual(next.subagents.byId['claude-tool:other'], otherBefore);
  assert.equal(otherBefore.items.length, 0, 'the previous state is never mutated');
  // Usage reports do not touch a card.
  const quiet = fold(next, record(300, { type: 'usage', agentId: child, data: { usage: { input_tokens: 1 } } }));
  assert.equal(quiet.subagents, next.subagents);
});

test('a long run keeps a bounded timeline', () => {
  let { t, child } = claudeRun();
  const records: JournalEvent[] = [];
  for (let i = 0; i < 450; i++) records.push(record(100 + i, { type: 'tool_activity', agentId: child, data: call(`r${i}`, 'Grep', 'completed', { input: { pattern: `p${i}` }, output: 'x'.repeat(20_000) }) }));
  t = foldTranscript(t, records);
  const agent = t.subagents.byId[child];
  assert.equal(agent.items.length, 400);
  assert.equal(agent.dropped, 50);
  assert.equal(agent.toolCount, 450);
  const output = agent.items[0].kind === 'tool' ? agent.items[0].call.output as string : '';
  assert.ok(output.length < 9000, 'large outputs keep a preview only');
});

test('actions read as a person says them', () => {
  assert.equal(actionLabel('Read', { file_path: 'src/main.go' }), 'Reading src/main.go');
  assert.equal(actionLabel('Grep', { pattern: 'appendTranscript', path: 'packages' }), 'Searching for “appendTranscript” in packages');
  assert.equal(actionLabel('Glob', { pattern: '**/*.test.ts' }), 'Finding **/*.test.ts');
  assert.equal(actionLabel('Edit', { file_path: 'a/very/long/path/that/goes/on/and/on/into/the/repository/tree/file.ts' }), 'Editing …/goes/on/and/on/into/the/repository/tree/file.ts');
  assert.equal(actionLabel('WebSearch', { query: 'claude agent sdk' }), 'Searching the web for “claude agent sdk”');
  assert.equal(actionLabel('Task', { description: 'Check CI' }), 'Delegating · Check CI');
  assert.equal(actionLabel('mcp__lazybox__command', { action: 'start', argv: ['make', 'test'] }), 'Running make test');
  assert.equal(actionLabel('mcp__github__get_issue', {}), 'Using github · get issue');
  assert.equal(tokenLabel(48_200), '48k tokens');
  assert.equal(tokenLabel(1_500), '1.5k tokens');
});

test('the Runner\'s attempt prefix joins the card made from the spawning call', () => {
  cursor = 0n;
  const real = 'attempt-2:claude-tool:toolu_9';
  const t = fold(emptyTranscript(),
    record(0, { type: 'tool_activity', data: call('toolu_9', 'Agent', 'running', { input: { description: 'Check docs', prompt: 'Check the docs' } }) }),
    record(10, { type: 'tool_activity', agentId: real, parentAgentId: 'root', data: call('x1', 'Read', 'running', { input: { file_path: 'README.md' } }) }),
    record(20, { type: 'agent_status', agentId: real, parentAgentId: 'root', nativeId: 'task-9', status: 'running', name: 'Check docs', data: { toolUseId: 'toolu_9' } }),
  );
  assert.equal(t.messages.filter(m => m.type === 'subagent').length, 1);
  const agent = t.subagents.byId['claude-tool:toolu_9'];
  assert.equal(agent.runnerId, real, 'stopping names the id the Runner knows');
  assert.equal(agent.toolCount, 1);
  assert.equal(agent.nativeId, 'task-9');
  assert.equal(mergedStatus(agent, { [real]: 'stopped' }), 'stopped');
});

test('a background command is not a sub-agent: no card, a short name, the output the agent read', () => {
  cursor = 0n;
  const script = 'cd /work/repo && source .venv/bin/activate\n# run the MCP tests\nPYTHONPATH=src uv run pytest tests/test_mcp.py -x -q 2>&1 | tail -50';
  let t = fold(emptyTranscript(),
    record(0, { type: 'tool_activity', data: call('toolu_bg', 'Bash', 'running', { input: { command: script, run_in_background: true } }) }),
    record(10, { type: 'agent_status', agentId: 'claude-tool:toolu_bg', parentAgentId: 'root', nativeId: 'bash_7', status: 'running', name: script, agentKind: 'local_bash', data: { toolUseId: 'toolu_bg', taskType: 'local_bash' } }),
    record(20, { type: 'tool_activity', data: call('toolu_read', 'BashOutput', 'running', { input: { bash_id: 'bash_7' } }) }),
    record(30, { type: 'tool_activity', data: { type: 'tool_call', id: 'toolu_read', status: 'completed', output: '12 passed, 1 failed' } }),
  );
  assert.deepEqual(t.messages.filter(m => m.type === 'subagent'), [], 'no card in the conversation');
  const command = t.subagents.byId['claude-tool:toolu_bg'];
  assert.equal(command.kind, 'command');
  assert.equal(command.name, 'pytest tests/test_mcp.py');
  assert.equal(command.command, script);
  assert.equal(command.output, '12 passed, 1 failed');
  assert.deepEqual(subagentTree(t.subagents), []);
  assert.deepEqual(backgroundCommands(t.subagents).map(c => c.id), ['claude-tool:toolu_bg']);
  t = fold(t, record(40, { type: 'agent_status', agentId: 'claude-tool:toolu_bg', status: 'failed', data: { toolUseId: 'toolu_bg', outputFile: '/tmp/tasks/bash_7.output' } }));
  assert.equal(t.subagents.byId['claude-tool:toolu_bg'].status, 'failed');
  assert.equal(t.subagents.byId['claude-tool:toolu_bg'].outputFile, '/tmp/tasks/bash_7.output');
  assert.equal(t.subagents.byId['claude-tool:toolu_bg'].name, 'pytest tests/test_mcp.py', 'a status without a name keeps it');
});

test('background commands are named by what runs, never by the whole script', () => {
  assert.equal(commandTitle('go test ./internal/... -run TestX -count 20'), 'go test ./internal/...');
  assert.equal(commandTitle('npm run build -- --watch'), 'npm run build');
  assert.equal(commandTitle('cd v2 && make test-real'), 'make test-real');
  assert.equal(commandTitle('python -m pytest tests/test_api.py -k slow'), 'pytest tests/test_api.py');
  assert.equal(commandTitle('npx playwright test e2e/login.spec.ts'), 'playwright test');
  assert.equal(commandTitle('export FOO=1\nset -e\ntimeout 600 ./scripts/long-job.sh --all > out.log'), 'long-job.sh');
  assert.equal(commandTitle('sleep 30; curl -s http://localhost:8080/health'), 'curl http://localhost:8080/health');
  assert.ok(!commandTitle('for f in *.go; do\n  gofmt -l $f\ndone').includes('\n'));
});

/** A sub-agent with only what a test names; the rest as the fold starts it. */
const bare = (fields: Partial<Subagent>): Subagent => ({ ...fold(emptyTranscript(), record(0, { type: 'agent_status', agentId: 'claude-tool:bare', status: 'running', name: 'x' })).subagents.byId['claude-tool:bare'], name: '', ...fields });

test('a sub-agent is called by its kind and task; a placeholder is never its title', () => {
  // Claude's Agent tool input: {description, prompt, subagent_type}.
  assert.deepEqual(subagentLabel(bare({ name: 'Review the datasource diff', type: 'Explore' })), { kind: 'Explore', title: 'Review the datasource diff' });
  // A kind that says nothing is no badge.
  assert.deepEqual(subagentLabel(bare({ name: 'Survey adapters', type: 'general-purpose' })), { kind: '', title: 'Survey adapters' });
  // No description: the first line of its instructions, never the Runner's "Sub-agent".
  assert.deepEqual(subagentLabel(bare({ prompt: '\n## Review the diff in /tmp/rev.diff\nLook for regressions.', type: 'Explore' }), 'Sub-agent'), { kind: 'Explore', title: 'Review the diff in /tmp/rev.diff' });
  assert.equal(subagentLabel(bare({}), 'Check the docs').title, 'Check the docs');
  assert.deepEqual(subagentLabel(bare({ type: 'Explore' }), 'Agent'), { kind: '', title: 'Explore sub-agent' });
  assert.deepEqual(subagentLabel(bare({}), 'Sub-agent'), { kind: '', title: 'Sub-agent' });
});

test('real Claude event orders all name the card from the spawning call', () => {
  // A placeholder name in a status update does not replace the task.
  cursor = 0n;
  let t = fold(emptyTranscript(),
    record(0, { type: 'tool_activity', data: call('toolu_r', 'Agent', 'running', { input: { description: 'Review the datasource diff', prompt: 'Review /tmp/rev.diff.', subagent_type: 'Explore' } }) }),
    record(10, { type: 'agent_status', agentId: 'claude-tool:toolu_r', status: 'running', name: 'Sub-agent', data: { toolUseId: 'toolu_r' } }),
  );
  assert.equal(t.subagents.byId['claude-tool:toolu_r'].name, 'Review the datasource diff');

  // Its own events first, under the Runner's attempt prefix, then the spawning call: one card, named.
  cursor = 0n;
  const real = 'attempt-2:claude-tool:toolu_e';
  t = fold(emptyTranscript(),
    record(0, { type: 'activity', agentId: real, parentAgentId: 'root', status: 'working' }),
    record(5, { type: 'tool_activity', agentId: real, data: call('e1', 'Bash', 'running', { input: { command: 'git diff --stat', description: 'Gather diff stats' } }) }),
    record(10, { type: 'tool_activity', data: call('toolu_e', 'Agent', 'running', { input: { description: 'Review the datasource diff', prompt: 'Review it.', subagent_type: 'Explore' } }) }),
    record(20, { type: 'agent_status', agentId: real, status: 'running', data: { toolUseId: 'toolu_e' } }),
  );
  assert.equal(t.messages.filter(m => m.type === 'subagent').length, 1, 'one card');
  const agent = t.subagents.byId[real];
  assert.deepEqual(subagentLabel(agent), { kind: 'Explore', title: 'Review the datasource diff' });
  assert.equal(agent.toolCount, 1);

  // The first report of the call lacks its input (a provider that streams it); a later one carries it.
  cursor = 0n;
  t = fold(emptyTranscript(),
    record(0, { type: 'tool_activity', data: call('toolu_s', 'Task', 'running') }),
    record(5, { type: 'tool_activity', data: call('toolu_s', 'Task', 'running', { input: { description: 'Map the pipeline', prompt: 'Trace it.', subagent_type: 'Explore' } }) }),
  );
  assert.deepEqual(subagentLabel(t.subagents.byId['claude-tool:toolu_s']), { kind: 'Explore', title: 'Map the pipeline' });

  // A journal written with spaces after colons still names its spawns.
  cursor = 0n;
  const spaced = create(JournalEventSchema, { cursor: ++cursor, capturedAt: new Date(T0).toISOString(), event: { type: 'tool_activity', providerDataJson: new TextEncoder().encode(JSON.stringify(call('toolu_p', 'Agent', 'running', { input: { description: 'Spaced', prompt: 'p' } }), null, 1)) } });
  assert.equal(foldTranscript(emptyTranscript(), [spaced]).subagents.byId['claude-tool:toolu_p']?.name, 'Spaced');
});

test('a timeline reads as the chat\'s tool lines: reads grouped, a file read again counted, a failure on its own line', () => {
  cursor = 0n;
  const child = 'claude-tool:toolu_g';
  const read = (id: string, at: number, status = 'completed') => [
    record(at, { type: 'tool_activity', agentId: child, data: call(id, 'Read', 'running', { input: { file_path: '/tmp/rev.diff' } }) }),
    record(at + 1, { type: 'tool_activity', agentId: child, data: call(id, undefined as never, status) }),
  ];
  const t = fold(emptyTranscript(),
    record(0, { type: 'tool_activity', data: call('toolu_g', 'Agent', 'running', { input: { description: 'Review', prompt: 'p' } }) }),
    ...read('r1', 10), ...read('r2', 20), ...read('r3', 30),
    ...read('r4', 40, 'failed'), ...read('r5', 50, 'failed'),
    record(60, { type: 'message_delta', agentId: child, text: 'Found it.' }),
    ...read('r6', 70),
    record(80, { type: 'tool_activity', agentId: child, data: call('r7', 'Read', 'running', { input: { file_path: '/tmp/rev.diff' } }) }),
    record(90, { type: 'tool_activity', agentId: child, data: call('b1', 'Bash', 'running', { input: { command: 'git diff --stat', description: 'Gather diff stats' } }) }),
    record(91, { type: 'tool_activity', agentId: child, data: call('b1', undefined as never, 'completed', { output: ' 2 files changed' }) }),
  );
  const rows = stepRows(t.subagents.byId[child].items);
  const shape = rows.map(row => row.kind !== 'tool' ? row.kind : row.item.kind === 'call' ? `call:${row.item.entry.status}` : `${row.item.kind}:${row.item.entries.length}`);
  assert.deepEqual(shape, ['explore:3', 'call:failed', 'call:failed', 'message', 'explore:2', 'call:completed']);
  const [reads, , , , rereads, bash] = rows as Extract<typeof rows[number], { kind: 'tool' }>[];
  assert.equal(describeGroup(reads.item as never).text, 'Read /tmp/rev.diff ×3');
  assert.equal(describeGroup(rereads.item as never).text, 'Reading /tmp/rev.diff ×2', 'a run with the running call says so');
  assert.deepEqual(rereads.ids, ['r6', 'r7'], 'the running call is found in its row');
  // Under a second says nothing.
  assert.deepEqual(describeTool((bash.item as { entry: never }).entry).meta, []);
  assert.equal(stepRows(t.subagents.byId[child].items), rows, 'rows are computed once per timeline');
});

test('a forked Skill (no task of its own) ends with its call: done, named, its result without the fork line', () => {
  // What an older Claude worker journals for `/code-review` run with `context: fork`:
  // the Skill call, the fork's own steps under it, and the call's result. No agent_status.
  cursor = 0n;
  const fork = 'attempt-0:claude-tool:toolu_skill';
  let t = fold(emptyTranscript(),
    record(0, { type: 'turn_started' }),
    record(10, { type: 'tool_activity', data: call('toolu_skill', 'Skill', 'running', { input: { skill: 'code-review', args: 'medium' } }) }),
    record(1000, { type: 'tool_activity', agentId: fork, data: call('g1', 'Bash', 'running', { input: { command: 'grep -n refusal src' } }) }),
    record(1100, { type: 'tool_activity', agentId: fork, data: call('g1', undefined as never, 'completed', { output: 'src/a.py:1' }) }),
  );
  const id = t.messages.find(m => m.type === 'subagent')!.agentId!;
  assert.equal(t.subagents.byId[id].status, 'running');
  assert.deepEqual(subagentLabel(t.subagents.byId[id]), { kind: 'skill', title: 'code-review' });
  t = fold(t, record(9000, { type: 'tool_activity', data: { type: 'tool_call', id: 'toolu_skill', status: 'completed', output: 'Skill "code-review" completed (forked execution).\n\nResult:\nI found 10 findings.' } }));
  const done = t.subagents.byId[id];
  assert.equal(done.status, 'done');
  assert.equal(done.result, 'I found 10 findings.');
  assert.equal(agentElapsed(done, T0 + 99_999_999), 8000, 'it ran from its first step to its call\'s result');
  // A context report for it the next day is not its own event, and changes nothing.
  t = fold(t, record(86_400_000, { type: 'context', agentId: fork, data: { used: 1 } }));
  assert.equal(agentElapsed(t.subagents.byId[id], T0 + 99_999_999), 8000);
});

test('the end of the main turn ends a foreground sub-agent left running, at its last event; background work runs on', () => {
  cursor = 0n;
  const fg = 'claude-tool:toolu_fg', bg = 'claude-tool:toolu_bg', inner = 'claude-tool:toolu_inner', cmd = 'claude-tool:toolu_cmd', codex = 'codex:thread-1', task = 'claude-task:t9';
  let t = fold(emptyTranscript(),
    record(0, { type: 'turn_started' }),
    record(10, { type: 'tool_activity', data: call('toolu_fg', 'Task', 'running', { input: { description: 'Foreground', prompt: 'p' } }) }),
    record(20, { type: 'tool_activity', data: call('toolu_bg', 'Task', 'running', { input: { description: 'Background', prompt: 'p', run_in_background: true } }) }),
    record(30, { type: 'tool_activity', agentId: bg, data: call('toolu_inner', 'Task', 'running', { input: { description: 'Inside the background one', prompt: 'p' } }) }),
    record(40, { type: 'agent_status', agentId: cmd, nativeId: 'b1', status: 'running', name: 'npm test', agentKind: 'local_bash', data: { toolUseId: 'toolu_cmd' } }),
    record(50, { type: 'agent_status', agentId: codex, nativeId: 'thread-1', status: 'running', name: 'Codex helper', agentKind: 'agent' }),
    record(60, { type: 'agent_status', agentId: task, nativeId: 't9', status: 'running', name: 'Found running', agentKind: 'local_agent' }),
    record(5000, { type: 'tool_activity', agentId: fg, data: call('f1', 'Read', 'running', { input: { file_path: 'a.ts' } }) }),
    record(60_000, { type: 'turn_ended', status: 'interrupted' }),
  );
  const by = t.subagents.byId;
  assert.equal(by[fg].status, 'unreported');
  assert.equal(by[fg].endedAt, T0 + 5000, 'ended at its last own event, not at the turn\'s end');
  assert.equal(agentElapsed(by[fg], T0 + 999_999), 5000 - 10, 'the clock is frozen');
  assert.equal(currentAction(by[fg]), undefined, 'nothing is "now"');
  assert.equal(by[fg].items[0].kind === 'tool' && by[fg].items[0].call.status, 'Result not reported');
  for (const id of [bg, inner, cmd, codex, task]) assert.ok(['running', 'starting'].includes(by[id].status), `${id} outlives the turn`);
  assert.equal(endsWithTurn(by[inner], by), false, 'inside a background sub-agent');
  // A later report that it still runs (moved to the background) is believed.
  t = fold(t, record(61_000, { type: 'agent_status', agentId: fg, nativeId: 'task-fg', status: 'running', data: { toolUseId: 'toolu_fg', backgrounded: true } }));
  assert.equal(t.subagents.byId[fg].status, 'running');
  assert.equal(t.subagents.byId[fg].backgrounded, true);
});

test('the worker\'s word that a turn ended under a sub-agent reads as ended without a report', () => {
  let { t, child } = claudeRun();
  t = fold(t,
    record(2000, { type: 'tool_activity', agentId: child, data: call('c1', 'Read', 'running', { input: { file_path: 'a.ts' } }) }),
    record(9000, { type: 'agent_status', agentId: child, status: 'unknown', data: { reason: 'turn_ended' } }),
  );
  const agent = t.subagents.byId[child];
  assert.equal(agent.status, 'unreported');
  assert.equal(statusWord[agent.status], 'Ended without a report');
  assert.equal(agentElapsed(agent, T0 + 1e9), 9000 - 20, 'its own status report is its last event');
  // Without that reason, unknown is still unknown.
  const other = fold(claudeRun().t, record(9000, { type: 'agent_status', agentId: child, status: 'unknown' }));
  assert.equal(other.subagents.byId[child].status, 'unknown');
});

test('an end nobody dated is put at the last event: never a clock that keeps growing', () => {
  let { t, child } = claudeRun();
  t = fold(t, record(4000, { type: 'tool_activity', agentId: child, data: call('c1', 'Read', 'running', { input: { file_path: 'a.ts' } }) }));
  const agent = t.subagents.byId[child];
  // The Runner says it ended; the journal never did.
  const status = mergedStatus(agent, { [child]: 'unknown' });
  assert.equal(status, 'unknown');
  assert.equal(agentElapsed({ ...agent, status }, T0 + 1e9), 4000 - 20);
  assert.equal(agentElapsed({ ...agent, status: mergedStatus(agent, { [child]: 'unreported' }) }, T0 + 1e9), 4000 - 20);
  // A worker lost hours later: the end is its last event, not the loss.
  t = fold(t, record(3_600_000, { type: 'process_exit', status: 'unknown' }));
  assert.equal(agentElapsed(t.subagents.byId[child], T0 + 1e9), 4000 - 20);
});

test('Stop is offered for a running sub-agent the Runner can address, when the provider can stop one', () => {
  let { t, child } = claudeRun();
  const agent = t.subagents.byId[child];
  const native = new Set([child]);
  assert.equal(canStopSubagent(agent, 'running', { canStop: true, native }), true);
  assert.equal(canStopSubagent(agent, 'waiting', { canStop: true, native }), true);
  assert.equal(canStopSubagent(agent, 'running', { canStop: false, native }), false, 'no stopAgent capability');
  assert.equal(canStopSubagent(agent, 'done', { canStop: true, native }), false, 'ended');
  assert.equal(canStopSubagent(agent, 'unreported', { canStop: true, native }), false, 'ended without a report');
  assert.equal(canStopSubagent(agent, 'running', { canStop: true, native: new Set() }), false, 'the Runner has no provider id for it (a forked Skill)');
  assert.equal(canStopSubagent({ ...agent, runnerId: 'attempt-1:' + child }, 'running', { canStop: true, native: new Set(['attempt-1:' + child]) }), true, 'by the id the Runner knows');
  // The accepted stop is in the journal: it was the person's.
  t = fold(t, record(5000, { type: 'operation_accepted', agentId: child }));
  assert.equal(t.subagents.byId[child].stopRequested, true);
});

test('Clear finished hides what ended, never what runs or what a running one started from', () => {
  cursor = 0n;
  const parent = 'claude-tool:p', child = 'claude-tool:c', done = 'claude-tool:d';
  const t = fold(emptyTranscript(),
    record(0, { type: 'tool_activity', data: call('p', 'Task', 'running', { input: { description: 'Parent' } }) }),
    record(10, { type: 'tool_activity', agentId: parent, data: call('c', 'Task', 'running', { input: { description: 'Child' } }) }),
    record(20, { type: 'tool_activity', data: call('d', 'Task', 'running', { input: { description: 'Done' } }) }),
    record(30, { type: 'tool_activity', data: { type: 'tool_call', id: 'd', status: 'completed', output: 'ok' } }),
  );
  const by = t.subagents.byId;
  const status = (a: Subagent) => a.id === parent ? 'done' as const : a.status;
  assert.deepEqual(finishedWork(Object.values(by), status).sort(), [done, parent].sort());
  const cleared = new Set([parent, done, child]);
  assert.equal(hiddenFromWork(by[done], cleared, status, by), true);
  assert.equal(hiddenFromWork(by[parent], cleared, status, by), false, 'something it started still runs');
  assert.equal(hiddenFromWork(by[child], cleared, status, by), false, 'running work always shows');
  assert.equal(hiddenFromWork(by[done], new Set(), status, by), false);
});

const REJECTED = "The user doesn't want to proceed with this tool use. The tool use was rejected (eg. if it was a file edit, the new_string was NOT written to the file). STOP what you are doing and wait for the user to tell you how to proceed.";
const toolOf = (t: Transcript, id: string) => t.messages.flatMap(m => m.tools ?? []).find(entry => entry.id === id);

test('a call the person stopped reads Stopped, a declined one Denied; a real failure stays a failure', () => {
  cursor = 0n;
  let t = fold(emptyTranscript(),
    record(0, { type: 'operation_accepted', text: 'Review it' }),
    record(10, { type: 'turn_started' }),
    record(20, { type: 'tool_activity', data: call('broken', 'Bash', 'running', { input: { command: 'make test' } }) }),
    record(30, { type: 'tool_activity', data: call('broken', undefined as never, 'failed', { output: 'Exit code 2\nmake: *** [test] Error 2' }) }),
    record(40, { type: 'tool_activity', data: call('skill', 'Skill', 'running', { input: { skill: 'code-review' } }) }),
    // An interrupt as a Runner from before operation kinds journals it: no text, no request, no sub-agent.
    record(50, { type: 'operation_accepted' }),
    record(60, { type: 'tool_activity', data: call('skill', undefined as never, 'failed', { output: REJECTED }) }),
    record(70, { type: 'turn_ended', status: 'interrupted' }),
  );
  assert.equal(toolOf(t, 'broken')?.status, 'failed');
  const stopped = toolOf(t, 'skill')!;
  assert.equal(stopped.status, 'stopped');
  assert.equal(describeTool(stopped).problem, undefined, 'no red problem line');
  assert.match(stopped.output!, /doesn't want to proceed/, 'the provider\'s words stay in the details');
  // Declined in Ask mode: the approval's response is in the journal, and the worker's words say so.
  t = fold(t,
    record(1000, { type: 'operation_accepted', text: 'Edit it' }),
    record(1010, { type: 'turn_started' }),
    record(1020, { type: 'tool_activity', data: call('edit', 'Edit', 'running', { input: { file_path: 'a.ts', old_string: 'a', new_string: 'b' } }) }),
    record(1030, { type: 'approval', requestId: 'r1', data: { tool: 'Edit', toolUseId: 'edit' } }),
    record(1040, { type: 'operation_accepted', requestId: 'r1', data: { response: { kind: 'approve', decision: 'decline' } } }),
    record(1050, { type: 'tool_activity', data: call('edit', undefined as never, 'failed', { output: 'Declined by operator' }) }),
    // An interrupt the Runner names, in a turn with nothing declined.
    record(1060, { type: 'tool_activity', data: call('grep', 'Grep', 'running', { input: { pattern: 'x' } }) }),
  );
  assert.equal(toolOf(t, 'edit')?.status, 'denied');
  t = fold(t,
    record(2000, { type: 'turn_ended' }),
    record(2010, { type: 'turn_started' }),
    record(2020, { type: 'tool_activity', data: call('read', 'Read', 'running', { input: { file_path: 'b.ts' } }) }),
    record(2030, { type: 'operation_accepted', data: { kind: 'interrupt' } }),
    record(2040, { type: 'tool_activity', data: call('read', undefined as never, 'failed', { output: REJECTED }) }),
  );
  assert.equal(toolOf(t, 'read')?.status, 'stopped');
  assert.deepEqual(noteStops(undefined, create(JournalEventSchema, { event: { type: 'operation_accepted', agentId: 'claude-tool:x' } }).event!), undefined, 'a sub-agent stop is not a turn interrupt');
});

test('a sub-agent whose call was stopped is stopped, with everything it started; a background one runs on', () => {
  cursor = 0n;
  const fork = 'attempt-0:claude-tool:toolu_skill', nested = 'claude-tool:toolu_nested', bg = 'claude-tool:toolu_bgn';
  let t = fold(emptyTranscript(),
    record(0, { type: 'turn_started' }),
    record(10, { type: 'tool_activity', data: call('toolu_skill', 'Skill', 'running', { input: { skill: 'code-review' } }) }),
    record(100, { type: 'tool_activity', agentId: fork, data: call('g1', 'Bash', 'running', { input: { command: 'git diff' } }) }),
    record(110, { type: 'tool_activity', agentId: fork, data: call('toolu_nested', 'Task', 'running', { input: { description: 'Check one file' } }) }),
    record(120, { type: 'tool_activity', agentId: fork, data: call('toolu_bgn', 'Task', 'running', { input: { description: 'Long survey', run_in_background: true } }) }),
    record(130, { type: 'tool_activity', agentId: nested, data: call('n1', 'Read', 'running', { input: { file_path: 'a.ts' } }) }),
    record(200, { type: 'operation_accepted', data: { kind: 'interrupt' } }),
    record(210, { type: 'tool_activity', agentId: fork, data: call('g1', undefined as never, 'failed', { output: REJECTED }) }),
    record(220, { type: 'tool_activity', data: call('toolu_skill', undefined as never, 'failed', { output: REJECTED }) }),
  );
  const id = t.messages.find(m => m.type === 'subagent')!.agentId!;
  const by = t.subagents.byId;
  assert.equal(by[id].status, 'stopped');
  assert.equal(by[id].error, '', 'a stop is not an error');
  assert.equal(by[id].result, '');
  assert.equal(by[nested].status, 'stopped');
  assert.equal(by[nested].endedAt, T0 + 130, 'at its last own event');
  assert.equal(by[bg].status, 'starting', 'a background one outlives its parent\'s stop');
  const step = stepRows(by[id].items).find(row => row.kind === 'tool' && row.ids.includes('g1'));
  assert.equal(step?.kind === 'tool' && step.item.kind === 'call' && step.item.entry.status, 'stopped', 'its own stopped step reads Stopped');
  // The turn's end changes nothing more.
  t = fold(t, record(300, { type: 'turn_ended', status: 'interrupted' }));
  assert.equal(t.subagents.byId[id].status, 'stopped');
});

// Past 256, new sub-agents and background commands were dropped: a long
// conversation's latest work was invisible. The oldest finished ones make room.
test('past the limit, the oldest finished sub-agent makes room for a new one; running ones are kept', () => {
  cursor = 0n;
  const status = (id: string, value: string, parent = 'root') => record(0, { type: 'agent_status', agentId: id, parentAgentId: parent, status: value, name: `Task ${id}` });
  const records: JournalEvent[] = [];
  // Two still running, then a parent with a finished child, then finished ones up to the limit.
  records.push(status('codex:run-0', 'running'), status('codex:run-1', 'running'), status('codex:parent', 'running'), status('codex:child', 'running', 'codex:parent'), status('codex:child', 'done'), status('codex:parent', 'done'));
  for (let i = 4; i < SUBAGENT_LIMIT; i++) records.push(status(`codex:done-${i}`, 'running'), status(`codex:done-${i}`, 'done'));
  let t = foldTranscript(emptyTranscript(), records);
  assert.equal(t.subagents.order.length, SUBAGENT_LIMIT);
  assert.equal(t.subagents.truncated, false);
  t = fold(t, status('codex:new', 'running'));
  const kept = t.subagents;
  assert.equal(kept.order.length, SUBAGENT_LIMIT, 'still at the limit');
  assert.ok(kept.byId['codex:new'], 'the new one is in');
  assert.ok(kept.byId['codex:run-0'] && kept.byId['codex:run-1'], 'running ones stay');
  assert.ok(!kept.byId['codex:child'], 'the oldest finished one went');
  assert.deepEqual(kept.byId['codex:parent'].children, [], 'and left its parent');
  assert.ok(!kept.byId['codex:parent'].items.some(item => item.kind === 'agent' && item.agentId === 'codex:child'), 'and its parent\'s timeline');
  assert.equal(kept.truncated, true, 'the list says it is no longer whole');
  // The parent, now without children, goes next.
  t = fold(t, status('codex:newer', 'running'));
  assert.ok(!t.subagents.byId['codex:parent'] && t.subagents.byId['codex:newer']);
});

test('past the limit with everything still running, a new sub-agent is dropped and the list says so', () => {
  cursor = 0n;
  const records: JournalEvent[] = [];
  for (let i = 0; i < SUBAGENT_LIMIT; i++) records.push(record(0, { type: 'agent_status', agentId: `codex:r-${i}`, parentAgentId: 'root', status: 'running' }));
  const t = foldTranscript(emptyTranscript(), [...records, record(1, { type: 'agent_status', agentId: 'codex:extra', parentAgentId: 'root', status: 'running' })]);
  assert.equal(t.subagents.order.length, SUBAGENT_LIMIT);
  assert.ok(!t.subagents.byId['codex:extra']);
  assert.equal(t.subagents.truncated, true);
});

test('retained managed command events remain one command instead of a sub-agent card', () => {
  const id = 'managed-1', agentId = 'attempt-0:claude-tool:' + id;
  const started = fold(emptyTranscript(), record(0, { type: 'tool_activity', data: call(id, 'mcp__lazybox__command', 'running', { input: { action: 'interactive', argv: ['gcloud', 'auth', 'login'] } }) }));
  const t = fold(started,
    record(1, { type: 'agent_status', agentId, status: 'running', agentKind: 'mcp__lazybox__command', name: 'mcp__lazybox__command', data: { toolUseId: id } }),
    record(2, { type: 'message_delta', agentId, text: '{"exitCode":0}' }),
    record(3, { type: 'agent_status', agentId, status: 'done', data: { toolUseId: id } }),
    record(4, { type: 'tool_activity', data: call(id, 'mcp__lazybox__command', 'completed', { output: '{"exitCode":0}' }) }),
  );
  assert.deepEqual(t.subagents.order, []);
  const command = t.messages.flatMap(message => message.tools || []).find(tool => tool.id === id);
  assert.equal(command?.output, 'Completed successfully.');
  assert.deepEqual(describeTool(command!).meta, []);
});

test('an open task panel follows the spawn once the provider links the two IDs', () => {
  const task = 'claude-task:research', spawn = 'claude-tool:call';
  const t = fold(emptyTranscript(),
    record(1, {type: 'agent_status', agentId: task, status: 'running', name: 'Access model document research'}),
    record(2, {type: 'tool_activity', data: call('call', 'Agent', 'running', {input: {description: 'Research', prompt: 'Read the document'}})}),
    record(3, {type: 'agent_status', agentId: task, status: 'running', data: {toolUseId: 'call'}}),
    record(4, {type: 'message_delta', agentId: task, text: 'Found the document'}),
  );
  assert.deepEqual(subagentTree(t.subagents).map(row => row.agent.id), [spawn]);
  assert.equal(panelAgent(t.subagents, task)?.id, spawn);
  assert.equal(panelAgent(t.subagents, task)?.items.length, 1);
});

test('a retained tail starting at a command lifecycle frame never creates a reviewer card', () => {
  for (const name of ['command', 'mcp__lazybox__command']) {
    const agentId = 'attempt-1:claude-tool:shell';
    const started = fold(emptyTranscript(), record(0, {type:'agent_status', agentId, agentKind:name, name, status:'running'}));
    const ended = fold(started,
      record(1, {type:'activity', agentId, status:'working'}),
      record(2, {type:'agent_status', agentId, status:'unknown'}));
    assert.deepEqual(ended.subagents.order, []);
    assert.ok(!ended.messages.some(message => message.type === 'subagent'));
  }
});
