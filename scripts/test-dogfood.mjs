import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from 'vite';
import react from '@vitejs/plugin-react';
import {chromium,expect} from './fixtures/playwright.mjs';
import {fileURLToPath} from 'node:url';
import {mkdir} from 'node:fs/promises';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
const fixture=await startRunnerFixture();
fixture.sessions.push({...fixture.sessions[0],id:'alpha-history',workId:'alpha-history',title:'Earlier investigation',closed:true,status:'stopped'});
fixture.sessions[0].title='Investigate API contract';fixture.sessions[1].title='Release web';
const server=await createServer({...testViteCache(import.meta.url),configFile:false,root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
let browser;
try{
 await server.listen();browser=await chromium.launch({headless:true});const page=await browser.newPage({viewport:{width:1500,height:1000}});page.setDefaultTimeout(10000);
 const errors=[];page.on('pageerror',e=>errors.push(String(e)));const reads=[];
 const lists=[];await page.route('**/ListWorkspaceChanges',route=>{lists.push(route.request().postDataJSON());return route.fulfill({json:{repositories:[{member:'api',branch:'feature/steady-otter',head:'a'.repeat(40),base:'c'.repeat(40),committed:[{path:'handler.go',indexStatus:'M'}],branches:['feature/steady-otter','main','origin/main','release/2'],defaultBranch:'origin/main',commits:[{id:'d'.repeat(40),subject:'Add contract handler',committedAtUnix:String(Math.floor(Date.now()/1000)-120)}],files:[{path:'contract.ts',indexStatus:'M',worktreeStatus:'M'}]},{member:'web',branch:'feature/steady-otter',head:'b'.repeat(40),files:[{path:'new-view.tsx',indexStatus:'?',worktreeStatus:'?'}]}]}})});
 await page.route('**/ReadWorkspaceDiff',route=>{const body=route.request().postDataJSON();reads.push(body);return route.fulfill({json:{text:body.committed?'@@ branch @@\n+branch change':body.staged?'@@ staged @@\n+saved change':'@@ working tree @@\n+working change'}})});
 const url=`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace#token=${fixture.token}`;await page.goto(url);
 await page.getByRole('combobox',{name:'Workspace',exact:true}).selectOption('alpha');await page.getByRole('combobox',{name:'Session',exact:true}).selectOption('alpha-session');
 const chat=page.locator('[data-panel-id="alpha-work-chat"]');await chat.getByRole('button',{name:'Rename conversation',exact:true}).click();await chat.getByRole('textbox',{name:'Conversation name',exact:true}).fill('Ship API contract');await chat.getByRole('button',{name:'Save conversation name',exact:true}).click();await expect(chat.locator('.conversation-title')).toContainText('Ship API contract');
 await page.getByRole('button',{name:'Conversation history',exact:true}).click();const history=page.getByRole('dialog',{name:'Conversation history',exact:true});const filter=history.getByRole('searchbox');await filter.fill('SHIP');await expect(history.getByRole('button',{name:/Ship API contract/})).toBeVisible();await filter.fill('missing');await expect(history.getByText('No conversation titles match this search.')).toBeVisible();await page.getByRole('button',{name:'Close conversation history'}).click();
 const message=chat.getByRole('textbox',{name:'Message',exact:true});await message.fill('Keep this draft through review');
 await page.getByRole('button',{name:'Review changes',exact:true}).click();const changes=page.getByRole('region',{name:'Workspace changes'});
 await expect(changes).toContainText('1 file changed on the branch · 2 uncommitted');
 // The first branch change opens by itself, compared with the workspace start.
 await expect(changes.getByLabel('File diff')).toContainText('+branch change');assert.equal(reads.at(-1).committed,true);assert.equal(reads.at(-1).member,'api');await expect(changes.getByRole('checkbox',{name:'Staged',exact:true})).toHaveCount(0);
 await changes.getByText('1 commit since cccccccc').click();await expect(changes.getByText('Add contract handler')).toBeVisible();
 // Compare with the default branch, another branch, or uncommitted work only.
 const compare=changes.getByRole('combobox',{name:'Compare with',exact:true});
 await expect(compare.locator('option')).toContainText(['Uncommitted only','Workspace start','origin/main','main','release/2']);
 await compare.selectOption('origin/main');await expect.poll(()=>lists.at(-1)?.compare).toBe('origin/main');await expect(changes).toContainText('1 file changed vs origin/main');
 await expect(changes.getByLabel('File diff')).toContainText('+branch change');assert.equal(reads.at(-1).compare,'origin/main');
 await compare.selectOption({label:'Uncommitted only'});await expect.poll(()=>lists.at(-1)?.compare??'').toBe('');await expect(changes.getByRole('button',{name:'M handler.go'})).toHaveCount(0);await expect(changes).toContainText('2 uncommitted');
 await compare.selectOption({label:'Workspace start'});
await changes.getByRole('button',{name:'MM contract.ts'}).click();await expect(changes.getByLabel('File diff')).toContainText('+working change');await changes.getByRole('checkbox',{name:'Staged',exact:true}).check();await expect(changes.getByLabel('File diff')).toContainText('+saved change');assert.equal(reads.at(-1).collectionId,'alpha');assert.equal(reads.at(-1).member,'api');assert.equal(reads.at(-1).staged,true);
 await changes.getByRole('button',{name:'?? new-view.tsx'}).click();await expect(changes.getByRole('checkbox',{name:'Staged',exact:true})).not.toBeChecked();await expect(changes.getByLabel('File diff')).toContainText('+working change');
 await mkdir('.cache',{recursive:true});await page.screenshot({path:'.cache/dogfood-changes.png'});
 await page.getByRole('button',{name:'Work',exact:true}).click();await expect(message).toHaveValue('Keep this draft through review');
 await page.reload();await expect(page.locator('[data-panel-id="alpha-work-chat"]').getByRole('textbox',{name:'Message',exact:true})).toHaveValue('Keep this draft through review');await expect(page.locator('.conversation-title')).toContainText('Ship API contract');
 assert.deepEqual(errors,[]);assert.deepEqual(fixture.mutations.map(m=>m.method),['RenameConversation']);console.log('PASS: conversation rename/search, multi-repository review, staged diff selection, persisted draft/page, no execution or Git mutation.');
}finally{await browser?.close();await server.close();await fixture.close();}
