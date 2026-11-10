import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {create} from '@bufbuild/protobuf';
import {JournalEventSchema,SessionSchema} from '@lazybox/protocol';
import {NativeStatus} from '../workers/claude-sdk/src/status.ts';
import type {SDKMessage} from '@anthropic-ai/claude-agent-sdk';
import {toolActivity} from '../packages/surfaces/src/tool-activity.ts';
import {activityLabel} from '../packages/surfaces/src/activity.ts';
import {fileReference} from '../packages/surfaces/src/file-reference.ts';
const wire=(event:any,cursor=1)=>create(JournalEventSchema,{cursor:BigInt(cursor),event:{...event,providerDataJson:new TextEncoder().encode(JSON.stringify(event.data||{}))}});
test('Claude protocol fixture replays requesting, real compaction, completion and results',async()=>{
 const raw=JSON.parse(await readFile(new URL('../tests/fixtures/providers/claude-activity.json',import.meta.url),'utf8'));
 const adapter=new NativeStatus();const events:ReturnType<typeof wire>[]=[];
 const session=create(SessionSchema,{busy:true,status:'running'});
 const labels=[];
 for(const m of raw){for(const ev of adapter.normalize(m as SDKMessage))events.push(wire(ev,events.length+1));labels.push(activityLabel(events,session));}
 assert.equal(labels[1],'Waiting for provider…');assert.equal(labels[2],'Working…');
 assert.equal(labels[5],'Compacting context…');assert.equal(labels[6],'Working…');assert.equal(labels[7],'Waiting for provider…');assert.equal(labels[8],'');
 const activity=toolActivity(events);assert.deepEqual(activity.available,['Read','Bash']);assert.equal(activity.calls.length,1);assert.equal(activity.calls[0].status,'Completed');assert.equal(activity.calls[0].output,'# Fixture README');
});
test('Codex item lifecycle deduplicates updates, filters reasoning, preserves failure',()=>{
 const events=[wire({type:'tool_activity',status:'running',data:{item:{id:'c',type:'commandExecution',command:'echo fixture'}}}),wire({type:'tool_activity',status:'completed',data:{item:{id:'c',type:'commandExecution',command:'echo fixture',aggregatedOutput:'fixture',exitCode:0}}}),wire({type:'tool_activity',data:{item:{id:'private',type:'reasoning',text:'PRIVATE'}}}),wire({type:'tool_activity',status:'completed',agentId:'codex:child',data:{item:{id:'c',type:'mcpToolCall',tool:'search',error:'failed'}}})];
 const {calls}=toolActivity(events);assert.equal(calls.length,2);assert.equal(calls[0].output,'fixture');assert.equal(calls[0].status,'Completed');assert.equal(calls[1].status,'Failed');assert.ok(!JSON.stringify(calls).includes('PRIVATE'));
});
test('missing tool results remain unknown and child activity cannot replace the root',()=>{
 const events=[wire({type:'tool_activity',data:{type:'tool_use',id:'t',name:'Read',input:{}}}),wire({type:'turn_ended'})];
 assert.equal(toolActivity(events).calls[0].status,'Result not reported');
 const root=[wire({type:'activity',status:'requesting'}),wire({type:'activity',status:'compacting',agentId:'child'})];assert.equal(activityLabel(root),'Waiting for provider…');
});
test('automatic file links do not turn Git references into files',()=>{
 for(const value of ['origin/main','feature/rapidly-social-lemming'])assert.equal(fileReference(value,'automatic'),undefined);
 assert.deepEqual(fileReference('repo/README.md','automatic'),{path:'repo/README.md',line:undefined});assert.ok(fileReference('workspace/repo','explicit'));
});
import {appendTranscript,emptyTranscript} from '../packages/surfaces/src/transcript-model.ts';
test('tool history survives hundreds of streamed text chunks',()=>{
 let state=appendTranscript(emptyTranscript(),wire({type:'tool_activity',data:{type:'tool_use',id:'t',name:'Read',input:{}}},1));
 for(let i=2;i<250;i++)state=appendTranscript(state,wire({type:'message_delta',text:'a'},i));
 assert.equal(state.recent.length,80);assert.equal(toolActivity(state.activity).calls.length,1);
});
test('large tool payloads are bounded independently of the message transcript',()=>{
 const state=appendTranscript(emptyTranscript(),wire({type:'tool_activity',data:{type:'tool_result',tool_use_id:'t',content:'x'.repeat(100000)}}));
 assert.ok(state.activity[0].event!.providerDataJson.length<32768);assert.equal(toolActivity(state.activity).calls[0].status,'Completed');assert.match(String(toolActivity(state.activity).calls[0].output),/truncated/);
});
import {toolTitle,toolDuration,durationLabel} from '../packages/surfaces/src/tool-activity.ts';
test('activity timing uses call/result observations, never turn completion',()=>{
 const event=(data:any,time:string,cursor:number)=>({...wire({type:'tool_activity',data},cursor),capturedAt:time});
 const start=event({type:'tool_call',id:'one',name:'Bash',status:'running',input:{description:'Locate the repository API',command:'rg AddRepository'}},'2026-11-03T16:00:00Z',1);
 const end=event({type:'tool_call',id:'one',status:'completed',output:'found'},'2026-11-03T16:00:02.500Z',2);
 const call=toolActivity([start,end,{...end,cursor:3n,capturedAt:'2026-11-03T16:00:30Z'}]).calls[0];
 assert.equal(toolTitle(call),'Locate the repository API');assert.equal(toolDuration(call,0),2500);assert.equal(durationLabel(toolDuration(call,0)),'2.5s');
 const missing=toolActivity([start,{...wire({type:'turn_ended'},2),capturedAt:'2026-11-03T16:02:00Z'}]).calls[0];assert.equal(toolDuration(missing,Date.now()),undefined);assert.equal(durationLabel(undefined),'—');
 const live=toolActivity([start]).calls[0];assert.equal(toolDuration(live,Date.parse('2026-11-03T16:01:05Z')),65000);assert.equal(durationLabel(65000),'1m 5s');
 assert.equal(durationLabel(-1),'—');assert.equal(durationLabel(100),'<1s');assert.equal(durationLabel(3600000),'1h 0m');
 const unknown=toolActivity([wire({type:'tool_activity',data:{type:'tool_use',id:'untimed',name:'Read',input:{file_path:'repo/README.md'}}})]).calls[0];assert.equal(toolDuration(unknown,Date.now()),undefined);assert.equal(toolTitle(unknown),'repo/README.md');
});
