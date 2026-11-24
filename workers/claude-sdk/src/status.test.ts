import { test } from 'node:test';
import assert from 'node:assert/strict';
import { NativeStatus, limitState } from './status.js';
import type { SDKMessage } from '@anthropic-ai/claude-agent-sdk';
const message = (value: object) => value as SDKMessage;
test('a completed root turn forks at its last assistant UUID and clears it before the next turn', () => {
  const status = new NativeStatus();
  const assistant = (uuid: string, parent_tool_use_id?: string) => status.normalize(message({
    type: 'assistant', uuid, parent_tool_use_id, message: { content: [], model: 'fixture' },
  }));
  const end = (stop_reason: string) => status.normalize(message({ type: 'result', uuid: 'result-id', subtype: 'success', stop_reason }))
    .find(event => event.type === 'turn_ended');
  assistant('earlier');
  assistant('last-root');
  assistant('child-only', 'agent-tool');
  assert.equal(end('end_turn')?.turnId, 'last-root');
  assert.equal(end('end_turn')?.turnId, undefined);
  assistant('unfinished');
  assert.equal(end('tool_use')?.turnId, undefined);
});

test('child output and task completion cannot replace the main conversation', () => {
  const status = new NativeStatus();
  status.normalize(message({ type: 'system', subtype: 'init', session_id: 'root-native' }));
  const child = status.normalize(message({ type: 'stream_event', session_id: 'child-native', parent_tool_use_id: 'tool-1', event: { type: 'content_block_delta', delta: { type: 'text_delta', text: 'child text' } } }));
  assert.equal(status.reference, 'root-native'); assert.equal(child[0].agentId, 'claude-tool:tool-1');
  const started = status.normalize(message({ type: 'system', subtype: 'task_started', task_id: 'task-1', tool_use_id: 'tool-1', run_id: '002', description: 'Review', task_type: 'local_agent' }));
  assert.equal(started[0].agentId, child[0].agentId); assert.equal(started[0].status, 'running');
  assert.deepEqual(status.normalize(message({ type: 'system', subtype: 'task_notification', task_id: 'task-1', run_id: '001', status: 'completed' })), []);
  assert.equal(status.normalize(message({ type: 'system', subtype: 'task_notification', task_id: 'task-1', run_id: '002', status: 'completed' }))[0].status, 'done');
});
test('native quota remains distinct from context and local budget failures', () => {
  const status = new NativeStatus();
  const quota = status.normalize(message({ type: 'rate_limit_event', rate_limit_info: { status: 'rejected', rateLimitType: 'five_hour', resetsAt: 100, utilization: 0.85 } }));
  assert.equal(quota[0].account?.state, 'exhausted'); assert.equal(quota[0].account?.resetAt, 100); assert.equal(quota[0].account?.windows?.[0].usedPercent, 85);
  const coarse = status.normalize(message({ type: 'assistant', error: 'rate_limit', message: { content: [] } }));
  assert.equal(coarse.find(e => e.type === 'failure')?.account?.state, 'exhausted');
  const result = status.normalize(message({ type: 'result', is_error: true, subtype: 'error_during_execution', errors: ['generic result'], uuid: 'result' }));
  assert.equal(result.filter(e => e.type === 'failure').length, 0);
  assert.equal(limitState('error_max_turns'), 'budget_exceeded'); assert.equal(limitState('context_length_exceeded'), 'context_full'); assert.equal(limitState('authentication_failed'), 'auth_required');
});
test('background membership reconciles a missing terminal event', () => {
  const status = new NativeStatus();
  status.normalize(message({ type: 'system', subtype: 'background_tasks_changed', tasks: [{ task_id: 'task', task_type: 'local_agent', description: 'Test' }] }));
  assert.equal(status.normalize(message({ type: 'system', subtype: 'background_tasks_changed', tasks: [] }))[0].status, 'unknown');
});


test('unknown quota sentinel is unavailable rather than zero', () => {
 const status = new NativeStatus();
 const events = status.normalize(message({ type: 'rate_limit_event', rate_limit_info: { status: 'allowed', rateLimitType: 'five_hour', utilization: -1 } }));
 assert.equal(events[0].account?.windows, undefined);
});

