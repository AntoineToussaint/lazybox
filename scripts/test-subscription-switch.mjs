import {collectPageErrors} from './fixtures/page-errors.mjs';
// Fixture only: a subscription limit, as the person meets it. An automatic
// switch is one quiet line; without one, one card offers to switch to the best
// other subscription, or to wait for the reset; a message sent meanwhile goes
// once; ⋯ → Switch subscription… works any time. Switching by itself is an
// extension's account policy (EXTENSIONS.md): by default the person chooses,
// and only with Subscription rotation on do "Always switch automatically" and
// the dialog's switch appear.
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from 'vite';import react from '@vitejs/plugin-react';import {webkit,expect} from './fixtures/playwright.mjs';import {fileURLToPath} from 'node:url';import {mkdir} from 'node:fs/promises';import assert from 'node:assert/strict';import {startRunnerFixture} from './fixtures/runner-ui.mjs';
const f=await startRunnerFixture();
const now=Date.now(),reset=Math.floor(now/1000)+3*3600;
const clock=seconds=>new Date(seconds*1000).toLocaleTimeString('en-US',{hour:'numeric',minute:'2-digit'});
const backLabel=new RegExp(`(tomorrow )?at ${clock(reset).replace(/\s/g,'\\s')}`);
const saved=(id,label,availability)=>({id,label,provider:'claude',isolation:'environment',managed:true,availability});
// Codex subscriptions too: a Claude conversation's lists never offer them (it cannot switch across providers).
f.profiles.push({...saved('codex-work','Codex at work',{state:'available'}),provider:'codex'});
f.profiles.push(saved('work-1','Work subscription 1',{state:'available',windows:[{name:'five_hour',usedPercent:37,durationMinutes:'300'}]}),saved('work-2','Work subscription 2',{state:'exhausted',resetAt:String(reset)}),saved('work-3','Work subscription 3',{state:'available'}));
const data=value=>Buffer.from(JSON.stringify(value)).toString('base64');
const limitEvent=resetAt=>({type:'failure',status:'exhausted',text:'rate_limit',account:{state:'exhausted',resetAt:String(resetAt)}});

// Alpha switched by itself when Work subscription 2 hit its limit.
Object.assign(f.sessions[0],{title:'Fix the flaky login test',provider:'claude',profileId:'work-1',status:'done',busy:false,rotationProfileIds:['work-2','work-3'],agents:[{id:'root',status:'done',lastTurnStatus:'completed'}]});
const alpha=f.openEvents('alpha-session');
for(const event of [{type:'operation_accepted',text:'The login test is flaky, can you find out why?'},{type:'message_delta',text:'Looking at the test first.'},limitEvent(reset),{type:'capacity_observation',status:'exhausted',account:{state:'exhausted',resetAt:String(reset)}},{type:'profile_switching',status:'exhausted'},{type:'profile_changed',text:'work-1',status:'checkpoint_handoff',providerDataJson:data({from:'work-2',fromState:'exhausted',resetAt:reset,requested:false})},{type:'message_delta',text:'Picking up where I stopped: the test races the session cookie.'},{type:'turn_ended',status:'completed'}])alpha.push(event);

// Beta started before automatic switching existed and stopped at the limit.
Object.assign(f.sessions[1],{title:'Ship the release notes',provider:'claude',profileId:'work-2',status:'failed',fault:'exhausted',busy:false,rotationProfileIds:[],account:{state:'exhausted',resetAt:String(reset)},agents:[{id:'root',status:'failed',lastTurnStatus:'failed'}]});
const beta=f.openEvents('beta-session');
for(const event of [{type:'operation_accepted',text:'Write the release notes for 0.1.21'},{type:'message_delta',text:'Collecting the merged pull requests.'},limitEvent(reset),{type:'turn_ended',status:'failed'}])beta.push(event);

