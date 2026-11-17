// Ask Lazybox: a help assistant's settings change is shown as current → proposed
// values with its reason before the user allows it. Isolated RPC fixture only.
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from 'vite';import react from '@vitejs/plugin-react';import {chromium,expect} from '@playwright/test';import {fileURLToPath} from 'node:url';import assert from 'node:assert/strict';import {mkdir} from 'node:fs/promises';import {startRunnerFixture} from './fixtures/runner-ui.mjs';
const fixture=await startRunnerFixture();const session=fixture.sessions[0];session.busy=true;session.agents=[{id:'root',status:'running'}];
const encode=value=>Buffer.from(JSON.stringify(value)).toString('base64');
const change={revision:1,reason:'You asked new coding sessions to ask before running tools.',changes:{permission:'ask',claudeContext:'Prefer small, reviewable commits.'}};
// Claude names the tool through its MCP server; Codex sends the Runner tool name with its arguments.
const claude={type:'approval',requestId:'settings-1',text:'mcp__lazybox_settings__update_settings',providerDataJson:encode({tool:'mcp__lazybox_settings__update_settings',input:change,toolUseId:'tool-1'})};
session.pending=[claude];session.status='waiting_approval';
const channel=fixture.openEvents(session.id);channel.push(claude);
const server=await createServer({...testViteCache(import.meta.url),configFile:false,root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});let browser;
try{
 await server.listen();browser=await chromium.launch({headless:true});const page=await browser.newPage({viewport:{width:1450,height:1050}});page.setDefaultTimeout(15000);const errors=[];page.on('pageerror',e=>errors.push(String(e)));
 await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`);
 const card=page.locator('.request');await expect(card.getByRole('heading',{name:'Change Lazybox settings',exact:true})).toBeVisible();
 const proposal=card.getByRole('region',{name:'Proposed settings change'});await expect(proposal).toContainText(change.reason);
 const permission=proposal.locator('dl>div').filter({hasText:'Default permissions'});
 await expect(permission.getByLabel('Current value')).toHaveText('Current CLI settings');await expect(permission.getByLabel('Proposed value')).toHaveText('Ask for approval');
 const context=proposal.locator('dl>div').filter({hasText:'Additional Claude context'});
 await expect(context.getByLabel('Current value')).toHaveText('(empty)');await expect(context.getByLabel('Proposed value')).toHaveText(change.changes.claudeContext);
 await expect(card.getByText('Requested input')).toHaveCount(0);await expect(proposal.getByRole('alert')).toHaveCount(0);
 await mkdir('.cache',{recursive:true});await page.screenshot({path:'.cache/settings-approval.png',fullPage:true});
 await card.getByRole('button',{name:'Allow once',exact:true}).click();
 await expect.poll(()=>fixture.mutations.some(m=>m.method==='Apply'&&JSON.stringify(m.body).includes('settings-1'))).toBe(true);
 session.pending=[];channel.push({type:'request_resolved',requestId:'settings-1'});await expect(card).toHaveCount(0);
 // A proposal made against an older revision is flagged before the user allows it.
 const codex={type:'approval',requestId:'settings-2',text:'Change Lazybox settings',providerDataJson:encode({threadId:'thread-1',turnId:'turn-1',tool:'lazybox_update_settings',arguments:{...change,revision:0,changes:{naming:'generated'}}})};
 session.pending=[codex];channel.push(codex);
 await expect(card.getByRole('heading',{name:'Change Lazybox settings',exact:true})).toBeVisible();
 const naming=card.locator('dl>div').filter({hasText:'Suggested workspace names'});await expect(naming.getByLabel('Current value')).toHaveText('Repository names');await expect(naming.getByLabel('Proposed value')).toHaveText('Memorable generated names');
 await expect(card.getByRole('alert')).toContainText('Lazybox will refuse this change');
 assert.deepEqual(errors,[]);console.log('PASS: settings approvals from Claude and Codex show the reason, current → proposed values and a stale-revision warning. Fixture only.');
}finally{await browser?.close();await server.close();await fixture.close();}
