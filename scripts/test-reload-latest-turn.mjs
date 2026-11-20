// Fixture only: a busy Claude conversation with a long history keeps showing
// its latest turn across a Runner restart and a reload. A real one stopped at
// the restart: the restarting Runner ended the conversation's stream like a
// finished conversation's, the window stopped following it, and a reload just
// before the restart showed the conversation as it was then (the latest
// message, delivered minutes later, and everything the agent did after it were
// missing) while the header said the agent was busy. The Runner now ends such
// a stream Unavailable (internal/transport WatchEvents); the fixture's `cut()`
// does the same.
//
// The journal is driven like the Runner's: a message queued mid-turn is
// journaled when accepted, Send now interrupts the turn and delivers it (its
// receipt), and its turn goes on working while another message waits in the
// queue. A queued message is in one place at a time: the Queued strip while it
// waits, the conversation once delivered, at the start of the turn it began.
// Then the page reloads: thousands of events replay, only the latest messages
// are mounted, they end with the turn in progress, and every message is where
// it was.
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from 'vite';import react from '@vitejs/plugin-react';import {chromium,expect} from './fixtures/playwright.mjs';import {fileURLToPath} from 'node:url';import assert from 'node:assert/strict';import {mkdir} from 'node:fs/promises';import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import {longConversation,deliveredQueuedTurn} from './fixtures/long-conversation.mjs';

const f=await startRunnerFixture();
const now=Date.now();
const claude=f.sessions[0];
Object.assign(claude,{provider:'claude',busy:true,status:'running',queued:0,queuedMessages:[],agents:[{id:'root',status:'running',phase:'tool',tool:'Bash',phaseSinceUnixMillis:String(now-60000),turnStartedAtUnixMillis:String(now-90000)}]});
const ch=f.openEvents(claude.id);
const history=longConversation({turns:60,toolOutputBytes:200});
for(const event of history)ch.push(event);
assert.ok(ch.events.length>4000,`a long history (${ch.events.length} events)`);
const {events:latest}=deliveredQueuedTurn();
const step=marker=>{const i=latest.findIndex(marker);assert.ok(i>=0);return i;};
// The Runner journals what the person does: the queued messages are the ones typed below.
const queuedAt=step(e=>e.operationId==='op-teams'),interruptAt=step(e=>e.operationId==='op-teams:now');
// The delivered message's turn, as the agent records it after the receipt (the fixture journals the stop, the receipt and the turn's start).
const workingAt=step((e,i)=>e.type==='turn_started'&&i>interruptAt)+1;
const pushAll=events=>{for(const event of events)ch.push(event);};
const accepted=(text,operationId)=>ch.push({type:'operation_accepted',text,operationId});
const messages=()=>f.mutations.filter(m=>m.method==='Apply'&&m.body.message);

