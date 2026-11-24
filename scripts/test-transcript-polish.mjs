// Fixture only: what Lazybox itself puts in a conversation reads as quiet
// events, never as the person's words; the background work list counts each
// kind apart and keeps long lists short; the token cost says what it is made
// of and what has no price. Screenshots: .cache/transcript-polish-*.png, in
// dark and light.
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from 'vite';import react from '@vitejs/plugin-react';import {webkit,chromium,expect} from './fixtures/playwright.mjs';import {fileURLToPath} from 'node:url';import {mkdir} from 'node:fs/promises';import assert from 'node:assert/strict';import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import { collectPageErrors } from './fixtures/page-errors.mjs';

const f=await startRunnerFixture();
const b64=value=>Buffer.from(JSON.stringify(value)).toString('base64');
const now=Date.now(),at=offset=>new Date(now+offset).toISOString();
Object.assign(f.sessions[0],{title:'Wire the auth service',provider:'claude',model:'claude-sonnet-4-6',modelReported:true,status:'ready',busy:false,agents:[{id:'root',status:'ready'}]});
const ch=f.openEvents('alpha-session');
const push=(offset,event)=>ch.push(event.data===undefined?event:{...Object.fromEntries(Object.entries(event).filter(([k])=>k!=='data')),providerDataJson:b64(event.data)},at(offset));
const origin=(kind,summary,extra={})=>({files:[],origin:{kind,summary,detail:'',copy:'',from:'',...extra}});
const PATH='/Users/me/.cache/environments/dev/collections/heavily-heroic-firefly/authority';
const NOTE=`[Lazybox] You asked to add codefly-dev/authority to this workspace. The person added it:\n- authority at ${PATH} (branch feature/revenge)\nContinue with what you needed it for.`;
const ASK='Codex asked Claude: why does Handle log the error instead of returning it?';
push(-600000,{type:'operation_accepted',text:'Wire the auth service into the API, then get it reviewed.',operationId:'p1'});
push(-599000,{type:'turn_started'});
push(-598000,{type:'message_delta',text:'The API needs `codefly-dev/authority`; I asked to add it to the workspace.'});
push(-597000,{type:'turn_ended'});
// Hot reloads: the Runner restarts thirteen times while the agent works on.
for(let i=0;i<13;i++)push(-590000+i*1000,{type:'runner_reattached',status:'native_process_adopted'});
push(-570000,{type:'operation_accepted',text:NOTE,operationId:'repository-note-rr-1',data:origin('repository_added','Added authority to this workspace',{detail:'feature/revenge',copy:PATH})});
push(-569000,{type:'operation_accepted',text:'Please address these 3 review comments on the workspace’s changes.\n\n### 1. api/handler.go:42\n\n```go\nlog.Println(err)\n```\n\nReturn the error.',operationId:'review-ab12-findings-1',data:origin('review_findings','Codex review · 3 findings sent',{detail:'Round 1 of 3',from:'Codex'})});
push(-568000,{type:'operation_accepted',text:`[Lazybox] Codex (reviewing your work) asks you:\n\n> why does Handle log the error instead of returning it?\n\nReply in your next message: Lazybox passes your reply back to Codex.`,operationId:'review-ask-1',data:origin('agent_ask',ASK,{from:'Codex'})});
push(-567990,{type:'review_exchange',text:ASK,data:{review:'rv-1',workspace:'alpha'}});
push(-567000,{type:'turn_started'});
push(-566000,{type:'message_delta',text:'Handle predates the error type; I will return the error and wrap it.'});
push(-565000,{type:'turn_ended'});
// A journal from before origins: the Runner's operation id says it was Lazybox's.
push(-560000,{type:'operation_accepted',text:'[Lazybox] Round 2 of 3. Claude changed files in turn 3 after your findings.',operationId:'review-ab12-round-2'});
push(-559000,{type:'receipt'});
ch.events.at(-1).receipt={operationId:'p9',delivery:'unknown_after_runner_restart'};
const manualReview = 'Please address these 4 review comments on the workspace’s changes. Each names the file and lines.\n\n### 1. api/handler.go:42\n\nReturn the original error.';
push(-558500,{type:'operation_accepted',text:manualReview,operationId:'manual-review'});
push(-558000,{type:'operation_accepted',text:'Thanks, ship it.',operationId:'p2'});

