import {test} from 'node:test';import assert from 'node:assert/strict';
import {actionDefinitions,actionMenuItems,actionsFor,bindings,canonicalKey,chordFromEvent,displayBinding,displayKey,isChord,nativeMenuItems,resolveMod,runAction,runMenuAction} from '../packages/surfaces/src/actions';
import {keyboardPreset} from '../packages/surfaces/src/keyboard';
import {fuzzyMatch,bestMatch} from '../packages/surfaces/src/fuzzy';
import {alertSoundFor, notificationRefusal, sendSystemNotice} from '../packages/surfaces/src/attention-alerts';
import {defaultNotificationPreferences} from '../packages/surfaces/src/attention';

test('every action is declared once, with a title, group and icon',()=>{
 const ids=actionDefinitions.map(a=>a.id);assert.equal(new Set(ids).size,ids.length);
 for(const a of actionDefinitions){assert.ok(a.title&&a.group&&a.icon,a.id);}
 // The ids keyboard settings were saved under before the registry are all still there.
 for(const id of ['navigation.exit','workspace.new','conversation.new','snippets.open','inbox.open','files.find','files.explorer','artifacts.open','navigation.next','navigation.previous','navigation.parent','navigation.open','chat.send'])assert.ok(ids.includes(id as never),id);});
test('keys parse, normalize and read like the platform writes them',()=>{
 assert.deepEqual(bindings('Meta+k  or ] ]  p'),['Meta+k','] ] p']);
 assert.equal(canonicalKey('Shift+Cmd+P'),'Meta+Shift+p');assert.equal(canonicalKey('Meta++'),'Meta++');
 assert.equal(resolveMod('Mod+Shift+p or ] ] p',true),'Meta+Shift+p or ] ] p');assert.equal(resolveMod('Mod+1',false),'Ctrl+1');
 assert.equal(isChord('Meta+k'),true);assert.equal(isChord('Ctrl+b s'),false);assert.equal(isChord('?'),false);assert.equal(isChord('Shift+a'),false);
 assert.equal(displayKey('Meta+Shift+p',true),'⇧⌘P');assert.equal(displayKey('Ctrl+Shift+p',false),'Ctrl+Shift+P');assert.equal(displayKey('ArrowDown',true),'↓');
 assert.deepEqual(displayBinding('] ] i',true),[']',']','i']);
 // By physical key: ⌥ and ⇧ do not change the letter or digit.
 assert.equal(chordFromEvent({key:'∆',code:'KeyJ',ctrlKey:false,metaKey:true,altKey:true,shiftKey:false}),'Meta+Alt+j');
 assert.equal(chordFromEvent({key:'!',code:'Digit1',ctrlKey:true,metaKey:false,altKey:false,shiftKey:true}),'Ctrl+Shift+1');
 assert.equal(chordFromEvent({key:']',code:'BracketRight',ctrlKey:false,metaKey:true,altKey:false,shiftKey:false}),'Meta+]');});
test('handlers: running an action reaches its handler; context menus and the native menu read the registry',async()=>{
 (globalThis as any).window ??= new EventTarget();(globalThis as any).CustomEvent ??= class extends Event{detail:unknown;constructor(type:string,init?:{detail?:unknown}){super(type);this.detail=init?.detail;}};
 const seen:string[]=[];window.addEventListener('lazybox-action',e=>seen.push((e as CustomEvent).detail));
 assert.equal(runAction('inbox.open'),false,'no handler mounted');assert.deepEqual(seen,['inbox.open'],'event listeners still hear it');
 const items=actionMenuItems(['conversation.new','inbox.open'],keyboardPreset('vim',true));
 assert.deepEqual(items.map(i=>[i.label,i.icon,i.disabled]),[['New conversation','chat',true],['Open the inbox','inbox',true]]);
 assert.equal(actionsFor({group:'General'}).every(a=>a.group==='General'),true);
 const menu=nativeMenuItems(keyboardPreset('vim',true));
 const palette=menu.find(i=>i.id==='action:palette.open')!;assert.equal(palette.accelerator,'Cmd+K');assert.equal(palette.section,'Go');
 assert.equal(menu.find(i=>i.id==='action:inbox.open')!.accelerator,undefined,'a sequence has no accelerator');
 // The menu item a key already ran is not run twice.
 runAction('attention.next',undefined,'keys');assert.equal(runMenuAction('attention.next'),false);});
test('fuzzy ranking: prefixes and word starts beat scattered letters',()=>{
 assert.equal(fuzzyMatch('xyz','New conversation'),null);
 const rank=(q:string,titles:string[])=>titles.map(t=>({t,m:fuzzyMatch(q,t)})).filter(r=>r.m).sort((a,b)=>b.m!.score-a.m!.score).map(r=>r.t);
 assert.deepEqual(rank('new',['Next conversation that needs you','New workspace','Review changes']).slice(0,1),['New workspace']);
 assert.deepEqual(rank('nc',['Open the inbox','Next conversation that needs you','New conversation','Insert a snippet']),['New conversation','Next conversation that needs you'],'word starts, then the shorter title');
 assert.equal(rank('gtf',['Go to file','Go to workspace 1','Keyboard shortcuts'])[0],'Go to file');
 assert.equal(rank('term',['Open a terminal','Layout configuration'])[0],'Open a terminal');
 assert.deepEqual(fuzzyMatch('conv','New conversation')!.positions,[4,5,6,7]);
 // A keyword match counts, but less than the title.
 assert.ok(bestMatch('cheatsheet','Keyboard shortcuts','keys help cheatsheet')!.score<bestMatch('keyboard','Keyboard shortcuts','keys')!.score);});
test('alert sounds: needs you on by default, done off',()=>{
 const notice=(kind:'needs-input'|'failed'|'review')=>({key:'k',kind,title:'',body:'',collectionId:'',sessionId:''});
 assert.equal(alertSoundFor([notice('needs-input')],defaultNotificationPreferences),'request');
 assert.equal(alertSoundFor([notice('failed')],defaultNotificationPreferences),'request');
 assert.equal(alertSoundFor([notice('review')],defaultNotificationPreferences),undefined);
 assert.equal(alertSoundFor([notice('review')],{...defaultNotificationPreferences,soundDone:true}),'done');
 assert.equal(alertSoundFor([notice('needs-input')],{...defaultNotificationPreferences,soundNeedsYou:false}),undefined);});

test('a successful system banner clears the previous refusal', async () => {
  const notice = { key: 'reminder:1', title: 'Reminder', body: 'Check CI', collectionId: '', sessionId: '' };
  sendSystemNotice({ notifyAttention: async () => { throw new Error('Permission denied'); } }, notice);
  await Promise.resolve();
  assert.equal(notificationRefusal(), 'Permission denied');
  sendSystemNotice({ notifyAttention: async () => {} }, notice);
  await Promise.resolve();
  assert.equal(notificationRefusal(), '');
});
