// Lazybox context menus on a running turn, against the fake Runner in WebKit:
// the live bar (Stop, Open in console, copy the command), a sub-agent's card
// and chip (its steps, a panel, copy its prompt, stop it) and a background
// command's chip (Show output, Copy command). Part of test:context-menus.
import {createServer} from 'vite';
import react from '@vitejs/plugin-react';
import {webkit,chromium,expect} from './fixtures/playwright.mjs';
import {fileURLToPath} from 'node:url';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import {testViteCache} from './fixtures/vite-cache.mjs';
import { collectPageErrors } from './fixtures/page-errors.mjs';

const f=await startRunnerFixture();
const b64=value=>Buffer.from(JSON.stringify(value)).toString('base64');
const now=Date.now(),at=offset=>new Date(now+offset).toISOString();
const session=f.sessions[0];
const SA='claude-tool:toolu_sa',BG='claude-tool:toolu_bg';
const SCRIPT='cd /work/api && uv run pytest tests/test_mcp.py -x -q';
Object.assign(session,{title:'Ship the front door',provider:'claude',busy:true,status:'running',capabilities:{...session.capabilities,stopAgent:true},agents:[
 {id:'root',status:'running',phase:'tool',tool:'Bash',phaseSinceUnixMillis:String(now-321000),turnStartedAtUnixMillis:String(now-400000)},
 {id:SA,parentId:'root',status:'running',name:'Review the API',kind:'local_agent',nativeId:'task-sa'},
 {id:BG,parentId:'root',status:'running',name:SCRIPT,kind:'local_bash',nativeId:'bash-bg'}]});
const output='step one\nCHECK_TAIL_LINE\n';
f.commands.push({id:'cmd-check',sessionId:session.id,title:'pnpm run check',argv:['pnpm','run','check'],mode:'managed',done:false,offset:String(output.length),output});
const ch=f.openEvents(session.id);
const push=(offset,event)=>ch.push(event.data===undefined?event:{...Object.fromEntries(Object.entries(event).filter(([k])=>k!=='data')),providerDataJson:b64(event.data)},at(offset));
push(-400000,{type:'operation_accepted',text:'Run the checks and fix what fails.'});
push(-399000,{type:'turn_started'});
push(-390000,{type:'message_delta',text:'Running the checks first.'});
push(-321000,{type:'activity',status:'tool',name:'Bash'});
push(-321000,{type:'tool_activity',data:{type:'tool_call',id:'toolu_check',name:'Bash',status:'running',input:{command:'lazybox run pnpm run check',description:'Run the checks'}}});
push(-300000,{type:'tool_activity',data:{type:'tool_call',id:'toolu_sa',name:'Task',status:'running',input:{description:'Review the API',prompt:'Review the API for missing checks.',subagent_type:'Explore'}}});
push(-299000,{type:'agent_status',agentId:SA,parentAgentId:'root',nativeId:'task-sa',status:'running',name:'Review the API',agentKind:'local_agent',data:{toolUseId:'toolu_sa',subagentType:'Explore',prompt:'Review the API for missing checks.'}});
push(-350000,{type:'tool_activity',data:{type:'tool_call',id:'toolu_bg',name:'Bash',status:'running',input:{command:SCRIPT,run_in_background:true}}});
push(-349900,{type:'agent_status',agentId:BG,parentAgentId:'root',nativeId:'bash_3',status:'running',name:SCRIPT,agentKind:'local_bash',data:{toolUseId:'toolu_bg',taskType:'local_bash'}});

