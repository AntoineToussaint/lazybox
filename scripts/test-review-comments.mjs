// Inline review comments on the Changes page against the fake Runner: comment
// on a range and on a removed line, edit, see the tray collect them, follow a
// comment whose lines moved or changed, send the review to the conversation's
// message box (never sent for the person), and find the same comments in a
// second window.
import {createServer} from 'vite';
import react from '@vitejs/plugin-react';
import {chromium,expect} from './fixtures/playwright.mjs';
import {fileURLToPath} from 'node:url';
import {mkdir} from 'node:fs/promises';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import {testViteCache} from './fixtures/vite-cache.mjs';

const fixture=await startRunnerFixture();
fixture.sessions[0].title='Harden the handler';
const api=fixture.shipState('alpha','api');
api.files=[{path:'src/handler.go',indexStatus:' ',worktreeStatus:'M'}];
const DIFF=`diff --git a/src/handler.go b/src/handler.go
index 1111111..2222222 100644
--- a/src/handler.go
+++ b/src/handler.go
@@ -1,7 +1,8 @@
 package api

-func Handle() {
+func Handle() error {
+\tlog.Print("handle")
 \tif ready() {
 \t\treturn
 \t}
 }
`;
fixture.diffs.set('alpha/api/src/handler.go',DIFF);
const server=await createServer({...testViteCache(import.meta.url),configFile:false,root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
const reviewWrites=()=>fixture.mutations.filter(m=>/Review/.test(m.method));
let browser;
try{
 await server.listen();browser=await chromium.launch({headless:true});
 const url=`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace#token=${fixture.token}`;
 const context=await browser.newContext({viewport:{width:1500,height:1000}});
 const page=await context.newPage();page.setDefaultTimeout(10000);
 const errors=[];page.on('pageerror',e=>errors.push(String(e)));
 await page.goto(url);
 await page.getByRole('combobox',{name:'Workspace',exact:true}).selectOption('alpha');await page.getByRole('combobox',{name:'Session',exact:true}).selectOption('alpha-session');
 await page.getByRole('button',{name:'Review changes',exact:true}).click();
 const changes=page.getByRole('region',{name:'Workspace changes'});
 const diff=changes.getByRole('grid',{name:'File diff'});
 await expect(diff).toContainText('log.Print("handle")');
 // No comments yet: the tray stays out of the way until asked for.
 await expect(changes.getByRole('complementary',{name:'Review'})).toHaveCount(0);

 // 1. A range: click + on line 3, Shift-click line 4. The composer names the range.
 await diff.getByRole('row').filter({hasText:'func Handle() error {'}).hover();
 await diff.getByRole('button',{name:'Comment on line 3',exact:true}).click();
 await diff.getByRole('button',{name:'Comment on line 4',exact:true}).click({modifiers:['Shift']});
 let composer=changes.getByRole('form',{name:'Comment on lines 3–4'});
 const field=composer.getByRole('textbox',{name:'Comment'});await expect(field).toBeFocused();
 await expect(diff.getByRole('row',{selected:true})).toHaveCount(2);
 await field.fill('Return the error instead of **logging** it.');
 await mkdir('.cache',{recursive:true});await page.screenshot({path:'.cache/review-compose.png'});
 await field.press('Meta+Enter');await expect(composer).toHaveCount(0);
 const added=fixture.mutations.find(m=>m.method==='AddReviewComment').body;
 assert.deepEqual({member:added.member,path:added.path,side:added.side,startLine:added.startLine,snippet:added.snippet,view:added.view},
  {member:'api',path:'src/handler.go',side:'new',startLine:3,snippet:['func Handle() error {','\tlog.Print("handle")'],view:'uncommitted'});
 const thread=changes.getByRole('article',{name:'Comment on lines 3–4'});
 await expect(thread).toContainText('Return the error instead of logging it.');await expect(thread.locator('strong').filter({hasText:'logging'})).toHaveCount(1);
 await expect(thread).toContainText(`uncommitted changes on ${api.head.slice(0,8)}`);
 // The first comment opens the tray.
 const tray=changes.getByRole('complementary',{name:'Review'});
 await expect(tray).toContainText('1 pending');await expect(changes.getByRole('button',{name:'Review comments, 1 pending'})).toBeVisible();

 // 2. A removed line, from the keyboard alone.
 await diff.getByRole('button',{name:'Comment on removed line 3'}).focus();await page.keyboard.press('Enter');
 composer=changes.getByRole('form',{name:'Comment on removed line 3'});
 await composer.getByRole('textbox',{name:'Comment'}).fill('Keep the old signature as a wrapper for callers.');
 await composer.getByRole('button',{name:'Add comment'}).click();await expect(composer).toHaveCount(0);
 assert.equal(fixture.mutations.filter(m=>m.method==='AddReviewComment')[1].body.side,'old');
 await expect(tray).toContainText('2 pending');

 // 3. Edit; Escape abandons a draft without saving.
 await thread.getByRole('button',{name:'Edit'}).click();
 const edit=thread.getByRole('form',{name:'Edit comment'});await edit.getByRole('textbox',{name:'Comment'}).fill('Return the error; do not log it here.');
 await edit.getByRole('button',{name:'Save'}).click();await expect(thread).toContainText('Return the error; do not log it here.');
 assert.equal(reviewWrites().filter(m=>m.method==='EditReviewComment').length,1);
 await diff.getByRole('button',{name:'Comment on line 6',exact:true}).click();
 await page.keyboard.type('never mind');await page.keyboard.press('Escape');
 await expect(changes.getByRole('form',{name:'Comment on line 6'})).toHaveCount(0);
 assert.equal(fixture.mutations.filter(m=>m.method==='AddReviewComment').length,2,'Escape must not save');

 // 4. The agent moves the lines: the comment says so and follows them.
 const handler=['package api','','import "log"','','func Handle() error {','\tlog.Print("handle")','\tif ready() {','\t\treturn','\t}','}'].join('\n');
 fixture.workingFiles.set('alpha/api/src/handler.go',handler);
 await expect(thread).toContainText('Moved to lines 5–6',{timeout:15000});
 // Then changes them: outdated, with the lines quoted as they were.
 fixture.workingFiles.set('alpha/api/src/handler.go',handler.replace('\tlog.Print("handle")','\treturn errors.New("x")'));
 await expect(thread).toContainText('Outdated',{timeout:15000});
 await expect(thread.getByLabel('Quoted lines')).toContainText('log.Print("handle")');
 await expect(tray.getByRole('button',{name:/Lines 3–4.*Outdated/})).toBeVisible();
 // A new diff without those lines lists the comment above it, quote included.
 fixture.diffs.set('alpha/api/src/handler.go',DIFF.replace('+\tlog.Print("handle")','+\treturn errors.New("x")'));
 await changes.getByRole('button',{name:'Refresh changes'}).click();
 const elsewhere=changes.getByRole('region',{name:'Comments not on lines in this diff'});
 await expect(elsewhere).toContainText('1 comment on lines not in this diff');await expect(elsewhere.getByRole('article',{name:'Comment on lines 3–4'})).toBeVisible();
 await page.screenshot({path:'.cache/review-outdated.png'});

 // 5. Send the review: one message in the conversation's message box, not sent.
 const applies=fixture.mutations.filter(m=>m.method==='Apply').length;
 await tray.getByRole('button',{name:'Send 2 comments to the agent'}).click();
 const box=page.getByRole('region',{name:'Agent chat'}).getByRole('textbox',{name:'Message',exact:true});
 await expect(box).toHaveValue(/^Please address these 2 review comments/);
 await expect(box).toHaveValue(/### 1\. api\/src\/handler\.go:3–4\n\n_uncommitted changes on [0-9a-f]{8} · outdated: these lines have changed since_\n\n```go\nfunc Handle\(\) error \{\n\tlog\.Print\("handle"\)\n```\n\nReturn the error; do not log it here\./);
 await expect(box).toHaveValue(/### 2\. api\/src\/handler\.go — removed line 3 \(in the version before the change\)/);
 await expect(changes).toContainText('Review it and send');
 assert.equal(fixture.mutations.filter(m=>m.method==='Apply').length,applies,'the review must not be sent on the person\'s behalf');
 const sent=reviewWrites().find(m=>m.method==='SetReviewDisposition').body;assert.equal(sent.disposition,'sent');assert.equal(sent.sessionId,'alpha-session');assert.equal(sent.ids.length,2);
 await expect(tray).toContainText('2 sent');await expect(tray).toContainText('Nothing pending');
 await expect(thread).toContainText('Sent to “Harden the handler”');
 await page.screenshot({path:'.cache/review-sent.png'});

 // 6. Another window of the same Runner shows the same review; a change there
 // reaches this one.
 const second=await context.newPage();second.setDefaultTimeout(10000);second.on('pageerror',e=>errors.push(String(e)));
 await second.goto(url);
 await second.getByRole('combobox',{name:'Workspace',exact:true}).selectOption('alpha');await second.getByRole('combobox',{name:'Session',exact:true}).selectOption('alpha-session');
 await second.getByRole('button',{name:'Review changes',exact:true}).click();
 const otherTray=second.getByRole('complementary',{name:'Review'});
 await otherTray.getByRole('button',{name:'Sent',exact:false}).first().click();
 await expect(otherTray.getByRole('button',{name:/Removed line 3/})).toBeVisible();
 await otherTray.getByRole('button',{name:/Removed line 3/}).click();
 const removedThread=second.getByRole('article',{name:'Comment on removed line 3'});
 await expect(removedThread).toHaveClass(/is-focused/);
 await removedThread.getByRole('button',{name:'Resolve'}).click();await expect(removedThread).toContainText('Resolved');
 await tray.getByRole('button',{name:/^Done/}).click();
 await expect(tray.getByRole('button',{name:/Removed line 3.*Resolved/})).toBeVisible({timeout:15000});

 // 7. Reopen puts it back in Pending; Delete asks first.
 await page.getByRole('article',{name:'Comment on removed line 3'}).getByRole('button',{name:'Reopen'}).click();
 await expect(tray.getByRole('button',{name:/^Pending/})).toContainText('1');
 const removed=page.getByRole('article',{name:'Comment on removed line 3'});
 await removed.getByRole('button',{name:'Delete'}).click();
 await expect(removed.getByRole('group',{name:'Confirm delete'})).toBeVisible();await removed.getByRole('button',{name:'Delete'}).click();
 await expect(removed).toHaveCount(0);assert.equal(fixture.reviewOf('alpha').comments.length,1);
 assert.deepEqual(errors,[]);
 console.log('PASS: range and removed-line comments, edit, tray, moved and outdated anchors, one unsent message in the composer, shared across windows, resolve, reopen, delete.');
}finally{await browser?.close();await server.close();await fixture.close();}
