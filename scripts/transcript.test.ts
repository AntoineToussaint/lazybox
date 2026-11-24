import test from 'node:test';
import assert from 'node:assert/strict';
import { create } from '@bufbuild/protobuf';
import { JournalEventSchema } from '@lazybox/protocol';
import { appendTranscript, emptyTranscript } from '../packages/surfaces/src/transcript-model.ts';
import { agentCliStatus } from '../packages/surfaces/src/lazybox-cli.ts';
test('diagnostic limits never truncate conversation text or prompts', () => {
 let state=emptyTranscript();
 const add=(type:string,text:string,turnId='turn')=>{state=appendTranscript(state,create(JournalEventSchema,{cursor:state.cursor+1n,event:{type,text,turnId}}));};
 add('operation_accepted','prompt');
 for(let i=0;i<2000;i++)add('message_delta','x');
 assert.equal(state.messages[0].text,'prompt');assert.equal(state.messages[1].text.length,2000);assert.equal(state.recent.length,80);
 add('turn_ended','');add('message_delta','next');assert.equal(state.messages.length,3);
 add('message_delta','other turn','second');assert.equal(state.messages.length,4);
 const duplicate=state.recent.at(-1)!;assert.equal(appendTranscript(state,duplicate),state);
});

import {fileReference} from '../packages/surfaces/src/file-reference.ts';
test('file links preserve line anchors and reject executable schemes',()=>{
 assert.deepEqual(fileReference('handbook/concepts/glossary.md:12'),{path:'handbook/concepts/glossary.md',line:12});
 assert.deepEqual(fileReference('README.md#L2'),{path:'README.md',line:2});
 assert.deepEqual(fileReference('/workspace/api/a%20b.ts:5:2'),{path:'/workspace/api/a b.ts',line:5});
 for(const value of ['javascript:alert(1)','https://example.com/a.md','data:text/plain,a','not a file'])assert.equal(fileReference(value),undefined);
 // Inline code: a dotted name is a file only when it ends like one; a path or a line number says so itself.
 for(const value of ['rate.limit','console.log','v1.2','user.name','e.g','Object.keys'])assert.equal(fileReference(value,'automatic'),undefined,value);
 for(const value of ['app.ts','README.md','README','go.mod','package.json','main.go:12','src/rate.limit','build.gradle'])assert.ok(fileReference(value,'automatic'),value);
});

import {ConversationReader,conversationReader,continueConversation} from '../packages/surfaces/src/transcript-cache.ts';
import type {RunnerClient} from '@lazybox/client-core';
import {conversationDetails,sessionModelLabel,sessionAccountLabel} from '../packages/surfaces/src/conversation-model.ts';
import {SessionSchema,ProfileSchema} from '@lazybox/protocol';
test('reader retains transcript and resumes from its cursor when remounted',async()=>{
 const cursors:bigint[]=[];let event=1;
 const api={async *watchEvents({after}:{after:bigint},{signal}:{signal:AbortSignal}){cursors.push(after);yield create(JournalEventSchema,{cursor:BigInt(event++),event:{type:'message_delta',text:'answer'}});await new Promise(resolve=>signal.addEventListener('abort',resolve,{once:true}));}} as unknown as RunnerClient;
 const reader=conversationReader(api,'chat');const unsubscribe=reader.subscribe(()=>{});await new Promise(resolve=>setTimeout(resolve,5));unsubscribe();
 assert.equal(reader.getSnapshot().messages[0].text,'answer');assert.equal(conversationReader(api,'chat'),reader);
 const stop=reader.subscribe(()=>{});await new Promise(resolve=>setTimeout(resolve,5));stop();assert.deepEqual(cursors,[0n,1n]);assert.equal(reader.getSnapshot().messages[0].text,'answeranswer');
});
test('conversation details identify provider, model provenance and subscription',()=>{
 const session=create(SessionSchema,{provider:'codex',profileId:'saved',model:'test-model',modelReported:true});const profile=create(ProfileSchema,{id:'saved',label:'Work account'});
 assert.equal(conversationDetails(session,[profile]),'Codex · test-model · Work account');session.modelReported=false;assert.match(conversationDetails(session,[profile]),/test-model \(requested\)/);session.model='';assert.equal(conversationDetails(session,[]),'Codex · Saved account','an unknown model is left out, never a placeholder');
});

import {compareRepositories,repositoryOrganization} from '../packages/surfaces/src/repository-model.ts';
test('repository grouping sorts organization then repository and puts locals last',()=>{
 const rows=[{identity:'local:z',name:'local'},{identity:'github:zoo/api',name:'zoo/api'},{identity:'github:alpha/web',name:'alpha/web'},{identity:'github:alpha/api',name:'alpha/api'}].sort(compareRepositories);
 assert.deepEqual(rows.map(row=>row.name),['alpha/api','alpha/web','zoo/api','local']);assert.equal(repositoryOrganization(rows[0].identity),'alpha');
});


