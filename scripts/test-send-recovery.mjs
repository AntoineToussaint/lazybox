import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from './fixtures/vite-cache.mjs';
import react from '@vitejs/plugin-react';
import {webkit,expect} from './fixtures/playwright.mjs';
import {fileURLToPath} from 'node:url';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import {monacoVimCompat} from './monaco-vim-compat.mjs';
import { collectPageErrors } from './fixtures/page-errors.mjs';

const fixture=await startRunnerFixture();
const session=fixture.sessions[0];
Object.assign(session,{fault:'provider_error',busy:false,status:'failed'});
const server=await createServer({...testViteCache(import.meta.url),configFile:false,root:fileURLToPath(new URL('../apps/web',import.meta.url)),logLevel:'warn',plugins:[react(),monacoVimCompat()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
await server.listen();
const png=Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==','base64');
let browser;
try{
 browser=await webkit.launch({headless:true});
 const page=await browser.newPage({viewport:{width:1300,height:850}});page.setDefaultTimeout(15000);
 const errors=[];await collectPageErrors(page, errors);
 const url=`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=${session.collectionId}&session=${session.id}#token=${fixture.token}`;
 await page.goto(url);
 const composer=page.getByRole('textbox',{name:'Message',exact:true});
 const applies=()=>fixture.mutations.filter(m=>m.method==='Apply').length;


 await expect(page.getByRole('button',{name:'Attach files',exact:true})).toBeEnabled();
 await page.locator('input[type=file]').first().setInputFiles({name:'CleanShot.png',mimeType:'image/png',buffer:png});
 // An attached image is a thumbnail named by its label (attachment-previews covers the rest).
 await expect(page.getByRole('list',{name:'Attached files'}).getByRole('button',{name:/^Open CleanShot\.png,/})).toBeVisible();
 await composer.fill('I want to do MCP');
 await page.reload();
 const notice=page.getByRole('group',{name:'Files to attach again'});
 await expect(page.getByRole('list',{name:'Attached files'}).getByRole('button',{name:/^Open CleanShot/})).toBeVisible();
 await expect(notice).toHaveCount(0);
 const before=applies();
 await expect(page.getByRole('button',{name:'Send message',exact:true})).toBeEnabled();
 await composer.press('Enter');
 await expect.poll(applies).toBe(before+1);
 const sent=fixture.mutations.filter(m=>m.method==='Apply').at(-1).body;
 assert.ok(JSON.stringify(sent).includes('I want to do MCP'),'the draft text is sent');
 assert.equal(sent.message.attachments[0].data,png.toString('base64'),'the restored screenshot is sent');
 await expect(composer).toHaveValue('');
 // Old drafts only have names; they still require explicit removal or reattachment.
 await page.evaluate(id => {
   const key = Object.keys(localStorage).find(key => key.endsWith(':draft:' + id));
   localStorage.setItem(key, JSON.stringify({text:'legacy draft',files:[{name:'old.png',size:3,lastModified:1}],operationId:''}));
 },session.id);
 await page.reload();
 await expect(notice).toContainText('old.png');
 await expect(page.getByRole('button',{name:'Send message',exact:true})).toBeDisabled();
 await notice.getByRole('button',{name:'Remove old.png'}).click();
 await expect(notice).toHaveCount(0);
 // A conversation the Runner refuses says why instead of swallowing Enter.
 const other=fixture.sessions[1];Object.assign(other,{fault:'provider delivery failed; reconcile before continuing',busy:false,status:'failed'});
 await page.goto(url.replace(`collection=${session.collectionId}`,`collection=${other.collectionId}`).replace(`session=${session.id}`,`session=${other.id}`));
 const otherComposer=page.getByRole('textbox',{name:'Message',exact:true});
 await otherComposer.fill('anything');const refused=applies();await otherComposer.press('Enter');
 await expect(page.getByRole('alert').filter({hasText:'Resume it or start a new conversation to continue.'})).toBeVisible();
 assert.equal(applies(),refused,'a refused conversation sends nothing');
 Object.assign(other,{fault:'unknown'});
 const events=fixture.openEvents(other.id);
 events.push({type:'failure',status:'unknown',text:'Turn failed',turnId:'first'});
 await page.reload();
 const recover=page.getByRole('button',{name:'Continue from here'});
 await expect(otherComposer).toHaveValue('anything');
 await expect(recover).toHaveCount(0);
 await otherComposer.fill('');
 await recover.click();
 await expect.poll(applies).toBe(refused+1);
 assert.match(fixture.mutations.filter(m=>m.method==='Apply').at(-1).body.message.text,/Check the existing commands and their results first/);
 // The Apply receipt can arrive before GetSession clears the old fault.
 await expect(page.getByText('Continuation sent.', {exact:true})).toBeVisible();
 await expect(recover).toHaveCount(0);
 events.push({type:'failure',status:'unknown',text:'Another turn failed',turnId:'next'});
 await expect(recover).toBeVisible();
 assert.equal(applies(),refused+1,'a new fault offers recovery without sending automatically');
 assert.deepEqual(errors,[]);
 console.log('PASS: screenshot bytes survive a reload; a provider error does not block the next message; a refused conversation says why.');
}finally{await browser?.close();await server.close();await fixture.close();}
