import {test} from 'node:test';import assert from 'node:assert/strict';
import {create} from '@bufbuild/protobuf';import {JournalEventSchema} from '@lazybox/protocol';
import {appendTranscript,emptyTranscript} from '../packages/surfaces/src/transcript-model';
import {boxAfterCommand,commandLine,decideSend,DEFAULT_MODEL,emptyCatalog,findCommand,lazyboxCommands,MODEL_CHOICES_SHOWN,mergeCatalog,mergeModels,modelChoices,permissionBlocked,shownModelChoices,modelRestartText,paletteCommand,parseSlash,providerCommands,rankChoices,rankCommands,resolveModel,sameModel,shortTokens,slashCommandList,slashMenu,snippetCommands,type SlashContext} from '../packages/surfaces/src/slash-commands';

const ctx=(over:Partial<SlashContext>={}):SlashContext=>({provider:'claude',live:true,busy:false,coding:true,workspace:true,permission:'ask',planWorkflow:false,livePermission:true,model:'claude-opus-5-5',liveModel:true,resumable:true,models:[{value:'claude-opus-5-5',displayName:'Opus 5.5',description:'Most capable'},{value:'claude-sonnet-5',displayName:'Sonnet 5',description:'Fast'}],agentMessage:true,shellBlock:true,compact:true,detail:'smart',subscriptions:1,...over});
const catalog=mergeCatalog(emptyCatalog(),{commands:[{name:'compact',description:'Clear history but keep a summary',argumentHint:'<instructions>',builtin:true},{name:'review-pr',description:'Review a pull request',argumentHint:'<number>'},{name:'/init',description:'Initialize CLAUDE.md',argumentHint:''},{name:'model',description:'native model picker',argumentHint:''},{name:'exit',description:'Exit',argumentHint:''},{name:'bad name',description:'',argumentHint:''}],models:[{value:'opus',displayName:'Opus'}],terminal:['exit']});
const all=(c=ctx())=>{const lazybox=lazyboxCommands(c);return [...lazybox,...providerCommands(catalog,lazybox),...snippetCommands([{key:'review',description:'Review this change',category:'Review',body:'Review the current change carefully.'},{key:'test',description:'Run tests',category:'Testing',body:'Run the relevant tests.'}],['test'])];};

test('parsing: a command at the very start, never a path',()=>{
 assert.deepEqual(parseSlash('/model'),{name:'model',args:'',completed:false});
 assert.deepEqual(parseSlash('/model opus'),{name:'model',args:'opus',completed:true});
 assert.deepEqual(parseSlash('/rename Login flake\nsecond'),{name:'rename',args:'Login flake\nsecond',completed:true});
 assert.deepEqual(parseSlash('/'),{name:'',args:'',completed:false});
 assert.equal(parseSlash('/usr/local/bin is missing'),null,'a path is not a command');
 assert.equal(parseSlash(' /model'),null,'only at the start');
 assert.equal(parseSlash('please /model'),null);});

test('the catalog: names without their slash, bounded, terminal commands kept apart; a partial payload keeps the rest',()=>{
 assert.deepEqual(catalog.commands.map(c=>c.name),['compact','review-pr','init','model','exit']);
 assert.deepEqual(catalog.models,[{value:'opus',displayName:'Opus',description:''}]);
 const changed=mergeCatalog(catalog,{commands:[{name:'deploy',description:'Ship it',argumentHint:'<env>'}]});
 assert.deepEqual(changed.commands.map(c=>c.name),['deploy']);assert.equal(changed.models.length,1,'a commands_changed keeps the models');assert.deepEqual(changed.terminal,['exit']);});

test('provider commands: Lazybox stands in for a name it has; terminal-only ones are left out',()=>{
 const names=providerCommands(catalog,lazyboxCommands(ctx())).map(c=>c.name);
 assert.deepEqual(names,['review-pr','init']);});

