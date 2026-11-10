import test from 'node:test';
import assert from 'node:assert/strict';
import { create } from '@bufbuild/protobuf';
import { JournalEventSchema } from '@lazybox/protocol';
import { appendTranscript, emptyTranscript } from '../packages/surfaces/src/transcript-model.ts';
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
 assert.equal(conversationDetails(session,[profile]),'Codex · test-model · Work account');session.modelReported=false;assert.match(conversationDetails(session,[profile]),/test-model \(requested\)/);session.model='';assert.match(conversationDetails(session,[]),/Model not reported yet · Saved account/);
});

import {compareRepositories,repositoryOrganization} from '../packages/surfaces/src/repository-model.ts';
test('repository grouping sorts organization then repository and puts locals last',()=>{
 const rows=[{identity:'local:z',name:'local'},{identity:'github:zoo/api',name:'zoo/api'},{identity:'github:alpha/web',name:'alpha/web'},{identity:'github:alpha/api',name:'alpha/api'}].sort(compareRepositories);
 assert.deepEqual(rows.map(row=>row.name),['alpha/api','alpha/web','zoo/api','local']);assert.equal(repositoryOrganization(rows[0].identity),'alpha');
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
 assert.equal(sessionModelLabel(), 'Model not reported yet');
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
 assert.deepEqual(state.messages.map(m=>m.type),['operation_accepted','message_delta','notice','message_delta','notice','notice']);
 assert.match(state.messages[2].text,/re-attached/);
 assert.match(state.messages[4].text,/outcome is unknown/);
 assert.match(state.messages[5].text,/not sent again/);
 assert.equal(state.messages[1].text,'Deploying','a restart notice must not merge into the reply around it');
});
