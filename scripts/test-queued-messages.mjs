// Fixture only: a message sent while the agent is busy waits in the queue, and
// is never stuck there behind a server or a long build. The strip above the
// message box says it is queued and what the agent is busy with; Send now
// warns which step it stops, then interrupts the turn and sends the message
// next (Escape keeps waiting); Steer gives it to a Codex turn, or a Claude one
// whose worker steers, without stopping it; for a Claude worker that cannot,
// the strip says it cannot take a message mid-turn. Several
// queued messages collapse; a phone keeps it on screen. A queued message is in
// one place at a time: the strip while it waits, never also a "You" bubble in
// the running turn; once delivered, the conversation, where it reached the
// agent (after the stop Send now made, or steered inside the turn), the same
// after a reload.
// ⌘↵ (Send now's key) sends the typed message now while the agent works, the
// oldest queued one from an empty box, and is an ordinary send while idle.
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from './fixtures/vite-cache.mjs';import react from '@vitejs/plugin-react';import {chromium,expect} from './fixtures/playwright.mjs';import {fileURLToPath} from 'node:url';import assert from 'node:assert/strict';import {mkdir} from 'node:fs/promises';import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import { collectPageErrors } from './fixtures/page-errors.mjs';

const f=await startRunnerFixture();
const b64=value=>Buffer.from(JSON.stringify(value)).toString('base64');
const now=Date.now();
const busy=(session,provider)=>Object.assign(session,{provider,busy:true,status:'running',queued:0,queuedMessages:[],agents:[{id:'root',status:'running',phase:'tool',tool:'Bash',phaseSinceUnixMillis:String(now-300000),turnStartedAtUnixMillis:String(now-310000)}]});
const [claude,codex]=f.sessions;
busy(claude,'claude');claude.sentNowReply='Switching to port 4000.';
busy(codex,'codex');Object.assign(codex,{profileId:'codex-default',capabilities:{...codex.capabilities,steer:true,subagents:true}});
const channels={};
for(const session of [claude,codex]){
 const ch=channels[session.id]=f.openEvents(session.id),at=offset=>new Date(now+offset).toISOString();
 ch.push({type:'operation_accepted',text:'Start the app and check the home page.'},at(-310000));
 ch.push({type:'turn_started'},at(-309000));
 ch.push({type:'tool_activity',providerDataJson:b64({type:'tool_call',id:'toolu_dev',name:'Bash',status:'running',input:{command:'npm run dev'}})},at(-300000));
}
// The agent goes on after a message is queued: once its words are shown, the acceptance before them has been read too.
const agentSays=async(chat,session,text)=>{channels[session.id].push({type:'message_delta',text});await expect(chat.getByText(text)).toBeVisible();};
const youSaid=(chat,text)=>chat.locator('article.message.operation_accepted',{hasText:text});
const deliveries=()=>f.mutations.filter(m=>m.method==='Apply'&&m.body.deliverQueued).map(m=>m.body.deliverQueued);
const sentAs=text=>f.mutations.findLast(m=>m.method==='Apply'&&m.body.message?.text===text)?.body.operationId;