test('an agent\'s own command takes the place of a Lazybox command that does nothing for it (an ACP agent\'s /compact)',()=>{
 const acp=ctx({provider:'acp:gemini',agentName:'Gemini CLI',compact:false});
 const lazybox=lazyboxCommands(acp),provider=providerCommands(catalog,lazybox);
 assert.deepEqual(provider.map(c=>c.name),['compact','review-pr','init'],'its /compact is listed; Lazybox\'s /model still stands in for its own');
 const list=slashCommandList(lazybox,provider,[]);
 assert.equal(list.filter(c=>c.name==='compact').length,1,'one /compact');
 assert.equal(findCommand(list,'compact')?.group,'provider');
 assert.deepEqual(decideSend('/compact keep the API notes',list),{kind:'provider',command:findCommand(list,'compact'),args:'keep the API notes'},'Enter sends it as the prompt');
 // An agent without one keeps Lazybox's, which says why it cannot run.
 const bare=slashCommandList(lazybox,providerCommands(emptyCatalog(),lazybox),[]);
 assert.match(findCommand(bare,'compact')!.disabled!,/Gemini CLI has no \/compact/);
 // Claude: Lazybox's /compact stands in for Claude's, as before; a command disabled only for now still does.
 for(const c of [ctx(),ctx({busy:true})]){const own=lazyboxCommands(c);assert.equal(findCommand(slashCommandList(own,providerCommands(catalog,own),[]),'compact')?.group,'lazybox');}
});

test('⌘K: Open the review tray opens the tray; a command with choices borrows the box; the palette leaves a draft alone',()=>{
 const lazybox=lazyboxCommands(ctx({reviewers:[{key:'codex',label:'Codex'}]}));
 const named=(name:string)=>lazybox.find(c=>c.name===name)!;
 assert.deepEqual(paletteCommand(named('review')),{args:'tray'},'it starts no review');
 assert.deepEqual(paletteCommand(named('model')),{borrow:'/model '});
 assert.deepEqual(paletteCommand(named('rename')),{borrow:'/rename '});
 assert.deepEqual(paletteCommand(named('compact')),{args:''});
 // A command typed in the box gives the box back: empty, or the draft set aside for its choices.
 assert.equal(boxAfterCommand('box',null),'');
 assert.equal(boxAfterCommand('box','half a draft'),'half a draft');
 // From the palette the box holds the draft: it stays (and its saved copy with it).
 assert.equal(boxAfterCommand('palette',null),undefined);
 assert.equal(boxAfterCommand('palette','half a draft'),'half a draft','a box the palette borrowed is given back');
});

test('filtering and ranking: exact, then prefix, then letters in order; grouped Lazybox · agent · snippets',()=>{
 const {items,best}=rankCommands('',all());
 assert.equal(items[0].command.group,'lazybox');assert.equal(items.at(-1)!.command.group,'snippet');
 assert.equal(items[best].command.name,'model');
 assert.deepEqual(items.filter(i=>i.command.group==='snippet').map(i=>[i.command.name,i.command.badge]),[['test','Next'],['review',undefined]],'the chained snippet comes first');
 const rev=rankCommands('rev',all());
 assert.deepEqual(rev.items.map(i=>i.command.key),['lazybox:review','provider:review-pr','snippet:review']);
 assert.equal(rev.items[rev.best].command.key,'lazybox:review');
 const pr=rankCommands('review-pr',all());assert.equal(pr.items[pr.best].command.key,'provider:review-pr','an exact provider name beats a Lazybox prefix');
 const alias=rankCommands('yolo',all());assert.equal(alias.items[alias.best].command.name,'permissions','an alias finds its command');
 const words=rankCommands('tokens',all());assert.ok(words.items.some(i=>i.command.name==='cost'),'a description word finds /cost');
 assert.equal(rankCommands('xqz',all()).items.length,0);});

