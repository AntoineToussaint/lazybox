import {createServer} from 'node:http';
import assert from 'node:assert/strict';
import {webkit, chromium, expect} from './fixtures/playwright.mjs';
import {collectPageErrors, collectContextPageErrors, drainPageErrors} from './fixtures/page-errors.mjs';

const server=createServer((req,res)=>{
  if(req.url==='/slow'){setTimeout(()=>res.end('{}'),10);return;}
  res.setHeader('content-type','text/html');
  // A page whose frame is another site (localhost, beside 127.0.0.1): its own process in Chromium.
  if(req.url==='/framed'){res.end(`<html><body><iframe src="http://localhost:${server.address().port}/frame"></iframe></body></html>`);return;}
  if(req.url==='/button'){res.end('<html><body><button onclick="throw new Error(\'the last click\')">Last</button></body></html>');return;}
  res.end('<html><body>Navigation fixture</body></html>');
});
// Every interface: the framed page's frame is reached as localhost.
await new Promise(resolve=>server.listen(0,resolve));
const url=`http://127.0.0.1:${server.address().port}`;
try {
  for(const engine of [webkit,chromium]) {
    const browser=await engine.launch();
    try {
      const page=await browser.newPage();
      const errors=await collectPageErrors(page);
      await page.goto(url);
      await page.evaluate(()=>{
        setTimeout(()=>{throw new Error('uncaught fixture error');},0);
        void Promise.reject(new Error('unhandled fixture rejection'));
        void Promise.reject(new Error('handled fixture rejection')).catch(()=>{});
      });
      await expect.poll(()=>errors.length).toBe(2);
      assert.ok(errors.some(error=>error.includes('uncaught fixture error')));
      assert.ok(errors.some(error=>error.includes('unhandled fixture rejection')));
      errors.length=0;
      // What the app marks handled (preventDefault in its own listener, registered after the
      // collector's) is not an error, as the browser does not report it either.
      await page.evaluate(()=>{
        const handled=event=>{if(String(event.error||event.reason).includes('handled by the app'))event.preventDefault();};
        window.addEventListener('error',handled);window.addEventListener('unhandledrejection',handled);
        setTimeout(()=>{throw new Error('thrown, handled by the app');},0);
        void Promise.reject(new Error('rejected, handled by the app'));
        setTimeout(()=>{throw new Error('after the handled ones');},50);
      });
      await expect.poll(()=>errors).toEqual(['Error: after the handled ones']);
      errors.length=0;
      // A normal HTTP page, caught fetches and full navigation: no app code.
      // WebKit can report interrupted fetches as JavaScript console errors.
      for(let attempt=0;attempt<20;attempt++) {
        await page.goto(url);
        await page.evaluate(()=>{setInterval(()=>{void fetch('/slow').catch(()=>{});},1);});
        await page.waitForTimeout(20);
        await page.goto(url+'/next');
      }
      assert.deepEqual(errors,[],'caught navigation failures are not uncaught application errors');
      await page.evaluate(()=>{setTimeout(()=>{throw new Error('error after navigation');},0);});
      await expect.poll(()=>errors).toEqual(['Error: error after navigation']);
      // Collecting after the first navigation would miss the document already loaded: refused.
      await assert.rejects(collectPageErrors(page),/already loaded/);
      await page.close();

      // Fail closed: an error is reported as it is dispatched, not a task later. One thrown while
      // the document unloads (a pagehide handler), or by the last action before the page closes,
      // arrives; a later task never runs.
      const leaving=await browser.newPage();
      const lost=await collectPageErrors(leaving);
      await leaving.goto(url);
      await leaving.evaluate(()=>{addEventListener('pagehide',()=>{throw new Error('thrown while leaving');});});
      await leaving.goto(url+'/next');
      await drainPageErrors(lost);
      assert.deepEqual(lost,['Error: thrown while leaving'],'an error in a pagehide handler counts');
      // One the app handles in its own error listener while the page is left is not: the retraction
      // task never runs once the document is gone, so the verdict is read during pagehide itself.
      await leaving.goto(url);
      await leaving.evaluate(()=>{
        addEventListener('error',event=>{if(String(event.error).includes('handled while leaving'))event.preventDefault();});
        addEventListener('pagehide',()=>{throw new Error('handled while leaving');});
      });
      await leaving.goto(url+'/next');
      await drainPageErrors(lost);
      assert.deepEqual(lost,['Error: thrown while leaving'],'an error the app handled in pagehide is not counted');
      await leaving.goto(url+'/button');
      await leaving.getByRole('button').click();
      await drainPageErrors(lost);
      assert.deepEqual(lost,['Error: thrown while leaving','Error: the last click'],'drained, the last action\'s error is there before the assertion');
      await leaving.close();
      await drainPageErrors(lost);

      // Playwright's clock replaces the page's timers; with it paused, errors still arrive, and
      // what the app handles is still retracted.
      const paused=await browser.newPage();
      const frozen=await collectPageErrors(paused);
      await paused.clock.install({time:new Date('2026-01-01T00:00:00Z')});
      await paused.goto(url);
      await paused.clock.pauseAt(new Date('2026-01-01T00:01:00Z'));
      await paused.evaluate(()=>{
        addEventListener('error',event=>{if(String(event.error).includes('handled by the app'))event.preventDefault();});
        dispatchEvent(new ErrorEvent('error',{error:new Error('handled by the app while paused'),cancelable:true}));
        dispatchEvent(new ErrorEvent('error',{error:new Error('unhandled while paused'),cancelable:true}));
      });
      await drainPageErrors(frozen);
      assert.deepEqual(frozen,['Error: unhandled while paused'],'a paused clock neither holds back an error nor keeps a handled one');
      await paused.close();

      // Drained at a boundary, every error dispatched before it is recorded with what was true then
      // (the scenario runner names the step each error was dispatched in this way).
      const steps=await browser.newPage();
      let step='first';
      const stamped=await collectPageErrors(steps,[],{format:message=>`${message} (during: ${step})`});
      await steps.goto(url);
      await steps.evaluate(()=>{for(let i=0;i<20;i++)setTimeout(()=>{throw new Error('in the first step');},0);});
      await steps.evaluate(()=>new Promise(resolve=>setTimeout(resolve,10)));
      await drainPageErrors(stamped);
      step='second';
      await steps.evaluate(()=>{setTimeout(()=>{throw new Error('in the second step');},0);});
      await expect.poll(()=>stamped.length).toBe(21);
      assert.deepEqual(stamped,[...Array(20).fill('Error: in the first step (during: first)'),'Error: in the second step (during: second)']);
      await steps.close();

      // An array no collector fills would always look clean: draining it is a mistake.
      await assert.rejects(drainPageErrors([]),/no collector fills/);

      // Installed first, the collector hears an init script installed after it that throws.
      const early=await browser.newContext();
      const initErrors=await collectContextPageErrors(early);
      await early.addInitScript(()=>{if(location.protocol==='http:')throw new Error('init script threw');});
      const earlyPage=await early.newPage();
      await earlyPage.goto(url);
      await drainPageErrors(initErrors);
      // An engine may mute an init script's error to "Script error.": what counts is that it arrives.
      assert.equal(initErrors.length,1,'an init script installed after the collector is heard: '+JSON.stringify(initErrors));
      await early.close();

      // A context collector hears every page, popups included; a label and a callback see each error.
      const context=await browser.newContext();
      const seen=[];
      const pooled=await collectContextPageErrors(context,[],{label:'ctx',onError:(message,stack)=>seen.push({message,stack})});
      const main=await context.newPage();
      const own=await collectPageErrors(main,[],{label:'main'});
      await main.goto(url);
      const [popup]=await Promise.all([context.waitForEvent('page'),main.evaluate(target=>{window.open(target);},url+'/popup')]);
      await popup.waitForLoadState('load');
      await popup.evaluate(()=>{setTimeout(()=>{throw new Error('popup error');},0);});
      await main.evaluate(()=>{setTimeout(()=>{throw new Error('main error');},0);});
      await expect.poll(()=>[...pooled].sort()).toEqual(['ctx: Error: main error','ctx: Error: popup error']);
      assert.deepEqual(own,['main: Error: main error'],'a page collector hears only its page, beside the context collector');
      assert.deepEqual(seen.map(({message})=>message).sort(),[...pooled].sort());assert.ok(seen.every(({stack})=>typeof stack==='string'),JSON.stringify(seen));
      // A popup's blank document hands its window, and the collector's state, to the page it
      // loads. A verdict and a drain still come: a channel the blank document made never
      // delivered in WebKit, so a drain over a popup waited forever and a handled error there
      // was never withdrawn (the reattach gate hung at its drain).
      // The throw has happened before the drain starts: the drain cannot overtake it.
      await popup.evaluate(()=>{window.addEventListener('error',event=>{if(String(event.error).includes('handled in the popup'))event.preventDefault();});setTimeout(()=>{throw new Error('handled in the popup');},0);return new Promise(resolve=>setTimeout(resolve,0));});
      let timer;const drained=await Promise.race([drainPageErrors(pooled).then(()=>'drained'),new Promise(resolve=>{timer=setTimeout(()=>resolve('still waiting after 5 s'),5000);})]).finally(()=>clearTimeout(timer));
      assert.equal(drained,'drained','a drain over a popup finishes');
      assert.deepEqual([...pooled].sort(),['ctx: Error: main error','ctx: Error: popup error'],'an error the popup handled is withdrawn');
      await assert.rejects(collectContextPageErrors(context),/already has pages/);
      await context.close();
    } finally {await browser.close();}
  }
  // Draining waits for every frame, not only the page's. A frame of another site in its own
  // process (site isolation on) that reports an error, which its app handles and then stays
  // busy, has not retracted it yet when the page's own frame is drained.
  const isolated=await chromium.launch({args:['--site-per-process']});
  try {
    const framed=await isolated.newPage();
    const frameErrors=await collectPageErrors(framed);
    await framed.goto(url+'/framed');
    await expect.poll(()=>framed.frames().length).toBe(2);
    const frame=framed.frames()[1];
    await frame.waitForLoadState();
    const busy=frame.evaluate(()=>{
      addEventListener('error',event=>{event.preventDefault();const until=Date.now()+1500;while(Date.now()<until);});
      dispatchEvent(new ErrorEvent('error',{error:new Error('handled in the frame'),cancelable:true}));
    }).catch(error=>error);
    await expect.poll(()=>frameErrors.length).toBe(1);
    await drainPageErrors(frameErrors);
    assert.deepEqual(frameErrors,[],'drained, the frame has retracted what its app handled');
    assert.equal(await busy,undefined);
  } finally {await isolated.close();}
  console.log('PASS: runtime errors and unhandled rejections are retained across navigation; caught fetch diagnostics and errors the app marks handled are not application errors; a context collector covers popups, labels and callbacks see each error, and late collection is refused; errors arrive as dispatched (pagehide, a last click) and a pagehide error the app handles is not kept; draining waits for frames, under a paused clock, and from later init scripts.');
} finally {server.closeAllConnections();await new Promise(resolve=>server.close(resolve));}
