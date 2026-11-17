// The page dock (DockLayout, dockview): drag a tab onto another panel's edge to
// split or onto its middle to join its tabs, with a live drop preview; resize
// by mouse and keyboard; maximize; minimize; presets applied and saved; Files
// and the Editor popped out into their own window and brought back with an
// unsaved edit intact; nothing remounts when moved (a chat draft and a running
// terminal survive); switching workspace keeps every panel where it was; the
// Agents list beside Workspaces; one panel at a time on a phone. WebKit, the
// desktop app's engine, against the fake Runner fixture.
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from 'vite';
import react from '@vitejs/plugin-react';
import {webkit,expect} from './fixtures/playwright.mjs';
import {fileURLToPath} from 'node:url';
import {mkdir} from 'node:fs/promises';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import {monacoVimCompat} from './monaco-vim-compat.mjs';

const fixture=await startRunnerFixture();
const server=await createServer({...testViteCache(import.meta.url),configFile:false,root:fileURLToPath(new URL('../apps/web',import.meta.url)),logLevel:'warn',plugins:[react(),monacoVimCompat()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
await server.listen();
const shot=async(page,name)=>{await mkdir('.cache',{recursive:true});await page.screenshot({path:`.cache/layout-${name}.png`});};
let browser;
try{
 browser=await webkit.launch({headless:true});
 const context=await browser.newContext({viewport:{width:1440,height:900}});
 const page=await context.newPage();page.setDefaultTimeout(15000);
 const errors=[];context.on('page',p=>p.on('pageerror',e=>errors.push(String(e))));page.on('pageerror',e=>errors.push(String(e)));
 await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`);
 const tab=name=>page.locator('.dv-tab').filter({has:page.locator(`[data-kind="${name}"]`)});
 const content=kind=>page.locator(`[data-dock-panel]:has(> .panel-host > .panel-${kind})`);
 const chat=page.locator('.panel-chat'),composer=chat.getByRole('textbox',{name:'Message',exact:true});
 await expect(composer).toBeEnabled();
 await expect(page.locator('.dv-tab .dock-tab-title')).toHaveText(['Workspaces','Agents','Agent chat','Command output']);
 await shot(page,'default');

 // Identity markers: a remount would lose them.
 await composer.fill('draft kept through every move');
 await chat.evaluate(node=>{node.dataset.identity='original';});
 const kept=async(why)=>{
  assert.equal(await chat.getAttribute('data-identity'),'original',`${why}: the chat was not remounted`);
  await expect(composer,why).toHaveValue('draft kept through every move');
 };
 let streams=fixture.calls.filter(c=>c.method==='WatchEvents').length;

 // A minimized group opens from its tab.
 const outputHeight=()=>content('console').evaluate(n=>Math.round(n.getBoundingClientRect().height));
 assert.ok(await page.locator('.dock-group-minimized').count()===1,'command output starts minimized');
 await tab('console').click();
 await expect.poll(outputHeight).toBeGreaterThan(120);
 await expect(page.locator('.dock-group-minimized')).toHaveCount(0);

 // Drag the output's tab to the right edge of the chat: a live preview, then a split.
 const drag=async(from,to,during)=>{
  const a=await from.boundingBox();
  await page.mouse.move(a.x+a.width/2,a.y+a.height/2);await page.mouse.down();
  await page.mouse.move(a.x+a.width/2+14,a.y+a.height/2+14,{steps:4});
  await page.mouse.move(to.x,to.y,{steps:12});
  await page.waitForTimeout(120);
  if(during)await during();
  await page.mouse.up();
 };
 let chatBox=await content('chat').boundingBox();
 await drag(tab('console'),{x:chatBox.x+chatBox.width-24,y:chatBox.y+chatBox.height/2},async()=>{
  const preview=page.locator('.dv-drop-target-anchor, .dv-drop-target-selection').first();
  await expect(preview).toBeVisible();
  // The outline settles (it animates) on the right half of the chat.
  await expect.poll(async()=>{const box=await preview.boundingBox();return box.x>chatBox.x+chatBox.width*0.4&&box.width<chatBox.width*0.62;},{message:'the preview shows the right half'}).toBe(true);
  await shot(page,'drop-preview-right');
 });
 await expect.poll(async()=>{const c=await content('chat').boundingBox(),o=await content('console').boundingBox();return o.x>=c.x+c.width-4&&Math.abs(o.y-c.y)<4;},{message:'output docked right of the chat'}).toBe(true);
 await kept('split right');
 await shot(page,'split-right');

 // Drag it onto the middle of the chat: it joins the chat's tabs.
 chatBox=await content('chat').boundingBox();
 await drag(tab('console'),{x:chatBox.x+chatBox.width/2,y:chatBox.y+chatBox.height/2},async()=>{await shot(page,'drop-preview-center');});
 await expect.poll(()=>page.locator('.dv-groupview').filter({has:page.locator('[data-kind="chat"]')}).locator('.dock-tab').count(),{message:'chat and output share one group'}).toBe(2);
 await tab('chat').click();
 await kept('joined as tabs');
 // And back below the chat, from the group menu (the keyboard way to dock).
 await tab('console').click({button:'right'});
 await page.getByRole('menuitem',{name:'Dock bottom',exact:true}).click();
 await expect.poll(async()=>{const c=await content('chat').boundingBox(),o=await content('console').boundingBox();return o.y>=c.y+c.height-4;}).toBe(true);
 await kept('docked from the menu');

 const rects=()=>page.locator('.dv-groupview').evaluateAll(nodes=>nodes.map(n=>{const b=n.getBoundingClientRect();return [Math.round(b.x),Math.round(b.y),Math.round(b.width),Math.round(b.height)].join(',');}).sort().join(' '));
 // Resize: the divider between chat and output with the keyboard; the sidebar with the mouse.
 const divider=page.getByRole('separator',{name:/Resize Agent chat and Command output/});
 const before=Number(await divider.getAttribute('aria-valuenow'));
 await divider.focus();await page.keyboard.press('ArrowDown');await page.keyboard.press('ArrowDown');
 await expect.poll(async()=>Number(await page.getByRole('separator',{name:/Resize Agent chat and Command output/}).getAttribute('aria-valuenow'))).toBeGreaterThan(before);
 const sidebarWidth=()=>content('collections').evaluate(n=>Math.round(n.getBoundingClientRect().width));
 assert.equal(await sidebarWidth(),280);
 const edge=await page.getByRole('separator',{name:'Resize Workspaces sidebar'}).boundingBox();
 await page.mouse.move(edge.x+edge.width/2,edge.y+edge.height/2);await page.mouse.down();await page.mouse.move(edge.x+edge.width/2+60,edge.y+edge.height/2,{steps:6});await page.mouse.up();
 await expect.poll(sidebarWidth).toBeGreaterThan(330);
 await expect.poll(()=>page.getByRole('separator',{name:'Resize Workspaces sidebar'}).getAttribute('aria-valuenow')).toBe(String(await sidebarWidth()));
 const widened=await sidebarWidth();

 // Maximize: double-click a tab, and the keyboard chord; everything else steps aside, then comes back.
 await tab('chat').dblclick();
 await expect(content('console')).toBeHidden();
 await expect(page.getByRole('button',{name:'Restore layout'}).first()).toBeVisible();
 await shot(page,'maximized');
 await tab('chat').dblclick();
 await expect(content('console')).toBeVisible();
 await page.locator('.workspace-brand').click();
 // The registry's shortcut (⇧⌘M) and the which-key sequence (] ] m) do the same.
 await page.keyboard.press('Meta+Shift+m');
 await expect(content('console')).toBeHidden();
 await page.keyboard.press(']');await page.keyboard.press(']');await page.keyboard.press('m');
 await expect(content('console')).toBeVisible();
 await kept('maximize and restore');

 // A terminal: running, then moved; its screen is the same element.
 await page.getByRole('button',{name:'Open terminal',exact:true}).click();
 const screen=page.locator('.panel-terminal').getByTestId('terminal');
 await expect(screen).toContainText('fixture shell in Alpha feature');
 await screen.evaluate(node=>{node.dataset.identity='shell';}); const opened=fixture.calls.filter(c=>c.method==='OpenTerminal').length;
 const terminalBox=await content('terminal').boundingBox();
 await drag(tab('terminal'),{x:(await content('console').boundingBox()).x+40,y:(await content('console').boundingBox()).y+60});
 await expect.poll(async()=>{const t=await content('terminal').boundingBox();return Math.abs(t.x-terminalBox.x)>20||Math.abs(t.y-terminalBox.y)>20;},{message:'the terminal moved'}).toBe(true);
 assert.equal(await screen.getAttribute('data-identity'),'shell','the terminal was not remounted');
 await expect(screen).toContainText('fixture shell in Alpha feature');
 assert.equal(fixture.calls.filter(c=>c.method==='OpenTerminal').length,opened,'moving never starts another shell');
 await kept('terminal moved');

 // Presets: pictures in the Layout menu; applying one keeps panels; save, rename, delete; reset.
 const layoutMenu=page.locator('summary[aria-label="Layout"]');
 await layoutMenu.click();
 await expect(page.getByRole('button',{name:/^Apply layout /})).toHaveCount(5);
 await shot(page,'presets');
 await page.getByRole('button',{name:'Apply layout Review',exact:true}).click();
 await expect(page.locator('.dv-tab .dock-tab-title')).toHaveText(['Workspaces','Agents','Changes','Editor','Agent chat']);
 await kept('Review preset');
 await shot(page,'preset-review');
 await layoutMenu.click();
 await page.getByRole('button',{name:'Save current layout as…'}).click();
 await page.getByRole('textbox',{name:'Layout name'}).fill('My review');
 await page.getByRole('button',{name:'Save',exact:true}).click();
 await expect(page.getByRole('button',{name:'Apply layout My review'})).toBeVisible();
 await page.getByRole('button',{name:'Rename layout My review'}).click();
 await page.getByRole('textbox',{name:'New layout name'}).fill('Code review');
 await page.getByRole('button',{name:'Rename',exact:true}).click();
 await expect(page.getByRole('button',{name:'Apply layout Code review'})).toBeVisible();
 await page.getByRole('button',{name:'Apply layout Chat + Editor + Terminal'}).click();
 await expect(page.locator('.dv-tab .dock-tab-title')).toHaveText(['Workspaces','Agents','Agent chat','Editor','Terminal']);
 // Review closed the Terminal panel; the shell kept running and the panel finds it again.
 await expect(screen).toContainText('fixture shell in Alpha feature');
 assert.equal(fixture.calls.filter(c=>c.method==='OpenTerminal').length,opened,'closing and reopening the panel never starts another shell');
 await layoutMenu.click();
 await page.getByRole('button',{name:'Apply layout Code review'}).click();
 await expect(page.locator('.dv-tab .dock-tab-title')).toHaveText(['Workspaces','Agents','Changes','Editor','Agent chat']);
 await layoutMenu.click();
 await page.getByRole('button',{name:'Delete layout Code review'}).click();
 await expect(page.getByRole('button',{name:'Apply layout Code review'})).toHaveCount(0);
 await page.getByRole('button',{name:'Reset to default'}).click();
 await expect(page.locator('.dv-tab .dock-tab-title')).toHaveText(['Workspaces','Agents','Agent chat','Command output']);
 await kept('reset');

 // Files and the Editor: open a file, edit it, pop the editor out, bring it back: the edit is there.
 await page.locator('summary[aria-label="Panels"]').click();
 await page.getByRole('button',{name:'Show Editor'}).click();
 await page.getByRole('button',{name:'Browse files',exact:true}).click();
 const tree=page.getByRole('tree',{name:'Workspace files'});
 await tree.locator('[data-path="api/config.toml"]').dblclick();
 const editor=page.locator('section.workspace-panel.panel-editor');
 const source=editor.getByRole('textbox',{name:'File source editor'}).first();
 await expect(source).toBeVisible();
 await source.focus();await page.keyboard.press('Meta+a');await page.keyboard.insertText('unsaved through a window\n');
 await expect(editor.locator('.view-lines')).toContainText('unsaved through a window');
 await editor.evaluate(node=>{node.dataset.identity='editor';});
 await shot(page,'files-editor');
 const popupOpened=page.waitForEvent('popup');
 await tab('editor').click({button:'right'});
 await page.getByRole('menuitem',{name:'Open in new window',exact:true}).click();
 const popup=await popupOpened;
 await expect(popup.locator('section.workspace-panel.panel-editor .view-lines')).toContainText('unsaved through a window');
 assert.equal(await popup.locator('section.workspace-panel.panel-editor').getAttribute('data-identity'),'editor','the editor moved into the window; it was not remounted');
 await expect(page.locator('section.workspace-panel.panel-editor')).toHaveCount(0);
 await popup.screenshot({path:'.cache/layout-popout-editor.png'});
 // Opening another file from Files goes to the editor in its window.
 await tree.locator('[data-path="api/package.json"]').dblclick();
 await expect(popup.locator('section.workspace-panel.panel-editor').getByRole('tab')).toHaveCount(2);
 const closed=popup.waitForEvent('close');
 await popup.getByRole('button',{name:'Put back in main window'}).click();
 await closed;
 await expect(page.locator('section.workspace-panel.panel-editor')).toBeVisible();
 assert.equal(await page.locator('section.workspace-panel.panel-editor').getAttribute('data-identity'),'editor');
 await page.locator('section.workspace-panel.panel-editor').getByRole('tab',{name:/config\.toml/}).click();
 await expect(page.locator('section.workspace-panel.panel-editor .view-lines')).toContainText('unsaved through a window');
 // Files pops out too.
 const filesOpened=page.waitForEvent('popup');
 await tab('files').click({button:'right'});
 await page.getByRole('menuitem',{name:'Open in new window',exact:true}).click();
 const filesWindow=await filesOpened;
 await expect(filesWindow.getByRole('tree',{name:'Workspace files'})).toBeVisible();
 const filesClosed=filesWindow.waitForEvent('close');await filesWindow.close();await filesClosed;
 await expect(page.getByRole('tree',{name:'Workspace files'})).toBeVisible();
 await kept('popouts');

 assert.equal(fixture.calls.filter(c=>c.method==='WatchEvents').length,streams,'no rearrangement reconnected the chat');
 // Switching workspace keeps every panel where it was, and the sidebar width.
 const rectsLater=()=>page.locator('.dv-groupview').evaluateAll(nodes=>nodes.map(n=>{const b=n.getBoundingClientRect();return [Math.round(b.x),Math.round(b.y),Math.round(b.width),Math.round(b.height)].join(',');}).sort().join(' '));
 await page.locator('summary[aria-label="Layout"]').click();await page.getByRole('button',{name:'Reset to default'}).click();
 await tab('console').click();await expect.poll(outputHeight).toBeGreaterThan(120);
 const here=await rectsLater(); await page.locator('.workspace-row').filter({hasText:'Beta release'}).locator('button').first().click();
 await expect(page.getByRole('combobox',{name:'Workspace',exact:true})).toHaveValue('beta');
 await page.waitForTimeout(400);
 assert.equal(await sidebarWidth(),widened,'the sidebar keeps its width in another workspace');
 await page.locator('.workspace-row').filter({hasText:'Alpha feature'}).locator('button').first().click();
 await expect(page.getByRole('combobox',{name:'Workspace',exact:true})).toHaveValue('alpha');
 await page.waitForTimeout(400);
 assert.equal(await rectsLater(),here,'coming back, every group is where it was');

 // Coming back reads the conversation again (it is another target); from here, nothing does.
 streams=fixture.calls.filter(c=>c.method==='WatchEvents').length;
 // Agents: the tab beside Workspaces lists every conversation by urgency.
 await tab('agents').click();
 const agents=page.locator('.agents-panel');
 await expect(agents.locator('.agent-row').first()).toBeVisible();
 await expect(agents.getByRole('group',{name:'Group agents'}).getByRole('button',{name:'Priority'})).toHaveAttribute('aria-pressed','true');
 await shot(page,'agents');
 await agents.getByRole('button',{name:'By workspace'}).click();
 await expect(agents.locator('.agents-group-title').first()).toContainText(/Alpha feature|Beta release/);
 await agents.locator('.agent-row').first().hover();
 await agents.locator('.agent-pin').first().click();
 await expect(agents.locator('[data-group="pinned"] .agent-row')).toHaveCount(1);
 await tab('collections').click();

 // Close and reopen from Panels; every panel can be closed.
 await tab('console').locator('.dock-tab-close').click();
 await expect(content('console')).toHaveCount(0);
 await page.locator('summary[aria-label="Panels"]').click();
 await page.getByRole('button',{name:'Show Command output'}).click();
 await expect(content('console')).toBeVisible();

 // Minimums: in a narrower window, Files, Editor and Terminal become tabs rather than being crushed.
 await page.setViewportSize({width:1100,height:900});
 await page.locator('summary[aria-label="Layout"]').click();await page.getByRole('button',{name:'Reset to default'}).click();
 for(const name of ['Files','Editor','Terminal']){
  await page.locator('summary[aria-label="Panels"]').click();
  await page.getByRole('button',{name:new RegExp(`^(Show|Focus) ${name}$`)}).click();
 }
 await page.waitForTimeout(400);
 for(const [kind,minimum] of [['chat',360],['files',200],['editor',320],['terminal',280]]){
  const box=await content(kind).boundingBox();
  if(box&&await content(kind).isVisible())assert.ok(box.width>=minimum-2,`${kind} keeps at least ${minimum}px (got ${Math.round(box.width)})`);
 }
 assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth),false,'no horizontal overflow');
 await shot(page,'minimums');
 // An open Changes panel's tool keeps only its icon: "Changes" is not said twice.
 await page.getByRole('button',{name:'Review changes',exact:true}).click();
 await expect(page.getByRole('button',{name:'Review changes',exact:true})).toHaveText('');
 await page.setViewportSize({width:1440,height:900});
 await page.getByRole('navigation',{name:'Pages'}).getByRole('button',{name:'Work',exact:true}).click();
 // Another page shows other panels; back on Work, the chat is drawn again: mark it anew.
 await expect(composer).toHaveValue('draft kept through every move');
 await chat.evaluate(node=>{node.dataset.identity='original';});

 // A phone shows one panel at a time; the chat is still the same.
 await page.setViewportSize({width:390,height:820});
 await expect(page.locator('.dock-layout')).toHaveCount(0);
 const switcher=page.getByRole('navigation',{name:'Workspace panels'});
 await expect(switcher).toBeVisible();
 await expect(page.locator('.page-layout .workspace-panel')).toHaveCount(1);
 await switcher.getByRole('button',{name:'Agent chat'}).click();
 await kept('phone');
 await switcher.getByRole('button',{name:'Command output'}).click();
 await expect(chat).toBeHidden();
 await page.getByRole('navigation',{name:'Workspace panels'}).getByRole('button',{name:'Agent chat'}).click();
 await kept('phone switch');
 // The phone's header keeps the essentials; Files, Terminal and Help are in ⋯.
 for(const tool of await page.locator('.workspace-header-actions > .phone-overflow, .workspace-header-actions > .help-menu').all())await expect(tool).toBeHidden();
 await page.getByLabel('Page actions',{exact:true}).click();
 for(const name of ['Browse files','Open terminal',"What's new"])await expect(page.getByRole('group',{name:'Page actions menu'}).getByRole('button',{name,exact:true})).toBeVisible();
 await page.keyboard.press('Escape');
 await shot(page,'phone');
 await page.setViewportSize({width:1440,height:900});
 await expect(page.locator('.dock-layout')).toBeVisible();
 await kept('back to desktop');

 assert.equal(fixture.calls.filter(c=>c.method==='WatchEvents').length,streams,'no rearrangement reconnected the chat');
 // Saved layouts are files (POWER.md, "Config files"): saving, renaming and deleting one writes it.
 assert.deepEqual(fixture.mutations.filter(m=>m.method==='UpdateConfigFile').map(m=>m.body.name),['layouts/my-review','layouts/my-review','layouts/my-review'],'saved, renamed and deleted as a file');
 assert.equal(fixture.mutations.filter(m=>!['OpenTerminal','ResizeTerminal','WriteTerminal','UpdateConfigFile'].includes(m.method)).length,0,'arranging panels never starts or stops an agent');
 assert.deepEqual(errors,[]);
 console.log('PASS docking: drag-to-split and drag-to-tab with live previews, keyboard and mouse resize, maximize (double-click, ⇧⌘M and ] ] m), minimize, presets applied/saved/renamed/deleted/reset, Editor and Files popped out and back with an unsaved edit, no remount of chat or terminal, workspace switch keeps geometry, Agents list, close/reopen, phone. WebKit, fixture Runner.');
}finally{await browser?.close();await server.close();await fixture.close();}