test('the menu: names while typing, choices after a space, a hint for free arguments, nothing for paths or other lines',()=>{
 const menu=slashMenu('/mo',all());assert.equal(menu?.kind,'commands');
 const models=slashMenu('/model ',all());assert.equal(models?.kind,'choices');
 assert.deepEqual(models!.kind==='choices'&&models.items.map(c=>[c.label,!!c.current]),[['Opus 5.5',true],['Sonnet 5',false]]);
 assert.equal(models!.kind==='choices'&&models.items[models.best].label,'Sonnet 5','the current model is marked, the other one offered');
 const sonnet=slashMenu('/model son',all());assert.deepEqual(sonnet!.kind==='choices'&&sonnet.items.map(c=>c.value),['claude-sonnet-5']);
 const hint=slashMenu('/rename ',all());assert.equal(hint?.kind,'hint');assert.equal(hint!.kind==='hint'&&hint.command.argumentHint,'<title>');
 const pr=slashMenu('/review-pr 12',all());assert.equal(pr?.kind,'hint');
 assert.equal(slashMenu('/usr/local/bin',all()),null);
 assert.equal(slashMenu('/zzzz',all()),null,'nothing matches: no empty menu in the way');
 assert.equal(slashMenu('/model\nmore',all()),null);
 assert.equal(slashMenu('/mo',all(),true),null,'closed with Esc');
 assert.equal(slashMenu('hello',all()),null);});

test('choices: Full access is refused in plan mode; detail marks the current one',()=>{
 const perms=lazyboxCommands(ctx({planWorkflow:true})).find(c=>c.name==='permissions')!;
 assert.equal(perms.choices!.find(c=>c.value==='yolo')!.disabled,'Full access is unavailable in plan mode');
 const {items,best}=rankChoices('',perms.choices!);assert.equal(items[best].value,'read-only','the current one is not the default pick');
 assert.equal(rankChoices('yo',perms.choices!).items[0].value,'yolo');
 assert.equal(rankChoices('bypass',perms.choices!).items[0].value,'yolo','keywords');
 const detail=lazyboxCommands(ctx()).find(c=>c.name==='detail')!;assert.equal(detail.choices!.find(c=>c.current)!.value,'smart');});

test('argument hints, and what cannot run now says why',()=>{
 const byName=Object.fromEntries(lazyboxCommands(ctx({busy:true,shellBlock:false,workspace:false,compact:false,provider:'codex',liveModel:false,models:[]})).map(c=>[c.name,c]));
 assert.equal(byName.model.argumentHint,'<name> [effort]');assert.equal(byName.detail.argumentHint,'quiet|smart|verbose');assert.equal(byName.rename.argumentHint,'<title>');
 assert.match(byName.model.description,/restarts the agent/,'a model change that is not live says so');
 assert.deepEqual(byName.model.choices!.map(c=>[c.value,c.current]),[['claude-opus-5-5',true]],'the current model is listed even when the provider lists none');
 assert.equal(byName.compact.disabled,'Wait for the turn to finish');
 assert.equal(byName.run.disabled,'No command block to run yet');
 assert.equal(byName.review.disabled,'Only in a workspace');
 assert.equal(byName.stop.disabled,undefined,'busy: stop is offered');
 assert.equal(lazyboxCommands(ctx()).find(c=>c.name==='stop')!.disabled,'Claude is not working');
 assert.equal(lazyboxCommands(ctx({busy:false,provider:'codex',compact:false})).find(c=>c.name==='compact')!.disabled,'Not available until the agent reports its commands');});

test('/browser gives the agent the browser when it has none, and says why it cannot otherwise',()=>{
 const browser=(over:Partial<SlashContext>)=>lazyboxCommands(ctx(over)).find(c=>c.name==='browser')!;
 assert.equal(browser({browser:'can_give'}).disabled,undefined);
 assert.equal(browser({browser:'can_give'}).description,'Give Claude the browser (restarts the agent; the conversation continues)');
 assert.equal(browser({browser:'has'}).disabled,'Claude has the browser');
 assert.equal(browser({browser:'off'}).disabled,'The agent browser is off in Settings → Browser');
 assert.equal(browser({browser:''}).disabled,'The agent browser is not available in this conversation');
 assert.equal(browser({browser:'can_give',live:false}).disabled,'The conversation is not running');});