const server=await createServer({...testViteCache(import.meta.url),configFile:false,logLevel:'error',root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:f.url}}}});let browser;
try{
 await server.listen();await mkdir('.cache',{recursive:true});browser=await chromium.launch({headless:true});const page=await browser.newPage({viewport:{width:1400,height:900}});page.setDefaultTimeout(15000);
 const errors=[];page.on('pageerror',e=>errors.push(String(e)));
 await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=${claude.collectionId}&session=${claude.id}#token=${f.token}`);
 const chat=page.locator('.embedded-chat:visible').first(),pane=chat.locator('.messages:visible'),composer=chat.getByRole('textbox',{name:'Message',exact:true});
 const strip=chat.getByRole('region',{name:'Queued messages'});
 await expect(pane).toContainText('ANSWER_END_59',{timeout:30000});

 // ── The Runner restarts: it ends the stream, is down a moment, and the next
 // Runner re-attaches the agent, which goes on (a new turn starts).
 f.runner.down=true;ch.cut();
 pushAll(latest.slice(0,queuedAt));
 await page.waitForTimeout(500);f.runner.down=false;
 await expect(pane,'the open view follows the conversation past the restart').toContainText('ROLES_ANSWER',{timeout:10000});

 // ── A message sent mid-turn waits in the queue; the Runner journals it when it accepts it.
 await composer.fill('QUEUED_DELIVERED: only two teams and two users in every solution.');await composer.press('Enter');
 await expect(strip).toBeVisible({timeout:5000});
 // (The fixture journals it on acceptance, as the Runner does.) The agent goes on:
 // the strip is its only place, not a "You" bubble in the running turn.
 pushAll(latest.slice(queuedAt+1,interruptAt));
 const you=pane.locator('article.message.operation_accepted'),said=text=>you.filter({hasText:text});
 await expect(pane.getByRole('list',{name:'Tool calls'}).last()).toBeVisible();
 await expect(said('QUEUED_DELIVERED')).toHaveCount(0);
 await expect(you.last()).toContainText('Make the fixture global');
 // ── Send now: the turn is interrupted, and the queued message is delivered next.
 await strip.getByRole('listitem').getByRole('button',{name:'Send now'}).click();
 await expect.poll(()=>claude.interrupted).toBe(1);
 await expect(strip).toHaveCount(0,{timeout:5000});
 await expect(said('QUEUED_DELIVERED')).toHaveCount(1);
 // ── Another message waits behind the delivered one, in the strip only.
 await composer.fill('QUEUED_BEHIND: ok so continue');await composer.press('Enter');
 await expect.poll(()=>claude.queuedMessages.length).toBe(1);
 await expect(strip).toBeVisible({timeout:5000});
 // ── The delivered message's turn: the agent working.
 pushAll(latest.slice(workingAt));
 ch.push({type:'message_delta',text:'LATEST_FIXTURE_CAUGHT_UP'});
 await expect(pane).toContainText('LATEST_FIXTURE_CAUGHT_UP');
 await expect(pane.getByRole('list',{name:'Tool calls'}).last()).toContainText(/2 commands/,{timeout:10000});
 // A turn's commands are one line until opened; the last line is the turn in progress.
 // The marker follows every fixture tool event; a two-command count alone
 // can appear while the second command is still streaming and regrouping.
 const latestCommands=async()=>{const run=pane.getByRole('list',{name:'Tool calls'}).last();await expect(pane).toContainText('LATEST_FIXTURE_CAUGHT_UP');await expect(run).toContainText(/2 commands/,{timeout:10000});await run.getByRole('button').first().click();await expect(run).toContainText(/lazybox artifact put plan\.md/i);await expect(run).toContainText(/go test \.\/libraries\/authority-core/i);};
 await latestCommands();
 /** Every message once, where it reached the agent; the queued one nowhere in the conversation. */
 const inPlace=async()=>{
  await expect(you.last()).toContainText('QUEUED_DELIVERED: only two teams',{timeout:30000});
  await expect(said('QUEUED_DELIVERED: only two teams')).toHaveCount(1);
  await expect(said('Make the fixture global')).toHaveCount(1);
  await expect(said('QUEUED_BEHIND')).toHaveCount(0);
  const order=await pane.evaluate(node=>{const text=node.innerText.toLowerCase();return ['answer_end_59','roles_answer','stopped to send your message','queued_delivered','lazybox artifact put plan.md','go test ./libraries/authority-core'].map(s=>text.lastIndexOf(s));});
  assert.ok(order.every((at,i)=>at>=0&&(i===0||at>order[i-1])),`latest turn last, in order: ${order}`);
 };
 await inPlace();

 // ── Reload in the middle of that turn: everything up to the latest is shown, each message in its place.
 await page.reload();
 await expect(pane).toContainText('ROLES_ANSWER',{timeout:30000});
 // Thousands of events, but only the latest messages are mounted.
 await expect(chat.getByRole('button',{name:/Show earlier messages/})).toBeVisible();
 await expect(pane).not.toContainText('ANSWER_END_10');
 // The view is at its end: the turn in progress, not an earlier turn.
 await expect.poll(()=>pane.evaluate(node=>node.scrollHeight-node.clientHeight-node.scrollTop)).toBeLessThan(3);
 await latestCommands();
 await inPlace();
 // The agent is still working on it, and the message behind it still waits.
 await expect(chat.getByRole('region',{name:'What the agent is doing'})).toBeVisible();
 await expect(strip).toBeVisible();
 await expect(strip.getByRole('listitem').locator('q')).toHaveText('QUEUED_BEHIND: ok so continue');
 await page.screenshot({path:'.cache/reload-latest-turn.png'});
 assert.deepEqual(errors,[]);
 console.log(`PASS reload latest turn: a busy conversation of ${ch.events.length} events follows a Runner restart (its stream ends Unavailable and reconnects from its cursor); a message queued mid-turn is in the strip only until Send now delivers it, then said once at the start of its turn; its turn's activity and the message queued behind it (in the strip, not the conversation) are all shown the same after a reload, at the end of the view, with only the latest messages mounted. Fixture only.`);
}finally{await browser?.close();await server.close();await f.close();}
