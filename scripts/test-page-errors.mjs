import {createServer} from 'node:http';
import assert from 'node:assert/strict';
import {webkit, chromium, expect} from './fixtures/playwright.mjs';
import {collectPageErrors} from './fixtures/page-errors.mjs';

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
    } finally {await browser.close();}
  }
  console.log('PASS: runtime errors and unhandled rejections are retained across navigation; caught fetch diagnostics are not application errors.');
} finally {server.closeAllConnections();await new Promise(resolve=>server.close(resolve));}