test('prompt metadata retains the last root observation at acceptance across journal batches', () => {
 let state = emptyTranscript();
 const add = (type: string, text = '', data = {}, agentId = 'root') => {
  state = appendTranscript(state, create(JournalEventSchema, {
   cursor: state.cursor + 1n, event: { type, text, agentId, providerDataJson: new TextEncoder().encode(JSON.stringify(data)) },
  }));
 };
 add('operation_accepted', 'Before telemetry');
 add('usage', '', {model: 'first', tokenUsage: {last: {totalTokens: 20}, modelContextWindow: 100}});
 add('context', '', {model: 'child', used: 90, capacity: 200}, 'reviewer');
 add('model_changed', 'requested-for-next-turn');
 add('operation_accepted', 'First observation');
 add('context', '', {model: 'second', used: 0, capacity: 300});
 add('operation_accepted', 'Second observation');
 assert.deepEqual(state.messages.filter(m => m.type === 'operation_accepted').map(m => [m.promptModel, m.promptContext]), [
  [undefined, undefined],
  ['first', {used: 20, capacity: 100}],
  ['second', {used: 0, capacity: 300}],
 ]);
});

test('main agent model survives diagnostic eviction and ignores child models',()=>{
 let state=emptyTranscript();
 const add=(type:string,model:string,agentId='root')=>state=appendTranscript(state,create(JournalEventSchema,{cursor:state.cursor+1n,event:{type,agentId,providerDataJson:new TextEncoder().encode(JSON.stringify({model}))}}));
 add('session_ready','actual-main-model');for(let i=0;i<100;i++)add('activity','');
 add('context','child-model','reviewer');assert.equal(state.reportedModel,'actual-main-model');
 add('usage','updated-main-model');assert.equal(state.reportedModel,'updated-main-model');
});


test('model and account labels distinguish reported models from internal defaults',()=>{
 const session=create(SessionSchema,{profileId:'claude-default',model:'alias'});
 assert.equal(sessionModelLabel(session),'alias (requested)');
 assert.equal(sessionModelLabel(session,'provider-exact-model'),'provider-exact-model');
 assert.equal(sessionModelLabel(), '', 'an unknown model is left out, never a placeholder');
 assert.equal(sessionAccountLabel(session),'Account not reported');
});

test('successor keeps transcript identity and cursor, unrelated executions remain separate',async()=>{
 const calls:{id:string;after:bigint}[]=[];
 const api={async *watchEvents({sessionId,after}:{sessionId:string;after:bigint}){calls.push({id:sessionId,after});if(!after)yield create(JournalEventSchema,{cursor:1n,event:{type:'message_delta',text:'retained'}});}} as unknown as RunnerClient;
 const old=conversationReader(api,'old');const stop=old.subscribe(()=>{});await new Promise(resolve=>setTimeout(resolve,5));stop();old.view.scrollTop=120;
 const next=continueConversation(api,'old',create(SessionSchema,{id:'next',previousExecutionId:'old'}));assert.equal(next.identity,old.identity);assert.equal(next.snapshot.messages,old.snapshot.messages);assert.equal(next.view.scrollTop,120);const end=next.subscribe(()=>{});await new Promise(resolve=>setTimeout(resolve,5));end();assert.deepEqual(calls,[{id:'old',after:0n},{id:'next',after:1n}]);
 const unrelated=continueConversation(api,'old',create(SessionSchema,{id:'unrelated'}));assert.equal(unrelated.snapshot.cursor,0n);assert.equal(unrelated.identity,'unrelated');
});

test('runner restarts surface what they leave uncertain, and only that',()=>{
 let state=emptyTranscript();
 const add=(fields:Record<string,unknown>)=>{state=appendTranscript(state,create(JournalEventSchema,{cursor:state.cursor+1n,...fields}));};
 add({event:{type:'operation_accepted',text:'deploy it'}});
 add({event:{type:'message_delta',text:'Deploying',turnId:'t'}});
 add({event:{type:'runner_reattached',status:'native_process_adopted'}});
 add({event:{type:'message_delta',text:' now',turnId:'t'}});
 add({event:{type:'request_resolved',status:'outcome_unknown_after_restart',requestId:'n:41'}});
 add({event:{type:'request_resolved',status:'response_written',requestId:'n:42'}});
 add({event:{type:'receipt'},receipt:{operationId:'second',delivery:'unknown_after_runner_restart'}});
 add({event:{type:'receipt'},receipt:{operationId:'third',delivery:'accepted'}});
 assert.deepEqual(state.messages.map(m=>m.type),['operation_accepted','message_delta','reattached','message_delta','notice','notice']);
 assert.equal(state.messages[2].count,1,'a restart that interrupted nothing is a quiet line, not a card');
 assert.match(state.messages[4].text,/outcome is unknown/);
 assert.match(state.messages[5].text,/not sent again/);
 assert.equal(state.messages[1].text,'Deploying','a restart notice must not merge into the reply around it');
});

