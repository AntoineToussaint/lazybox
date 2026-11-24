// Workspace terminal panel against the fake Runner: open from the header,
// type into the shell, open one in a member repository, close it, and
// re-attach after a reload without starting another shell. WebKit by default
// (the desktop app's engine); TEST_CHROMIUM=1 runs Chromium.
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from 'vite';
import react from '@vitejs/plugin-react';
import {webkit, chromium, expect} from './fixtures/playwright.mjs';
import {fileURLToPath} from 'node:url';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import { collectPageErrors } from './fixtures/page-errors.mjs';

const fixture=await startRunnerFixture();
const chatEvents=fixture.openEvents('alpha-session');
const server=await createServer({...testViteCache(import.meta.url),configFile:false,root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
const calls=method=>fixture.calls.filter(call=>call.method===method);
let browser;
try {
 await server.listen();browser=await (process.env.TEST_CHROMIUM?chromium:webkit).launch();
 const context=await browser.newContext({viewport:{width:1440,height:900}});
 const page=await context.newPage();
 const errors=await collectPageErrors(page);
 const reload=async()=>{await page.reload();await expect(page.locator('.panel-terminal').getByTestId('terminal')).toContainText('ran: ls -la');};
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

 // Select a single word in xterm beside an existing conversation draft.
 const composer=page.locator('.panel-chat').getByRole('textbox',{name:'Message',exact:true});
 await composer.fill('Please explain');
 const banner=screen.locator('.xterm-rows > div').filter({hasText:'fixture shell in Alpha feature'}).first();
 const bannerBox=await banner.boundingBox();
 await page.mouse.dblclick(bannerBox.x+12,bannerBox.y+bannerBox.height/2);
 const share=panel.getByRole('button',{name:'Add selection to draft',exact:true});
 await expect(share).toBeVisible();
 const applied=calls('Apply').length;
 await share.click();
 await expect(composer).toHaveValue(/^Please explain\s+`{3}[^\n]*\nfixture\n`{3}$/);
 await expect(share).toHaveCount(0);
 assert.equal(calls('Apply').length,applied,'sharing prepares a draft');
 await composer.fill('');

 // ── Links: a sign-in URL that wraps across rows is one link, underlined with a pointer on hover; a click on any
 // part of it opens the whole address; a drag selects text and opens nothing; the right-click menu offers it.
 const signIn='https://accounts.example.com/o/oauth2/auth?response_type=code&client_id=32555940559.apps.example.com&redirect_uri=https%3A%2F%2Fsdk.example.com%2Fauthcode.html&scope=openid+https%3A%2F%2Fwww.example.com%2Fauth%2Fuserinfo.email+https%3A%2F%2Fwww.example.com%2Fauth%2Fcloud-platform+https%3A%2F%2Fwww.example.com%2Fauth%2Fappengine.admin+https%3A%2F%2Fwww.example.com%2Fauth%2Fsqlservice.login+https%3A%2F%2Fwww.example.com%2Fauth%2Fcompute+https%3A%2F%2Fwww.example.com%2Fauth%2Faccounts.reauth&state=Qx7fZk2bH0wLm4&prompt=consent&token_usage=remote&access_type=offline&code_challenge=AbCdEf0123456789_-AbCdEf0123456789_-AbCdEf0&code_challenge_method=S256';
 assert.ok(signIn.length>3*first.cols,`the address spans at least three rows of ${first.cols}`);
 await page.evaluate(()=>{window.__opened=[];window.open=url=>{window.__opened.push(String(url));return null;};});
 const linksOpened=()=>page.evaluate(()=>window.__opened);
 fixture.terminalOutput(first,`Go to the following link in your browser, and complete the sign-in prompts:\r\n\r\n    ${signIn}\r\n\r\nOnce finished, enter the verification code provided in your browser: `);
 await expect(screen).toContainText('enter the verification code');
 const rows=screen.locator('.xterm-rows > div');
 const at=(await rows.allTextContents()).findIndex(text=>text.includes('https://accounts'));
 assert.ok(at>=0,'the address is on screen');
 // The middle of the second row: the address's continuation, not its start.
 const row=await rows.nth(at+1).boundingBox();
 const middle={x:row.x+row.width/2,y:row.y+row.height/2};
 await page.mouse.move(middle.x,middle.y-30);await page.mouse.move(middle.x,middle.y,{steps:3});
 await expect(screen.locator('.xterm-screen')).toHaveClass(/xterm-cursor-pointer/);
 await expect(rows.nth(at).locator('span[style*="underline"]').first()).toContainText('https://accounts');
 await expect(rows.nth(at+2).locator('span[style*="underline"]').first()).toBeVisible();
 await page.mouse.down();await page.mouse.up();
 await expect.poll(linksOpened).toEqual([signIn]);
 // A drag across the address selects it and opens nothing.
 await page.mouse.move(row.x+20,middle.y);await page.mouse.down();await page.mouse.move(row.x+row.width/2,middle.y,{steps:6});await page.mouse.up();
 await page.waitForTimeout(200);
 assert.deepEqual(await linksOpened(),[signIn],'a drag does not open the link');
 // Right-click: open in the browser or the Web panel, or copy the link; the right-click itself opens nothing.
 await page.mouse.move(middle.x,middle.y);
 await expect(screen.locator('.xterm-screen')).toHaveClass(/xterm-cursor-pointer/);
 await page.mouse.click(middle.x,middle.y,{button:'right'});
 const linkMenu=page.locator('.context-menu[data-depth="0"]');
 const linkItems=await linkMenu.locator('.context-menu-label').allTextContents();
 assert.deepEqual(linkItems.slice(0,4),['Open in your browser','Open in Lazybox','Open in a new Lazybox panel','Copy link'],`link menu: ${linkItems}`);
 assert.ok(linkItems.includes('Copy')&&linkItems.includes('Add selection to draft'),'the terminal items follow');
 await linkMenu.getByRole('menuitem',{name:'Open in your browser'}).click();
 await expect.poll(linksOpened).toEqual([signIn,signIn]);
 await page.screenshot({path:'/tmp/lazybox-terminal-link.png'});
 await screen.click();

 await panel.getByLabel('Terminal directory').selectOption('api');
 await panel.getByRole('button',{name:'New terminal',exact:true}).click();
 await expect(panel.getByRole('tab',{name:'api',exact:true})).toHaveAttribute('aria-selected','true');
 await expect(screen).toContainText('fixture shell in Alpha feature / api');
 assert.equal(calls('OpenTerminal').at(-1).body.directory,'api');
 const strip = panel.getByRole('tablist',{name:'Terminals'});
 const overflow = await strip.evaluate(node => {
  node.style.maxWidth = '160px';
  const scrollable = node.scrollWidth > node.clientWidth;
  const clearance = node.getBoundingClientRect().bottom - node.querySelector('.terminal-tab').getBoundingClientRect().bottom;
  node.style.removeProperty('max-width');
  return {scrollable,clearance};
 });
 assert.ok(overflow.scrollable && overflow.clearance >= 8, 'the horizontal scrollbar has room below the terminal tabs');
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
 // A handoff in chat opens a PTY only after the owner reviews and accepts.
 const handoff={type:'terminal_handoff',status:'pending',name:'alpha',text:'/fixture/collections/alpha',providerDataJson:Buffer.from(JSON.stringify(['acme','login','team blue',''])).toString('base64')};
 // Reviewed before it runs: every character visible (an ESC is \e), the folder named from the workspace.
 const reviewed={...handoff,text:'/fixture/collections/alpha/api/scripts',providerDataJson:Buffer.from(JSON.stringify(['acme','login','team blue','','safe\u001b[2Kgone'])).toString('base64')};
 chatEvents.push({...reviewed,requestId:'declined'});
 const declined=page.getByRole('article',{name:'Interactive command'}).last();
 await expect(declined.locator('pre')).toHaveText("acme login 'team blue' '' $'safe\\e[2Kgone'");
 const directory=declined.getByText('Working directory · api/scripts');
 await expect(declined.getByText(reviewed.text,{exact:true})).toBeHidden();
 await directory.click();
 await expect(declined.getByText(reviewed.text,{exact:true})).toBeVisible();
 await declined.getByRole('button',{name:'Decline',exact:true}).click();
 await expect(declined).toContainText('Interactive command declined');
 await expect(panel).toHaveCount(0);
 chatEvents.push({...handoff,requestId:'accepted'});
 const accepted=page.getByRole('article',{name:'Interactive command'}).last();
 await expect(accepted.getByRole('button',{name:'Run and take over'})).toBeVisible();
 assert.equal(calls('DecideTerminalHandoff').length,1);
 await accepted.getByRole('button',{name:'Run and take over'}).click();
 await expect(screen).toContainText('Enter device code');
 await expect(screen.locator('textarea')).toBeFocused();
 await page.keyboard.type('fixture');
 await expect.poll(()=>fixture.terminals.get('interactive-accepted')?.input.join('')).toBe('fixture');
 chatEvents.push({...handoff,requestId:'accepted',status:'finished',text:'interactive-accepted'});
 await expect(accepted).toContainText('Interactive command finished');
 chatEvents.push({...handoff,requestId:'agent-input',terminalAgentInput:true,providerDataJson:Buffer.from(JSON.stringify(['example','deploy'])).toString('base64')});
 const shared=page.getByRole('article',{name:'Interactive command'}).last();
 await expect(shared.locator('pre')).toHaveText('example deploy');
 await expect(shared).toContainText('additional commands without asking again');
 const decided=calls('DecideTerminalHandoff').length;
 await shared.getByRole('button',{name:'Run with agent input'}).click();
 // The decision is sent after the click returns: wait for that request, then read it.
 await expect.poll(()=>calls('DecideTerminalHandoff').length).toBe(decided+1);
 assert.equal(calls('DecideTerminalHandoff').at(-1).body.allowAgentInput,true);
 await expect(shared).toContainText('Interactive command running');
 fixture.terminals.get('interactive-agent-input').done=true;
 await expect(shared).toContainText('Interactive command finished',{timeout:12000});
 assert.equal(calls('OpenTerminal').length,opened,'handoff reused the PTY started by the owner decision');
 assert.deepEqual(errors,[]);
 console.log('PASS terminal: an agent handoff requires approval, focuses the PTY and receives owner input; a wrapped sign-in URL is one link (pointer and underline on hover, a click opens the whole address, a drag does not, the right-click menu offers it), exit closes the tab and panel, header opens a shell docked right, focuses rather than duplicates, fitted size, ordered input, member directory, close, re-attach after reload, × close and reopen, dock via menu and keyboard, layout survives reload, remembered dock side, drag a tab below the chat with a drop preview, neighbours stay mounted, reopen from Panels, phone close.');
} finally {await browser?.close();await server.close();await fixture.close();}
