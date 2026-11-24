// A chat that cannot get its own window (the browser blocks it) comes back
// where it left: on the page, or, when the person switched workspace while the
// window was being opened, in the workspace it left, never in the one now on
// screen. WebKit (the desktop app's engine), the browser build's host, against
// the fake Runner fixture.
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from 'vite';
import react from '@vitejs/plugin-react';
import {webkit,expect} from './fixtures/playwright.mjs';
import {fileURLToPath} from 'node:url';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';

const fixture=await startRunnerFixture();
const server=await createServer({...testViteCache(import.meta.url),configFile:false,root:fileURLToPath(new URL('../apps/web',import.meta.url)),logLevel:'warn',plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
let browser;
try{
 await server.listen();
 browser=await webkit.launch();
 const context=await browser.newContext({viewport:{width:1440,height:900}});
 const page=await context.newPage();page.setDefaultTimeout(15000);
 const errors=[];page.on('pageerror',e=>errors.push(String(e)));
 // The browser refuses every chat window. Just before it does, a scenario may act as the person
 // would in that moment (switch workspace), so the refusal lands after the switch, every time.
 await page.addInitScript(()=>{
  const open=window.open.bind(window);
  window.open=(url,name,...rest)=>{if(String(name||'').startsWith('chat-')){window.__beforeRefusal?.();return null;}return open(url,name,...rest);};
 });
 await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`);
 const chat=page.locator('.page-layout .panel-chat');
 const nav=page.getByRole('navigation',{name:'Workspaces'});
 const alphaLink=nav.locator('.workspace-row').filter({hasText:'Alpha feature'}).locator('.workspace-link');
 const betaLink=nav.locator('.workspace-row').filter({hasText:'Beta release'}).locator('.workspace-link');
 const sessionChoice=page.getByRole('combobox',{name:'Session',exact:true});
 await expect(chat.getByRole('textbox',{name:'Message',exact:true})).toBeEnabled();
 await expect(sessionChoice).toHaveValue('alpha-session');

 // 1. Refused on the same page: the chat is back on it, and the refusal is said.
 await chat.getByRole('button',{name:'Open chat window ↗',exact:true}).click();
 await expect(page.getByRole('alert').filter({hasText:'The chat window was blocked by the browser'})).toBeVisible();
 await expect(chat.getByRole('textbox',{name:'Message',exact:true})).toBeEnabled();
 await expect(sessionChoice).toHaveValue('alpha-session');

 // 2. Refused after a switch to Beta: Beta keeps its own page; Alpha gets its chat back.
 await page.evaluate(()=>{window.__beforeRefusal=()=>{window.__beforeRefusal=undefined;[...document.querySelectorAll('.workspace-row')].find(row=>row.textContent.includes('Beta release'))?.querySelector('.workspace-link')?.click();};});
 await chat.getByRole('button',{name:'Open chat window ↗',exact:true}).click();
 await expect(betaLink).toHaveAttribute('aria-current','page');
 await expect(page.getByRole('alert').filter({hasText:'The chat window was blocked by the browser'})).toBeVisible();
 await expect(page.getByRole('combobox',{name:'Workspace',exact:true})).toHaveValue('beta');
 await expect(sessionChoice,'Beta shows its own conversation, not the chat that left Alpha').not.toHaveValue('alpha-session');
 await alphaLink.click();
 await expect(alphaLink).toHaveAttribute('aria-current','page');
 await expect(sessionChoice).toHaveValue('alpha-session');
 await expect(chat.getByRole('textbox',{name:'Message',exact:true}),'Alpha has its chat back on the page').toBeEnabled();
 await expect(page.getByRole('region',{name:'In other windows'})).toHaveCount(0);
 assert.deepEqual(errors,[]);
 console.log('PASS: a chat whose window is refused comes back on its page, and after a workspace switch meanwhile, in the workspace it left, not the one on screen. Fixture only.');
}finally{await browser?.close();await server.close();await fixture.close();}