test('another lazybox ahead on agents\' PATH: a Runner line in the conversation, a status in Settings',()=>{
 let state=emptyTranscript();
 const add=(fields:Record<string,unknown>)=>{state=appendTranscript(state,create(JournalEventSchema,{cursor:state.cursor+1n,...fields}));};
 const text='Your shell startup files put another lazybox first on PATH (/Users/me/.cargo/bin/lazybox, in zsh -c). Lazybox puts its own back first in every agent and workspace terminal shell, so agents run this Lazybox\'s lazybox; nothing to do.';
 add({event:{type:'cli_notice',status:'fixed',text}});
 add({event:{type:'message_delta',text:'Hello',turnId:'t'}});
 assert.deepEqual(state.messages.map(m=>[m.type,m.text]),[['notice',text],['message_delta','Hello']]);
 assert.deepEqual(agentCliStatus({problem:'',unfixed:[],shadow:''}),{label:'OK',tone:'ok'});
 assert.deepEqual(agentCliStatus({problem:'',unfixed:[],shadow:'/Users/me/.cargo/bin/lazybox'}),{label:'Fixed',tone:'ok'});
 assert.deepEqual(agentCliStatus({problem:'',unfixed:['sh -lc finds /usr/local/bin/lazybox'],shadow:'/usr/local/bin/lazybox'}),{label:'Needs full path',tone:'warn'});
 assert.equal(agentCliStatus({problem:'timed out',unfixed:[],shadow:''}).tone,'warn');
});

test('consecutive harmless restarts collapse into one line',()=>{
 let state=emptyTranscript();
 const add=(fields:Record<string,unknown>)=>{state=appendTranscript(state,create(JournalEventSchema,{cursor:state.cursor+1n,...fields}));};
 add({event:{type:'message_delta',text:'Done',turnId:'t'}});
 for(let i=0;i<5;i++)add({event:{type:'runner_reattached',status:'native_process_adopted'}});
 assert.deepEqual(state.messages.map(m=>[m.type,m.count]),[['message_delta',undefined],['reattached',5]]);
 add({event:{type:'operation_accepted',text:'next'}});
 add({event:{type:'runner_reattached',status:'native_process_adopted'}});
 assert.deepEqual(state.messages.map(m=>m.type),['message_delta','reattached','operation_accepted','reattached'],'restarts separated by other messages stay separate');
});

