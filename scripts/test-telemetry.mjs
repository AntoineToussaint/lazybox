import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from './fixtures/vite-cache.mjs';
import react from '@vitejs/plugin-react';
import {chromium,webkit,expect} from './fixtures/playwright.mjs';
import {fileURLToPath} from 'node:url';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import { collectPageErrors, drainPageErrors } from './fixtures/page-errors.mjs';
const fixture=await startRunnerFixture();
const server=await createServer({...testViteCache(import.meta.url),configFile:false,root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
let browser;
try{
 await server.listen();browser=await (process.env.TEST_WEBKIT?webkit:chromium).launch({headless:true});const page=await browser.newPage({viewport:{width:1500,height:1100}});page.setDefaultTimeout(10000);
 const errors=[];await collectPageErrors(page, errors);
 await page.route('**/GetTelemetry',route=>route.fulfill({json:{groups:[{scope:{workspace:'alpha',execution:'alpha-session',profile:'Account one'},source:'gateway',model:'fixture',records:'2',unpriced:'1',incomplete:'1',nanos:'12300000',tokens:{input:'1200',cacheRead:'800',output:'100'}}],contexts:[...['worker-one','worker-two'].map(agent=>({scope:{execution:'alpha-session'},agent,used:'10',capacity:'100'})),{scope:{execution:'alpha-session'},agent:'root',used:'80000',capacity:'1000000',reserve:'33000',source:'sdk_summary_estimate',observedAt:'2026-11-02T12:00:00Z',categories:[{name:'messages',tokens:'24500',kind:'used',included:true},{name:'system_tools',tokens:'20600',kind:'used',included:true},{name:'mcp_tools',tokens:'16800',kind:'used',included:true},{name:'system_prompt',tokens:'11500',kind:'used',included:true},{name:'skills',tokens:'6600',kind:'used',included:true},{name:'mcp_tools_deferred',tokens:'50000',kind:'deferred',included:false}]}]}}));
 const lifecycle=fixture.openEvents('alpha-session');

 await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace#token=${fixture.token}`);
 await page.getByRole('combobox',{name:'Workspace',exact:true}).selectOption('alpha');await page.getByRole('combobox',{name:'Session',exact:true}).selectOption('alpha-session');
 const trigger=page.getByRole('button',{name:'Context and token cost'});await expect(trigger).toContainText('8%');
 const messagesBefore=await page.locator('.messages:visible').boundingBox();await trigger.click();
 const panel=page.locator('.context-popover');await expect(panel).toBeVisible();
 assert.deepEqual(await page.locator('.messages:visible').boundingBox(),messagesBefore,'context should not displace conversation');
 for(const agentId of ['worker-one','worker-two']) lifecycle.push({type:'agent_status',agentId:'attempt-1:'+agentId,status:'running',parentAgentId:'root'});
 await expect(panel.locator('.context-card')).toHaveCount(3);
 lifecycle.push({type:'agent_status',agentId:'attempt-1:worker-one',status:'done'});
 await expect(panel.getByRole('region',{name:'worker-one context'})).toHaveCount(0);
 await expect(panel.getByRole('region',{name:'worker-two context'})).toBeVisible();
 lifecycle.push({type:'agent_status',agentId:'attempt-1:worker-two',status:'stopped'});
 await expect(panel.locator('.context-card')).toHaveCount(1);

 await expect(panel).toContainText('80K');await expect(panel).toContainText('1M');
 await expect(panel.locator('.context-deferred')).toContainText('Outside context');await expect(panel.getByRole('meter')).toHaveAttribute('aria-valuenow','80000');
 await expect(panel.locator('.context-meter>span').first()).toHaveAttribute('style',/width: 2.45%;/);
 await page.screenshot({path:'.cache/telemetry-context.png'});
 await panel.locator('.token-cost>summary').click();
 await expect(panel).toContainText('API-equivalent');
 // What has no price says which model and why, never "+ unpriced".
 await expect(panel.getByRole('note').filter({hasText:'Not priced'}).locator('li')).toHaveText('fixture: No price1 request');await expect(panel).not.toContainText('+ unpriced');
 await expect(panel).toContainText('1 request did not report all of its usage.');
 await page.keyboard.press('Escape');await expect(panel).not.toBeVisible();await expect(trigger).toBeFocused();
 await page.setViewportSize({width:420,height:850});
 // At phone width the header folds the meter into its ⋯ details (chat-header R11/R12): open that first.
 // Wait for the header to fold (it re-lays out after the resize), then open its details.
 const more=page.getByRole('button',{name:/^More details/});await expect(more).toBeVisible();await more.click();
 await trigger.click();await expect(panel).toBeVisible();const bounds=await panel.boundingBox();assert.ok(bounds.x>=0&&bounds.x+bounds.width<=420);await page.screenshot({path:'.cache/telemetry-context-narrow.png'});await page.keyboard.press('Escape');await page.setViewportSize({width:1500,height:1100});
 await page.route('**/Info',route=>route.fulfill({status:503,json:{code:'unavailable',message:'Fixture Runner restarting'}}));
 await expect(page.getByText('Runner disconnected.',{exact:true})).toBeVisible({timeout:10000});await expect(page.getByRole('button',{name:'Retry now',exact:true})).toBeVisible();
 await expect(page.getByText('Could not load saved conversations.',{exact:true})).toHaveCount(0);
 await page.unroute('**/Info');await page.getByRole('button',{name:'Retry now',exact:true}).click();await expect(page.getByText('Runner disconnected.',{exact:true})).toHaveCount(0);
 // Codex has token types for its latest request instead of semantic categories.
 const channel=fixture.openEvents('alpha-session');
 channel.push({type:'message_delta',text:'Before compaction'});
 channel.push({type:'compacted'});
 channel.push({type:'message_delta',text:'Child-only text',agentId:'child'});
 channel.push({type:'message_delta',text:'New reply'});
 channel.push({type:'tool_activity',providerDataJson:Buffer.from(JSON.stringify({type:'tool_call',id:'check',name:'Bash',status:'completed',input:{command:'/bin/zsh -lc "make check"'},output:'check output '.repeat(2000)})).toString('base64')});
 for(let i=0;i<6;i++) channel.push({type:'tool_activity',providerDataJson:Buffer.from(JSON.stringify({type:'tool_call',id:'small'+i,name:'Bash',status:'completed',input:{command:'pwd'},output:'x'.repeat(1000)})).toString('base64')});
 // The conversation an execution belongs to is known once its session is read, after the usage chip.
 // Usage is read by execution until then, by conversation after: each read's scope is kept, and the
 // conversation's figures differ from the execution's, so the page shows which one it applied.
 fixture.sessions.find(s=>s.id==='alpha-session').conversationId='alpha-conversation';
 const usageReport=used=>({
   groups:[{scope:{execution:'alpha-session'},source:'sdk',model:'gpt-6.2-sol',records:'1',nanos:'340500000',tokens:{input:'66000',cacheRead:'300000',output:'1100'}}],
   contexts:[{scope:{execution:'alpha-session'},agent:'root',model:'gpt-6.2-sol',used,capacity:'475000',source:'sdk_token_breakdown',
     categories:[['uncached_input','66000'],['cached_input','300000'],['reasoning_output','800'],['other_output','300']].map(([name,tokens])=>({name,tokens,kind:'used',included:true}))}]
 });
 const codex = await browser.newPage({viewport:{width:1500,height:1100}});
 await collectPageErrors(codex, errors);
 const reads=[];
 await codex.route('**/GetTelemetry',route=>{const scope=route.request().postDataJSON();reads.push(scope);return route.fulfill({json:usageReport(scope.conversation==='alpha-conversation'?'380000':'367100')});});
 // The session is held until the popover is open with a group unfolded.
 let releaseSession;const sessionHeld=new Promise(resolve=>{releaseSession=resolve;});
 await codex.route('**/GetSession',async route=>{await sessionHeld;await route.continue();});
 await codex.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`);
 await codex.getByRole('button',{name:'Context and token cost'}).click();
 const codexPanel=codex.locator('.context-popover');
 await expect(codexPanel).toContainText('367.1K');
 const contributors=codexPanel.getByRole('region',{name:'Transcript size'});
 await expect(contributors.locator('summary').first()).toHaveText('Tool outputs32 kB');
 await expect(contributors.getByText('Ran make check')).toBeHidden();
 await contributors.locator('summary').first().click();
 await expect(contributors.locator('.context-items').first().locator('details')).toHaveCount(5);
 assert.ok(reads.length>0&&reads.every(scope=>scope.execution==='alpha-session'&&!scope.conversation),'read by execution until the conversation is known');
 // The session answers: the read moves to the conversation. The popover stays open, keeps its
 // breakdown and what is unfolded in it, and shows the conversation's figures once they come.
 releaseSession();
 await expect.poll(()=>reads.some(scope=>scope.conversation==='alpha-conversation')).toBe(true);
 await expect(codexPanel).toContainText('380K');
 await expect(codexPanel).toBeVisible();
 await expect(contributors.locator('.context-items').first().locator('details')).toHaveCount(5);
 await expect(contributors.locator('.context-items').first()).toBeVisible();
 await contributors.getByText('Ran make check').click();
 await expect(contributors.locator('pre').first()).toBeVisible();
 await expect(contributors).toContainText('check output');
 await expect(contributors.locator(':scope > details')).toHaveCount(2);
 await expect(contributors).not.toContainText('Before compaction');
 await expect(contributors).not.toContainText('Child-only text');
 // Folding a group back is not closing the popover: the breakdown stays.
 await contributors.getByText('Ran make check').click();
 await contributors.locator('summary').first().click();
 await expect(contributors.locator('summary').first()).toHaveText('Tool outputs32 kB');
 await expect(contributors.locator('.context-items').first()).toBeHidden();
 await codexPanel.getByText('Latest request details',{exact:true}).click();
 for(const label of ['Uncached input','Cached input','Reasoning output','Other output']) await expect(codexPanel).toContainText(label);
 await expect(codexPanel).toContainText('Token types from the latest Codex request.');
 await expect(codexPanel.locator('.token-cost>summary')).toContainText('$0.3405');
 await expect(codexPanel).not.toContainText('Category breakdown not reported.');
 // Another workspace, by keyboard (no click outside to dismiss it): the popover closes.
 await expect(codexPanel).toBeVisible();
 await codex.keyboard.press('Meta+2');
 await expect(codex.getByRole('combobox',{name:'Workspace',exact:true})).toHaveValue('beta');
 await expect(codexPanel).toBeHidden();
 await drainPageErrors(errors);
 await codex.close();

 // A window that moves to another conversation reads that one's usage, never the one it left,
 // even before the new session is read: a fork opens the conversation it came from in its window.
 const alpha=fixture.sessions.find(s=>s.id==='alpha-session');
 fixture.sessions.push({...structuredClone(alpha),id:'fork-session',conversationId:'fork-conversation',title:'Forked',forkedFromSessionId:'alpha-session',forkedFromCursor:'0'});
 const forkWindow=await browser.newPage({viewport:{width:1200,height:900}});
 await collectPageErrors(forkWindow, errors);
 await forkWindow.route('**/GetTelemetry',route=>{const scope=route.request().postDataJSON();return route.fulfill({json:usageReport(scope.conversation==='fork-conversation'?'100000':'237500')});});
 // Opening the linked conversation reads its session once; the window's own reads of it wait.
 let alphaReads=0,releaseAlpha;const alphaHeld=new Promise(resolve=>{releaseAlpha=resolve;});
 await forkWindow.route('**/GetSession',async route=>{if(route.request().postDataJSON().sessionId==='alpha-session'&&++alphaReads>1)await alphaHeld;await route.continue();});
 await forkWindow.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=agent&collection=alpha&session=fork-session#token=${fixture.token}`);
 const chip=forkWindow.getByRole('button',{name:'Context and token cost'});
 await expect(chip).toContainText('21.1%');
 // With the popover open, the window moves to the other conversation by keyboard (no click outside):
 // the popover closes, and the usage shown is the new conversation's.
 await chip.click();
 const forkPanel=forkWindow.locator('.context-popover');
 await expect(forkPanel).toContainText('100K');
 await forkWindow.getByRole('button',{name:'Open previous conversation'}).focus();
 await expect(forkPanel).toBeVisible();
 await forkWindow.keyboard.press('Enter');
 await expect.poll(()=>new URL(forkWindow.url()).searchParams.get('session')).toBe('alpha-session');
 await expect(forkPanel).toBeHidden();
 await expect(chip).toContainText('50%');
 releaseAlpha();
 await drainPageErrors(errors);
 await forkWindow.close();
 await drainPageErrors(errors);assert.deepEqual(errors,[]);assert.equal(fixture.mutations.length,0);console.log('PASS: normalized context, deferred counts, partial pricing, disconnect/retry recovery. Fixture only.');
}finally{await browser?.close();await server.close();await fixture.close();}
