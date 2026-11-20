// Explicitly fake account authority: no provider login, credentials or inference.
import {testViteCache} from './fixtures/vite-cache.mjs';
import { createServer } from 'vite';
import react from '@vitejs/plugin-react';
import { chromium, expect } from './fixtures/playwright.mjs';
import { fileURLToPath } from 'node:url';
import { mkdir } from 'node:fs/promises';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';
const fixture = await startRunnerFixture();
// This suite enrolls Claude through Lazybox and toggles automatic switching per
// account: both are opt-in (CLAUDE_COMPLIANCE.md). Claude sign-in in Lazybox
// is turned on, and Subscription rotation installed and allowed, as the person
// would in Settings; scripts/test-claude-compliance.mjs covers them off.
fixture.runnerSettings.claudeSubscriptionSignIn = true;
await fixture.extensions.handle('InstallExampleExtension', { example: 'subscription-rotation' });
{ const rotation = (await fixture.extensions.handle('ListExtensions', {})).value.extensions.find(e => e.id === 'subscription-rotation');
  await fixture.extensions.handle('ApproveExtension', { id: rotation.id, grants: rotation.grants, hosts: rotation.hosts }); }
fixture.mutations.length = 0;
const localAccounts = JSON.stringify(fixture.profiles.filter(profile => !profile.managed));
const server = await createServer({...testViteCache(import.meta.url), configFile: false, root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: fixture.url } } } });
let browser; let activePage;
try {
 await server.listen(); browser=await chromium.launch({headless:true});
 const context=await browser.newContext({viewport:{width:1440,height:1050}});
 await context.addInitScript(()=>{window.__copiedCodes=[];Object.defineProperty(navigator,'clipboard',{value:{writeText:async text=>{window.__copiedCodes.push(text);}}});window.__signInLinks=[];window.open=(url)=>{window.__signInLinks.push(String(url));return null;};});
 const page=await context.newPage(), errors=[]; activePage=page;page.on('pageerror',error=>errors.push(String(error)));
 const url=`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace#token=${fixture.token}`;
 await page.goto(url);
 await page.getByRole('combobox',{name:'Workspace',exact:true}).selectOption('alpha');
 await page.getByRole('combobox',{name:'Session',exact:true}).selectOption('alpha-session');
 await page.getByRole('textbox',{name:'Message',exact:true}).fill('draft preserved while editing settings');
 let finishOpeningRefresh;
 await page.route('**/RefreshSubscription', async route => {
  await new Promise(resolve => { finishOpeningRefresh = resolve; });
  await route.continue();
 });
 await page.getByRole('button',{name:'Settings',exact:true}).click();
 await expect(page.getByRole('tab',{name:'Subscriptions',exact:true})).toHaveAttribute('aria-selected','true');
 await expect(page.getByRole('heading',{name:'Settings',exact:true})).toBeVisible();
 const codex=page.getByRole('article',{name:'Saved Codex subscription',exact:true});
 await expect(codex.locator('.subscription-email')).toHaveText('codex-fixture@example.invalid');
 await expect(codex).toContainText('45% used');await expect(codex).toContainText('60% used');
 await expect(codex.getByRole('progressbar',{name:'5-hour limit used'})).toHaveAttribute('value','45');
 assert.equal(fixture.mutations.length,0,'opening settings mutated execution');
 await expect(codex.getByRole('button',{name:'Refreshing…',exact:true})).toBeDisabled();
 await expect.poll(()=>!!finishOpeningRefresh).toBe(true);
 finishOpeningRefresh();
 await expect(codex).toContainText('50% used');
 await page.unroute('**/RefreshSubscription');
 await codex.getByRole('button',{name:'Refresh usage',exact:true}).click();await expect(codex).toContainText('55% used');
 // With an account policy on, every saved subscription is in automatic switching by default; one switch takes an account out, and back.
 const switching=codex.getByRole('switch',{name:'Automatic switching',exact:true});
 await expect(switching).toBeChecked();await switching.uncheck();await expect(switching).not.toBeChecked();
 await expect.poll(()=>fixture.mutations.filter(m=>m.method==='SaveRunnerSettings').at(-1)?.body.rotationExcludedProfileIds).toEqual(['codex-saved']);
 // The Runner's answer, and the reload after it, keep the switch where it was put: checked again only once the save is done.
 await expect(switching).toBeEnabled();await expect(switching).not.toBeChecked();
 await switching.check();await expect(switching).toBeChecked();
 await expect.poll(()=>fixture.mutations.filter(m=>m.method==='SaveRunnerSettings').at(-1)?.body.rotationExcludedProfileIds??[]).toEqual([]);
 await expect(switching).toBeEnabled();await expect(switching).toBeChecked();
 const saves=fixture.mutations.filter(m=>m.method==='SaveRunnerSettings');assert.equal(saves.length,2);assert.ok(saves.every(m=>m.body.naming==='repositories'&&m.body.context),'a switch must not reset other Runner settings');
 assert.equal(fixture.mutations.length,2,'switching an account must not touch execution');
 await mkdir('.cache',{recursive:true});await page.screenshot({path:'.cache/settings-subscriptions.png',fullPage:true});
 await page.getByRole('button',{name:'Add subscription',exact:true}).click();
 // Codex signs in with the browser on this computer: no code, Codex receives the callback itself.
 let dialog=page.getByRole('dialog');await dialog.getByLabel('Account name',{exact:true}).fill('Work');
 await dialog.getByRole('button',{name:'Continue in your browser',exact:true}).click();
 await expect(dialog.getByText('Finish signing in in your browser',{exact:true})).toBeVisible();
 await expect(dialog).toContainText('Lazybox continues when you’re done.');await dialog.screenshot({path:'.cache/settings-codex-browser-sign-in.png'});
 await expect(dialog.getByRole('textbox',{name:'Sign-in code',exact:true})).toHaveCount(0);
 assert.equal(fixture.mutations.filter(m=>m.method==='AddSubscription').at(-1).body.signInMethod,'browser');
 const browserPage='https://auth.openai.com/oauth/authorize?state=fixture';
 await expect.poll(()=>page.evaluate(()=>window.__signInLinks)).toEqual([browserPage]);
 await dialog.getByRole('button',{name:'Open again',exact:true}).click();
 assert.deepEqual(await page.evaluate(()=>window.__signInLinks),[browserPage,browserPage]);
 let storage=await page.evaluate(()=>JSON.stringify(localStorage));assert.ok(!storage.includes('auth.openai.com'),'one-time auth data persisted');
 await page.reload();await page.getByRole('button',{name:'Settings',exact:true}).click();
 await page.getByRole('button',{name:'Continue sign-in',exact:true}).first().click();dialog=page.getByRole('dialog');
 await expect(dialog.getByText('Finish signing in in your browser',{exact:true})).toBeVisible();
 // Opening the dialog focuses its main action, never the title bar (no focus ring on the header).
 await expect(dialog.getByRole('button',{name:'Open again',exact:true})).toBeFocused();
 const id=[...fixture.enrollments.keys()][0];fixture.completeEnrollment(id);
 // Done in the browser: the dialog closes by itself and the subscription is listed.
 await expect(dialog).toBeHidden();
 await expect(page.getByRole('article',{name:'Work subscription',exact:true})).toContainText('73% used');
 // "Use a code instead" switches a waiting browser sign-in to a device code for the same account.
 const enrollmentOf=label=>[...fixture.enrollments.values()].filter(flow=>flow.label===label).at(-1);
 await page.getByRole('button',{name:'Add subscription',exact:true}).click();dialog=page.getByRole('dialog');
 await dialog.getByLabel('Account name',{exact:true}).fill('Phone');
 await dialog.getByRole('button',{name:'Continue in your browser',exact:true}).click();
 await expect(dialog.getByText('Finish signing in in your browser',{exact:true})).toBeVisible();
 const phoneBrowser=enrollmentOf('Phone');
 await dialog.getByRole('button',{name:'Use a code instead (for another device)',exact:true}).click();
 await expect(dialog.getByRole('textbox',{name:'Sign-in code',exact:true})).toHaveValue('AUTH-FIXTURE-CODE');
 assert.equal(phoneBrowser.state,'cancelled','the browser sign-in was left waiting');
 assert.equal(enrollmentOf('Phone').signInMethod,'device');
 await dialog.getByRole('button',{name:'Copy code',exact:true}).click();
 await expect(dialog.getByRole('button',{name:'Copied',exact:true})).toBeVisible();
 assert.deepEqual(await page.evaluate(()=>window.__copiedCodes),['AUTH-FIXTURE-CODE']);
 await expect.poll(()=>page.evaluate(()=>window.__signInLinks.at(-1))).toBe('https://auth.openai.com/codex/device');
 await dialog.getByRole('button',{name:'Open sign-in page',exact:false}).click();
 assert.equal((await page.evaluate(()=>window.__signInLinks)).filter(link=>link==='https://auth.openai.com/codex/device').length,2);
 storage=await page.evaluate(()=>JSON.stringify(localStorage));assert.ok(!storage.includes('AUTH-FIXTURE-CODE')&&!storage.includes('auth.openai.com'),'one-time auth data persisted');
 // The account refuses device codes: one line says why, and the browser is the way out.
 fixture.failEnrollment(enrollmentOf('Phone').id,'Device-code sign-in is off for this ChatGPT account. In a Business or Enterprise workspace, an admin turns it on.','device_sign_in_disabled');
 await expect(dialog).toContainText('Device-code sign-in is off for this ChatGPT account. In a Business or Enterprise workspace, an admin turns it on.');await dialog.screenshot({path:'.cache/settings-codex-device-refused.png'});
 await dialog.getByRole('button',{name:'Sign in with the browser instead',exact:true}).click();
 await expect(dialog.getByText('Finish signing in in your browser',{exact:true})).toBeVisible();
 assert.equal(enrollmentOf('Phone').signInMethod,'browser');
 // Cancel stops the sign-in and closes the dialog; nothing is added.
 await dialog.getByRole('button',{name:'Cancel',exact:true}).click();
 await expect(dialog).toBeHidden();
 assert.equal(enrollmentOf('Phone').state,'cancelled');
 await expect(page.getByRole('article',{name:'Phone subscription',exact:true})).toHaveCount(0);
 // Starting with a code from the form, then another Codex sign-in holding the callback port.
 await page.getByRole('button',{name:'Add subscription',exact:true}).click();dialog=page.getByRole('dialog');
 await dialog.getByLabel('Account name',{exact:true}).fill('Busy');await dialog.screenshot({path:'.cache/settings-codex-form.png'});
 await dialog.getByRole('button',{name:'Use a code instead (for another device)',exact:true}).click();
 await expect(dialog.getByRole('button',{name:'Sign in with the browser instead',exact:true})).toBeVisible();
 await dialog.getByRole('button',{name:'Sign in with the browser instead',exact:true}).click();
 await dialog.getByRole('button',{name:'Continue in your browser',exact:true}).click();
 await expect(dialog.getByText('Finish signing in in your browser',{exact:true})).toBeVisible();
 fixture.failEnrollment(enrollmentOf('Busy').id,'Another Codex sign-in is in progress on this Mac. Finish or close it, then try again.','sign_in_in_progress');
 await expect(dialog).toContainText('Another Codex sign-in is in progress on this Mac. Finish or close it, then try again.');
 await dialog.getByRole('button',{name:'Use a code instead',exact:true}).click();
 await expect(dialog.getByRole('textbox',{name:'Sign-in code',exact:true})).toHaveValue('AUTH-FIXTURE-CODE');
 await dialog.getByRole('button',{name:'Cancel',exact:true}).click();
 await expect(dialog).toBeHidden();
 await page.getByRole('button',{name:'Add subscription',exact:true}).click();dialog=page.getByRole('dialog');
 await dialog.getByLabel('Provider',{exact:true}).selectOption('claude');
 await dialog.getByLabel('Account name',{exact:true}).fill('Personal');
 await expect(dialog.getByLabel('Subscription token',{exact:true})).toHaveCount(0);
 await expect(dialog).not.toContainText('setup-token');
 await dialog.getByRole('button',{name:'Continue with Claude',exact:true}).click();
 await expect(dialog.getByText('Finish signing in in your browser',{exact:true})).toBeVisible();
 // Claude has no device code to switch to.
 await expect(dialog.getByRole('button',{name:'Use a code instead (for another device)',exact:true})).toHaveCount(0);
 assert.equal(fixture.mutations.filter(m=>m.method==='AddSubscription').at(-1).body.signInMethod||'','','Claude names no sign-in method');
 await dialog.getByRole('button',{name:'Open again',exact:true}).click();
 assert.equal((await page.evaluate(()=>window.__signInLinks)).at(-1),'https://claude.com/cai/oauth/authorize?state=fixture');
 fixture.completeEnrollment([...fixture.enrollments.values()].find(flow=>flow.label==='Personal').id);
 await expect(dialog).toBeHidden();
 const claude=page.getByRole('article',{name:'Personal subscription',exact:true});
 await expect(claude).toContainText('Usage not reported yet');await expect(claude.getByRole('progressbar')).toHaveCount(0);
 await claude.getByRole('button',{name:'Refresh usage',exact:true}).click();await expect(claude).toContainText('42% used');
 // Subscriptions group by provider, Claude first, each under a small header; available ones lead, then by name.
 const groups=page.locator('#subscriptions-panel .provider-group');
 await expect(groups).toHaveCount(2);
 await expect(groups.locator('.provider-group-heading')).toHaveText(['Claude','Codex']);
 await expect(groups.nth(0).locator('.subscription-card h3')).toHaveText(['Personal']);
 await expect(groups.nth(1).locator('.subscription-card h3')).toHaveText(['Saved Codex','Work']);
 await expect(groups.nth(0)).toHaveAttribute('aria-label','Claude');
 await page.screenshot({path:'.cache/settings-subscriptions-grouped.png',fullPage:true});
 await page.emulateMedia({colorScheme:'dark'});await page.waitForTimeout(400);await page.screenshot({path:'.cache/settings-subscriptions-grouped-dark.png',fullPage:true});await page.emulateMedia({colorScheme:'light'});
 // Hold each refresh independently: refreshing one card must not disable its sibling.
 const held = new Map();
 const savedCodexUsage = () => fixture.profiles.find(profile=>profile.id==='codex-saved').availability.windows[0].usedPercent;
 const beforeManualUsage = savedCodexUsage();
 const beforeRefreshes = fixture.calls.filter(call=>call.method==='RefreshSubscription').length;
 await page.route('**/RefreshSubscription', async route => {
  const profileId=route.request().postDataJSON().profileId;
  await new Promise(resolve=>held.set(profileId,resolve));
  if(profileId===fixture.profiles.find(p=>p.label==='Personal').id) await route.fulfill({status:503,contentType:'application/json',body:JSON.stringify({code:'unavailable',message:'Fixture unavailable'})});
  else await route.continue();
 });
 await codex.getByRole('button',{name:'Refresh usage',exact:true}).click();
 await expect(codex.getByRole('button',{name:'Refreshing…',exact:true})).toBeDisabled();
 await expect(claude.getByRole('button',{name:'Refresh usage',exact:true})).toBeEnabled();
 await expect(claude).toContainText('42% used');
 await claude.getByRole('button',{name:'Refresh usage',exact:true}).click();
 await expect(claude.getByRole('button',{name:'Refreshing…',exact:true})).toBeDisabled();
 await expect.poll(()=>held.size).toBe(2);
 held.get(fixture.profiles.find(p=>p.label==='Saved Codex').id)();
 await expect(codex.getByRole('progressbar',{name:'5-hour limit used'})).toHaveAttribute('value',String(beforeManualUsage+5));
 await expect(codex.getByRole('button',{name:'Refresh usage',exact:true})).toBeEnabled();
 await expect(claude.getByRole('button',{name:'Refreshing…',exact:true})).toBeDisabled();
 held.get(fixture.profiles.find(p=>p.label==='Personal').id)();
 await expect(claude.getByRole('alert')).toContainText('Usage could not be refreshed');
 await expect(claude).toContainText('42% used');
 await expect(claude.getByRole('button',{name:'Refresh usage',exact:true})).toBeEnabled();
 await expect(codex.getByRole('alert')).toHaveCount(0);
 assert.equal(fixture.calls.filter(call=>call.method==='RefreshSubscription').length,beforeRefreshes+1,'unexpected refresh target or duplicate request');
 await page.unroute('**/RefreshSubscription');
 const refreshCount = () => fixture.calls.filter(call=>call.method==='RefreshSubscription').length;
 const beforeReopen = refreshCount();
 await page.getByRole('tab',{name:'Appearance',exact:true}).click();
 assert.equal(refreshCount(),beforeReopen);
 await page.getByRole('tab',{name:'Subscriptions',exact:true}).click();
 await expect.poll(refreshCount).toBe(beforeReopen+3);
 await expect(codex.getByRole('progressbar',{name:'5-hour limit used'})).toHaveAttribute('value',String(beforeManualUsage+10));
 storage=await page.evaluate(()=>JSON.stringify(localStorage));assert.ok(!storage.includes('fixture-private-oauth-token'),'token persisted in client storage');
 // Lose a successful Claude Add acknowledgement, then recover by request key.
 let loseAck=true;
 await page.route('**/AddSubscription',async route=>{const response=await route.fetch();if(loseAck){loseAck=false;fixture.completeEnrollment([...fixture.enrollments.values()].find(flow=>flow.label==='Travel').id);await route.abort('failed');}else await route.fulfill({response});});
 await page.getByRole('button',{name:'Add subscription',exact:true}).click();dialog=page.getByRole('dialog');
 await dialog.getByLabel('Provider',{exact:true}).selectOption('claude');await dialog.getByLabel('Account name',{exact:true}).fill('Travel');
 await dialog.getByRole('button',{name:'Continue with Claude',exact:true}).click();
 await expect(dialog.getByRole('alert')).toContainText('Could not confirm');
 await page.reload();await page.getByRole('button',{name:'Settings',exact:true}).click();
 await expect(page.getByRole('article',{name:'Travel subscription',exact:true})).toBeVisible();
 await expect.poll(()=>fixture.calls.filter(c=>c.method==='GetSubscriptionEnrollment'&&c.body.requestKey).length).toBeGreaterThanOrEqual(1);
 // The recovered request was already done: nothing is left waiting, the next Add starts afresh.
 await page.getByRole('button',{name:'Add subscription',exact:true}).click();dialog=page.getByRole('dialog');
 await expect(dialog.getByLabel('Account name',{exact:true})).toHaveValue('');await dialog.getByRole('button',{name:'Close subscription dialog',exact:true}).click();
 await page.unroute('**/AddSubscription');
 // Explicit cancellation must not register another account.
 await page.getByRole('button',{name:'Add subscription',exact:true}).click();dialog=page.getByRole('dialog');
 await dialog.getByLabel('Provider',{exact:true}).selectOption('codex');await dialog.getByLabel('Account name',{exact:true}).fill('Cancelled');await dialog.getByRole('button',{name:'Continue in your browser',exact:true}).click();
 await expect(dialog.getByText('Finish signing in in your browser',{exact:true})).toBeVisible();await dialog.getByRole('button',{name:'Cancel',exact:true}).click();
 // Cancelling is what the person asked for: the dialog closes, with no dead-end "Sign-in cancelled" screen.
 await expect(dialog).toBeHidden();
 await expect(page.getByRole('article',{name:'Cancelled subscription',exact:true})).toHaveCount(0);
 // A sign-in that ran out of time: plain words, the dialog's normal type scale, the focus on Try again.
 await page.getByRole('button',{name:'Add subscription',exact:true}).click();dialog=page.getByRole('dialog');
 await dialog.getByLabel('Account name',{exact:true}).fill('Slow');await dialog.getByRole('button',{name:'Continue in your browser',exact:true}).click();
 await expect(dialog.getByText('Finish signing in in your browser',{exact:true})).toBeVisible();
 fixture.enrollments.delete(enrollmentOf('Slow').id);
 await expect(dialog.getByText('Sign-in timed out',{exact:true})).toBeVisible();
 await expect(dialog).toContainText('The sign-in took too long and was stopped. Try again when you’re ready.');
 await expect(dialog).not.toContainText('tracked');
 await expect(dialog.getByRole('heading')).toHaveCount(1);
 const titleSize=await dialog.locator('h2').evaluate(node=>parseFloat(getComputedStyle(node).fontSize));assert.ok(titleSize<=16,`dialog title is ${titleSize}px`);
 const statusSize=await dialog.locator('.enrollment-status').evaluate(node=>parseFloat(getComputedStyle(node).fontSize));assert.ok(statusSize<=14,`state line is ${statusSize}px`);
 await expect(dialog.getByRole('button',{name:'Try again',exact:true})).toBeFocused();
 await dialog.screenshot({path:'.cache/settings-subscription-expired.png'});
 await dialog.getByRole('button',{name:'Close',exact:true}).click();await expect(dialog).toBeHidden();
 await page.screenshot({path:'.cache/settings-subscriptions-added.png',fullPage:true});
 // Settings → Process shows the effective orchestration process, read-only: phases left to right, then role cards.
 const before=fixture.mutations.length;
 await page.getByRole('tab',{name:'Process',exact:true}).click();
 const processView=page.locator('.orchestration-process');
 await expect(processView.getByRole('heading',{name:'Process',exact:true})).toBeVisible();
 await expect(processView.locator('.process-verdict')).toHaveText('Valid · reviewed-team · autonomy 4 · $60 · 10h per mission');
 await expect(processView.locator('.process-phases li')).toHaveText([/plan.*contract \(human\)/,/build.*all_verified \(rule\)/,/integrate.*merged \(check\)/,/accept.*accepted \(human\)/]);
 await expect(processView.getByRole('article',{name:'coordinator role',exact:true})).toContainText('session.spawn:worker');
 await expect(processView.getByRole('article',{name:'verifier role',exact:true})).toContainText("not worker's");
 await expect(processView.getByRole('article',{name:'transition role',exact:true})).toContainText('on timer every 30m');
 await processView.getByRole('combobox',{name:'For workspace'}).selectOption({index:1});
 await expect(processView.locator('.process-layers')).toContainText('api/.lazybox/process.yaml');
 await expect(processView.locator('.process-issues')).toContainText('roles.worker.instructions.file');
 assert.equal(fixture.mutations.length,before,'reading the process changed something');
 assert.ok(await page.locator('.settings-page').evaluate(node=>node.scrollWidth<=node.clientWidth),'the process view overflows horizontally');
 await page.screenshot({path:'.cache/settings-process.png',fullPage:true});
 await page.getByRole('button',{name:'Work',exact:true}).click();
 await expect(page.getByRole('textbox',{name:'Message',exact:true})).toHaveValue('draft preserved while editing settings');
 assert.equal(fixture.calls.filter(c=>['OpenSession','OpenAssistant','Apply','CloseSession','StartCommand'].includes(c.method)).length,0,'settings started/stopped an agent or command');
 assert.equal(fixture.calls.filter(c=>c.method==='AddSubscription').length,10,'reload/lost acknowledgement duplicated account enrollment');
 assert.equal(JSON.stringify(fixture.profiles.filter(profile=>!profile.managed)),localAccounts,'Settings modified a local CLI reference');
 assert.equal(errors.length,0,errors.join('\n'));
 // Ten accounts remain compact and readable; long identities must not widen the page.
 const saved=fixture.profiles.find(profile=>profile.id==='codex-saved');
 for(let index=fixture.profiles.filter(profile=>profile.managed).length;index<10;index++) fixture.profiles.push({...structuredClone(saved),id:`density-${index}`,label:index===9?'A long subscription name for the engineering team':`Account ${index+1}`,availability:{...structuredClone(saved.availability),email:index===9?'long.account.identity@engineering.example.invalid':`account${index+1}@example.invalid`}});
 await page.reload();await page.getByRole('button',{name:'Settings',exact:true}).click();
 await expect(page.locator('.subscription-card')).toHaveCount(10);
 // The Codex group holds the added ones; it lays out like the whole list did.
 const cards=page.locator('.provider-group[data-provider="codex"] .subscription-card');await expect(cards).not.toHaveCount(0);
 const first=await cards.nth(0).boundingBox(),third=await cards.nth(2).boundingBox();
 assert.equal(first.y,third.y,'wide Settings view should fit three accounts per row');
 assert.ok(first.height<=240,`two-window card too tall: ${first.height}`);
 await page.screenshot({path:'.cache/settings-ten-subscriptions.png',fullPage:true});
 await page.setViewportSize({width:480,height:850});
 const narrowFirst=await cards.nth(0).boundingBox(),narrowSecond=await cards.nth(1).boundingBox();
 assert.ok(narrowSecond.y>narrowFirst.y,'narrow Settings view should use one column');
 assert.ok(await page.locator('.settings-page').evaluate(node=>node.scrollWidth<=node.clientWidth),'subscription identities cause horizontal overflow');
 await expect(page.getByRole('article',{name:'A long subscription name for the engineering team subscription',exact:true}).locator('.subscription-email')).toHaveText('long.account.identity@engineering.example.invalid');
 console.log('PASS: Settings enrollment (Codex browser sign-in, a code instead, device codes refused, a busy callback, cancel), per-account refresh, per-account automatic-switching opt-out, subscriptions grouped by provider, retained drafts, and compact ten-account layout with responsive wrapping. Fake authority only; no live login/inference.');
} catch (error) {if(activePage){await activePage.screenshot({path:'.cache/settings-test-failure.png',fullPage:true});console.error(error);if(await activePage.locator('.subscription-dialog').count())console.error((await activePage.locator('.subscription-dialog').innerText()).slice(0,1500));}throw error;} finally {await browser?.close();await server.close();await fixture.close();}