import {foldTranscript} from '../packages/surfaces/src/transcript-model.ts';
import {longConversation} from './fixtures/long-conversation.mjs';
const wireRecords=(events:any[],from=1)=>events.map((e,i)=>create(JournalEventSchema,{cursor:BigInt(from+i),capturedAt:'2026-11-16T10:00:00Z',event:{...e,providerDataJson:e.providerDataJson?Buffer.from(e.providerDataJson,'base64'):new Uint8Array()}}));
test('folding a batch equals folding its records one by one',()=>{
 const records=wireRecords(longConversation({turns:30}));
 let one=emptyTranscript();for(const record of records)one=appendTranscript(one,record);
 // Any split into batches, including one record and the whole replay, lands on the same state.
 for(const size of [1,7,500,records.length]){
  let batched=emptyTranscript();for(let i=0;i<records.length;i+=size)batched=foldTranscript(batched,records.slice(i,i+size));
  assert.deepEqual(batched.messages,one.messages,`batches of ${size}`);
  assert.deepEqual(batched.recent.map(r=>r.cursor),one.recent.map(r=>r.cursor));
  assert.deepEqual(batched.activity.map(r=>r.cursor),one.activity.map(r=>r.cursor));
  assert.equal(batched.activityTruncated,one.activityTruncated);assert.equal(batched.cursor,one.cursor);assert.equal(batched.reportedModel,one.reportedModel);assert.equal(batched.appendToLast,one.appendToLast);
 }
 // Each turn: the prompt, its tool calls as one run of lines, the answer.
 assert.equal(one.messages.length,90);assert.equal(one.messages.filter(m=>m.type==='tools').length,30);assert.match(one.messages.at(-1)!.text,/ANSWER_END_29$/);assert.equal(one.reportedModel,'claude-fixture-model');
});
test('a fold never changes the state it extends, and skips records it already has',()=>{
 const records=wireRecords([{type:'operation_accepted',text:'q'},{type:'message_delta',text:'a',turnId:'t'},{type:'message_delta',text:'b',turnId:'t'}]);
 const first=foldTranscript(emptyTranscript(),records.slice(0,2));const snapshot=structuredClone(first.messages);
 const second=foldTranscript(first,records.slice(1));
 assert.deepEqual(first.messages,snapshot,'the earlier snapshot a view rendered is unchanged');
 assert.notEqual(second.messages.at(-1),first.messages.at(-1));assert.equal(second.messages.at(-1)!.text,'ab');
 assert.equal(foldTranscript(second,records),second,'a replayed batch is a no-op');
 assert.equal(foldTranscript(second,[]),second);
});
test('activity keeps the latest 1,024 events and says it dropped the rest',()=>{
 const records=wireRecords(Array.from({length:1500},(_,i)=>({type:'tool_activity',providerDataJson:Buffer.from(JSON.stringify({type:'tool_call',id:`t${i}`,name:'Bash',status:'completed'})).toString('base64')})));
 const batched=foldTranscript(emptyTranscript(),records);
 assert.equal(batched.activity.length,1024);assert.equal(batched.activity[0].cursor,477n);assert.equal(batched.activityTruncated,true);
 const small=foldTranscript(foldTranscript(emptyTranscript(),records.slice(0,1000)),records.slice(1000,1010));
 assert.equal(small.activity.length,1010);assert.equal(small.activityTruncated,false);
});
test('a reader publishes a replay burst once, not once per record',async()=>{
 const records=wireRecords(longConversation({turns:20,toolOutputBytes:200}));
 const api={async *watchEvents(){yield* records;}} as unknown as RunnerClient;
 const reader=new ConversationReader(api,'burst');let published=0;
 const stop=reader.subscribe(()=>{published++;});
 await new Promise(resolve=>setTimeout(resolve,100));stop();
 const state=reader.getSnapshot();
 assert.equal(state.cursor,BigInt(records.length));assert.equal(state.messages.length,60);assert.equal(state.status,'Complete');assert.equal(state.loading,false);
 assert.ok(published<=4,`${records.length} records published ${published} times`);
});
test('a replay is held until it reaches the retained history, reporting progress meanwhile',async()=>{
 const records=wireRecords(longConversation({turns:6,toolOutputBytes:200}));const total=BigInt(records.length);
 for(const record of records)record.retained=total;
 const live=create(JournalEventSchema,{cursor:total+1n,retained:total+1n,event:{type:'operation_accepted',text:'LIVE'}});
 let release!:()=>void;const gate=new Promise<void>(resolve=>{release=resolve;});
 const api={async *watchEvents(){for(const [i,record] of records.slice(0,-1).entries()){yield record;if(i%50===0)await new Promise(resolve=>setTimeout(resolve,20));}await gate;yield records.at(-1)!;yield live;await new Promise(()=>{});}} as unknown as RunnerClient;
 const reader=new ConversationReader(api,'held');const seen:{messages:number;replay?:{loaded:number;total:number}}[]=[];
 const stop=reader.subscribe(()=>{const s=reader.getSnapshot();seen.push({messages:s.messages.length,replay:s.replay});});
 await new Promise(resolve=>setTimeout(resolve,400));
 assert.equal(reader.getSnapshot().messages.length,0,'nothing from the middle of the history is shown');
 assert.ok(reader.getSnapshot().loading);const progress=reader.getSnapshot().replay!;assert.equal(progress.total,records.length);assert.ok(progress.loaded>0&&progress.loaded<records.length);
 release();await new Promise(resolve=>setTimeout(resolve,100));
 const state=reader.getSnapshot();stop();
 assert.equal(state.replay,undefined);assert.equal(state.loading,false);assert.equal(state.messages.at(-1)!.text,'LIVE');
 assert.equal(seen.filter(s=>s.messages>0)[0].messages,19,'the first published conversation is the whole history');
});