// Delta stops at a limit after Subscription rotation is turned on.
f.collections.push({id:'delta',name:'Delta notes',workId:'delta',root:'/fixture/collections/delta',state:'ready',ownerExecutionId:'delta-session',members:[{spec:{name:'api',repositoryId:'backend',baseRef:'main',branch:'lazybox/delta/api'},path:'/fixture/collections/delta/api',baseCommit:'c'.repeat(40),ready:true}]});
f.sessions.push({id:'delta-session',collectionId:'delta',workId:'delta',title:'Draft the notes',provider:'claude',profileId:'work-2',gatewayId:'direct',workingDirectory:'/fixture/collections/delta',capabilities:{files:true,images:true,resume:true,livePermission:true},permission:'ask',pending:[],status:'failed',fault:'exhausted',rotationProfileIds:[],account:{state:'exhausted',resetAt:String(reset)},agents:[{id:'root',status:'failed',lastTurnStatus:'failed'}]});
const delta=f.openEvents('delta-session');
for(const event of [{type:'operation_accepted',text:'Draft the notes'},{type:'turn_started'},{type:'message_delta',text:'I found the deployment checklist in docs/runbook.md.'},{type:'turn_ended',status:'completed'},{type:'operation_accepted',text:'Include the rollback procedure.'},limitEvent(reset),{type:'turn_ended',status:'failed'}])delta.push(event);

// Gamma waits for a reset a few seconds away.
const soon=Math.floor(Date.now()/1000)+6;
f.collections.push({id:'gamma',name:'Gamma docs',workId:'gamma',root:'/fixture/collections/gamma',state:'ready',ownerExecutionId:'gamma-session',members:[{spec:{name:'api',repositoryId:'backend',baseRef:'main',branch:'lazybox/gamma/api'},path:'/fixture/collections/gamma/api',baseCommit:'c'.repeat(40),ready:true}]});
f.sessions.push({id:'gamma-session',collectionId:'gamma',workId:'gamma',title:'Tidy the docs',provider:'claude',profileId:'work-2',gatewayId:'direct',workingDirectory:'/fixture/collections/gamma',capabilities:{files:true,images:true,resume:true,livePermission:true},permission:'ask',pending:[],status:'failed',fault:'exhausted',rotationProfileIds:[],account:{state:'exhausted',resetAt:String(soon)},agents:[{id:'root',status:'failed',lastTurnStatus:'failed'}]});
const gamma=f.openEvents('gamma-session');
for(const event of [{type:'operation_accepted',text:'Tidy the docs folder'},limitEvent(soon),{type:'turn_ended',status:'failed'}])gamma.push(event);

