// Workspace rows carry git state and the conversation's task: the branch,
// uncommitted files, commits not pushed, commits behind the default branch,
// the pull request with its checks, and the title of the conversation at work
// with its live status (never "Claude · model", which truncated to "C." at the
// default sidebar width). Rows stay one fixed height, whatever they hold.
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from './fixtures/vite-cache.mjs';
import react from '@vitejs/plugin-react';
import {webkit,chromium,expect} from './fixtures/playwright.mjs';
import {fileURLToPath} from 'node:url';
import {mkdir} from 'node:fs/promises';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import { collectPageErrors, drainPageErrors } from './fixtures/page-errors.mjs';

const fixture=await startRunnerFixture();
const [alphaSession]=fixture.sessions;
alphaSession.title='Harden the request handler against slow clients';alphaSession.model='claude-opus-5-5';alphaSession.provider='claude';
const api=fixture.shipState('alpha','api'),web=fixture.shipState('alpha','web');
web.branch=api.branch;
api.files=[{path:'src/handler.go',indexStatus:' ',worktreeStatus:'M'},{path:'src/new.go',indexStatus:'?',worktreeStatus:'?'}];
api.upstream='origin/'+api.branch;api.ahead=1;api.behindDefault=3;api.githubRepository='fixture-team/api';
api.pr={number:12,title:'Harden the handler',url:'https://github.com/fixture-team/api/pull/12',state:'open',checksState:'failure'};
// The agent has been running one command for almost three minutes.
alphaSession.busy=true;alphaSession.agents=[{id:'root',name:'Main agent',status:'running',phase:'tool',tool:'Bash',phaseSinceUnixMillis:String(Date.now()-179000)}];
// Beta's conversation has no title of its own (the Runner names it after its folder).
Object.assign(fixture.sessions[1],{title:'beta',workingDirectory:'/fixture/collections/beta',status:'ready',busy:false,agents:[]});
// A workspace with no repositories, as the person makes one for notes or reviews.
fixture.collections.push({id:'comments',name:'Comments',workId:'comments',root:'/fixture/collections/comments',state:'ready',members:[]});
const server=await createServer({...testViteCache(import.meta.url),configFile:false,root:fileURLToPath(new URL('../apps/web',import.meta.url)),logLevel:'warn',plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
await server.listen();
let browser;
try{
 browser=await (process.env.TEST_CHROMIUM?chromium:webkit).launch({headless:true});
 const page=await browser.newPage({viewport:{width:1400,height:900}});page.setDefaultTimeout(15000);
 const errors=[];await collectPageErrors(page, errors);
 await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha#token=${fixture.token}`);
 const nav=page.getByRole('navigation',{name:'Workspaces'});
 const alpha=nav.locator('.workspace-row').filter({hasText:'Alpha feature'}),beta=nav.locator('.workspace-row').filter({hasText:'Beta release'});
 const git=alpha.locator('.workspace-git');
 await expect(git).toHaveAttribute('data-git-line','');
 // One branch for both repositories, then what needs doing.
 await expect(git.locator('.workspace-git-branch')).toHaveText(api.branch);
 await expect(git.locator('.workspace-git-more')).toHaveCount(0);
 await expect(git.locator('[data-git-badge=dirty]')).toHaveText('2');
 await expect(git.locator('[data-git-badge=ahead]')).toHaveText('↑1');
 await expect(git.locator('[data-git-badge=behind-default]')).toHaveText('⇣3');
 await expect(git.locator('[data-git-pull=failure]')).toHaveText('#12');
 // Words for every number: the tooltip per repository, and the screen reader's line.
 const title=await git.getAttribute('title');
 assert.match(title,/^api · lazybox\/alpha\/api · 2 uncommitted · 1 not pushed · 3 behind main · Pull request #12 · checks failing$/m);
 assert.match(title,/^web · lazybox\/alpha\/api · not pushed$/m);
 await expect(git.locator('.sr-only')).toHaveText(`Branch ${api.branch}, 2 uncommitted files, 1 commit not pushed, 3 commits behind main, Pull request #12 · checks failing — Harden the handler`);
 await expect(nav.getByRole('button',{name:/Alpha feature.*2 uncommitted files/})).toBeVisible();

 // The agent line is the conversation's task and live status; provider and model are the tooltip.
 const agent=alpha.locator('.workspace-agent');
 await expect(agent.locator('.workspace-agent-label')).toHaveText('Harden the request handler against slow clients');
 await expect(agent).toHaveAttribute('title',/Claude · claude-opus-5-5/);
 await expect(agent).not.toContainText('Claude ·');
 await expect(agent.getByRole('img',{name:'Claude',exact:true})).toBeVisible();
 await expect(agent.locator('.workspace-agent-words')).toBeVisible();
 // No "C.": at the default sidebar width the title shows a real phrase and the status is whole.
 const label=await agent.locator('.workspace-agent-label').evaluate(n=>({width:n.getBoundingClientRect().width}));
 assert.ok(label.width>=90,`the task title has room at the default width (${label.width}px)`);
 const status=await agent.locator('.workspace-agent-words').evaluate(n=>n.getBoundingClientRect().width);
 assert.ok(status>=40,`the live status keeps some of its words (${status}px)`);
 // The badges never shrink: the branch gives way first.
 const badges=await git.locator('.workspace-git-badges').evaluate(n=>({scroll:n.scrollWidth,client:n.clientWidth,right:n.getBoundingClientRect().right,row:n.closest('.workspace-link').getBoundingClientRect().right}));
 assert.ok(badges.scroll<=badges.client+1&&badges.right<=badges.row,'badges are whole and inside the row');

 // One indicator per agent line, then the title, then the status with its elapsed time, never cut.
 await expect(agent.locator('.workspace-agent-status')).toHaveText(/^Running a command · \dm \d\ds$/);
 const oneLine=async row=>row.locator('.workspace-agent').evaluate(n=>{
  const elapsed=n.querySelector('.workspace-agent-elapsed'),label=n.querySelector('.workspace-agent-label');
  return {indicators:n.querySelectorAll('.live-indicator,.session-dot,.session-mark').length-n.querySelectorAll('.session-mark .live-indicator').length,
   label:label?.textContent.trim()||'',labelWidth:label?.getBoundingClientRect().width||0,
   elapsedCut:elapsed?elapsed.scrollWidth>elapsed.clientWidth+0.5||elapsed.getBoundingClientRect().right>n.getBoundingClientRect().right+0.5:false,
   inside:n.getBoundingClientRect().right<=n.closest('.workspace-row').getBoundingClientRect().right};
 });
 const comments=nav.locator('.workspace-row').filter({hasText:'Comments'});
 for(const row of [alpha,beta,comments]){
  const line=await oneLine(row);
  assert.equal(line.indicators,1,'exactly one indicator on the agent line');
  assert.ok(line.label&&line.labelWidth>=20,`the agent line names something: "${line.label}"`);
  assert.ok(!line.elapsedCut&&line.inside,`the elapsed time is whole and inside the row: ${JSON.stringify(line)}`);
 }
 await expect(beta.locator('.workspace-agent-label')).toHaveText('New conversation');
 await expect(beta.locator('.workspace-agent-status')).toHaveText('Ready');
 // An empty workspace: "No repositories" and "Add…" in the repository line's slot.
 await expect(comments.locator('.workspace-repo-names')).toHaveText('No repositories');
 await expect(comments.getByRole('button',{name:'Add repositories to Comments'})).toBeVisible();
 await expect(comments.locator('.workspace-agent-label')).toHaveText('No conversation yet');

 // Equal heights: a row full of state, a clean one and an empty one, selected or not, are the same card.
 await expect(beta.locator('.workspace-git')).toHaveAttribute('data-git-line','');
 await expect(beta.locator('.workspace-git-badges')).toBeEmpty();
 const heights=async()=>nav.locator('.workspace-row').evaluateAll(rows=>rows.map(row=>Math.round(row.getBoundingClientRect().height)));
 // The selection is drawn inside the row: background and a left accent, nothing outside its box.
 const selection=async row=>row.evaluate(n=>{const s=getComputedStyle(n);return {shadow:s.boxShadow,outline:s.outlineStyle,overflowing:[...n.querySelectorAll('*')].some(c=>{const r=c.getBoundingClientRect(),b=n.getBoundingClientRect();return r.width&&r.height&&(r.left<b.left-0.5||r.right>b.right+0.5||r.top<b.top-0.5||r.bottom>b.bottom+0.5);})};});
 await mkdir('.cache',{recursive:true});
 let measured=await heights();
 assert.equal(new Set(measured).size,1,`rows have one height: ${measured}`);
 for(const [name,row] of [['Comments',comments],['Beta release',beta],['Alpha feature',alpha]]){
  await row.locator('.workspace-link').click();
  await expect(row.locator('.workspace-link')).toHaveAttribute('aria-current','page');
  measured=await heights();
  assert.equal(new Set(measured).size,1,`rows have one height with ${name} selected: ${measured}`);
  const drawn=await selection(row);
  assert.ok(!drawn.overflowing,`nothing of the selected ${name} row is drawn outside it`);
  assert.match(drawn.shadow,/inset/,'the accent is inside the row');
  if(name==='Comments')await nav.screenshot({path:'.cache/workspace-rows-empty-selected.png'});
 }
 // An even rhythm: the same gap between every pair of rows.
 const gaps=await nav.locator('.workspace-row').evaluateAll(rows=>rows.slice(1).map((row,i)=>Math.round(row.getBoundingClientRect().top-rows[i].getBoundingClientRect().bottom)));
 assert.equal(new Set(gaps).size,1,`rows are evenly spaced: ${gaps}`);
 await nav.screenshot({path:'.cache/workspace-rows.png'});

 // State moves: the agent pushed and CI passed. The window refreshes the rows
 // when it comes back into focus (and every 20 seconds while visible).
 api.files=[];api.ahead=0;api.pr={...api.pr,checksState:'success'};
 await page.evaluate(()=>window.dispatchEvent(new Event('focus')));
 await expect(git.locator('[data-git-pull=success]')).toHaveText('#12');
 await expect(git.locator('[data-git-badge=dirty]')).toHaveCount(0);
 await expect(git.locator('[data-git-badge=ahead]')).toHaveCount(0);
 // Different branches across repositories: the first, and how many others.
 web.branch='feature/web-only';web.pr={number:13,state:'open',draft:true};
 await page.evaluate(()=>window.dispatchEvent(new Event('focus')));
 await expect(git.locator('.workspace-git-more')).toHaveText('+1');
 await expect(git.locator('[data-git-pull=draft]')).toHaveText('#13');

 // A second conversation: both are listed under the card, one line each. Folded,
 // the row speaks for the one at work and counts the rest, at the same height as every row.
 fixture.sessions.push({...structuredClone({...alphaSession,agents:undefined}),id:'alpha-second',title:'Write the release notes',provider:'codex',profileId:'codex-default',status:'',busy:false,closed:true,createdAtUnixMillis:'1'});
 await page.reload();
 await expect(alpha.locator('xpath=..').locator('.workspace-conversations .conversation-row')).toHaveCount(2);
 await expect(beta.locator('.workspace-link > svg:first-child')).toBeVisible();
 await expect(beta.locator('.workspace-fold')).toHaveCount(0);
 await expect(alpha.locator('.workspace-link > svg:first-child')).toBeVisible();
 const expanded=alpha.locator('xpath=..').locator('.workspace-conversations');
 await expect(expanded.getByRole('img',{name:'Claude',exact:true})).toBeVisible();
 await expect(expanded.getByRole('img',{name:'Codex',exact:true})).toBeVisible();
 await expect(agent).toHaveCount(0);
 await alpha.getByRole('button',{name:/^Fold the 2 conversations/}).click();
 await expect(alpha.locator('.workspace-link > svg:first-child')).toBeVisible();
 await expect(agent.locator('.workspace-agent-label')).toHaveText('Harden the request handler against slow clients');
 await expect(agent.locator('.workspace-agent-more')).toHaveText('+1');
 await expect(agent.getByRole('img',{name:'Claude',exact:true})).toBeVisible();
 measured=await heights();
 assert.equal(new Set(measured).size,1,`rows keep one height with several conversations: ${measured}`);

 // A repository that cannot be read says so, without breaking the row.
 fixture.shipState('beta','web').summaryError='worktree missing';
 await page.evaluate(()=>window.dispatchEvent(new Event('focus')));
 await expect(beta.locator('.workspace-git.has-problem')).toHaveAttribute('title',/web · unavailable: worktree missing/);

 // The narrowest sidebar keeps every line on one row.
 const handle=page.locator('.sidebar-cell + .split-handle, .sidebar-handle').first();
 if(await handle.count()){const b=await handle.boundingBox();await page.mouse.move(b.x+b.width/2,b.y+40);await page.mouse.down();await page.mouse.move(b.x-400,b.y+40,{steps:5});await page.mouse.up();}
 measured=await heights();
 assert.equal(new Set(measured).size,1,`narrow sidebar rows have one height: ${measured}`);
await nav.screenshot({path:'.cache/workspace-rows-narrow.png'});
 const narrow=await git.locator('.workspace-git-badges').evaluate(n=>n.getBoundingClientRect().right<=n.closest('.workspace-link').getBoundingClientRect().right+0.5);
 assert.ok(narrow,'badges stay inside a narrow row'); for(const row of [alpha,beta,comments]){const line=await oneLine(row);assert.ok(!line.elapsedCut&&line.inside&&line.label,`narrow: the agent line is whole: ${JSON.stringify(line)}`);}
 await nav.screenshot({path:'.cache/workspace-rows-narrow.png'});

 // F2 renames in place; leaving the form (Esc, Cancel, or saving) puts focus back on the row, not on the page.
 const betaLink=beta.locator('.workspace-link'),field=nav.getByRole('textbox',{name:'Workspace name'});
 await betaLink.focus();await page.keyboard.press('F2');await expect(field).toBeFocused();
 await page.keyboard.press('Escape');await expect(field).toHaveCount(0);
 await expect(betaLink).toBeFocused();
 await page.keyboard.press('F2');await nav.getByRole('button',{name:'Cancel',exact:true}).click();await expect(betaLink).toBeFocused();
 await page.keyboard.press('F2');await field.fill('Beta release');await field.press('Enter');
 await expect.poll(()=>fixture.mutations.filter(m=>m.method==='RenameCollection').length).toBe(1);
 await expect(field).toHaveCount(0);await expect(betaLink).toBeFocused();

 // Only the owner's window asks, once for every workspace.
 const asks=fixture.calls.filter(c=>c.method==='ListWorkspaceGitSummaries');
 assert.ok(asks.length>=4&&asks.every(c=>!c.body.collectionIds?.length),'one request covers every workspace');
 await drainPageErrors(errors);assert.deepEqual(errors,[]);
 console.log('workspace rows: git state, task titles, equal heights, live refresh — ok');
}finally{await browser?.close();await server.close();await fixture.close();}
