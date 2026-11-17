// Workspace terminal panel against the fake Runner: open from the header,
// type into the shell, open one in a member repository, close it, and
// re-attach after a reload without starting another shell. WebKit by default
// (the desktop app's engine); TEST_CHROMIUM=1 runs Chromium.
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from 'vite';
import react from '@vitejs/plugin-react';
import {webkit, chromium, expect} from '@playwright/test';
import {fileURLToPath} from 'node:url';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';

const fixture=await startRunnerFixture();
const server=await createServer({...testViteCache(import.meta.url),configFile:false,root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
const calls=method=>fixture.calls.filter(call=>call.method===method);
let browser;
try {
 await server.listen();browser=await (process.env.TEST_CHROMIUM?chromium:webkit).launch();
 const context=await browser.newContext({viewport:{width:1440,height:900}});
 const page=await context.newPage();
 // A reload cuts in-flight requests (a CloseTerminal still on its way on a slow machine), which WebKit reports as page errors ("access control checks") on the page being left: not failures.
 let navigating=false;const errors=[];page.on('pageerror',e=>{if(!navigating)errors.push(String(e));});
 const reload=async()=>{navigating=true;await page.reload();await expect(page.locator('.panel-terminal').getByTestId('terminal')).toContainText('ran: ls -la');navigating=false;};
 await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`);
 await expect(page.locator('.panel-chat').getByRole('textbox',{name:'Message',exact:true})).toBeVisible();
 assert.equal(calls('OpenTerminal').length,0,'showing a workspace never starts a shell');

 await page.getByRole('button',{name:'Open terminal',exact:true}).click();
 const panel=page.locator('.panel-terminal');
 const screen=panel.getByTestId('terminal');
 await expect(screen).toContainText('fixture shell in Alpha feature');
 await expect.poll(()=>calls('OpenTerminal').length).toBe(1);
 assert.equal(calls('OpenTerminal')[0].body.collectionId,'alpha');
 // The view fits the panel and tells the Runner its real size.
 await expect.poll(()=>calls('ResizeTerminal').length).toBeGreaterThan(0);
 const [first]=fixture.terminals.values();
 assert.ok(first.cols>20&&first.rows>3&&!(first.cols===80&&first.rows===24),`fitted size ${first.cols}x${first.rows}`);
 // It opens beside the conversation, full height, rather than as a row under the page.
 const box=locator=>locator.boundingBox();
 let chatBox=await box(page.locator('.panel-chat')),termBox=await box(panel);
 assert.ok(termBox.x>=chatBox.x+chatBox.width-8,`terminal docks right of the chat (${termBox.x} vs ${chatBox.x+chatBox.width})`);
 assert.ok(Math.abs(termBox.y-chatBox.y)<8&&termBox.height>600,'the docked terminal spans the page height');
 assert.ok(termBox.width>350,'a side terminal keeps enough columns');
 await expect(screen.locator('textarea')).toBeFocused();
 // Asking again focuses the open terminal: no second panel, no second shell.
 await page.locator('.panel-chat').getByRole('textbox',{name:'Message',exact:true}).focus();
 await page.getByRole('button',{name:'Open terminal',exact:true}).click();
 await expect(screen.locator('textarea')).toBeFocused();
 await expect(page.locator('.panel-terminal')).toHaveCount(1);
 assert.equal(calls('OpenTerminal').length,1,'showing the open terminal does not start another shell');

 await screen.click();await page.keyboard.type('ls -la');await page.keyboard.press('Enter');
 await expect(screen).toContainText('ran: ls -la');
 assert.equal(first.input.join(''),'ls -la\r','keystrokes reach the shell in order');

 await panel.getByLabel('Terminal directory').selectOption('api');
 await panel.getByRole('button',{name:'New terminal',exact:true}).click();
 await expect(panel.getByRole('tab',{name:'api',exact:true})).toHaveAttribute('aria-selected','true');
 await expect(screen).toContainText('fixture shell in Alpha feature / api');
 assert.equal(calls('OpenTerminal').at(-1).body.directory,'api');
 await page.screenshot({path:'/tmp/lazybox-terminal.png'});
 await panel.getByRole('button',{name:'Close terminal api',exact:true}).click();
 await expect(panel.getByRole('tab',{name:'api',exact:true})).toHaveCount(0);
 await expect(panel.getByRole('tab',{name:'Workspace root',exact:true})).toHaveAttribute('aria-selected','true');
 await expect(screen).toContainText('ran: ls -la');

 // A reload re-attaches to the running shell with its scrollback.
 const opened=calls('OpenTerminal').length;
 await reload();
 assert.equal(calls('OpenTerminal').length,opened,'re-attaching does not start another shell');

 // Rearranging never remounts a neighbour: the chat keeps its element and its event stream.
 await page.locator('.panel-chat').evaluate(node=>{node.dataset.qaIdentity='kept';});
 const streams=calls('WatchEvents').length;
 // × closes the panel; the shell keeps running and the header brings it back, re-attached.
 await page.getByRole('button',{name:'Close Terminal panel',exact:true}).click();
 await expect(page.locator('.panel-terminal')).toHaveCount(0);
 await expect(page.locator('.panel-chat').getByRole('textbox',{name:'Message',exact:true})).toBeFocused();
 await page.getByRole('button',{name:'Open terminal',exact:true}).click();
 await expect(page.locator('.panel-terminal').getByTestId('terminal')).toContainText('ran: ls -la');
 assert.equal(calls('OpenTerminal').length,opened,'reopening the panel re-attaches to the running shell');

 // The group menu (right-click a tab) docks the command output on the right, and a reload keeps the arrangement.
 await page.getByRole('tab',{name:'Command output',exact:true}).click({button:'right'});
 await page.getByRole('menuitem',{name:'Dock right',exact:true}).click();
 const output=page.locator('.panel-console');
 const arrangement=async()=>{const [c,t,o]=await Promise.all([box(page.locator('.panel-chat')),box(page.locator('.panel-terminal')),box(output)]);return {c,t,o};};
 await expect.poll(async()=>{const a=await arrangement();return a.o.x>=a.t.x+a.t.width-8&&a.t.x>=a.c.x+a.c.width-8;},{message:'chat | terminal | output'}).toBe(true);
 let shown=await arrangement();
 assert.ok(shown.o.height>600,'output docked right spans the page height');
 assert.equal(await page.locator('.panel-chat').getAttribute('data-qa-identity'),'kept','closing and docking neighbours keeps the chat mounted');
 assert.equal(calls('WatchEvents').length,streams,'the chat did not reconnect');
 await page.screenshot({path:'/tmp/lazybox-docked-panels.png'});
 await reload();
 const restored=await arrangement();
 assert.ok(Math.abs(restored.o.x-shown.o.x)<4&&Math.abs(restored.t.x-shown.t.x)<4,'reload keeps the docked layout');

 await page.locator('.panel-chat').evaluate(node=>{node.dataset.qaIdentity='kept';});
 // Keyboard only: the group menu docks the terminal left, and the next terminal opens where it was last docked.
 await page.getByLabel('Terminal panel actions',{exact:true}).focus();
 await page.keyboard.press('Enter');
 await expect(page.getByRole('button',{name:/^Split right/})).toBeFocused();
 // Each item lays its icon beside its label, apart (the shell's generic menu rule does not win).
 const itemLayout=await page.getByRole('button',{name:/^Split right/}).evaluate(node=>{const icon=node.querySelector('svg').getBoundingClientRect(),label=node.querySelector('span').getBoundingClientRect();return {display:getComputedStyle(node).display,gap:label.left-icon.right,offset:Math.abs((icon.top+icon.bottom)/2-(label.top+label.bottom)/2)};});
 assert.equal(itemLayout.display,'flex','a group menu item is a row');
 assert.ok(itemLayout.gap>=6&&itemLayout.offset<=2,`icon and label side by side, apart and centred (${JSON.stringify(itemLayout)})`);
 await page.getByRole('button',{name:'Dock left',exact:true}).focus();
 await page.keyboard.press('Enter');
 await expect.poll(async()=>{const a=await arrangement();return a.t.x+a.t.width<=a.c.x+8;},{message:'terminal docked left of the chat'}).toBe(true);
 await page.getByRole('button',{name:'Close Terminal panel',exact:true}).click();
 await page.getByRole('button',{name:'Open terminal',exact:true}).click();
 await expect(page.locator('.panel-terminal').getByTestId('terminal')).toContainText('ran: ls -la');
 shown=await arrangement();
 assert.ok(shown.t.x+shown.t.width<=shown.c.x+8,'a reopened terminal returns to the side it was last docked at');
 // The workspace list keeps the page edge whatever is docked.
 const list=await box(page.locator('.page-layout .panel-collections'));
 assert.ok(list.x<shown.t.x,'workspace list stays leftmost');

 // Dragging a tab onto the lower part of the conversation docks the terminal below it.
 const tab=await page.getByRole('tab',{name:'Terminal',exact:true}).boundingBox(),chatArea=await box(page.locator('.panel-chat'));
 await page.mouse.move(tab.x+tab.width/2,tab.y+tab.height/2);await page.mouse.down();
 await page.mouse.move(tab.x+tab.width/2+12,tab.y+tab.height/2+12,{steps:4});
 await page.mouse.move(chatArea.x+chatArea.width/2,chatArea.y+chatArea.height-30,{steps:10});
 await expect(page.locator('.dv-drop-target-anchor, .dv-drop-target-selection').first()).toBeVisible();
 await page.screenshot({path:'/tmp/lazybox-drag-bottom.png'});
 await page.mouse.up();
 await expect.poll(async()=>{const a=await arrangement();return a.t.y>=a.c.y+a.c.height-8;},{message:'dragging to the bottom of the chat docks the terminal below it'}).toBe(true);
 await page.screenshot({path:'/tmp/lazybox-dragged-terminal.png'});
 assert.equal(await page.locator('.panel-chat').getAttribute('data-qa-identity'),'kept');
 await expect(page.locator('.panel-terminal').getByTestId('terminal')).toContainText('ran: ls -la');

 // A closed panel comes back from the Panels menu, where it was last docked.
 await page.getByRole('button',{name:'Close Command output panel',exact:true}).click();
 await expect(output).toHaveCount(0);
 await page.locator('summary[aria-label="Panels"]').click();
 await page.getByRole('button',{name:'Show Command output',exact:true}).click();
 await expect(output).toBeVisible();
 shown=await arrangement();
 assert.ok(shown.o.x>=shown.c.x+shown.c.width-8,'output returns at the side it was last docked');

 // On a phone, one panel at a time: no docking menu, a reachable ×, no page overflow.
 await page.setViewportSize({width:390,height:844});
 await page.getByRole('navigation',{name:'Workspace panels'}).getByRole('button',{name:'Terminal',exact:true}).click();
 await expect(page.locator('.panel-terminal').getByTestId('terminal')).toBeVisible();
 await expect(page.getByLabel('Terminal panel actions',{exact:true})).toHaveCount(0);
 await expect(page.getByRole('button',{name:'Close Terminal panel',exact:true})).toBeVisible();
 assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth),false,'no horizontal overflow on a phone');
 await page.screenshot({path:'/tmp/lazybox-terminal-phone.png'});
 await page.getByRole('button',{name:'Close Terminal panel',exact:true}).click();
 await expect(page.locator('.panel-terminal')).toHaveCount(0);
 await expect(page.locator('.panel-chat')).toBeVisible();
 assert.equal(calls('OpenTerminal').length,opened);
 // A shell that exits cleanly closes its tab, and the panel with the last one.
 await page.setViewportSize({width:1300,height:850});
 await page.getByRole('button',{name:'Open terminal',exact:true}).click();
 const last=page.locator('.panel-terminal').getByTestId('terminal');await expect(last).toBeVisible();
 await last.click();await page.keyboard.type('exit');await page.keyboard.press('Enter');
 await expect(page.locator('.panel-terminal')).toHaveCount(0);
 await expect(page.locator('.panel-chat')).toBeVisible();
 assert.deepEqual(errors,[]);
 console.log('PASS terminal: exit closes the tab and panel, header opens a shell docked right, focuses rather than duplicates, fitted size, ordered input, member directory, close, re-attach after reload, × close and reopen, dock via menu and keyboard, layout survives reload, remembered dock side, drag a tab below the chat with a drop preview, neighbours stay mounted, reopen from Panels, phone close.');
} finally {await browser?.close();await server.close();await fixture.close();}
