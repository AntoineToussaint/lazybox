// Fixture only: per-turn code checkpoints in the conversation. A quiet marker
// after each finished turn, its changes on the Changes page, a revert that
// says exactly what it will change before it does, and an undo.
import {createServer} from 'vite';
import react from '@vitejs/plugin-react';
import {webkit,expect} from './fixtures/playwright.mjs';
import {fileURLToPath} from 'node:url';
import {mkdir} from 'node:fs/promises';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import {testViteCache} from './fixtures/vite-cache.mjs';

const f=await startRunnerFixture();
const session=f.sessions[0];
// Mid-turn: the third turn is running.
Object.assign(session,{title:'Parser work',provider:'claude',busy:true,status:'running',agents:[{id:'root',status:'running'}]});
const events=f.openEvents(session.id);
const say=(operationId,prompt,reply)=>{events.push({type:'operation_accepted',operationId,text:prompt});events.push({type:'message_delta',text:reply});events.push({type:'turn_ended'});};
say('op-1','Add a parser for the config file','REPLY_ONE: parser added.');
say('op-2','Add tests for it','REPLY_TWO: tests added.');
events.push({type:'operation_accepted',operationId:'op-3',text:'Now tidy the docs'});events.push({type:'message_delta',text:'REPLY_THREE: working on the docs.'});
const state=f.setCheckpoints(session.id,{turns:[
 {operationId:'op-1',prompt:'Add a parser for the config file',files:[{member:'api',path:'parser.go',status:'A',additions:40},{member:'api',path:'README.md',status:'M',additions:2,deletions:1}]},
 {operationId:'op-2',prompt:'Add tests for it',files:[{member:'web',path:'app.test.ts',status:'A',additions:12}]},
 {operationId:'op-3',prompt:'Now tidy the docs',files:[],ended:false},
]});
state.commitsSince={api:1};

