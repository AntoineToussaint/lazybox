import { collectPageErrors } from './fixtures/page-errors.mjs';
// Fixture only: sub-agents in a conversation. Realistic Claude worker journals
// (two parallel sub-agents, one nested, one failed, one long finished run):
// inline cards with live current action, their own timelines, the tray beside
// the composer, the tree, the panel (docked, floating, phone sheet), stopping
// one, and the failed one in the parent's status line.
import {createServer} from 'vite';import react from '@vitejs/plugin-react';import {webkit,expect} from './fixtures/playwright.mjs';import {fileURLToPath} from 'node:url';import {mkdir} from 'node:fs/promises';import assert from 'node:assert/strict';import {startRunnerFixture} from './fixtures/runner-ui.mjs';

const f=await startRunnerFixture();
const b64=value=>Buffer.from(JSON.stringify(value)).toString('base64');
const now=Date.now(),at=offset=>new Date(now+offset).toISOString();
const A='claude-tool:toolu_explore',B='claude-tool:toolu_audit',C='claude-tool:toolu_logs',D='claude-tool:toolu_flaky',E='claude-tool:toolu_long';
Object.assign(f.sessions[0],{title:'Make sub-agents visible',provider:'claude',model:'claude-fable-5-1',modelReported:true,busy:true,status:'running',
 capabilities:{...f.sessions[0].capabilities,stopAgent:true},
 agents:[{id:'root',status:'running',phase:'thinking',phaseSinceUnixMillis:String(now-4000),turnStartedAtUnixMillis:String(now-300000)},
  ...[[A,'running'],[B,'running'],[C,'running'],[D,'failed'],[E,'done']].map(([id,status])=>({id,parentId:id===C?B:'root',status,name:'Sub-agent',kind:'local_agent',nativeId:'task-'+id})),{id:'claude-tool:toolu_bg',parentId:'root',status:'running',name:'cd /work/api && source .venv/bin/activate',kind:'local_bash',nativeId:'bash_3'},
  // The Runner knows it running, but by the time a stop reaches it, it has ended (the race the card must survive).
  {id:'claude-tool:toolu_gone',parentId:'root',status:'running',name:'Sub-agent',kind:'local_agent',nativeId:'task-gone',notRunning:true}]});
