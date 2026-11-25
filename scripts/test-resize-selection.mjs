import {openWorkspaceChanges} from './fixtures/workspace-changes.mjs';
// Dragging a divider, an edge or a header never selects text, in WebKit (the
// desktop app's engine) and in Chromium, against the fake Runner. Every kind of
// drag handle is pressed with text selected beside it and dragged across text
// with the real mouse (down, moves in steps, up): the selection is gone from
// the press on and stays empty to the release, and the size or place changed.
// The handles: a panel's divider (Resizer: the Changes file list), the dock's
// dividers (the Workspaces sidebar's edge, between the chat and the command
// output), a dock tab dragged, a floating group's edge and title bar, the
// editor's split, a sub-agent panel's edge and header, and a dialog's header.
// Escape in the middle of a divider's drag puts it back and ends the drag.
import {createServer} from './fixtures/vite-cache.mjs';
import react from '@vitejs/plugin-react';
import {webkit,chromium,expect} from './fixtures/playwright.mjs';
import {fileURLToPath} from 'node:url';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import {testViteCache} from './fixtures/vite-cache.mjs';
import {monacoVimCompat} from './monaco-vim-compat.mjs';
import { collectPageErrors, drainPageErrors } from './fixtures/page-errors.mjs';

const fixture=await startRunnerFixture();
for(const id of ['alpha','beta']){
 const api=fixture.shipState(id,'api');
 api.files=[{path:'src/handler.go',indexStatus:' ',worktreeStatus:'M'}];
 fixture.diffs.set(`${id}/api/src/handler.go`,'diff --git a/src/handler.go b/src/handler.go\n--- a/src/handler.go\n+++ b/src/handler.go\n@@ -1,3 +1,3 @@\n package api\n-func Handle() {}\n+func Handle() error { return nil } // a line long enough to drag the pointer across its words\n \n');
}
// Prose in the conversation, and a sub-agent with a panel of its own.
const SUB='claude-tool:toolu_map';
Object.assign(fixture.sessions[0],{title:'Drag over text',provider:'claude',agents:[{id:'root',status:'running'},{id:SUB,parentId:'root',status:'running',name:'Sub-agent',kind:'local_agent',nativeId:'task-map'}]});
{
 const channel=fixture.openEvents('alpha-session'),b64=value=>Buffer.from(JSON.stringify(value)).toString('base64'),now=Date.now();
 const push=(offset,event)=>channel.push(event.data===undefined?event:{...Object.fromEntries(Object.entries(event).filter(([k])=>k!=='data')),providerDataJson:b64(event.data)},new Date(now+offset).toISOString());
 push(-9000,{type:'operation_accepted',text:'Explain why a drag selects text.'});
 push(-8900,{type:'turn_started'});
 const prose='Every word of this paragraph is text a careless drag would select: the pointer crosses it on its way from one edge of the page to the other, and nothing of it may turn blue. ';
 push(-8800,{type:'message_delta',text:prose.repeat(6)});
 push(-8000,{type:'tool_activity',data:{type:'tool_call',id:'toolu_map',name:'Task',status:'running',input:{description:'Map the selection',prompt:'Find where the selection starts.',subagent_type:'Explore'}}});
 push(-7800,{type:'agent_status',agentId:SUB,parentAgentId:'root',nativeId:'task-toolu_map',status:'running',name:'Map the selection',agentKind:'local_agent',data:{toolUseId:'toolu_map',subagentType:'Explore',prompt:'Find where the selection starts.'}});
 push(-7000,{type:'message_delta',agentId:SUB,parentAgentId:'root',text:prose.repeat(3)});
 push(-6000,{type:'message_delta',text:prose.repeat(4)});
}
const server=await createServer({...testViteCache(import.meta.url),configFile:false,root:fileURLToPath(new URL('../apps/web',import.meta.url)),logLevel:'warn',plugins:[react(),monacoVimCompat()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
await server.listen();
const base=`http://127.0.0.1:${server.httpServer.address().port}`;
const failures=[];

async function run(type,name){
 const browser=await type.launch({headless:true});
 try{
  const page=await browser.newPage({viewport:{width:1600,height:950}});page.setDefaultTimeout(15000);
  const errors=[];await collectPageErrors(page, errors);
  await page.goto(`${base}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`);
  const chat=page.locator('.embedded-chat:visible').first(),prose=chat.locator('.message').filter({hasText:'careless drag'}).first();
  await expect(prose).toBeVisible();
  // What is selected. A selection in text that cannot be selected right now (user-select: none
  // while a drag is on) is not shown by toString() in Chromium, but comes back after it: it counts.
  const selection=()=>page.evaluate(()=>{const s=getSelection();if(!s||!s.rangeCount||s.isCollapsed)return s?.toString()??'';return s.toString()||`(hidden) ${s.getRangeAt(0).toString()}`;});
  /** Selects all the text of `text`, as a person would have before reaching for the handle. */
  const select=async text=>{
   // Again if the text was drawn anew meanwhile (a diff's highlighting arrives after its lines).
   await expect.poll(()=>text.evaluate(node=>{const range=document.createRange();range.selectNodeContents(node);const s=getSelection();s.removeAllRanges();s.addRange(range);return s.toString().trim();}),{message:'text to select beside the handle'}).not.toBe('');
  };
  const centre=box=>({x:box.x+box.width/2,y:box.y+box.height/2});
  /** Where it is, once it is shown (a divider places itself after the layout settles). */
  const box=async locator=>{let found;await expect.poll(async()=>{found=await locator.boundingBox({timeout:1000}).catch(()=>null);return !!found&&found.width>0&&found.height>0;},{message:'shown'}).toBe(true);return found;};
  /**
   * One drag of a handle: text beside it selected first, then pressed at `from`
   * (the handle's centre unless given), moved by `by` in steps across text, and
   * released. `measure` reads its size or place before and after. A tab's drag
   * begins only once the pointer has moved a little (a press is a click until
   * then): `slop` checks the selection from there.
   */
  const gesture=async(what,{handle,from,by,text,measure,slop=false})=>{
   await expect(handle,`${what}: the handle`).toBeVisible();
   const start=from??centre(await box(handle));
   const before=await measure();
   await select(text);
   const problems=[];
   await page.mouse.move(start.x,start.y);await page.mouse.down();
   if(slop)await page.mouse.move(start.x+Math.sign(by.x)*12,start.y+Math.sign(by.y)*12,{steps:3});
   const pressed=await selection();if(pressed)problems.push(`${slop?'starting to drag':'pressing'} it kept the selection (${JSON.stringify(pressed.slice(0,30))}…)`);
   await page.mouse.move(start.x+by.x/2,start.y+by.y/2,{steps:8});
   const during=await selection();if(during)problems.push(`dragging selected ${JSON.stringify(during.slice(0,30))}…`);
   await page.mouse.move(start.x+by.x,start.y+by.y,{steps:8});
   await page.mouse.up();
   const after=await selection();if(after)problems.push(`after the release ${JSON.stringify(after.slice(0,30))}… is selected`);
   const changed=await expect.poll(measure,{timeout:3000}).not.toEqual(before).then(()=>true,()=>false);
   if(!changed)problems.push(`its size or place did not change (${JSON.stringify(before)})`);
   const leftover=await page.evaluate(()=>[...document.documentElement.classList].filter(c=>/dragging|resizing/.test(c)));
   if(leftover.length)problems.push(`the page is still marked ${leftover.join(' ')}`);
   if(problems.length)failures.push(`${name}, ${what}: ${problems.join('; ')}`);
   console.log(`${problems.length?'FAIL':'ok  '} ${name}: ${what}${problems.length?` — ${problems.join('; ')}`:''}`);
   await page.evaluate(()=>getSelection()?.removeAllRanges());
  };
  const rect=locator=>()=>locator.evaluate(n=>{const b=n.getBoundingClientRect();return [Math.round(b.x),Math.round(b.y),Math.round(b.width),Math.round(b.height)];});
  const width=locator=>()=>locator.evaluate(n=>Math.round(n.getBoundingClientRect().width));
  const height=locator=>()=>locator.evaluate(n=>Math.round(n.getBoundingClientRect().height));
  const proseY=async()=>centre(await box(prose)).y;

  // The dock's divider at the sidebar's edge, dragged right across the conversation.
  const sidebar=page.locator('.dv-groupview').filter({has:page.locator('[data-kind="collections"]')});
  const edge=page.getByRole('separator',{name:'Resize Workspaces sidebar'});
  {const b=await box(edge);await gesture('the Workspaces sidebar edge (dock divider)',{handle:edge,from:{x:b.x+b.width/2,y:await proseY()},by:{x:160,y:0},text:prose,measure:width(sidebar)});}
  // Back, across the workspace rows.
  {const b=await box(edge);await gesture('the Workspaces sidebar edge, back over its rows',{handle:edge,from:{x:b.x+b.width/2,y:centre(await box(page.locator('.workspace-row').first())).y},by:{x:-160,y:0},text:page.locator('.workspace-row').first(),measure:width(sidebar)});}

  // The sub-agent's panel, docked: its edge, dragged left across the conversation.
  await chat.getByRole('button',{name:'Open Map the selection in a panel'}).click();
  const panel=chat.getByRole('complementary',{name:'Sub-agent: Map the selection'});
  await expect(panel).toHaveAttribute('data-mode','dock');
  {const handle=panel.getByRole('separator',{name:'Resize sub-agent panel'});
   await gesture('the sub-agent panel\'s edge',{handle,from:{x:centre(await box(handle)).x,y:await proseY()},by:{x:-240,y:0},text:prose,measure:width(panel)});}
  // Floating: its header, dragged across the conversation.
  await panel.getByRole('button',{name:'Float the panel'}).click();
  await expect(panel).toHaveAttribute('data-mode','float');
  {const head=panel.locator('.subagent-panel-head'),b=await box(head);
   await gesture('the floating sub-agent panel\'s header',{handle:head,from:{x:b.x+40,y:b.y+b.height/2},by:{x:-300,y:200},text:prose,measure:rect(panel)});}
  await panel.locator('.subagent-panel-head').focus();await page.keyboard.press('Escape');await expect(panel).toHaveCount(0);

  // The dock's divider between the chat and the command output, dragged up across the conversation.
  await page.getByRole('tab',{name:'Command output',exact:true}).click();
  // Keep transcript text visible for the following tab-drag selection check.
  const inner=page.getByRole('separator',{name:/^Resize .* and Command output$/});
  const chatPanel=page.locator('[data-dock-panel]:has(> .panel-host > .panel-chat)');
  await gesture('the divider between the chat and the command output (dock divider)',{handle:inner,by:{x:0,y:-80},text:prose,measure:height(chatPanel)});

  // A dock tab dragged over the conversation to the chat's right edge: it splits there.
  {const tab=page.getByRole('tab',{name:'Command output',exact:true}),chatBox=await box(chatPanel),tabBox=await box(tab);
   const output=page.locator('[data-dock-panel]').filter({has:page.getByTestId('console-output')});
   await gesture('a dock tab dragged across the conversation',{handle:tab,by:{x:chatBox.x+chatBox.width-30-centre(tabBox).x,y:chatBox.y+chatBox.height/2-centre(tabBox).y},text:prose,measure:rect(output),slop:true});}

  // A floating group: its edge, and its title bar.
  {const tab=page.getByRole('tab',{name:'Command output',exact:true});
   await tab.click({button:'right'});await page.getByRole('menuitem',{name:'Float over the page'}).click();
   const floating=page.locator('.dv-resize-container').first();await expect(floating).toBeVisible();
   const right=floating.locator('.dv-resize-handle-right'),b=await box(floating);
   await gesture('a floating group\'s right edge',{handle:right,from:{x:b.x+b.width-1,y:b.y+b.height/2},by:{x:200,y:0},text:prose,measure:width(floating)});
   const title=floating.locator('.dv-floating-titlebar');
   // Its title bar drags the group: dropped on the conversation below it, it docks there.
   const output=page.locator('[data-dock-panel]').filter({has:page.getByTestId('console-output')}),start=centre(await box(title)),under=await box(floating);
   await gesture('a floating group\'s title bar',{handle:title,by:{x:0,y:under.y+under.height+60-start.y},text:prose,measure:rect(output)});
}

  // A panel's own divider (Resizer): the Changes file list, dragged right across the diff.
  await openWorkspaceChanges(page);
  const changes=page.getByRole('region',{name:'Workspace changes'});
  await changes.getByRole('navigation',{name:'Changed files'}).getByRole('button',{name:/src\/handler\.go/}).click();
  const line=changes.locator('.diff-view').getByText('a line long enough',{exact:false}).first();
  await expect(line).toBeVisible();
  {const handle=changes.getByRole('separator',{name:'Resize the file list'});
   const nav=width(changes.locator('.changes-columns>nav'));
   await gesture('a panel divider (the Changes file list)',{handle,from:{x:centre(await box(handle)).x,y:centre(await box(line)).y},by:{x:260,y:0},text:line,measure:nav});
   // Escape in the middle of a drag puts it back and ends the drag: the pointer no longer moves it, nothing is marked.
   const before=await nav(),at=centre(await box(handle));
   await page.mouse.move(at.x,at.y);await page.mouse.down();await page.mouse.move(at.x-120,at.y,{steps:6});
   assert.ok(await nav()<before-100,`${name}: the drag moved it`);
   await page.keyboard.press('Escape');
   assert.equal(await nav(),before,`${name}: Escape put the divider back`);
   assert.equal(await page.evaluate(()=>document.documentElement.classList.contains('is-dragging')),false,`${name}: Escape ended the drag`);
   await page.mouse.move(at.x-60,at.y,{steps:3});await page.mouse.up();
   assert.equal(await nav(),before,`${name}: after Escape the pointer does not move it`);
   await expect(changes,'Escape ended the drag, and did not close anything').toBeVisible();}

  // The editor's split, dragged left across the file shown in the first group.
  await page.getByRole('button',{name:'Browse files',exact:true}).click();
  const tree=page.getByRole('tree',{name:'Workspace files'});
  await tree.locator('[data-path="api/README.md"]').dblclick();
  const editor=page.locator('section.workspace-panel.panel-editor');
  await editor.getByRole('button',{name:'Split editor right'}).first().click();
  await expect(editor.locator('.editor-group')).toHaveCount(2);
  {const handle=editor.locator('.editor-split-handle').first(),group=editor.locator('.editor-group').first(),words=group.getByText('A retained file.');
   await expect(words).toBeVisible();
   await gesture('the editor split',{handle,from:{x:centre(await box(handle)).x,y:centre(await box(words)).y},by:{x:-260,y:0},text:words,measure:width(group)});}

  // A dialog's header, dragged down past the screen's edge: the pointer goes on over its text.
  await page.getByRole('button',{name:'Settings',exact:true}).click();
  await page.getByRole('button',{name:'Add subscription',exact:true}).click();
  const dialog=page.getByRole('dialog',{name:'Connect a subscription'});
  {const header=dialog.locator('[data-dialog-drag-handle]'),b=await box(header),words=dialog.locator('p').first();
   await gesture('a dialog\'s header',{handle:header,from:{x:b.x+35,y:b.y+b.height/2},by:{x:0,y:900},text:words,measure:rect(dialog)});}
  await page.keyboard.press('Escape');

  await drainPageErrors(errors);assert.deepEqual(errors,[],`${name}: page errors`);
 }finally{await browser.close();}
}

try{
 for(const [type,name] of [[webkit,'WebKit'],[chromium,'Chromium']])await run(type,name);
 assert.deepEqual(failures,[],'every drag left no text selected');
 console.log('PASS resize-selection: every divider, edge, dock tab, floating group, editor split, sub-agent panel and dialog header drags without selecting text (and clears a selection made before), in WebKit and Chromium.');
}finally{await server.close();await fixture.close();}
