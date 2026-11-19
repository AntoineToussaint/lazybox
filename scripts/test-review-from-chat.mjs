// Review from the chat against the fake Runner (GITHUB.md, "Agent review"):
// the conversation header's Review button, `/review <focus>` and ⌘K "Review
// this workspace's changes" each start the review Changes → Review starts,
// with the default reviewer: the other agent (Codex reviews Claude's work),
// or, when it is not installed, a fresh conversation of the author's agent,
// said quietly. The review opens as a background tab beside the chat — the
// person stays in their composer — titled "Review · Codex", with a badge that
// ends as its findings count; the sidebar's row brings that tab to the front.
import {createServer} from 'vite';
import react from '@vitejs/plugin-react';
import {chromium,expect} from './fixtures/playwright.mjs';
import {fileURLToPath} from 'node:url';
import {mkdir} from 'node:fs/promises';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import {testViteCache} from './fixtures/vite-cache.mjs';

const fixture=await startRunnerFixture();
const author=fixture.sessions[0];
Object.assign(author,{title:'Harden the handler',provider:'claude',status:'ready',busy:false,agents:[{id:'root',name:'Main agent',status:'done'}]});
const events=fixture.openEvents(author.id);
events.push({type:'operation_accepted',operationId:'op-1',text:'Return errors from Handle instead of logging them'});
events.push({type:'message_delta',text:'REPLY_ONE: Handle returns its error now.'});events.push({type:'turn_ended'});
const starts=()=>fixture.mutations.filter(m=>m.method==='StartReview').map(m=>m.body);
const pick=({provider='',profileId='',focus='',scope,compare=''})=>({provider,profileId,focus,scope,compare});
const server=await createServer({...testViteCache(import.meta.url),configFile:false,logLevel:'error',root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
let browser;
try{
 await server.listen();await mkdir('.cache',{recursive:true});
 browser=await chromium.launch({headless:true});
 const url=`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=${author.id}#token=${fixture.token}`;
 const open=async()=>{const page=await browser.newPage({viewport:{width:1500,height:1000}});page.setDefaultTimeout(10000);page.on('pageerror',e=>errors.push(String(e)));await page.goto(url);await expect(page.locator('.messages:visible')).toContainText('REPLY_ONE');return page;};
 const errors=[];
 const mod='Meta';
 let page=await open();
 const chat=()=>page.locator('section.panel-chat:visible');
 const tabs=()=>page.locator('[data-panel-tab]');
 const reviewTabs=()=>tabs().filter({hasText:/^Review · /});
 const activeTab=()=>page.locator('.dv-tab.dv-active-tab [data-panel-tab]');
 const box=()=>chat().getByRole('textbox',{name:'Message',exact:true});

 // 1. The header's Review button names who reviews and its key: Codex, the other agent than Claude.
 const header=chat().locator('.chat-heading');
 const review=header.getByRole('group',{name:"Review this conversation's work"});
 const button=review.getByRole('button',{name:'Review with Codex'});
 await expect(button).toBeVisible();
 await expect(button).toHaveAttribute('title',/^Review with Codex \(\] \] (⇧|Shift\+)r\)$/);
 await page.screenshot({path:'.cache/review-from-chat-header.png'});
 // One click: the same review Changes starts (the workspace's changes), the default reviewer left to the Runner.
 await button.click();
 await expect.poll(()=>starts().length).toBe(1);
 assert.deepEqual(pick(starts()[0]),{provider:'',profileId:'',focus:'',scope:'changes',compare:''});
 assert.equal(starts()[0].sourceSessionId,author.id);
 const first=fixture.sessions.find(s=>s.title==='Review · Changes · Codex');
 assert.equal(first.provider,'codex','the other agent reviews Claude’s work');
 // A background tab beside the chat, in its group, right after it; the chat stays in front, on its conversation.
 await expect(reviewTabs()).toHaveCount(1);
 await expect(reviewTabs().first()).toContainText('Review · Codex');
 const group=page.locator('.dv-groupview').filter({has:tabs().filter({hasText:'Harden the handler'})});
 await expect(group.locator('[data-panel-tab]')).toHaveText([/Harden the handler/,/Review · Codex/]);
 await expect(activeTab().filter({hasText:'Harden the handler'})).toHaveCount(1);
 await expect(chat().locator('.messages:visible')).toContainText('REPLY_ONE');
 // Progress inline: the review's card in the conversation, a live badge on its tab, and the button says so.
 await expect(chat().locator('.messages:visible').getByRole('group',{name:'Codex review'})).toContainText('Codex is reviewing changes');
 await expect(reviewTabs().first().locator('.dock-tab-badge')).toHaveAttribute('data-tone','busy');
 await expect(review.getByRole('button',{name:'Reviewing'})).toBeVisible();
 await page.screenshot({path:'.cache/review-from-chat-tab.png'});

 // 2. Codex files three findings and ends its turn: the tab says "3 findings"; they are in the tray as before.
 const run=fixture.reviewOf('alpha').runs.at(-1);
 for(const [line,severity,title] of [[3,'major','Wrap the error'],[4,'minor','Use the structured logger'],[5,'nit','Name it handleErr']])fixture.fileFinding(run.id,{path:'src/handler.go',startLine:line,snippet:['x'],severity,title});
 fixture.finishReview(run.id);
 await expect(reviewTabs().first().locator('.dock-tab-badge')).toHaveText('3 findings',{timeout:15000});
 await expect(reviewTabs().first().locator('.dock-tab-badge')).toHaveAttribute('data-tone','warn');
 assert.equal(fixture.reviewOf('alpha').comments.filter(c=>c.reviewId===run.id).length,3);
 await page.screenshot({path:'.cache/review-from-chat-findings.png'});

 // 3. The sidebar's row of the review brings its tab to the front: no second panel, the work's tab kept.
 const chatTabs=await tabs().filter({has:page.locator('[data-kind=chat]')}).count();
 const nav=page.getByRole('navigation',{name:'Workspaces'});
 await nav.getByText('Review · Changes · Codex',{exact:true}).first().click();
 await expect(activeTab().filter({hasText:'Review · Codex'})).toHaveCount(1);
 await expect(tabs()).toHaveCount(await tabs().count());
 assert.equal(await tabs().filter({has:page.locator('[data-kind=chat]')}).count(),chatTabs,'no duplicate panel for the conversation');
 await expect(chat().locator('.system-event[data-kind=review_brief] .system-event-text')).toHaveText("Asked Codex to review Claude's work");
 // And the work's row brings the work back.
 await nav.getByText('Harden the handler',{exact:true}).first().click();
 await expect(activeTab().filter({hasText:'Harden the handler'})).toHaveCount(1);
 await expect(reviewTabs()).toHaveCount(1);

 // 4. `/review <focus>`: the default reviewer, with what to look at; the person stays in the composer.
 await box().click();await page.keyboard.type('/review the error path of Handle');
 await page.keyboard.press('Enter');
 await expect.poll(()=>starts().length).toBe(2);
 assert.deepEqual(pick(starts()[1]),{provider:'',profileId:'',focus:'the error path of Handle',scope:'changes',compare:''});
 assert.equal(fixture.mutations.filter(m=>m.method==='Apply'&&/\/review/.test(m.body?.message?.text||'')).length,0,'/review runs in Lazybox; nothing is sent to the agent');
 await expect(reviewTabs()).toHaveCount(2);
 await expect(activeTab().filter({hasText:'Harden the handler'})).toHaveCount(1);
 await expect.poll(()=>page.evaluate(()=>document.activeElement?.getAttribute('aria-label'))).toBe('Message');
 await expect(box()).toHaveValue('');
 // The focus reaches the brief and the review's card.
 const focused=fixture.reviewOf('alpha').runs.at(-1);
 assert.equal(focused.focus,'the error path of Handle');
 await expect(chat().locator('.messages:visible .review-card').last().locator('.review-card-note')).toHaveText('Focus: the error path of Handle');

 // 5. ⌘K "Review this workspace's changes".
 await page.keyboard.press(`${mod}+k`);
 const palette=page.getByRole('dialog',{name:'Command palette'});
 await palette.getByRole('combobox',{name:'Search commands'}).fill('review this workspace');
 await expect(palette.getByRole('option').first()).toContainText('Review this workspace’s changes');
 await page.keyboard.press('Enter');
 await expect.poll(()=>starts().length).toBe(3);
 assert.deepEqual(pick(starts()[2]),{provider:'',profileId:'',focus:'',scope:'changes',compare:''});
 await expect(reviewTabs()).toHaveCount(3);
 await expect(activeTab().filter({hasText:'Harden the handler'})).toHaveCount(1);

 // 6. The chevron picks another reviewer for one run; the ⋯ menu holds Review too (it folds there in a slim panel).
 await review.getByLabel('Choose the reviewer',{exact:true}).click();
 await page.getByRole('button',{name:/^Review with Claude/}).click();
 await expect.poll(()=>starts().length).toBe(4);
 assert.equal(starts()[3].provider,'claude');
 await header.getByLabel('Session actions',{exact:true}).click();
 await expect(page.getByRole('button',{name:'Review with Codex'}).last()).toBeVisible();
 await page.keyboard.press('Escape');

 // 7. Codex is not installed: Claude reviews, in a fresh conversation, and the review says so.
 // (The reviews under way end first: while one runs, the button shows it instead.)
 for(const r of fixture.reviewOf('alpha').runs.filter(r=>r.state==='running'))fixture.finishReview(r.id);
 await page.close();
 fixture.availability.codex.installed=false;
 page=await open();
 const fallbackButton=chat().locator('.chat-heading').getByRole('button',{name:'Review with Claude'});
 await expect(fallbackButton).toHaveAttribute('title',/^Review with Claude .*\nCodex is not available, so a fresh Claude conversation reviews\.$/);
 await fallbackButton.click();
 await expect.poll(()=>starts().length).toBe(5);
 assert.deepEqual(pick(starts()[4]),{provider:'',profileId:'',focus:'',scope:'changes',compare:''});
 const fallback=fixture.reviewOf('alpha').runs.at(-1);
 assert.equal(fallback.reviewerProvider,'claude');
 assert.notEqual(fallback.reviewerSessionId,author.id,'a fresh conversation reviews');
 await expect(chat().locator('.messages:visible .review-card').last().locator('.review-card-note')).toHaveText('Codex is not available, so a fresh Claude conversation reviews.');
 await expect(tabs().filter({hasText:'Review · Claude'}).first()).toBeVisible();
 await page.screenshot({path:'.cache/review-from-chat-fallback.png'});
 fixture.availability.codex.installed=true;

 // 8. Cancel review (GITHUB.md, "Cancelling a review"): from the card, the keyboard, the reviewer's ⋯ menu and ⌘K.
 const cancels=()=>fixture.mutations.filter(m=>m.method==='CancelReview').map(m=>m.body);
 const lastCard=()=>chat().locator('.messages:visible .review-card').last();
 const newest=()=>fixture.reviewOf('alpha').runs.at(-1);
 // 8a. The card, with nothing filed yet: no question, it stops at once and says what it kept.
 await expect(lastCard()).toContainText('Claude is reviewing changes');
 await lastCard().getByRole('button',{name:'Cancel review'}).click();
 await expect.poll(()=>cancels().length).toBe(1);
 assert.equal(cancels()[0].reviewId,fallback.id);
 await expect(page.getByRole('dialog',{name:'Cancel review'})).toHaveCount(0);
 await expect(lastCard()).toContainText('Review cancelled · 0 findings kept');
 await expect(lastCard()).toContainText('You cancelled it');
 await expect(lastCard().getByRole('button',{name:'Cancel review'})).toHaveCount(0);
 assert.equal(fixture.sessions.find(s=>s.id===fallback.reviewerSessionId).closed,true,'its session ended');
 assert.equal(newest().stoppedBy,'You');
 await page.screenshot({path:'.cache/review-cancelled-card.png'});
 // Review again: the same scope, by the same reviewer.
 await lastCard().getByRole('button',{name:'Review again'}).click();
 await expect.poll(()=>starts().length).toBe(6);
 assert.deepEqual(pick(starts()[5]),{provider:'',profileId:fallback.reviewerProfileId,focus:'',scope:'changes',compare:''});
 await expect(lastCard()).toContainText('Claude is reviewing changes');

 // 8b. The keyboard, with a finding filed: Esc on the card asks first; Esc in the question keeps it reviewing.
 const again=newest();
 fixture.fileFinding(again.id,{path:'src/handler.go',startLine:3,snippet:['x'],severity:'major',title:'Wrap the error'});
 await expect(lastCard()).toContainText('1 major so far',{timeout:15000});
 await lastCard().focus();
 await page.keyboard.press('Escape');
 const ask=page.getByRole('dialog',{name:'Cancel review'});
 await expect(ask).toBeVisible();
 await expect(ask).toContainText('1 finding filed');
 await expect(ask).toContainText('stays in the Review tray');
 await page.screenshot({path:'.cache/review-cancel-confirm.png'});
 await page.keyboard.press('Escape');
 await expect(ask).toHaveCount(0);
 assert.equal(cancels().length,1,'Esc in the question keeps the review');
 await expect(lastCard()).toContainText('Claude is reviewing changes');
 await lastCard().focus();
 await page.keyboard.press('Escape');
 await ask.getByRole('button',{name:'Cancel review'}).click();
 await expect.poll(()=>cancels().length).toBe(2);
 await expect(lastCard()).toContainText('Review cancelled · 1 finding kept');
 assert.equal(fixture.reviewOf('alpha').comments.filter(c=>c.reviewId===again.id).length,1,'the finding is kept');

 // 8c. The reviewer conversation's ⋯ menu.
 // Another review, by whichever reviewer is the default now (Codex is back).
 const another=async count=>{
  await chat().locator('.chat-heading').getByRole('button',{name:/^Review with /}).click();
  await expect.poll(()=>starts().length).toBe(count);
  const run=newest();
  await expect(lastCard()).toContainText(`${run.reviewerLabel} is reviewing changes`);
  return run;
 };
 const viaMenu=await another(7);
 // The card's Open conversation shows the reviewer (the page is full of review tabs by now).
 const openReviewer=async run=>{
  await lastCard().getByRole('button',{name:'Open conversation'}).click();
  await expect(chat().locator('.chat-heading')).toContainText(`Review · Changes · ${run.reviewerLabel}`);
  await expect(chat().locator('.messages:visible')).toContainText(`Asked ${run.reviewerLabel} to review Claude's work`);
  await chat().locator('.chat-heading').getByLabel('Session actions',{exact:true}).click();
 };
 const backToWork=async()=>{await page.getByRole('navigation',{name:'Workspaces'}).getByText('Harden the handler',{exact:true}).first().click();await expect(chat().locator('.messages:visible')).toContainText('REPLY_ONE');};
 await openReviewer(viaMenu);
 await page.getByRole('button',{name:'Cancel review'}).click();
 await expect.poll(()=>cancels().length).toBe(3);
 assert.equal(cancels()[2].reviewId,viaMenu.id);
 await backToWork();
 await expect(lastCard()).toContainText('Review cancelled · 0 findings kept');

 // 8d. ⌘K "Cancel review".
 const viaPalette=await another(8);
 await page.keyboard.press(`${mod}+k`);
 const commands=page.getByRole('dialog',{name:'Command palette'});
 await commands.getByRole('combobox',{name:'Search commands'}).fill('cancel review');
 await expect(commands.getByRole('option').first()).toContainText('Cancel review');
 await page.keyboard.press('Enter');
 await expect.poll(()=>cancels().length).toBe(4);
 assert.equal(cancels()[3].reviewId,viaPalette.id);
 await expect(lastCard()).toContainText('Review cancelled · 0 findings kept');

 // 8e. Ending the reviewer by hand (its ⋯ End session) moves the card out of "reviewing": Review stopped.
 const ended=await another(9);
 await openReviewer(ended);
 await page.getByRole('button',{name:'End session',exact:true}).click();
 await expect.poll(()=>fixture.mutations.filter(m=>m.method==='StopSessionResources'&&m.body.sessionId===ended.reviewerSessionId).length).toBe(1);
 await backToWork();
 await expect(lastCard()).toContainText('Review stopped · 0 findings kept');
 await expect(lastCard()).toContainText(`You ended ${ended.reviewerLabel}'s conversation; the review stopped.`);
 await expect(lastCard().getByRole('button',{name:'Cancel review'})).toHaveCount(0);
 await expect(lastCard().getByRole('button',{name:'Review again'})).toBeVisible();
 assert.equal(cancels().length,4,'ending it is not a cancel');
 await page.screenshot({path:'.cache/review-stopped-card.png'});

 // Code review is also available as a template in the workspace's New tab menu.
 await page.close();
 page=await open();
 await page.locator('.dv-groupview').filter({has:tabs().filter({hasText:'Harden the handler'})})
  .getByLabel(/^New tab in the .* group$/).first().click();
 const template=page.getByRole('button',{name:'Code review Workspace changes'});
 await expect(template).toBeVisible();
 await template.click();
 await expect.poll(()=>starts().length).toBe(10);
 assert.equal(starts()[9].sourceSessionId,author.id);
 assert.equal(starts()[9].templateId,'code-review-v1');
 assert.deepEqual(pick(starts()[9]),{provider:'',profileId:'',focus:'',scope:'changes',compare:''});
 await expect(lastCard()).toContainText('Codex is reviewing changes');
 assert.notEqual(newest().reviewerSessionId,author.id);

 await page.locator('.dv-groupview').filter({has:tabs().filter({hasText:'Harden the handler'})})
  .getByLabel(/^New tab in the .* group$/).first().click();
 await page.getByRole('button',{name:'Security review Workspace changes'}).click();
 await expect.poll(()=>starts().length).toBe(11);
 assert.equal(starts()[10].templateId,'security-review-v1');
 await expect(tabs().filter({hasText:'Security review · Codex'})).toBeVisible();
 const security=newest();
 const selected=fixture.fileFinding(security.id,{path:'src/handler.go',startLine:3,snippet:['x'],severity:'major',title:'Validate the caller'});
 fixture.finishReview(security.id);
 await expect(lastCard().getByRole('button',{name:'Open findings artifact'})).toBeVisible();
 await lastCard().getByRole('button',{name:'Open review',exact:true}).click();
 const tray=page.getByRole('complementary',{name:'Review',exact:true});
 await tray.getByRole('group',{name:'Triage “Validate the caller”'}).getByRole('button',{name:'Accept',exact:true}).click();
 await tray.getByRole('button',{name:'Address selected findings in a new conversation'}).click();
 await expect(box()).toHaveValue(new RegExp('findings artifact '+security.id+' v1.*'+selected.id,'s'));
 const opens=fixture.mutations.filter(m=>m.method==='OpenSession');
 assert.equal(opens.at(-1).body.options.permission,'ask');
 assert.equal(fixture.mutations.filter(m=>m.method==='Apply'&&m.body?.message?.text?.includes(selected.id)).length,0);

 await page.locator('.dv-groupview').filter({has:tabs().filter({hasText:'Harden the handler'})})
  .getByLabel(/^New tab in the .* group$/).first().click();
 await page.getByRole('button',{name:'Behavior verification Workspace changes'}).click();
 await expect.poll(()=>starts().length).toBe(12);
 assert.equal(starts()[11].templateId,'behavior-verification-v1');
 await expect(tabs().filter({hasText:'Behavior verification · Codex'})).toBeVisible();
 const verification=newest();
 const candidateRun=fixture.startRun(verification.reviewerSessionId,"Candidate acceptance check");
 candidateRun.output="ACCEPTANCE_OUTPUT\n";
 verification.verification=[{name:'Restart recovery',member:'api',expected:'Tasks survive restart',argv:['test'],regression:'Keep a restart regression test',runs:[{round:1,base:'abcdef123456',candidate:'fedcba123456',baseCommandId:'',candidateCommandId:candidateRun.id,baseExit:1,candidateExit:0,state:'passed',detail:''}]}];
 fixture.finishReview(verification.id);
 await tabs().filter({hasText:'Harden the handler'}).first().click();
 await lastCard().getByRole('button',{name:'Open review',exact:true}).click();
 await page.getByText('Acceptance checks · 1').click();
 await expect(page.getByText('Tasks survive restart',{exact:true})).toBeVisible();
 await expect(page.getByText('Round 1 · passed',{exact:true})).toBeVisible();
 await page.getByRole('button',{name:'Candidate output',exact:true}).click();
 await expect(page.getByText('ACCEPTANCE_OUTPUT',{exact:false})).toBeVisible();


 assert.deepEqual(errors,[]);
 console.log('Review from the chat: the header button, /review <focus> and ⌘K; the other agent by default, the fallback said; a background tab with its findings; the sidebar row focuses it; cancelled from the card, Esc, the reviewer’s menu and ⌘K; stopped when its reviewer is ended.');
}finally{await browser?.close();await server.close();await fixture.close();}