test('send as text: commands run, the agent\'s go through, an unknown /word passes through, anything else is a message',()=>{
 assert.deepEqual(decideSend('hello',all()),{kind:'text'});
 assert.deepEqual(decideSend('/usr/local/bin is missing',all()),{kind:'text'},'a path is a message');
 assert.deepEqual(decideSend('/ 2',all()),{kind:'text'});
 const model=decideSend('/model sonnet',all());assert.equal(model.kind,'lazybox');assert.equal(model.kind==='lazybox'&&model.args,'sonnet');
 const pr=decideSend('/review-pr 12',all());assert.equal(pr.kind,'provider');assert.equal(pr.kind==='provider'&&pr.args,'12');
 assert.equal(decideSend('/INIT',all()).kind,'provider','names are case-insensitive');
 assert.equal(decideSend('/test',all()).kind,'snippet');
 assert.deepEqual(decideSend('/deploy now',all()),{kind:'text'});
 assert.deepEqual(decideSend('/compact',lazyboxCommands(ctx({provider:'codex',compact:false}))),{kind:'text'});
 assert.equal(decideSend('/compact  ',all()).kind,'lazybox','Lazybox\'s /compact stands in for the agent\'s');});

test('models resolve by id, name or prefix; an id the list lacks is kept as typed',()=>{
 const models=ctx().models;
 assert.equal(resolveModel('sonnet 5',models),'claude-sonnet-5');assert.equal(resolveModel('Opus',models),'claude-opus-5-5');
 assert.equal(resolveModel('claude-haiku-5',models),'claude-haiku-5');assert.equal(resolveModel('  ',models),'');});

// What Claude Code 2.x lists (its initialize response), and the Runner's built-in Claude list.
const claudeListed=[{value:'default',displayName:'Default (recommended)',description:'Opus 5.5 · Best for everyday, complex tasks'},{value:'opus',displayName:'Opus 5.5',description:'For complex work'},{value:'claude-fable-5-2',displayName:'Fable 5.2',description:'Toughest'},{value:'sonnet',displayName:'Sonnet 5.5',description:'Efficient'},{value:'haiku',displayName:'Haiku 5.5',description:'Fastest'},{value:'fable',displayName:'Fable 5.1',description:'Longest'},{value:'claude-opus-4-8',displayName:'Opus 4.8',description:''}];
const claudeBuiltIn=[{value:'claude-opus-5-5',displayName:'Opus 5.5',description:''},{value:'claude-sonnet-5-5',displayName:'Sonnet 5.5',description:''},{value:'claude-haiku-5-5',displayName:'Haiku 5.5',description:''},{value:'claude-fable-5-2',displayName:'Fable 5.2',description:''}];

test('/model pins the current model and sorts by name regardless of catalog order', () => {
 const models = [
  {value:'gpt-10',displayName:'GPT-10',description:''},
  {value:'gpt-2',displayName:'GPT-2',description:''},
  {value:'gpt-6',displayName:'GPT-6',description:''},
 ];
 for (const listed of [models, [...models].reverse()]) {
  const menu = slashMenu('/model ', lazyboxCommands(ctx({provider:'codex', model:'gpt-6', models:listed})));
  assert.equal(menu?.kind, 'choices');
  if (menu?.kind !== 'choices') throw new Error('Expected model choices');
  assert.deepEqual(menu.items.map(item => item.value), ['gpt-6', 'gpt-2', 'gpt-10']);
  assert.equal(menu.items[menu.best].value, 'gpt-2');
 }
});