const server=await createServer({...testViteCache(import.meta.url),configFile:false,logLevel:'error',root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:f.url}}}});let browser;
const raw=chat=>Promise.all([expect(chat).not.toContainText(/exhausted|rate_limit|provider failure|provider_error/i),expect(chat.locator('.account-notice')).toHaveCount(0)]);
try{
 await server.listen();await mkdir('.cache',{recursive:true});browser=await webkit.launch();
 const context=await browser.newContext({viewport:{width:1440,height:900},locale:'en-US'});
 const page=await context.newPage();page.setDefaultTimeout(15000);
 const errors=await collectPageErrors(page);
 const base=`http://127.0.0.1:${server.httpServer.address().port}`;

 // 1. Automatic: one quiet line says where the conversation went and when the old one resets.
 await page.goto(`${base}/?view=workspace&collection=alpha&session=alpha-session#token=${f.token}`);
 let chat=page.locator('.embedded-chat:visible').first();
 const line=chat.locator('.subscription-switched');
 await expect(line).toHaveCount(1);
 await expect(line).toHaveText(new RegExp(`^Switched to Work subscription 1 · Work subscription 2 is back ${backLabel.source}$`));
 await expect(chat).toContainText('Picking up where I stopped');
 await expect(chat.locator('.limit-card')).toHaveCount(0);
 await expect(chat.locator('.messages:visible .error')).toHaveCount(0);
 await raw(chat);
 await page.screenshot({path:'.cache/subscription-auto-switch.png'});
 // The header's subscription chip opens Switch subscription… (the same as /subscription).
 await chat.getByRole('button',{name:'Subscription Work subscription 1: switch subscription'}).click();
 await expect(page.getByRole('dialog',{name:'Switch subscription'})).toBeVisible();
 await page.keyboard.press('Escape');
 await expect(page.getByRole('dialog',{name:'Switch subscription'})).toHaveCount(0);

 // 4. ⋯ → Switch subscription… at any time. Without an account policy it switches only when asked, and says where switching by itself lives.
 await chat.locator('summary[aria-label="Session actions"]').click();
 await page.getByRole('button',{name:'Switch subscription…'}).click();
 let dialog=page.getByRole('dialog',{name:'Switch subscription'});
 await expect(dialog).toBeVisible();
 await expect(dialog.getByRole('radio',{name:/Work subscription 1/})).toBeDisabled();
 await expect(dialog.getByRole('radio',{name:/Work subscription 1/})).toHaveAccessibleName(/current/);
 // One provider, so no group header: the current one first, then available, then not known to be limited, then limited; by name within each.
 await expect(dialog.locator('.subscription-choice-name')).toHaveText(['Work subscription 1 · current','Work subscription 3','Claude CLI sign-in','Work subscription 2']);
 await expect(dialog.locator('.provider-group-heading')).toHaveCount(0);
 await expect(dialog).not.toContainText('Codex at work');
 // Arrow keys move through the choices.
 await dialog.getByRole('radio',{name:/Work subscription 3/}).focus();await page.keyboard.press('ArrowDown');
 await expect(dialog.getByRole('radio',{name:/Claude CLI sign-in/})).toBeChecked();
 await expect(dialog.locator('.subscription-choice').filter({hasText:'Work subscription 2'})).toContainText(/Limit reached · back (tomorrow )?at/); // a reset after midnight reads "tomorrow at"
 await expect(dialog.locator('.subscription-choice').filter({hasText:'Work subscription 3'})).toContainText('Available');
 await expect(dialog.getByRole('checkbox',{name:'Switch automatically when a subscription reaches its limit'})).toHaveCount(0);
 await expect(dialog).toContainText('Lazybox switches only when you ask. Switching by itself at a limit is an extension: Settings → Extensions.');
 await dialog.getByRole('radio',{name:/Work subscription 3/}).check();
 await page.screenshot({path:'.cache/subscription-switch-dialog.png'});
 await page.emulateMedia({colorScheme:'dark'});await page.waitForTimeout(400);await page.screenshot({path:'.cache/subscription-switch-dialog-dark.png'});await page.emulateMedia({colorScheme:'light'});
 await dialog.getByRole('button',{name:'Switch to Work subscription 3'}).click();
 await expect(dialog).toHaveCount(0);
 assert.deepEqual(f.mutations.findLast(m=>m.method==='SwitchSubscription').body,{sessionId:'alpha-session',profileId:'work-3'});
 await expect(chat.locator('.subscription-switched')).toHaveCount(2);
 await expect(chat.locator('.subscription-switched').last()).toHaveText('Switched to Work subscription 3');

 // 2. No automatic switching: ONE card, in words, with the best other subscription first.
 await page.goto(`${base}/?view=workspace&collection=beta&session=beta-session#token=${f.token}`);
 chat=page.locator('.embedded-chat:visible').first();
 const card=chat.locator('.limit-card');
 await expect(card).toHaveCount(1);
 await expect(card.getByRole('heading')).toHaveText(new RegExp(`^Work subscription 2 reached its limit · back ${backLabel.source} \\(in .+\\)$`));
 await expect(chat.locator('.chat-heading .chat-status')).toContainText(new RegExp(`Limit reached · back ${backLabel.source}`));
 await expect(chat.locator('.chat-heading .chat-status')).not.toContainText('Needs attention');
 await expect(chat.locator('.messages:visible .error')).toHaveCount(0);
 await raw(chat);
 await expect(card.getByRole('radio')).toHaveCount(3);
 // Only the conversation's own provider, available first then by name; no header for one group.
 await expect(card.locator('.subscription-choice-name')).toHaveText(['Work subscription 1','Work subscription 3','Claude CLI sign-in']);
 await expect(card.locator('.provider-group-heading')).toHaveCount(0);
 await expect(card).not.toContainText('Codex');
 await expect(card.getByRole('radio',{name:/Work subscription 1/})).toBeChecked();
 await expect(card.locator('.subscription-choice').filter({hasText:'Work subscription 1'})).toContainText('Available · 37% used');
 await expect(card.getByRole('button',{name:'Wait for reset'})).toBeVisible();
 // No account policy: the person chooses; nothing offers to switch by itself.
 await expect(card.getByRole('checkbox',{name:'Always switch automatically'})).toHaveCount(0);
 // A message sent while waiting is kept, and goes once.
 const composer=chat.getByRole('textbox',{name:'Message',exact:true});
 await expect(composer).toHaveAttribute('placeholder',/sent when the conversation continues/);
 await composer.fill('Also mention the shim fix');await composer.press('Enter');
 await expect(composer).toHaveValue('');
 await expect(card).toContainText('Your message is saved and will be sent once, when the conversation continues.');
 await page.screenshot({path:'.cache/subscription-limit-card.png'});
 await card.getByRole('button',{name:'Switch to Work subscription 1'}).click();
 await expect.poll(()=>f.mutations.findLast(m=>m.method==='SwitchSubscription')?.body).toEqual({sessionId:'beta-session',profileId:'work-1'});
 await expect(chat.locator('.limit-card[aria-label="Subscription limit"]')).toHaveCount(0);
 await expect(chat.locator('.subscription-switched')).toHaveText(new RegExp(`^Switched to Work subscription 1 · Work subscription 2 is back ${backLabel.source}$`));
 await expect(chat.locator('.chat-heading .chat-status')).not.toContainText('Limit reached');
 // The person's own switch asks nothing more: no question about switching by itself.
 await expect(chat.getByRole('region',{name:'Automatic switching'})).toHaveCount(0);
 assert.equal(f.mutations.filter(m=>m.method==='SetSessionRotation').length,0,'nothing turned automatic switching on');
 // Never sent twice: the interrupted request continues on the Runner, the follow-up was one operation.
 const applies=f.mutations.filter(m=>m.method==='Apply'&&m.body.sessionId==='beta-session');
 assert.equal(applies.length,1,`one send, not ${applies.length}`);
 assert.equal(applies[0].body.message.text,'Also mention the shim fix');
 await page.waitForTimeout(2000);
 assert.equal(f.mutations.filter(m=>m.method==='Apply'&&m.body.sessionId==='beta-session').length,1,'no resend after the switch');
 await raw(chat);

 // 3. Wait for reset: a quiet line, then the same request continues on its own, once.
 f.sessions.find(s=>s.id==='gamma-session').account.resetAt=String(Math.floor(Date.now()/1000)+6);
 await page.goto(`${base}/?view=workspace&collection=gamma&session=gamma-session#token=${f.token}`);
 chat=page.locator('.embedded-chat:visible').first();
 await chat.locator('.limit-card').getByRole('button',{name:'Wait for reset'}).click();
 await expect(chat.locator('.limit-card.is-waiting')).toContainText(/Waiting for Work subscription 2: back .*at .* \(in .+\)\. The conversation continues on its own then\./);
 await expect(chat.locator('.limit-card.is-waiting').getByRole('button',{name:'Switch to Work subscription 1'})).toBeVisible();
 await page.screenshot({path:'.cache/subscription-waiting.png'});
 await expect.poll(()=>f.mutations.filter(m=>m.method==='SwitchSubscription'&&m.body.sessionId==='gamma-session').map(m=>m.body),{timeout:20000}).toEqual([{sessionId:'gamma-session',profileId:'work-2'}]);
 await expect(chat.locator('.subscription-switched')).toHaveText('Continued on Work subscription 2');
 await expect(chat.locator('.limit-card')).toHaveCount(0);
 await page.waitForTimeout(2500);
 assert.equal(f.mutations.filter(m=>m.method==='SwitchSubscription'&&m.body.sessionId==='gamma-session').length,1,'resumed once');
 // 5. With Subscription rotation installed and allowed (EXTENSIONS.md), the card offers switching by itself, once asked everywhere.
 await f.extensions.handle('InstallExampleExtension',{example:'subscription-rotation'});
 const rotation=(await f.extensions.handle('ListExtensions',{})).value.extensions.find(e=>e.id==='subscription-rotation');
 assert.deepEqual(rotation.accountPolicy?.providers,['codex','claude']);
 await f.extensions.handle('ApproveExtension',{id:'subscription-rotation',grants:rotation.grants,hosts:rotation.hosts});
 await page.goto(`${base}/?view=workspace&collection=delta&session=delta-session#token=${f.token}`);
 chat=page.locator('.embedded-chat').first();
 const deltaCard=chat.locator('.limit-card');
 const always=deltaCard.getByRole('checkbox',{name:'Always switch automatically'});
 await expect(always).toBeChecked();
 await deltaCard.getByRole('button',{name:'Switch to Work subscription 1'}).click();
 await expect.poll(()=>f.mutations.findLast(m=>m.method==='SwitchSubscription')?.body).toEqual({sessionId:'delta-session',profileId:'work-1',enableRotation:true});
 const everywhere=chat.getByRole('region',{name:'Automatic switching'});
 await expect(everywhere).toContainText('Do the same for every Claude conversation?');
 await page.screenshot({path:'.cache/subscription-everywhere.png'});
 await everywhere.getByRole('button',{name:'Every conversation'}).click();
 await expect.poll(()=>f.mutations.findLast(m=>m.method==='SetSessionRotation')?.body).toEqual({sessionId:'delta-session',enabled:true,allSessions:true});
 await expect(everywhere).toHaveCount(0);
 // ⋯ → Switch subscription… now has the switch for this conversation.
 await chat.locator('summary[aria-label="Session actions"]').click();
 await page.getByRole('button',{name:'Switch subscription…'}).click();
 dialog=page.getByRole('dialog',{name:'Switch subscription'});
 const auto=dialog.getByRole('checkbox',{name:'Switch automatically when a subscription reaches its limit'});
 await expect(auto).toBeVisible();
 await page.screenshot({path:'.cache/subscription-switch-dialog-policy.png'});
 await dialog.getByRole('button',{name:'Cancel'}).click();

 // When every Claude account is limited, the card offers a fresh Codex
 // conversation in the same workspace and sends the handoff.
 for (const profile of f.profiles.filter(p=>p.provider==='claude')) profile.availability={state:'exhausted',resetAt:String(reset)};
 const limited=f.sessions.find(s=>s.id==='delta-session');
 Object.assign(limited,{status:'failed',fault:'exhausted',account:{state:'exhausted',resetAt:String(reset)}});
 await expect(deltaCard).toContainText('No other Claude subscription is available right now.');
 await expect(deltaCard.getByRole('radio')).toHaveCount(0);
 await expect(deltaCard.getByRole('button',{name:/Switch to/})).toHaveCount(0);
 await deltaCard.getByRole('button',{name:'Show all subscriptions'}).click();
 const statusDialog=page.getByRole('dialog',{name:'All subscriptions'});
 await expect(statusDialog.locator('dt')).toHaveCount(f.profiles.length);
 await expect(statusDialog.getByRole('region',{name:'Claude'})).toContainText('Limit reached');
 await expect(statusDialog.getByRole('region',{name:'Codex'})).toBeVisible();
 await expect(statusDialog).toContainText('· current');
 await statusDialog.getByRole('button',{name:'Close subscription status'}).click();
 await expect(statusDialog).toHaveCount(0);
 const fallback=deltaCard.getByRole('button',{name:'Continue in a new Codex conversation'});
 await page.screenshot({path:'.cache/subscription-codex-fallback-light.png'});
 await page.emulateMedia({colorScheme:'dark'});
 await page.waitForTimeout(400);
 await page.screenshot({path:'.cache/subscription-codex-fallback-dark.png'});
 limited.queuedMessages=[{operationId:'queued-verify',text:'Verify the staging environment.'}];
 limited.queued=1;
 await expect(deltaCard).toContainText('Your message is saved');
 await chat.getByRole('textbox',{name:'Message',exact:true}).fill('Keep the notes concise.');
 const writes=f.mutations.filter(m=>m.method==='Apply').length;
 await fallback.focus();
 await page.keyboard.press('Enter');
 await expect.poll(()=>f.mutations.findLast(m=>m.method==='OpenSession')?.body).toMatchObject({collectionId:'delta',profileId:'codex-work'});
 await expect.poll(()=>f.mutations.findLast(m=>m.method==='Apply')?.body?.message?.text).toContain('Include the rollback procedure.');
 const handoff=f.mutations.findLast(m=>m.method==='Apply').body;
 assert.match(handoff.message.text,/Draft the notes/);
 assert.match(handoff.message.text,/deployment checklist in docs\/runbook\.md/);
 assert.match(handoff.message.text,/Verify the staging environment/);
 assert.match(handoff.message.text,/Keep the notes concise/);
 assert.equal(handoff.sessionId,f.sessions.at(-1).id);
 assert.equal(f.mutations.filter(m=>m.method==='Apply').length,writes+1,'the handoff sends once');
 const revisit=await page.context().newPage();
 await revisit.goto(`${base}/?view=workspace&collection=delta&session=delta-session#token=${f.token}`);
 await revisit.getByRole('combobox',{name:'Session',exact:true}).selectOption('delta-session');
 const continued=revisit.getByRole('button',{name:'Open continued Codex conversation'});
 await expect(continued).toBeVisible();
 await expect(revisit.getByRole('region',{name:'Subscription limit'})).toHaveCount(0);
 const openedBefore=f.mutations.filter(m=>m.method==='OpenSession').length;
 await continued.click();
 await expect(revisit.getByRole('combobox',{name:'Session',exact:true})).toHaveValue(handoff.sessionId);
 assert.equal(f.mutations.filter(m=>m.method==='OpenSession').length,openedBefore);
 await revisit.close();
 const refreshPage=await browser.newPage({viewport:{width:1440,height:900}});
 await collectPageErrors(refreshPage,errors);
 await refreshPage.goto(`${base}/?view=workspace&collection=delta&session=delta-session#token=${f.token}`);
 const refreshCard=refreshPage.locator('.embedded-chat:visible').getByRole('region',{name:'Subscription limit'});
 const checkedBefore=f.calls.filter(call=>call.method==='RefreshSubscription').length;
 const switchesBefore=f.mutations.filter(m=>m.method==='SwitchSubscription').length;
 await refreshCard.getByRole('button',{name:'Refresh',exact:true}).click();
 await expect(refreshCard).toContainText('is available again');
 await expect(refreshCard.getByRole('button',{name:/Continue on Work/})).toBeEnabled();
 const checked=f.calls.filter(call=>call.method==='RefreshSubscription').slice(checkedBefore);
 assert.equal(checked.length,3);
 assert.equal(f.mutations.filter(m=>m.method==='SwitchSubscription').length,switchesBefore,'refresh does not switch or resume the execution');
 assert.deepEqual(errors,[]);
 console.log('PASS automatic switch line, one limit card in words (Switch to the best subscription, picker of the same provider in order, wait), the header chip opens Switch subscription, nothing switches by itself without an account policy, a message sent while limited goes once, wait-for-reset resumes once, ⋯ Switch subscription; with Subscription rotation allowed: Always switch automatically and the everywhere question. Fixture only.');
}finally{await browser?.close();await server.close();await f.close();}