// Background work: two sub-agents and seventeen commands, described by the agent.
const call=(id,name,status,extra={})=>({type:'tool_call',id,name,status,...extra});
const COMMANDS=['Generate the protobuf and Go code','Add postgres to the dev stack','Run the API tests','Build the web console','Lint the Go services','Seed the dev database','Rebuild the docker images','Check the migrations','Run the slow integration suite','Format the TypeScript','Regenerate the OpenAPI client','Warm the module cache','Run the browser tests','Collect coverage','Bundle the worker','Tail the API logs','Run the load test'];
const SCRIPTS=['buf generate && go generate ./...','docker compose up -d postgres','go test ./api/...','npm run build -w web-console','golangci-lint run ./...','make seed','docker compose build','go run ./cmd/migrate --check','go test -tags slow ./tests/...','npx prettier --write .','make openapi','go mod download','npx playwright test','go test -cover ./...','npm run build:worker','tail -f /tmp/api.log','k6 run load.js'];
COMMANDS.forEach((description,i)=>{const id=`bg${i}`;push(-500000+i*10000,{type:'tool_activity',data:call(id,'Bash','running',{input:{command:SCRIPTS[i],description,run_in_background:true}})});
 push(-499900+i*10000,{type:'agent_status',agentId:`claude-tool:${id}`,parentAgentId:'root',nativeId:`bash_${i}`,status:'running',name:description,agentKind:'local_bash',data:{toolUseId:id,taskType:'local_bash'}});
 push(-495000+i*10000,{type:'agent_status',agentId:`claude-tool:${id}`,status:i===3?'failed':'done',data:{toolUseId:id}});});
const spawn=(offset,toolUse,description,status)=>{const id=`claude-tool:${toolUse}`;push(offset,{type:'tool_activity',data:call(toolUse,'Task','running',{input:{description,prompt:description,subagent_type:'general-purpose'}})});
 push(offset+100,{type:'agent_status',agentId:id,parentAgentId:'root',nativeId:'task-'+toolUse,status:'running',name:description,agentKind:'local_agent',data:{toolUseId:toolUse}});
 push(offset+20000,{type:'agent_status',agentId:id,status,data:{toolUseId:toolUse}});};
spawn(-540000,'toolu_map','Map where the API authenticates requests','done');
spawn(-530000,'toolu_audit','Audit the token refresh path for races','failed');

// Usage: priced Claude usage with its cache, and Codex usage that has no price.
const usage={groups:[
 {scope:{workspace:'alpha',execution:'alpha-session',profile:'claude-default'},agent:'root',source:'sdk',model:'claude-sonnet-4-6',records:'41',unpriced:'1',incomplete:'0',nanos:'412500000',
  tokens:{input:'18400',cacheRead:'1240000',cacheWrite5m:'61000',cacheWrite1h:'0',output:'22800'},costs:{input:'55200000',cacheRead:'372000000',cacheWrite5m:'228800000',cacheWrite1h:'0',output:'342000000'},
  unpricedReasons:['Additional tool fees are not priced'],unpricedTokens:{input:'200',cacheRead:'9000',cacheWrite5m:'0',cacheWrite1h:'0',output:'300'}},
 {scope:{workspace:'alpha',execution:'alpha-session',profile:'codex-default'},agent:'reviewer',source:'sdk',model:'gpt-5.5',records:'1',unpriced:'1',incomplete:'0',tokens:{input:'90000',cacheRead:'410000',cacheWrite5m:'0',cacheWrite1h:'0',output:'12000'},unpricedReasons:['Provider cumulative usage; per-request model price unavailable']},
],contexts:[{scope:{execution:'alpha-session'},agent:'root',used:'64000',capacity:'200000',source:'sdk',observedAt:new Date(now).toISOString(),categories:[{name:'messages',tokens:'40000',kind:'used',included:true}]}]};
// The groups' nanos add up their parts.
usage.groups[0].nanos=String(Object.values(usage.groups[0].costs).reduce((s,v)=>s+Number(v),0));