const ch=f.openEvents('alpha-session');
const push=(offset,event)=>ch.push(event.data===undefined?event:{...Object.fromEntries(Object.entries(event).filter(([k])=>k!=='data')),providerDataJson:b64(event.data)},at(offset));
const call=(id,name,status,extra={})=>({type:'tool_call',id,name,status,...extra});
const spawn=(offset,id,toolUse,description,type,prompt,parent='root',extra={})=>{
 push(offset,{type:'tool_activity',...(parent==='root'?{}:{agentId:parent}),data:call(toolUse,'Task','running',{input:{description,prompt,subagent_type:type,...extra}})});
 push(offset+200,{type:'agent_status',agentId:id,parentAgentId:parent,nativeId:'task-'+toolUse,status:'running',name:description,agentKind:'local_agent',data:{toolUseId:toolUse,subagentType:type,prompt}});
};
push(-300000,{type:'operation_accepted',text:'Why are sub-agents invisible in the chat? Investigate end to end.'});
push(-299000,{type:'turn_started'});
push(-298000,{type:'message_delta',text:'I will split this: one agent maps the transcript, one audits the worker, and I will rerun the flaky test.'});
// A long run that already finished, with hundreds of steps and a result.
spawn(-290000,E,'toolu_long','Survey every adapter','general-purpose','Read every adapter under internal/runner/adapters and list what each forwards.');
for(let i=0;i<160;i++){push(-289000+i*500,{type:'tool_activity',agentId:E,data:call('e'+i,i%3?'Read':'Bash','running',{input:i%3?{file_path:`services/runner/code/internal/runner/adapters/file${i}.go`}:{command:`rg -n parent_tool_use_id adapters/file${i}.go`}})});push(-288800+i*500,{type:'tool_activity',agentId:E,data:call('e'+i,undefined,'completed',{output:`line ${i}\n`.repeat(40)})});if(i%20===0)push(-288700+i*500,{type:'message_delta',agentId:E,text:`Step ${i}: the adapter forwards tool calls but drops sub-agent prose.`});}
push(-200000,{type:'tool_activity',data:{type:'tool_call',id:'toolu_long',status:'completed',output:[{type:'text',text:'## Findings\n\n- **Claude**: sub-agent prose is dropped unless `forwardSubagentText` is on.\n- **Codex**: sub-threads are attributed by thread id.\n\nLONG_RESULT_END'},{type:'text',text:'agentId: a1b2 (use to resume)'}]}});
push(-199990,{type:'agent_status',agentId:E,status:'done',data:{toolUseId:'toolu_long',summary:'Surveyed 4 adapters',usage:{totalTokens:182000,toolUses:160,durationMs:90000}}});
// Two in parallel, now.
spawn(-95000,A,'toolu_explore','Map the transcript pipeline','Explore','Trace how journal events become transcript messages.\nReport where sub-agent events are dropped.');
spawn(-60000,B,'toolu_audit','Audit worker events','general-purpose','Audit the Claude worker for dropped sub-agent events.');
push(-90000,{type:'tool_activity',agentId:A,data:call('a1','Grep','running',{input:{pattern:'agentId',path:'packages/surfaces/src'}})});
push(-89000,{type:'tool_activity',agentId:A,data:call('a1',undefined,'completed',{output:'transcript-model.ts:52: if (!event || (event.agentId && event.agentId !== \'root\')) continue;'})});
push(-88000,{type:'message_delta',agentId:A,parentAgentId:'root',text:'The transcript fold skips every event whose agentId is not root.'});
push(-8000,{type:'tool_activity',agentId:A,data:call('a2','Read','running',{input:{file_path:'packages/surfaces/src/transcript-model.ts'}})});
push(-5000,{type:'agent_status',agentId:A,status:'running',data:{toolUseId:'toolu_explore',usage:{totalTokens:48200,toolUses:14,durationMs:90000},summary:'Reading the transcript fold'}});
push(-50000,{type:'tool_activity',agentId:B,data:call('b1','Bash','running',{input:{command:'rg -n parent_tool_use_id workers/claude-sdk/src',description:'Find attribution'}})});
// B spawned a nested sub-agent that is still reading CI logs.
spawn(-40000,C,'toolu_logs','Read the CI logs','general-purpose','Read the last CI run logs for sub-agent errors.',B);
push(-30000,{type:'tool_activity',agentId:C,data:call('c1','WebFetch','running',{input:{url:'https://ci.example.com/runs/4242/logs'}})});
// One failed.
spawn(-120000,D,'toolu_flaky','Rerun the flaky test 50 times','general-purpose','Run go test -run TestLogin -count 50 and report failures.');
// The same file read twice in a row reads as one step; the failed run stands out.
push(-119000,{type:'tool_activity',agentId:D,data:call('dq','Bash','running',{input:{command:'git diff --stat',description:'Gather diff stats'}})});
push(-118700,{type:'tool_activity',agentId:D,data:call('dq',undefined,'completed',{output:' 2 files changed'})});
for(const [n,offset] of [['d0a',-118000],['d0b',-117000]]){push(offset,{type:'tool_activity',agentId:D,data:call(n,'Read','running',{input:{file_path:'/tmp/rev.diff'}})});push(offset+100,{type:'tool_activity',agentId:D,data:call(n,undefined,'completed',{output:'diff --git a/x b/x'})});}
push(-110000,{type:'tool_activity',agentId:D,data:call('d1','Bash','running',{input:{command:'go test ./... -run TestLogin -count 50',description:'Rerun the login test'}})});
push(-101000,{type:'tool_activity',agentId:D,data:call('d1',undefined,'failed',{output:'panic: test timed out after 10m0s'})});
push(-100000,{type:'agent_status',agentId:D,status:'failed',data:{toolUseId:'toolu_flaky',error:'The test binary timed out after 10m.'}});
push(-4000,{type:'activity',status:'thinking'});
// A background command whose name is a whole script, as Claude reports it.
const SCRIPT='cd /work/api && source .venv/bin/activate\n# the MCP suite is slow\nPYTHONPATH=src uv run pytest tests/test_mcp.py -x -q 2>&1 | tail -80';
const BG='claude-tool:toolu_bg';
push(-45000,{type:'tool_activity',data:call('toolu_bg','Bash','running',{input:{command:SCRIPT,run_in_background:true}})});
push(-44900,{type:'agent_status',agentId:BG,parentAgentId:'root',nativeId:'bash_3',status:'running',name:SCRIPT,agentKind:'local_bash',data:{toolUseId:'toolu_bg',taskType:'local_bash'}});
push(-20000,{type:'tool_activity',data:call('toolu_read','BashOutput','running',{input:{bash_id:'bash_3'}})});
push(-19900,{type:'tool_activity',data:{type:'tool_call',id:'toolu_read',status:'completed',output:'tests/test_mcp.py::test_tools PASSED\nBG_OUTPUT_SEEN'}});
// Many sub-agents in a long saved conversation: it must open as fast as any other.
Object.assign(f.sessions[1],{title:'Fan-out',provider:'claude',status:'stopped',closed:true,busy:false,agents:[{id:'root',status:'done'}]});
const many=f.openEvents('beta-session');many.pace=50;many.paceMs=1;
const pushMany=event=>many.push(event.data===undefined?event:{...Object.fromEntries(Object.entries(event).filter(([k])=>k!=='data')),providerDataJson:b64(event.data)},at(-100000));
for(let n=0;n<80;n++){const id='claude-tool:fan'+n;pushMany({type:'operation_accepted',text:`Fan out batch ${n}`});
 pushMany({type:'tool_activity',data:call('fan'+n,'Task','running',{input:{description:`Batch ${n}`,prompt:'Check one package.',subagent_type:'Explore'}})});
 for(let i=0;i<60;i++){pushMany({type:'tool_activity',agentId:id,data:call(`f${n}-${i}`,'Read','running',{input:{file_path:`pkg/${n}/file${i}.go`}})});pushMany({type:'tool_activity',agentId:id,data:call(`f${n}-${i}`,undefined,'completed',{output:'x'.repeat(2000)})});}
 for(const chunk of `Batch ${n} is clean.`.match(/.{1,4}/g))pushMany({type:'message_delta',agentId:id,text:chunk});
 pushMany({type:'tool_activity',data:{type:'tool_call',id:'fan'+n,status:'completed',output:`Batch ${n}: no problems. FAN_END_${n}`}});
 pushMany({type:'message_delta',text:`Batch ${n} reviewed.`});}
many.complete();

