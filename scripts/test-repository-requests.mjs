// An agent asks to add a repository to its workspace instead of reaching
// outside it (`lazybox workspace request-repo`): the request is a card in the
// conversation and a needs-you row in the Inbox; Add uses the workspace's own
// add path with its progress, Choose another… opens the repository picker on
// the likely repositories with the match preselected, and Decline sends the
// person's reason. Fake Runner only.
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from './fixtures/vite-cache.mjs';import react from '@vitejs/plugin-react';import {chromium,webkit,expect} from './fixtures/playwright.mjs';import {fileURLToPath} from 'node:url';import assert from 'node:assert/strict';import {mkdir} from 'node:fs/promises';import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import { collectPageErrors, drainPageErrors } from './fixtures/page-errors.mjs';

const b64=value=>Buffer.from(JSON.stringify(value)).toString('base64');
const fixture=await startRunnerFixture();const session=fixture.sessions[0];
Object.assign(session,{title:'Fix the parser',provider:'claude',busy:true,status:'running',agents:[{id:'root',status:'running'}]});
// The catalog: repositories the person has, none of them in the workspace yet.
fixture.library.push(
 {id:'module',name:'module-directory',path:'/fixture/repos/module-directory',gitIdentity:'github:acme/module-directory'},
 {id:'other-module',name:'module-directory',path:'/fixture/repos/other-module-directory',gitIdentity:'github:other/module-directory'},
 {id:'tools',name:'tools',path:'/fixture/repos/tools',gitIdentity:'github:acme/tools'},
 {id:'tools-legacy',name:'tools-legacy',path:'/fixture/repos/tools-legacy',gitIdentity:'github:acme/tools-legacy'},
);
const candidate=(name,source='registered',score=90)=>({identity:'github:'+name,name,source,score});
// As the Runner lists a waiting request: a pending entry carrying the request.
const ask=(id,query,reason,candidates)=>({type:'repository_request',requestId:id,text:candidates[0]?`Add ${candidates[0].name} to this workspace`:'Add a repository to this workspace',
 providerDataJson:b64({id,sessionId:session.id,conversationId:session.id,collectionId:'alpha',query,reason,branch:'feature/parser',state:'pending',candidates,createdAtUnixMillis:String(Date.now())})});
session.pending=[ask('repo-module','module_directory','The parser imports its tokenizer; I need to change its token types.',[candidate('acme/module-directory','registered',85),candidate('other/module-directory','registered',60)])];
const mutations=method=>fixture.mutations.filter(m=>m.method===method).map(m=>m.body);

