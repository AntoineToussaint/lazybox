import { test } from 'node:test';
import assert from 'node:assert/strict';
import { NativeStatus, limitState } from './status.js';
import type { SDKMessage } from '@anthropic-ai/claude-agent-sdk';
const message = (value: object) => value as SDKMessage;
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