test('thinking and tool phases are observable without exposing reasoning text',()=>{
 const status=new NativeStatus();
 const thinking=status.normalize(message({type:'stream_event',event:{type:'content_block_start',index:0,content_block:{type:'thinking',thinking:''}}}));
 assert.equal(thinking.find(e=>e.type==='activity')?.status,'thinking');
 const privateDelta=status.normalize(message({type:'stream_event',event:{type:'content_block_delta',index:0,delta:{type:'thinking_delta',thinking:'PRIVATE_REASONING'}}}));
 assert.equal(JSON.stringify(privateDelta).includes('PRIVATE_REASONING'),false);
 const tool=status.normalize(message({type:'stream_event',event:{type:'content_block_start',index:1,content_block:{type:'tool_use',name:'Bash',id:'t',input:{}}}}));
 assert.equal(tool.find(e=>e.type==='activity')?.name,'Bash');
 const text=status.normalize(message({type:'stream_event',event:{type:'content_block_delta',index:2,delta:{type:'text_delta',text:'first chunk'}}}));
 assert.equal(text.find(e=>e.type==='message_delta')?.text,'first chunk');
});
test('tool result previews are bounded before journaling',()=>{
 const status=new NativeStatus();const result=status.normalize(message({type:'user',message:{content:[{type:'tool_result',tool_use_id:'big',content:'x'.repeat(1000000)}]}}));
 const text=JSON.stringify(result);assert.ok(text.length<34000);assert.match(text,/Tool preview truncated/);
});