test('/model lists the provider\'s models, the Runner\'s first, then what the agent listed, each once',()=>{
 const merged=mergeModels(claudeBuiltIn,claudeListed);
 assert.deepEqual(merged.map(m=>m.value),['claude-opus-5-5','claude-sonnet-5-5','claude-haiku-5-5','claude-fable-5-2','default','fable','claude-opus-4-8'],'an alias of a listed model (by name) is listed once');
 const choices=modelChoices(merged,'claude-opus-5-5');
 assert.deepEqual(choices.filter(c=>c.current).map(c=>c.value),['claude-opus-5-5'],'the current model is marked once');
 assert.equal(choices[0].label,'Opus 5.5');assert.match(choices[0].detail||'',/^claude-opus-5-5/,'the id follows the name');
 const {items,best}=rankChoices('',choices);assert.equal(items[best].value,'claude-sonnet-5-5','the current model is not offered first as a switch');});

test('an agent started by an older Lazybox lists nothing: the Runner\'s list, never only the current model',()=>{
 const choices=lazyboxCommands(ctx({models:[],providerModels:claudeBuiltIn,liveModel:false})).find(c=>c.name==='model')!.choices!;
 assert.deepEqual(choices.map(c=>[c.label,!!c.current]),[['Opus 5.5',true],['Sonnet 5.5',false],['Haiku 5.5',false],['Fable 5.2',false]]);
 // Nothing listed anywhere: what runs now, by its name.
 assert.deepEqual(modelChoices([],'claude-opus-5-5').map(c=>[c.value,c.label,c.current]),[['claude-opus-5-5','Opus 5.5',true]]);
 // Codex: what its app-server listed; a current model the list lacks is first, marked.
 const codex=modelChoices(mergeModels([{value:'gpt-6.1-sol',displayName:'GPT-6.1-Sol',description:''},{value:'gpt-6-luna',displayName:'GPT-6-Luna',description:''}],[]),'gpt-custom');
 assert.deepEqual(codex.map(c=>[c.value,!!c.current]),[['gpt-custom',true],['gpt-6.1-sol',false],['gpt-6-luna',false]]);});

test('the current model is found by id, by name or as an alias, and an alias of it is never offered as a switch',()=>{
 // The agent reports the full id; it lists an alias of it under a short name.
 const listed=[{value:'opus',displayName:'Opus',description:''},{value:'sonnet',displayName:'Sonnet',description:''}];
 const choices=modelChoices(listed,'claude-opus-5-5');
 assert.deepEqual(choices.map(c=>[c.value,!!c.current]),[['opus',true],['sonnet',false]],'not listed twice: the alias is the current model');
 const {items,best}=rankChoices('',choices);assert.equal(items[best].value,'sonnet');
 assert.equal(sameModel({value:'opus',displayName:'Opus 5.5',description:''},'claude-opus-5-5'),true);
 assert.equal(sameModel({value:'fable',displayName:'Fable 5.1',description:''},'claude-fable-5-2'),false,'an alias for another version is another model');
 assert.equal(sameModel({value:'default',displayName:'Default (recommended)',description:''},'claude-opus-5-5'),false);
 // After a switch by alias the agent reports the alias: the newest model of that name is current.
 assert.deepEqual(modelChoices(mergeModels(claudeBuiltIn,[{value:'claude-sonnet-5',displayName:'Sonnet 5',description:''}]),'sonnet').filter(c=>c.current).map(c=>c.value),['claude-sonnet-5-5']);
 // Typing the alias of the current model resolves to it.
 assert.equal(resolveModel('opus',mergeModels(claudeBuiltIn,claudeListed)),'claude-opus-5-5');});

test('the restart card says why, when, and that the conversation continues, in one sentence',()=>{
 assert.equal(modelRestartText('claude','Claude','Sonnet 5.5',false,false),'Switch to Sonnet 5.5? An older Lazybox started this conversation\'s agent, so switching restarts it and your conversation continues.');
 assert.match(modelRestartText('claude','Claude','Sonnet 5.5',true,false),/restarts it when this turn ends/);
 assert.match(modelRestartText('acp:x','Gemini CLI','Flash',false,false),/Gemini CLI cannot change models mid-conversation/);
 assert.equal(modelRestartText('claude','Claude','Sonnet 5.5',true,true),'Switching to Sonnet 5.5 when this turn ends: Claude restarts and your conversation continues.');
 assert.doesNotMatch(modelRestartText('claude','Claude','Sonnet 5.5',false,false),/before models could change/);});

