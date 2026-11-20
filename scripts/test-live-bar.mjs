// Fixture only: the live bar of a running turn. It says the step with the
// real command and where it runs, how long it has taken, streams the output of
// a Lazybox-managed command (last lines, more on request, the console), stops
// that command or interrupts the turn (saying which), and carries the
// sub-agents at work. Narrow layouts and reduced motion included.
import {testViteCache} from './fixtures/vite-cache.mjs';import {createServer} from 'vite';import react from '@vitejs/plugin-react';import {webkit,expect} from './fixtures/playwright.mjs';import {fileURLToPath} from 'node:url';import {mkdir} from 'node:fs/promises';import assert from 'node:assert/strict';import {startRunnerFixture} from './fixtures/runner-ui.mjs';

const f=await startRunnerFixture();
const b64=value=>Buffer.from(JSON.stringify(value)).toString('base64');
const now=Date.now(),at=offset=>new Date(now+offset).toISOString();
const session=f.sessions[0];
Object.assign(session,{title:'Ship the front door',provider:'claude',busy:true,status:'running',agents:[{id:'root',status:'running',phase:'tool',tool:'Bash',phaseSinceUnixMillis:String(now-321000),turnStartedAtUnixMillis:String(now-400000)},{id:'claude-tool:toolu_sa',parentId:'root',status:'running',name:'Review the API',kind:'local_agent'}]});
const output=Array.from({length:12},(_,i)=>`\x1b[32m✓\x1b[0m check step ${i+1}`).join('\n')+'\nCHECK_TAIL_LINE\n';
const checkCommand = f.streamCommand(session.id, 'pnpm run check', ['pnpm', 'run', 'check']);
const ch=f.openEvents(session.id);
const push=(offset,event)=>ch.push(event.data===undefined?event:{...Object.fromEntries(Object.entries(event).filter(([k])=>k!=='data')),providerDataJson:b64(event.data)},at(offset));
// What the agent does during the test happens when the test says it: timed from then, not from the fixture
// (a cold dev server can take more than 10 s to show the page).
const live=event=>push(Date.now()-now,event);
push(-400000,{type:'operation_accepted',text:'Run the checks in the front door and fix what fails.'});
push(-399000,{type:'turn_started'});
push(-390000,{type:'message_delta',text:'Running the checks first.'});
push(-321000,{type:'activity',status:'tool',name:'Bash'});
push(-321000,{type:'tool_activity',data:{type:'tool_call',id:'toolu_check',name:'Bash',status:'running',input:{command:'cd services/front-door && lazybox run pnpm run check',description:'Run the checks'}}});
push(-300000,{type:'tool_activity',data:{type:'tool_call',id:'toolu_sa',name:'Task',status:'running',input:{description:'Review the API',prompt:'Review it.',subagent_type:'Explore'}}});
push(-299000,{type:'agent_status',agentId:'claude-tool:toolu_sa',parentAgentId:'root',nativeId:'task-sa',status:'running',name:'Review the API',data:{toolUseId:'toolu_sa',subagentType:'Explore'}});