import {Code,ConnectError} from '@connectrpc/connect';
import {deliveredQueuedTurn} from './fixtures/long-conversation.mjs';
/** Journal events as WatchEvents sends them: a receipt in its own field, retained up to `retained`. */
const wireJournal=(events:any[],from:number,retained:number)=>events.map(({receipt,...e},i)=>create(JournalEventSchema,{cursor:BigInt(from+i),retained:BigInt(Math.max(retained,from+i)),capturedAt:'2026-11-17T09:40:00Z',...(receipt?{receipt}:{}),event:{...e,providerDataJson:e.providerDataJson?Buffer.from(e.providerDataJson,'base64'):new Uint8Array()}}));
// A real conversation's view stopped at a Runner restart: the restarting
// Runner ended the stream like the end of a finished conversation, and the
// reader stopped following. Every later turn, including the one a delivered
// queued message started, was missing while the agent kept working. A stream
// a restart cuts ends Unavailable: the reader reconnects from its cursor and
// shows everything up to the latest, the turn in progress included.
test('a reader cut by a Runner restart reconnects from its cursor and shows the turn in progress',async()=>{
 const history=longConversation({turns:80,toolOutputBytes:200});
 const {events:latest}=deliveredQueuedTurn();
 assert.ok(history.length>3000,`a long history (${history.length} events)`);
 const replay=wireJournal(history,1,history.length),after=wireJournal(latest,history.length+1,history.length+latest.length);
 const asked:bigint[]=[];
 const api={async *watchEvents({after:from}:{after:bigint},{signal}:{signal:AbortSignal}){
  asked.push(from);
  if(asked.length===1){yield* replay;await new Promise(resolve=>setTimeout(resolve,50));throw new ConnectError('the Runner is restarting',Code.Unavailable);}
  yield* after.filter(record=>record.cursor>from);
  await new Promise(resolve=>signal.addEventListener('abort',resolve,{once:true}));
 }} as unknown as RunnerClient;
 const reader=new ConversationReader(api,'restarted');const stop=reader.subscribe(()=>{});
 await new Promise(resolve=>setTimeout(resolve,1500));
 const state=reader.getSnapshot();stop();
 assert.deepEqual(asked,[0n,BigInt(history.length)],'the reader reconnects after the last record it had');
 assert.equal(state.cursor,BigInt(history.length+latest.length));assert.equal(state.status,'Live');assert.equal(state.error,'');
 const texts=state.messages.map(m=>m.text);
 for(const text of ['Make the fixture global: two roles, admin and user.','QUEUED_DELIVERED: only two teams and two users in every solution.'])assert.equal(texts.filter(t=>t===text).length,1,`shows "${text}" once`);
 // The message delivered by Send now starts its turn, after the stop; the one behind it still waits, so it is not said yet.
 const stopped=state.messages.findIndex(m=>m.type==='stopped'),delivered=texts.indexOf('QUEUED_DELIVERED: only two teams and two users in every solution.');
 assert.ok(stopped>=0&&delivered===stopped+1,`the stop, then the message: ${JSON.stringify(state.messages.map(m=>m.type))}`);
 assert.ok(!texts.some(t=>t.startsWith('QUEUED_BEHIND')),'a queued message is not in the conversation');
 assert.deepEqual(Object.values(state.pending!).map(m=>m.text),['QUEUED_BEHIND: ok so continue']);
 // The delivered message's turn is the last thing shown, still working.
 const last=state.messages.at(-1)!;assert.equal(last.type,'tools');
 assert.deepEqual(last.tools!.map(t=>t.status),['completed','completed','completed']);
 assert.ok(JSON.stringify(last.tools).includes('lazybox artifact put plan.md'));
});
// A finished conversation's stream ends cleanly; that, and only that, stops the reader.
test('a reader stops following a conversation whose stream ended',async()=>{
 let opened=0;
 const api={async *watchEvents(){opened++;yield* wireJournal([{type:'operation_accepted',text:'done'},{type:'session_closed'}],1,2);}} as unknown as RunnerClient;
 const reader=new ConversationReader(api,'finished');const stop=reader.subscribe(()=>{});
 await new Promise(resolve=>setTimeout(resolve,1200));stop();
 assert.equal(reader.getSnapshot().status,'Complete');assert.equal(opened,1);
});

// Send now stops Claude's turn; Claude reports that stop as an "error during
// execution" ("[ede_diagnostic] …"). The conversation says it in one quiet
// line, never as an error and never in the provider's words; a real failure
// with that text is a plain sentence with the raw text kept as its detail.
test('a turn Send now stopped is one quiet line; a raw provider diagnostic is never shown',()=>{
 const ede='[ede_diagnostic] result_type=user last_content_type=n/a stop_reason=tool_use';
 let state=emptyTranscript();
 const add=(event:Record<string,unknown>)=>{state=appendTranscript(state,create(JournalEventSchema,{cursor:state.cursor+1n,event:{turnId:'turn',...event}}));};
 add({type:'operation_accepted',operationId:'build',text:'Run the build'});
 add({type:'turn_started'});
 add({type:'tool_activity',providerDataJson:new TextEncoder().encode(JSON.stringify({type:'tool_call',id:'toolu_dev',name:'Bash',status:'running',input:{command:'npm run dev'}}))});
 add({type:'operation_accepted',operationId:'urgent',text:'Use port 4000 instead'});
 add({type:'operation_accepted',operationId:'urgent:now',text:''});
 add({type:'failure',status:'provider_error',text:ede,account:{state:'unknown',reason:'error_during_execution'}});
 add({type:'turn_ended',status:'interrupted'});
 assert.deepEqual(state.messages.map(m=>[m.type,m.text]),[['operation_accepted','Run the build'],['tools',''],['stopped','Stopped to send your message']]);
 assert.equal(state.messages[1].tools![0].status,'unknown','the step it stopped is no longer shown running');
 assert.equal(state.sendingNow,false);
 // Delivered, the message is the next turn's start.
 state=appendTranscript(state,create(JournalEventSchema,{cursor:state.cursor+1n,event:{type:'receipt'},receipt:{operationId:'urgent',delivery:'submitted'}}));
 assert.deepEqual(state.messages.slice(2).map(m=>[m.type,m.text]),[['stopped','Stopped to send your message'],['operation_accepted','Use port 4000 instead']]);
 // The next turn's failure, with no stop asked for, is a failure: in plain words.
 add({type:'turn_started'});
 add({type:'failure',status:'provider_error',text:ede});
 const failure=state.messages.at(-1)!;
 assert.equal(failure.type,'failure');assert.equal(failure.text,'Claude stopped unexpectedly.');assert.equal(failure.detail,ede);
 assert.ok(state.messages.every(m=>!m.text.includes('ede_diagnostic')),'no raw diagnostic is a line of the conversation');
 // A limit during a Send now is still a limit, never taken for the stop.
 add({type:'operation_accepted',operationId:'other:now',text:''});
 add({type:'failure',status:'exhausted',account:{state:'exhausted'}});
 assert.equal(state.sendingNow,true);
});

