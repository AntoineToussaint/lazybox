import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from './fixtures/vite-cache.mjs';
import react from '@vitejs/plugin-react';
import {webkit,chromium,expect} from './fixtures/playwright.mjs';
import {fileURLToPath} from 'node:url';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import { collectPageErrors } from './fixtures/page-errors.mjs';

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
  await server.listen();browser=await engine.launch(name==='chromium'?{headless:true,ignoreDefaultArgs:['--hide-scrollbars']}:{headless:true});const page=await browser.newPage({viewport:{width:1440,height:900}});page.setDefaultTimeout(15000);
  const errors=[];await collectPageErrors(page, errors);
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

  // A compositor correction during an upward key scroll is not a new
  // downward input, even when both positions are within the follow threshold.
  await pane.evaluate(node=>{
   node.scrollTop=node.scrollHeight;node.dispatchEvent(new Event('scroll'));
   node.dispatchEvent(new KeyboardEvent('keydown',{key:'Home',bubbles:true,cancelable:true}));
   node.scrollTop-=10;node.dispatchEvent(new Event('scroll'));
   node.scrollTop+=2;node.dispatchEvent(new Event('scroll'));
  });
  await expect(latest,'A correction during upward scrolling must not resume following').toBeVisible();
  await latest.click();await expect.poll(gap).toBeLessThan(3);

  // A reply streams in a delta every 30 ms. Whatever way the reader scrolls
  // up, following stops at once and nothing pulls them back down.
  channel.push({type:'operation_accepted',text:'Stream a long reply'});
  // Pace the producer from the rendered stream. A slow CI browser otherwise
  // accumulates deltas while Playwright itself waits to sample the scrollTop.
  // Each gesture still has to hold while new content is visibly arriving.
  let line=0,stopped=false;
  const stream=async()=>{
   while(!stopped){
    line++;
    const marker=`Streamed line ${line}.`;
    channel.push({type:'message_delta',text:`${marker} ${'More streamed words. '.repeat(line%5?3:12)}\n\n`});
    await page.waitForFunction(text=>document.querySelector('.messages-content')?.textContent.includes(text),marker);
    await pause(30);
   }
  };
  streaming=()=>{stopped=true;};
  const streamDone=stream();
  // Test failures close the browser while the producer may be waiting for a paint.
  streamDone.catch(()=>{});
  await expect(pane).toContainText('Streamed line 20');await expect.poll(gap,{message:'A followed stream stays at the bottom'}).toBeLessThan(3);
  const settle=how=>pane.evaluate(async(node,label)=>{
   // Sample in the browser. Under load three Playwright round trips can
   // exceed the entire deadline even when scrollTop never moves.
   let last=node.scrollTop,stableSince=performance.now(),frames=0;
   const start=stableSince;
   for(;;){
    await new Promise(requestAnimationFrame);
    const now=performance.now(),top=node.scrollTop;
    if(Math.abs(top-last)>=1){stableSince=now;frames=0;}
    else frames++;
    if(frames>=3&&now-stableSince>=240)return top;
    if(now-start>3000&&now-stableSince<240)throw new Error(`${label}: scrollTop still moving (${last} → ${top})`);
    last=top;
   }
  },`${name}: ${how}`);
  // Observe actual DOM progress, including a larger delta every fifth line.
  // Elapsed wall time alone says nothing about how far a busy browser got.
  const afterStreamProgress=async(check,count=5)=>{
   let observed=line;
   for(let i=0;i<count;i++){
    await page.waitForFunction(previous=>{
     const text=document.querySelector('.messages-content')?.textContent ?? '';
     return text.includes(`Streamed line ${previous+1}.`);
    },observed);
    await check();
    observed=line;
   }
  };
  const holds=async(how,nudge=false)=>{
   // Wheel delivery is asynchronous. A quiet pane before the gesture lands
   // is not evidence that the gesture has finished.
   await expect(latest,`${how}: the Latest pill offers the way back`).toBeVisible();
   await expect.poll(gap,{message:`${name}: ${how} moves away from the bottom`}).toBeGreaterThan(nudge?1:200);
   const held=await settle(how),grown=await pane.evaluate(node=>node.scrollHeight);
   if(!nudge)assert.ok(held<grown-(await pane.evaluate(node=>node.clientHeight))-200,`${how}: the reader is clearly away from the bottom`);
   await expect(latest,`${how}: the Latest pill offers the way back`).toBeVisible();
   await afterStreamProgress(async()=>{
    const now=await top();
    assert.ok(Math.abs(now-held)<=4,`${name}: ${how} must hold the reading position while the stream continues (${held} → ${now})`);
   });
   assert.ok(await pane.evaluate(node=>node.scrollHeight)>grown+50,`${how}: the stream kept writing while the reader held still`);
  };
  const follows=async how=>{
   await expect.poll(gap,{message:`${name}: ${how} resumes following`}).toBeLessThan(3);await expect(latest).toBeHidden();
   await afterStreamProgress(()=>expect.poll(gap,{message:`${name}: ${how}: following holds while the stream continues`}).toBeLessThan(3));
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
  const endGap=await pane.evaluate(node=>{node.dispatchEvent(new KeyboardEvent('keydown',{key:'End',bubbles:true,cancelable:true}));return node.scrollHeight-node.clientHeight-node.scrollTop;});
  assert.ok(endGap<3,'End resumes following immediately, before native scrolling or another streamed delta');
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
  // The custom scrollbar changes the pane's content width. Wait until the
  // prompt rail has consumed that resize before sampling the drag geometry.
  await expect.poll(()=>pane.evaluate(node=>{
   const edge=node.getBoundingClientRect().left+node.clientLeft+node.clientWidth;
   const markers=[...node.parentElement.querySelectorAll('.prompt-rail button')];
   return markers.length>0&&markers.every(marker=>marker.getBoundingClientRect().right<=edge);
  }),{message:`${name}: prompt markers must leave the scrollbar gutter clear`}).toBe(true);
  // Dragging the scrollbar thumb.
  const bar=await pane.evaluate(node=>{const r=node.getBoundingClientRect(),thumb=Math.max(40,node.clientHeight*node.clientHeight/node.scrollHeight);return{x:r.right-Math.max(3,(node.offsetWidth-node.clientWidth)/2),y:Math.min(r.top+node.clientHeight-thumb/2,r.top+node.scrollTop/node.scrollHeight*node.clientHeight+thumb/2),native:node.offsetWidth>node.clientWidth};});
  if(name==='chromium')assert.ok(bar.native,'Chromium must exercise a real scrollbar drag');

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
  streaming();await streamDone;streaming=undefined;
  await page.screenshot({path:`/tmp/lazybox-chat-scroll-${name}.png`});
  assert.deepEqual(errors,[]);
  console.log(`PASS ${name}: true bottom after late header, resize and Mermaid; reading held mid-stream against wheel, PageUp, Home, ArrowUp and a scrollbar drag; Latest pill, End, wheel to bottom and send resume following; layout shifts above the viewport keep the reading position.`);
 }finally{streaming?.();await browser?.close();await server.close();await fixture.close();}
}
for(const [name,engine] of Object.entries(engines))await run(name,engine);