const server=await createServer({...testViteCache(import.meta.url),configFile:false,logLevel:'error',root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:f.url}}}});let browser;
try{
 await server.listen();await mkdir('.cache',{recursive:true});browser=await (process.env.TEST_CHROMIUM?chromium:webkit).launch();
 const page=await browser.newPage({viewport:{width:1440,height:960},colorScheme:'dark'});page.setDefaultTimeout(15000);
 const errors=[];await collectPageErrors(page, errors);
 await page.route('**/GetTelemetry',route=>route.fulfill({json:usage}));
 await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${f.token}`);
 const chat=page.locator('.embedded-chat:visible').first(),messages=chat.locator('.messages:visible');
 await expect(messages).toContainText('Thanks, ship it.');

 // ── Lazybox's messages are events: an icon and one line, never a "You" card.
 const yours=messages.locator('article.message.operation_accepted');
 await expect(yours).toHaveCount(3);
 const reviewBox=yours.getByRole('button',{name:'Open 4 review comments'});
 await expect(reviewBox).toBeVisible();
 await reviewBox.click();
 await expect(page.getByRole('dialog',{name:'4 review comments'}).locator('pre')).toHaveText(manualReview);
 await page.keyboard.press('Escape');
 await expect(reviewBox).toBeFocused();
 await expect(yours.filter({hasText:'[Lazybox]'})).toHaveCount(0);
 await expect(yours.filter({hasText:'Please address'})).toHaveCount(0);
 const events=messages.locator('.system-event:not(.notice)');
 await expect(events).toHaveCount(4);
 const added=events.filter({has:page.locator('[data-kind]')}).first();
 await expect(messages.locator('.system-event[data-kind=repository_added] .system-event-text')).toHaveText('Added authority to this workspace');
 await expect(messages.locator('.system-event[data-kind=repository_added] .system-event-detail')).toHaveText('feature/revenge');
 await expect(messages.locator('.system-event[data-kind=review_findings] .system-event-text')).toHaveText('Codex review · 3 findings sent');
 await expect(messages.locator('.system-event[data-kind=agent_ask] .system-event-text')).toHaveText(ASK);
 await expect(messages.locator('.review-exchange'),'the question is said once').toHaveCount(0);
 await expect(messages.locator('.system-event[data-kind=review_round] .system-event-text'),'an old journal still reads as an event').toHaveText('Re-review asked');
 // Events that follow each other are one block.
 const group=messages.locator('.system-event-group').filter({has:page.locator('[data-kind=repository_added]')});
 await expect(group.locator('.system-event')).toHaveCount(3);
 await expect(group).toHaveAttribute('aria-label','3 events from Lazybox');
 // Each takes one line.
 for(const box of await events.evaluateAll(nodes=>nodes.map(n=>n.querySelector('.system-event-line').getBoundingClientRect().height)))assert.ok(box<=30,`an event is one line (${box}px)`);
 // The path is on hover and to copy; the event opens to exactly what the agent received.
 const repo=messages.locator('.system-event[data-kind=repository_added]');
 await expect(repo.locator('.system-event-toggle')).toHaveAttribute('title',new RegExp(PATH.replaceAll('/','\\/').replaceAll('.','\\.')));
 await expect(repo.getByRole('button',{name:'Copy path'})).toHaveAttribute('title',`Copy ${PATH}`);
 await page.screenshot({path:'.cache/transcript-polish-events-dark.png'});
 await repo.locator('.system-event-toggle').click();
 await expect(repo.locator('.system-event-toggle')).toHaveAttribute('aria-expanded','true');
 await expect(repo.locator('.system-event-body')).toContainText('What Lazybox told Claude');
 await expect(repo.locator('.system-event-body')).toContainText('You asked to add codefly-dev/authority to this workspace.');
 await expect(repo.locator('.system-event-body')).toContainText(PATH);
 await expect(repo.locator('.system-event-line'),'opened, its line stays on screen').toBeInViewport();
 await page.screenshot({path:'.cache/transcript-polish-event-open-dark.png'});
 await repo.locator('.system-event-toggle').focus();await page.keyboard.press('Enter');
 await expect(repo.locator('.system-event-body')).toHaveCount(0);
 await expect(repo.locator('.system-event-toggle'),'it opens and closes from the keyboard').toBeFocused();
 // Never in the prompts the person can recall or jump to.
 await chat.getByRole('textbox',{name:'Message',exact:true}).focus();
 await page.keyboard.press('ArrowUp');
 await expect(chat.getByRole('textbox',{name:'Message',exact:true})).toHaveValue('Thanks, ship it.');
 await chat.getByRole('textbox',{name:'Message',exact:true}).fill('');

 // ── Restarts: one quiet mark for thirteen, its story on hover; what one left uncertain is one line.
 const mark=messages.locator('.runner-mark');
 await expect(mark).toHaveCount(1);
 await expect(mark).toHaveAttribute('aria-label',/restarted 13 times here, last at .*nothing was interrupted/);
 await expect(mark).toHaveText('13');
 await expect(messages).not.toContainText('nothing interrupted');
 {const box=await mark.boundingBox();assert.ok(box.height<=16,`the restart mark is small (${box.height}px)`);}
 const notice=messages.locator('.system-event.notice');
 await expect(notice).toHaveCount(1);
 await expect(notice).toHaveText('Runner restarted before a message was confirmed · resend it if it is still needed');
 await expect(notice).toHaveAttribute('title',/It was not sent again/);
 await expect(messages.locator('article.message.notice')).toHaveCount(0);

 // ── Background work: each kind counted apart, newest first, older finished ones folded.
 const workButton=chat.locator('.chat-heading').getByRole('button',{name:/^Background work:/});
 await expect(workButton).toHaveAccessibleName('Background work: 2 sub-agents · 17 commands · 0 running · 2 failed');
 await expect(workButton).toHaveText('2 sub-agents · 17 commands');
 await workButton.click();
 const list=page.getByRole('dialog',{name:'Background work',exact:true});
 await expect(list.locator('.subagent-tree-summary')).toHaveText('2 sub-agents · 17 commands0 running2 failed');
 const agentsSection=list.getByRole('region',{name:'Sub-agents'}),commandsSection=list.getByRole('region',{name:'Background commands'});
 await expect(agentsSection.locator('h4')).toHaveText('Sub-agents21 failed');
 await expect(commandsSection.locator('h4')).toHaveText('Commands171 failed');
 await expect(agentsSection.locator('li .subagent-tree-name').first(),'newest first').toContainText('Audit the token refresh path for races');
 const rows=commandsSection.locator('li[data-kind=command]');
 await expect(rows).toHaveCount(6,{timeout:5000});
 await expect(rows.first().locator('.subagent-tree-name'),'newest first').toHaveText('Run the load test');
 await expect(commandsSection.locator('li[data-status=failed] .subagent-tree-name'),'a failed one is never folded away').toHaveText('Build the web console');
 await expect(rows.first().locator('.subagent-tree-command')).toHaveText('k6 run');
 // Titles are whole: what the agent said, never cut mid-phrase.
 await expect(commandsSection.locator('.subagent-tree-name')).not.toContainText(['Generate the','Add postgres']);
 await page.screenshot({path:'.cache/transcript-polish-background-dark.png'});
 await commandsSection.getByRole('button',{name:'Show 11 earlier'}).click();
 await expect(rows).toHaveCount(17);
 await expect(rows.nth(5).locator('.subagent-tree-row'),'the keyboard lands on the first of the earlier ones').toBeFocused();
 await expect(commandsSection.locator('.subagent-tree-name').last()).toHaveText('Generate the protobuf and Go code');
 await expect(commandsSection.locator('li').filter({hasText:'Add postgres to the dev stack'}).locator('.subagent-tree-command')).toHaveText('docker compose up');
 await page.keyboard.press('Escape');await expect(list).toHaveCount(0);

 // ── Usage: what the tokens are made of and what each part cost; what has no price, and why.
 await chat.getByRole('button',{name:'Context and token cost'}).click();
 const popover=page.locator('.context-popover');await expect(popover).toBeVisible();
 await popover.locator('.token-cost>summary').click();
 const table=popover.getByRole('table',{name:'This conversation: tokens and cost by part'});
 // Tokens count every model's usage; the cost, the priced usage only (the star says so).
 await expect(table.locator('tbody tr')).toHaveText([/^Input108(\.4)?K\$0\.0552$/,/^Cache read1\.7M\$0\.3720$/,/^Cache write61K\$0\.2288$/,/^Output34\.8K\$0\.3420$/]);
 await expect(table.locator('tfoot tr')).toHaveText(/^Total1\.9M\$0\.9980\*$/);
 await expect(popover.locator('.token-cost>summary')).toContainText('Partly priced');
 await expect(popover.locator('.token-cost>summary')).not.toContainText('+ unpriced');
 await expect(popover.locator('.usage-cache')).toContainText('of input came from the cache');
 const unpriced=popover.getByRole('note').filter({hasText:'Not priced'});
 await expect(unpriced.locator('li')).toHaveText([/^claude-sonnet-4-6: Additional tool fees are not priced9\.5K tokens$/,/^gpt-5\.5 \(reviewer\): Provider cumulative usage; per-request model price unavailable512K tokens$/]);
 await expect(unpriced).toContainText('running total');
 await popover.locator('.usage-by-model>summary').click();
 await expect(popover.locator('.usage-by-model .token-cost-group')).toHaveCount(2);
 await popover.locator('.token-cost').evaluate(n=>n.scrollIntoView({block:'start'}));
 await page.screenshot({path:'.cache/transcript-polish-usage-dark.png'});
 await page.keyboard.press('Escape');

 // ── Light.
 await page.emulateMedia({colorScheme:'light',reducedMotion:'reduce'});
 await messages.evaluate(n=>{n.scrollTop=n.scrollHeight;});
 await page.waitForTimeout(400);
 await page.screenshot({path:'.cache/transcript-polish-events-light.png'});
 await workButton.click();await expect(list).toBeVisible();
 await page.screenshot({path:'.cache/transcript-polish-background-light.png'});
 await page.keyboard.press('Escape');
 await chat.getByRole('button',{name:'Context and token cost'}).click();await expect(popover).toBeVisible();
 if(!(await popover.locator('.token-cost').getAttribute('open')!==null))await popover.locator('.token-cost>summary').click();
 await popover.locator('.token-cost').evaluate(n=>n.scrollIntoView({block:'start'}));
 await page.screenshot({path:'.cache/transcript-polish-usage-light.png'});
 await page.keyboard.press('Escape');

 // ── The phone sees the same events.
 await page.setViewportSize({width:390,height:844});
 await expect(messages.locator('.system-event[data-kind=repository_added] .system-event-text')).toBeVisible();
 await messages.locator('.system-event[data-kind=repository_added]').scrollIntoViewIfNeeded();
 {const width=await page.evaluate(()=>document.documentElement.scrollWidth);assert.ok(width<=390,`no sideways scroll on a phone (${width}px)`);}
 await page.screenshot({path:'.cache/transcript-polish-events-narrow-light.png'});


 // A fork leaves the live source running. Restart stops it and sends the revised prompt.

 Object.assign(f.sessions[0],{provider:'codex',profileId:'codex-default',providerReference:'source-thread',busy:true});
 push(1,{type:'turn_started',turnId:'historical-turn'});
 push(2,{type:'message_delta',text:'A completed reply for the branching fixture.',turnId:'historical-turn'});
 push(3,{type:'turn_ended',status:'completed',turnId:'historical-turn'});
 const branchRequests=[];
 const branchPage=await browser.newPage({viewport:{width:1440,height:960}});
 const branchErrors=[];
 await collectPageErrors(branchPage, branchErrors);
 await branchPage.route('**/OpenSession',async route=>{
   const {fork}=route.request().postDataJSON();
   branchRequests.push(fork);
   const session={...f.sessions[0],id:`fork-session-${branchRequests.length}`,conversationId:`fork-session-${branchRequests.length}`,busy:false,providerReference:'new-thread',forkedFromSessionId:'alpha-session',forkedFromCursor:fork.beforePromptCursor};
   f.sessions.push(session);
   f.openEvents(session.id).push({type:'message_delta',text:'Inherited reply'});
   await route.fulfill({json:{session}});
 });
 await branchPage.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${f.token}`);
 const branchMessages=branchPage.locator('.embedded-chat:visible').first().locator('.messages:visible');
 const reply=branchMessages.locator('article.message').filter({hasText:'A completed reply for the branching fixture.'});
 await reply.click({button:'right'});
 await branchPage.getByRole('menuitem',{name:'Fork after this turn…'}).click();
 await expect(branchPage.getByLabel('Workspace files')).toHaveValue('checkpoint');
 await branchPage.getByRole('button',{name:'Cancel',exact:true}).click();
 assert.equal(branchRequests.length,0);
 await reply.click({button:'right'});
 await branchPage.getByRole('menuitem',{name:'Fork after this turn…'}).click();
 await branchPage.getByLabel('Workspace files').selectOption('current');
 await branchPage.getByRole('button',{name:'Fork conversation',exact:true}).click();
 await branchPage.getByRole('button',{name:'Open previous conversation'}).click();
 await expect(reply).toBeVisible();
 assert.equal(branchRequests[0].afterTurnId,'historical-turn');
 assert.equal(f.sessions[0].busy,true);
 assert.equal(f.mutations.filter(m=>m.method==='StopSessionResources').length,0);
 await reply.click({button:'right'});
 await branchPage.getByRole('menuitem',{name:'Ask a side question…'}).click();
 await branchPage.getByLabel('Side question',{exact:true}).fill('Explain this fixture change');
 await branchPage.getByLabel('Workspace files').selectOption('current');
 await branchPage.getByRole('button',{name:'Ask in new conversation'}).click();
 await expect.poll(()=>f.mutations.findLast(m=>m.method==='Apply')?.body?.message?.text).toBe('Explain this fixture change');
 assert.equal(f.mutations.findLast(m=>m.method==='Apply').body.sessionId,'fork-session-2');
 assert.equal(branchRequests[1].afterTurnId,'historical-turn');
 assert.equal(f.mutations.filter(m=>m.method==='StopSessionResources').length,0);
 await branchPage.getByRole('button',{name:'Open previous conversation'}).click();
 const prompt=branchMessages.locator('article.message.operation_accepted').filter({hasText:'Thanks, ship it.'});
 const cursor=await prompt.getAttribute('data-message-id');
 await prompt.click({button:'right'});
 await branchPage.getByRole('menuitem',{name:'Edit and restart here…'}).click();
 await expect(branchPage.getByLabel('Revised prompt')).toHaveValue('Thanks, ship it.');
 await branchPage.getByLabel('Revised prompt').fill('Run the tests first.');
 await expect(branchPage.getByLabel('Workspace files')).toHaveValue('current');
 await branchPage.getByRole('button',{name:'Restart with revised prompt'}).click();
 await expect.poll(()=>f.mutations.findLast(m=>m.method==='Apply')?.body?.message?.text).toBe('Run the tests first.');
 assert.equal(f.sessions[0].closed,true);
 assert.equal(branchRequests.length,3);
 assert.equal(branchRequests[2].sourceSessionId,'alpha-session');
 assert.equal(branchRequests[2].beforePromptCursor,cursor);
 assert.ok(branchRequests[2].requestKey);
 await branchPage.getByRole('button',{name:'Open previous conversation'}).click();
 await expect(branchMessages.locator('article.message').filter({hasText:'A completed reply for the branching fixture.'})).toBeVisible();
 assert.deepEqual(branchErrors,[]);
 assert.deepEqual(errors,[]);
 console.log('PASS Lazybox messages as one-line events (icon, summary, detail, copy, open to the exact text, grouped, legacy journals, never "You", not recalled), one restart mark for 13 restarts and one line for an uncertain one, background work counted by kind (newest first, failed visible, older folded, whole titles), usage by part with cost and unpriced reasons, dark and light. Fixture only.');
}finally{await browser?.close();await server.close();await f.close();}