// ── A message sent while the agent works is queued, not said ───────────────
// The Runner journals a message when it accepts it, queued behind the turn in
// progress or not, and its receipt when the agent gets it. A real conversation
// showed one sent mid-turn twice: as a "You" bubble inside the running turn,
// above the agent's next words, and in the Queued strip. The conversation
// shows a message once, where it reached the agent; the strip shows it while
// it waits.
import type {Transcript} from '../packages/surfaces/src/transcript-model.ts';
const q={
 accepted:(operationId:string,text:string)=>({type:'operation_accepted',operationId,text}),
 receipt:(operationId:string,delivery='submitted',status='')=>({type:'receipt',status,receipt:{operationId,delivery}}),
 delta:(text:string)=>({type:'message_delta',text}),
 tool:(id:string,command:string,status='running')=>({type:'tool_activity',providerDataJson:Buffer.from(JSON.stringify({type:'tool_call',id,name:'Bash',status,...(status==='running'?{input:{command}}:{output:'ok'})})).toString('base64')}),
 started:{type:'turn_started'},ended:{type:'turn_ended',status:'completed'},
};
/** Journal records as WatchEvents sends them, numbered from 1. */
const journalOf=(events:any[])=>wireJournal(events,1,events.length);
/** The conversation as lines: who said what, and the runs of tool calls. */
const lines=(t:Transcript)=>t.messages.map(m=>m.type==='operation_accepted'?`You${m.steered?' (steered)':''}: ${m.text}`:m.type==='tools'?`tools ${m.tools!.filter(x=>x.name==='Bash').map(x=>x.id).join(',')}`:m.type==='message_delta'?`Agent: ${m.text}`:`${m.type}${m.text?`: ${m.text}`:''}`);
const foldAll=(events:any[])=>foldTranscript(emptyTranscript(),journalOf(events));
/** The real conversation's shape: a message accepted mid-turn, the turn going on, then delivered. */
const queuedMidTurn:any[]=[
 q.accepted('first','Make the fixture global.'),q.receipt('first'),q.started,
 q.delta('Looking at the fixture.'),q.tool('t1','cat module/fixtures/dev-admin.yaml'),
 q.accepted('focus','for now, REALLY focus on unit tests and API / proto'),
 q.tool('t1','','completed'),q.delta('The fixture seeds three teams.'),q.tool('t2','go test ./...'),q.tool('t2','','completed'),
 q.ended,q.receipt('focus'),q.started,q.delta('Focusing on unit tests.'),q.ended,
];

test('a message accepted mid-turn is not said until delivered, then starts the next turn after the previous one',()=>{
 const records=journalOf(queuedMidTurn),delivery=queuedMidTurn.findIndex(e=>e.receipt?.operationId==='focus');
 // While it waits: nowhere in the conversation, the agent's words unbroken by it.
 const waiting=foldTranscript(emptyTranscript(),records.slice(0,delivery));
 assert.deepEqual(lines(waiting),['You: Make the fixture global.','Agent: Looking at the fixture.','tools t1','Agent: The fixture seeds three teams.','tools t2']);
 assert.deepEqual(Object.keys(waiting.pending!),['focus']);
 // Delivered: once, after the turn it waited for, before the turn it starts.
 const done=foldTranscript(waiting,records.slice(delivery));
 assert.deepEqual(lines(done),['You: Make the fixture global.','Agent: Looking at the fixture.','tools t1','Agent: The fixture seeds three teams.','tools t2','You: for now, REALLY focus on unit tests and API / proto','Agent: Focusing on unit tests.']);
 assert.deepEqual(done.pending,{});
 assert.equal(done.messages.find(m=>m.operationId==='focus')!.id,'6','the message keeps its identity: the record that accepted it');
});

