import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from 'vite';
import react from '@vitejs/plugin-react';
import {webkit,chromium,expect} from './fixtures/playwright.mjs';
import {fileURLToPath} from 'node:url';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';

// WebKit is the desktop engine; Chromium has a different scroll pipeline
// (threaded wheel scrolling, native scroll anchoring), so both are covered.
const engines=process.argv.includes('--webkit')?{webkit}:process.argv.includes('--chromium')?{chromium}:{webkit,chromium};
const pause=ms=>new Promise(resolve=>setTimeout(resolve,ms));

async function run(name,engine){
 const fixture=await startRunnerFixture();fixture.sessions[0].model='requested-alias';fixture.sessions[0].modelReported=false;fixture.sessions[0].provider='claude';
 const channel=fixture.openEvents('alpha-session');
 channel.push({type:'session_ready',providerDataJson:Buffer.from(JSON.stringify({model:'claude-reported-model'})).toString('base64')});
 for(let i=0;i<30;i++){channel.push({type:'operation_accepted',text:`Question ${i}`});channel.push({type:'message_delta',text:`Answer ${i}\n\n${'Long answer. '.repeat(40)}`});}
 const server=await createServer({...testViteCache(import.meta.url),configFile:false,root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
 let browser,streaming;
 try{
  await server.listen();browser=await engine.launch({headless:true});const page=await browser.newPage({viewport:{width:1440,height:900}});page.setDefaultTimeout(15000);
  const errors=[];page.on('pageerror',e=>errors.push(String(e)));
  await page.route('**/GetSession',async route=>{await new Promise(resolve=>setTimeout(resolve,500));await route.continue();});
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`);
  const pane=page.locator('.messages:visible');const gap=()=>pane.evaluate(node=>node.scrollHeight-node.clientHeight-node.scrollTop);
  const top=()=>pane.evaluate(node=>node.scrollTop);
  const latest=page.getByRole('button',{name:'Jump to latest'});
  await expect(pane).toContainText('Answer 29');await expect(page.locator('.conversation-title')).toBeVisible();
  await expect.poll(gap,{message:'Late session header must not leave the last lines below the viewport'}).toBeLessThan(3);
  await page.setViewportSize({width:1100,height:650});
  await expect.poll(gap,{message:'Resizing a followed chat must keep its real bottom visible'}).toBeLessThan(3);
  channel.push({type:'operation_accepted',text:'Show a tall diagram'});
  channel.push({type:'message_delta',text:'```mermaid\nflowchart TD\nA-->B\nB-->C\nC-->D\nD-->E\nE-->F\nF-->G\nG-->H\nH-->I\n```\n\nREAL_END'});
  const diagram=page.getByRole('img',{name:'Mermaid diagram'});await expect.poll(()=>diagram.evaluate(img=>img.complete&&img.naturalHeight>0)).toBe(true);
  await expect.poll(gap,{message:'Delayed Mermaid layout must reach the actual chat end'}).toBeLessThan(3);
  await expect(page.locator('.chat-heading')).toContainText('claude-reported-model');await expect(page.locator('.chat-heading')).not.toContainText('claude-default');
  // Deliberately read earlier messages, then stream and resize without snapping back.
  await pane.evaluate(node=>{node.scrollTop=200;node.dispatchEvent(new Event('scroll'));});await expect(latest).toBeVisible();
  const reading=await top();channel.push({type:'message_delta',text:'\n\nMore text while reading earlier.'});await expect(pane).toContainText('More text while reading earlier.');await page.setViewportSize({width:1100,height:750});
  assert.ok(Math.abs((await top())-reading)<3,'stream and layout must preserve intentional reading position');
  await page.locator('.workspace-row').filter({hasText:'Beta release'}).locator('button').first().click();await page.locator('.workspace-row').filter({hasText:'Alpha feature'}).locator('button').first().click();await expect.poll(top).toBe(reading);
  await latest.click();await expect.poll(gap).toBeLessThan(3);
  await pane.evaluate(node=>{node.scrollTop=200;node.dispatchEvent(new Event('scroll'));});
  await page.getByRole('textbox',{name:'Message',exact:true}).fill('Continue from here');await page.getByRole('button',{name:'Send message',exact:true}).click();
  await expect.poll(gap,{message:'Sending should return to the latest turn'}).toBeLessThan(3);
  channel.push({type:'operation_accepted',text:'Continue from here'});channel.push({type:'message_delta',text:'LATEST_SENT_TURN'});await expect(pane).toContainText('LATEST_SENT_TURN');await expect.poll(gap).toBeLessThan(3);

  // A reply streams in a delta every 30 ms. Whatever way the reader scrolls
  // up, following stops at once and nothing pulls them back down.
  channel.push({type:'operation_accepted',text:'Stream a long reply'});
  let line=0;streaming=setInterval(()=>{line++;channel.push({type:'message_delta',text:`Streamed line ${line}. ${'More streamed words. '.repeat(line%5?3:12)}\n\n`});},30);
  await expect(pane).toContainText('Streamed line 20');await expect.poll(gap,{message:'A followed stream stays at the bottom'}).toBeLessThan(3);
  const settle=async how=>{let last=await top();for(let still=0,end=Date.now()+3000;still<3;){assert.ok(Date.now()<end,`${name}: ${how}: the pane never came to rest (it keeps moving with the stream)`);await pause(80);const now=await top();still=Math.abs(now-last)<1?still+1:0;last=now;}return last;};
  const holds=async(how,nudge=false)=>{
   const held=await settle(how),grown=await pane.evaluate(node=>node.scrollHeight);
   if(!nudge)assert.ok(held<grown-(await pane.evaluate(node=>node.clientHeight))-200,`${how}: the reader is clearly away from the bottom`);
   await expect(latest,`${how}: the Latest pill offers the way back`).toBeVisible();
   for(const end=Date.now()+2000;Date.now()<end;){await pause(100);const now=await top();assert.ok(Math.abs(now-held)<=4,`${name}: ${how} must hold the reading position while the stream continues (${held} → ${now})`);}
   assert.ok(await pane.evaluate(node=>node.scrollHeight)>grown+50,`${how}: the stream kept writing while the reader held still`);
  };
  const follows=async how=>{
   await expect.poll(gap,{message:`${name}: ${how} resumes following`}).toBeLessThan(3);await expect(latest).toBeHidden();
   for(const end=Date.now()+600;Date.now()<end;)await pause(100);
   await expect.poll(gap,{message:`${name}: ${how}: following holds while the stream continues`}).toBeLessThan(3);
  };
  const box=await pane.boundingBox();
  // Mouse wheel / trackpad.
  // Clicking the pill moves the mouse off the pane: each wheel starts over the reading area.
  const wheel=async dy=>{await page.mouse.move(box.x+box.width/2,box.y+box.height/3);await page.mouse.wheel(0,dy);};
  await wheel(-400);
  await holds('Wheel up mid-stream');
  await latest.click();await follows('The Latest pill');
  // A small nudge up, still within the follow threshold, is a scroll up too.
  await wheel(-30);await holds('A 30 px wheel nudge up',true);
  // Scrolling back to the bottom resumes following.
  await wheel((await gap())+20000);await follows('Wheeling back to the bottom');
  // Keyboard.
  await pane.focus();await page.keyboard.press('PageUp');await holds('PageUp mid-stream');
  await page.keyboard.press('End');await follows('End');
  await page.keyboard.press('ArrowUp');await page.keyboard.press('ArrowUp');await page.keyboard.press('ArrowUp');await page.keyboard.press('ArrowUp');
  await expect(latest,'ArrowUp stops following').toBeVisible();await settle('ArrowUp');
  await page.keyboard.press('Home');await holds('Home mid-stream');
  await latest.click();await follows('The Latest pill after Home');
  // Fix the browser-owned thumb geometry for the drag. Native themes have
  // different minimum thumb sizes and arrow buttons; guessing 20px can hit
  // the bottom arrow instead of the thumb on Linux.
  await page.addStyleTag({content:'.messages::-webkit-scrollbar{width:16px}.messages::-webkit-scrollbar-track{background:#eee}.messages::-webkit-scrollbar-thumb{background:#777;min-height:40px;border:0}.messages::-webkit-scrollbar-button{display:none;width:0;height:0}'});
  await expect.poll(gap).toBeLessThan(3);
  // Dragging the scrollbar thumb.
  const bar=await pane.evaluate(node=>{const r=node.getBoundingClientRect(),thumb=Math.max(40,node.clientHeight*node.clientHeight/node.scrollHeight);return{x:r.right-Math.max(3,(node.offsetWidth-node.clientWidth)/2),y:Math.min(r.top+node.clientHeight-thumb/2,r.top+node.scrollTop/node.scrollHeight*node.clientHeight+thumb/2),native:node.offsetWidth>node.clientWidth};});
  await page.mouse.move(bar.x,bar.y);await page.mouse.down();
  for(let step=1;step<=8;step++){await page.mouse.move(bar.x,bar.y-step*12);await pause(30);}
  // Headless macOS WebKit has no painted scrollbar, even with custom CSS.
  // Keep its held-pointer simulation; Linux exercises the actual native drag.
  if(!bar.native)await pane.evaluate(node=>{node.scrollTop-=600;return new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve)));});
  await page.mouse.up();await holds('A scrollbar drag mid-stream');
  // A layout shift above the viewport, such as an image or diagram rendering
  // in an earlier message, leaves what the reader looks at where it is.
  const shift=async(padding,how)=>{
   const before=await pane.evaluate((node,padding)=>{
    const paneTop=node.getBoundingClientRect().top,rows=[...node.querySelectorAll('[data-message-id]')];
    const seen=rows.find(row=>row.getBoundingClientRect().bottom>paneTop+40),above=rows.filter(row=>row.getBoundingClientRect().bottom<paneTop-20).at(-1);
    const offset=seen.getBoundingClientRect().top-paneTop;above.style.paddingBottom=padding;window.__seen=seen;return offset;
   },padding);
   await pause(300);
   for(const end=Date.now()+1000;Date.now()<end;){await pause(100);const after=await pane.evaluate(node=>window.__seen.getBoundingClientRect().top-node.getBoundingClientRect().top);assert.ok(Math.abs(after-before)<=4,`${name}: ${how} moved the reading position (${before} → ${after})`);}
  };
  await shift('360px','Content growing above the viewport');
  await shift('0px','Content shrinking above the viewport');
  await expect(latest).toBeVisible();
  // Sending mid-stream, while reading, returns to the bottom and follows.
  await page.getByRole('textbox',{name:'Message',exact:true}).fill('One more thing');await page.getByRole('button',{name:'Send message',exact:true}).click();
  await follows('Sending a message');
  clearInterval(streaming);streaming=undefined;
  await page.screenshot({path:`/tmp/lazybox-chat-scroll-${name}.png`});
  assert.deepEqual(errors,[]);
  console.log(`PASS ${name}: true bottom after late header, resize and Mermaid; reading held mid-stream against wheel, PageUp, Home, ArrowUp and a scrollbar drag; Latest pill, End, wheel to bottom and send resume following; layout shifts above the viewport keep the reading position.`);
 }finally{clearInterval(streaming);await browser?.close();await server.close();await fixture.close();}
}
for(const [name,engine] of Object.entries(engines))await run(name,engine);