const server=await createServer({...testViteCache(import.meta.url),configFile:false,logLevel:'error',root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:f.url}}}});let browser;
try{
 await server.listen();await mkdir('.cache',{recursive:true});browser=await chromium.launch({headless:true});const context=await browser.newContext({viewport:{width:1400,height:900}});const page=await context.newPage();page.setDefaultTimeout(15000);
 const errors=[];await collectPageErrors(page, errors);
 const open=async session=>page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=${session.collectionId}&session=${session.id}#token=${f.token}`);
 await open(claude);
 const chat=page.locator('.embedded-chat:visible').first(),composer=chat.getByRole('textbox',{name:'Message',exact:true});
 const strip=chat.getByRole('region',{name:'Queued messages'});
 // Send now's key, as the app shows it and as it is pressed.
 const mac=await page.evaluate(()=>/Mac|iPhone|iPad/.test(navigator.platform)),nowKey=mac?'⌘↵':'Ctrl+↵',sendNowKey='Meta+Enter';
 const hint=chat.locator('.composer .keyboard-hint');

 // ── Sent during a busy turn, the message waits, and the strip says why.
 await expect(chat.getByRole('region',{name:'What the agent is doing'})).toBeVisible();
 await expect(strip).toHaveCount(0);
 // While the agent works, the message box says what ↵ and Send now's key do.
 await expect(hint).toHaveText(`↵ queues · ${nowKey} sends now`);
 // The session poll lags the stream (frozen here): the strip shows the message from the stream, at once.
 f.freezeSession(claude.id);
 await composer.fill('Use port 4000 instead, 3000 is taken.');await composer.press('Enter');
 await expect(strip).toBeVisible({timeout:5000});
 await expect(strip.locator('.queued-head [role=status]')).toHaveText(/^Queued — Claude is busy \(Running a command · 5m \d\ds\)$/);
 // The ticking time is not announced: a screen reader would repeat the line every second.
 await expect(strip.locator('.queued-head [role=status] [aria-hidden=true]')).toHaveText(/^ · 5m \d\ds$/);
 const item=strip.getByRole('listitem');
 await expect(item.locator('q')).toHaveText('Use port 4000 instead, 3000 is taken.');
 // Queued is one place: the strip. The running turn shows no "You" bubble for it.
 await agentSays(chat,claude,'The dev server is still starting.');
 await expect(youSaid(chat,'Use port 4000 instead')).toHaveCount(0);
 // Claude cannot take a message mid-turn: no Steer, and the strip says so.
 await expect(item.getByRole('button',{name:'Steer'})).toHaveCount(0);
 await expect(strip.locator('.queued-note')).toHaveText(`Claude can’t take a message in the middle of a turn: Send now (${nowKey}) stops its current step first, then sends yours.`);
 await page.screenshot({path:'.cache/queued-claude.png'});

 // ── Send now is one click; its tooltip names the step it stops, and the key that sends the oldest queued message.
 // The poll still lists it after it is delivered (frozen again, with it queued): it is shown once, in the conversation.
 f.thawSession(claude.id);f.freezeSession(claude.id);
 assert.equal(f.sessions[0].queuedMessages.length,1);
 const sendNow=item.getByRole('button',{name:'Send now'});
 await expect(sendNow).toHaveAttribute('title',`Stop npm run dev and send this as the next turn (${nowKey})`);
 await page.screenshot({path:'.cache/queued-send-now.png'});
 await sendNow.click();
 await expect.poll(deliveries).toEqual([{operationId:f.mutations.find(m=>m.method==='Apply'&&m.body.message)?.body.operationId,how:'now'}]);
 assert.equal(claude.interrupted,1,'the Runner interrupts the turn');
 await expect(strip).toHaveCount(0,{timeout:5000});
 await expect(youSaid(chat,'Use port 4000 instead')).toHaveCount(1);
 f.thawSession(claude.id);
 // Claude reports the stop as an "error during execution": the conversation
 // says it in one quiet line, then the message is the next turn. No error
 // card, no fault, and never the provider's raw diagnostic.
 await expect(chat.locator('.stopped-line')).toHaveText('Stopped to send your message');
 await expect(chat.getByText('Switching to port 4000.')).toBeVisible();
 await expect(chat.locator('article.message.failure')).toHaveCount(0);
 await expect(chat.getByText(/error from the provider/i)).toHaveCount(0);
 await expect(chat.getByText(/ede_diagnostic/)).toHaveCount(0);
 await expect(chat.getByRole('alert')).toHaveCount(0);
 // Delivered, it is said once, where it reached the agent: after the turn it stopped, starting its own.
 const sentNowOrder=async()=>{
  await expect(youSaid(chat,'Use port 4000 instead')).toHaveCount(1);
  const order=await chat.locator('[data-message-id]').evaluateAll(nodes=>nodes.map(n=>n.textContent.trim()));
  const startingAt=order.findIndex(t=>t.includes('The dev server is still starting.')),stoppedAt=order.indexOf('Stopped to send your message'),queuedAt=order.findIndex(t=>t.includes('Use port 4000 instead, 3000 is taken.')),repliedAt=order.findIndex(t=>t.includes('Switching to port 4000.'));
  assert.ok(startingAt>=0&&startingAt<stoppedAt&&stoppedAt<queuedAt&&queuedAt<repliedAt,`the turn, the stop, the message, then its turn: ${JSON.stringify(order)}`);
 };
 await sentNowOrder();
 await page.screenshot({path:'.cache/queued-sent-now.png'});

 // ── Typed, then ⌘↵ while the agent works: the message goes now, never left waiting in the queue.
 claude.sentNowReply='Restarting on port 5000.';
 await composer.fill('Actually use port 5000.');await composer.press(sendNowKey);
 await expect.poll(()=>deliveries().at(-1)).toEqual({operationId:sentAs('Actually use port 5000.'),how:'now'});
 assert.equal(deliveries().length,2);assert.equal(claude.interrupted,2,'the Runner interrupts the turn for it');
 await expect(composer).toHaveValue('');
 await expect(chat.locator('.stopped-line')).toHaveCount(2);
 await expect(chat.getByText('Restarting on port 5000.')).toBeVisible();
 await expect(strip).toHaveCount(0);
 await expect(chat.getByRole('alert')).toHaveCount(0);
 {const order=await chat.locator('[data-message-id]').evaluateAll(nodes=>nodes.map(n=>n.textContent.trim()));
  const typedAt=order.findIndex(t=>t.includes('Actually use port 5000.')),stoppedAt=order.lastIndexOf('Stopped to send your message'),repliedAt=order.findIndex(t=>t.includes('Restarting on port 5000.'));
  assert.ok(typedAt>=0&&stoppedAt<typedAt&&typedAt<repliedAt,`the stop, then the typed message where the agent got it, then its turn: ${JSON.stringify(order)}`);}

 // ── Several queued messages: two shown, the rest counted, and the strip can be put away.
 for(const text of ['First follow-up','Second follow-up','Third follow-up']){await composer.fill(text);await composer.press('Enter');await expect.poll(()=>claude.queuedMessages.length).toBeGreaterThan(0);}
 await expect.poll(()=>claude.queuedMessages.length).toBe(3);
 await expect(strip.locator('.queued-head [role=status]')).toHaveText(/^3 messages queued — Claude is busy/,{timeout:5000});
 await expect(strip.getByRole('listitem')).toHaveCount(2);
 await expect(strip.locator('.queued-more')).toHaveText('Show 1 more');
 await agentSays(chat,claude,'Port 4000 is free.');
 for(const text of ['First follow-up','Second follow-up','Third follow-up'])await expect(youSaid(chat,text)).toHaveCount(0);
 // A reload shows the same: the delivered message once, in its place; the queued ones in the strip only.
 await page.reload();
 await expect(chat.getByText('Port 4000 is free.')).toBeVisible();
 await sentNowOrder();
 for(const text of ['First follow-up','Second follow-up','Third follow-up'])await expect(youSaid(chat,text)).toHaveCount(0);
 await expect(strip.locator('.queued-head [role=status]')).toHaveText(/^3 messages queued — Claude is busy/);
 await strip.getByRole('button',{name:'Hide'}).click();
 await expect(strip.getByRole('listitem')).toHaveCount(0);
 await expect(strip.getByRole('button',{name:'Show'})).toHaveAttribute('aria-expanded','false');
 await strip.getByRole('button',{name:'Show'}).click();
 await expect(strip.getByRole('listitem')).toHaveCount(2);
 await composer.fill('Delete this queued follow-up');await composer.press('Enter');
 await expect.poll(()=>claude.queuedMessages.length).toBe(4);
 await strip.getByRole('button',{name:'Show 2 more'}).click();
 const deleted=strip.getByRole('listitem',{name:'Queued message: Delete this queued follow-up'});
 const interruptedBeforeDelete=claude.interrupted;
 await deleted.getByRole('button',{name:'Delete',exact:true}).click();
 await expect(deleted).toHaveCount(0);
 await expect(youSaid(chat,'Delete this queued follow-up')).toHaveCount(0);
 assert.equal(claude.interrupted,interruptedBeforeDelete);
 await page.reload();
 await expect(strip.locator('.queued-head [role=status]')).toHaveText(/^3 messages queued/);
 await expect(youSaid(chat,'Delete this queued follow-up')).toHaveCount(0);
 // A phone keeps the strip and its buttons on screen, without sideways scroll.
 await page.setViewportSize({width:390,height:844});
 {const chatTab=page.locator('.mobile-panel-switcher').getByRole('button',{name:/Agent chat/});if(await chatTab.count())await chatTab.first().click();}
 await expect(strip.getByRole('listitem').first().getByRole('button',{name:'Send now'})).toBeVisible();
 // Edit stays in the action row beside Send now on a phone, on screen.
 await expect(strip.getByRole('listitem').first().locator('.queued-actions').getByRole('button',{name:'Edit',exact:true})).toBeVisible();
 assert.deepEqual(await strip.getByRole('listitem').first().locator('.queued-actions button').evaluateAll(buttons=>buttons.map(button=>button.textContent.trim())),['Edit','Send now','Delete']);
 assert.ok(await page.evaluate(()=>document.documentElement.scrollWidth<=window.innerWidth+1),'no horizontal page scroll');
 await page.screenshot({path:'.cache/queued-phone.png'});
 await page.setViewportSize({width:1400,height:900});
 await expect.poll(async () => (await chat.boundingBox()).width).toBeGreaterThan(600);

 // ── ↵ queued them; ⌘↵ with an empty message box sends the oldest now, the others keep their order.
 // (Back at desktop width the chat is laid out again: it shows the queue before the key is pressed.)
 await expect(strip.locator('.queued-head [role=status]')).toHaveText(/^3 messages queued/);
 await composer.focus();await expect(composer).toHaveValue('');
 await composer.press(sendNowKey);
 await expect.poll(()=>deliveries().at(-1)).toEqual({operationId:sentAs('First follow-up'),how:'now'});
 assert.equal(claude.interrupted,3);
 assert.deepEqual(claude.queuedMessages.map(m=>m.text),['Second follow-up','Third follow-up']);
 await expect(strip.locator('.queued-head [role=status]')).toHaveText(/^2 messages queued — Claude is busy/,{timeout:5000});
 await expect(strip.getByRole('listitem').first().locator('q')).toHaveText('Second follow-up');

 // ── Codex takes a message mid-turn: Steer delivers it without stopping the turn.
 await open(codex);
 await expect(chat.getByRole('region',{name:'What the agent is doing'})).toBeVisible();
 await composer.fill('Also log the port it picked.');await composer.press('Enter');
 await expect(strip).toBeVisible({timeout:5000});
 await expect(strip.locator('.queued-head [role=status]')).toHaveText(/^Queued — Codex is busy \(Running a command/);
 await expect(strip.locator('.queued-note')).toHaveCount(0);
 await agentSays(chat,codex,'Codex is reading the server logs.');
 await expect(youSaid(chat,'Also log the port it picked.')).toHaveCount(0);
 await page.screenshot({path:'.cache/queued-codex.png'});
 await strip.getByRole('listitem').getByRole('button',{name:'Steer'}).click();
 await expect.poll(()=>deliveries().at(-1)).toEqual({operationId:f.mutations.findLast(m=>m.method==='Apply'&&m.body.message)?.body.operationId,how:'steer'});
 assert.equal(codex.interrupted,undefined,'steering does not interrupt the turn');
 await expect(strip).toHaveCount(0,{timeout:5000});
 // Steered, it is said where it reached the running turn, and says so.
 await expect(youSaid(chat,'Also log the port it picked.')).toHaveCount(1);
 await expect(youSaid(chat,'Also log the port it picked.').locator('.message-tag')).toHaveText('steered');await expect(chat.getByRole('article',{name:'You, steered: Also log the port it picked.'})).toHaveCount(1);await expect(youSaid(chat,'Also log the port it picked.').locator('.eyebrow')).toHaveCount(0);await page.mouse.move(0,0);await youSaid(chat,'Also log the port it picked.').screenshot({path:'.cache/message-own-steered.png'});

 await composer.fill('Inspect the error path independently');await composer.press('Enter');
 await strip.getByRole('button',{name:'Request delegation'}).click();
 await expect.poll(()=>deliveries().at(-1)).toEqual({operationId:sentAs('Inspect the error path independently'),how:'delegate'});
 assert.equal(codex.interrupted,undefined);
 await expect(youSaid(chat,'Inspect the error path independently').locator('.message-tag')).toHaveText('sub-agent requested');
 await expect(strip).toHaveCount(0);
 // Steer stays a button: ⌘↵ with nothing typed and nothing queued sends nothing, and says so quietly.
 const before=f.mutations.filter(m=>m.method==='Apply').length;
 await composer.focus();await composer.press(sendNowKey);
 await expect(page.locator('.context-menu-notice')).toHaveText('Nothing queued to send now');
 assert.equal(f.mutations.filter(m=>m.method==='Apply').length,before,'nothing sent');

 // ── A Claude worker that steers (Capabilities.steer) offers Steer too, and no "can't" note; ⌘↵ stays Send now.
 claude.capabilities={...claude.capabilities,steer:true};
 await open(claude);
 await expect(strip.locator('.queued-head [role=status]')).toHaveText(/^2 messages queued — Claude is busy/,{timeout:5000});
 await expect(strip.locator('.queued-note')).toHaveCount(0);
 const claudeSteer=strip.getByRole('listitem').first().getByRole('button',{name:'Steer'});
 await expect(claudeSteer).toHaveAttribute('title','Give Claude this message now, inside the running turn, without stopping it');
 await claudeSteer.click();
 await expect.poll(()=>deliveries().at(-1)).toEqual({operationId:sentAs('Second follow-up'),how:'steer'});
 assert.equal(claude.interrupted,3,'steering Claude does not interrupt its turn');
 await expect(youSaid(chat,'Second follow-up').locator('.message-tag')).toHaveText('steered');
 await expect(strip.locator('.queued-head [role=status]')).toHaveText(/^Queued — Claude is busy/);
 await open(codex);

 // ── The agent idle: ⌘↵ is an ordinary send, nothing interrupted.
 Object.assign(codex,{busy:false,status:'ready',agents:[]});
 await expect(hint).toHaveText('Enter to send · Shift+Enter for a new line',{timeout:5000});
 const delivered=deliveries().length;
 // Keep the receipt in flight while a second shortcut arrives for the same draft.
 let releaseReceipt;
 const receiptGate = new Promise(resolve => { releaseReceipt = resolve; });
 await page.route('**/Apply', async route => {
   const response = await route.fetch();
   await receiptGate;
   await route.fulfill({response});
 }, {times:1});
 await composer.fill('Thanks, that is all.');await composer.press(sendNowKey);
 try {
   await expect(composer).toHaveAttribute('readonly','');
   await composer.press(sendNowKey);
   await expect(page.getByRole('alert').filter({hasText:'Still sending your previous message'})).toHaveCount(0);
 } finally { releaseReceipt(); }
 await expect.poll(()=>sentAs('Thanks, that is all.')).toBeTruthy();
 await expect(composer).toHaveValue('');
 assert.equal(f.mutations.filter(m => m.method === 'Apply' && m.body.message?.text === 'Thanks, that is all.').length, 1);
 assert.equal(deliveries().length,delivered,'no Send now');assert.equal(codex.interrupted,undefined,'nothing interrupted');
 // A custom send shortcut takes precedence over the default newline key.
 await page.getByRole('button',{name:'Settings',exact:true}).click();
 await page.getByRole('tab',{name:'Shortcuts'}).click();
 await page.getByRole('textbox',{name:'Send message',exact:true}).fill('Shift+Enter');
 await page.getByRole('button',{name:'Save shortcuts'}).click();
 await expect(page.getByRole('status').filter({hasText:'Shortcuts saved.'})).toBeVisible();
 await page.getByRole('button',{name:'Work',exact:true}).click();
 await open(codex);
 await expect(hint).toHaveText('Shift+Enter to send');
 busy(codex,'codex');
 for (const text of ['Shift queue first', 'Shift queue second', 'Shift queue third']) {
   await composer.fill(text);
   await composer.press('Shift+Enter');
   await expect.poll(() => sentAs(text)).toBeTruthy();
   await expect(composer).toHaveValue('');
 }
 assert.deepEqual(codex.queuedMessages.filter(message => message.text.startsWith('Shift queue')).map(message => message.text),
   ['Shift queue first', 'Shift queue second', 'Shift queue third']);
 assert.equal(deliveries().length,delivered,'the custom send shortcut queues without interrupting');
 const editingRow=strip.getByRole('listitem',{name:/Shift queue first/});
 // Edit is one of the message's actions, in their row, right before Send now; Tab follows the same order.
 const actionNames=row=>row.locator('.queued-actions button').evaluateAll(buttons=>buttons.map(button=>button.textContent.trim()));
 assert.deepEqual(await actionNames(editingRow),['Steer','Request delegation','Edit','Send now','Delete']);
 await editingRow.getByRole('button',{name:'Request delegation'}).focus();
 for (const name of ['Edit','Send now','Delete']) {
   await page.keyboard.press('Tab');
   await expect(editingRow.getByRole('button',{name,exact:true})).toBeFocused();
 }
 // A wide chat: the text and all its actions on one line.
 {
   const middle=async locator=>{const at=await locator.boundingBox();return at.y+at.height/2;};
   const line=await middle(editingRow.getByRole('button',{name:'Send now'}));
   for (const at of [editingRow.locator('.queued-text'),editingRow.getByRole('button',{name:'Edit',exact:true}),editingRow.getByRole('button',{name:'Steer'})]) assert.ok(Math.abs(await middle(at)-line)<2,'one line in a wide chat');
 }
 // A narrow chat panel, at any window width: the actions wrap under the text and stay inside the strip.
 await strip.evaluate(element=>{element.style.width='300px';});
 {
   const box=await strip.boundingBox(),row=editingRow.locator('.queued-row');
   for (const button of await editingRow.locator('.queued-actions button').all()) {
     const at=await button.boundingBox();
     assert.ok(at.x>=box.x&&at.x+at.width<=box.x+box.width+1,`${await button.textContent()} stays inside a narrow strip`);
   }
   assert.ok(await row.evaluate(element=>element.scrollWidth<=element.clientWidth+1),'a narrow queued row does not overflow');
   const text=await editingRow.locator('.queued-text').boundingBox(),edit=await editingRow.getByRole('button',{name:'Edit',exact:true}).boundingBox();
   assert.ok(edit.y>=text.y+text.height-1,'in a narrow strip the actions wrap under the text');
 }
 await page.screenshot({path:'.cache/queued-narrow-panel.png'});
 await strip.evaluate(element=>{element.style.width='';});
 await editingRow.getByRole('button',{name:'Edit',exact:true}).click();
 const editor=editingRow.getByRole('textbox');
 await expect(editor).toHaveValue('Shift queue first');
 await expect(editingRow.locator('.queued-actions')).toBeHidden();
 await editor.fill('Updated queued message');
 const other=await context.newPage();
 await other.goto(page.url());
 const otherRow=other.getByRole('listitem',{name:/Shift queue first/}).first();
 await otherRow.getByRole('button',{name:'Resume edit'}).click();
 await expect(otherRow.getByRole('alert')).toContainText('another window');
 await expect(otherRow.getByRole('textbox')).toHaveCount(0);
 await other.close();
 await page.reload();
 await editingRow.getByRole('button',{name:'Resume edit'}).click();
 await expect(editor).toHaveValue('Updated queued message');
 await editingRow.getByRole('button',{name:'Save',exact:true}).click();
 const revised=strip.getByRole('listitem',{name:/Updated queued message/});
 await expect(revised).toBeVisible();
 await revised.getByRole('button',{name:'Edit',exact:true}).click();
 await revised.getByRole('textbox').fill('Discard this edit');
 await revised.getByRole('button',{name:'Cancel edit'}).click();
 await expect(revised.getByRole('textbox')).toHaveCount(0);
 assert.equal(codex.queuedMessages[0].text,'Updated queued message');
 // ── Send now, then Edit before the agent stops: editing supersedes the Send now at
 // work. The message stays queued, held, and Send now's timeout never says the step
 // would not stop. The agent is slow to stop (the fixture keeps the message queued, as
 // the Runner does until the turn ends), and a window on Playwright's clock runs out
 // that timeout (DELIVERY_TIMEOUT_MS, 20 s) at once.
 codex.sendNowWaits=true;
 {
   const held=await context.newPage();held.setDefaultTimeout(15000);
   const heldErrors=[];await collectPageErrors(held,heldErrors);
   await held.goto(`${page.url().split('#')[0]}#token=${f.token}`);
   const heldStrip=held.locator('.embedded-chat:visible').first().getByRole('region',{name:'Queued messages'});
   const heldRow=heldStrip.getByRole('listitem',{name:/Updated queued message/});
   await expect(heldRow.getByRole('button',{name:'Send now'})).toBeVisible();
   // The clock goes in once the window shows the queue, before Send now starts its timeout.
   await held.clock.install();
   await heldRow.getByRole('button',{name:'Send now'}).click();
   await expect(heldRow.locator('.queued-working')).toHaveText('Waiting for the current turn to end…');
   assert.equal(codex.queuedMessages[0].text,'Updated queued message','Send now leaves it queued until the turn ends');
   await heldRow.getByRole('button',{name:'Edit',exact:true}).click();
   await expect(heldRow.getByRole('textbox')).toHaveValue('Updated queued message');
   assert.equal(codex.queuedMessages[0].editing,true,'Edit holds it');
   await expect(heldRow.locator('.queued-working')).toHaveCount(0);
   await held.clock.fastForward('00:25');
   await expect(heldRow.getByRole('textbox')).toBeVisible();
   await expect(heldStrip.getByRole('alert')).toHaveCount(0);
   await expect(heldStrip.getByText(/has not stopped yet/)).toHaveCount(0);
   await heldRow.getByRole('button',{name:'Cancel edit'}).click();
   await expect(heldRow.getByRole('textbox')).toHaveCount(0);
   assert.deepEqual(heldErrors,[]);
   await held.close();
 }
 codex.sendNowWaits=false;
 assert.equal(codex.queuedMessages[0].editing,false);
 await revised.getByRole('button',{name:'Steer'}).click();
 await expect(youSaid(chat,'Updated queued message')).toBeVisible();
 assert.deepEqual(errors,[]);
 console.log('PASS queued messages: a message sent during a busy turn shows as queued with what the agent is busy with, from the stream at once even while the session poll lags, and its ticking time is not announced; once delivered it is never also in a stale poll\'s strip; it is in the strip only, never also a bubble in the running turn; Send now delivers it in one click (interrupt, then next), said once after the stop, also after a reload, and its tooltip names the step it stops; Steer for Codex delivers into the running turn; Claude says it cannot be steered; several collapse and hide; Edit sits in the action row before Send now, in Tab order, pauses and edits, and supersedes a Send now still at work (no retry message when its time runs out); the row wraps in a narrow panel and on a phone. Fixture only.');
}finally{await browser?.close();await server.close();await f.close();}
