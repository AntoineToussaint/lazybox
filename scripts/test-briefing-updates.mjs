// Fixture only: a conversation started before Lazybox gained something hears
// of it with the person's next message (runner/briefing.go), and the person
// sees that as one quiet event, never as their own words: an icon and "Lazybox
// told Claude about new features", before their message, opened (by mouse or
// keyboard) to exactly what the agent received. Screenshots:
// .cache/briefing-updates-*.png, in dark and light.
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from './fixtures/vite-cache.mjs';import react from '@vitejs/plugin-react';import {webkit,chromium,expect} from './fixtures/playwright.mjs';import {fileURLToPath} from 'node:url';import {mkdir} from 'node:fs/promises';import assert from 'node:assert/strict';import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import { collectPageErrors, drainPageErrors } from './fixtures/page-errors.mjs';

const f=await startRunnerFixture();
const DELTA='[Lazybox] Since this conversation started, Lazybox added or changed the following; everything else you were told still holds.\n- Start another conversation with any agent (Claude, Codex, …) when the person asks: `lazybox conversation new --provider P --title T --brief-artifact ID` (or `--brief "…"`), which the person approves in one click. Your own sub-agent tool only starts copies of yourself, and switching this conversation\'s subscription never changes its provider.';
Object.assign(f.sessions[0],{title:'Port the parser',provider:'claude',model:'claude-sonnet-4-6',modelReported:true,status:'ready',busy:false,agents:[{id:'root',status:'ready'}],instructionsPending:true,briefingUpdate:DELTA});
const ch=f.openEvents('alpha-session');
ch.push({type:'operation_accepted',text:'Port the parser to the new AST.',operationId:'p1'});
ch.push({type:'turn_started'});
ch.push({type:'message_delta',text:'The parser is ported.'});
ch.push({type:'turn_ended'});

const server=await createServer({...testViteCache(import.meta.url),configFile:false,logLevel:'error',root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:f.url}}}});let browser;
try{
 await server.listen();await mkdir('.cache',{recursive:true});browser=await (process.env.TEST_CHROMIUM?chromium:webkit).launch();
 const page=await browser.newPage({viewport:{width:1280,height:900},colorScheme:'dark'});page.setDefaultTimeout(15000);
 const errors=[];await collectPageErrors(page, errors);
 await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${f.token}`);
 const chat=page.locator('.embedded-chat:visible').first(),messages=chat.locator('.messages:visible');
 await expect(messages).toContainText('The parser is ported.');
 await expect(chat.getByRole('status').filter({hasText:'Instructions updated'}),'pending until the next message').toBeVisible();
 await expect(messages.locator('.system-event')).toHaveCount(0);

 // ── The person's next message carries the update.
 const composer=chat.getByRole('textbox',{name:'Message',exact:true});
 await composer.fill('Continue this with Codex.');await composer.press('Enter');
 await expect.poll(()=>f.mutations.filter(m=>m.method==='Apply'&&m.body.message).length).toBe(1);
 const update=messages.locator('.system-event[data-kind=briefing_update]');
 await expect(update).toHaveCount(1);
 await expect(update.locator('.system-event-text')).toHaveText('Lazybox told Claude about new features');
 await expect(update).toHaveAttribute('data-tone','muted');
 // Never the person's words: theirs is the one bubble, and the update is not in any.
 const yours=messages.locator('article.message.operation_accepted');
 await expect(yours).toHaveCount(2);
 await expect(yours.filter({hasText:'Continue this with Codex.'})).toHaveCount(1);
 await expect(yours.filter({hasText:'[Lazybox]'})).toHaveCount(0);
 await expect(messages).not.toContainText('Since this conversation started');
 // One line, quiet, just before the message it went with.
 {const box=await update.locator('.system-event-line').boundingBox();assert.ok(box.height<=30,`the update is one line (${box.height}px)`);}
 const order=await messages.evaluate(root=>[...root.querySelectorAll('.system-event[data-kind=briefing_update], article.message.operation_accepted')].map(n=>n.matches('.system-event')?'update':n.textContent.includes('Continue this with Codex.')?'next':'first'));
 assert.deepEqual(order,['first','update','next'],'the update sits before the message it went with');
 await page.screenshot({path:'.cache/briefing-updates-line-dark.png'});

 // ── It opens to exactly what the agent received, and closes, by mouse or keyboard.
 const toggle=update.locator('.system-event-toggle');
 await expect(toggle).toHaveAttribute('aria-expanded','false');
 await toggle.click();
 await expect(toggle).toHaveAttribute('aria-expanded','true');
 const body=update.locator('.system-event-body');
 await expect(body).toContainText('What Lazybox told Claude');
 await expect(body.locator('.system-event-text-sent')).toHaveText(DELTA);
 await page.screenshot({path:'.cache/briefing-updates-open-dark.png'});
 await page.emulateMedia({colorScheme:'light'});
 await page.screenshot({path:'.cache/briefing-updates-open-light.png'});
 await toggle.focus();await page.keyboard.press('Enter');
 await expect(body).toHaveCount(0);
 await expect(toggle,'it opens and closes from the keyboard').toBeFocused();
 await page.keyboard.press('Enter');
 await expect(update.locator('.system-event-body')).toBeVisible();
 await page.keyboard.press('Enter');

 // ── Never in the prompts the person can recall.
 await composer.focus();await page.keyboard.press('ArrowUp');
 await expect(composer).toHaveValue('Continue this with Codex.');
 await composer.fill('');

 // ── Said once: the next message goes without it.
 await composer.fill('And keep the tests green.');await composer.press('Enter');
 await expect.poll(()=>f.mutations.filter(m=>m.method==='Apply'&&m.body.message).length).toBe(2);
 await expect(update).toHaveCount(1);
 await drainPageErrors(errors);assert.deepEqual(errors,[]);
 console.log('briefing updates: one quiet event before the message it went with, opened to what the agent received');
}finally{await browser?.close();await server.close();await f.close();}