test('the line a command leaves in the conversation',()=>{
 assert.equal(commandLine('model_changed','claude-sonnet-5-5','next_turn',{}),'Switched model to claude-sonnet-5-5 · from the next turn');
 assert.equal(shortTokens(156000),'156k');assert.equal(shortTokens(61_400),'61k');assert.equal(shortTokens(1_200_000),'1.2M');assert.equal(shortTokens(800),'800');
 assert.equal(commandLine('model_changed','claude-opus-5-5','live',{}),'Switched model to claude-opus-5-5');
 assert.equal(commandLine('model_changed','claude-opus-5-5','restart',{}),'Switched model to claude-opus-5-5 · the agent restarted');
 assert.equal(commandLine('permission_changed','yolo','live',{}),'Permissions: Full access');
 assert.equal(commandLine('compacted','','',{trigger:'manual',preTokens:156000,postTokens:61000}),'Compacted context: 156k → 61k tokens');
 assert.equal(commandLine('compacted','','',{trigger:'auto',preTokens:180000}),'Context compacted automatically from 180k tokens');
 assert.equal(commandLine('message_delta','hi','',{}),'');});

test('the conversation: the agent\'s catalog is kept, commands run are one line, a model switch is the model shown',()=>{
 let state=emptyTranscript();
 const add=(type:string,text='',data?:unknown,status='')=>{state=appendTranscript(state,create(JournalEventSchema,{cursor:state.cursor+1n,event:{type,text,status,turnId:'t',providerDataJson:data?new TextEncoder().encode(JSON.stringify(data)):new Uint8Array()}}));};
 add('command_catalog','',{commands:[{name:'review-pr',description:'Review',argumentHint:'<n>'}],models:[{value:'opus',displayName:'Opus'}]});
 add('command_catalog','',{commands:[{name:'review-pr',description:'Review',argumentHint:'<n>'},{name:'deploy',description:'Ship',argumentHint:''}]});
 assert.deepEqual(state.catalog.commands.map(c=>c.name),['review-pr','deploy']);assert.equal(state.catalog.models[0].value,'opus','a later commands list keeps the models');
 assert.equal(state.messages.length,0,'the catalog is not a message');
 add('operation_accepted','/review-pr 12');add('operation_accepted','/usr/bin is gone');add('operation_accepted','/compact');
 assert.deepEqual(state.messages.map(m=>[m.text,!!m.command]),[['/review-pr 12',true],['/usr/bin is gone',false],['/compact',true]]);
 add('compacted','',{trigger:'manual',preTokens:156000,postTokens:61000});
 add('model_changed','claude-sonnet-5',undefined,'live');add('permission_changed','yolo',undefined,'live');
 assert.deepEqual(state.messages.slice(3).map(m=>[m.type,m.text]),[['command','Compacted context: 156k → 61k tokens'],['command','Switched model to claude-sonnet-5'],['command','Permissions: Full access']]);
 assert.equal(state.reportedModel,'claude-sonnet-5');});

test('the mode an agent is in is the current choice, never a blocked one: only Full access in plan mode is',()=>{
 // A Codex conversation in Full access: Full access is current (selected), not unavailable.
 const perms=lazyboxCommands(ctx({permission:'yolo'})).find(c=>c.name==='permissions')!.choices!;
 assert.deepEqual(perms.map(c=>[c.value,!!c.current,c.disabled||'']),[['ask',false,''],['read-only',false,''],['yolo',true,'']]);
 assert.equal(permissionBlocked('','yolo'),'','the current mode is not blocked');
 assert.equal(permissionBlocked('plan','yolo'),'Full access is unavailable in plan mode');
 assert.equal(permissionBlocked('plan','ask'),'');});