const server=await createServer({configFile:false,logLevel:'error',root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:f.url}}}});let browser;
try{
 await server.listen();await mkdir('.cache',{recursive:true});browser=await webkit.launch();const page=await browser.newPage({viewport:{width:1440,height:960}});page.setDefaultTimeout(15000);
 const errors=await collectPageErrors(page);
 const base=`http://127.0.0.1:${server.httpServer.address().port}`;
 await page.goto(`${base}/?view=workspace&collection=alpha&session=alpha-session#token=${f.token}`);
 const chat=page.locator('.embedded-chat:visible').first(),messages=chat.locator('.messages:visible');
 const card=id=>messages.locator(`.subagent-card[data-agent-id="${id}"]`);

 // Cards sit where they started, in order, with their type, task and state.
 await expect(messages.locator('.subagent-anchor > .subagent-card')).toHaveCount(4);
 assert.deepEqual(await messages.locator('.subagent-anchor > .subagent-card').evaluateAll(n=>n.map(e=>e.dataset.agentId)),[E,A,B,D],"cards follow the journal");
 // "Explore · Map the transcript pipeline": a kind that says something, then the task. The Runner's
 // placeholder name ("Sub-agent") is never a title, and a generic kind is no badge.
 await expect(card(A).locator('.subagent-who')).toHaveText('Explore · Map the transcript pipeline');
 await expect(card(A).locator('.subagent-type')).toHaveText('Explore');
 await expect(card(A).locator('.subagent-name')).toHaveText('Map the transcript pipeline');
 await expect(card(E).locator('.subagent-who')).toHaveText('Survey every adapter');
 await expect(card(E).locator('.subagent-type'),'general-purpose says nothing: no badge').toHaveCount(0);
 await expect(messages.locator('.subagent-card .subagent-who').filter({hasText:/^Sub-agent$/})).toHaveCount(0);
 await expect(card(A)).toHaveAttribute('data-status','running');
 await expect(card(A).locator('.subagent-now-text')).toHaveText('Reading packages/surfaces/src/transcript-model.ts');
 await expect(card(A).locator('.subagent-stats')).toContainText(/\d+m \d+s · 14 tools · 48k tokens/);
 // Its clock counts from its spawn, 95 s before the fixture was made: however long the page took to load.
 {const [,m,sec]=(await card(A).locator('.subagent-stats').textContent()).match(/(\d+)m (\d+)s/),shown=Number(m)*60+Number(sec),since=(Date.now()-now)/1000;
  assert.ok(shown>=94&&shown<=96+since,`elapsed ${shown}s, ${Math.round(since)}s after the fixture`);}
 // One elapsed time: the card's. The current step does not run a second clock.
 await expect(card(A).locator('time')).toHaveCount(1);
 // Its numbers and actions sit in its header row, inside the card.
 {const head=await card(A).locator(':scope > .subagent-head').boundingBox(),box=await card(A).boundingBox(),open=await card(A).getByRole('button',{name:/in a panel$/}).boundingBox();
  assert.ok(open.y>=head.y&&open.y+open.height<=head.y+head.height+0.5&&open.x+open.width<=box.x+box.width-4,'the panel button sits inside the header row');}
 await expect(card(A).locator('.subagent-progress')).toHaveText('Reading the transcript fold');
 {const first=await card(A).locator('.subagent-stats time').textContent();await expect.poll(()=>card(A).locator('.subagent-stats time').textContent(),{timeout:4000}).not.toBe(first);}
 await expect(messages.locator('.message').filter({hasText:'skips every event'}),'a sub-agent\'s prose is not a main message').toHaveCount(0);
 // Finished ones: outcome and result.
 await expect(card(E)).toHaveAttribute('data-status','done');
 await expect(card(E).locator('.subagent-outcome')).toContainText('Done');
 await expect(card(E).locator('.subagent-outcome')).toContainText('Findings · Claude: sub-agent prose is dropped');
 await expect(card(E).locator('.subagent-stats')).toContainText('1m 30s · 160 tools · 182k tokens');
 await card(E).getByRole('button',{name:'Show full result'}).click();
 await expect(card(E).locator('.subagent-result')).toContainText('LONG_RESULT_END');
 await expect(card(E).locator('.subagent-result')).not.toContainText('agentId: a1b2');
 await expect(card(D)).toHaveAttribute('data-status','failed');
 await expect(card(D).locator('.subagent-outcome')).toContainText('The test binary timed out after 10m.');
 await page.screenshot({path:'.cache/subagents-polish-cards.png'});

 // Live: the current action moves with the sub-agent.
 push(0,{type:'tool_activity',agentId:A,data:call('a2',undefined,'completed',{output:'export function foldTranscript…'})});
 push(10,{type:'tool_activity',agentId:A,data:call('a3','Bash','running',{input:{command:'go test ./internal/runner/... -run Subagent'}})});
 await expect(card(A).locator('.subagent-now-text')).toHaveText('Running go test ./internal/runner/... -run Subagent');

 // Expand: its own timeline, tool output collapsed until asked for.
 await card(A).locator(':scope > .subagent-head > .subagent-toggle').click();
 const body=card(A).locator('.subagent-body');
 await expect(body.locator('.subagent-prompt summary')).toContainText('Trace how journal events become transcript messages.');
 await expect(body.locator('.subagent-message')).toContainText('skips every event whose agentId is not root');
 // Each step is the chat's tool line: one dense line, icon and what it did.
 const rows=body.locator('.subagent-step.tool .tool-line-row');
 await expect(rows).toHaveCount(3);
 await expect(rows.first()).toHaveText('Searched for agentId in packages/surfaces/src');
 for(const height of await rows.evaluateAll(n=>n.map(e=>e.getBoundingClientRect().height)))assert.ok(height<=30,`a step is one line (${height}px)`);
 // No tooltip repeats a line that is fully shown (it would cover the next line).
 assert.deepEqual(await body.locator('.tool-line-row, .tool-line-row [title]').evaluateAll(n=>n.map(e=>e.getAttribute('title')).filter(Boolean)),[]);
 await rows.first().locator('.tool-line-title').hover();
 assert.equal(await rows.first().locator('.tool-line-title').getAttribute('title'),null,'hovering a line that fits adds no tooltip');
 // Open, what it does now is the last line, pinned; the separate line above is gone.
 await expect(body.locator('.subagent-step[data-current]')).toHaveCount(1);
 await expect(body.locator('.subagent-step[data-current]')).toHaveText('Running go test ./internal/runner/... -run Subagent');
 await expect(body.locator('.subagent-step[data-current] .tool-line')).toHaveAttribute('data-state','running');
 await expect(card(A).locator(':scope > .subagent-now')).toHaveCount(0);
 await expect(body.locator('.tool-details')).toHaveCount(0);
 await rows.first().click();
 await expect(body.locator('.tool-details').first()).toContainText('transcript-model.ts:52');
 await page.screenshot({path:'.cache/subagents-polish-expanded.png'});
 // (WebKit does not focus a clicked button; a keyboard reader is on the line.)
 await rows.first().focus();
 await page.keyboard.press('Escape');
 await expect(card(A).locator('.subagent-body')).toHaveCount(0);
 await expect(card(A).locator(':scope > .subagent-head > .subagent-toggle')).toHaveAttribute('aria-expanded','false');

 // A repeated call is one line with a count; a failed step stands out. (Expanding the failed card counts as having seen it.)
 const reviewFailed=async()=>{
 const toggleD=card(D).locator(':scope > .subagent-head > .subagent-toggle');
 if(await toggleD.getAttribute('aria-expanded')!=='true')await toggleD.click();
 const lines=card(D).locator('.subagent-step.tool > .tool-lines > .tool-line');
 await expect(lines).toHaveCount(3);
 await expect(lines.nth(0).locator('.tool-meta'),'under a second says nothing').toHaveCount(0);
 await expect(lines.nth(1).locator(':scope > .tool-line-row')).toHaveText('Read /tmp/rev.diff ×2');
 await expect(lines.nth(2)).toHaveAttribute('data-state','failed');
 await expect(lines.nth(2).locator('.tool-meta'),'a step of 9s says so').toHaveText('9s');
 await expect(lines.nth(2).locator('.tool-line-problem')).toHaveText('panic: test timed out after 10m0s');
 {const bg=await lines.nth(2).locator(':scope > .tool-line-row').evaluate(n=>getComputedStyle(n).backgroundColor);assert.notEqual(bg,'rgba(0, 0, 0, 0)','a failed step is tinted');}
 // Facts sit at the right edge of the line.
 {const row=await lines.nth(2).locator(':scope > .tool-line-row').boundingBox(),meta=await lines.nth(2).locator('.tool-meta').boundingBox();assert.ok(row.x+row.width-(meta.x+meta.width)<12,'facts are right-aligned');}
 await card(D).screenshot({path:'.cache/subagents-polish-failed.png'});
 await lines.nth(1).locator(':scope > .tool-line-row').click();
 await expect(lines.nth(1).locator('.tool-group-calls .tool-line-row')).toHaveText([/^Read \/tmp\/rev\.diff\s*1 line$/,/^Read \/tmp\/rev\.diff\s*1 line$/]);
 await card(D).locator(':scope > .subagent-head > .subagent-toggle').click();
 };

 // A long run shows its latest few steps; all of them on request, mounted from the end.
 await card(E).getByRole('button',{name:'Hide full result'}).click();
 await card(E).locator(':scope > .subagent-head > .subagent-toggle').click();
 await expect(card(E).locator('.subagent-step')).toHaveCount(5);
 const showAll=card(E).getByRole('button',{name:/^Show all \d+ steps$/});
 {const steps=Number((await showAll.textContent()).match(/\d+/)[0]);assert.ok(steps>40&&steps<168,`reads are grouped, so fewer steps than calls (${steps})`);}
 {const box=await card(E).boundingBox();assert.ok(box.height<420,`an open card stays compact (${box.height}px)`);}
 await page.screenshot({path:'.cache/subagents-polish-long.png'});
 await showAll.click();
 await expect(card(E).locator('.subagent-step')).toHaveCount(40);
 await expect(card(E).getByRole('button',{name:/Show earlier steps/})).toBeVisible();
 await card(E).getByRole('button',{name:'Show only the latest 5'}).click();
 await expect(card(E).locator('.subagent-step')).toHaveCount(5);
 await card(E).locator(':scope > .subagent-head > .subagent-toggle').click();

 // The tray: parallel sub-agents side by side, the nested one on its parent's chip.
 const tray=chat.locator('.subagent-tray');
 await expect(tray.locator('.subagent-tray-count')).toHaveText('3 sub-agents · 1 command running');
 await expect(tray.locator('.subagent-chip:not(.command)')).toHaveCount(2);
 // A background command is a chip of its own, named by what runs, never by its script; it has no card.
 const bgChip=tray.locator('.subagent-chip.command');
 await expect(bgChip).toHaveText(/^pytest tests\/test_mcp\.py\d/);
 await expect(tray).not.toContainText('source .venv');
 await expect(messages.locator('.subagent-card[data-agent-id="claude-tool:toolu_bg"]')).toHaveCount(0);
 await expect(tray.locator('.subagent-chip').nth(1)).toContainText('Audit worker events');
 await expect(tray.locator('.subagent-chip').nth(1).locator('.subagent-chip-nested')).toHaveText('+1');
 // One live bar: the main agent's step, and the background work in it.
 await expect(chat.locator('.live-bar')).toHaveCount(1);
 await expect(chat.locator('.live-bar .live-activity')).toContainText('Thinking');
 await expect(chat.locator('.live-bar .subagent-tray')).toHaveCount(1);
 const chips=await tray.locator('.subagent-chip').evaluateAll(n=>n.map(e=>e.getBoundingClientRect()));
 assert.ok(Math.abs(chips[0].top-chips[1].top)<1&&chips[1].left>chips[0].right,'parallel chips sit side by side');
 await page.screenshot({path:'.cache/subagents-tray.png'});
 // A chip takes you to its card and opens it.
 await messages.evaluate(n=>{n.scrollTop=0;});
 await tray.locator('.subagent-chip').nth(1).click();
 await expect(card(B).locator(':scope > .subagent-head > .subagent-toggle')).toHaveAttribute('aria-expanded','true');
 await expect(card(B)).toBeInViewport();
 await expect(card(B).locator('.subagent-body .subagent-card[data-agent-id="'+C+'"]'),'the nested sub-agent shows in its parent\'s timeline').toBeVisible();
 await expect(card(B).locator(`.subagent-card[data-agent-id="${C}"] .subagent-now-text`)).toHaveText('Fetching ci.example.com/runs/4242/logs');
 // Chips move with the arrow keys.
 await tray.locator('.subagent-chip').first().focus();await page.keyboard.press('ArrowRight');
 await expect(tray.locator('.subagent-chip').nth(1)).toBeFocused();
 // Active agents nest under the parent that started them.
 await tray.getByRole('button',{name:'All background work: 3 sub-agents · 1 command · 4 running'}).click();
 const tree=page.getByRole('dialog',{name:'Background work',exact:true});
 await expect(tree.getByRole('region',{name:'Sub-agents'}).locator('li')).toHaveCount(3);
 await expect(tree.getByRole('region',{name:'Background commands'}).locator('li')).toHaveText(/pytest tests\/test_mcp\.py/);
 await expect(tree.locator(`li[data-depth="1"]`)).toContainText('Read the CI logs');
 await expect(tree.locator('.subagent-tree-row').first()).toBeFocused();
 await page.screenshot({path:'.cache/subagents-tree.png'});
 await page.keyboard.press('Escape');await expect(tree).toHaveCount(0);
 await expect(tray.locator('.subagent-tray-all')).toBeFocused();
 // A command's chip opens its script and the output the agent read.
 await bgChip.click();
 const bgPanel=chat.getByRole('complementary',{name:'Background command: pytest tests/test_mcp.py'});
 await expect(bgPanel.locator('.background-command')).toContainText('source .venv/bin/activate');
 await expect(bgPanel.locator('.background-command')).toContainText('BG_OUTPUT_SEEN');
 await page.screenshot({path:'.cache/subagents-background-command.png'});
 await bgPanel.getByRole('button',{name:'Close background command panel'}).click();
 // The header keeps the way to all background work.
 // Its numbers say what they count: in words, and in full in the tooltip.
 const workButton=chat.locator('.chat-heading').getByRole('button',{name:'Background work: 3 sub-agents · 1 command · 4 running'});
 await expect(workButton).toHaveText('3 sub-agents · 1 command running');
 await expect(workButton).toHaveAttribute('title','3 sub-agents · 1 command · 4 running. Show them all.');
 // The count is neutral; only what failed is red, and it says so.
 {const activity=chat.locator('.chat-heading').getByRole('button',{name:'View tools and activity'});
  await expect(activity.locator('.tool-activity-count')).toHaveText(/^\d+ tool calls$/);await expect(activity.locator('.tool-activity-failed')).toHaveText('1 failed');
  const colours=await activity.evaluate(node=>({count:getComputedStyle(node.querySelector('.tool-activity-count')).color,failed:getComputedStyle(node.querySelector('.tool-activity-failed')).color,context:getComputedStyle(node.closest('.chat-heading').querySelector('.context-trigger')).color}));
  assert.notEqual(colours.count,colours.failed,'the count is not drawn in the failure colour');}
 await expect(tray.locator('.subagent-tray-all')).toHaveText('All · 3 sub-agents · 1 command');
 await page.screenshot({path:'.cache/subagents-polish-header.png',clip:{x:280,y:88,width:1160,height:80}});

 // A panel of its own, docked, following live.
 await card(A).getByRole('button',{name:'Open Map the transcript pipeline in a panel'}).click();
 const panel=chat.getByRole('complementary',{name:'Sub-agent: Map the transcript pipeline'});
 await expect(panel).toHaveAttribute('data-mode','dock');
 await expect(panel.locator('.subagent-panel-head')).toBeFocused();
 await expect(panel.locator('.subagent-who')).toHaveText('Explore · Map the transcript pipeline');
 await expect(panel.locator('.subagent-step[data-current]')).toHaveText('Running go test ./internal/runner/... -run Subagent');
 await expect(panel.locator('.subagent-panel-head, .subagent-panel-meta').locator('time'),'one elapsed time for the sub-agent').toHaveCount(1);
 // Beside a docked panel the header is slim: counts stay, their words go to the tooltip, the status line keeps its room.
 await expect(workButton).toHaveText('3 · 1',{useInnerText:true});
 await expect(workButton).toHaveAttribute('title',/^3 sub-agents · 1 command · 4 running/);
 // The card, its panel and its chip read one clock: never two different times for one sub-agent.
 for(let i=0;i<3;i++){const times=await page.evaluate(id=>[document.querySelector(`.subagent-card[data-agent-id="${id}"] .subagent-stats time`),document.querySelector('.subagent-panel .subagent-stats time'),[...document.querySelectorAll('.subagent-chip')].find(c=>c.textContent.includes('Map the transcript'))?.querySelector('time')].map(n=>n?.textContent),A);
  assert.ok(times[0]&&times.every(t=>t===times[0]),`one elapsed time everywhere: ${times.join(' / ')}`);await page.waitForTimeout(400);}
 {const box=await panel.boundingBox(),composer=await chat.locator('.composer').boundingBox();assert.ok(composer.x+composer.width<=box.x+1,'the docked panel sits beside the conversation');}
 push(20,{type:'message_delta',agentId:A,parentAgentId:'root',text:'PANEL_FOLLOWS_LIVE'});
 await expect(panel.locator('.subagent-panel-body')).toContainText('PANEL_FOLLOWS_LIVE');
 assert.ok(await panel.locator('.subagent-panel-body').evaluate(n=>n.scrollHeight-n.scrollTop-n.clientHeight<40),'the panel follows the latest step');
 await page.screenshot({path:'.cache/subagents-polish-panel.png'});
 // Resize from its edge with the keyboard.
 {const handle=panel.getByRole('separator',{name:'Resize sub-agent panel'});const before=Number(await handle.getAttribute('aria-valuenow'));await handle.focus();await page.keyboard.press('ArrowLeft');await expect(handle).toHaveAttribute('aria-valuenow',String(before+24));}
 // Float it and move it by its header.
 await panel.getByRole('button',{name:'Float the panel'}).click();
 await expect(panel).toHaveAttribute('data-mode','float');
 {const head=panel.locator('.subagent-panel-head'),start=await panel.boundingBox();await head.focus();await page.keyboard.press('ArrowLeft');await page.keyboard.press('ArrowDown');const moved=await panel.boundingBox();assert.ok(moved.x<start.x&&moved.y>start.y,'arrow keys move a floating panel');}
 await page.screenshot({path:'.cache/subagents-panel-float.png'});
 await page.keyboard.press('Escape');
 await expect(panel).toHaveCount(0);

 // ■ Stop on a running card, in its header beside the panel button: one click sends it to the Runner.
 const stop=card(A).getByRole('button',{name:'Stop sub-agent Map the transcript pipeline'});
 await expect(stop).toHaveText('Stop');
 await expect(stop).toHaveAttribute('title','Stop this sub-agent; the main agent continues');
 {const head=await card(A).locator(':scope > .subagent-head').boundingBox(),box=await stop.boundingBox();assert.ok(box.y>=head.y&&box.y+box.height<=head.y+head.height+0.5,'Stop sits in the header row');}
 await expect(card(D).getByRole('button',{name:/^Stop /}),'an ended one has no Stop').toHaveCount(0);
 // The same on its chip and its row in Background work, and for a background command.
 await expect(tray.locator('.subagent-chip-item').filter({hasText:'Map the transcript'}).getByRole('button',{name:'Stop sub-agent Map the transcript pipeline'})).toHaveCount(1);
 await expect(tray.locator('.subagent-chip-item').filter({has:page.locator('.subagent-chip.command')}).getByRole('button',{name:/^Stop background command pytest/})).toHaveCount(1);
 await tray.locator('.subagent-tray-all').click();
 {const list=page.getByRole('dialog',{name:'Background work',exact:true});
  await expect(list.locator('li[data-status="running"]').getByRole('button',{name:/^Stop /})).toHaveCount(4);
  await expect(list.locator('li[data-status="failed"], li[data-status="done"]').getByRole('button',{name:/^Stop /})).toHaveCount(0);
  await page.keyboard.press('Escape');}
 await stop.click();
 await expect.poll(()=>f.mutations.findLast(m=>m.method==='Apply'&&m.body.stopAgent)?.body.stopAgent.agentId).toBe(A);
 await expect(card(A).getByRole('button',{name:/^Stopping sub-agent/})).toHaveText('Stopping…');
 await expect(card(A).getByRole('button',{name:/^Stopping sub-agent/})).toBeDisabled();
 await expect(card(A)).toHaveAttribute('data-status','running');
 // A child can keep working after its parent stops.
 const child='claude-tool:surviving-child';
 push(24,{type:'agent_status',agentId:child,parentAgentId:A,name:'Inspect the retry path',status:'running',agentKind:'local_agent'});
 // The Runner journals the stop it accepted, then Claude reports the end.
 push(25,{type:'operation_accepted',agentId:A});
 push(30,{type:'agent_status',agentId:A,status:'stopped',data:{toolUseId:'toolu_explore'}});
 await expect(card(A)).toHaveAttribute('data-status','stopped');
 await expect(card(A).locator('.subagent-outcome strong')).toHaveText('Stopped by you');
 await expect(card(A).getByRole('button',{name:/Stop sub-agent/})).toHaveCount(0);
 await expect(tray.locator('.subagent-chip').filter({hasText:'Inspect the retry path'})).toBeVisible();
 await tray.locator('.subagent-tray-all').click();
 await expect(tree.locator(`li[data-id="${child}"]`)).toHaveAttribute('data-depth','0');
 await expect(tree.locator(`li[data-id="${A}"]`)).toHaveCount(0);
 await page.keyboard.press('Escape');
 push(31,{type:'agent_status',agentId:child,status:'done'});
 await expect(tray.locator('.subagent-chip:not(.command)')).toHaveCount(1);
 // From the keyboard: Delete on a running card's header stops it too.
 const stopsBefore=f.mutations.filter(m=>m.method==='Apply'&&m.body.stopAgent).length;
 await card(B).locator(':scope > .subagent-head > .subagent-toggle').focus();
 await expect(card(B).locator(':scope > .subagent-head > .subagent-toggle')).toHaveAttribute('aria-keyshortcuts',/Delete/);
 await page.keyboard.press('Delete');
 await expect.poll(()=>f.mutations.filter(m=>m.method==='Apply'&&m.body.stopAgent).length).toBe(stopsBefore+1);
 assert.equal(f.mutations.findLast(m=>m.method==='Apply'&&m.body.stopAgent).body.stopAgent.agentId,B);
 await expect(card(B).getByRole('button',{name:/^Stopping sub-agent/})).toBeVisible();
 // A stop the Runner answers "not running": the card ends at once, quietly; no raw error anywhere.
 const G='claude-tool:toolu_gone';
 push(40,{type:'tool_activity',data:call('toolu_gone','Task','running',{input:{description:'Count the call sites',prompt:'Count them.'}})});
 push(50,{type:'tool_activity',agentId:G,data:call('g1','Grep','running',{input:{pattern:'stopAgent'}})});
 await card(G).getByRole('button',{name:'Stop sub-agent Count the call sites'}).click();
 await expect(card(G)).toHaveAttribute('data-status','unreported');
 await expect(card(G).locator('.subagent-outcome')).toContainText('Ended without a report');
 await expect(card(G).locator('.subagent-outcome')).toContainText('no longer running when you asked to stop it');
 await expect(card(G).locator('.subagent-mark')).toHaveAttribute('data-status','unreported');
 await expect(page.getByText(/ConnectError|failed_precondition|Could not stop/)).toHaveCount(0);
 await page.screenshot({path:'.cache/subagents-stop.png'});

 // The failed one is in the parent's status line until it is looked at.
 const attention=chat.locator('.chat-heading .subagent-attention');
 await expect(attention).toContainText('1 failed');
 await attention.getByRole('button',{name:'1 sub-agent failed',exact:true}).click();
 await expect(card(D)).toBeInViewport();
 await expect(attention).toHaveCount(0);
 await reviewFailed();

 // Reduced motion stills every animation.
 assert.equal(await card(B).locator('.subagent-mark').first().evaluate(n=>getComputedStyle(n).animationName),'live-spin');
 await page.emulateMedia({reducedMotion:'reduce'});
 assert.equal(await card(B).locator('.subagent-mark').first().evaluate(n=>getComputedStyle(n).animationName),'none');
 assert.equal(await card(B).locator('.subagent-now-text').first().evaluate(n=>getComputedStyle(n).animationName),'none');
 await page.emulateMedia({reducedMotion:'no-preference'});

 // The turn ends under work nobody reported the end of. A foreground sub-agent left "running" ends
 // without a report: no spinner, its clock frozen at its last step. A forked skill whose call was
 // stopped (Claude's "doesn't want to proceed") is stopped, and its line says so quietly.
 const F='claude-tool:toolu_fg',SK='attempt-0:claude-tool:toolu_fork';
 const REJECTED="The user doesn't want to proceed with this tool use. The tool use was rejected (eg. if it was a file edit, the new_string was NOT written to the file). STOP what you are doing and wait for the user to tell you how to proceed.";
 push(60,{type:'tool_activity',data:call('toolu_fg','Task','running',{input:{description:'Trace the stale timer',prompt:'Find why the clock grows.'}})});
 push(70,{type:'tool_activity',agentId:F,data:call('f1','Bash','running',{input:{command:'grep -n agentElapsed packages/surfaces/src'}})});
 push(80,{type:'tool_activity',agentId:F,data:call('f1',undefined,'completed',{output:'subagent-model.ts:611'})});
 push(90,{type:'tool_activity',data:call('toolu_fork','Skill','running',{input:{skill:'code-review',args:'xhigh'}})});
 push(100,{type:'tool_activity',agentId:SK,data:call('s1','Bash','running',{input:{command:'git diff main...HEAD --stat'}})});
 push(110,{type:'tool_activity',agentId:SK,data:call('s1',undefined,'completed',{output:' 3 files changed'})});
 await expect(card(F)).toHaveAttribute('data-status','running');
 await expect(messages.locator('.subagent-card').filter({hasText:'code-review'})).toHaveAttribute('data-status','running');
 push(200,{type:'operation_accepted',data:{kind:'interrupt'}});
 push(210,{type:'tool_activity',data:{type:'tool_call',id:'toolu_fork',status:'failed',output:REJECTED}});
 push(220,{type:'turn_ended',status:'interrupted'});
 await expect(card(F)).toHaveAttribute('data-status','unreported');
 await expect(card(F).locator('.subagent-outcome strong')).toHaveText('Ended without a report');
 assert.equal(await card(F).locator('.subagent-mark').evaluate(n=>getComputedStyle(n).animationName),'none','no spinner');
 await expect(card(F).getByRole('button',{name:/^Stop /})).toHaveCount(0);
 {const first=await card(F).locator('.subagent-stats time').textContent();await page.waitForTimeout(2200);assert.equal(await card(F).locator('.subagent-stats time').textContent(),first,'the clock is frozen');}
 const fork=messages.locator('.subagent-card').filter({hasText:'code-review'});
 await expect(fork).toHaveAttribute('data-status','stopped');
 await expect(fork.locator('.subagent-who')).toHaveText('skill · code-review');
 const skillLine=chat.locator('.tool-line[data-tool-id="toolu_fork"]');
 await expect(skillLine).toHaveAttribute('data-state','stopped');
 await expect(skillLine.locator('.tool-line-stopped')).toHaveText('Stopped');
 await expect(skillLine.locator('.tool-line-problem')).toHaveCount(0);
 await expect(chat.locator('.messages:visible')).not.toContainText("doesn't want to proceed");
 await skillLine.locator(':scope > .tool-line-row').click();
 await expect(skillLine.locator('.tool-details')).toContainText("doesn't want to proceed");
 // A background command outlives the turn: still live, its clock still running.
 await expect(bgChip).toBeVisible();
 {const first=await bgChip.locator('time').textContent();await expect.poll(()=>bgChip.locator('time').textContent(),{timeout:4000}).not.toBe(first);}
 await page.screenshot({path:'.cache/subagents-stale.png'});

 push(230,{type:'agent_status',agentId:'claude-tool:finished-command',parentAgentId:'root',status:'done',name:'pwd',agentKind:'local_bash',data:{toolUseId:'finished-command',taskType:'local_bash'}});
 // Clear finished: the list leaves out what ended, counts follow, Undo brings it back; running work stays.
 await tray.locator('.subagent-tray-all').click();
 {const list=page.getByRole('dialog',{name:'Background work',exact:true});
  const rows=()=>list.locator('li[data-id]').count();
  await expect(list.getByRole('region',{name:'Sub-agents'})).toHaveCount(0);
  await expect.poll(rows).toBe(2);
  const before=await rows();
  await list.getByRole('button',{name:'Clear finished'}).click();
  await expect(list.locator('li[data-id]')).toHaveCount(1);
  await expect(list.locator('li[data-id]')).toContainText('pytest tests/test_mcp.py');
  await expect(list.locator('.subagent-tree-cleared')).toContainText(/^Cleared \d+ · Undo$/);
  await expect(list.locator('.subagent-tree-summary')).toContainText('1 command');
  await expect(list.locator('.subagent-tree-summary')).not.toContainText('sub-agent');
  await expect(list.getByRole('button',{name:'Clear finished'})).toHaveCount(0);
  await expect(card(E),'the conversation keeps them').toBeVisible();
  await list.getByRole('button',{name:'Undo'}).click();
  await expect.poll(rows).toBe(before);
  await list.getByRole('button',{name:'Clear finished'}).click();
  await page.keyboard.press('Escape');}
 // Per conversation, kept across a reload.
 await page.reload();
 await tray.locator('.subagent-tray-all').click();
 await expect(page.getByRole('dialog',{name:'Background work',exact:true}).locator('li[data-id]')).toHaveCount(1);
 await page.keyboard.press('Escape');

 // Phone width: the tray scrolls, a panel covers the chat as a sheet.
 await page.setViewportSize({width:390,height:844});
 await expect(tray.locator('.subagent-chip').first()).toBeVisible();
 assert.ok(await page.evaluate(()=>document.documentElement.scrollWidth<=window.innerWidth+1),'no horizontal page scroll');
 // A narrow card keeps its numbers and actions inside it, on a row of their own.
 await card(B).scrollIntoViewIfNeeded();
 for(const id of [B,D]){const box=await card(id).boundingBox(),head=card(id).locator(':scope > .subagent-head');
  for(const b of await head.locator('button,.subagent-stats').evaluateAll(n=>n.map(e=>{const r=e.getBoundingClientRect();return {left:r.left,right:r.right};})))
   assert.ok(b.left>=box.x-0.5&&b.right<=box.x+box.width+0.5,`header controls stay inside the card at phone width (${JSON.stringify(b)} in ${box.x}..${box.x+box.width})`);}
 await card(B).screenshot({path:'.cache/subagents-polish-narrow.png'});
 await card(B).getByRole('button',{name:'Open Audit worker events in a panel'}).click();
 const sheet=chat.getByRole('complementary',{name:'Sub-agent: Audit worker events'});
 await expect(sheet).toHaveAttribute('data-mode','sheet');
 // It and the sub-agent it started both ended unreported: said once, by it, not again by the nested card.
 await expect(sheet.locator('.subagent-outcome').filter({hasText:'Ended without a report'})).toHaveCount(1);
 await page.screenshot({path:'.cache/subagents-phone.png'});
 await sheet.getByRole('button',{name:'Close sub-agent panel'}).click();
 await expect(sheet).toHaveCount(0);
 // Opening a conversation that spawned 80 sub-agents (~10k events) stays fast; only the tail is mounted.
 await page.setViewportSize({width:1440,height:960});
 {const started=Date.now();await page.goto(`${base}/?view=workspace&collection=beta&session=beta-session#token=${f.token}`);
  const fan=page.locator('.embedded-chat:visible').first().locator('.messages:visible');
  await expect(fan.locator('.subagent-card[data-agent-id="claude-tool:fan79"] .subagent-outcome')).toContainText('FAN_END_79',{timeout:20000});
  const elapsed=Date.now()-started;assert.ok(elapsed<6000,`80 sub-agents opened in ${elapsed} ms`);
  const mounted=await fan.locator('.subagent-anchor').count();assert.ok(mounted>0&&mounted<=40,`only the latest cards are mounted (${mounted})`);
  console.log(`80 sub-agents, ${many.events.length} events: latest card in ${elapsed} ms`);}
 assert.deepEqual(errors,[]);
 console.log('PASS sub-agent cards (order, "kind · task" title without placeholders or generic kinds, live action, one elapsed time on one clock, tools, tokens, progress, outcome, full result, controls inside the header row at any width), timeline (prompt, prose, steps as the chat’s one-line tool lines with facts on the right and no time under 1s, reads grouped and a re-read file counted, failed step stands out, latest 5 with "Show all", current step pinned, no duplicate tooltip, windowed long run), header and tray counts in words, tray (parallel chips, nested badge, keyboard, reveal), tree, panel (dock, follow, resize, float, move, Esc), stop, failed attention, reduced motion, phone sheet, background commands (short names, no card, script and output panel, header access), 80-sub-agent conversation opens fast. Fixture only.');
}finally{await browser?.close();await server.close();await f.close();}
