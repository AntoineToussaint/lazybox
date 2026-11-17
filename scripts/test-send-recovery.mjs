// Sending is never silently blocked: after a provider error the next message
// continues the conversation, and a draft whose image was lost in a window
// reload says so, offering "Attach again…" and "Send without it".
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from 'vite';
import react from '@vitejs/plugin-react';
import {webkit,expect} from '@playwright/test';
import {fileURLToPath} from 'node:url';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import {monacoVimCompat} from './monaco-vim-compat.mjs';

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
 const errors=[];page.on('pageerror',e=>errors.push(String(e)));
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
 await expect(notice).toBeVisible();
 await expect(notice).toContainText('CleanShot.png');
 await expect(notice.getByRole('button',{name:'Attach again…'})).toBeVisible();
 // Enter explains instead of doing nothing.
 const before=applies();
 await composer.press('Enter');
 await expect(page.getByRole('alert').filter({hasText:'Attach CleanShot.png again, or remove it, to send.'})).toBeVisible();
 assert.equal(applies(),before,'nothing is sent while a file is missing');
 // Send without it: the notice goes and, despite the provider error, the message is sent.
 await notice.getByRole('button',{name:'Send without it'}).click();
 await expect(notice).toHaveCount(0);
 await expect(composer).toBeFocused();
 await composer.press('Enter');
 await expect.poll(applies).toBe(before+1);
 const sent=fixture.mutations.filter(m=>m.method==='Apply').at(-1).body;
 assert.ok(JSON.stringify(sent).includes('I want to do MCP'),'the draft text is sent');
 assert.ok(!JSON.stringify(sent).includes('CleanShot'),'the removed file is not sent');
 await expect(composer).toHaveValue('');
 // A conversation the Runner refuses says why instead of swallowing Enter.
 const other=fixture.sessions[1];Object.assign(other,{fault:'provider delivery failed; reconcile before continuing',busy:false,status:'failed'});
 await page.goto(url.replace(`collection=${session.collectionId}`,`collection=${other.collectionId}`).replace(`session=${session.id}`,`session=${other.id}`));
 const otherComposer=page.getByRole('textbox',{name:'Message',exact:true});
 await otherComposer.fill('anything');const refused=applies();await otherComposer.press('Enter');
 await expect(page.getByRole('alert').filter({hasText:'Resume it or start a new conversation to continue.'})).toBeVisible();
 assert.equal(applies(),refused,'a refused conversation sends nothing');
 assert.deepEqual(errors,[]);
 console.log('PASS: lost attachment after reload is explained (Enter → reason, Attach again…, Send without it); a provider error does not block the next message; a refused conversation says why.');
}finally{await browser?.close();await server.close();await fixture.close();}