test('a conversation on the provider\'s default model has a current choice: Default',()=>{
 // Codex on its own default (no model asked for, none reported yet): Default is current, the models follow.
 const codex=modelChoices([{value:'gpt-6.1-sol',displayName:'GPT-6.1-Sol',description:''},{value:'gpt-6-luna',displayName:'GPT-6-Luna',description:''}],'');
 assert.deepEqual(codex.map(c=>[c.value,c.label,!!c.current]),[[DEFAULT_MODEL,'Default',true],['gpt-6.1-sol','GPT-6.1-Sol',false],['gpt-6-luna','GPT-6-Luna',false]]);
 const {items,best}=rankChoices('',codex);assert.equal(items[best].value,'gpt-6.1-sol','Default is not offered as a switch');
 // Claude Code lists its own Default entry: that one is current, not a second Default.
 const claude=modelChoices(mergeModels(claudeBuiltIn,claudeListed),'');
 assert.deepEqual(claude.filter(c=>c.current).map(c=>c.label),['Default (recommended)']);
 assert.equal(claude.filter(c=>c.value===DEFAULT_MODEL).length,1);
 // Through /model: the context's model is empty.
 assert.equal(lazyboxCommands(ctx({model:'',models:[],providerModels:claudeBuiltIn})).find(c=>c.name==='model')!.choices!.find(c=>c.current)!.label,'Default');});

test('the ⋯ menu shows the first models and the current one, the rest behind More',()=>{
 const twelve=Array.from({length:12},(_,i)=>({value:`m${i}`,current:i===9}));
 const {shown,more}=shownModelChoices(twelve,false);
 assert.deepEqual(shown.map(c=>c.value),['m0','m1','m2','m3','m9'],'the current model is shown wherever it is listed');
 assert.equal(more,7);
 assert.equal(shownModelChoices(twelve,true).shown.length,12);
 assert.equal(shownModelChoices(twelve.slice(0,MODEL_CHOICES_SHOWN+1),false).more,0,'one hidden model is not worth a More button');});

test('model rows stay unique when the provider repeats or shuffles effort levels', () => {
 const models = [
  {value:'fable',displayName:'Fable 5',description:'',effortLevels:['max','low','high','medium','xhigh','low']},
  {value:'default',displayName:'Default (recommended)',description:'',effortLevels:['high','low']},
  {value:'sonnet',displayName:'Sonnet 5',description:''},
 ];
 const commands=lazyboxCommands(ctx({model:'fable',effort:'high',providerModels:models,models:[]}));
 const menu=slashMenu('/model ',commands);
 assert.ok(menu?.kind==='choices');
 assert.deepEqual(menu.items.map(item=>item.value),['fable','default','sonnet']);
 const efforts=commands.find(command=>command.name==='effort')!.choices!;
 assert.deepEqual(efforts.map(item=>item.value),['low','medium','high','xhigh','max']);
 assert.deepEqual(efforts.filter(item=>item.current).map(item=>item.value),['high']);
});

test('typing /rev highlights the most-used matching name across groups', () => {
  const commands = [...all(ctx({reviewers:[{key:'codex',label:'Codex'}]})), ...snippetCommands([
    {key:'rev', description:'Review the current diff', category:'Review', body:'Review it.'},
  ], [])];
  const best = (review: number, rev: number) => {
    const menu = slashMenu('/rev', commands, false, {
      'lazybox:review': {count: review}, 'snippet:rev': {count: rev}, 'lazybox:help': {count: 99},
    });
    assert.ok(menu?.kind === 'commands');
    return menu.items[menu.best].command.key;
  };
  assert.equal(best(0, 0), 'snippet:rev');
  assert.equal(best(4, 1), 'lazybox:review');
  assert.equal(best(4, 4), 'snippet:rev');
});
