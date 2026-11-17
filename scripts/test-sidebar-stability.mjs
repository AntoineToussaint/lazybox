// The Workspaces sidebar has one width, the person's: switching workspace
// never changes it, whatever the workspace's page holds. Seven workspaces are
// visited in turn, each saved with a different page: the default; no content
// panel at all (the sidebar alone: an empty content area takes the rest); only
// a group floating over the page; a group that was popped out; a chat in its own
// window whose window is gone; another arrangement with the sidebar on the
// right and a saved share for it; a group maximized. The width is checked to
// the pixel on every visit, after window resizes, and after a drag sets a new
// one. Then Agents: closed under Workspaces, it stays closed in every
// workspace, on every page and after a reload, the sidebar keeping its width
// and Workspaces the full height; Panels → Agents brings it back everywhere;
// folded, it stays folded the same way. WebKit (the desktop app's engine), the
// browser build, fake Runner.
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from 'vite';
import react from '@vitejs/plugin-react';
import {webkit,expect} from '@playwright/test';
import {fileURLToPath} from 'node:url';
import {mkdir} from 'node:fs/promises';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';

const fixture=await startRunnerFixture();
// Five more workspaces, each with a conversation of its own.
const names={gamma:'Gamma empty',delta:'Delta floating',epsilon:'Epsilon window',zeta:'Zeta popped',eta:'Eta arranged',theta:'Theta two pages'};
for(const [id,name] of Object.entries(names)){
 fixture.collections.push({...structuredClone(fixture.collections[1]),id,name,workId:id,root:`/fixture/collections/${id}`,ownerExecutionId:`${id}-session`});
 fixture.sessions.push({...structuredClone(fixture.sessions.find(s=>s.collectionId==='beta')||fixture.sessions[0]),id:`${id}-session`,collectionId:id,title:`${name} conversation`});
}
const panel=(id,kind,extra={})=>({id,kind,binding:{mode:'follow'},detached:false,...extra});
const group=(id,panels)=>({type:'group',id,panels,active:panels[0].id});
const sidebar=w=>group(`g-${w}-side`,[panel(`${w}-col`,'collections'),panel(`${w}-agents`,'agents')]);
const chat=w=>group(`g-${w}-chat`,[panel(`${w}-chat`,'chat')]);
const work=(w,fields)=>({id:'work',name:'Work',selection:{collectionId:w,sessionId:`${w}-session`,commandId:''},layout:null,floating:[],popouts:[],detached:[],...fields});
const page=(w,fields)=>({version:2,selectedPage:'work',pages:[{id:'work',name:'Work',selection:{collectionId:w,sessionId:`${w}-session`,commandId:''},layout:null,floating:[],popouts:[],detached:[],...fields}]});
const saved={
 gamma:page('gamma',{layout:sidebar('gamma')}),
 delta:page('delta',{layout:sidebar('delta'),floating:[{group:chat('delta'),box:{left:620,top:40,width:520,height:420}}]}),
 epsilon:page('epsilon',{layout:sidebar('epsilon'),detached:[panel('epsilon-chat','chat',{detached:true,binding:{mode:'pinned',target:{collectionId:'epsilon',sessionId:'epsilon-session',commandId:''}}})]}),
 zeta:page('zeta',{layout:sidebar('zeta'),popouts:[group('g-zeta-editor',[panel('zeta-editor','editor')])]}),
 theta:{version:2,selectedPage:'work',pages:[work('theta',{layout:{type:'split',direction:'row',sizes:[0.2,0.8],children:[sidebar('theta'),chat('theta')]}}),work('theta',{id:'more',name:'More',layout:{type:'split',direction:'row',sizes:[0.2,0.8],children:[sidebar('theta-more'),group('g-theta-files',[panel('theta-files','files')])]}})]},
 eta:page('eta',{layout:{type:'split',direction:'row',sizes:[0.55,0.45],children:[{type:'split',direction:'column',sizes:[0.5,0.5],children:[chat('eta'),group('g-eta-files',[panel('eta-files','files')])]},sidebar('eta')]}}),
};

