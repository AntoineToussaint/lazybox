// Prose wraps in the editor; code does not; Alt+Z toggles wrapping and is remembered.
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from 'vite';
import react from '@vitejs/plugin-react';
import {webkit,expect} from './fixtures/playwright.mjs';
import {fileURLToPath} from 'node:url';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import {monacoVimCompat} from './monaco-vim-compat.mjs';
import { collectPageErrors } from './fixtures/page-errors.mjs';
const fixture=await startRunnerFixture();
const long='**Canonical instructions live in [AGENTS.md](AGENTS.md).** '+'This file is a forwarding note so tools looking for it find the same content every other agent uses. '.repeat(4);
fixture.workspaceFiles.set('alpha/api/README.md',{path:'api/README.md',text:`# Title\n\n${long}\n`,version:'v1',binary:false,truncated:false});
const channel=fixture.openEvents('alpha-session');channel.push({type:'message_delta',text:'Open [README](api/README.md).'});
const server=await createServer({...testViteCache(import.meta.url),configFile:false,root:fileURLToPath(new URL('../apps/web',import.meta.url)),logLevel:'warn',plugins:[react(),monacoVimCompat()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
let browser;
try{
 await server.listen();browser=await webkit.launch({headless:true});const page=await browser.newPage({viewport:{width:1200,height:900}});page.setDefaultTimeout(20000);
 const errors=[];await collectPageErrors(page, errors);
 await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`);
 const open=async()=>{await page.getByRole('button',{name:'README',exact:true}).click({modifiers:['Alt']});const dialog=page.getByRole('dialog',{name:'File preview',exact:true});await dialog.getByRole('button',{name:'Edit file',exact:true}).click();
  // Monaco's first load from a cold dev server outlasts expect's 5 s on a CI runner.
  await expect(dialog.locator('.view-lines')).toContainText('Canonical',{timeout:20000});return dialog;};
 const rows=dialog=>dialog.locator('.view-lines .view-line').count();
 let dialog=await open();
 // 3 lines of Markdown, the long one wrapped onto several rows, nothing past the right edge.
 await expect.poll(()=>rows(dialog)).toBeGreaterThan(4);
 const overflow=await dialog.locator('.monaco-editor').evaluate(n=>{const box=n.getBoundingClientRect();return [...n.querySelectorAll('.view-line span')].some(s=>s.getBoundingClientRect().right>box.right+1);});
 assert.equal(overflow,false,'no text past the editor edge');
 // Alt+Z turns wrapping off (one row per line) and the choice is remembered.
 await dialog.getByRole('textbox',{name:'File source editor',exact:true}).focus();await page.keyboard.press('Alt+z');
 await expect.poll(()=>rows(dialog)).toBeLessThanOrEqual(4);
 await page.keyboard.press('Escape');await page.keyboard.press('Escape');
 await page.reload();dialog=await open();
 await expect.poll(()=>rows(dialog)).toBeLessThanOrEqual(4);
 await dialog.getByRole('textbox',{name:'File source editor',exact:true}).focus();await page.keyboard.press('Alt+z');
 await expect.poll(()=>rows(dialog)).toBeGreaterThan(4);
 assert.deepEqual(errors,[]);
 console.log('PASS: Markdown wraps by default with nothing past the edge; Alt+Z toggles wrapping and is remembered.');
}finally{await browser?.close();await server.close();await fixture.close();}
