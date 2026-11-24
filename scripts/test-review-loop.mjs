import {openWorkspaceChanges} from './fixtures/workspace-changes.mjs';
// The review loop against the fake Runner (GITHUB.md, "The review loop"):
// round 1 finds a blocker, the reviewer asks the author a question and gets
// the reply (both conversations show it), you send the blocker, the author's
// fix starts round 2, the reviewer resolves it and the loop ends clean, with
// its timeline. Then the escalation path: the same blocker still open after
// two fixes stops the loop and asks you, in the tray and in the author's
// conversation. Then your controls: pause, resume, mode, take over; and the
// loop's settings. The fixture plays both agents.
import {createServer} from 'vite';
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
api.files=[{path:'src/handler.go',indexStatus:' ',worktreeStatus:'M'}];
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
const blockerAt={path:'src/handler.go',startLine:4,snippet:['\tlog.Print("handle")'],severity:'blocker'};
const server=await createServer({...testViteCache(import.meta.url),configFile:false,logLevel:'error',root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
let browser;
try{
 await server.listen();await mkdir('.cache',{recursive:true});
 browser=await chromium.launch({headless:true});
 const page=await browser.newPage({viewport:{width:1500,height:1000}});page.setDefaultTimeout(10000);
 const errors=[];await collectPageErrors(page, errors);
 await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=${author.id}#token=${fixture.token}`);
 await expect(page.locator('.messages:visible')).toContainText('REPLY_ONE');
 await openWorkspaceChanges(page);
 const changes=page.getByRole('region',{name:'Workspace changes'});
 const tray=changes.getByRole('complementary',{name:'Review'});
 const chat=page.locator('section.panel-chat:visible');
 const start=async(label='Review with Codex')=>{const before=fixture.reviewOf('alpha').runs.length;await changes.getByRole('group',{name:'Review with another agent'}).getByRole('button',{name:label}).click();await expect.poll(()=>fixture.reviewOf('alpha').runs.length).toBe(before+1);return fixture.reviewOf('alpha').runs.at(-1);};
 const reviewRow=run=>tray.locator(`li[data-review="${run.id}"]`);
 // Filed findings are drafted in the author's message box ("Fix N findings in
 // <conversation>"); they count as sent when the person sends that message.
 const sendAccepted=async n=>{
  await tray.getByRole('button',{name:new RegExp(`^Fix ${n} finding${n===1?'':'s'} in `)}).click();
  const box=chat.getByRole('textbox',{name:'Message',exact:true});
  await expect(box).not.toHaveValue('');
  const draft=await box.inputValue();
  await box.press('Enter');await expect(box).toHaveValue('');
  return draft;
 };

 // ---- Two rounds, ending clean ------------------------------------------
 const run=await start();
 const row=reviewRow(run);
 await expect(row).toContainText('Round 1 of 3 · reviewing');
 // Round 1: a blocker; the reviewer asks the author, and gets the reply.
 const blocker=fixture.fileFinding(run.id,{...blockerAt,title:'The error is logged, not returned',body:'Return it so callers can retry.'});
 fixture.exchange(run.id,{from:'reviewer',question:'Should Handle ever log?',answer:'No: return the error.'});
 fixture.finishReview(run.id);
 await expect(row).toContainText('Round 1 of 3 · waiting for you to send findings',{timeout:15000});
 await expect(row.getByRole('group',{name:'Codex review loop'})).toBeVisible();
 await tray.locator('.review-tray-list').getByRole('listitem').filter({hasText:'The error is logged, not returned'}).getByRole('button',{name:'Accept'}).click();
 await tray.getByRole('button',{name:/^Pending/}).click();
 assert.match(await sendAccepted(1),/To ask about one, run `lazybox ask reviewer/);
 // The reviewer's conversation is a tab beside the work, on its page; the author's shows the exchange too.
 await expect(chat.getByRole('note').filter({hasText:'Codex asked Claude:'})).toContainText('Should Handle ever log?');
 await expect(chat.getByRole('note').filter({hasText:'Claude answered Codex:'})).toContainText('No: return the error.');
 await expect(row).toContainText('Round 1 of 3 · Claude is fixing',{timeout:15000});
 // The author's turn ends with changes: round 2 re-reviews only that turn.
 fixture.authorFixed(run.id,2);
 await expect(row).toContainText('Round 2 of 3 · re-reviewing the fix',{timeout:15000});
 // The reviewer resolves the blocker; the loop ends clean.
 fixture.resolveFinding(blocker.id,'returns the error now');
 fixture.finishReview(run.id);
 await expect(row).toContainText('Clean ✓ after 2 rounds',{timeout:15000});
 await expect(row.getByRole('group',{name:'Codex review loop'})).toHaveCount(0);
 await row.locator('details.review-timeline>summary').click();
 const timeline=row.locator('details.review-timeline ol');
 for(const line of ['Codex asked Claude: Should Handle ever log?','Claude answered Codex: No: return the error.','Sent 1 finding to','Round 2: Claude changed 1 file in turn 2','Resolved “The error is logged, not returned”: returns the error now','Clean: no blocker open after 2 rounds.'])await expect(timeline).toContainText(line);
 await page.screenshot({path:'.cache/review-loop-clean.png'});

 // ---- Escalation: the same blocker still open after two fixes ------------
 const stuck=await start();
 const stuckRow=reviewRow(stuck);
 const again=fixture.fileFinding(stuck.id,{...blockerAt,title:'Errors are swallowed',key:'swallowed'});
 fixture.finishReview(stuck.id);
 await expect(stuckRow).toContainText('waiting for you to send findings',{timeout:15000});
 await tray.getByRole('button',{name:/^To triage/}).click();
 await tray.locator('.review-tray-list').getByRole('listitem').filter({hasText:'Errors are swallowed'}).getByRole('button',{name:'Accept'}).click();
 await tray.getByRole('button',{name:/^Pending/}).click();
 await sendAccepted(1);
 await expect(stuckRow).toContainText('Claude is fixing',{timeout:15000});
 fixture.authorFixed(stuck.id,3);
 fixture.reopenFinding(again.id,'still swallowed in the retry path');
 fixture.finishReview(stuck.id);
 // Reopened: pending again, to send with the next round.
 await expect(stuckRow).toContainText('Round 2 of 3 · waiting for you to send findings',{timeout:15000});
 await expect(tray.locator('.review-tray-list').getByRole('listitem').filter({hasText:'Errors are swallowed'})).toBeVisible();
 await sendAccepted(1);
 await expect(stuckRow).toContainText('Round 2 of 3 · Claude is fixing',{timeout:15000});
 fixture.authorFixed(stuck.id,4);
 fixture.finishReview(stuck.id); // the reviewer says nothing about it: still open
 await expect(stuckRow).toContainText('Stuck: the same blocker is still open after two fixes · needs you',{timeout:15000});
 await expect(stuckRow).toHaveClass(/needs-you/);
 // The author's conversation says so too, with the way to the review.
 await page.locator('.messages:visible').first().waitFor();
 await chat.getByRole('textbox',{name:'Message',exact:true}).fill('');
 await expect(page.locator('section.panel-chat:visible .review-alert')).toContainText('Review needs you. The blocker “Errors are swallowed”',{timeout:15000});
 await page.screenshot({path:'.cache/review-loop-stuck.png'});

 // ---- Your controls: pause, resume, mode, take over -----------------------
 const third=await start('Review with Codex');
 const thirdRow=reviewRow(third);
 const controls=thirdRow.getByRole('group',{name:'Codex review loop'});
 fixture.fileFinding(third.id,{...blockerAt,title:'Missing test',key:'test'});
 fixture.finishReview(third.id);
 await expect(thirdRow).toContainText('waiting for you to send findings',{timeout:15000});
 await controls.getByRole('button',{name:'Pause'}).click();
 await expect(thirdRow).toContainText('· paused');
 assert.equal(fixture.mutations.filter(m=>m.method==='UpdateReviewRun').at(-1).body.action,'pause');
 await controls.getByRole('button',{name:'Resume'}).click();
 await expect(thirdRow).not.toContainText('· paused');
 await controls.getByRole('combobox',{name:'Between rounds'}).selectOption('auto');
 await expect.poll(()=>fixture.mutations.filter(m=>m.method==='UpdateReviewRun').at(-1)?.body).toMatchObject({action:'loop',loop:'auto'});
 await controls.getByRole('button',{name:'Take over'}).click();
 await expect(thirdRow).toContainText('You took over');
 await expect(chat).toContainText('REPLY_ONE');

 // ---- A narrow Changes panel: the loop controls are always in reach -------
 // The chat docked beside the panel (1500×1000), a small window (1024×680) and
 // a narrower docked panel: the left column shows the files or the tray (tabs),
 // the reviews under way stay at the top of the tray however far its findings
 // scroll, and the diff keeps its whole height, uncovered.
 const fourth=await start();
 const fourthRow=reviewRow(fourth);
 fixture.fileFinding(fourth.id,{...blockerAt,title:'Handle has no test for the error path',key:'error-path'});
 fixture.finishReview(fourth.id);
 await expect(fourthRow).toContainText('waiting for you to send findings',{timeout:15000});
 const fourthControls=fourthRow.getByRole('group',{name:'Codex review loop'});
 const hit=locator=>locator.evaluate(node=>{const r=node.getBoundingClientRect(),at=document.elementFromPoint(r.x+r.width/2,r.y+r.height/2);return r.width>0&&r.height>0&&!!at&&(at===node||node.contains(at));});
 const tabs=changes.getByRole('tablist',{name:'Left column'});
 for(const [width,height,name] of [[1500,1000,'docked'],[1024,680,'1024'],[1180,760,'docked-small']]){
  await page.setViewportSize({width,height});
  await expect(tabs).toBeVisible();
  await expect(tabs.getByRole('tab',{name:/^Review/})).toHaveAttribute('aria-selected','true');
  const panelWidth=await changes.evaluate(node=>node.getBoundingClientRect().width);
  assert.ok(panelWidth<=1100,`${name}: the Changes panel is narrow (${Math.round(panelWidth)}px)`);
  // The diff: its whole height (≥300px), never under the tray.
  const [diffBox,trayBox]=[await changes.locator('.diff-view').boundingBox(),await tray.boundingBox()];
  assert.ok(diffBox.height>=300,`${name}: the diff keeps ≥300px (${Math.round(diffBox.height)}px)`);
  assert.ok(trayBox.x+trayBox.width<=diffBox.x+1,`${name}: the tray covers no part of the diff`);
  // Every loop control and the Send button answer a click where they are, even with the findings scrolled to their end.
  await tray.locator('.review-tray-scroll').evaluate(node=>{node.scrollTop=node.scrollHeight;});
  for(const control of [fourthControls.getByRole('combobox',{name:'Between rounds'}),fourthControls.getByRole('button',{name:'Pause'}),fourthControls.getByRole('button',{name:'Stop loop'}),fourthControls.getByRole('button',{name:'Take over'}),fourthRow.getByRole('status'),tray.getByRole('button',{name:/^(Send .* to the agent|Fix \d+ findings? in )/}),tray.getByRole('button',{name:'Close review'})]) {
   await control.scrollIntoViewIfNeeded();
   assert.ok(await hit(control),`${name}: ${await control.evaluate(node=>node.getAttribute('aria-label')||node.textContent)} is in reach`);
  }
  const triage=tray.getByRole('button',{name:/^To triage/});
  assert.ok(await hit(triage),`${name}: To triage is in reach with the findings scrolled`);
  await triage.click();
  const finding=tray.locator('.review-tray-list').getByRole('listitem').filter({hasText:'Handle has no test for the error path'});
  for(const action of ['Accept','Dismiss','Ask Codex']){const button=finding.getByRole('button',{name:action});await button.scrollIntoViewIfNeeded();assert.ok(await hit(button),`${name}: ${action} is in reach, scrolled to`);}
  for(const scheme of ['dark','light']){await page.emulateMedia({colorScheme:scheme});await page.mouse.move(0,0);await page.waitForTimeout(400);await page.screenshot({path:`.cache/review-tray-narrow-${name}-${scheme}.png`});}
  await page.emulateMedia({colorScheme:null});
  // Files and Review share the column: one click (or ←/→) between them, each where it was.
  await tabs.getByRole('tab',{name:/^Files/}).click();
  await expect(changes.getByRole('navigation',{name:'Changed files'})).toBeVisible();
  await expect(tray).toBeHidden();
  await page.keyboard.press('ArrowRight');
  await expect(tray).toBeVisible();
  await expect(tabs.getByRole('tab',{name:/^Review/})).toBeFocused();
 }
 // A wide panel shows the files, the diff and the tray side by side: no tabs.
 await page.setViewportSize({width:2400,height:1000});
 await expect(tabs).toBeHidden();
 await expect(changes.getByRole('navigation',{name:'Changed files'})).toBeVisible();
 await expect(tray).toBeVisible();
 assert.ok(await hit(fourthControls.getByRole('button',{name:'Take over'})),'wide: the loop controls are in reach');
 await page.mouse.move(0,0);await page.screenshot({path:'.cache/review-tray-wide.png'});
 await page.setViewportSize({width:1500,height:1000});
 // The loop controls work there.
 await fourthControls.getByRole('button',{name:'Pause'}).click();
 await expect(fourthRow).toContainText('· paused');
 await fourthControls.getByRole('button',{name:'Stop loop'}).click();
 await expect(fourthControls).toHaveCount(0);
 // With the files in front, the header's Review button brings the tray back rather than closing it.
 await tabs.getByRole('tab',{name:/^Files/}).click();
 await changes.getByRole('button',{name:/^Review comments/}).click();
 await expect(tray).toBeVisible();
 await page.setViewportSize({width:1500,height:1000});

 // ---- Settings: between rounds, and the round limit -----------------------
 await page.keyboard.press('Meta+,');
 await page.getByRole('tab',{name:'Agents & workspaces'}).click();
 await page.getByRole('combobox',{name:'Between rounds'}).selectOption('auto');
 await page.getByRole('spinbutton',{name:'Rounds before it asks you'}).fill('5');
 await page.getByRole('button',{name:'Save agent settings'}).click();
 await expect.poll(()=>fixture.mutations.filter(m=>m.method==='SaveRunnerSettings').at(-1)?.body.review).toMatchObject({loop:'auto',maxRounds:5});

 assert.deepEqual(errors,[]);
 console.log('Review loop: two rounds ending clean with a question answered between the agents; a stuck blocker escalated to you; pause, resume, mode and take over; settings.');
}finally{await browser?.close();await server.close();await fixture.close();}