const server=await createServer({...testViteCache(import.meta.url),configFile:false,logLevel:'error',root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:f.url}}}});let browser;
try{
 await server.listen();await mkdir('.cache',{recursive:true});browser=await webkit.launch();const page=await browser.newPage({viewport:{width:1440,height:900}});page.setDefaultTimeout(15000);
 const errors=[];page.on('pageerror',e=>errors.push(String(e)));
 await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=${session.id}#token=${f.token}`);
 const chat=page.locator('.embedded-chat:visible').first(),bar=chat.getByRole('region',{name:'What the agent is doing'});

 // The step, with the actual command, where it runs, and for how long.
 await expect(bar.locator('.live-activity')).toHaveText(/^Running a command · \d+m \d+s$/);
 // Its clock counts from the step's start, 321 s before the fixture was made: however long the page took to load.
 {const [,m,sec]=(await bar.locator('.live-activity').textContent()).match(/(\d+)m (\d+)s/),shown=Number(m)*60+Number(sec),since=(Date.now()-now)/1000;
  assert.ok(shown>=320&&shown<=322+since,`elapsed ${shown}s, ${Math.round(since)}s after the fixture`);}
 await expect(bar.locator('.live-bar-code')).toHaveText('lazybox run pnpm run check');
 await expect(bar.locator('.live-bar-where')).toHaveText('in services/front-door');
 // A managed command streams: its last lines, without colour codes.
 const out=bar.locator('.live-bar-output');
 await expect(out).toContainText('No output yet.');
 const commandRow = bar.locator('.live-bar-row > .live-activity');
 const compactHeight = await out.evaluate(node => node.getBoundingClientRect().height);
 await commandRow.dblclick();
 await expect(out).toHaveAttribute('data-expanded','true');
 const expandedHeight = await out.evaluate(node => node.getBoundingClientRect().height);
 assert.ok(expandedHeight > compactHeight);
 await commandRow.dblclick();
 await expect(out).not.toHaveAttribute('data-expanded','true');
 checkCommand.write(output);
 await expect(out).toContainText('CHECK_TAIL_LINE');
 await expect(out).not.toContainText('[32m');
 assert.equal(await out.evaluate(node => node.getBoundingClientRect().height), compactHeight, 'compact output does not grow as lines arrive');
 assert.equal((await out.textContent()).split('\n').length,3,'three lines until more is asked for');
 await commandRow.dblclick();
 await expect(bar.getByRole('button',{name:'Less output'})).toHaveAttribute('aria-expanded','true');
 await commandRow.dblclick();
 await expect(bar.getByRole('button',{name:'More output'})).toHaveAttribute('aria-expanded','false');
 await bar.getByRole('button',{name:'More output'}).click();
 await expect(out).toContainText('check step 1');
 assert.equal(await out.evaluate(node => node.getBoundingClientRect().height), expandedHeight, 'expanded output does not grow as lines arrive');
 await expect(bar.getByRole('button',{name:'Less output'})).toHaveAttribute('aria-expanded','true');
 // Esc in the expanded output folds it back to its last lines.
 await out.focus();await page.keyboard.press('Escape');
 await expect(bar.getByRole('button',{name:'More output'})).toHaveAttribute('aria-expanded','false');
 // The output can be put away: its own ×, clear of the text, and it stays away for the next command and a reload.
 {const hide=bar.getByRole('button',{name:'Hide output'}),h=await hide.boundingBox(),o=await out.boundingBox();
  assert.ok(h.x>=o.x&&h.x+h.width<=o.x+o.width+1&&h.y>=o.y&&h.y+h.height<=o.y+o.height+1,'the hide control sits inside the output');
  await hide.click();}
 await expect(out).toHaveCount(0);
 await expect(bar.getByRole('button',{name:'More output'})).toHaveCount(0);
 await expect(bar.getByRole('button',{name:'Stop command'})).toBeVisible();
 await page.reload();
 await expect(bar.locator('.live-bar-code')).toHaveText('lazybox run pnpm run check');
 await expect(out).toHaveCount(0);
 await page.screenshot({path:'.cache/subagents-live-bar-hidden.png'});
 // And brought back, from the bar.
 await bar.getByRole('button',{name:'Show output'}).click();
 await expect(out).toContainText('CHECK_TAIL_LINE');
 await bar.getByRole('button',{name:'More output'}).click();
 await expect(out).toContainText('check step 1');
 // The sub-agents at work are in the same bar; the old separate lines are gone.
 await expect(bar.locator('.subagent-chip')).toContainText('Review the API');
 await expect(chat.locator('.live-bar')).toHaveCount(1);
 await expect(chat.locator('.agent-activity')).toHaveCount(0);
 await page.screenshot({path:'.cache/subagents-live-bar.png'});

 // Open in console: the workspace's command output shows it.
 await bar.getByRole('button',{name:/View command output:/}).click();
 await expect(page.locator('.command-panel')).toContainText('CHECK_TAIL_LINE');
 // The console says the command is running and can stop it itself.
 await expect(page.locator('.command-panel .console-state [role=status]')).toHaveText('Running');
 await expect(page.locator('.command-panel').getByRole('button',{name:'Stop',exact:true})).toBeVisible();await page.screenshot({path:'.cache/subagents-live-bar-console.png'});
 // The console has the whole output: the bar stops repeating it, so the shorter chat keeps its messages.
 await expect(bar.locator('.live-bar-output')).toHaveCount(0);
 await expect(bar.getByRole('button',{name:'Show output'})).toBeVisible();
 {const box=await bar.boundingBox(),pane=await chat.locator('.messages:visible').boundingBox();assert.ok(pane.y+pane.height<=box.y+1,'a short chat keeps the bar clear of the messages');}

 // Stop says what it stops: the command, not the turn.
 await bar.getByRole('button',{name:'Stop command'}).click();
 await expect.poll(()=>f.mutations.findLast(m=>m.method==='CancelCommand')?.body.commandId).toBe(checkCommand.id);
 assert.equal(f.mutations.filter(m=>m.method==='Apply'&&m.body.interrupt).length,0,'stopping the command does not interrupt the turn');
 // Once no managed command runs, the control interrupts the turn, and says so.
 await expect(bar.getByRole('button',{name:'Interrupt'})).toBeVisible({timeout:5000});
 await expect(bar.locator('.live-bar-output')).toHaveCount(0);
 await bar.getByRole('button',{name:'Interrupt'}).click();
 await expect.poll(()=>f.mutations.some(m=>m.method==='Apply'&&m.body.interrupt)).toBe(true);

 // A step that is not a command says what it does, in words.
 live({type:'tool_activity',data:{type:'tool_call',id:'toolu_check',status:'completed',output:'ok'}});
 live({type:'tool_activity',data:{type:'tool_call',id:'toolu_read',name:'Read',status:'running',input:{file_path:'services/front-door/src/app.ts'}}});
 await expect(bar.locator('.live-activity')).toHaveText(/^Reading services\/front-door\/src\/app\.ts · \ds$/);
 await expect(bar.locator('.live-bar-code')).toHaveCount(0);

 // A `lazybox run` that will not end (it listens on a port) can move to the background: the agent's call returns, the server keeps running.
 live({type:'tool_activity',data:{type:'tool_call',id:'toolu_read',status:'completed',output:'ok'}});
 live({type:'tool_activity',data:{type:'tool_call',id:'toolu_dev',name:'Bash',status:'running',input:{command:'lazybox run -- npm run dev'}}});
 const devOutput='VITE v6 ready in 412 ms\n  ➜  Local: http://localhost:5173/\n';
 f.commands.push({id:'cmd-dev',sessionId:session.id,title:'npm run dev',argv:['npm','run','dev'],mode:'pipe',done:false,canBackground:true,ports:[5173],offset:String(devOutput.length),output:devOutput});
 const move=bar.getByRole('button',{name:'Move to background'});
 await expect(move).toBeVisible({timeout:5000});
 await expect(bar.getByRole('button',{name:'Less output'})).toHaveAttribute('aria-expanded','true');
 await expect(bar.getByRole('button',{name:'Stop command'})).toBeVisible();
 await page.screenshot({path:'.cache/live-bar-move.png'});
 await move.click();
 await expect.poll(()=>f.mutations.findLast(m=>m.method==='BackgroundCommand')?.body.commandId).toBe('cmd-dev');
 await expect(bar.getByRole('status').filter({hasText:'Moved to the background'})).toHaveText('Moved to the background · port 5173. Claude continues; stop it from the console or Processes.');
 // The server is not the turn's step any more: its output and Stop leave the bar.
 await expect(bar.locator('.live-bar-output')).toHaveCount(0,{timeout:5000});
 await expect(bar.getByRole('button',{name:'Interrupt'})).toBeVisible();
 await expect(move).toHaveCount(0);
 await page.screenshot({path:'.cache/live-bar-moved.png'});
 // A shell step that is not a lazybox run and looks like a server may never end: the bar says how to get out.
 live({type:'tool_activity',data:{type:'tool_call',id:'toolu_dev',status:'completed',output:'[lazybox] npm run dev → running in the background as command cmd-dev'}});
 live({type:'tool_activity',data:{type:'tool_call',id:'toolu_tail',name:'Bash',status:'running',input:{command:'tail -f log/development.log'}}});
 await expect(bar.getByRole('note')).toHaveText('This command may never end. Interrupt the turn and ask Claude to start it with lazybox run — it moves servers to the background.');
 await bar.getByRole('button',{name:/View command output:/}).click();
 const activity=page.getByRole('dialog',{name:'Tools and activity'});
 await expect(activity.getByText('Waiting for the result…',{exact:true})).toBeVisible();
 live({type:'tool_activity',data:{type:'tool_call',id:'toolu_tail',status:'completed',output:'Tail command output'}});
 await expect(activity.getByText('Tail command output',{exact:true})).toBeVisible();
 await activity.getByRole('button',{name:'Close tools'}).click();

 await expect(bar.getByRole('button',{name:'Interrupt'})).toBeVisible();

 // Reduced motion stills the indicator; a phone keeps the bar on screen without sideways scroll.
 await page.emulateMedia({reducedMotion:'reduce'});
 assert.equal(await bar.locator('.live-indicator').first().evaluate(n=>getComputedStyle(n).animationName),'none');
 await page.setViewportSize({width:390,height:844});
 // A phone shows one panel at a time: back to the chat from the console. The switcher comes with the
 // narrow layout's next render: waited for, not counted at once (on a slow machine it is not there yet).
 {const chatTab=page.locator('.mobile-panel-switcher').getByRole('button',{name:/Agent chat/});await chatTab.click();await expect(chatTab).toHaveAttribute('aria-pressed','true');}
 await expect(bar.getByRole('button',{name:'Interrupt'})).toBeVisible();
 assert.ok(await page.evaluate(()=>document.documentElement.scrollWidth<=window.innerWidth+1),'no horizontal page scroll');
 await page.screenshot({path:'.cache/subagents-live-bar-phone.png'});

 // The turn ends: the bar leaves; only background work would keep it.
 Object.assign(session,{busy:false,status:'ready',agents:[{id:'root',status:'done'}]});live({type:'agent_status',agentId:'claude-tool:toolu_sa',status:'done',data:{toolUseId:'toolu_sa'}});live({type:'turn_ended',status:'completed'});
 // The "Moved to the background" notice may outlive the turn by a few seconds; the bar itself leaves.
 await expect(chat.locator('.live-bar:not([data-only="notice"])')).toHaveCount(0);
 assert.deepEqual(errors,[]);
 console.log('PASS live bar: step with real command and directory, elapsed, managed output tail (3 lines, more, console), Stop command vs Interrupt, step in words, Move to background for a listening lazybox run (then out of the bar) and the may-never-end hint for a shell step, sub-agents in the bar, reduced motion, phone width, leaves when the turn ends. Fixture only.');
}finally{await browser?.close();await server.close();await f.close();}
