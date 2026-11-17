// An agent the person asked, in its chat, to start another conversation asks
// for it (`lazybox conversation new`): the request is a card in the chat and a
// needs-you row in the Inbox. Start opens the new conversation as a tab beside
// this one; Edit… changes the title (and brief, agent, workspace) first; Deny
// sends the person's reason; all of it from the keyboard alone. The card says
// when the new conversation would share this workspace's files, offers a new
// workspace in one click, links the brief artifact, and leads to the
// Orchestrator grant. Fake Runner only.
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from 'vite';import react from '@vitejs/plugin-react';import {chromium,webkit,expect} from '@playwright/test';import {fileURLToPath} from 'node:url';import assert from 'node:assert/strict';import {mkdir} from 'node:fs/promises';import {startRunnerFixture} from './fixtures/runner-ui.mjs';

const b64=value=>Buffer.from(JSON.stringify(value)).toString('base64');
const fixture=await startRunnerFixture();const session=fixture.sessions[0];
Object.assign(session,{title:'Plan the module review',provider:'claude',model:'claude-opus-5-5',busy:true,status:'running',agents:[{id:'root',status:'running'}]});
// Another conversation already works in this workspace: a new one there would share its files.
fixture.sessions.push({id:'alpha-other',collectionId:'alpha',workId:'alpha',profileId:'claude-default',title:'Fix the parser',capabilities:{files:true},status:'ready',pending:[]});
// As the Runner lists a waiting request: a pending entry carrying the request.
const ask=(id,title,{brief='Review every module: list its findings and fix the obvious ones.\nDone when each module has a findings list.\nDo not change public APIs.\nReport per module.',reason='You asked me to in this chat.',workspace='same',sharedWith=2,artifact}={})=>({type:'conversation_request',requestId:id,text:'Start a conversation: '+title,
 providerDataJson:b64({id,sessionId:session.id,conversationId:session.id,collectionId:'alpha',fromTitle:'Plan the module review',title,brief,reason,workspace,provider:'claude',profileId:'claude-default',model:'claude-opus-5-5',permission:'ask',sharedWith,state:'pending',
  ...(artifact?{briefArtifactId:artifact.id,briefArtifactTitle:artifact.title,briefArtifactVersion:artifact.version}:{}),createdAtUnixMillis:String(Date.now())})});
const mutations=method=>fixture.mutations.filter(m=>m.method===method).map(m=>m.body);
session.pending=[ask('start-review','Review all modules')];

