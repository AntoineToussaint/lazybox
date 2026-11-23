import {test} from 'node:test';
import assert from 'node:assert/strict';
import type {SDKControlGetContextUsageResponse} from '@anthropic-ai/claude-agent-sdk';
import {contextReporter,contextSnapshot} from './context.js';
test('context categories preserve provider classification without private paths or deferred inflation',()=>{
 const report={model:'fixture',totalTokens:100,maxTokens:900,rawMaxTokens:1000,categories:[
  {id:'messages',name:'Messages',tokens:100,kind:'used'},
  {id:'mcp_tools_deferred',name:'MCP tools',tokens:500,kind:'deferred'},
  {id:'autocompact_buffer',name:'Buffer',tokens:100,kind:'buffer'},
  {id:'free_space',name:'Free',tokens:800,kind:'free'},
 ],memoryFiles:[{path:'/private/file'}]} as SDKControlGetContextUsageResponse;
 const result=contextSnapshot(report);
 assert.equal(result.used,100);assert.equal(result.capacity,1000);assert.equal(result.reserve,100);
 assert.equal(result.categories.filter(c=>c.included).reduce((n,c)=>n+c.tokens,0),100);
 assert.equal(result.source,'sdk_summary_estimate');assert.ok(!JSON.stringify(result).includes('/private'));
});
test('detail remains attached to categories and cannot inflate used tokens',()=>{
 const report={model:'fixture',totalTokens:100,rawMaxTokens:1000,categories:[{id:'system_tools',tokens:50,kind:'used'},{id:'mcp_tools_deferred',tokens:900,kind:'deferred'},{id:'memory_files',tokens:50,kind:'used'}],systemTools:[{name:'Read',tokens:50}],mcpTools:[{name:'search',serverName:'docs',isLoaded:false,tokens:900}],memoryFiles:[{path:'/private/account/repo/AGENTS.md',type:'Project',tokens:50}]} as SDKControlGetContextUsageResponse;
 const result=contextSnapshot(report);
 assert.deepEqual(result.categories[0].details,[{name:'Read',tokens:50}]);
 assert.equal(result.categories[1].included,false);assert.equal(result.used,100);
 assert.deepEqual(result.categories[2].details,[{name:'Project · AGENTS.md',tokens:50}]);assert.ok(!JSON.stringify(result).includes('/private/'));
});

test('root usage during a turn refreshes categories without erasing the last estimate', async () => {
 const report = {model:'fixture',totalTokens:10,categories:[{id:'messages',tokens:10,kind:'used'}]} as SDKControlGetContextUsageResponse;
 const reads: Array<(value: SDKControlGetContextUsageResponse)=>void> = [];
 const snapshots: unknown[] = [];
 const update = contextReporter(() => new Promise(resolve => reads.push(resolve)), data => snapshots.push(data));
 update({used:10});
 assert.deepEqual(snapshots,[{used:10}]);
 reads.shift()!(report);
 await new Promise(resolve => setImmediate(resolve));
 update({used:11});
 update({used:12});
 assert.equal(reads.length,1);
 assert.deepEqual(snapshots.at(-1),contextSnapshot(report));
 reads.shift()!(report);
 await new Promise(resolve => setImmediate(resolve));
 assert.equal(reads.length,1);
 reads.shift()!({...report,totalTokens:12});
 await new Promise(resolve => setImmediate(resolve));
 assert.deepEqual(snapshots.at(-1),contextSnapshot({...report,totalTokens:12}));
});

test('a failed summary refresh keeps the last detailed observation and retries on usage', async () => {
 const report = {model:'fixture',totalTokens:10,categories:[{id:'messages',tokens:10,kind:'used'}]} as SDKControlGetContextUsageResponse;
 let calls = 0;
 const snapshots: unknown[] = [];
 const update = contextReporter(async () => {
  calls++;
  if (calls === 2) throw new Error('unavailable');
  return {...report,totalTokens:calls * 10};
 }, data => snapshots.push(data));
 for (const used of [10,20,30]) {
  update({used});
  await new Promise(resolve => setImmediate(resolve));
 }
 assert.deepEqual(snapshots,[{used:10},contextSnapshot(report),contextSnapshot({...report,totalTokens:30})]);
});