test('a sub-agent\'s tool calls, results and text are attributed through parent_tool_use_id', () => {
  const status = new NativeStatus();
  status.normalize(message({ type: 'system', subtype: 'init', session_id: 'root-native' }));
  const call = status.normalize(message({ type: 'assistant', parent_tool_use_id: 'spawn', message: { id: 'm1', model: 'm', content: [{ type: 'text', text: 'Looking at the reader.' }, { type: 'thinking', thinking: 'PRIVATE' }, { type: 'tool_use', id: 'read-1', name: 'Read', input: { file_path: 'a.ts' } }] } }));
  assert.equal(JSON.stringify(call).includes('PRIVATE'), false);
  const text = call.find(e => e.type === 'message_delta');
  assert.deepEqual([text?.agentId, text?.parentAgentId, text?.text], ['claude-tool:spawn', 'root', 'Looking at the reader.']);
  const tool = call.find(e => e.type === 'tool_activity');
  assert.equal(tool?.agentId, 'claude-tool:spawn'); assert.equal((tool?.data as any).id, 'read-1');
  const result = status.normalize(message({ type: 'user', parent_tool_use_id: 'spawn', message: { content: [{ type: 'tool_result', tool_use_id: 'read-1', content: 'ok' }] } }));
  assert.equal(result[0].agentId, 'claude-tool:spawn'); assert.equal((result[0].data as any).status, 'completed');
  // The main agent's text is never taken from whole messages: it streams.
  const root = status.normalize(message({ type: 'assistant', parent_tool_use_id: null, message: { id: 'm2', model: 'm', content: [{ type: 'text', text: 'root text' }] } }));
  assert.equal(root.some(e => e.type === 'message_delta'), false);
});
test('streamed sub-agent text is not repeated by its complete message', () => {
  const status = new NativeStatus();
  const streamed = status.normalize(message({ type: 'stream_event', parent_tool_use_id: 'spawn', event: { type: 'content_block_delta', delta: { type: 'text_delta', text: 'streamed' } } }));
  assert.equal(streamed.filter(e => e.type === 'message_delta').length, 1);
  const whole = status.normalize(message({ type: 'assistant', parent_tool_use_id: 'spawn', message: { id: 'm', model: 'm', content: [{ type: 'text', text: 'streamed' }] } }));
  assert.equal(whole.filter(e => e.type === 'message_delta').length, 0);
  const next = status.normalize(message({ type: 'assistant', parent_tool_use_id: 'spawn', message: { id: 'm2', model: 'm', content: [{ type: 'text', text: 'forwarded' }] } }));
  assert.equal(next.find(e => e.type === 'message_delta')?.text, 'forwarded');
});
test('task events carry the brief, progress, usage and outcome', () => {
  const status = new NativeStatus();
  const [started] = status.normalize(message({ type: 'system', subtype: 'task_started', task_id: 't', tool_use_id: 'spawn', description: 'Find the bug', subagent_type: 'Explore', task_type: 'local_agent', prompt: 'p'.repeat(9000), is_backgrounded: false, spawn_depth: 1 }));
  assert.equal(started.agentId, 'claude-tool:spawn'); assert.equal(started.name, 'Find the bug');
  const data = started.data as any;
  assert.equal(data.toolUseId, 'spawn'); assert.equal(data.subagentType, 'Explore'); assert.equal(data.backgrounded, false); assert.equal(data.depth, 1); assert.equal(data.taskType, 'local_agent');
  assert.equal(data.prompt.length, 8001); assert.ok(data.prompt.endsWith('…'));
  const [progress] = status.normalize(message({ type: 'system', subtype: 'task_progress', task_id: 't', tool_use_id: 'spawn', description: 'Find the bug', usage: { total_tokens: 1200, tool_uses: 3, duration_ms: 4000 }, last_tool_name: 'Grep', summary: 'Reading the reader' }));
  assert.equal(progress.status, 'running');
  assert.deepEqual(progress.data, { toolUseId: 'spawn', usage: { totalTokens: 1200, toolUses: 3, durationMs: 4000 }, lastTool: 'Grep', summary: 'Reading the reader' });
  const [done] = status.normalize(message({ type: 'system', subtype: 'task_notification', task_id: 't', tool_use_id: 'spawn', status: 'failed', output_file: '', summary: 'It broke', usage: { total_tokens: 2000, tool_uses: 5, duration_ms: 9000 } }));
  assert.equal(done.status, 'failed');
  assert.deepEqual(done.data, { toolUseId: 'spawn', summary: 'It broke', usage: { totalTokens: 2000, toolUses: 5, durationMs: 9000 } });
  // A nested spawn names its parent.
  const [nested] = status.normalize(message({ type: 'system', subtype: 'task_started', task_id: 'n', tool_use_id: 'inner', parent_task_id: 't', description: 'Inner' }));
  assert.equal(nested.parentAgentId, 'claude-tool:spawn');
  // A background command says where its output is kept.
  status.normalize(message({ type: 'system', subtype: 'task_started', task_id: 'b', tool_use_id: 'bash-1', description: 'pytest tests', task_type: 'local_bash' }));
  const [bash] = status.normalize(message({ type: 'system', subtype: 'task_notification', task_id: 'b', tool_use_id: 'bash-1', status: 'completed', output_file: '/tmp/tasks/b.output', summary: '' }));
  assert.equal((bash.data as { outputFile?: string }).outputFile, '/tmp/tasks/b.output');
});
test('task_updated moves a known task and reports its error, once', () => {
  const status = new NativeStatus();
  assert.deepEqual(status.normalize(message({ type: 'system', subtype: 'task_updated', task_id: 'unknown', patch: { status: 'killed' } })), []);
  status.normalize(message({ type: 'system', subtype: 'task_started', task_id: 't', tool_use_id: 'spawn', run_id: '002', description: 'Work' }));
  assert.deepEqual(status.normalize(message({ type: 'system', subtype: 'task_updated', task_id: 't', patch: { status: 'running' } })), [], 'no change, no event');
  assert.deepEqual(status.normalize(message({ type: 'system', subtype: 'task_updated', task_id: 't', run_id: '001', patch: { status: 'killed' } })), [], 'a late run is ignored');
  assert.deepEqual(status.normalize(message({ type: 'system', subtype: 'task_updated', task_id: 't', patch: { status: 'paused' } })), []);
  const [killed] = status.normalize(message({ type: 'system', subtype: 'task_updated', task_id: 't', run_id: '002', patch: { status: 'killed' } }));
  assert.deepEqual([killed.agentId, killed.status], ['claude-tool:spawn', 'stopped']);
  assert.deepEqual(status.normalize(message({ type: 'system', subtype: 'task_updated', task_id: 't', run_id: '002', patch: { status: 'running' } })), [], 'a finished run stays finished');
  const [failed] = status.normalize(message({ type: 'system', subtype: 'task_updated', task_id: 't', patch: { status: 'failed', error: 'boom' } }));
  assert.deepEqual([failed.status, failed.data], ['failed', { error: 'boom' }]);
  for (const [patch, expected] of [['completed', 'done'], ['pending', 'starting']]) {
    const s = new NativeStatus();
    s.normalize(message({ type: 'system', subtype: 'task_started', task_id: 'x', description: 'X' }));
    assert.equal(s.normalize(message({ type: 'system', subtype: 'task_updated', task_id: 'x', patch: { status: patch } }))[0].status, expected);
  }
});

