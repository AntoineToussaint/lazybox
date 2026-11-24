import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from './fixtures/vite-cache.mjs';
import react from '@vitejs/plugin-react';
import {chromium,webkit,expect} from './fixtures/playwright.mjs';
import {fileURLToPath} from 'node:url';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
const fixture=await startRunnerFixture();
const channel=fixture.openEvents('alpha-session');
channel.push({type:'operation_accepted',text:'FIRST_PROMPT'});
channel.push({type:'message_delta',text:'ANSWER_BEGINNING\n\n'});
fixture.sessions.push({...fixture.sessions[0],id:'saved-session',title:'Saved conversation',closed:true,historical:true,status:'stopped',providerReference:'native-reference',conversationId:'saved-session'});
fixture.sessions.push({...fixture.sessions[0],id:'deleted-session',collectionId:'deleted-workspace',title:'Retained deleted workspace conversation',closed:true,historical:true,resumeBlocked:'Workspace deleted'});
const server=await createServer({...testViteCache(import.meta.url),configFile:false,root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
let browser;
try {
 await server.listen();browser=await(process.env.TEST_WEBKIT?webkit:chromium).launch({headless:true});
 const page=await browser.newPage({viewport:{width:1440,height:900}});page.setDefaultTimeout(10000);
 const url=`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`;
 await page.goto(url);
 await page.getByLabel('Actions for Alpha feature',{exact:true}).click();
 await page.getByRole('button',{name:'Rename workspace',exact:true}).click();
 const editor=page.getByLabel('Workspace name',{exact:true});await editor.fill('Alpha renamed');
 assert.equal(await editor.locator('xpath=ancestor::*[contains(@class,"workspace-row")]').count(),1,'rename belongs to its row');
 await page.locator('.workspace-row').filter({hasText:'Beta release'}).locator('button').first().click();
 await page.getByRole('button',{name:'Save name',exact:true}).click();
 await expect.poll(()=>fixture.mutations.filter(m=>m.method==='RenameCollection').length).toBe(1);
 assert.deepEqual(fixture.mutations.at(-1).body,{collectionId:'alpha',name:'Alpha renamed'});
 await page.locator('.workspace-row').filter({hasText:'Alpha renamed'}).locator('button').first().click();
 await expect(page.locator('.messages:visible')).toContainText('FIRST_PROMPT');
 for(let i=0;i<805;i++)channel.push({type:'message_delta',text:`part${i} `});
 await expect(page.locator('.messages:visible')).toContainText('part804');
 await expect(page.locator('.messages:visible')).toContainText('FIRST_PROMPT');await expect(page.locator('.messages:visible')).toContainText('ANSWER_BEGINNING');
 for(let i=0;i<110;i++){channel.push({type:'operation_accepted',text:`Prompt ${i}`});channel.push({type:'message_delta',text:`Reply ${i}\n\n${'long answer '.repeat(30)}`});}
 await expect(page.locator('.messages:visible')).toContainText('Reply 109');
 const pane=page.locator('.messages:visible');
 // Earlier history: the button mounts the next step, and nearing the top mounts it on its own.
 await expect(pane).not.toContainText('FIRST_PROMPT');await page.getByRole('button',{name:'Show earlier messages'}).click();
 await expect.poll(async()=>{await pane.evaluate(node=>{node.scrollTop=0;node.dispatchEvent(new Event('scroll'));});return (await pane.textContent()).includes('FIRST_PROMPT');},{message:'scrolling to the top reaches the start of the conversation'}).toBe(true);
 await expect(page.getByRole('button',{name:'Show earlier messages'})).toHaveCount(0);
 await pane.evaluate(node=>{node.scrollTop=100;node.dispatchEvent(new Event('scroll'));});
 const scroll=await pane.evaluate(node=>node.scrollTop);
 channel.push({type:'message_delta',text:'NEW_LIVE_CHUNK'});await expect(pane).toContainText('NEW_LIVE_CHUNK');
 assert.ok(Math.abs(await pane.evaluate(node=>node.scrollTop)-scroll)<2,'incoming output preserves reading position');
 await page.getByRole('button',{name:'Jump to latest'}).click();
 await expect.poll(()=>pane.evaluate(node=>node.scrollHeight-node.scrollTop-node.clientHeight)).toBeLessThan(5);
 await page.getByRole('button',{name:'Conversation history',exact:true}).click();
 const history=page.getByRole('dialog',{name:'Conversation history',exact:true});await history.getByRole('searchbox').fill('Retained deleted');
 await history.getByRole('button',{name:/Retained deleted workspace conversation/}).click();
 await expect(page.locator('.messages:visible')).toContainText('Retained conversation for deleted-session');
 // Opening old history must not create or resume an execution.
 assert.deepEqual(fixture.mutations.map(m=>m.method),['RenameCollection']);
 // A fresh window gets an independent reader, even if the main window cached Beta.
 const gap=fixture.openEvents('beta-session');gap.push({type:'message_delta',text:'gap'});gap.events[0].cursor='2';
 const gapPage=await browser.newPage();await gapPage.goto(url.replace('collection=alpha','collection=beta').replace('session=alpha-session','session=beta-session'));
 await expect(gapPage.getByRole('alert').filter({hasText:'history is missing'})).toBeVisible();await expect(gapPage.getByRole('alert').filter({hasText:'Replay gap'})).toHaveCount(0);
 await expect(gapPage.locator('.messages:visible')).not.toContainText('READY TO WORK');
 gap.events[0].cursor='1';await gapPage.getByRole('button',{name:'Retry conversation'}).click();
 await expect(gapPage.locator('.messages:visible')).toContainText('gap');await expect(gapPage.getByRole('alert').filter({hasText:'history is missing'})).toHaveCount(0);await gapPage.close();
 channel.push({type:'operation_accepted',text:'Show a file'});
 channel.push({type:'message_delta',text:'Open [the file](api/README.md#L2) or `api/README.md:2`.'});
 await page.locator('.workspace-row').filter({hasText:'Alpha renamed'}).locator('button').first().click();
 await page.getByRole('navigation',{name:'Workspace conversations'}).getByRole('button',{name:/Conversation alpha-session/}).click();
 await page.getByRole('button',{name:'the file',exact:true}).click({modifiers:['Alt']});
 const preview=page.getByRole('dialog',{name:'File preview',exact:true});await expect(preview).toContainText('A retained file.');await expect(preview.locator('.monaco-editor')).toBeVisible();assert.ok((await preview.boundingBox()).height>700,'file surface must use the available height');
 assert.equal(fixture.calls.filter(call=>call.method==='ReadWorkspaceFile').at(-1).body.path,'api/README.md');await page.screenshot({path:'/tmp/lazybox-file-preview.png'});await page.getByRole('button',{name:'Close file preview'}).click();
 await page.getByRole('navigation',{name:'Workspace conversations'}).getByRole('button',{name:/Saved conversation/}).click();
 await expect(page.getByRole('textbox',{name:'Message',exact:true})).toBeEnabled();
 const before=fixture.mutations.length;assert.equal(fixture.mutations.some(m=>m.method==='ResumeSession'),false);
 await page.getByRole('textbox',{name:'Message',exact:true}).fill('Continue the saved work');await page.getByRole('button',{name:'Send message',exact:true}).click();
 await expect.poll(()=>fixture.mutations.slice(before).filter(m=>m.method==='Apply').length).toBe(1);
 const sent=fixture.mutations.slice(before);assert.deepEqual(sent.map(m=>m.method),['ResumeSession','Apply']);assert.equal(sent[1].body.sessionId,'saved-session-continued');assert.equal(sent[0].body.processesStopped,undefined);
 // A reopened window anchored to an earlier attempt follows its successor without launching another.
 const restored=await browser.newPage();await restored.goto(url.replace('alpha-session','saved-session'));
 await expect(restored.locator('.messages:visible')).toContainText('saved-session-continued');
 assert.equal(fixture.mutations.filter(m=>m.method==='ResumeSession').length,1);await restored.close();
 // A conversation the Runner failed when it lost its stream from a live agent (Dev: its own write hit a full disk)
 // recovers once the Runner re-adopts the agent: the banner goes, the composer sends, and the loss stays as one line of history.
 const alpha=fixture.sessions.find(s=>s.id==='alpha-session');const lostAt=Date.parse('2026-11-17T11:39:51Z');
 channel.push({type:'process_exit',status:'unknown',text:'database or disk is full (13)'},new Date(lostAt).toISOString());
 channel.push({type:'process_exit',status:'unknown',text:'provider supervisor ended'},new Date(lostAt+4).toISOString());
 Object.assign(alpha,{fault:'provider stream ended: provider supervisor ended',status:'failed'});
 await page.locator('.workspace-row').filter({hasText:'Alpha renamed'}).locator('button').first().click();
 await page.getByRole('navigation',{name:'Workspace conversations'}).getByRole('button',{name:/Conversation alpha-session/}).click();
 const banner=page.getByRole('alert').filter({hasText:'The connection to the agent ended unexpectedly.'});
 await expect(banner).toBeVisible();await expect(banner.getByRole('button',{name:'Resume',exact:true})).toBeVisible();
 const lost=page.locator('.system-event.notice[data-kind="connection-lost"]');
 await expect(lost).toHaveCount(1);await expect(lost).toContainText('Lazybox lost the connection to the agent at');await expect(lost).not.toContainText('reconnected');
 const composerBox=page.getByRole('textbox',{name:'Message',exact:true});
 await composerBox.fill('Are you still there?');await expect(page.getByRole('button',{name:'Send message',exact:true})).toBeDisabled();
 const applied=()=>fixture.mutations.filter(m=>m.method==='Apply'&&m.body.sessionId==='alpha-session').length,appliedBefore=applied();
 // The next Runner re-adopts the live agent: the Runner settles the fault, and the agent's buffered turn finishes.
 Object.assign(alpha,{fault:'',status:'ready'});
 channel.push({type:'runner_reattached',status:'native_process_adopted'},new Date(lostAt+2_600_000).toISOString());
 channel.push({type:'message_delta',text:'RECOVERED_REPLY'});channel.push({type:'turn_ended',status:'completed'});
 await expect(banner).toHaveCount(0);await expect(page.locator('.messages:visible')).toContainText('RECOVERED_REPLY');
 await expect(lost).toContainText('reconnected at');await expect(lost).toHaveAttribute('data-tone','muted');
 await expect(page.locator('.runner-mark')).toHaveCount(0);
 await expect(composerBox).toBeEnabled();await composerBox.fill('Carry on');await page.getByRole('button',{name:'Send message',exact:true}).click();
 await expect.poll(applied).toBe(appliedBefore+1);
 // Resume on a conversation that only lost its stream reconnects it in place: no second conversation.
 Object.assign(alpha,{fault:'provider stream ended: session host connection lost',status:'failed'});
 await expect(banner).toBeVisible();await banner.getByRole('button',{name:'Resume',exact:true}).click();
 await expect(banner).toHaveCount(0);
 assert.deepEqual(fixture.mutations.filter(m=>m.method==='ResumeSession').at(-1).body,{sessionId:'alpha-session'});
 assert.equal(fixture.sessions.some(s=>s.id==='alpha-session-continued'),false,'Resume started a second conversation');
 await page.screenshot({path:'/tmp/lazybox-stabilization.png'});
 console.log('PASS: rename identity and placement, retained long transcript, earlier messages, reading position, deleted workspace history, visible replay failure/retry, file preview, send-to-resume and successor restoration, a conversation failed by a lost stream recovering on re-adoption and Resume reconnecting in place. Fixture only.');
} finally {await browser?.close();await server.close();await fixture.close();}
