// Conversation tabs (docs/guides/layouts.md, "Conversation tabs").
//
// With "Open a workspace's conversations as tabs" on (the default): a page an
// earlier build saved (its conversation lost, its review alone in the preview
// tab) comes back with every open conversation as a kept tab, capped; a click
// focuses a conversation's tab; a tab closed stays closed across a reload
// until opened again; a tab dropped on the Workspaces list stays out of it; ⌘W
// in the sidebar closes the conversation in front, not the sidebar; deleting a
// conversation closes its tab; ⌘-clicking another workspace's conversation
// goes to that workspace; a page saved before tabs (a chat following the page)
// has its conversation as a kept tab; turned off, a click shows the preview tab.
//
// With it off: a single click shows a conversation in the chat group's preview
// tab (italic), which the next single click replaces; a double-click keeps it
// in a tab; ⌘-click opens a tab behind the one in front; typing in a preview
// tab keeps it; a row's Open beside splits right; a tab dragged to an edge
// splits; ⌘W closes a tab (the conversation keeps going) and ⇧⌘T reopens it
// where it was; a reload keeps every tab, preview or not; an orchestrator's
// agent opens as a tab next to its parent, behind it, and a preview parent is
// kept first. WebKit (the desktop app's engine), the browser build, against
// the fake Runner fixture.
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from 'vite';
import react from '@vitejs/plugin-react';
import {webkit,expect} from './fixtures/playwright.mjs';
import {fileURLToPath} from 'node:url';
import {mkdir} from 'node:fs/promises';
import {createHash} from 'node:crypto';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import {monacoVimCompat} from './monaco-vim-compat.mjs';

const fixture=await startRunnerFixture();
// Five conversations in Alpha, so the sidebar lists them under the workspace, and the review of the first.
const first=fixture.sessions.find(s=>s.id==='alpha-session');
fixture.sessions.find(s=>s.id==='beta-session').title='Prepare Beta release';
first.title='Main alpha conversation';
for(const [id,title] of [['alpha-second','Second alpha conversation'],['alpha-third','Third alpha conversation'],['alpha-fourth','Fourth alpha conversation'],['alpha-fifth','Fifth alpha conversation'],['alpha-review','Review of the main conversation']])
 fixture.sessions.push({...structuredClone(first),id,workId:id,title,instructionsPending:false});