test('a compaction says how far the context shrank; a sub-agent compaction does not', () => {
  const status = new NativeStatus();
  const events = status.normalize(message({ type: 'system', subtype: 'compact_boundary', compact_metadata: { trigger: 'manual', pre_tokens: 156000, post_tokens: 61000, duration_ms: 4200 } }));
  assert.deepEqual(events.find(e => e.type === 'compacted')?.data, { trigger: 'manual', preTokens: 156000, postTokens: 61000, durationMs: 4200 });
  const auto = status.normalize(message({ type: 'system', subtype: 'compact_boundary', compact_metadata: { trigger: 'auto', pre_tokens: 190000 } }));
  assert.deepEqual(auto.find(e => e.type === 'compacted')?.data, { trigger: 'auto', preTokens: 190000 });
  const child = status.normalize(message({ type: 'system', subtype: 'compact_boundary', parent_tool_use_id: 'tool-9', compact_metadata: { trigger: 'auto', pre_tokens: 1 } }));
  assert.equal(child.some(e => e.type === 'compacted'), false);
});

test("a slash command's printed output is the agent's answer, bounded", () => {
  const status = new NativeStatus();
  const [said] = status.normalize(message({ type: 'system', subtype: 'local_command_output', content: 'Session cost: $0.12' }));
  assert.equal(said.type, 'message_delta'); assert.equal(said.text, 'Session cost: $0.12');
  const [long] = status.normalize(message({ type: 'system', subtype: 'local_command_output', content: 'x'.repeat(70000) }));
  assert.ok(long.text!.length < 66000 && long.text!.endsWith('[Output truncated]'));
  assert.deepEqual(status.normalize(message({ type: 'system', subtype: 'local_command_output', content: '' })), []);
});

test('a forked Skill is a sub-agent from its first word to its call\'s result', () => {
  // Claude reports no task for a Skill run with `context: fork`: only the
  // call, the child's own messages under it, and the call's result.
  const status = new NativeStatus();
  status.normalize(message({ type: 'assistant', parent_tool_use_id: null, message: { id: 'm1', model: 'm', content: [{ type: 'tool_use', id: 'skill-1', name: 'Skill', input: { skill: 'code-review', args: 'medium' } }] } }));
  const first = status.normalize(message({ type: 'assistant', parent_tool_use_id: 'skill-1', message: { id: 'm2', model: 'm', content: [{ type: 'tool_use', id: 'grep-1', name: 'Bash', input: { command: 'grep -n x' } }] } }));
  assert.deepEqual(first[0], { sessionId: '', type: 'agent_status', agentId: 'claude-tool:skill-1', parentAgentId: 'root', status: 'running', name: 'code-review', agentKind: 'skill', data: { toolUseId: 'skill-1' } }, 'its start comes before its first step');
  assert.equal(first.filter(e => e.type === 'agent_status').length, 1);
  const again = status.normalize(message({ type: 'user', parent_tool_use_id: 'skill-1', message: { content: [{ type: 'tool_result', tool_use_id: 'grep-1', content: 'x' }] } }));
  assert.equal(again.some(e => e.type === 'agent_status'), false, 'said once');
  const end = status.normalize(message({ type: 'user', parent_tool_use_id: null, message: { content: [{ type: 'tool_result', tool_use_id: 'skill-1', content: 'Skill "code-review" completed (forked execution).' }] } }));
  assert.deepEqual(end.filter(e => e.type === 'agent_status').map(e => [e.agentId, e.status]), [['claude-tool:skill-1', 'done']]);
  const turn = status.normalize(message({ type: 'result', subtype: 'success', is_error: false, uuid: 'r', modelUsage: {} }));
  assert.equal(turn.some(e => e.type === 'agent_status'), false, 'nothing is left to end');
});

test('a Task\'s own events before its task_started do not make a forked run', () => {
  const status = new NativeStatus();
  status.normalize(message({ type: 'assistant', parent_tool_use_id: null, message: { id: 'm1', model: 'm', content: [{ type: 'tool_use', id: 'spawn', name: 'Task', input: { description: 'Find' } }] } }));
  const early = status.normalize(message({ type: 'assistant', parent_tool_use_id: 'spawn', message: { id: 'm2', model: 'm', content: [{ type: 'text', text: 'hi' }] } }));
  assert.equal(early.some(e => e.type === 'agent_status'), false);
});

