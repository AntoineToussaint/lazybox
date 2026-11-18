// Fixture only: the compact chat header, live status, prompt recall, snippet
// usage, equal workspace cards, the workspace home and the naming flow.
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from 'vite';import react from '@vitejs/plugin-react';import {webkit,expect} from './fixtures/playwright.mjs';import {fileURLToPath} from 'node:url';import {mkdir} from 'node:fs/promises';import assert from 'node:assert/strict';import {startRunnerFixture} from './fixtures/runner-ui.mjs';
const f=await startRunnerFixture();
const repos=['authority','cli','core','module-datasource','web-console'];
for(const [i,name] of repos.entries())f.library.push({id:'r'+i,name,path:'/fixture/'+name,gitIdentity:'github:codefly-dev/'+name});
const member=(i,collection,branch)=>({spec:{name:repos[i],repositoryId:'r'+i,baseRef:'main',branch},path:`/fixture/collections/${collection}/${repos[i]}`,ready:true});
f.collections.push({id:'gamma',name:'authority +4',workId:'gamma',root:'/fixture/collections/gamma',state:'ready',members:[0,1,2,3,4].map(i=>member(i,'gamma','feature/authority'))});
f.collections.push({id:'solo',name:'gentle-copper-otter',workId:'solo',root:'/fixture/collections/solo',state:'ready',members:[member(1,'solo','feature/gentle-copper-otter')]});
const now=Date.now();
Object.assign(f.sessions[0],{title:'Fix the flaky login test',provider:'claude',model:'claude-fable-5-1',modelReported:true,busy:true,status:'running',agents:[{id:'root',status:'running',phase:'tool',tool:'Bash',phaseSinceUnixMillis:String(now-12000),turnStartedAtUnixMillis:String(now-40000)}]});
// A conversation that opened and never ran a turn, titled after its workspace folder.
Object.assign(f.sessions[1],{title:'beta',provider:'claude',status:'starting',agents:[{id:'root',status:'starting'}]});
const alpha=f.openEvents('alpha-session');alpha.push({type:'operation_accepted',text:'The login test is flaky, can you find out why?'});alpha.push({type:'message_delta',text:'Let me look at the test and the CI logs.'},new Date(now-150_000).toISOString());alpha.push({type:'activity',status:'tool',name:'Bash'});
const beta=f.openEvents('beta-session');beta.push({type:'runner_reattached'});beta.push({type:'runner_reattached'});
// Two conversations, each waiting for an approval, to show side by side.
f.collections.push({id:'delta',name:'approvals',workId:'delta',root:'/fixture/collections/delta',state:'ready',members:[member(2,'delta','feature/approvals')]});
const approvalOf=(id,command)=>({type:'approval',requestId:id,text:'Bash',providerDataJson:Buffer.from(JSON.stringify({tool:'Bash',toolUseId:`use-${id}`,input:{command,description:'Run the checks'}})).toString('base64')});
for(const [id,title,command] of [['delta-left','Left approval','npm test'],['delta-right','Right approval','npm run lint']]){
 const request=approvalOf(`${id}-request`,command);
 f.sessions.push({...structuredClone(f.sessions[1]),id,workId:id,collectionId:'delta',title,provider:'claude',busy:true,status:'waiting_approval',pending:[request],agents:[{id:'root',status:'waiting_approval'}]});
 const ch=f.openEvents(id);ch.push({type:'operation_accepted',text:`Run ${command}`});ch.push(request);
}
const server=await createServer({...testViteCache(import.meta.url),configFile:false,logLevel:'error',root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:f.url}}}});let browser;
try{
 await server.listen();await mkdir('.cache',{recursive:true});browser=await webkit.launch();const page=await browser.newPage({viewport:{width:1440,height:900}});page.setDefaultTimeout(15000);
 // Leaving a page (a goto below) cuts its in-flight polls, which WebKit reports as page errors
 // ("access control checks") on the page being left: not failures. Anything else is.
 const errors=[];let leaving=0;page.on('pageerror',e=>{const text=String(e);if(leaving&&/access control checks/.test(text))return;errors.push(text);});
 const leave=async(go,ready)=>{leaving++;try{await go();await ready();}finally{leaving--;}};
 const base=`http://127.0.0.1:${server.httpServer.address().port}`;
 await page.goto(`${base}/?view=workspace&collection=alpha&session=alpha-session#token=${f.token}`);

 // One header: title, who and how, and the tools; no second or third bar.
 const chat=page.locator('.embedded-chat:visible').first(),header=chat.locator('.chat-heading');
 await expect(header.locator('.conversation-title')).toContainText('Fix the flaky login test');
 const reply=chat.locator('article.message_delta').filter({hasText:'Let me look at the test'});
 const replyTime=reply.getByRole('button',{name:/Show exact time/});
 await expect(chat.locator('.message-day').first()).toBeVisible();
 await expect(replyTime).toHaveCSS('opacity','0');
 await reply.hover();
 await expect(replyTime).toHaveCSS('opacity','1');
 await replyTime.focus();
 await page.keyboard.press('Enter');
 await expect(replyTime).toHaveAttribute('aria-pressed','true');
 assert.equal(await replyTime.textContent(),await replyTime.getAttribute('title'));
 // The model by the name people say; its id stays in the tooltip.
 await expect(header.locator('.chat-model')).toHaveText(/Claude · Fable 5\.1/);await expect(header.locator('.chat-model')).toHaveAttribute('title',/claude-fable-5-1/);
 await expect(header.locator('.chat-status')).toContainText('Running a command');
 await expect(chat.locator('.agent-inspection-bar')).toHaveCount(0);
 // No usage reported yet: no Context meter, not an empty grey bar (R12).
 await expect(header.locator('[aria-label="Context and token cost"]')).toHaveAttribute('data-empty','true');
 await expect(header.locator('[aria-label="Context and token cost"]'),'Context stays out of the way until there is usage').toBeHidden();
 await expect(header.locator('[aria-label="View tools and activity"]')).toHaveAttribute('data-empty','true');
 await expect(header.locator('[aria-label="View tools and activity"]'),'Activity stays out of the way until there is some').toBeHidden();
 {const h=(await header.boundingBox()).height;assert.ok(h<=64,`the header is one compact row: ${h}`);}
 assert.equal(await chat.locator('.conversation-title').evaluate(n=>n.closest('.chat-heading')!==null),true,'the title lives in the header');
 assert.equal(await header.locator('.chat-status .live-indicator').evaluate(n=>getComputedStyle(n).animationName),'live-spin','a working agent turns');

 // The sidebar row moves with the agent: its step and a ticking time.
 const alphaRow=page.locator('.workspace-row').filter({hasText:'Alpha feature'});
 const rowStatus=alphaRow.locator('.workspace-agent-status');
 await expect(rowStatus).toHaveText(/^Running a command · \d+s$/);
 const first=await rowStatus.textContent();await expect.poll(()=>rowStatus.textContent(),{timeout:4000}).not.toBe(first);
 await page.screenshot({path:'.cache/chat-polish-header.png'});
 await page.emulateMedia({reducedMotion:'reduce'});
 assert.equal(await header.locator('.chat-status .live-indicator').evaluate(n=>getComputedStyle(n).animationName),'none','reduced motion stops the indicator');
 await page.emulateMedia({reducedMotion:'no-preference'});

 // The inbox says the same thing for a working conversation.
 // The Inbox is the docked Agents panel now (audit R1): its working row says the same.
 await page.getByRole('button',{name:/^Inbox/}).click();const agentsPanel=page.locator('.agents-panel');
 await expect(agentsPanel.locator('li[data-state=working]').first()).toContainText(/Running a command/);

 // Rename in place: Enter saves.
 await header.getByRole('button',{name:'Rename conversation',exact:true}).click();
 const nameBox=header.getByRole('textbox',{name:'Conversation name',exact:true});await expect(nameBox).toBeFocused();
 await nameBox.fill('Login flake');await nameBox.press('Enter');
 await expect(header.locator('.conversation-title')).toContainText('Login flake');
 assert.equal(f.mutations.findLast(m=>m.method==='RenameConversation').body.title,'Login flake');
 await header.getByRole('button',{name:'Rename conversation',exact:true}).click();await header.getByRole('textbox',{name:'Conversation name'}).fill('Discarded');await page.keyboard.press('Escape');
 await expect(header.locator('.conversation-title')).toContainText('Login flake');

 // ↑/↓ recall what was sent, newest first, and give the draft back.
 const message=chat.getByRole('textbox',{name:'Message',exact:true});
 for(const text of ['first prompt','second prompt']){await message.fill(text);await message.press('Enter');await expect(message).toHaveValue('');}
 await message.focus();await message.press('ArrowUp');await expect(message).toHaveValue('second prompt');
 await message.press('ArrowUp');await expect(message).toHaveValue('first prompt');
 await message.press('ArrowUp');await expect(message).toHaveValue('The login test is flaky, can you find out why?');
 await message.press('ArrowDown');await expect(message).toHaveValue('first prompt');
 await message.press('ArrowDown');await expect(message).toHaveValue('second prompt');
 // What is typed or selected right after a recall keeps its caret and
 // selection: nothing recall deferred to a later frame may move them. Frames
 // are held across each step so that interleaving happens every run.
 const holdFrames=()=>page.evaluate(()=>{const held=[];window.__heldFrames=held;window.__requestFrame=window.requestAnimationFrame;window.requestAnimationFrame=cb=>held.push(cb);});
 const releaseFrames=()=>page.evaluate(()=>{window.requestAnimationFrame=window.__requestFrame;for(const cb of window.__heldFrames.splice(0))cb(performance.now());});
 await holdFrames();
 await message.press('ArrowDown');await expect(message).toHaveValue('');
 await message.fill('half a draft');
 await releaseFrames();
 assert.equal(await message.evaluate(n=>n.selectionStart),'half a draft'.length,'typing after a recall keeps its caret');
 await message.press('ArrowUp');await expect(message).toHaveValue('half a draft','a draft is never replaced from the middle');
 await message.press('Home');await message.press('ArrowUp');await expect(message).toHaveValue('second prompt');
 await holdFrames();
 await message.press('ArrowDown');await expect(message).toHaveValue('half a draft');
 await message.evaluate(n=>n.select());
 await releaseFrames();
 await message.press('Backspace');await expect(message,'a selection made after a recall is kept').toHaveValue('');

 // Sending keeps the keyboard in the message box: WebKit blurred it while it was disabled for the send.
 await message.fill('Look at the retry loop too.');await message.press('Enter');
 await expect(message).toHaveValue('');await expect(message).toBeFocused();
 // Text another panel adds while a send is in flight (Quote in reply) waits for it: the send no longer erases it.
 f.sessions[0].applyDelayMs=1500;
 await message.fill('And the timeout.');await message.press('Enter');
 await expect(message).toHaveAttribute('aria-disabled','true');await expect(message).toBeFocused();
 await chat.getByText('Let me look at the test and the CI logs.').click({button:'right'});
 await page.getByRole('menuitem',{name:'Quote in reply'}).click();
 await expect(message).toHaveValue(/^> Let me look at the test and the CI logs\./,{timeout:8000});
 await expect(message).not.toHaveAttribute('aria-disabled','true');
 f.sessions[0].applyDelayMs=0;
 await message.fill('');
 // A failure says what happened in words, never the raw Connect error.
 f.sessions[0].turnOver=true;
 await chat.locator('.live-bar-stop').click();
 await expect(chat.getByRole('alert').filter({hasText:'The agent is not working on anything: there is nothing to stop.'})).toBeVisible();
 await expect(chat.getByRole('alert').filter({hasText:/ConnectError|failed_precondition|no active turn/})).toHaveCount(0);
 await chat.getByRole('alert').getByRole('button',{name:'Dismiss'}).click();
 f.sessions[0].turnOver=false;

 // Snippets: the last used comes first; Frequent counts uses.
 await chat.getByRole('button',{name:'Snippets',exact:true}).click();let picker=page.getByRole('dialog',{name:'Snippets',exact:true});
 // Enter sends the selected snippet; the list loads from the Runner, so wait for it.
 await picker.getByRole('searchbox').fill('Run the relevant');await expect(picker.getByRole('option',{selected:true})).toContainText('test');
 await picker.getByRole('searchbox').press('Enter');await expect(picker).toHaveCount(0);
 await chat.getByRole('button',{name:'Snippets',exact:true}).click();picker=page.getByRole('dialog',{name:'Snippets',exact:true});
 await expect(picker.getByRole('option').first()).toContainText('test');await expect(picker.getByRole('option').first()).toContainText('Last used');
 await picker.getByRole('button',{name:'Frequent',exact:true}).click();
 await expect(picker.getByRole('option')).toHaveCount(1);await expect(picker.getByRole('option').first()).toContainText('1×');
 await picker.getByRole('button',{name:'Close snippets'}).click();

 // Every workspace card has the same height, however many repositories.
 const gammaRow=page.locator('.workspace-row').filter({hasText:'authority +4'}),soloRow=page.locator('.workspace-row').filter({hasText:'gentle-copper-otter'});
 await expect(gammaRow.locator('.workspace-repo-names')).toContainText('codefly-dev · authority, cli, core +2');
 await expect(gammaRow.locator('.workspace-repo-names')).toHaveAttribute('title',/codefly-dev\/web-console/);
 const heights=await Promise.all([gammaRow,soloRow,alphaRow].map(row=>row.evaluate(n=>n.getBoundingClientRect().height)));
 assert.ok(heights.every(h=>Math.abs(h-heights[0])<0.5),`cards differ in height: ${heights}`);
 assert.ok(heights[0]<100,'cards stay compact: four one-line rows (name, repositories, git, agent)');
 await page.screenshot({path:'.cache/chat-polish-sidebar.png'});

 // A generated name gets a one-click "Name it", never a prompt.
 await soloRow.hover();await soloRow.getByRole('button',{name:'Name gentle-copper-otter'}).click();
 const workspaceName=page.getByLabel('Workspace name',{exact:true});await expect(workspaceName).toHaveValue('cli');
 await workspaceName.fill('CLI polish');await page.getByRole('button',{name:'Save name'}).click();
 await expect.poll(()=>f.mutations.findLast(m=>m.method==='RenameCollection')?.body.name).toBe('CLI polish');

 // A workspace with no conversation open: what is in it and how to start.
 await gammaRow.locator('.workspace-link').click();
 const home=page.locator('.workspace-home');await expect(home).toBeVisible();
 await expect(home.getByRole('list',{name:'Repositories'}).getByRole('listitem')).toHaveCount(5);
 await expect(home).toContainText('feature/authority');
 await page.screenshot({path:'.cache/chat-polish-workspace-home.png'});
 await home.getByRole('button',{name:'Start a conversation'}).click();
 const dialog=page.getByRole('dialog',{name:'New conversation',exact:true});await expect(dialog).toBeVisible();
 await dialog.getByRole('button',{name:'Close new conversation'}).click();
 await home.getByRole('button',{name:'Open a terminal'}).click();
 await expect.poll(()=>f.mutations.some(m=>m.method==='OpenTerminal'&&m.body.collectionId==='gamma')).toBe(true);

 // A fresh conversation is Ready (not "Starting"), leads with its branch, and keeps quiet about restarts.
 const betaChat=page.locator('.embedded-chat:visible').first(),betaHeader=betaChat.locator('.chat-heading');
 await leave(()=>page.goto(`${base}/?view=workspace&collection=beta&session=beta-session#token=${f.token}`),()=>expect(betaHeader.locator('.chat-status')).toHaveText('Ready'));
 await expect(betaHeader.locator('.chat-branch')).toContainText('lazybox/beta/api');
 await expect(betaHeader.locator('.conversation-title-text'),'the folder name is not a title').toHaveCount(0);await expect(betaHeader.getByRole('button',{name:'Rename conversation'})).toContainText('Name it');
 await expect(betaHeader).not.toContainText('Model not reported');
 await expect(betaChat.locator('.runner-divider, .runner-mark')).toHaveCount(0);
 await expect(betaChat.getByRole('textbox',{name:'Message',exact:true})).toBeFocused();
 await page.screenshot({path:'.cache/chat-polish-ready.png'});
 // Its stream drops and comes back: nothing arrives on an idle conversation's stream, but the Runner's
 // heartbeat does, and it reads live again instead of "reconnecting" for good.
 const reconnecting=betaChat.getByText('Connection interrupted · reconnecting…');
 beta.cut();
 await expect(reconnecting).toBeVisible();
 await expect(reconnecting).toHaveCount(0,{timeout:6000});
 await expect(betaHeader.locator('.chat-status')).toHaveText('Ready');

 // Naming a workspace names its branch.
 await page.getByRole('button',{name:'New workspace',exact:true}).click();
 const create=page.getByRole('dialog',{name:'New workspace',exact:true});const field=create.getByLabel('Workspace name',{exact:true});
 await expect(field).toBeFocused();
 await create.getByRole('checkbox',{name:'codefly-dev/authority',exact:true}).check();
 await expect(field).toHaveValue('authority');await expect(create.getByTestId('branch-preview')).toHaveText('feature/authority');
 await field.fill('Login fix');await expect(create.getByTestId('branch-preview')).toHaveText('feature/login-fix');
 await page.screenshot({path:'.cache/chat-polish-new-workspace.png'});
 await create.getByRole('button',{name:'Edit branch'}).click();await create.getByRole('textbox',{name:'Branch'}).fill('fix/login');await create.getByRole('textbox',{name:'Branch'}).press('Enter');
 await expect(create.getByTestId('branch-preview')).toHaveText('fix/login');
 await create.getByRole('button',{name:'Create workspace',exact:true}).click();
 await expect.poll(()=>f.mutations.findLast(m=>m.method==='PrepareWorkspace')?.body.branch).toBe('fix/login');
 assert.equal(f.mutations.findLast(m=>m.method==='PrepareWorkspace').body.name,'Login fix');

 // Two chats side by side, each waiting for an approval: a key on the page itself answers the chat in front only.
 const card=command=>page.locator('.embedded-chat .request').filter({hasText:command});
 await leave(()=>page.goto(`${base}/?view=workspace&collection=delta&session=delta-left#token=${f.token}`),()=>expect(card('npm test')).toBeVisible());
 await page.locator('.page-layout .panel-collections .conversation-link').filter({hasText:'Right approval'}).click({button:'right'});
 await page.getByRole('menu').getByRole('menuitem',{name:/^Open beside/}).click();
 await expect(card('npm run lint')).toBeVisible();
 // The chat opened first listens first: the key used to answer its approval whichever chat was in front.
 const rightChat=page.locator('.embedded-chat').filter({has:page.locator('.request').filter({hasText:'npm run lint'})});
 await rightChat.locator('.chat-heading').click({position:{x:4,y:4}});
 await page.evaluate(()=>document.activeElement instanceof HTMLElement&&document.activeElement.blur());
 assert.equal(await page.evaluate(()=>document.activeElement===document.body),true,'the key goes to the page itself');
 await page.keyboard.press('y');
 const approvals=()=>f.mutations.filter(m=>m.method==='Apply'&&m.body.approval).map(m=>m.body.approval.requestId);
 await expect.poll(approvals).toEqual(['delta-right-request']);
 await page.waitForTimeout(300);
 assert.deepEqual(approvals(),['delta-right-request'],'the other chat\'s approval is not answered');
 assert.deepEqual(errors,[]);
 console.log('PASS compact header, live status (header, row, inbox; reduced motion), inline rename, prompt recall, snippet usage, equal workspace cards, Name it, workspace home, Ready fresh session, name→branch creation; the box keeps its focus after a send and text quoted while sending; a failure in words; an idle stream that reconnects reads live; a page key answers the approval of the chat in front only. Fixture only.');
}finally{await browser?.close();await server.close();await f.close();}