const server=await createServer({...testViteCache(import.meta.url),configFile:false,logLevel:'error',root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:f.url}}}});
let browser;
try{
 await server.listen();await mkdir('.cache',{recursive:true});
 browser=await webkit.launch();const page=await browser.newPage({viewport:{width:1440,height:900}});page.setDefaultTimeout(15000);
 const errors=[];page.on('pageerror',e=>errors.push(String(e)));
 await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=${session.id}#token=${f.token}`);
 const chat=page.locator('.messages:visible');
 await expect(chat).toContainText('REPLY_THREE');
 const marker=n=>chat.getByRole('group',{name:`Turn ${n} checkpoint`});
 // A marker is one quiet divider: its actions are in its ⋯ menu.
 const action=async(n,name)=>{await marker(n).getByLabel(`Turn ${n} actions`,{exact:true}).click();return marker(n).getByRole('button',{name});};
 const closeMenu=()=>page.keyboard.press('Escape');

 // Mid-turn: finished turns have markers, the running one has none, and nothing can be reverted.
 await expect(marker(1)).toContainText('Turn 1 · 2 files changed · +42 −1');
 await expect(marker(2)).toContainText('Turn 2 · 1 file changed · +12');await expect(marker(2)).not.toContainText('−0');
 await expect(marker(3)).toHaveCount(0);
 {const revert=await action(1,'Revert to before this turn…');await expect(revert).toBeDisabled();
  assert.match(await revert.getAttribute('title'),/Wait for the agent’s turn to end/);await closeMenu();}
 {const box=await marker(1).boundingBox();assert.ok(box.height<=26,`a marker is one line (${box.height}px)`);}
 // The marker closes its turn: after the reply, before the next prompt.
 assert.deepEqual(await marker(1).evaluate(node=>[node.previousElementSibling?.textContent?.includes('REPLY_ONE'),node.nextElementSibling?.textContent?.includes('Add tests for it')]),[true,true]);

 // The turn ends: its marker appears; an empty turn offers no changes to show.
 Object.assign(session,{busy:false,status:'ready',agents:[{id:'root',status:'done'}]});state.turns[2].ended=true;events.push({type:'turn_ended'});
 await expect(marker(3)).toContainText('Turn 3 · No file changes');
 {await action(3,'Revert to before this turn…');await expect(marker(3).getByRole('button',{name:/Show changes/})).toHaveCount(0);await closeMenu();}
 {const revert=await action(1,'Revert to before this turn…');await expect(revert).toBeEnabled();await closeMenu();}
 await page.screenshot({path:'.cache/turn-checkpoints-markers.png'});

 // Revert asks first and lists exactly what will change; cancelling changes nothing.
 await (await action(1,'Revert to before this turn…')).click();
 const dialog=page.getByRole('dialog',{name:'Revert files to before turn 1'});
 await expect(dialog).toContainText('3 files will change');
 await expect(dialog.getByLabel('Files that will change')).toContainText('Removedparser.go');
 await expect(dialog.getByLabel('Files that will change')).toContainText('Put backREADME.md');
 await expect(dialog.getByLabel('Files that will change')).toContainText('Removedapp.test.ts');
 await expect(dialog).toContainText('1 commit made since stays on the branch');
 await expect(dialog).toContainText('The current files are saved first, so you can undo this revert.');
 await expect(dialog.getByRole('button',{name:'Cancel',exact:true})).toBeFocused();
 await page.screenshot({path:'.cache/turn-checkpoints-revert-dialog.png'});
 await page.keyboard.press('Escape');
 await expect(dialog).toHaveCount(0);
 assert.equal(f.mutations.filter(m=>m.method==='RevertCode').length,0,'cancel reverted nothing');

 // Confirm: the files go back, the conversation says so, and the toast offers undo.
 await (await action(2,'Revert to before this turn…')).click();
 const second=page.getByRole('dialog',{name:'Revert files to before turn 2'});
 await second.getByRole('button',{name:'Revert 1 file'}).click();
 await expect(second).toHaveCount(0);
 assert.deepEqual(f.mutations.findLast(m=>m.method==='RevertCode').body,{sessionId:session.id,turn:2});
 const toast=page.locator('.checkpoint-toast');
 await expect(toast).toContainText('Reverted 1 file to before turn 2.');
 await expect(chat).toContainText('Files reverted to before turn 2.');
 await expect(marker(2)).toContainText('Files reverted to before this turn');
 await page.screenshot({path:'.cache/turn-checkpoints-reverted.png'});
 await toast.getByRole('button',{name:'Undo revert'}).click();
 await expect(toast).toContainText('Revert undone. 1 file back as they were.');
 assert.deepEqual(f.mutations.findLast(m=>m.method==='UndoRevertCode').body,{sessionId:session.id,revertId:'r1'});
 await expect(marker(2)).not.toContainText('Files reverted to before this turn');
 await expect(chat).toContainText('Revert to before turn 2 undone');

 // Another agent working in the workspace: the dialog says who, and refuses.
 f.sessions.push({id:'alpha-other',collectionId:'alpha',workId:'alpha',profileId:'claude-default',title:'Other agent',busy:true,status:'running',capabilities:{},pending:[]});
 await (await action(1,'Revert to before this turn…')).click();
 const blocked=page.getByRole('dialog',{name:'Revert files to before turn 1'});
 await expect(blocked.getByRole('alert')).toContainText('busy: “Other agent”');
 await expect(blocked.getByRole('button',{name:/^Revert \d+ files?$/})).toBeDisabled();
 await blocked.getByRole('button',{name:'Cancel',exact:true}).click();
 f.sessions.pop();

 // Show changes opens the workspace's Changes page on that turn: before → after.
 await (await action(1,'Show changes in this turn')).click();
 const changes=page.getByRole('region',{name:'Changes in turn 1'});
 await expect(changes).toContainText('2 files changed in turn 1');
 await expect(page.locator('.changes-panel>header')).toContainText('Agent turn 1 · “Add a parser for the config file”');
 await expect(page.getByLabel('Compare with')).toHaveValue(`:turn:${session.id}:1`);
 const files=changes.getByRole('navigation',{name:'Files changed in turn 1'});
 await expect(files.getByRole('button')).toHaveCount(2);
 await expect(changes.locator('.diff-view')).toContainText('+api/parser.go');
 await files.getByRole('button',{name:/README\.md/}).click();await expect(files.getByRole('button',{name:/README\.md/})).toHaveClass('selected');await expect(files.getByRole('button',{name:/parser\.go/})).not.toHaveClass('selected');
 await expect(changes.locator('.diff-view')).toContainText('+api/README.md');
 await expect(changes.locator('.diff-comparison')).toHaveText('Before turn 1 → after it');
 await page.screenshot({path:'.cache/turn-checkpoints-changes.png'});
 // The turns are a comparison like any other: pick another from the list.
 await page.getByLabel('Compare with').selectOption(`:turn:${session.id}:2`);
 await expect(page.getByRole('region',{name:'Changes in turn 2'})).toContainText('1 file changed in turn 2');
 await page.getByLabel('Compare with').selectOption(':start');
 await expect(page.getByRole('region',{name:/Changes in turn/})).toHaveCount(0);

 assert.deepEqual(errors,[]);
 console.log('PASS: turn checkpoint markers (mid-turn and after), revert confirmation listing affected files and kept commits, cancel, revert with conversation notice and undo toast, refusal while another agent works, and a turn\'s changes on the Changes page.');
}finally{await browser?.close();await server.close();await f.close();}