const server=await createServer({...testViteCache(import.meta.url),configFile:false,logLevel:'warn',root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
await server.listen();await mkdir('.cache',{recursive:true});
const url=`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`;
let browser;
try{
 browser=await webkit.launch({headless:true});
 const context=await browser.newContext({viewport:{width:1440,height:900}});
 // Each workspace's saved page, under the key the app reads (the browser build's scope: origin and token).
 await context.addInitScript(({saved,token})=>{
  if(sessionStorage.getItem('seeded'))return;sessionStorage.setItem('seeded','1');
  void crypto.subtle.digest('SHA-256',new TextEncoder().encode(location.origin+'\0'+token)).then(digest=>{
   const scope=Array.from(new Uint8Array(digest),b=>b.toString(16).padStart(2,'0')).join('');
   for(const [id,layout] of Object.entries(saved))localStorage.setItem(`${scope}:workspace:${id}:pages`,JSON.stringify(layout));
  });
 },{saved,token:fixture.token});
 const p=await context.newPage();p.setDefaultTimeout(15000);
 // A reload cuts in-flight polls, which WebKit reports as page errors ("access control checks") on the page being left: not failures.
 let navigating=false;
 const errors=[];p.on('pageerror',e=>{if(!navigating)errors.push(String(e));});
 const reload=async()=>{navigating=true;await p.reload();await expect(sidebarGroup).toBeVisible();navigating=false;};
 await p.goto(url);
 const sidebarGroup=p.locator('.dv-groupview:has(.dv-tab [data-kind="collections"])');
 await expect(sidebarGroup).toBeVisible();
 const width=()=>sidebarGroup.evaluate(n=>n.getBoundingClientRect().width);
 const select=async id=>{await p.locator('.page-layout .panel-collections .workspace-row').filter({hasText:id==='alpha'?'Alpha feature':id==='beta'?'Beta release':names[id]}).locator('.workspace-link').first().click();await expect(p.getByRole('combobox',{name:'Workspace',exact:true})).toHaveValue(id);};
 // The person's width: three steps wider than the default, from the keyboard on the sidebar's edge.
 const edge=p.getByRole('separator',{name:'Resize Workspaces sidebar'});
 await edge.focus();for(let i=0;i<3;i++)await edge.press('ArrowRight');
 await expect.poll(width).toBe(328);
 let expected=await width();
 const check=async(where)=>{
  // Every frame for a moment: not only where it settles, but never a flicker on the way.
  const seen=await p.evaluate(async()=>{const out=new Set();const group=()=>document.querySelector('.dv-groupview:has(.dv-tab [data-kind="collections"])');for(let i=0;i<24;i++){await new Promise(r=>requestAnimationFrame(r));const g=group();if(g&&g.getBoundingClientRect().width)out.add(g.getBoundingClientRect().width);}return [...out];});
  assert.deepEqual(seen,[expected],`${where}: the sidebar stays ${expected}px (saw ${seen.join(', ')})`);
  // And it runs to the bottom of the window: the page is not left short. (Agents stacks under Workspaces: the column is the sidebar.)
  const bottom=await p.evaluate(()=>[Math.round(Math.max(...[...document.querySelectorAll('.dv-groupview:has(.dv-tab [data-kind="collections"]),.dv-groupview:has(.dv-tab [data-kind="agents"])')].map(n=>n.getBoundingClientRect().bottom))),innerHeight]);
  assert.ok(Math.abs(bottom[0]-bottom[1])<=1,`${where}: the sidebar reaches the bottom of the window (${bottom.join(' vs ')})`);
 };
 const visits=['beta','gamma','delta','epsilon','zeta','eta','alpha','gamma','eta','delta','beta','zeta','alpha'];
 for(const id of visits){await select(id).catch(async e=>{await p.screenshot({path:'.cache/sidebar-failure.png'});throw new Error(`selecting ${id}: ${e.message.split('\n')[0]}`);});await check(`switching to ${id}`);}
 // A page with no content panel shows an empty content area beside the sidebar, not a sidebar across the page.
 await select('gamma');
 await expect(p.locator('.page-layout').getByRole('heading',{name:'No panels on this page'})).toBeVisible();
 await p.screenshot({path:'.cache/sidebar-empty-page.png'});
 // A maximized group hides the sidebar; switching workspace from the keyboard (⌘2) brings it back at its width.
 await select('alpha');
 // Double-click maximizes a kept tab; a preview tab (with conversations as tabs turned off) is kept open first.
 const chatTab=p.locator('.dv-tab.dv-active-tab').filter({has:p.locator('[data-kind="chat"]')}).first();
 if(await p.locator('.dock-tab[data-kind="chat"][data-preview]').count()){await chatTab.dblclick();await expect(p.locator('.dock-tab[data-kind="chat"][data-preview]')).toHaveCount(0);}
 await chatTab.dblclick();
 await expect(sidebarGroup).toBeHidden();
 await p.locator('.page-layout .panel-chat').getByRole('textbox',{name:'Message',exact:true}).click();
 await p.keyboard.press('ControlOrMeta+2');await expect(p.getByRole('combobox',{name:'Workspace',exact:true})).toHaveValue('beta');
 await check('after a maximized group');
 await p.keyboard.press('ControlOrMeta+1');await expect(p.getByRole('combobox',{name:'Workspace',exact:true})).toHaveValue('alpha');
 await check('back to the workspace that had a maximized group');
 // Window resizes, on pages of every kind.
 for(const [size,id] of [[{width:1100,height:800},'gamma'],[{width:1600,height:950},'eta'],[{width:900,height:700},'delta'],[{width:1440,height:900},'zeta']]){
  await p.setViewportSize(size);await check(`resized to ${size.width} on ${(await p.getByRole('combobox',{name:'Workspace',exact:true}).inputValue())}`);
  await select(id);await check(`resized to ${size.width}, then ${id}`);
 }
 // A drag sets a new width, which every workspace then has.
 const bar=await p.getByRole('separator',{name:'Resize Workspaces sidebar'}).boundingBox();
 await p.mouse.move(bar.x+bar.width/2,bar.y+bar.height/2);await p.mouse.down();await p.mouse.move(bar.x+bar.width/2+40,bar.y+bar.height/2,{steps:6});await p.mouse.up();
 await expect.poll(width).toBeGreaterThan(expected+20);
 await p.waitForTimeout(600);
 expected=await width();
 for(const id of ['gamma','delta','epsilon','zeta','eta','alpha','beta']){await select(id);await check(`after a drag, ${id}`);}
 // Agents is the window's, not a page's: closed, it stays closed in every workspace, on every page and after a reload.
 const agentsPanel=p.locator('.page-layout .panel-agents'),agentsTab=()=>p.locator('.dv-tab').filter({has:p.locator('[data-kind="agents"]')});
 const pages=p.getByRole('navigation',{name:'Pages'});
 const showPage=async name=>{await pages.getByRole('button',{name,exact:true}).click();await expect(pages.getByRole('button',{name,exact:true})).toHaveClass(/selected/);};
 await select('alpha');await expect(agentsPanel).toBeVisible();
 await agentsTab().locator('.dock-tab-close').click();
 const closed=async where=>{await expect(agentsPanel,where).toHaveCount(0);await check(where);};
 await closed('Agents closed');
 for(const id of ['beta','eta','theta','gamma','delta','alpha'])await select(id).then(()=>closed(`Agents closed, then ${id}`));
 await select('theta');await showPage('More');await closed('Agents closed, another page');
 await showPage('Work');await closed('Agents closed, back to the first page');
 await reload();await closed('Agents closed, after a reload');
 await select('beta');await closed('Agents closed, after a reload, another workspace');
 await p.screenshot({path:'.cache/sidebar-agents-closed.png'});
 // Panels → Agents brings it back under Workspaces, everywhere; the sidebar keeps its width.
 const shown=async where=>{
  await expect(agentsPanel,where).toBeVisible();await check(where);
  const [list,agents]=await Promise.all(['collections','agents'].map(kind=>p.locator(`.dv-groupview:has(.dv-tab [data-kind="${kind}"])`).boundingBox()));
  assert.ok(Math.abs(list.x-agents.x)<=1&&Math.abs(list.width-agents.width)<=1&&agents.y>=list.y+list.height-1,`${where}: Agents sits under Workspaces, as wide`);
 };
 await p.locator('summary[aria-label="Panels"]').click();await p.getByRole('button',{name:'Show Agents'}).click();
 await shown('Agents brought back');
 for(const id of ['alpha','eta','theta'])await select(id).then(()=>shown(`Agents brought back, then ${id}`));
 await showPage('More');await shown('Agents brought back, another page');await showPage('Work');
 // Folded, it stays folded the same way; unfolded from its tab, it is unfolded everywhere.
 const folded=p.locator('.dock-group-minimized:has(.dv-tab [data-kind="agents"])');
 await agentsTab().click({button:'right'});await p.getByRole('menuitem',{name:'Minimize',exact:true}).click();
 await expect(folded).toHaveCount(1);await check('Agents folded');
 for(const id of ['beta','eta','theta'])await select(id).then(async()=>{await expect(folded,`folded, then ${id}`).toHaveCount(1);await check(`Agents folded, then ${id}`);});
 await showPage('More');await expect(folded).toHaveCount(1);await showPage('Work');
 await reload();await expect(folded).toHaveCount(1);await check('Agents folded, after a reload');
 await agentsTab().click();await expect(folded).toHaveCount(0);
 await select('alpha');await shown('Agents unfolded, then alpha');await expect(folded).toHaveCount(0);
 assert.deepEqual(errors,[]);
 console.log(`PASS sidebar stability: ${visits.length+16} workspace switches across seven pages (default, empty, floating only, popped out, chat window gone, sidebar on the right, maximized), four window sizes and a drag; the sidebar kept the person's width to the pixel on every frame. Agents closed stayed closed through 8 workspace switches, two page switches and a reload; Panels → Agents brought it back everywhere; folded stayed folded the same way. WebKit, fake Runner.`);
}finally{await browser?.close();await server.close();await fixture.close();}