const server=await createServer({...testViteCache(import.meta.url),configFile:false,logLevel:'error',root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:f.url}}}});
let browser;
try{
 await server.listen();
 browser=await(process.env.TEST_CHROMIUM?chromium:webkit).launch({headless:true});
 const context=await browser.newContext({viewport:{width:1440,height:900}});
 await context.addInitScript(()=>{
  window.__copied=[];
  Object.defineProperty(navigator,'clipboard',{configurable:true,value:{writeText:async text=>{window.__copied.push(text);},readText:async()=>'',write:async items=>{for(const item of items)for(const type of item.types)await item.getType(type).catch(()=>{});},read:async()=>[]}});
  window.__menus=[];window.addEventListener('contextmenu',event=>{setTimeout(()=>window.__menus.push(event.defaultPrevented),0);});
 });
 const page=await context.newPage();page.setDefaultTimeout(15000);
 const errors=[];await collectPageErrors(page, errors);
 await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=${session.id}#token=${f.token}`);
 const chat=page.locator('.embedded-chat:visible').first(),bar=chat.getByRole('region',{name:'What the agent is doing'});
 const menu=page.locator('.context-menu[data-depth="0"]'),item=name=>menu.getByRole('menuitem',{name,exact:true});
 const labels=()=>menu.locator('.context-menu-label').allTextContents();
 const copied=()=>page.evaluate(()=>window.__copied.at(-1));
 const open=async locator=>{await locator.click({button:'right'});await expect(menu).toBeVisible();await page.waitForTimeout(30);assert.equal(await page.evaluate(()=>window.__menus.at(-1)),true,'the browser menu never shows');};
 const close=async()=>{await page.keyboard.press('Escape');await expect(menu).toHaveCount(0);};

 // The live bar: its buttons, and copying what runs.
 await expect(bar.locator('.live-bar-output')).toContainText('CHECK_TAIL_LINE');
 await open(bar.locator('.live-activity'));
 assert.deepEqual((await labels()).slice(0,3),['Stop command','Open in console','More output']);
 assert.ok((await labels()).includes('Copy command'));
 await item('Copy command').click();assert.match(await copied(),/pnpm run check/);
 await open(bar.locator('.live-activity'));await item('Open in console').click();
 await expect(page.locator('.command-panel')).toContainText('CHECK_TAIL_LINE');
 await open(bar.locator('.live-activity'));await item('Stop command').click();
 await expect.poll(()=>f.mutations.findLast(m=>m.method==='CancelCommand')?.body.commandId).toBe('cmd-check');

 // A sub-agent's card: its steps, a panel, its prompt, and stopping it.
 const card=chat.locator(`.subagent-card[data-agent-id="${SA}"]`);
 await expect(card).toBeVisible();
 await open(card.locator('.subagent-who'));
 assert.deepEqual((await labels()).slice(0,3),['Show its steps','Open in a panel','Copy prompt']);
 assert.ok((await labels()).includes('Stop sub-agent'));
 await item('Copy prompt').click();assert.equal(await copied(),'Review the API for missing checks.');
 // Its chip in the live bar has the same menu.
 await open(bar.locator('.subagent-chip').filter({hasText:'Review the API'}));
 assert.equal((await labels())[1],'Open in a panel');
 await item('Open in a panel').click();
 await expect(page.locator('.subagent-panel, [aria-label*="Review the API"][role="dialog"], [aria-label*="Review the API"][role="complementary"]').first()).toBeVisible();

 // A background command's chip: Show output, Copy command.
 const bg=bar.locator('.subagent-chip.command');
 await expect(bg).toBeVisible();
 await open(bg);
 assert.deepEqual((await labels()).slice(0,2),['Show output','Copy command']);
 assert.ok((await labels()).includes('Stop background command'));
 await item('Copy command').click();assert.equal(await copied(),SCRIPT);
 await open(bg);await item('Show output').click();await expect(page.locator('.background-command').first()).toBeVisible();
 await close().catch(()=>{});

 assert.deepEqual(errors,[]);
 console.log('PASS: context menus on the running turn: the live bar (stop, open in console, copy), a sub-agent card and chip (steps, panel, copy prompt, stop) and a background command (show output, copy command). Fixture only.');
}finally{await browser?.close();await server.close();await f.close();}
