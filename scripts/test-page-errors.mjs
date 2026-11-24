import {createServer} from 'node:http';
import assert from 'node:assert/strict';
import {webkit, chromium, expect} from './fixtures/playwright.mjs';
import {collectPageErrors, collectContextPageErrors} from './fixtures/page-errors.mjs';

const server=createServer((req,res)=>{
  if(req.url==='/slow'){setTimeout(()=>res.end('{}'),10);return;}
  res.setHeader('content-type','text/html');res.end('<html><body>Navigation fixture</body></html>');
});
await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
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
      await assert.rejects(collectContextPageErrors(context),/already has pages/);
      await context.close();
    } finally {await browser.close();}
  }
  console.log('PASS: runtime errors and unhandled rejections are retained across navigation; caught fetch diagnostics and errors the app marks handled are not application errors; a context collector covers popups, labels and callbacks see each error, and late collection is refused.');
} finally {server.closeAllConnections();await new Promise(resolve=>server.close(resolve));}