// The Queued strip read only the session poll (every 1.5 s) while the
// conversation read the stream: a message vanished (hidden from the
// conversation, not yet polled), then showed twice (in the conversation and
// in the last poll). The strip merges both queues: in one place at a time.
import {promptBefore,queuedMessages} from '../packages/surfaces/src/transcript-model.ts';
test('Jump to prompt lands on what you sent, never on a message Lazybox wrote to the agent',()=>{
 const origin={kind:'review_findings',summary:'Codex review · 3 findings',detail:'',copy:'',from:'Codex'};
 const messages=[{id:'1',type:'operation_accepted',text:'Fix the login test',turnId:''},{id:'2',type:'message_delta',text:'Done.',turnId:''},{id:'3',type:'operation_accepted',text:'Findings…',turnId:'',origin},{id:'4',type:'message_delta',text:'Fixed the findings.',turnId:''}];
 assert.equal(promptBefore(messages,'4')?.id,'1');
 assert.equal(promptBefore(messages,'2')?.id,'1');
 assert.equal(promptBefore(messages.slice(2),'4'),undefined);
});
test('the Queued strip: at once from the transcript, never also in the conversation, in the Runner\'s order',()=>{
 const records=journalOf(queuedMidTurn),delivery=queuedMidTurn.findIndex(e=>e.receipt?.operationId==='focus');
 const waiting=foldTranscript(emptyTranscript(),records.slice(0,delivery));
 const ids=(polled:any[]|undefined,t:Transcript)=>queuedMessages(polled,t).map(m=>`${m.operationId}: ${m.text}`);
 // Accepted, not polled yet: shown now.
 assert.deepEqual(ids([],waiting),['focus: for now, REALLY focus on unit tests and API / proto']);
 assert.deepEqual(ids(undefined,waiting),['focus: for now, REALLY focus on unit tests and API / proto']);
 // Polled too: once, the Runner's entry.
 assert.deepEqual(ids([{operationId:'focus',text:'for now, REALLY focus…'}],waiting),['focus: for now, REALLY focus…']);
 // Delivered, still in the last poll: in the conversation only.
 const done=foldTranscript(waiting,records.slice(delivery));
 assert.deepEqual(ids([{operationId:'focus',text:'for now, REALLY focus…'}],done),[]);
 // Queued where the stream has not said it yet (it lags): the poll shows it. The Runner's order first.
 assert.deepEqual(ids([{operationId:'other',text:'Also the docs.'},{operationId:'focus',text:'focus'}],waiting),['other: Also the docs.','focus: focus']);
 // A message Lazybox wrote keeps saying what it is.
 const origin={kind:'review_findings',summary:'Codex review · 3 findings',detail:'',copy:'',from:'Codex'};
 const fromLazybox=foldAll([q.accepted('first','Go.'),q.receipt('first'),q.started,{type:'operation_accepted',operationId:'findings',text:'Findings…',providerDataJson:Buffer.from(JSON.stringify({origin})).toString('base64')}]);
 assert.equal((queuedMessages([],fromLazybox)[0].origin as any)?.kind,'review_findings');
});

test('a resumed execution keeps a held prompt in the queue before the first turn',()=>{
 const transcript=foldAll([{...q.accepted('held','Original'),status:'queued_for_edit'}]);
 assert.equal(transcript.pending?.held?.text,'Original');
 assert.equal(transcript.messages.some(message=>message.operationId==='held'),false);
});

test('a queued message never delivered is never said; one the closing session dropped neither',()=>{
 const t=foldAll([q.accepted('first','Start the server.'),q.receipt('first'),q.started,q.tool('t1','npm run dev'),q.accepted('later','Then check the logs.'),q.delta('Started on port 3000.')]);
 assert.deepEqual(lines(t),['You: Start the server.','tools t1','Agent: Started on port 3000.']);
 assert.deepEqual(Object.values(t.pending!).map(m=>m.text),['Then check the logs.']);
 // engine.Close writes a dropped message's receipt as an event of its own.
 const closed=foldTranscript(t,wireJournal([{type:'receipt',operationId:'later',status:'canceled_before_delivery'},{type:'session_closed'}],7,8));
 assert.deepEqual(lines(closed),lines(t));assert.deepEqual(closed.pending,{});
});

test('Send now: the stop, then the message, then its turn',()=>{
 const ede='[ede_diagnostic] result_type=user last_content_type=n/a stop_reason=tool_use';
 const t=foldAll([
  q.accepted('build','Run the build'),q.receipt('build'),q.started,q.tool('t1','npm run dev'),
  q.accepted('urgent','Use port 4000 instead'),q.tool('t2','curl localhost:3000'),
  q.accepted('urgent:now',''),q.receipt('urgent:now','acknowledged'),
  {type:'failure',status:'provider_error',text:ede,account:{state:'unknown',reason:'error_during_execution'}},{type:'turn_ended',status:'interrupted'},
  q.receipt('urgent'),q.started,q.delta('Switching to port 4000.'),
 ]);
 assert.deepEqual(lines(t),['You: Run the build','tools t1,t2','stopped: Stopped to send your message','You: Use port 4000 instead','Agent: Switching to port 4000.']);
 assert.ok(t.messages[1].tools!.every(x=>x.status!=='running'),'the stopped steps are no longer running');
});

test('several queued messages are said in the order delivered, each starting its turn',()=>{
 const t=foldAll([
  q.accepted('a','A'),q.receipt('a'),q.started,q.delta('working on A'),
  q.accepted('b','B'),q.accepted('c','C'),q.delta(' still A'),q.ended,
  // Sent while the queue still holds B and C: it waits behind them, though no turn runs.
  q.accepted('d','D'),
  q.receipt('b'),q.started,q.delta('answer B'),q.ended,
  q.receipt('c'),q.started,q.delta('answer C'),q.ended,
  q.receipt('d'),q.started,q.delta('answer D'),q.ended,
 ]);
 assert.deepEqual(lines(t),['You: A','Agent: working on A still A','You: B','Agent: answer B','You: C','Agent: answer C','You: D','Agent: answer D']);
});