const recipient=fixture.sessions.find(s=>s.id==='alpha-second');
Object.assign(recipient,{profileId:'codex-default',busy:false,status:'done',agents:[]});
fixture.sessions.find(s=>s.id==='alpha-third').profileId='unknown-profile';
const sourceEvents=fixture.openEvents(first.id);
sourceEvents.push({type:'message_delta',text:'Retained conversation for alpha-session'});
sourceEvents.push({type:'operation_accepted',text:'How long does an entry live?'});
sourceEvents.push({type:'message_delta',text:'A cache entry expires after ten minutes.'});
fixture.openEvents(recipient.id).push({type:'message_delta',text:'Retained conversation for alpha-second'});
fixture.runner.journalMessages=true;
const server=await createServer({...testViteCache(import.meta.url),configFile:false,root:fileURLToPath(new URL('../apps/web',import.meta.url)),logLevel:'warn',plugins:[react(),monacoVimCompat()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
await server.listen();
await mkdir('.cache',{recursive:true});
const origin=`http://127.0.0.1:${server.httpServer.address().port}`;

/**
 * Page errors, but not the requests a reload cuts in flight: WebKit reports
 * those on the page being left as "access control checks". Anything else fails.
 */
function watchErrors(page){
 const state={reloading:false,errors:[]};
 page.on('pageerror',error=>{const text=String(error);if(state.reloading&&/access control checks/.test(text))return;state.errors.push(text);});
 state.reload=async()=>{
  state.reloading=true;
  try{await page.reload();await page.locator('.page-layout .panel-chat').first().waitFor({state:'attached'});await page.waitForTimeout(400);}
  finally{state.reloading=false;}
 };
 return state;
}
/** What every scenario reads of the chat tabs. */
function tabsOf(page){
 const chatTabs=page.locator('.dock-tab[data-kind="chat"]');
 const tab=title=>chatTabs.filter({hasText:title});
 const groupOf=title=>page.locator('.dv-groupview').filter({has:tab(title)});
 return {
  chatTabs,tab,groupOf,
  /** The chat tabs' titles, in reading order (every group). */
  titles:()=>chatTabs.locator('.dock-tab-title').allTextContents(),
  previews:()=>page.locator('.dock-tab[data-kind="chat"][data-preview] .dock-tab-title').allTextContents(),
  row:title=>page.locator('.page-layout .panel-collections .conversation-link').filter({hasText:title}),
  /** Whether a tab is the one in front of its group. */
  inFront:title=>groupOf(title).locator('.dv-tab.dv-active-tab .dock-tab-title').textContent().then(text=>text===title),
  fontStyle:title=>tab(title).locator('.dock-tab-title').evaluate(node=>getComputedStyle(node).fontStyle),
  workspace:name=>page.locator('.page-layout .panel-collections .workspace-row').filter({hasText:name}).locator('.workspace-link').first(),
 };
}

let browser;
try{
 browser=await webkit.launch({headless:true});
 await tabsOn();
 await previewTabs();
 await sourceAfterResume();
 console.log('PASS conversation tabs: with tabs on, a page an earlier build left with only its review in the preview tab comes back with every open conversation as a kept tab (capped), a click focuses its tab, a closed tab stays closed across a reload, a tab dropped on the sidebar stays out of it, ⌘W never closes the sidebar, a deleted conversation’s tab closes, another workspace’s conversation opens on its own page, a page from before tabs keeps its conversation as a kept tab, and the setting turns preview tabs back on; with it off, the preview tab (italic) is replaced by the next click, double-click and typing keep it, ⌘-click opens behind, one tab per conversation, the sidebar follows the tab in front, drag to an edge splits, Open beside splits right, tab menus, ⌘W and ⇧⌘T, ⌃⇥ walks tabs, a reload keeps the tabs, an orchestrator’s agent opens next to its parent and a preview parent is kept. Sidebar and tab icons agree, and a workspace handoff preserves the receiving draft and links to its source. WebKit, fake Runner fixture.');
}finally{
 await browser?.close();
 await server.close();
 await fixture.close?.();
}

/** A sent handoff still opens its source after that conversation resumes in a new execution. */
async function sourceAfterResume(){
 const id='alpha-source-successor';
 Object.assign(first,{closed:true,status:'stopped',successorExecutionId:id});
 fixture.sessions.push({...first,id,title:'Resumed source conversation',closed:false,status:'done',busy:false,previousExecutionId:first.id,successorExecutionId:''});
 const continued=fixture.openEvents(id);
 for(const record of sourceEvents.events) continued.push(record.event);
 const context=await browser.newContext({viewport:{width:1600,height:900}});
 try{
  const page=await context.newPage();
  await page.goto(origin+'/?view=workspace&collection=alpha&session='+recipient.id+'#token='+fixture.token);
  await page.getByRole('link',{name:'source message',exact:true}).click();
  await expect.poll(()=>tabsOf(page).inFront('Resumed source conversation')).toBe(true);
  await expect(page.locator('article.message').filter({hasText:'A cache entry expires after ten minutes.'})).toHaveClass(/message-flash/);
 }finally{await context.close();}
}

/** "Open a workspace's conversations as tabs", on as it is by default, on pages saved by earlier builds. */
async function tabsOn(){
 const scope=createHash('sha256').update(origin+'\0'+fixture.token).digest('hex');
 const panel=(id,kind,extra={})=>({id,kind,binding:{mode:'follow'},detached:false,...extra});
 const group=(id,panels)=>({type:'group',id,panels,active:panels[0].id});
 const sidebar=w=>({type:'split',direction:'column',sizes:[0.55,0.45],children:[group(`g-${w}-col`,[panel(`${w}-col`,'collections')]),group(`g-${w}-agents`,[panel(`${w}-agents`,'agents')])]});
 const work=(w,sessionId,chat)=>({version:2,selectedPage:'work',pages:[{id:'work',name:'Work',selection:{collectionId:w,sessionId,commandId:''},layout:{type:'split',direction:'row',sizes:[0.2,0.8],children:[sidebar(w),group(`g-${w}-chat`,[chat])]},floating:[],popouts:[],detached:[]}]});
 // Alpha as the first tabs build left it (the user's screenshot): the main conversation lost, its review alone in the preview tab.
 const alpha=work('alpha','alpha-review',{id:'alpha-review-tab',kind:'chat',binding:{mode:'pinned',target:{collectionId:'alpha',sessionId:'alpha-review',commandId:''}},detached:false,preview:true});
 // Beta as builds before tabs saved it: one chat following the page.
 const beta=work('beta','beta-session',panel('beta-chat','chat'));
 const context=await browser.newContext({viewport:{width:1600,height:900}});
 await context.addInitScript(([key,a,b])=>{if(sessionStorage.getItem('seeded'))return;sessionStorage.setItem('seeded','1');localStorage.setItem(`${key}:workspace:alpha:pages`,a);localStorage.setItem(`${key}:workspace:beta:pages`,b);},[scope,JSON.stringify(alpha),JSON.stringify(beta)]);
 const page=await context.newPage();page.setDefaultTimeout(15000);
 const watched=watchErrors(page);
 const {chatTabs,tab,titles,previews,row,inFront,workspace}=tabsOf(page);
 const all=['Review of the main conversation','Main alpha conversation','Second alpha conversation','Third alpha conversation','Fourth alpha conversation','Fifth alpha conversation'];
 await page.goto(`${origin}/?view=workspace&collection=alpha#token=${fixture.token}`);

 // Recovery: the review stays (kept now) and every open conversation is a tab again, the main one too.
 await expect.poll(async()=>(await titles()).sort()).toEqual([...all].sort());
 await expect.poll(previews).toEqual([]);
 assert.equal((await titles())[0],'Review of the main conversation','the tab that was there stays first');
 await expect.poll(()=>inFront('Review of the main conversation')).toBe(true);
 await page.screenshot({path:'.cache/conversation-tabs-recovered.png'});
 // A click focuses the conversation's tab: nothing is replaced, nothing is added.
 await row('Main alpha conversation').click();
 await expect.poll(()=>inFront('Main alpha conversation')).toBe(true);
 await expect(chatTabs).toHaveCount(6);
 await expect.poll(previews).toEqual([]);
 // Agent identity matches in both places, including sessions with only a profile or an unknown provider.
 for(const [title,agent] of [['Main alpha conversation','Claude'],['Second alpha conversation','Codex'],['Third alpha conversation','Unknown agent']]){
  const rowIcon=row(title).getByRole('img',{name:agent,exact:true});
  const tabIcon=tab(title).getByRole('img',{name:agent,exact:true});
  await expect(rowIcon).toBeVisible();
  await expect(tabIcon).toBeVisible();
  assert.deepEqual(await rowIcon.locator('path').evaluateAll(paths=>paths.map(p=>p.getAttribute('d'))),
    await tabIcon.locator('path').evaluateAll(paths=>paths.map(p=>p.getAttribute('d'))));
  await expect(tab(title).locator('.dock-tab-status')).toBeVisible();
 }
 // Put selected text into another chat, after its draft. Only this workspace is offered.
 const composer=page.locator('textarea[data-agent-composer]:visible');
 await row('Second alpha conversation').click();
 await composer.fill('Already drafted.');
 await row('Main alpha conversation').click();
 const source=page.locator('article.message').filter({hasText:'A cache entry expires after ten minutes.'});
 const words=source.getByText('A cache entry expires after ten minutes.',{exact:true});
 await words.scrollIntoViewIfNeeded();
 const selectedPoint=await words.evaluate(node=>{
  const range=document.createRange();
  range.setStart(node.firstChild,2);range.setEnd(node.firstChild,13);
  const selection=window.getSelection();selection.removeAllRanges();selection.addRange(range);
  // Some browsers replace the selection after pointerdown and before contextmenu.
  node.addEventListener('mousedown',()=>selection.removeAllRanges(),{once:true});
  const rect=range.getBoundingClientRect();
  return {x:rect.left+rect.width/2,y:rect.top+rect.height/2};
 });
 await page.mouse.click(selectedPoint.x,selectedPoint.y,{button:'right'});
 await page.getByRole('menuitem',{name:'Send to conversation…'}).click();
 const handoff=page.getByRole('dialog',{name:'Send to conversation'});
 await expect(handoff.getByRole('option',{name:'Second alpha conversation · Codex'})).toBeAttached();
 await expect(handoff.getByRole('option',{name:/Main alpha|beta/i})).toHaveCount(0);
 await handoff.getByLabel('Conversation',{exact:true}).selectOption('alpha-second');
 await handoff.getByLabel('Note (optional)').fill('Please check the TTL.');
 const sentBefore=fixture.calls.filter(call=>call.body?.message).length;
 await handoff.getByRole('button',{name:'Add to draft'}).click();
 await expect.poll(()=>inFront('Second alpha conversation')).toBe(true);
 await expect(composer).toHaveValue(/^Already drafted\.\n\nPlease check the TTL\.\n\nFrom “Main alpha conversation” .*\n\n> cache entry$/);
 assert.equal(fixture.calls.filter(call=>call.body?.message).length,sentBefore);
 await composer.press('Enter');
 await page.getByRole('link',{name:'source message',exact:true}).click();
 await expect.poll(()=>inFront('Main alpha conversation')).toBe(true);
 await expect(source).toHaveClass(/message-flash/);
 fixture.runner.journalMessages=false;
 // A tab closed stays closed when the workspace loads again; opening the conversation brings it back.
 await tab('Fifth alpha conversation').locator('.dock-tab-close').click();
 await expect(chatTabs).toHaveCount(5);
 await watched.reload();
 await expect.poll(async()=>(await titles()).sort()).toEqual(all.filter(title=>title!=='Fifth alpha conversation').sort());
 await row('Fifth alpha conversation').click();
 await expect(chatTabs).toHaveCount(6);
 await expect.poll(previews).toEqual([]);
 // A tab dragged onto the Workspaces list does not join the sidebar: nothing moves.
 const sidebarGroup=page.locator('.dv-groupview').filter({has:page.locator('.dock-tab[data-kind="collections"]')});
 const listBox=await sidebarGroup.boundingBox(),from=await tab('Second alpha conversation').boundingBox();
 await page.mouse.move(from.x+from.width/2,from.y+from.height/2);await page.mouse.down();
 await page.mouse.move(from.x+from.width/2+14,from.y+from.height/2+14,{steps:4});
 await page.mouse.move(listBox.x+listBox.width/2,listBox.y+listBox.height/2,{steps:12});await page.waitForTimeout(150);await page.mouse.up();
 await page.waitForTimeout(300);
 await expect(sidebarGroup.locator('.dock-tab')).toHaveCount(1);
 await expect(chatTabs).toHaveCount(6);
 // ⌘W with the Workspaces list focused closes the conversation in front, never the sidebar.
 await page.locator('.panel-collections .explorer-heading').first().click();
 await page.keyboard.press('Meta+w');
 await expect(chatTabs).toHaveCount(5);
 await expect(page.locator('.dock-tab[data-kind="collections"]')).toHaveCount(1);
 assert.ok((await sidebarGroup.boundingBox()).x<40,'the sidebar stays on the left');
 await page.keyboard.press('Meta+Shift+T');
 await expect(chatTabs).toHaveCount(6);
 // A conversation deleted leaves its tab.
 await row('Review of the main conversation').click({button:'right'});
 await page.getByRole('menuitem',{name:/^Delete…/}).click();
 const dialog=page.getByRole('dialog',{name:'Delete conversation'});
 const stop=dialog.getByRole('checkbox');
 await expect(dialog.getByRole('button',{name:/Delete conversation|Stop and delete/})).toBeVisible();
 if(await stop.count())await stop.check();
 await dialog.getByRole('button',{name:/Delete conversation|Stop and delete/}).click();
 await expect(dialog).toHaveCount(0);
 await expect(tab('Review of the main conversation')).toHaveCount(0);
 await expect(chatTabs).toHaveCount(5);
 // Another workspace's conversation, ⌘-clicked in Agents: the window goes to that workspace; this one's page does not get it.
 const pagesOf=w=>page.evaluate(key=>localStorage.getItem(key)||'',`${scope}:workspace:${w}:pages`);
 await page.getByRole('textbox',{name:/Filter agents/}).or(page.getByPlaceholder('Filter agents')).first().fill('Beta');
 await page.locator('.agent-row').filter({hasText:'Prepare Beta release'}).click({modifiers:['Meta']});
 await expect(page.getByRole('combobox',{name:'Workspace',exact:true})).toHaveValue('beta');
 assert.ok(!(await pagesOf('alpha')).includes('beta-session'),'alpha’s pages hold no beta conversation');
 // A page saved before tabs (a chat following the page): its conversation is a kept tab.
 await expect(chatTabs).toHaveCount(1);
 await expect.poll(previews).toEqual([]);
 // Turned off (Settings → Chat): a click on a conversation without a tab shows it in the preview tab.
 await page.getByRole('button',{name:'Settings',exact:true}).click();
 await page.getByRole('tab',{name:'Chat'}).click();
 const setting=page.getByRole('switch',{name:'Open a workspace’s conversations as tabs'});
 await expect(setting).toBeChecked();
 await setting.uncheck();
 await page.getByRole('button',{name:'Work',exact:true}).click();
 await workspace('Alpha feature').click();
 await expect(page.getByRole('combobox',{name:'Workspace',exact:true})).toHaveValue('alpha');
 await tab('Third alpha conversation').locator('.dock-tab-close').click();
 await row('Third alpha conversation').click();
 await expect.poll(previews).toEqual(['Third alpha conversation']);
 assert.deepEqual(watched.errors,[]);
 await context.close();
}

/** With the setting off: the preview tab, kept tabs, and everything a tab does. */
async function previewTabs(){
 const context=await browser.newContext({viewport:{width:1600,height:900},colorScheme:'dark'});
 await context.addInitScript(()=>{try{localStorage.setItem('lazybox:workspace.conversation-tabs','off');}catch{/* a popup without storage */}});
 const page=await context.newPage();page.setDefaultTimeout(15000);
 const watched=watchErrors(page);
 const shot=name=>page.screenshot({path:`.cache/conversation-tabs-${name}.png`});
 await page.goto(`${origin}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`);
 const {chatTabs,tab,groupOf,titles,previews,row,inFront,fontStyle}=tabsOf(page);
 const shown=page.locator('.page-layout .panel-chat').filter({visible:true});
 const key=async combo=>{await page.keyboard.press(combo);await page.waitForTimeout(60);};

 await expect(shown.getByRole('textbox',{name:'Message',exact:true})).toBeEnabled();
 // The page's conversation is its preview tab; nothing else is opened by itself.
 await expect.poll(titles).toEqual(['Main alpha conversation']);
 await expect.poll(previews).toEqual(['Main alpha conversation']);

 // ---- A single click shows the preview tab (italic); the next single click replaces it.
 await row('Second alpha conversation').click();
 await expect.poll(titles).toEqual(['Second alpha conversation']);
 await expect.poll(previews).toEqual(['Second alpha conversation']);
 assert.equal(await fontStyle('Second alpha conversation'),'italic','a preview tab reads in italics');
 await expect(shown).toContainText('Retained conversation for alpha-second');
 await row('Third alpha conversation').click();
 await expect.poll(titles,{message:'the preview tab is replaced, not stacked'}).toEqual(['Third alpha conversation']);
 await shot('preview');

 // ---- A double-click keeps it: a regular tab, upright; the next click opens a preview beside it.
 await row('Third alpha conversation').dblclick();
 await expect.poll(previews).toEqual([]);
 assert.equal(await fontStyle('Third alpha conversation'),'normal');
 await row('Second alpha conversation').click();
 await expect.poll(titles).toEqual(['Third alpha conversation','Second alpha conversation']);
 await expect.poll(previews).toEqual(['Second alpha conversation']);
 await expect.poll(()=>inFront('Second alpha conversation')).toBe(true);

 // ---- ⌘-click opens a tab behind the one in front, right after it.
 await row('Main alpha conversation').click({modifiers:['Meta']});
 await expect.poll(titles).toEqual(['Third alpha conversation','Second alpha conversation','Main alpha conversation']);
 await expect.poll(()=>inFront('Second alpha conversation'),{message:'the tab in front stays in front'}).toBe(true);
 await expect(shown).toContainText('Retained conversation for alpha-second');
 assert.equal(await fontStyle('Main alpha conversation'),'normal','a tab opened behind is a regular tab');
 // Typing in a preview tab keeps it.
 await shown.getByRole('textbox',{name:'Message',exact:true}).fill('a draft keeps this tab');
 await expect.poll(previews).toEqual([]);
 await expect(shown.getByRole('textbox',{name:'Message',exact:true}),'the draft stays in the tab it kept').toHaveValue('a draft keeps this tab');
 // A conversation already in a tab is brought forward, never opened twice.
 await row('Third alpha conversation').click();
 await expect.poll(titles).toEqual(['Third alpha conversation','Second alpha conversation','Main alpha conversation']);
 await expect.poll(()=>inFront('Third alpha conversation')).toBe(true);
 await expect(row('Third alpha conversation')).toHaveAttribute('aria-current','page');
 // Bringing a tab forward selects its conversation: the sidebar follows.
 await tab('Second alpha conversation').click();
 await expect(shown.getByRole('textbox',{name:'Message',exact:true})).toHaveValue('a draft keeps this tab');
 await expect(row('Second alpha conversation')).toHaveAttribute('aria-current','page');
 await shot('tabs');

 // ---- Drag a tab to the right edge of the chat group: it splits out to the right.
 const drag=async(from,to)=>{
  const a=await from.boundingBox();
  await page.mouse.move(a.x+a.width/2,a.y+a.height/2);await page.mouse.down();
  await page.mouse.move(a.x+a.width/2+14,a.y+a.height/2+14,{steps:4});
  await page.mouse.move(to.x,to.y,{steps:12});
  await page.waitForTimeout(120);
  await page.mouse.up();
 };
 const chatGroup=await groupOf('Second alpha conversation').boundingBox();
 await drag(tab('Main alpha conversation'),{x:chatGroup.x+chatGroup.width-24,y:chatGroup.y+chatGroup.height/2});
 await expect.poll(async()=>{const a=await groupOf('Second alpha conversation').boundingBox(),b=await groupOf('Main alpha conversation').boundingBox();return !!a&&!!b&&b.x>=a.x+a.width-4;},{message:'the dragged tab is in a group of its own, on the right'}).toBe(true);
 await expect.poll(titles).toEqual(['Third alpha conversation','Second alpha conversation','Main alpha conversation']);
 await expect(page.locator('.dv-groupview').filter({has:chatTabs})).toHaveCount(2);

 // ---- The row's menu: Open beside splits right of the conversation on screen.
 await tab('Second alpha conversation').click();
 await row('Fourth alpha conversation').click({button:'right'});
 const menu=page.getByRole('menu');
 for(const label of ['Open in new tab','Open beside','Open below','Open in new window','Move to new page'])await expect(menu.getByRole('menuitem',{name:new RegExp(`^${label}`)})).toBeVisible();
 await menu.getByRole('menuitem',{name:/^Open beside/}).click();
 await expect.poll(async()=>{const a=await groupOf('Second alpha conversation').boundingBox(),b=await groupOf('Fourth alpha conversation').boundingBox();return !!a&&!!b&&b.x>=a.x+a.width-4&&Math.abs(b.y-a.y)<4;},{message:'Open beside: right of the conversation on screen'}).toBe(true);
 await expect(page.locator('.dv-groupview').filter({has:chatTabs})).toHaveCount(3);
 await expect.poll(titles).toEqual(['Third alpha conversation','Second alpha conversation','Fourth alpha conversation','Main alpha conversation']);
 await expect(row('Fourth alpha conversation')).toHaveAttribute('aria-current','page');
 // A tab's own menu offers the tab commands.
 await tab('Third alpha conversation').click({button:'right'});
 for(const label of ['Split right','Split down','Open in new window','Move to new page','Close other tabs','Close tabs to the right'])await expect(page.getByRole('menuitem',{name:new RegExp(`^${label}`)})).toBeVisible();
 await key('Escape');
 await shot('split');

 // ---- ⌘W closes the tab (the conversation keeps going); ⇧⌘T brings it back where it was.
 const mutations=fixture.mutations.length;
 await tab('Second alpha conversation').click();
 await expect.poll(()=>inFront('Second alpha conversation')).toBe(true);
 await key('Meta+w');
 await expect.poll(titles).toEqual(['Third alpha conversation','Fourth alpha conversation','Main alpha conversation']);
 assert.equal(fixture.mutations.length,mutations,'closing a tab neither stops nor ends its conversation');
 assert.ok(!fixture.sessions.find(s=>s.id==='alpha-second').closed);
 await key('Meta+Shift+T');
 await expect.poll(titles).toEqual(['Third alpha conversation','Second alpha conversation','Fourth alpha conversation','Main alpha conversation']);
 await expect.poll(()=>inFront('Second alpha conversation')).toBe(true);
 await expect(shown.filter({hasText:'Retained conversation for alpha-second'})).toHaveCount(1);
 // ⌃⇥ walks the tabs: Third, then Second, in the same group.
 await tab('Third alpha conversation').click();
 await key('Control+Tab');
 await expect.poll(()=>inFront('Second alpha conversation')).toBe(true);

 // ---- A reload keeps every tab, where it was, regular or preview.
 await row('Fifth alpha conversation').click();
 await expect.poll(titles).toEqual(['Third alpha conversation','Second alpha conversation','Fifth alpha conversation','Fourth alpha conversation','Main alpha conversation']);
 await expect.poll(previews).toEqual(['Fifth alpha conversation']);
 const before={titles:await titles(),previews:await previews()};
 await watched.reload();
 await expect.poll(titles).toEqual(before.titles);
 await expect.poll(previews).toEqual(before.previews);
 await expect(page.locator('.dv-groupview').filter({has:chatTabs})).toHaveCount(3);

 // ---- An orchestrator's agent opens as a tab next to its parent, behind it; a preview parent is kept first.
 const mission=fixture.missions.handle('CreateMission',{title:'Ship search'}).value;
 fixture.missions.handle('UpdateMission',{missionId:mission.id,grant:{sessionId:'alpha-session',ceiling:{maxChildren:3}}});
 fixture.missions.spawnChild('alpha-session',{title:'Index the corpus',role:'worker'});
 await tab('Main alpha conversation').click();
 await tab('Main alpha conversation').click({button:'right'});
 await page.getByRole('menuitem',{name:'Use as preview tab',exact:true}).click();
 await expect.poll(()=>fontStyle('Main alpha conversation')).toBe('italic');
 const parent=page.locator(`[data-dock-panel="${await tab('Main alpha conversation').getAttribute('data-panel-tab')}"]`);
 const chip=parent.locator('.orchestrator-chip');
 await expect(chip).toBeVisible({timeout:20_000});
 await chip.click();
 const child=page.locator('.orchestrator-children li').filter({hasText:'Index the corpus'});
 await expect(child).toBeVisible({timeout:20_000});
 await child.getByRole('button').click();
 await expect.poll(titles).toEqual([...before.titles,'Index the corpus']);
 await expect.poll(()=>inFront('Main alpha conversation'),{message:'the parent stays in front'}).toBe(true);
 assert.equal(await fontStyle('Index the corpus'),'normal');
 assert.equal(await fontStyle('Main alpha conversation'),'normal','the preview parent was kept open');
 await expect(page.getByTestId('dock-announcer')).toContainText('next to');
 await shot('related');
 // Clearing the session picker is selection only. It must not replace a
 // preview or create an empty Chat tab behind one being closed.
 const sessionPicker=page.getByRole('combobox',{name:'Session',exact:true});
 await sessionPicker.selectOption('');
 await expect.poll(titles).toEqual([...before.titles,'Index the corpus']);
 while(await chatTabs.count()) {
   const count=await chatTabs.count();
   await chatTabs.first().locator('.dock-tab-close').click();
   await expect(chatTabs).toHaveCount(count-1);
 }
 await expect(sessionPicker).toHaveValue('');


 assert.deepEqual(watched.errors,[]);
 await context.close();
}