const server=await createServer({...testViteCache(import.meta.url),configFile:false,root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});let browser;
try{
 await server.listen();browser=await (process.env.TEST_WEBKIT?webkit:chromium).launch({headless:true});const page=await browser.newPage({viewport:{width:1450,height:1000}});page.setDefaultTimeout(15000);const errors=[];page.on('pageerror',e=>errors.push(String(e)));
 await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`);
 await mkdir('.cache',{recursive:true});

 // ── The card: who asks, the title, the brief, where and on which agent.
 const card=page.locator('.request.start-request-card');
 await expect(card).toHaveCount(1);
 await expect(card.getByRole('heading')).toHaveText('Claude wants to start a conversation: Review all modules');
 await expect(card.locator('.repository-reason')).toHaveText('WhyYou asked me to in this chat.');
 await expect(card.locator('.start-brief-preview')).toContainText('Review every module');
 await expect(card.locator('.start-brief-full summary')).toHaveText('Full brief');
 await expect(card.locator('[data-fact=workspace]')).toContainText('This one');
 await expect(card.locator('[data-fact=workspace]')).toContainText('Shared with 2 running conversations');
 await expect(card.locator('[data-fact=agent]')).toContainText('Claude · Opus 5.5');
 await expect(card.locator('.decision-row')).toHaveText([/Start/,/Start in a new workspace/,/Edit…/,/Deny…/]);
 await expect(card.getByRole('button',{name:'Let this conversation start agents (orchestrator grant)…'})).toBeVisible();
 await expect(card.getByRole('button',{name:'Start',exact:true})).toBeFocused();
 await expect(page.locator('.chat-status [role=status]').first()).toHaveText('Waiting to start a conversation');
 await page.screenshot({path:'.cache/start-conversation-card.png'});

 // ── The Inbox lists it as needing the person.
 const inboxButton=page.locator('.inbox-action');
 await expect(inboxButton).toHaveAccessibleName(/Inbox, 1 conversation needs attention/);
 await inboxButton.click();
 const row=page.locator('.panel-agents li[data-state="needs-input"]').filter({hasText:'Plan the module review'});
 await expect(row.locator('.agent-meta')).toContainText('New conversation');
 await expect(row.locator('.agent-meta')).toContainText('Start a conversation: Review all modules');
 await row.locator('.agent-row').click();

 // ── Start (the keyboard: y): the new conversation opens as a tab beside this one.
 await page.locator('.messages').first().click({position:{x:5,y:5}});
 await page.keyboard.press('y');
 await expect.poll(()=>mutations('AnswerConversationStart')).toEqual([{requestId:'start-review',decision:'start'}]);
 await expect(card).toHaveCount(0);
 const started=fixture.repositoryRequests.get('start-review');
 assert.equal(started.state,'started');
 await expect(page.getByRole('tab',{name:/Review all modules/})).toBeVisible();
 await page.screenshot({path:'.cache/start-conversation-started.png'});

 // ── Edit… changes the title first (keyboard only: e, type, ⌘↵).
 // The new conversation opened behind: this chat stays in front.
 await expect(page.getByRole('tab',{name:'Plan the module review'})).toHaveAttribute('aria-selected','true');
 session.pending=[ask('start-docs','Write docs')];
 await expect(card.getByRole('heading')).toContainText('Write docs');
 await page.locator('.messages').first().click({position:{x:5,y:5}});
 await page.keyboard.press('e');
 const title=card.getByRole('textbox',{name:'Title'});
 await expect(title).toBeFocused();
 await expect(card.getByRole('radio',{name:/This one \(shared with 2 running conversations\)/})).toBeChecked();
 await title.fill('Write the module docs');
 await page.keyboard.press('Tab');
 await expect(card.getByRole('textbox',{name:'Brief'})).toBeFocused();
 await page.screenshot({path:'.cache/start-conversation-edit.png'});
 await page.keyboard.press(process.platform==='darwin'?'Meta+Enter':'Control+Enter');
 await expect.poll(()=>mutations('AnswerConversationStart').at(-1)).toEqual({requestId:'start-docs',decision:'start',title:'Write the module docs'});
 await expect(card).toHaveCount(0);
 assert.equal(fixture.repositoryRequests.get('start-docs').title,'Write the module docs');
 // Escape leaves Edit… without starting anything.
 // The new conversation opened behind: this chat stays in front.
 await expect(page.getByRole('tab',{name:'Plan the module review'})).toHaveAttribute('aria-selected','true');
 session.pending=[ask('start-lint','Fix the lint')];
 await expect(card.getByRole('heading')).toContainText('Fix the lint');
 await card.getByRole('button',{name:'Edit…'}).click();
 await expect(card.getByRole('textbox',{name:'Title'})).toBeFocused();
 await page.keyboard.press('Escape');
 await expect(card.getByRole('button',{name:'Edit…'})).toBeFocused();
 assert.equal(mutations('AnswerConversationStart').length,2);

 // ── Start in a new workspace: one click (w), its own branches.
 await page.locator('.messages').first().click({position:{x:5,y:5}});
 await page.keyboard.press('w');
 await expect.poll(()=>mutations('AnswerConversationStart').at(-1)).toEqual({requestId:'start-lint',decision:'start',workspace:'new'});
 assert.equal(fixture.repositoryRequests.get('start-lint').workspace,'new');
 // It opens in its own workspace; back to the asking conversation.
 await expect(page.getByRole('tab',{name:/Fix the lint/})).toBeVisible();
 await page.screenshot({path:'.cache/start-conversation-new-workspace.png'});
 await page.locator('.panel-agents').getByText('Plan the module review',{exact:true}).click();
 await expect(page.getByRole('tab',{name:'Plan the module review'})).toHaveAttribute('aria-selected','true');

 // ── Deny, with a reason the agent is told: n, type, Enter.
 session.pending=[ask('start-rewrite','Rewrite the parser',{sharedWith:1})];
 await expect(card.getByRole('heading')).toContainText('Rewrite the parser');
 await expect(card.locator('[data-fact=workspace]')).toContainText('Shared with this conversation');
 await page.locator('.messages').first().click({position:{x:5,y:5}});
 await page.keyboard.press('n');
 const reason=card.getByRole('textbox',{name:/Why not\?/});await expect(reason).toBeFocused();
 await reason.fill('Do it here once the tests pass');
 await page.keyboard.press('Enter');
 await expect.poll(()=>mutations('AnswerConversationStart').at(-1)).toEqual({requestId:'start-rewrite',decision:'decline',reason:'Do it here once the tests pass'});
 await expect(card).toHaveCount(0);
 assert.equal(fixture.repositoryRequests.get('start-rewrite').state,'declined');

 // ── A phone-width window: the card still fits.
 session.pending=[ask('start-narrow','Review on the go')];
 await page.setViewportSize({width:420,height:900});
 await expect(card).toBeVisible();
 assert.ok(await card.evaluate(node=>node.scrollWidth<=node.clientWidth+1),'the card does not overflow a narrow window');
 await page.screenshot({path:'.cache/start-conversation-narrow.png'});
 await page.setViewportSize({width:1450,height:1000});

 // ── A brief artifact: linked, opened in the Artifacts panel; arrows move between choices.
 session.pending=[ask('start-spec','Implement the spec',{brief:'',workspace:'new',artifact:{id:'spec-1',title:'Module review spec',version:3}})];
 await expect(card.getByRole('heading')).toContainText('Implement the spec');
 await expect(card.locator('[data-fact=brief]')).toContainText('Module review spec');
 await expect(card.locator('[data-fact=brief]')).toContainText('version 3');
 await expect(card.locator('[data-fact=workspace]')).toContainText('A new workspace');
 await expect(card.locator('.decision-row')).toHaveText([/Start/,/Edit…/,/Deny…/]);
 await card.getByRole('button',{name:'Start',exact:true}).focus();
 await page.keyboard.press('ArrowDown');
 await expect(card.getByRole('button',{name:'Edit…'})).toBeFocused();
 await page.keyboard.press('ArrowDown');
 await expect(card.getByRole('button',{name:'Deny…'})).toBeFocused();
 await card.getByRole('button',{name:'Open the artifact Module review spec'}).click();
 await expect(page.locator('.panel-artifacts, [data-panel-kind=artifacts]').first()).toBeVisible();
 session.pending=[];
 await expect(card).toHaveCount(0);

 assert.deepEqual(errors,[]);
 console.log('Agent starts a conversation: card, inbox, start opens a tab, edit the title, new workspace, deny with a reason, artifact brief, keyboard only, narrow.');
}finally{await browser?.close();await server.close();await fixture.close?.();}
