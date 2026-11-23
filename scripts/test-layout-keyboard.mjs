// The dock from the keyboard alone (R6): ⌥⌘ arrows move the focus between
// panel groups (a ring shows where it is, a screen reader hears it, an edge
// with nothing beyond it nudges the ring); ⌥⇧⌘ arrows move the active panel
// into the neighbouring group, swap two groups, or split a tab out; ⌃⌘ arrows
// move the active group's edge (5 % a tap, smaller accelerating steps held
// down; the sidebar's edge is its one width); ⌃1–⌃9 pick a tab of the active
// group (⌘1–⌘9 stay workspaces); a group's + starts a conversation there or
// opens any panel there (⌘T for the active group); a maximized panel stays
// maximized while the focus moves, ⌃⇥ cycles and ⌘] changes the conversation;
// ⌘\ splits a tab out at once and asks what goes beside a lone panel;
// ⌃⌥⇧Esc, which cannot be rebound, resets the layout and the keys, and Undo
// brings both back. Every key is in the palette and the shortcuts overlay.
// WebKit (the desktop app's engine), the browser build, against the fake
// Runner fixture, dark and light.
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
// A second conversation in Alpha, so ⌘] has somewhere to go.
const first=fixture.sessions.find(s=>s.collectionId==='alpha');
fixture.sessions.push({...structuredClone(first),id:'alpha-second',workId:'alpha-second',title:'Second alpha conversation',instructionsPending:false});
const server=await createServer({...testViteCache(import.meta.url),configFile:false,root:fileURLToPath(new URL('../apps/web',import.meta.url)),logLevel:'warn',plugins:[react(),monacoVimCompat()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
await server.listen();
await mkdir('.cache',{recursive:true});
const url=`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`;

async function scenario(browser,scheme){
 const context=await browser.newContext({viewport:{width:1440,height:900},colorScheme:scheme});
 // The default arrangement with one chat (the preview tab): "Open a workspace's conversations as tabs" off; test-conversation-tabs covers it on.
 await context.addInitScript(()=>{try{localStorage.setItem('lazybox:workspace.conversation-tabs','off');}catch{/* a popup without storage */}});
 const page=await context.newPage();page.setDefaultTimeout(15000);
 const errors=[];page.on('pageerror',e=>errors.push(String(e)));
 const shot=name=>page.screenshot({path:`.cache/layout-keyboard-${name}-${scheme}.png`});
 await page.goto(url);
 const tab=kind=>page.locator('.dv-tab').filter({has:page.locator(`[data-kind="${kind}"]`)});
 // A typed-in conversation stays in its tab, so ⌘] opens the next one in a preview tab beside it: the chat is the one in view.
 const content=kind=>{const all=page.locator(`[data-dock-panel]:has(> .panel-host > .panel-${kind})`);return kind==='chat'?all.filter({visible:true}):all;};
 const titles=()=>page.locator('.dv-tab .dock-tab-title');
 const resetTitles=()=>page.locator('.dv-tab .dock-tab:not([data-kind="chat"]) .dock-tab-title');
 const chatGroups=()=>page.locator('.dv-groupview').filter({has:page.locator('.dock-tab[data-kind="chat"]')});
 const composer=page.locator('.page-layout .panel-chat').filter({visible:true}).getByRole('textbox',{name:'Message',exact:true});
 const said=page.getByTestId('dock-announcer');
 const ring=page.locator('.dock-focus-ring');
 const key=async combo=>{await page.keyboard.press(combo);await page.waitForTimeout(60);};
 /** The kinds of the active group's tabs, front one first. */
 const active=()=>page.evaluate(()=>{const g=document.querySelector('.dv-groupview.dv-active-group');if(!g)return '';const tabs=[...g.querySelectorAll('.dv-tab')];const front=g.querySelector('.dv-tab.dv-active-tab [data-kind]')?.dataset.kind;return [front,...tabs.map(t=>t.querySelector('[data-kind]')?.dataset.kind).filter(k=>k!==front)].join(',');});
 /** The kind of panel that has the keyboard focus. */
 const focused=()=>page.evaluate(()=>{const m=/\bpanel-([a-z-]+)\b/.exec(document.activeElement?.closest('.workspace-panel')?.className||'');return m?m[1]:'';});
 const width=kind=>content(kind).evaluate(n=>Math.round(n.getBoundingClientRect().width));
 const height=kind=>content(kind).evaluate(n=>Math.round(n.getBoundingClientRect().height));
 /** The ring sits on the active group. */
 const ringed=async()=>{const r=await ring.boundingBox(),g=await page.locator('.dv-groupview.dv-active-group').boundingBox();return !!r&&!!g&&Math.abs(r.x-g.x)<=2&&Math.abs(r.y-g.y)<=2&&Math.abs(r.width-g.width)<=2&&Math.abs(r.height-g.height)<=2;};
 await expect(composer).toBeEnabled();
 await expect(titles()).toHaveText(['Workspaces','Agents','Agent chat','Command output']);
 await expect(ring).toHaveCount(0);

 // ---- Focus: ⌥⌘ arrows, a ring, a voice, a nudge at the edge.
 await composer.click();await composer.fill('draft kept through the keyboard');
 await key('Meta+Alt+ArrowLeft');
 await expect.poll(active).toMatch(/^collections/);
 await expect.poll(focused).toBe('collections');
 await expect(said).toHaveText(/Workspaces/);
 await expect(ring).toBeVisible();await expect.poll(ringed).toBe(true);
 await shot('focus-ring');
 await key('Meta+Alt+ArrowLeft');
 await expect(said).toHaveText(/No panel group to the left/);
 await expect(ring).toHaveAttribute('data-bump','left');
 await key('Meta+Alt+ArrowRight');
 await expect.poll(focused).toBe('chat');
 await expect(composer).toBeFocused();
 await expect(said).toHaveText(/Agent chat/);
 await expect.poll(ringed).toBe(true);
 // Down reaches the minimized command output: it opens as the focus arrives.
 await expect(page.locator('.dock-group-minimized')).toHaveCount(1);
 await key('Meta+Alt+ArrowDown');
 await expect.poll(active).toMatch(/^console/);
 await expect(page.locator('.dock-group-minimized')).toHaveCount(0);
 await expect.poll(()=>height('console')).toBeGreaterThan(120);
 await expect.poll(ringed).toBe(true);
 await key('Meta+Alt+ArrowUp');
 await expect.poll(focused).toBe('chat');
 // The first click puts the ring away.
 await composer.click();await expect(ring).toHaveCount(0);
 await expect(composer).toHaveValue('draft kept through the keyboard');

 // ---- The + after a group's tabs: a conversation, or any panel, here.
 const add=page.locator('.dock-group-add').filter({has:page.locator('summary')});
 assert.equal(await page.locator('.dv-groupview').filter({has:tab('collections')}).locator('.dock-group-add summary').count(),0,'the sidebar has no +');
 const chatGroup=page.locator('.dv-groupview').filter({has:tab('chat')});
 const plus=chatGroup.locator('.dock-group-add summary');
 await expect(plus).toHaveAttribute('aria-label','New tab in the Agent chat group');
 await plus.click();
 const menu=page.getByRole('group',{name:'New tab in the Agent chat group menu'});
 await expect(menu.getByRole('button',{name:/New conversation/})).toBeFocused();
 await expect(menu.getByRole('button',{name:'Agent chat (already here)'})).toBeDisabled();
 await expect(menu.getByRole('button',{name:'Move Command output here'})).toBeVisible();
 await shot('plus-menu');
 await menu.getByRole('button',{name:'Open Changes here'}).click();
 await expect.poll(()=>chatGroup.locator('.dv-tab').count()).toBe(2);
 await expect(chatGroup.locator('.dv-tab.dv-active-tab [data-kind]')).toHaveAttribute('data-kind','changes');
 await expect(said).toHaveText(/Opened Changes here/);
 // ⌘T opens the active group's + from the keyboard.
 await expect.poll(focused).toBe("changes");
 await key('Meta+t');
 const addMenu=page.getByRole('group',{name:'New tab in the Changes group menu'});
 await expect(addMenu).toBeVisible();
 await expect(addMenu.getByRole('button',{name:/New conversation/})).toBeFocused();
 await key('Escape');
 await expect(addMenu).toBeHidden();
 assert.ok(await add.count()>=2);

 // ---- Tabs: ⌃1–⌃9 in the active group; ⌃9 is the last; ⌘1–⌘9 stay workspaces.
 await key("Control+1");await expect(composer).toBeFocused();
 await key('Control+2');
 await expect.poll(active).toMatch(/^changes/);
 await expect(said).toHaveText('Tab 2 of 2: Changes');
 await key('Control+1');
 await expect.poll(active).toMatch(/^chat/);
 await expect(composer).toBeFocused();
 await key('Control+9');
 await expect.poll(active).toMatch(/^changes/);
 await key('Control+5');
 await expect(said).toHaveText(/has 2 tabs/);
 // The sidebar is Workspaces above Agents, each its own group: ⌥⌘ ↑ ↓ move between them.
 await key('Meta+Alt+ArrowLeft');
 await expect.poll(active).toMatch(/^(collections|agents)/);
 await key('Meta+Alt+ArrowUp');
 await expect.poll(active).toMatch(/^collections/);
 await key('Meta+Alt+ArrowDown');
 await expect.poll(active).toMatch(/^agents/);
 await key('Meta+Alt+ArrowUp');
 await expect.poll(active).toMatch(/^collections/);

 // ---- Move: a tab splits out where there is no group, joins a neighbour, and a lone panel swaps.
 await key('Meta+Alt+ArrowRight');
 await key('Control+2');
 await expect.poll(active).toMatch(/^changes/);
 await key('Meta+Alt+Shift+ArrowRight');
 await expect(said).toHaveText(/Moved Changes to a new group on the right/);
 await expect.poll(async()=>{const c=await content('chat').boundingBox(),d=await content('changes').boundingBox();return !!c&&!!d&&d.x>=c.x+c.width-4;},{message:'Changes is right of the chat'}).toBe(true);
 await expect.poll(active).toBe('changes');
 const before={chat:await content('chat').boundingBox(),changes:await content('changes').boundingBox()};
 await key('Meta+Alt+Shift+ArrowLeft');
 await expect(said).toHaveText(/Swapped Changes with Agent chat/);
 await expect.poll(async()=>{const c=await content('chat').boundingBox(),d=await content('changes').boundingBox();return !!c&&!!d&&c.x>d.x&&Math.abs(d.x-before.chat.x)<=2;},{message:'the two groups traded places'}).toBe(true);
 await expect.poll(active).toBe('changes');
 await key('Meta+Alt+Shift+ArrowRight');
 await expect.poll(async()=>{const c=await content('chat').boundingBox();return !!c&&Math.abs(c.x-before.chat.x)<=2;}).toBe(true);
 await key('Meta+Alt+Shift+ArrowRight');
 await expect(said).toHaveText(/already at the right edge/);
 // Into the sidebar, never: a lone panel beside it stays.
 await key('Meta+Alt+ArrowLeft');
 await expect.poll(active).toMatch(/^chat/);
 await key('Meta+Alt+Shift+ArrowLeft');
 await expect(said).toHaveText(/cannot go into the sidebar/);
 await expect(titles()).toHaveText(['Workspaces','Agents','Agent chat','Changes','Command output']);
 await expect(composer).toHaveValue('draft kept through the keyboard');
 await shot('moved');

 // ---- Resize: ⌃⌘ arrows move the active group's edge, tmux style; 5 % a tap; held down, small accelerating steps.
 const share=(1440-280)*0.05;
 assert.equal(await width('collections'),280);
 // Held down on the chat: the first press is a tap, then each repeat is 1 % rising (far less than a tap each).
 const chatWidth=await width('chat');
 await page.keyboard.down('Control');await page.keyboard.down('Meta');
 for(let i=0;i<12;i++){await page.keyboard.down('ArrowRight');await page.waitForTimeout(30);}
 await page.keyboard.up('ArrowRight');await page.keyboard.up('Meta');await page.keyboard.up('Control');
 await expect.poll(()=>width('chat')).toBeGreaterThan(chatWidth+2*share);
 const held=await width('chat');
 assert.ok(held<chatWidth+6*share,`held steps are small (${chatWidth} → ${held}; twelve taps would be ${Math.round(12*share)} wider)`);
 await page.waitForTimeout(400);
 await expect(said).toHaveText(/Agent chat, \d+% of the width/);
 assert.equal(await width('collections'),280,'the sidebar keeps its width');
 // Three taps on Changes' left edge: three 5 % steps, the chat gives the room.
 await key('Meta+Alt+ArrowRight');
 await expect.poll(active).toBe('changes');
 const changesWidth=await width('changes');
 for(let i=0;i<3;i++)await key('Control+Meta+ArrowLeft');
 await expect.poll(()=>width('changes')).toBeGreaterThan(changesWidth+3*share-12);
 assert.ok(await width('changes')<changesWidth+3*share+12,'three taps are three 5 % steps');
 await expect(said).toHaveText(/Changes, \d+% of the width/);
 await expect.poll(ringed).toBe(true);
 await shot('resized');
 // The chat stops at its minimum width: the edge says so, and nudges.
 for(let i=0;i<8;i++)await key('Control+Meta+ArrowLeft');
 assert.ok(await width('chat')>=358,'the chat keeps its minimum width');
 await expect(said).toHaveText(/cannot move that edge further to the left/);
 // Above the command output: ↓ moves the edge between them.
 await key('Meta+Alt+ArrowDown');
 await expect.poll(active).toBe('console');
 const outputHeight=await height('console');
 await key('Control+Meta+ArrowUp');
 await expect.poll(()=>height('console')).toBeGreaterThan(outputHeight+20);
 await expect(said).toHaveText(/Command output, \d+% of the height/);
 // The sidebar: its edge changes its one width (a tap is 5 % of the page), and it is remembered.
 // Left of the command output is Agents, the sidebar's lower half.
 await key('Meta+Alt+ArrowLeft');
 await expect.poll(active).toMatch(/^agents/);
 await key('Control+Meta+ArrowRight');
 await expect.poll(()=>width('collections')).toBe(352);
 await expect(said).toHaveText('Workspaces sidebar, 352 pixels wide');
 await key('Control+Meta+ArrowLeft');
 await expect.poll(()=>width('collections')).toBe(280);
 // Nothing to the left of the sidebar: its right edge moves left, down to its minimum.
 await key('Control+Meta+ArrowLeft');
 await expect.poll(()=>width('collections')).toBe(208);
 await key("Control+Meta+ArrowLeft");await expect.poll(()=>width("collections")).toBe(200);await key("Control+Meta+ArrowLeft");
 await expect(said).toHaveText(/as narrow as it goes, 200 pixels/);
 await key('Control+Meta+ArrowRight');
 await expect.poll(()=>width('collections')).toBe(272);

 // ---- Maximize: the focus, ⌃⇥ and ⌘] keep it maximized; the zoom moves with the focus.
 await composer.click();
 await expect.poll(focused).toBe('chat');
 await key('Meta+Shift+m');
 await expect(content('changes')).toBeHidden();
 await expect(content('collections')).toBeHidden();
 await key('Meta+Alt+ArrowRight');
 await expect(content('changes')).toBeVisible();
 await expect(content('chat')).toBeHidden();
 await expect(said).toHaveText(/Changes, maximized/);
 await expect(page.getByRole('button',{name:'Restore layout'})).toBeVisible();
 await shot('maximized-moved');
 await key('Meta+Alt+ArrowLeft');
 await expect(content('chat')).toBeVisible();
 await expect(content('changes')).toBeHidden();
 await key('Control+Tab');
 await expect.poll(async()=>await content('chat').isHidden()&&!await page.locator('.dv-groupview.dv-active-group').isHidden()).toBe(true);
 await expect(page.getByRole('button',{name:'Restore layout'})).toBeVisible();
 // With Changes maximized, the next conversation (⌘]) shows maximized, in the chat.
 await expect(content('changes')).toBeVisible();
 await key('Meta+]');
 await expect(content('chat')).toBeVisible();
 await expect(content('changes')).toBeHidden();
 await expect(content('chat')).not.toContainText('Retained conversation for alpha-session');
 await expect(page.getByRole('button',{name:'Restore layout'})).toBeVisible();
 await key('Meta+[');
 await expect(content('chat')).toBeVisible();
 await expect(content('collections')).toBeHidden();
 await expect(page.getByRole('button',{name:'Restore layout'})).toBeVisible();
 // Resizing a maximized group is explained, not ignored.
 await key('Control+Meta+ArrowRight');
 await expect(said).toHaveText(/maximized: restore the layout/);
 await key('Meta+Shift+m');
 await expect(content('collections')).toBeVisible();
 await expect(content('changes')).toBeVisible();

 // ---- Every key is in the palette and the overlay, which lists the reset chord as one that cannot change.
 await page.locator('.workspace-brand').click();
 await key('Meta+k');
 const palette=page.getByRole('dialog',{name:/Command palette/});
 await palette.getByRole('combobox').fill('focus the panel group');
 await expect(palette.getByRole('option',{name:/Focus the panel group to the left/})).toBeVisible();
 await expect(palette.getByRole('option',{name:/Focus the panel group to the left/})).toContainText('⌥⌘←');
 await palette.getByRole('combobox').fill('reset the layout and');
 await expect(palette.getByRole('option',{name:/Reset the layout and shortcuts/})).toContainText('⌃⌥⇧Esc');
 await key('Escape');
 await key('Meta+/');
 const overlay=page.getByRole('dialog',{name:'Keyboard shortcuts'});
 await overlay.getByRole('searchbox').fill('tab');
 await expect(overlay.locator('[data-action="layout.tab"]')).toContainText('Go to tab 1–9');
 await expect(overlay.locator('[data-action="layout.tab"] kbd')).toHaveText(['⌃1','⌃9']);
 await overlay.getByRole('searchbox').fill('');
 await expect(overlay.locator('[data-action="layout.reset-all"]')).toContainText('always');
 await expect(overlay.locator('[data-action="layout.resize-right"] kbd')).toHaveText(['⌃⌘→']);
 await expect(overlay.locator('[data-action="layout.move-up"] kbd')).toHaveText(['⌥⇧⌘↑']);
 await expect(overlay.locator('footer')).toContainText('⌃⌥⇧Esc');
 await shot('overlay');
 await key('Escape');

 // ---- ⌘\ splits: a lone panel asks what goes beside it; a tab of a group splits out at once.
 // ⌘] showed the next conversation in a preview tab beside the typed-in one: closed, the chat is alone again.
 await page.locator('.dock-tab[data-kind="chat"][data-preview] .dock-tab-close').click();
 await expect(page.locator('.dock-tab[data-kind="chat"]')).toHaveCount(1);
 await composer.click();
 await key('Meta+\\');
 await expect(page.locator('.panels-menu .action-menu-heading')).toHaveText('Open beside Agent chat');
 await page.getByRole('button',{name:'Show Files'}).click();
 await expect.poll(async()=>{const c=await content('chat').boundingBox(),f=await content('files').boundingBox();return !!c&&!!f&&f.x>=c.x+c.width-4&&Math.abs(f.y-c.y)<4;},{message:'Files opened beside the chat'}).toBe(true);
 const filesGroup=page.locator('.dv-groupview').filter({has:tab('files')});
 await filesGroup.locator('.dock-group-add summary').click();
 await page.getByRole('group',{name:'New tab in the Files group menu'}).getByRole('button',{name:'Open Editor here'}).click();
 await expect.poll(()=>filesGroup.locator('.dv-tab').count()).toBe(2);
 await expect.poll(active).toMatch(/^editor/);
 await key('Meta+\\');
 await expect.poll(async()=>{const f=await content('files').boundingBox(),e=await content('editor').boundingBox();return !!f&&!!e&&e.x>=f.x+f.width-4;},{message:'the Editor split out right of Files'}).toBe(true);
 await expect(page.locator('.panels-menu[open]')).toHaveCount(0);
 // The + starts a conversation in its group: it shows there, in front.
 const editorGroup=page.locator('.dv-groupview').filter({has:tab('editor')});
 await editorGroup.locator('.dock-group-add summary').click();
 await page.getByRole('group',{name:'New tab in the Editor group menu'}).getByRole('button',{name:/New conversation/}).click();
 const dialog=page.getByRole('dialog',{name:'New conversation',exact:true});
 await expect(dialog).toBeVisible();
 const opened=fixture.mutations.filter(m=>m.method==='OpenSession').length;
 await dialog.getByRole('button',{name:/^Start /}).first().click();
 await expect(dialog).not.toBeVisible();
 await expect.poll(()=>editorGroup.locator('.dv-tab [data-kind]').evaluateAll(tabs=>tabs.map(t=>t.dataset.kind).join(','))).toBe('editor,chat');
 await expect(editorGroup.locator('.dv-tab.dv-active-tab [data-kind]')).toHaveAttribute('data-kind','chat');
 assert.equal(fixture.mutations.filter(m=>m.method==='OpenSession').length,opened+1,'one conversation started');
 const arranged=await titles().allTextContents();

 // ---- The reset chord: the default layout and keys, at once; Undo puts both back.
 await page.getByRole('button',{name:'Settings',exact:true}).click();
 await page.getByRole('tab',{name:'Shortcuts'}).click();
 const inbox=page.getByRole('textbox',{name:'Open the inbox',exact:true});
 await inbox.fill('] ] x');
 const resetField=page.getByRole('textbox',{name:'Reset the layout and shortcuts',exact:true});
 await expect(resetField).toHaveAttribute('readonly','');
 await expect(resetField).toHaveValue('Ctrl+Alt+Shift+Escape');
 await page.getByRole('button',{name:'Save shortcuts'}).click();
 await expect(page.getByRole('status').filter({hasText:'Shortcuts saved.'})).toBeVisible();
 // From Settings, too: the chord closes it and shows the reset page.
 await key('Control+Alt+Shift+Escape');
 // The default layout, keeping every conversation tab (the + started one) in its one chat group.
 await expect(resetTitles()).toHaveText(['Workspaces','Agents','Command output']);
 await expect(chatGroups()).toHaveCount(1);
 await expect(page.locator('.dock-group-minimized')).toHaveCount(1);
 const toast=page.locator('.toast').filter({hasText:'Layout and shortcuts reset'});
 await expect(toast).toBeVisible();
 await expect(toast.getByRole('button',{name:'Undo'})).toBeVisible();
 await shot('reset-toast');
 const keymap=()=>page.evaluate(()=>{for(let i=0;i<localStorage.length;i++){const k=localStorage.key(i);if(/keyboard:v2$/.test(k))return JSON.parse(localStorage.getItem(k))['inbox.open'];}return null;});
 await expect.poll(keymap).toBe('] ] i');
 await toast.getByRole('button',{name:'Undo'}).click();
 // Undo goes back to Settings, where the chord was pressed, with the arrangement restored.
 await expect(page.getByRole('tab',{name:'Shortcuts'})).toBeVisible();
 await page.getByRole('navigation',{name:'Pages'}).getByRole('button',{name:'Work',exact:true}).click();
 await expect(titles()).toHaveText(arranged);
 await expect.poll(keymap).toBe('] ] x');
 await expect(toast).toHaveCount(0);
 // And again with the arrangement reset and kept so (no Undo).
 await key('Control+Alt+Shift+Escape');
 await expect(resetTitles()).toHaveText(['Workspaces','Agents','Command output']);
 await expect(chatGroups()).toHaveCount(1);
 await expect.poll(keymap).toBe('] ] i');
 await expect(composer).toBeEnabled();

 // Saved keys are keys.yaml (POWER.md, "Config files"): saving them, the reset chord and its Undo write that file, nothing else.
 assert.ok(fixture.mutations.filter(m=>m.method==='UpdateConfigFile').every(m=>m.body.name==='keys'),'only keys.yaml is written');
 assert.equal(fixture.mutations.filter(m=>!['OpenTerminal','ResizeTerminal','WriteTerminal','OpenSession','UpdateConfigFile'].includes(m.method)).length,0,'arranging panels never starts or stops an agent (the one started from a + aside)');
 assert.deepEqual(errors,[]);
 await context.close();
}

let browser;
try{
 browser=await webkit.launch({headless:true});
 for(const scheme of ['dark','light'])await scenario(browser,scheme);
 console.log('PASS layout keyboard: ⌥⌘ arrows focus each neighbour with a ring, a screen-reader line and an edge nudge (minimized groups open); ⌥⇧⌘ arrows split a tab out, swap two groups and refuse the sidebar; ⌃⌘ arrows resize by 5 % taps and smaller accelerating held steps, the sidebar by its one width; ⌃1–⌃9 pick tabs (⌃9 the last); the + menu and ⌘T open any panel in that group, and a conversation started from a + shows there; ⌘\\ splits a tab out or asks what goes beside a lone panel; maximize survives focus moves, ⌃⇥ and ⌘]/⌘[; palette and overlay list every key; ⌃⌥⇧Esc resets layout and keys from anywhere and Undo restores both. WebKit, fixture Runner, dark and light.');
}finally{await browser?.close();await server.close();await fixture.close();}
