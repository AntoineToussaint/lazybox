import {openWorkspaceChanges} from './fixtures/workspace-changes.mjs';
// The PR loop on the Changes page against the fake Runner: commit (with an
// agent-drafted message), push, open a pull request, read its checks, send the
// failing ones to the conversation's message box, and merge when green.
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from './fixtures/vite-cache.mjs';
import react from '@vitejs/plugin-react';
import {chromium,expect} from './fixtures/playwright.mjs';
import {fileURLToPath} from 'node:url';
import {mkdir} from 'node:fs/promises';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import { collectPageErrors } from './fixtures/page-errors.mjs';

const fixture=await startRunnerFixture();
fixture.sessions[0].title='Ship API contract';
const channel=fixture.openEvents('alpha-session');
const api=fixture.shipState('alpha','api');
Object.assign(api,{githubRepository:'fixture-team/api',files:[{path:'contract.ts',indexStatus:'M',worktreeStatus:'M'},{path:'new-view.tsx',indexStatus:'?',worktreeStatus:'?'}]});
fixture.shipState('alpha','web'); // not on GitHub, nothing to ship: stays quiet
const server=await createServer({...testViteCache(import.meta.url),configFile:false,root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
const writes=()=>fixture.mutations.filter(m=>['CommitWorkspaceChanges','PushWorkspaceBranch','CreatePullRequest','MergePullRequest','Apply'].includes(m.method));
let browser;
try{
 await server.listen();browser=await chromium.launch({headless:true});
 const page=await browser.newPage({viewport:{width:1500,height:1000}});page.setDefaultTimeout(10000);
 const errors=[];await collectPageErrors(page, errors);
 await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace#token=${fixture.token}`);
 await page.getByRole('combobox',{name:'Workspace',exact:true}).selectOption('alpha');await page.getByRole('combobox',{name:'Session',exact:true}).selectOption('alpha-session');
 await openWorkspaceChanges(page);
 const changes=page.getByRole('region',{name:'Workspace changes'});
 const bar=changes.getByRole('toolbar',{name:'Ship api'});
 await expect(changes.getByRole('toolbar',{name:'Ship web'})).toHaveCount(0);

 // 1. Commit one of two files, with the message the agent drafts.
 const commit=bar.getByRole('button',{name:'Commit 2…'});await expect(commit).toHaveClass(/primary/);await commit.click();
 let dialog=page.getByRole('dialog',{name:'Commit changes in api'});
 const message=dialog.getByRole('textbox',{name:'Message'});await expect(message).toHaveValue('Ship API contract');await expect(message).toBeFocused();
 await dialog.getByRole('button',{name:'Ask the agent to write it'}).click();await expect(dialog.getByRole('status')).toContainText('Waiting for the agent');
 await expect.poll(()=>fixture.mutations.find(m=>m.method==='Apply')?.body.message?.text||'').toContain('lazybox-commit');
 const request=fixture.mutations.find(m=>m.method==='Apply').body;assert.equal(request.sessionId,'alpha-session');assert.match(request.message.text,/contract\.ts/);
 channel.push({type:'operation_accepted',text:request.message.text});
 channel.push({type:'message_delta',text:'Here it is:\n```lazybox-commit\nAdd the API contract\n\nKeeps the web client and the API in step.\n```'});
 await expect(message).toHaveValue('Add the API contract\n\nKeeps the web client and the API in step.');await expect(dialog).toContainText('Written by the agent');
 await dialog.getByRole('checkbox',{name:/new-view\.tsx/}).uncheck();await expect(dialog).toContainText('1 of 2 changed files');await mkdir('.cache',{recursive:true});await page.screenshot({path:'.cache/pull-request-commit.png'});
 await message.press('Meta+Enter');await expect(dialog).toHaveCount(0);
 const committed=fixture.mutations.find(m=>m.method==='CommitWorkspaceChanges').body;assert.deepEqual(committed.paths,['contract.ts']);assert.match(committed.message,/^Add the API contract/);
 await expect(changes.getByRole('status').filter({hasText:'Committed 1 file'})).toBeVisible();

 // 2. Commit the rest and push in one step; the branch is published.
 await expect(bar.getByRole('button',{name:'Publish branch'})).toBeVisible();
 await bar.getByRole('button',{name:'Commit 1…'}).click();dialog=page.getByRole('dialog',{name:'Commit changes in api'});
 await expect(dialog.getByRole('textbox',{name:'Message'})).toHaveValue('Ship API contract');await dialog.getByRole('textbox',{name:'Message'}).fill('Add the new view');
 // The commit lands, the push fails: the dialog says so and offers only the push again (committing again would fail).
 let pushFails=true;
 await page.route('**/PushWorkspaceBranch',async route=>{if(pushFails){pushFails=false;await route.fulfill({status:503,json:{code:'unavailable',message:'the remote hung up'}});}else await route.continue();});
 await dialog.getByRole('button',{name:'Commit & push'}).click();
 await expect(dialog.getByRole('status')).toHaveText(/^Committed 1 file · [0-9a-f]{8} Add the new view\. Not pushed yet\.$/);
 await expect(dialog.getByRole('alert')).toContainText('The push failed: the remote hung up');
 await expect(dialog.getByRole('button',{name:'Commit & push'})).toHaveCount(0);
 await dialog.getByRole('button',{name:'Push again'}).click();await expect(dialog).toHaveCount(0);
 await page.unroute('**/PushWorkspaceBranch');
 assert.deepEqual(writes().map(m=>m.method),['Apply','CommitWorkspaceChanges','CommitWorkspaceChanges','PushWorkspaceBranch']);
 assert.equal(fixture.mutations.filter(m=>m.method==='CommitWorkspaceChanges')[1].body.paths?.length??0,0,'all remaining files commit without a path list');

 // 3. Open the pull request: title and body come from the branch's commits.
 const create=bar.getByRole('button',{name:'Create PR…'});await expect(create).toHaveClass(/primary/);await create.click();
 dialog=page.getByRole('dialog',{name:'Create pull request for api'});
 await expect(dialog.getByRole('textbox',{name:'Title'})).toHaveValue('Ship API contract');
 await expect(dialog.getByRole('textbox',{name:'Description'})).toHaveValue('## Changes\n\n- Add the API contract\n- Add the new view\n');
 await expect(dialog.getByRole('combobox',{name:'Base branch'})).toHaveValue('main');await expect(dialog).toContainText('fixture-team/api');await page.screenshot({path:'.cache/pull-request-create.png'});
 await dialog.getByRole('button',{name:'Create pull request'}).click();await expect(dialog).toHaveCount(0);
 const created=fixture.mutations.find(m=>m.method==='CreatePullRequest').body;assert.equal(created.title,'Ship API contract');assert.equal(created.base??'','');assert.equal(created.draft??false,false);

 // 4. The card leads with the failing check and its log link; merge waits.
 const card=changes.getByRole('region',{name:'Pull request #7'});
 await expect(card.getByRole('link',{name:/#7 Ship API contract/})).toHaveAttribute('href','https://github.com/fixture-team/api/pull/7');
 await expect(card).toContainText('Checks · 1 failing · 1 running · 1 passed');
 await expect(card.getByRole('link',{name:'Open test on GitHub'})).toHaveAttribute('href','https://github.com/fixture-team/api/actions/runs/41/job/1');
 await expect(card.getByRole('button',{name:'Merge',exact:true})).toBeDisabled();await expect(card).toContainText('Checks are failing');
 await mkdir('.cache',{recursive:true});await page.screenshot({path:'.cache/pull-request-checks.png'});

 // 5. Failing checks go to the conversation's message box beside the changes; nothing is sent.
 const applies=fixture.mutations.filter(m=>m.method==='Apply').length;
 await card.getByRole('button',{name:'Send failing check to the agent'}).click();
 const composer=page.getByRole('region',{name:'Agent chat'}).getByRole('textbox',{name:'Message',exact:true});
 await expect(composer).toHaveValue(/CI is failing on pull request #7 \(fixture-team\/api, branch lazybox\/alpha\/api/);
 await expect(composer).toHaveValue(/- test \(failure\): https:\/\/github\.com\/fixture-team\/api\/actions\/runs\/41\/job\/1 — 2 tests failed/);
 await expect(composer).toBeFocused();await expect(changes).toContainText('Review it and send');
 assert.equal(fixture.mutations.filter(m=>m.method==='Apply').length,applies,'failing checks must not be sent on the person\'s behalf');

 // 6. Checks go green; a branch-protection refusal is shown as GitHub states it; then merge.
 Object.assign(api.pr,{checksState:'success',mergeableState:'clean',checks:api.pr.checks.map(c=>({...c,status:'completed',conclusion:'success'}))});
 api.mergeRefusal='At least 1 approving review is required by reviewers with write access.';
 await card.getByRole('button',{name:'Refresh pull request status'}).click();await expect(card).toContainText('Checks · 3 passed');
 const merge=card.getByRole('button',{name:'Merge',exact:true});await expect(merge).toBeEnabled();await expect(merge).toHaveClass(/primary/);
 await card.getByRole('combobox',{name:'Merge method'}).selectOption('rebase');await merge.click();
 const confirm=card.getByRole('group',{name:'Confirm merge'});await expect(confirm).toContainText('Rebase and merge #7 into main?');
 await expect(confirm.getByRole('button',{name:'Confirm merge'})).toBeFocused();await page.keyboard.press('Enter');
 await expect(card.getByRole('alert')).toContainText('At least 1 approving review is required');
 api.mergeRefusal='';await confirm.getByRole('button',{name:'Confirm merge'}).click();
 await expect(card).toContainText('Merged');await expect(changes.getByRole('status').filter({hasText:'Merged #7 into main'})).toBeVisible();
 const merged=fixture.mutations.filter(m=>m.method==='MergePullRequest').at(-1).body;assert.equal(merged.method,'rebase');assert.equal(merged.expectedHead,api.head);assert.equal(String(merged.number),'7');
 await page.screenshot({path:'.cache/pull-request-merged.png'});
 assert.deepEqual(errors,[]);
 console.log('PASS: commit with an agent-drafted message, commit & push, PR from commits, checks with log links, failing checks to the composer unsent, protected merge explained, merge.');
}finally{await browser?.close();await server.close();await fixture.close();}