const server=await createServer({...testViteCache(import.meta.url),configFile:false,root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});let browser;
try{
 await server.listen();browser=await (process.env.TEST_WEBKIT?webkit:chromium).launch({headless:true});const page=await browser.newPage({viewport:{width:1450,height:1000}});page.setDefaultTimeout(15000);const errors=[];await collectPageErrors(page, errors);
 await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`);
 await mkdir('.cache',{recursive:true});

 // ── The card: who asks, for what, why, on which branch.
 const card=page.locator('.request.repository-request-card');
 await expect(card).toHaveCount(1);
 await expect(card.getByRole('heading')).toHaveText('Claude asks to add acme/module-directory to this workspace');
 await expect(card.locator('.repository-reason')).toHaveText('WhyThe parser imports its tokenizer; I need to change its token types.');
 await expect(card.locator('.repository-facts')).toContainText('feature/parser');
 await expect(card.locator('.repository-facts')).toContainText('Already on this computer');
 await expect(card.getByRole('group',{name:'Matches'}).getByRole('button')).toHaveText(['acme/module-directory','other/module-directory']);
 await expect(card.locator('.decision-row')).toHaveText([/Add acme\/module-directory/,/Choose another…/,/Decline…/]);
 await expect(card).toBeFocused();
 // The conversation waits on the person, and says so.
 await expect(page.locator('.chat-status [role=status]')).toHaveText('Waiting for a repository');
 await page.screenshot({path:'.cache/repository-request-card.png'});

 // ── The Inbox lists it as needing the person.
 const inboxButton=page.locator('.inbox-action');
 await expect(inboxButton).toHaveAccessibleName(/Inbox, 1 conversation needs attention/);
 await inboxButton.click();
 // The Inbox is the docked Agents panel (audit R1): the row says what is asked.
 const row=page.locator('.panel-agents li[data-state="needs-input"]').filter({hasText:'Fix the parser'});
 await expect(row.locator('.agent-meta')).toContainText('Repository request');
 await expect(row.locator('.agent-meta')).toContainText('Add acme/module-directory to this workspace');
 await page.screenshot({path:'.cache/repository-request-inbox.png'});
 await row.locator('.agent-row').click();

 // A failed addition leaves the request open and shows the Runner's next step.
 const noCommit='module-directory has no starting commit. Create an initial commit, then retry.';
 const end=Buffer.from(JSON.stringify({error:{code:'failed_precondition',message:noCommit}}));
 const header=Buffer.alloc(5);
 header[0]=2;
 header.writeUInt32BE(end.length,1);
 await page.route('**/PrepareWorkspace',route=>route.fulfill({
  contentType:'application/connect+json',body:Buffer.concat([header,end]),
 }),{times:1});
 await card.getByRole('button',{name:'Add acme/module-directory'}).click();
 await expect(card.getByRole('alert')).toHaveText(noCommit);

 // ── Add: the workspace's own add path, its progress in the card, then gone.
 fixture.runner.preparationDelayMs=400;
 // The done line shows until the next session poll drops the answered card, which can be at once on
 // a slow machine: watched every frame from before the click, so a brief one is still seen.
 const done=page.waitForFunction(()=>document.querySelector('.request.repository-request-card .repository-done')?.textContent.replace(/\s+/g,' ').trim(),undefined,{polling:'raf'});
 await card.getByRole('button',{name:'Add acme/module-directory'}).click();
 await expect(card.getByRole('status')).toContainText('Preparing acme/module-directory');
 await expect(card.getByRole('progressbar',{name:'Adding acme/module-directory'})).toBeVisible();
 await page.screenshot({path:'.cache/repository-request-adding.png'});
 assert.equal(await (await done).jsonValue(),'Added acme/module-directory · Claude is told');
 assert.deepEqual(mutations('PrepareWorkspace').map(({collectionId,repositoryIdentities,repositoryRequestId,requestKey})=>({collectionId,repositoryIdentities,repositoryRequestId,key:requestKey.startsWith('repository-request:repo-module:')})),
  [{collectionId:'alpha',repositoryIdentities:['github:acme/module-directory'],repositoryRequestId:'repo-module',key:true}]);
 await expect(card).toHaveCount(0);
 assert.ok(fixture.collections.find(c=>c.id==='alpha').members.some(m=>m.spec.name==='module-directory'),'the repository is in the workspace');
 assert.equal(fixture.repositoryRequests.get('repo-module').state,'added');
 fixture.runner.preparationDelayMs=0;

 // ── Choose another…: the picker opens on the likely repositories, the match preselected.
 session.pending=[ask('repo-tools','tool','The lint rules live there.',[candidate('acme/tools-legacy','registered',70)])];
 await expect(card.getByRole('heading')).toHaveText('Claude asks to add acme/tools-legacy to this workspace');
 await expect(card).toBeFocused();
 await page.locator('.messages:visible').first().click({position:{x:5,y:5}});
 await page.keyboard.press('c');
 const dialog=page.getByRole('dialog',{name:'Add repository to workspace'});
 await expect(dialog).toBeVisible();
 await expect(dialog).toContainText('Claude asked for acme/tools-legacy: “The lint rules live there.” Choose the repository to add.');
 await expect(dialog.getByRole('searchbox',{name:'Find repositories'})).toHaveValue('tool');
 await expect(dialog.getByRole('checkbox',{name:'acme/tools-legacy'})).toBeChecked();
 await expect(dialog.getByRole('checkbox',{name:'acme/tools',exact:true})).not.toBeChecked();
 await page.screenshot({path:'.cache/repository-request-choose.png'});
 await dialog.getByRole('checkbox',{name:'acme/tools-legacy'}).uncheck();
 await dialog.getByRole('checkbox',{name:'acme/tools',exact:true}).check();
 await dialog.getByRole('button',{name:'Add to workspace'}).click();
 await expect(dialog).toHaveCount(0);
 await expect.poll(()=>mutations('PrepareWorkspace').at(-1)?.repositoryIdentities).toEqual(['github:acme/tools']);
 assert.equal(mutations('PrepareWorkspace').at(-1).repositoryRequestId,'repo-tools');
 await expect(card).toHaveCount(0);
 assert.equal(fixture.repositoryRequests.get('repo-tools').state,'added');

 // ── Decline, with a reason the agent is told: n, type, Enter.
 session.pending=[ask('repo-fork','/Users/me/src/parser-fork','A fork has the fix already.',[{identity:'',name:'parser-fork',source:'folder',localPath:'/Users/me/src/parser-fork',score:100}])];
 await expect(card.getByRole('heading')).toHaveText('Claude asks to add parser-fork to this workspace');
 await expect(card.locator('.repository-facts')).toContainText('Local folder, not in your repositories yet');
 await page.locator('.messages:visible').first().click({position:{x:5,y:5}});
 await page.keyboard.press('n');
 const reason=card.getByRole('textbox',{name:/Why not\?/});await expect(reason).toBeFocused();
 await reason.fill('Use the upstream parser; the fork is abandoned');
 await page.screenshot({path:'.cache/repository-request-decline.png'});
 await page.keyboard.press('Enter');
 await expect.poll(()=>mutations('DeclineRepositoryRequest')).toEqual([{requestId:'repo-fork',reason:'Use the upstream parser; the fork is abandoned'}]);
 await expect(card).toHaveCount(0);
 assert.equal(fixture.repositoryRequests.get('repo-fork').reason,'Use the upstream parser; the fork is abandoned');

 // ── Nothing matched: the person chooses one, or declines.
 session.pending=[ask('repo-unknown','billing-sdk','The invoices call it.',[])];
 await expect(card.getByRole('heading')).toHaveText('Claude asks for a repository: “billing-sdk”');
 await expect(card.locator('.repository-nomatch')).toBeVisible();
 await expect(card.locator('.decision-row')).toHaveText([/Choose a repository…/,/Decline…/]);
 await expect(card).toBeFocused();
 // Escape leaves the reason without declining.
 await card.getByRole('button',{name:'Decline…'}).click();
 await expect(card.getByRole('textbox',{name:/Why not\?/})).toBeFocused();
 await page.keyboard.press('Escape');
 await expect(card.getByRole('button',{name:'Decline…'})).toBeFocused();
 assert.equal(mutations('DeclineRepositoryRequest').length,1);
 session.pending=[];
 await expect(card).toHaveCount(0);

 // ── A phone: narrow, the card still fits.
 session.pending=[ask('repo-narrow','module','Needed on the go.',[candidate('acme/module-directory'),candidate('other/module-directory','github',60)])];
 await page.setViewportSize({width:420,height:900});
 await expect(card).toBeVisible();
 assert.ok(await card.evaluate(node=>node.scrollWidth<=node.clientWidth+1),'the card does not overflow a narrow window');
 await page.screenshot({path:'.cache/repository-request-narrow.png'});

 await drainPageErrors(errors);assert.deepEqual(errors,[]);
 console.log('Repository requests: card, inbox, add with progress, choose another, decline with a reason, no match, narrow.');
}finally{await browser?.close();await server.close();await fixture.close?.();}