test('a message sent while the agent is idle is said at once, as before',()=>{
 const t=foldAll([q.accepted('a','Hello'),q.receipt('a'),q.started,q.delta('Hi.'),q.ended,q.accepted('b','Again')]);
 assert.deepEqual(lines(t),['You: Hello','Agent: Hi.','You: Again'],'shown before its receipt arrives');
 assert.deepEqual(t.pending??{},{});
 assert.deepEqual(lines(foldTranscript(t,wireJournal([q.receipt('b'),q.started,q.delta('Hi again.')],7,9))),['You: Hello','Agent: Hi.','You: Again','Agent: Hi again.']);
});

test('a turn recorded a moment before its message\'s receipt still opens with the message (Codex)',()=>{
 const t=foldAll([q.accepted('a','A'),q.receipt('a'),q.started,q.delta('working'),q.accepted('b','B'),q.ended,q.started,q.delta('answer'),q.receipt('b'),q.delta(' to B')]);
 assert.deepEqual(lines(t),['You: A','Agent: working','You: B','Agent: answer to B'],'the reply is still one message after the moved one');
});

test('a steered message is said where it reached the running turn, and says it was steered',()=>{
 const t=foldAll([q.accepted('a','Start the server'),q.receipt('a','acknowledged'),q.started,q.tool('t1','npm run dev'),q.accepted('b','Also log the port'),q.delta('Starting.'),q.receipt('b','acknowledged','steered'),q.tool('t1','','completed'),q.delta('Logged port 3000.'),q.ended]);
 assert.deepEqual(lines(t),['You: Start the server','tools t1','Agent: Starting.','You (steered): Also log the port','Agent: Logged port 3000.']);
 assert.equal(t.messages[1].tools![0].status,'completed','a call from before the steer still gets its result');
});

test('a queued message the Runner restart left unconfirmed is said before the line that says so',()=>{
 const t=foldAll([q.accepted('a','A'),q.receipt('a'),q.started,q.accepted('b','B'),q.ended,{type:'runner_reattached',status:'native_process_adopted'},q.receipt('b','unknown_after_runner_restart')]);
 assert.deepEqual(lines(t).slice(-2),['You: B','notice: The Runner restarted before it could confirm this message reached the agent. It was not sent again; resend it if it is still needed.']);
});

// Reload, a reconnect after a Runner restart, a cache: any split of the same
// journal folds to the same conversation, so placement depends on the events alone.
test('queued messages land in the same place however the journal is split',()=>{
 const events=[...queuedMidTurn,
  q.accepted('x','X'),q.receipt('x'),q.started,q.accepted('y','Y'),q.delta('on X'),q.accepted('y:now',''),q.receipt('y:now','acknowledged'),{type:'turn_ended',status:'interrupted'},q.receipt('y'),q.started,q.accepted('z','Z'),q.delta('on Y'),q.receipt('z','acknowledged','steered'),q.delta(' and Z'),q.ended,
  q.accepted('w','W'),q.started,q.receipt('w'),q.delta('on W'),q.accepted('v','still waiting')];
 const records=journalOf(events),whole=foldTranscript(emptyTranscript(),records);
 let one=emptyTranscript();for(const record of records)one=appendTranscript(one,record);
 assert.deepEqual(one.messages,whole.messages);
 for(const size of [2,3,5,8,13]){
  let chunked=emptyTranscript();for(let i=0;i<records.length;i+=size)chunked=foldTranscript(chunked,records.slice(i,i+size));
  assert.deepEqual(chunked.messages,whole.messages,`chunks of ${size}`);assert.deepEqual(chunked.pending,whole.pending);
 }
 // A live view that saw a message queued, then delivered, matches a reload of the whole journal.
 const live=foldTranscript(foldTranscript(emptyTranscript(),records.slice(0,5)),records.slice(5));
 assert.deepEqual(live.messages,whole.messages);
 assert.deepEqual(lines(whole).slice(7),['You: X','Agent: on X','stopped: Stopped to send your message','You: Y','Agent: on Y','You (steered): Z','Agent:  and Z','You: W','Agent: on W']);
 assert.deepEqual(Object.values(whole.pending!).map(m=>m.text),['still waiting']);
});

test('the composer can distinguish a reported native permission from the selected mode', () => {
  const state = appendTranscript(emptyTranscript(), create(JournalEventSchema, {
    cursor: 1n, event: { type: 'agent_permission', text: 'bypassPermissions' },
  }));
  assert.equal(state.reportedPermission, 'YOLO');
  assert.equal(state.messages.length, 0);
});