test('the end of a turn ends what it blocked on, never background work', () => {
  const status = new NativeStatus();
  const start = (task: string, call: string, extra: object) => status.normalize(message({ type: 'system', subtype: 'task_started', task_id: task, tool_use_id: call, description: task, task_type: 'local_agent', ...extra }));
  start('fg', 'call-fg', { is_backgrounded: false });
  start('bg', 'call-bg', { is_backgrounded: true });
  start('moved', 'call-moved', { is_backgrounded: false });
  start('unsaid', 'call-unsaid', {});
  start('cmd', 'call-cmd', { is_backgrounded: false, task_type: 'local_bash' });
  start('done', 'call-done', { is_backgrounded: false });
  status.normalize(message({ type: 'system', subtype: 'task_notification', task_id: 'done', tool_use_id: 'call-done', status: 'completed', summary: '' }));
  // Moved to the background while it ran: the clients are told, and it outlives the turn.
  const [moved] = status.normalize(message({ type: 'system', subtype: 'task_updated', task_id: 'moved', patch: { is_backgrounded: true } }));
  assert.deepEqual([moved.agentId, moved.status, moved.data], ['claude-tool:call-moved', 'running', { backgrounded: true }]);
  // A forked run still going when the turn ends (an interrupted turn) ends too.
  status.normalize(message({ type: 'assistant', parent_tool_use_id: null, message: { id: 'm1', model: 'm', content: [{ type: 'tool_use', id: 'skill', name: 'Skill', input: { skill: 'review' } }] } }));
  status.normalize(message({ type: 'assistant', parent_tool_use_id: 'skill', message: { id: 'm2', model: 'm', content: [{ type: 'text', text: 'Looking' }] } }));
  const turn = status.normalize(message({ type: 'result', subtype: 'error_during_execution', is_error: true, uuid: 'r', modelUsage: {}, errors: [] }));
  const ended = turn.filter(e => e.type === 'agent_status');
  assert.deepEqual(ended.map(e => [e.agentId, e.status, (e.data as { reason?: string }).reason]), [
    ['claude-tool:skill', 'unknown', 'turn_ended'], ['claude-tool:call-fg', 'unknown', 'turn_ended'], ['claude-tool:call-cmd', 'unknown', 'turn_ended'],
  ]);
  assert.ok(turn.findIndex(e => e.type === 'turn_ended') > turn.findIndex(e => e.type === 'agent_status'), 'ended before the turn says it ended');
  // Said once: the next turn's end has nothing left to end.
  assert.equal(status.normalize(message({ type: 'result', subtype: 'success', is_error: false, uuid: 'r2', modelUsage: {} })).some(e => e.type === 'agent_status'), false);
  // A task the level lists as background is never ended by a turn.
  const level = new NativeStatus();
  level.normalize(message({ type: 'system', subtype: 'task_started', task_id: 'x', tool_use_id: 'cx', description: 'x', task_type: 'local_agent', is_backgrounded: false }));
  const [listed] = level.normalize(message({ type: 'system', subtype: 'background_tasks_changed', tasks: [{ task_id: 'x', task_type: 'local_agent', description: 'x' }] }));
  assert.deepEqual(listed.data, { backgrounded: true });
  assert.equal(level.normalize(message({ type: 'result', subtype: 'success', is_error: false, uuid: 'r', modelUsage: {} })).some(e => e.type === 'agent_status'), false);
});

test('managed command frames never create a forked agent', () => {
  const status = new NativeStatus();
  status.normalize(message({ type: 'assistant', parent_tool_use_id: null, message: { id: 'm1', model: 'm', content: [
    { type: 'tool_use', id: 'shell-1', name: 'mcp__lazybox__command', input: { action: 'interactive', argv: ['gcloud', 'auth', 'login'] } },
  ] } }));
  const child = status.normalize(message({ type: 'assistant', parent_tool_use_id: 'shell-1', message: { id: 'm2', model: 'm', content: [
    { type: 'text', text: '{"exitCode":0}' },
  ] } }));
  assert.deepEqual(child, []);
  const result = status.normalize(message({ type: 'user', parent_tool_use_id: null, message: { content: [
    { type: 'tool_result', tool_use_id: 'shell-1', content: '{"exitCode":0}' },
  ] } }));
  assert.ok(result.some(event => event.type === 'tool_activity' && !event.agentId));
});

