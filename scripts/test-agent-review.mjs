import {openWorkspaceChanges} from './fixtures/workspace-changes.mjs';
// One-click cross-agent review against the fake Runner (GITHUB.md, "Agent
// review"): Review with Codex from Changes opens a read-only reviewer
// conversation with its brief; the reviewer's findings land in the Review
// tray with their severity and author; accept one, dismiss one with a reason,
// send the accepted one to the author's message box exactly as filed; the
// author's conversation shows the review live, then its outcome. Then the
// other ways in: a turn's ⋯ menu, the PR card, the file list's context menu,
// ⌘K, and Settings → Agents & workspaces → Review by another agent. The
// header button, `/review <focus>`, the default reviewer and the review tab
// are test-review-from-chat.mjs's.
import {createServer} from './fixtures/vite-cache.mjs';
import react from '@vitejs/plugin-react';
import {chromium,expect} from './fixtures/playwright.mjs';
import {fileURLToPath} from 'node:url';
import {mkdir} from 'node:fs/promises';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import {testViteCache} from './fixtures/vite-cache.mjs';
import { collectPageErrors } from './fixtures/page-errors.mjs';

const fixture=await startRunnerFixture();
const author=fixture.sessions[0];
Object.assign(author,{title:'Harden the handler',provider:'claude',status:'ready',busy:false,agents:[{id:'root',name:'Main agent',status:'done'}]});
const events=fixture.openEvents(author.id);
events.push({type:'operation_accepted',operationId:'op-1',text:'Return errors from Handle instead of logging them'});
events.push({type:'message_delta',text:'REPLY_ONE: Handle returns its error now.'});events.push({type:'turn_ended'});
fixture.setCheckpoints(author.id,{turns:[{operationId:'op-1',prompt:'Return errors from Handle instead of logging them',files:[{member:'api',path:'src/handler.go',status:'M',additions:2,deletions:1}]}]});
const api=fixture.shipState('alpha','api');
Object.assign(api,{githubRepository:'fixture-team/api',files:[{path:'src/handler.go',indexStatus:' ',worktreeStatus:'M'}]});
fixture.diffs.set('alpha/api/src/handler.go',`diff --git a/src/handler.go b/src/handler.go
--- a/src/handler.go
+++ b/src/handler.go
@@ -1,5 +1,6 @@
 package api

-func Handle() {
+func Handle() error {
+\tlog.Print("handle")
 \treturn
 }
`);
const starts=()=>fixture.mutations.filter(m=>m.method==='StartReview').map(m=>m.body);
const server=await createServer({...testViteCache(import.meta.url),configFile:false,logLevel:'error',root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
let browser;
try{
 await server.listen();await mkdir('.cache',{recursive:true});
 browser=await chromium.launch({headless:true});
 let page=await browser.newPage({viewport:{width:1500,height:1000}});page.setDefaultTimeout(10000);
 const errors=[];await collectPageErrors(page, errors);
 const mod='Meta';
 await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=${author.id}#token=${fixture.token}`);
 await expect(page.locator('.messages:visible')).toContainText('REPLY_ONE');
 await openWorkspaceChanges(page);
 const changes=page.getByRole('region',{name:'Workspace changes'});
 await expect(changes.getByRole('grid',{name:'File diff'})).toContainText('log.Print("handle")');

 // 1. One click: Codex (the other provider than Claude, the author) reviews the changes.
 const reviewWith=changes.getByRole('group',{name:'Review with another agent'});
 await expect(reviewWith.getByRole('button',{name:'Review with Codex'})).toBeVisible();
 await page.screenshot({path:'.cache/agent-review-start.png'});
 await reviewWith.getByRole('button',{name:'Review with Codex'}).click();
 await expect.poll(()=>starts().length).toBe(1);
 // The default reviewer is left to the Runner, which applies the setting (the other agent).
 assert.deepEqual((({collectionId,sourceSessionId,scope,compare='',provider='',profileId=''})=>({collectionId,sourceSessionId,scope,compare,provider,profileId}))(starts()[0]),{collectionId:'alpha',sourceSessionId:author.id,scope:'changes',compare:'',provider:'',profileId:''});
 const reviewer=fixture.sessions.find(s=>s.title==='Review · Changes · Codex');
 assert.equal(reviewer.permission,'read-only');assert.equal(reviewer.provider,'codex');
 const chat=page.locator('section.panel-chat');
 // The tray follows the review live.
 const tray=changes.getByRole('complementary',{name:'Review'});
 await expect(tray.getByRole('list',{name:'Agent reviews'})).toContainText('Codex is reviewing changes');

 // 2. Codex files two findings (`lazybox review add`): they arrive to triage,
 // with their severity and author.
 const run=fixture.reviewOf('alpha').runs[0];
 const blocker=fixture.fileFinding(run.id,{path:'src/handler.go',startLine:3,endLine:4,snippet:['func Handle() error {','\tlog.Print("handle")'],severity:'blocker',title:'The error is logged, not returned',body:'Return it so callers can retry. Logged here, the failure never reaches the request handler, which answers 200 with an empty body instead.'});
 const minor=fixture.fileFinding(run.id,{path:'src/handler.go',startLine:4,snippet:['\tlog.Print("handle")'],severity:'minor',title:'Use the structured logger',body:'log.Print loses the request id.'});
 await expect(tray.getByRole('button',{name:/^To triage/})).toContainText('2',{timeout:15000});
 await expect(tray.getByRole('button',{name:/^To triage/})).toHaveAttribute('aria-pressed','true');
 const blockerRow=tray.locator('.review-tray-list').getByRole('listitem').filter({hasText:'The error is logged, not returned'});
 await expect(blockerRow).toContainText('Blocker');await expect(blockerRow).toContainText('from Codex');
 await expect(tray.locator('.review-tray-list').getByRole('listitem').filter({hasText:'Use the structured logger'})).toContainText('Minor');
 // A finding's text and the review's summary wrap in the narrow tray: nothing is cut off.
 const clipped=await tray.locator('.review-tray-body, .review-run-summary').evaluateAll(nodes=>nodes.filter(node=>node.scrollWidth>node.clientWidth+1).map(node=>node.textContent));
 assert.deepEqual(clipped,[],'no tray text runs past its box');
 // Filters: by severity, by author.
 await tray.getByRole('combobox',{name:'Severity'}).selectOption('blocker');
 await expect(tray.locator('.review-tray-list').getByRole('listitem')).toHaveCount(1);
 await tray.getByRole('combobox',{name:'Severity'}).selectOption('');
 await expect(tray.getByRole('combobox',{name:'Author'})).toContainText('Codex');
 await page.screenshot({path:'.cache/agent-review-findings.png'});
 // In the diff: a finding card with its severity, title and triage.
 const finding=changes.getByRole('article',{name:'Blocker finding on lines 3–4'});
 await expect(finding).toContainText('The error is logged, not returned');await expect(finding).toContainText('from Codex');

 // 3. Accept the blocker; dismiss the minor, saying why.
 await blockerRow.getByRole('button',{name:'Accept'}).click();
 await expect.poll(()=>fixture.mutations.filter(m=>m.method==='SetReviewDisposition').at(-1)?.body).toMatchObject({ids:[blocker.id],disposition:'open'});
 const minorRow=tray.locator('.review-tray-list').getByRole('listitem').filter({hasText:'Use the structured logger'});
 await minorRow.getByRole('button',{name:'Dismiss'}).click();
 await minorRow.getByRole('textbox',{name:'Why dismiss it'}).fill('Out of scope for this change');
 await page.keyboard.press('Enter');
 await expect.poll(()=>fixture.mutations.filter(m=>m.method==='SetReviewDisposition').at(-1)?.body).toMatchObject({ids:[minor.id],disposition:'dismissed',note:'Out of scope for this change'});
 await tray.getByRole('button',{name:/^Done/}).click();
 await expect(tray.locator('.review-tray-list').getByRole('listitem').filter({hasText:'Use the structured logger'})).toContainText('Dismissed: Out of scope for this change');

 // 4. Ask the reviewer: its conversation, with a follow-up started.
 await tray.getByRole('button',{name:/^Pending/}).click();
 await tray.locator('.review-tray-list').getByRole('listitem').filter({hasText:'The error is logged, not returned'}).getByRole('button',{name:'Ask Codex'}).click();
 const box=chat.getByRole('textbox',{name:'Message',exact:true});
 await expect(box).toHaveValue(`About your finding ${blocker.id} (blocker at api/src/handler.go:3–4, “The error is logged, not returned”): `);
 // The reviewer's conversation: its brief is its first message, an event from Lazybox,
 // never the person's words, that opens to the brief as sent.
 const briefEvent=chat.locator('.system-event[data-kind=review_brief]');
 await expect(briefEvent.locator('.system-event-text')).toHaveText("Asked Codex to review Claude's work");
 await expect(briefEvent.locator('.system-event-detail')).toHaveText('Changes');
 await expect(chat.locator('article.message.operation_accepted')).toHaveCount(0);
 await briefEvent.locator('.system-event-toggle').click();
 await expect(briefEvent.locator('.system-event-body')).toContainText('You are an independent reviewer (Codex) of work Claude did');
 await expect(briefEvent.locator('.system-event-body')).toContainText('lazybox review add');

 // 5. Send the accepted finding to the author, exactly as filed on the first click.
 // Closing the reviewer once its findings are all handed on is the default; the Runner decides when.
 const closeBox=tray.getByRole('checkbox',{name:'Close the reviewer after sending'});
 await expect(closeBox).toBeChecked();
 // Turned off, it is remembered for every send button; turned back on for this run.
 await closeBox.uncheck();
 assert.equal(await page.evaluate(()=>localStorage.getItem('lazybox:review-close-reviewers:v1')),'off');
 await closeBox.check();
 assert.equal(await page.evaluate(()=>localStorage.getItem('lazybox:review-close-reviewers:v1')),'on');
 const remaining=fixture.fileFinding(run.id,{path:'src/handler.go',startLine:3,snippet:['func Handle() error {'],severity:'nit',title:'Document the handler',body:'Describe its return value.'});
 fixture.finishReview(run.id);
 const applies=fixture.mutations.filter(m=>m.method==='Apply').length;
 await tray.getByRole('button',{name:/Fix 1 finding in/}).click();
 await expect.poll(()=>fixture.mutations.filter(m=>m.method==='Apply').length).toBe(applies+1);
 const sentFinding=fixture.mutations.filter(m=>m.method==='Apply').at(-1).body.message.text;
 assert.match(sentFinding, new RegExp(`### 1\\. api/src/handler\\.go:3–4\\n\\n\\*\\*Blocker: The error is logged, not returned\\*\\* \\(finding ${blocker.id}, from Codex\\)`));
 assert.match(sentFinding,/Return it so callers can retry/);
 await expect.poll(()=>fixture.mutations.filter(m=>m.method==='SetReviewDisposition').at(-1)?.body.sessionId).toBe(author.id);
 assert.equal(fixture.mutations.filter(m=>m.method==='SetReviewDisposition').at(-1).body.closeReviewers,true,'the tray asks the Runner to close a delivered reviewer');

 assert.equal(remaining.disposition,'proposed');
 assert.ok(!reviewer.closed && !reviewer.archivedAtUnixMillis,'a partial send leaves the reviewer open');

 // 6. The author's conversation shows the outcome.
 const card=page.locator('.messages:visible').getByRole('group',{name:'Codex review'});
 await expect(card).toContainText('Codex reviewed changes · 1 blocker',{timeout:15000});
 await page.screenshot({path:'.cache/agent-review-card.png'});
 await card.getByRole('button',{name:'Open review'}).click();
 await expect(tray.getByRole('list',{name:'Agent reviews'})).toContainText('Codex reviewed changes · 1 blocker');

 // 7. A review of a review is never offered: from the reviewer's conversation
 // the work under review is still the author's.
 await page.locator('.messages:visible').getByRole('group',{name:'Codex review'}).getByRole('button',{name:'Open conversation'}).click();
 await expect(chat.locator('.system-event[data-kind=review_brief] .system-event-text')).toHaveText("Asked Codex to review Claude's work");
 // A narrow Changes panel shows the files or the tray in its left column: the files come to the front.
 await changes.getByRole('tab',{name:/^Files/}).click();
 await changes.getByRole('navigation',{name:'Changed files'}).getByRole('button',{name:/src\/handler\.go/}).first().click({button:'right'});
 const menu=page.getByRole('menu');
 await menu.getByRole('menuitem',{name:'Review the changes with Codex'}).click();
 await expect.poll(()=>starts().length).toBe(2);
 assert.equal(starts()[1].sourceSessionId,author.id,'the reviewer conversation is never itself reviewed');

 // 8. ⌘K: "Review changes with Claude".
 await page.keyboard.press(`${mod}+k`);
 const palette=page.getByRole('dialog',{name:'Command palette'});
 await palette.getByRole('combobox',{name:'Search commands'}).fill('review changes with claude');
 await expect(palette.getByRole('option').first()).toContainText('Review changes with Claude');
 await page.keyboard.press('Enter');
 await expect.poll(()=>starts().length).toBe(3);
 assert.equal(starts()[2].provider,'claude');

 // 9. A turn's ⋯ menu: "Review this turn with Codex".
 // A fresh window on the author's conversation.
 await page.close();page=await browser.newPage({viewport:{width:1500,height:1000}});page.setDefaultTimeout(10000);await collectPageErrors(page, errors);
 await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=${author.id}#token=${fixture.token}`);
 const marker=page.locator('.messages:visible').getByRole('group',{name:'Turn 1 checkpoint'});
 await marker.getByLabel('Turn 1 actions',{exact:true}).click();
 await marker.getByRole('button',{name:'Review this turn with Codex'}).click();
 await expect.poll(()=>starts().length).toBe(4);
 assert.deepEqual((({scope,turn,provider=''})=>({scope,turn,provider}))(starts()[3]),{scope:'turn',turn:1,provider:''});
 // The reviewer opens as a tab beside the work (the newest review, next to its author); its brief says what it reviews.
 // A tab behind another does not follow its conversation, so the brief is read from the tab in front.
 await page.locator('[data-panel-tab]').filter({hasText:'Harden the handler'}).locator('xpath=ancestor::*[contains(concat(" ",normalize-space(@class)," ")," dv-tab ")][1]/following-sibling::*[1]').click();
 await expect(page.locator('section.panel-chat:visible .system-event[data-kind=review_brief] .system-event-detail')).toHaveText('Turn 1');

 // 9a. A review whose loop ends with majors still open is not "clean": it says what is left,
 // and hands those findings to the author in one click.
 // It ends while the author's tab is behind the reviewer's. Tabs behind hold no stream, so the
 // browser's six connections to the Runner stay free for the review's reads and the card updates.
 const turnRun=fixture.reviewOf('alpha').runs.at(-1);
 fixture.fileFinding(turnRun.id,{path:'src/handler.go',startLine:3,snippet:['func Handle() error {'],severity:'major',title:'Wrap the error',body:'Say which request failed.'});
 fixture.fileFinding(turnRun.id,{path:'src/handler.go',startLine:4,snippet:['\tlog.Print("handle")'],severity:'major',title:'Drop the log line',body:'It duplicates the error.'});
 fixture.finishReview(turnRun.id);
 await page.locator('[data-panel-tab]').filter({hasText:'Harden the handler'}).click();
 const turnCard=page.locator('.messages:visible').getByRole('group',{name:'Codex review'}).filter({hasText:'turn 1'});
 await expect(turnCard).toContainText('Codex reviewed turn 1 · 2 major',{timeout:15000});
 await expect(turnCard).toContainText('No blockers · 2 major open');
 await expect(turnCard).not.toContainText('Clean');
 await expect(turnCard.getByRole('button',{name:'Send 2 findings to Claude'})).toHaveClass(/primary/);
 await expect(turnCard.getByRole('button',{name:'Open review'})).not.toHaveClass(/primary/);
 await page.screenshot({path:'.cache/agent-review-majors-open.png'});
 const sentBefore=fixture.mutations.filter(m=>m.method==='Apply').length;
 const authorBox=page.locator('section.panel-chat:visible').getByRole('textbox',{name:'Message',exact:true});
 await authorBox.fill('Keep my draft');
 await turnCard.getByRole('button',{name:'Send 2 findings to Claude'}).click();
 await expect.poll(()=>fixture.mutations.filter(m=>m.method==='Apply').length).toBe(sentBefore+1);
 const handoff=fixture.mutations.filter(m=>m.method==='Apply').at(-1).body;
 assert.equal(handoff.sessionId,author.id);
 assert.match(handoff.message.text,/^Please address these 2 review comments/);
 assert.match(handoff.message.text,/\*\*Major: Wrap the error\*\*/);
 await expect(authorBox).toHaveValue('Keep my draft');
 await expect.poll(()=>fixture.mutations.filter(m=>m.method==='SetReviewDisposition').at(-1)?.body).toMatchObject({disposition:'sent',sessionId:author.id,closeReviewers:true});
 await expect(turnCard.getByRole('button',{name:/^Send \d+ finding/})).toHaveCount(0);
 const deliveredReviewer=fixture.sessions.find(s=>s.id===turnRun.reviewerSessionId);
 assert.ok(deliveredReviewer.closed && Number(deliveredReviewer.archivedAtUnixMillis)>0,'fully delivered reviewer stopped and archived');
 assert.equal(fixture.reviewOf('alpha').comments.filter(c=>c.reviewId===turnRun.id && c.disposition==='sent').length,2,'archiving preserves both findings');
 await authorBox.fill('');

 // 9b. The message box: `/review claude` (and `/review` alone is the default reviewer).
 // A fresh load of the author's conversation. This tab already signed in, and the app took #token= out
 // of the address bar: the plain URL loads the page (the same URL with #token= would only change the fragment).
 await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=${author.id}`);
 const message=page.locator('section.panel-chat').getByRole('textbox',{name:'Message',exact:true});
 await message.click();await page.keyboard.type('/review cla');
 await expect(page.getByRole('option',{name:/Review with Claude/})).toBeVisible();
 await page.keyboard.press('Enter');
 await expect.poll(()=>starts().length).toBe(5);
 assert.deepEqual((({scope,provider,sourceSessionId})=>({scope,provider,sourceSessionId}))(starts()[4]),{scope:'changes',provider:'claude',sourceSessionId:author.id});
 assert.equal(fixture.mutations.filter(m=>m.method==='Apply'&&m.body.sessionId===author.id&&/\/review/.test(m.body.message?.text||'')).length,0,'/review runs in Lazybox; nothing is sent to the agent');

 const reviewTabs=page.locator('[data-panel-tab]').filter({hasText:/^Review · /});
 await expect(reviewTabs.filter({hasText:'Claude'}).first()).toBeVisible();
 const beforePR=await reviewTabs.count();

 // 10. The PR card: "Review the PR with Codex".
 api.pr={number:'7',title:'Return handler errors',url:'https://github.com/fixture-team/api/pull/7',state:'open',draft:false,base:'main',headSha:api.head,mergeableState:'clean',reviewDecision:'',checksState:'success',author:'fixture-user',checks:[]};
 await openWorkspaceChanges(page);
 const pr=page.getByRole('region',{name:'Workspace changes'}).getByRole('region',{name:'Pull request #7'});
 await pr.getByRole('button',{name:'Review the PR with Codex'}).click();
 await expect.poll(()=>starts().length).toBe(6);
 assert.deepEqual((({scope,member})=>({scope,member}))(starts()[5]),{scope:'pr',member:'api'});
 assert.ok(fixture.sessions.some(s=>s.title==='Review · PR #7 · Codex'));
 // Opening this review adds one tab beside the work. Older reviewers are reached through their cards.
 await page.getByRole('navigation',{name:'Pages'}).getByRole('button',{name:'Work'}).click();
 await expect(reviewTabs).toHaveCount(beforePR+1);
 await expect(page.locator('[data-panel-tab]').filter({hasText:'Harden the handler'}).locator('xpath=ancestor::*[contains(concat(" ",normalize-space(@class)," ")," dv-tab ")][1]/following-sibling::*[1]')).toContainText('Review · Codex');

 // 11. Settings: the default reviewer, automatic reviews, the brief, and its reset.
 await page.keyboard.press(`${mod}+,`);
 await page.getByRole('tab',{name:'Agents & workspaces'}).click();
 const reviewerSetting=page.getByRole('combobox',{name:'Default reviewer'});
 await expect(reviewerSetting).toHaveValue('other');
 await expect(reviewerSetting.locator('option[value=other]')).toHaveText('The other agent (recommended)');
 await reviewerSetting.selectOption('fixed:codex-saved');
 await page.getByRole('textbox',{name:'Reviewer model'}).fill('gpt-5.2-codex');
 const auto=page.getByRole('switch',{name:'Review every turn that changes files'});
 await expect(auto).not.toBeChecked();await auto.check();
 const brief=page.getByRole('textbox',{name:'Review brief'});
 await expect(brief).toHaveValue(/You are an independent reviewer/);
 await brief.fill('Be terse. Review {{scope}}.');
 await page.getByRole('button',{name:'Save agent settings'}).click();
 await expect.poll(()=>fixture.mutations.filter(m=>m.method==='SaveRunnerSettings').at(-1)?.body.review).toMatchObject({reviewerMode:'fixed',reviewer:'codex-saved',reviewerModel:'gpt-5.2-codex',autoStart:true,template:'Be terse. Review {{scope}}.'});
 await page.getByRole('button',{name:'Reset the brief to the built-in one'}).click();
 await expect(brief).toHaveValue(/You are an independent reviewer/);
 await page.getByRole('button',{name:'Save agent settings'}).click();
 await expect.poll(()=>fixture.mutations.filter(m=>m.method==='SaveRunnerSettings').at(-1)?.body.review?.template||'').toBe('');
 await page.screenshot({path:'.cache/agent-review-settings.png'});

 assert.deepEqual(errors,[]);
 await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=${author.id}`);
 await page.getByRole('navigation',{name:'Pages'}).getByRole('button',{name:'Work',exact:true}).click();
 await page.getByRole('button',{name:'Snippets',exact:true}).click();
 const picker=page.getByRole('dialog',{name:'Snippets',exact:true});
 await picker.getByRole('searchbox').fill('review');
 await picker.getByRole('button',{name:'Start review'}).click();
 await expect(picker).toHaveCount(0);
 assert.equal(starts().at(-1).focus,'Review the current change carefully.');
 assert.equal(starts().at(-1).templateId,'code-review-v1');
 console.log('Agent review: one click from Changes, a turn, the PR card, the file list, /review and ⌘K; findings triaged and sent as filed; settings.');
}finally{await browser?.close();await server.close();await fixture.close();}
