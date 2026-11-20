// Fixture only: opening or reloading a long conversation shows its latest turn
// at once. A real conversation of about 10,000 journal records (a few MB, large
// tool outputs) replayed record by record from the Runner, and was folded in
// the window, before anything was shown. The window now reads the latest turns
// with one ReadSessionTail, follows the conversation from there, and reads the
// earlier history in a few pages behind them.
//
// The same 10,000-record conversation is opened two ways in the same app,
// server and browser: through a Runner that has no ReadSessionTail (the full
// replay, as before) and through one that has it. The time from the window's
// first read of the conversation to its last turn on screen is bounded
// absolutely (generously, for a loaded machine). The RPC cursors assert that
// opening uses the tail without replaying retained records; older-page responses
// are held until the latest turn is visible. Relative timings are diagnostic. Then: scrolling up
// brings the earlier history, in order and once; switching to another
// conversation and back is instant (no read of what the window holds); and a
// Runner restart while the earlier history loads loses and repeats nothing.
//
// LOAD_BUDGET_MS tunes the absolute bound; LOAD_TURNS the length.
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from 'vite';import react from '@vitejs/plugin-react';import {chromium,expect} from './fixtures/playwright.mjs';import {fileURLToPath} from 'node:url';import assert from 'node:assert/strict';import {mkdir} from 'node:fs/promises';import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import {longConversation} from './fixtures/long-conversation.mjs';

const turns=Number(process.env.LOAD_TURNS||135),budget=Number(process.env.LOAD_BUDGET_MS||1500);
const f=await startRunnerFixture();
const [alpha,beta]=f.sessions;
for(const session of [alpha,beta])Object.assign(session,{provider:'claude',busy:false,status:'ready',agents:[{id:'root',status:'done'}]});
// Both conversations: the same long history. Beta's Runner has no ReadSessionTail.
const history=longConversation({turns});
const fast=f.openEvents(alpha.id),replayed=f.openEvents(beta.id);
for(const event of history){fast.push(event);replayed.push(event);}
replayed.noTail=true;
assert.ok(fast.events.length>=9500,`a long conversation (${fast.events.length} records)`);
// The Runner streams a replay one message per record: pace it like that.
for(const channel of [fast,replayed]){channel.pace=50;channel.paceMs=1;}
const last=`ANSWER_END_${turns-1}`;