test('late progress and start notifications cannot restart a completed task', () => {
  const status = new NativeStatus();
  const task = { type: 'system', task_id: 't', tool_use_id: 'spawn', run_id: '002' };
  status.normalize(message({ ...task, subtype: 'task_started', description: 'Audit' }));
  status.normalize(message({ ...task, subtype: 'task_notification', status: 'completed', summary: 'Done' }));
  for (const subtype of ['task_progress', 'task_started']) {
    for (const run_id of ['002', undefined]) {
      assert.deepEqual(status.normalize(message({ ...task, subtype, run_id, description: 'Audit' })), []);
    }
  }
  assert.equal(status.normalize(message({ ...task, subtype: 'task_started', run_id: '003', description: 'Retry' }))[0].status, 'running');
});

test('a sign-in Claude cannot keep is auth required, a Keychain refusal included, never a provider error', () => {
  // Claude Code 2.1.329 (SDK 0.3.329): a Keychain that refuses a renewed login
  // says invalid_request (no HTTP status), with the typed cause in api_error.
  const keychain = new NativeStatus();
  const failure = keychain.normalize(message({ type: 'assistant', error: 'invalid_request', api_error: 'oauth_refresh_store_refused', is_api_error_message: true, parent_tool_use_id: null, message: { id: 'm', model: '<synthetic>', content: [{ type: 'text', text: "Could not refresh your login: can't save to the Keychain" }] } })).find(e => e.type === 'failure');
  assert.equal(failure?.status, 'auth_required');
  assert.deepEqual(failure?.account, { state: 'auth_required', reason: 'oauth_refresh_store_refused' });
  assert.equal(failure?.text, 'oauth_refresh_store_refused', 'the typed cause, which the client words; never the coarse invalid_request');
  // The result that ends the turn adds no second, generic failure.
  const ended = keychain.normalize(message({ type: 'result', subtype: 'success', is_error: true, api_error_status: null, api_error: 'oauth_refresh_store_refused', result: "Could not refresh your login: can't save to the Keychain", uuid: 'r' }));
  assert.equal(ended.filter(e => e.type === 'failure').length, 0);
  // A result alone (no assistant error first) still says auth required.
  const alone = new NativeStatus().normalize(message({ type: 'result', subtype: 'success', is_error: true, api_error_status: null, api_error: 'oauth_refresh_store_refused', result: 'x', uuid: 'r' })).find(e => e.type === 'failure');
  assert.equal(alone?.status, 'auth_required'); assert.equal(alone?.account?.reason, 'oauth_refresh_store_refused');
  // An expired or missing sign-in, as before.
  for (const [error, cause] of [['authentication_failed', undefined], ['invalid_request', 'gateway_signin_required'], ['invalid_request', 'gateway_session_expired']]) {
    const event = new NativeStatus().normalize(message({ type: 'assistant', error, ...(cause ? { api_error: cause } : {}), message: { id: 'm', model: 'm', content: [] } })).find(e => e.type === 'failure');
    assert.equal(event?.status, 'auth_required', `${error}/${cause}`);
  }
  assert.equal(new NativeStatus().normalize(message({ type: 'auth_status', isAuthenticating: false, output: [], error: 'Login expired' })).find(e => e.type === 'failure')?.status, 'auth_required');
  // A cause with no state of its own leaves the coarse error in charge: a malformed request is not a sign-in.
  const media = new NativeStatus().normalize(message({ type: 'assistant', error: 'invalid_request', api_error: 'media_removed', message: { id: 'm', model: 'm', content: [] } })).find(e => e.type === 'failure');
  assert.equal(media?.status, 'unknown'); assert.equal(media?.text, 'invalid_request');
  assert.equal(new NativeStatus().normalize(message({ type: 'assistant', error: 'invalid_request', message: { id: 'm', model: 'm', content: [] } })).find(e => e.type === 'failure')?.status, 'unknown');
});

test('a hook heartbeat (tool_progress naming no tool call) reports nothing', () => {
  // SDK 0.3.323: a turn waiting on a long hook sends tool_progress heartbeats whose
  // tool_use_id begins hook- and whose parent_tool_use_id may name no tool call.
  const status = new NativeStatus();
  status.normalize(message({ type: 'system', subtype: 'init', session_id: 'root' }));
  assert.deepEqual(status.normalize(message({ type: 'tool_progress', tool_use_id: 'hook-1', tool_name: 'PreToolUse', parent_tool_use_id: 'hook-parent', elapsed_time_seconds: 5, heartbeat: true })), []);
  assert.deepEqual(status.normalize(message({ type: 'tool_progress', tool_use_id: 'hook-2', tool_name: 'Stop', parent_tool_use_id: null, elapsed_time_seconds: 5, heartbeat: true })), []);
});
