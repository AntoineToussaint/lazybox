import {openWorkspaceChanges} from './fixtures/workspace-changes.mjs';
// Lazybox context menus against the fake Runner, in WebKit (the desktop
// app's engine; TEST_CHROMIUM=1 for Chromium): every wired place shows the
// Lazybox menu and never the browser's (the event's default is prevented),
// one action per place works, the keyboard (Shift+F10, arrows, type-ahead,
// Escape) and a long press open it, a press outside closes it, it stays on
// screen at the edges, text fields keep the native menu and Monaco its own.
// A right-click on a link never opens it: not a slow one, not at a window
// edge, not Ctrl+click on a Mac; press, drag onto an item and release does.
import {createServer} from 'vite';
import react from '@vitejs/plugin-react';
import {webkit,chromium,expect} from './fixtures/playwright.mjs';
import {fileURLToPath} from 'node:url';
import {mkdir} from 'node:fs/promises';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import {testViteCache} from './fixtures/vite-cache.mjs';
import {monacoVimCompat} from './monaco-vim-compat.mjs';
import { collectPageErrors } from './fixtures/page-errors.mjs';

const fixture=await startRunnerFixture();
const AGENT='The parser fails on `api/src/main.go`, see [the routes](api/src/routes.ts).\n\n```ts\nconst answer = 42;\n```\n\nDocs: [example](https://example.com/guide).';
const alpha=fixture.openEvents('alpha-session');
alpha.push({type:'operation_accepted',operationId:'op-1',text:'Why does the parser fail?'});
alpha.push({type:'message_delta',text:AGENT});alpha.push({type:'turn_ended'});
fixture.setCheckpoints('alpha-session',{turns:[{operationId:'op-1',prompt:'Why does the parser fail?',files:[{member:'api',path:'parser.go',status:'M',additions:3,deletions:1}]}]});
// A command still running in the conversation, for the console's menu.
fixture.commands.push({id:'cmd-running',sessionId:'alpha-session',title:'npm test',mode:'pipe',done:false,offset:'24',argv:['/bin/sh','-c','npm test -- --watch=false'],output:'PASS src/app.test.ts\nReport: https://example.com/report\n'});
const api=fixture.shipState('alpha','api');
api.files=[{path:'src/handler.go',indexStatus:' ',worktreeStatus:'M'}];
fixture.diffs.set('alpha/api/src/handler.go','diff --git a/src/handler.go b/src/handler.go\n--- a/src/handler.go\n+++ b/src/handler.go\n@@ -1,3 +1,3 @@\n package api\n-func Handle() {\n+func Handle() error {\n }\n');
const server=await createServer({...testViteCache(import.meta.url),configFile:false,logLevel:'error',root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react(),monacoVimCompat()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
let browser;
try{
 await server.listen();await mkdir('.cache',{recursive:true});
 browser=await(process.env.TEST_CHROMIUM?chromium:webkit).launch({headless:true});
 const context=await browser.newContext({viewport:{width:1500,height:950}});
 // The clipboard is observed, not used; every contextmenu records whether its default was prevented once all listeners ran.
 await context.addInitScript(()=>{
  window.__copied=[];
  const rememberPointer=event=>{window.__pointerPosition={clientX:event.clientX,clientY:event.clientY};};
  window.addEventListener('pointermove',rememberPointer,true);
  window.addEventListener('pointerdown',rememberPointer,true);
  Object.defineProperty(navigator,'clipboard',{configurable:true,value:{writeText:async text=>{window.__copied.push(text);},readText:async()=>'pasted from clipboard',write:async items=>{for(const item of items)for(const type of item.types)await item.getType(type).catch(()=>{});},read:async()=>[]}});
  // Links never leave: what would open in the browser is recorded.
  window.__opened=[];window.open=url=>{window.__opened.push(String(url));return null;};
  window.__menus=[];
  window.addEventListener('contextmenu',event=>{setTimeout(()=>window.__menus.push({prevented:event.defaultPrevented,target:event.target?.className?.toString?.()||''}),0);});
 });
 const page=await context.newPage();page.setDefaultTimeout(15000);
 const newTabs=[];context.on('page',tab=>{if(tab!==page){newTabs.push(tab.url());void tab.close();}});
 // Every way a link could open: window.open, a new tab, a Web panel.
 const nothingOpened=async what=>{await page.waitForTimeout(150);assert.deepEqual({opened:await page.evaluate(()=>window.__opened),tabs:newTabs,web:await page.getByRole('region',{name:'Web page'}).count()},{opened:[],tabs:[],web:0},`${what} opens nothing`);};
 const errors=[];await collectPageErrors(page, errors);
 await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`);
 const chat=page.locator('.panel-chat');
 const composer=chat.getByRole('textbox',{name:'Message',exact:true});
 await expect(composer).toBeVisible();
 const menu=page.locator('.context-menu[data-depth="0"]');
 const item=name=>menu.getByRole('menuitem',{name,exact:true});
 const lastPrevented=async()=>{await page.waitForTimeout(30);return page.evaluate(()=>window.__menus.at(-1)?.prevented);};
 const copied=()=>page.evaluate(()=>window.__copied.at(-1));
 const labels=()=>menu.locator('.context-menu-label').allTextContents();
 const close=async()=>{await page.keyboard.press('Escape');await expect(menu).toHaveCount(0);};
 const open=async(locator,options={})=>{await locator.click({button:'right',...options});await expect(menu).toBeVisible();assert.equal(await lastPrevented(),true,'the browser menu never shows');};

 // A chat message: quote, copy, find the prompt.
 const agentMessage=chat.locator('article.message.message_delta').filter({hasText:'The parser fails'});
 await expect(agentMessage).toBeVisible();
 await open(agentMessage.locator('p').first(),{position:{x:4,y:6}});
 assert.deepEqual((await labels()).slice(0,9),['Quote in reply','Send to conversation…','Copy message','Copy as Markdown','Jump to prompt','Show changes in this turn','Review this turn with Codex','Review this turn with Claude','Revert to before this turn…']);
 assert.ok((await labels()).includes('Track as artifact…'),'the artifacts feature contributes to the message menu');
 await page.screenshot({path:'.cache/context-menu-message.png'});
 await item('Copy as Markdown').click();await expect(menu).toHaveCount(0);
 assert.equal(await copied(),AGENT);
 await open(agentMessage.locator('p').first(),{position:{x:4,y:6}});
 await item('Quote in reply').click();
 await expect(composer).toHaveValue(/^> The parser fails on api\/src\/main\.go/);
 await expect(composer).toBeFocused();
 await composer.fill('');

 // A selection: Copy first, and the quote takes just the selection.
 await agentMessage.locator('p').first().evaluate(node=>{const range=document.createRange();range.setStart(node.firstChild,4);range.setEnd(node.firstChild,10);const s=getSelection();s.removeAllRanges();s.addRange(range);});
 const box=await agentMessage.locator('p').first().evaluate(node=>{const range=document.createRange();range.setStart(node.firstChild,4);range.setEnd(node.firstChild,10);const r=range.getBoundingClientRect();return {x:r.x+r.width/2,y:r.y+r.height/2};});
 await page.mouse.click(box.x,box.y,{button:'right'});await expect(menu).toBeVisible();
 assert.deepEqual((await labels()).slice(0,2),['Copy','Quote selection in reply']);
 await item('Copy').click();assert.equal(await copied(),'parser');
 await page.evaluate(()=>getSelection().removeAllRanges());

 // A code block: its items come first, then the message's.
 await open(agentMessage.locator('.code-block pre'));
 assert.deepEqual((await labels()).slice(0,3),['Copy code','Copy as Markdown','Quote in reply']);
 await item('Copy code').click();assert.equal(await copied(),'const answer = 42;');

 // A command's output: stop it while it runs, copy what ran.
 // Command output is a dock group minimized to its tab until opened: its tab opens it.
{const minimized=page.locator('.dock-group-minimized').filter({has:page.locator('[data-kind="console"]')});if(await minimized.count())await page.getByRole('tab',{name:'Command output',exact:true}).click();}
 const output=page.locator('.command-panel').getByTestId('console-output');
 await expect(output).toContainText('PASS src/app.test.ts');
 await open(output);
 assert.deepEqual((await labels()).slice(0,3),['Stop','Copy output','Copy command']);
 await item('Copy command').click();assert.equal(await copied(),'npm test -- --watch=false');
 // A link in the output routes itself, and still shows the link menu on a right-click, slow or Ctrl+click.
 {const reportLink=output.getByRole('link',{name:'https://example.com/report'});await expect(reportLink).toBeVisible();
  const b=await reportLink.boundingBox();await page.mouse.move(b.x+b.width/2,b.y+b.height/2);await page.mouse.down({button:'right'});await page.waitForTimeout(450);await page.mouse.up({button:'right'});
  await expect(item('Copy link')).toBeVisible();await nothingOpened('a slow right-click on a command output link');await close();
  if(process.platform==='darwin'){await reportLink.click({modifiers:['Control']});await expect(item('Copy link')).toBeVisible();await nothingOpened('Ctrl+click on a command output link');await close();}}

 // A web link.
 const exampleLink=agentMessage.getByRole('link',{name:'example'});
 await open(exampleLink);
 await item('Copy link').click();assert.equal(await copied(),'https://example.com/guide');

 // A slow right-click on a link (a trackpad click held a moment): the menu
 // stays open and nothing opens; the release where it opened chooses nothing.
 const linkItems=['Open in your browser','Open in Lazybox','Open in a new Lazybox panel','Copy link'];
 const slowRightClick=async(x,y,what)=>{
  await page.mouse.move(x,y);await page.mouse.down({button:'right'});await expect(menu).toBeVisible();
  await page.waitForTimeout(450);await page.mouse.up({button:'right'});
  await nothingOpened(what);await expect(menu).toBeVisible();
  for(const name of linkItems)await expect(item(name)).toBeVisible();
  const b=await menu.boundingBox();assert.ok(x<b.x||x>b.x+b.width||y<b.y||y>b.y+b.height,`${what}: the pointer is not on the menu ${JSON.stringify({x,y,b})}`);
 };
 {const b=await exampleLink.boundingBox();await slowRightClick(b.x+b.width/2,b.y+b.height/2,'a slow right-click on a link');await close();}
 // The same at the window's bottom-right corner, where the menu opens up and left.
 const vp=page.viewportSize();
 await page.evaluate(()=>{const a=document.createElement('a');a.href='https://example.com/corner';a.target='_blank';a.textContent='corner';a.id='corner-link';a.style.cssText='position:fixed;right:0;bottom:0;z-index:2147483646;padding:2px 4px;background:Canvas';document.body.append(a);});
 {const b=await page.locator('#corner-link').boundingBox();await slowRightClick(Math.min(b.x+b.width-3,vp.width-3),Math.min(b.y+b.height-3,vp.height-3),'a slow right-click at the corner');
  const m=await menu.boundingBox();assert.ok(m.y+m.height<=b.y+b.height&&m.x+m.width<=b.x+b.width,'the menu opened up and left of the pointer');await close();}
 // Press, drag onto an item and release still chooses it.
 {const b=await exampleLink.boundingBox();await page.mouse.move(b.x+b.width/2,b.y+b.height/2);await page.mouse.down({button:'right'});await expect(menu).toBeVisible();
  const target=await item('Copy link').boundingBox();await page.mouse.move(target.x+target.width/2,target.y+target.height/2,{steps:6});await page.mouse.up({button:'right'});
  await expect(menu).toHaveCount(0);await expect.poll(copied).toBe('https://example.com/guide');await nothingOpened('a drag onto Copy link');}
 // On a Mac, Ctrl+click is the right-click: the menu, never the link (WebKit
 // follows the menu with a click that has ctrlKey). The browser does that on a
 // Mac host only, whatever platform the page reports.
 const mac=process.platform==='darwin';
 if(mac){
  await exampleLink.click({modifiers:['Control']});await expect(menu).toBeVisible();await expect(item('Open in your browser')).toBeVisible();
  await nothingOpened('Ctrl+click on a link');await close();
  await page.locator('#corner-link').click({modifiers:['Control']});await expect(menu).toBeVisible();await nothingOpened('Ctrl+click on a link at the corner');await close();
 }
 await page.evaluate(()=>document.getElementById('corner-link')?.remove());

 // A file the agent named: open it to the side.
 const fileLink=agentMessage.getByRole('button',{name:'the routes',exact:true});
 await open(fileLink);
 assert.deepEqual((await labels()).slice(0,4),['Open','Open to the side','Quick look','Reveal in Files']);
 await item('Copy relative path').click();await expect.poll(copied).toBe('api/src/routes.ts');
 // Hold the file until keyboard focus has moved into a new menu.
 let releaseFile;
 const fileGate=new Promise(resolve=>{releaseFile=resolve;});
 let fileRequested=false;
 await page.route('**/lazybox.bench.v1.RunnerService/ReadWorkspaceFile',async route=>{
  if(route.request().postDataJSON()?.path==='api/src/routes.ts'){fileRequested=true;await fileGate;}
  await route.continue();
 });
 await open(fileLink);await item('Open to the side').click();
 await expect.poll(()=>fileRequested).toBe(true);
 const editorPanel=page.locator('section.workspace-panel.panel-editor');
 await expect(editorPanel.getByRole('tab',{name:/routes\.ts/})).toBeVisible();

 // Keyboard: Shift+F10 on the focused element, arrows, type-ahead, Escape returns focus.
 await fileLink.focus();await page.keyboard.press('Shift+F10');
 await expect(menu).toBeVisible();
 await expect(item('Open')).toBeFocused();
 await item('Open to the side').dispatchEvent('pointermove',{pointerType:'mouse',...await page.evaluate(()=>window.__pointerPosition),movementX:0,movementY:0});
 await expect(item('Open')).toBeFocused();
 releaseFile();
 await expect(editorPanel.locator('.monaco-editor')).toBeVisible();
 await expect(item('Open')).toBeFocused();
 await page.keyboard.press('ArrowDown');await expect(item('Open to the side')).toBeFocused();
 await page.keyboard.press('End');await expect(menu.locator('[role=menuitem]:not([aria-disabled])').last()).toBeFocused();
 await page.keyboard.press('r');await expect(item('Reveal in Files')).toBeFocused();
 await page.keyboard.press('Escape');await expect(menu).toHaveCount(0);await expect(fileLink).toBeFocused();
 // The context-menu key does the same.
 await page.keyboard.press('ContextMenu');await expect(menu).toBeVisible();await close();

 // The chat header: the ⋯ session actions, with permissions as a submenu.
 // (Anywhere on the header that is not a control: in a slim panel the avatar gives its room to the title.)
 await open(chat.locator('.chat-heading .chat-status'));
 const sessionItems=await labels();
 for(const name of ['Permissions','Switch subscription…','Delete conversation…'])assert.ok(sessionItems.includes(name),name);
 await page.keyboard.press('p');await expect(item('Permissions')).toBeFocused();
 await page.keyboard.press('ArrowRight');
 const submenu=page.locator('.context-menu[data-depth="1"]');
 await expect(submenu).toBeVisible();await expect(submenu.locator('[role=menuitemcheckbox]:not([aria-disabled])').first()).toBeFocused();
 {const a=await menu.boundingBox(),b=await submenu.boundingBox();assert.ok(b.x>=a.x+a.width-8||b.x+b.width<=a.x+8,'the submenu opens beside the menu');}
 await page.keyboard.press('ArrowLeft');await expect(submenu).toHaveCount(0);await expect(item('Permissions')).toBeFocused();
 await close();

 // A press outside closes it; so does scrolling.
 await open(agentMessage.locator('p').first(),{position:{x:4,y:6}});
 await page.mouse.click(5,500);await expect(menu).toHaveCount(0);
 await open(agentMessage.locator('p').first(),{position:{x:4,y:6}});
 await page.mouse.wheel(0,40);await expect(menu).toHaveCount(0);
 // One menu at a time: a second right-click replaces the first.
 await open(agentMessage.locator('p').first(),{position:{x:4,y:6}});await open(fileLink);await expect(page.locator('.context-menu[data-depth="0"]')).toHaveCount(1);await close();

 // At the edges it stays on screen.
 const viewport=page.viewportSize();
 for(const at of [{x:viewport.width-3,y:viewport.height-3},{x:2,y:viewport.height-2},{x:viewport.width-2,y:2}]){
  // A right-click with the pointer at the window's edge (on the message, so the menu is a long one).
  await agentMessage.evaluate((node,at)=>node.dispatchEvent(new MouseEvent('contextmenu',{bubbles:true,cancelable:true,clientX:at.x,clientY:at.y,button:2})),at);await expect(menu).toBeVisible();
  const b=await menu.boundingBox();
  assert.ok(b.x>=0&&b.y>=0&&b.x+b.width<=viewport.width&&b.y+b.height<=viewport.height,`menu inside the window at ${JSON.stringify(at)}: ${JSON.stringify(b)}`);
  await close();
 }

 // Text fields keep the native menu (spelling, paste, emoji).
 await composer.click({button:'right'});
 assert.equal(await lastPrevented(),false,'the composer keeps the native menu');await expect(menu).toHaveCount(0);
 await page.keyboard.press('Escape');

 // Nowhere in particular: Reload window, and the inspector in development.
 await open(page.locator('.explorer-heading').first());
 assert.deepEqual(await labels(),['Reload window','Inspect element']);
 await expect(item('Inspect element')).toHaveAttribute('aria-disabled','true');
 await close();

 // Files tree.
 await page.getByRole('button',{name:'Browse files',exact:true}).click();
 const tree=page.getByRole('tree',{name:'Workspace files'});
 const readme=tree.locator('[data-path="api/README.md"]');
 await expect(readme).toBeVisible();
 await open(readme);
 assert.deepEqual((await labels()).slice(0,3),['Open','Open to the side','Quick look']);
 assert.ok((await labels()).includes('Track as artifact'),'the artifacts feature contributes to the Files menu');
 await item('Copy relative path').click();assert.equal(await copied(),'api/README.md');
 await open(readme);await item('Copy path').click();await expect.poll(copied).toBe('/fixture/collections/alpha/api/README.md');
 await open(tree.locator('[data-path="api/src"]'));assert.ok((await labels()).includes('Collapse folder')||(await labels()).includes('Expand folder'));await close();
 await open(readme);await item('Open').click();
 await expect(editorPanel.getByRole('tab',{name:/README\.md/})).toBeVisible();

 // Editor tabs: Close others.
 const tabs=editorPanel.getByRole('tab');
 const before=await tabs.count();assert.ok(before>=2,`two tabs open (${before})`);
 await open(editorPanel.getByRole('tab',{name:/README\.md/}).first());
 assert.ok((await labels()).includes('Close to the right'));
 await item('Close others').click();
 await expect(editorPanel.locator('.editor-group').filter({has:page.getByRole('tab',{name:/README\.md/})}).getByRole('tab')).toHaveCount(1);

 // Monaco keeps its own menu, in Lazybox colours.
 const source=editorPanel.locator('.monaco-editor .view-lines').first();
 if(await source.count()){
  await source.click({button:'right'});
  await expect(page.locator('.monaco-menu').first()).toBeVisible();
  await expect(menu).toHaveCount(0);
  await page.keyboard.press('Escape');
 }

 // Panel tabs: the group menu's own actions (the same items as its ⋯).
 await open(page.getByRole('tab',{name:'Files',exact:true}));
 const panelItems=await labels();
 assert.deepEqual(panelItems.slice(0,2),['Split right','Split down']);
 for(const name of ['Dock left','Dock right','Dock top','Dock bottom','Open in new window','Close Files'])assert.ok(panelItems.includes(name),name);
 await item('Close Files').click();await expect(page.locator('section.workspace-panel.panel-files')).toHaveCount(0);

 // Workspace rows: the ⋯ menu's items, from the same list.
 const row=page.locator('.workspace-row').filter({hasText:'Alpha feature'});
 await open(row.locator('.workspace-link'));
 const rowItems=(await labels()).filter(name=>name!=='Inspect element');
 for(const name of ['New conversation','Add repositories…','Manage repositories…','Rename workspace','Delete workspace…'])assert.ok(rowItems.includes(name),name);
 await item('Rename workspace').click();
 await expect(page.getByRole('textbox',{name:'Workspace name'})).toBeFocused();
 // The rename field keeps the native menu.
 await page.getByRole('textbox',{name:'Workspace name'}).click({button:'right'});assert.equal(await lastPrevented(),false);
 await page.getByRole('textbox',{name:'Workspace name'}).press('Escape');await expect(page.getByRole('textbox',{name:'Workspace name'})).toHaveCount(0);
 await row.getByLabel('Actions for Alpha feature',{exact:true}).click();
 const dots=row.locator('.action-popover');
 const dotItems=(await dots.locator('button').allTextContents()).map(text=>text.trim());
 assert.deepEqual(dotItems,rowItems.filter(name=>dotItems.includes(name)),'the ⋯ menu lists the same actions in the same order');
 assert.equal(dotItems.length,rowItems.length);
 await page.keyboard.press('Escape');

 // Changes: a file row and a diff line.
 await openWorkspaceChanges(page);
 const changes=page.getByRole('region',{name:'Workspace changes'});
 const fileRow=changes.getByRole('navigation',{name:'Changed files'}).getByRole('button',{name:/handler\.go/});
 await open(fileRow);
 assert.deepEqual((await labels()).slice(0,3),['Show diff','Open file','Open file to the side']);
 await item('Copy relative path').click();await expect.poll(copied).toBe('api/src/handler.go');
 const diff=changes.getByRole('grid',{name:'File diff'});
 await open(diff.getByRole('row').filter({hasText:'func Handle() error'}).locator('.review-code'));
 assert.deepEqual((await labels()).slice(0,3),['Add review comment','Copy line','Open file']);
 await item('Add review comment').click();
 await expect(changes.getByRole('form',{name:/Comment on line/})).toBeVisible();

 // Processes.
 await page.locator('summary[aria-label="Panels"]').click();await page.getByRole('button',{name:'Show Processes',exact:true}).click();
 const processes=page.getByRole('region',{name:'Hosted processes',exact:true});
 const agentRow=processes.locator('.process-row').filter({hasText:'Fix the parser'}).first();
 await open(agentRow);
 assert.ok((await labels()).includes('Stop agent…'));
 await item('Copy PID').click();assert.match(await copied(),/^\d+$/);

 // Terminal: Copy, Paste, Select all, Clear, Add selection to draft.
 await page.getByRole('button',{name:'Open terminal',exact:true}).click();
 const screen=page.locator('.panel-terminal').getByTestId('terminal');
 await expect(screen).toContainText('fixture shell in Alpha feature');
 await open(screen);
 assert.deepEqual((await labels()).slice(0,5),['Copy','Paste','Select all','Clear','Add selection to draft']);
 await expect(item('Copy')).toHaveAttribute('aria-disabled','true');
 await item('Select all').click();
 await open(screen);await expect(item('Copy')).not.toHaveAttribute('aria-disabled','true');
 await item('Copy').click();assert.match(await copied(),/fixture shell in Alpha feature/);
 await open(screen);await item('Add selection to draft').click();
 await expect(composer).toHaveValue(/```\n[^]*fixture shell in Alpha feature/);
 await composer.fill('');

 // Inbox rows, inside the modal dialog.
 await page.getByRole('button',{name:/^Inbox/}).click();
 const inbox=page.getByRole('dialog',{name:'Inbox'});
 await open(inbox.locator('.inbox-row').first());
 // The same conversation menu as every list's, then Mark all seen.
 assert.deepEqual((await labels()).slice(0,7),['Open','Open in new tab','Open beside','Open below','Open in new window','Move to new page','Peek']);assert.ok((await labels()).includes('Mark as read'));assert.ok((await labels()).includes('Mark all seen'));
 const inMenu=await menu.evaluate(node=>!!node.closest('dialog'));assert.ok(inMenu,'the menu is above the modal inbox, not behind it');
 await item('Open').click();await expect(inbox).toHaveCount(0);

 // An agent turn: its checkpoint actions, from the message or from the turn's marker.
 // Its ⋯ menu lists the same actions.
 await chat.getByRole('group',{name:'Turn 1 checkpoint'}).getByLabel('Turn 1 actions',{exact:true}).click();
 await expect(chat.getByRole('group',{name:'Turn 1 checkpoint'}).locator('.action-popover button')).toHaveText(['Show changes in this turn','Review this turn with Codex','Review this turn with Claude','Revert to before this turn…']);
 await page.keyboard.press('Escape');
 await open(chat.getByRole('group',{name:'Turn 1 checkpoint'}));
 assert.deepEqual((await labels()).slice(0,4),['Show changes in this turn','Review this turn with Codex','Review this turn with Claude','Revert to before this turn…']);
 await item('Show changes in this turn').click();
 await expect(page.getByRole('region',{name:'Workspace changes'})).toContainText('Agent turn 1');

 // A long press on touch opens the same menu.
 await agentMessage.locator('p').first().evaluate(node=>{const r=node.getBoundingClientRect();node.dispatchEvent(new PointerEvent('pointerdown',{bubbles:true,pointerType:'touch',isPrimary:true,clientX:r.x+4,clientY:r.y+6}));});
 await expect(menu).toBeVisible({timeout:2000});assert.equal((await labels())[0],'Quote in reply');await close();

 // A window too small for the link menu either way (each side of the pointer
 // narrower than the menu, whatever the font) pins it over the pointer:
 // a slow right-click there still chooses nothing.
 await page.setViewportSize({width:400,height:250});
 await page.evaluate(()=>{const a=document.createElement('a');a.href='https://example.com/pinned';a.target='_blank';a.textContent='pinned';a.id='pinned-link';a.style.cssText='position:fixed;left:200px;top:110px;z-index:2147483646;padding:2px 4px;background:Canvas';document.body.append(a);});
 {const b=await page.locator('#pinned-link').boundingBox(),x=b.x+b.width/2,y=b.y+b.height/2;
  await page.mouse.move(x,y);await page.mouse.down({button:'right'});await expect(menu).toBeVisible();
  const m=await menu.boundingBox();assert.ok(x>=m.x&&x<=m.x+m.width&&y>=m.y&&y<=m.y+m.height,`the menu is pinned over the pointer here ${JSON.stringify({x,y,m})}`);
  await page.waitForTimeout(450);await page.mouse.up({button:'right'});
  await nothingOpened('a slow right-click with the menu over the pointer');await expect(menu).toBeVisible();await close();}
 await page.evaluate(()=>document.getElementById('pinned-link')?.remove());

 assert.deepEqual(errors,[]);
 console.log('PASS: Lazybox context menus on messages, agent turns, selections, code blocks, links, file links, command output, Files rows, editor tabs, panel headers, workspace rows, Changes rows and diff lines, processes, terminal and inbox; never the browser menu; keyboard, type-ahead, long press, outside/scroll/Escape dismissal, edge placement; text fields and Monaco keep their own menus. Fixture only.');
}finally{await browser?.close();await server.close();await fixture.close();}