const server=await createServer({...testViteCache(import.meta.url),configFile:false,logLevel:'error',root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:f.url}}}});let browser;
const url=(collection,session)=>`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=${collection}&session=${session}#token=${f.token}`;
// In the page: when the window first read a conversation, and when a text was first on screen.
const timing=()=>{
 const perf={read:undefined,seen:{},want:[]};window.__load=perf;
 const original=window.fetch;
 window.fetch=function(input,init){const target=String(input?.url||input);if(perf.read===undefined&&/\/(ReadSessionTail|WatchEvents)$/.test(target))perf.read=performance.now();return original.call(this,input,init);};
 const frame=()=>{const text=document.querySelector('.messages')?.textContent||'';for(const want of perf.want)if(perf.seen[want]===undefined&&text.includes(want))perf.seen[want]=performance.now();requestAnimationFrame(frame);};
 requestAnimationFrame(frame);
};
const order=text=>[...text.matchAll(/ANSWER_END_(\d+)/g)].map(m=>Number(m[1]));
try{
 await server.listen();await mkdir('.cache',{recursive:true});browser=await chromium.launch({headless:true});
 const open=async(collection,session)=>{
  const page=await browser.newPage({viewport:{width:1400,height:900}});page.setDefaultTimeout(20000);
  const errors=[];page.on('pageerror',e=>errors.push(String(e)));
  await page.addInitScript(timing);await page.addInitScript(want=>{window.__load.want.push(want);},last);
  const callsBefore=f.calls.length;
  let releaseHistory;
  const historyReady=new Promise(resolve=>{releaseHistory=resolve;});
  if(session===alpha.id)await page.route('**/ReadSessionTail',async route=>{
   const request=route.request().postDataJSON();
   if(BigInt(request.before||0)>0)await historyReady;
   await route.continue();
  });
  await page.goto(url(collection,session));
  try {
   await expect.poll(()=>page.evaluate(want=>window.__load.seen[want],last),{timeout:60000,intervals:[20]}).not.toBeUndefined();
   if(session===alpha.id){
    await expect.poll(()=>f.calls.slice(callsBefore).filter(call=>call.body.sessionId===session&&call.method==='WatchEvents').length).toBeGreaterThan(0);
    const reads=f.calls.slice(callsBefore).filter(call=>call.body.sessionId===session);
    assert.ok(reads.some(call=>call.method==='ReadSessionTail'&&!Number(call.body.before||0)),'opening requests the latest tail');
    const watches=reads.filter(call=>call.method==='WatchEvents');
    assert.ok(watches.length>0,'opening follows live events');
    assert.ok(watches.every(call=>BigInt(call.body.after||0)===BigInt(fast.events.length)),'opening must not replay retained history');
   }
  } finally { releaseHistory(); }
  const ms=await page.evaluate(want=>Math.round(window.__load.seen[want]-window.__load.read),last);
  return {page,errors,ms};
 };
 // ── Opening: the replay, against the tail first. Run 0 warms Vite and is not counted.
 const measured={replay:[],tail:[]};
 for(let run=0;run<=3;run++){
  for(const [kind,collection,session] of [['replay','beta',beta.id],['tail','alpha',alpha.id]]){
   const {page,errors,ms}=await open(collection,session);
   assert.deepEqual(errors,[]);if(run)measured[kind].push(ms);await page.close();
  }
 }
 const median=values=>[...values].sort((a,b)=>a-b)[Math.floor(values.length/2)];
 const replayMs=median(measured.replay),tailMs=median(measured.tail);
 console.log(JSON.stringify({records:fast.events.length,replay:measured.replay,tail:measured.tail,median:{replay:replayMs,tail:tailMs}}));
 assert.ok(tailMs<=budget,`the last turn took ${tailMs} ms to show (budget ${budget} ms)`);

 const {page,errors}=await open('alpha',alpha.id);
 const chat=page.locator('.embedded-chat:visible').first(),pane=chat.locator('.messages:visible');
 await expect(chat.getByRole('button',{name:/Show earlier messages/})).toBeVisible({timeout:30000});

 // ── Switching to another conversation and back: instant, from what the window holds.
 const reads=()=>f.calls.filter(call=>call.body.sessionId===alpha.id&&(call.method==='ReadSessionTail'||call.method==='WatchEvents'));
 const before=reads().length;
 await page.locator('.workspace-row').filter({hasText:'Beta release'}).locator('button').first().click();
 await expect(pane).not.toContainText(last,{timeout:30000});
 await page.evaluate(()=>{window.__load.seen={};});
 await page.locator('.workspace-row').filter({hasText:'Alpha feature'}).locator('button').first().evaluate(button=>{window.__load.clicked=performance.now();button.click();});
 await expect.poll(()=>page.evaluate(want=>window.__load.seen[want],last),{timeout:5000,intervals:[20]}).not.toBeUndefined();
 const back=await page.evaluate(want=>Math.round(window.__load.seen[want]-window.__load.clicked),last);
 const again=reads().slice(before);
 assert.ok(again.every(call=>call.method==='WatchEvents'&&BigInt(call.body.after||0)===BigInt(fast.events.length)),`switching back reads nothing it holds: ${JSON.stringify(again.map(c=>[c.method,c.body.after,c.body.before]))}`);
 assert.ok(back<budget,`switching back took ${back} ms`);

 // ── Earlier history: scrolling up brings it, in order, each turn once.
 for(let step=0;step<40&&!order(await pane.innerText()).includes(0);step++){
  await pane.evaluate(node=>{node.scrollTop=0;node.dispatchEvent(new Event('scroll'));});
  await page.waitForTimeout(120);
 }
 const shown=order(await pane.innerText());
 assert.deepEqual(shown,Array.from({length:turns},(_,i)=>i),'every turn once, oldest first');
 await expect(chat.getByRole('button',{name:/Show earlier messages/})).toHaveCount(0);
 assert.deepEqual(errors,[]);
 await page.close();

 // ── A Runner restart while the earlier history loads.
 f.tailReadDelayMs=250;
 const restart=await browser.newPage({viewport:{width:1400,height:900}});restart.setDefaultTimeout(20000);
 const restartErrors=[];restart.on('pageerror',e=>restartErrors.push(String(e)));
 await restart.goto(url('alpha',alpha.id));
 const view=restart.locator('.embedded-chat:visible').first().locator('.messages:visible');
 await expect(view).toContainText(last,{timeout:30000});
 await expect(view.getByRole('status').filter({hasText:'Loading earlier messages'})).toBeVisible();
 f.runner.down=true;fast.cut();
 fast.push({type:'operation_accepted',text:'Sent while the Runner restarted'});
 fast.push({type:'message_delta',text:'LIVE_AFTER_RESTART'});
 await restart.waitForTimeout(600);f.runner.down=false;f.tailReadDelayMs=0;
 await expect(view).toContainText('LIVE_AFTER_RESTART',{timeout:15000});
 await expect(restart.getByRole('button',{name:/Show earlier messages/})).toBeVisible({timeout:30000});
 for(let step=0;step<40&&!order(await view.innerText()).includes(0);step++){
  await view.evaluate(node=>{node.scrollTop=0;node.dispatchEvent(new Event('scroll'));});
  await restart.waitForTimeout(120);
 }
 const text=await view.innerText();
 assert.deepEqual(order(text),Array.from({length:turns},(_,i)=>i),'after the restart, every turn once, oldest first');
 assert.equal(text.split('LIVE_AFTER_RESTART').length-1,1,'the live record once');
 await restart.screenshot({path:'.cache/conversation-load.png'});
 assert.deepEqual(restartErrors,[]);
 console.log(`PASS conversation load: the last turn of a ${fast.events.length}-record conversation shows ${tailMs} ms after the window first reads it (the full replay: ${replayMs} ms; budget ${budget} ms; tail cursors and independent history loading asserted); scrolling up brings every earlier turn in order, once; switching back to it reads nothing it holds (${back} ms); a Runner restart while its earlier history loads loses and repeats nothing. Fixture only.`);
}finally{await browser?.close();await server.close();await f.close();}
