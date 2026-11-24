// Fixture only (CLAUDE_COMPLIANCE.md): no key, sign-in or provider is real.
// State the migration carried over (Claude sign-in kept on for saved Claude
// accounts, one of them imported from a token, which can be removed); the
// one-time notices; Settings → Agents & workspaces → Claude: how Claude
// agents sign in, an Anthropic API key saved and forgotten (a subscription
// token refused, the key never shown again), Claude sign-in in Lazybox off by
// default and what that changes in Subscriptions, Install Claude Code in the
// shell panel; and Subscription rotation, the extension that turns automatic
// switching on, with its warning for Claude.
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from 'vite';import react from '@vitejs/plugin-react';import {webkit,expect} from './fixtures/playwright.mjs';import {fileURLToPath} from 'node:url';import {mkdir} from 'node:fs/promises';import assert from 'node:assert/strict';import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import { collectPageErrors } from './fixtures/page-errors.mjs';
const f=await startRunnerFixture();
// Someone for whom both changes matter, as the migration leaves them: switching was on (kept, through the
// extension), and Claude subscriptions were saved in Lazybox (kept working: sign-in carried over on), one imported from a token.
Object.assign(f.runnerSettings,{notices:['automatic-switching-moved','claude-sign-in-kept'],claudeSubscriptionSignIn:true,claudeSignInCarriedOver:true});
f.profiles.push({id:'claude-work',provider:'claude',label:'Work',managed:true,isolation:'environment',availability:{state:'available'}},{id:'claude-old',provider:'claude',label:'Old',managed:true,isolation:'environment',tokenImported:true,availability:{state:'available'}});
const KEY='sk-ant-api03-fixture-not-a-real-key-abcd';
const server=await createServer({...testViteCache(import.meta.url),configFile:false,logLevel:'error',root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:f.url}}}});let browser;
const saves=()=>f.mutations.filter(m=>m.method==='SaveRunnerSettings').map(m=>m.body);
try{
 await server.listen();await mkdir('.cache',{recursive:true});browser=await webkit.launch();
 const page=await browser.newPage({viewport:{width:1360,height:900},locale:'en-US'});page.setDefaultTimeout(15000);
 const errors=[];await collectPageErrors(page, errors);
 const base=`http://127.0.0.1:${server.httpServer.address().port}`;
 await page.goto(`${base}/?view=workspace&collection=alpha#token=${f.token}`);

 // 1. The notices: what stayed on and where to review it; Got it dismisses one for good.
 const moved=page.getByRole('status',{name:'Automatic switching is now an extension'});
 const kept=page.getByRole('status',{name:'Lazybox now follows Anthropic’s terms for Claude sign-in'});
 await expect(moved).toContainText('Your conversations still switch subscription at a limit, as before');
 await expect(kept).toContainText('Your saved Claude accounts keep working; review in Settings → Agents → Claude.');
 await expect(kept.getByRole('link',{name:'Anthropic’s terms'})).toHaveAttribute('href','https://code.claude.com/docs/en/legal-and-compliance');
 await page.screenshot({path:'.cache/claude-compliance-notices.png'});
 await moved.getByRole('button',{name:'Got it'}).click();
 await expect(moved).toHaveCount(0);
 await expect.poll(()=>saves().at(-1)?.notices).toEqual(['claude-sign-in-kept']);
 await kept.getByRole('button',{name:'Review Claude settings'}).click();
 await expect(page.getByRole('tab',{name:'Agents & workspaces'})).toHaveAttribute('aria-selected','true');

 // 2. How Claude agents sign in: the person's own Claude Code sign-in, said as unconfirmed.
 const claude=page.locator('section.claude-settings');
 await expect(claude.getByRole('heading',{name:'Claude'})).toBeVisible();
 await expect(claude.locator('.claude-auth-mode')).toHaveText('Your Claude Code sign-in');
 await expect(claude).toContainText('Anthropic has not confirmed');
 await expect(claude).toContainText('2.1.311');

 // 3. An API key: a subscription token is refused; a key is saved, shown only by its last four characters, and forgotten.
 const field=claude.getByRole('textbox',{name:'Use an Anthropic API key'});
 await expect(field).toHaveAttribute('type','password');
 await field.fill('sk-ant-oat01-fixture-subscription');
 await claude.getByRole('button',{name:'Save key'}).click();
 await expect(claude.getByRole('alert')).toContainText('subscription token, not an API key');
 assert.equal(f.claudeAuth.key,'','a subscription token was kept');
 await field.fill(KEY);
 await claude.getByRole('button',{name:'Save key'}).click();
 await expect(claude.locator('.claude-auth-mode')).toHaveText('Anthropic API key ending abcd');
 await expect(claude.getByRole('status').filter({hasText:'Saved. New Claude conversations use this key.'})).toBeVisible();
 await expect(claude.getByRole('textbox',{name:'Use an Anthropic API key'})).toHaveCount(0);
 await expect(claude).toContainText('sk-ant-…abcd');
 assert.ok(!(await page.content()).includes(KEY),'the key is shown again');
 assert.equal(f.claudeAuth.key,KEY);
 await page.screenshot({path:'.cache/claude-compliance-api-key.png'});
 await claude.getByRole('button',{name:'Forget key'}).click();
 await expect(claude.locator('.claude-auth-mode')).toHaveText('Your Claude Code sign-in');
 await expect(claude.getByRole('textbox',{name:'Use an Anthropic API key'})).toBeVisible();
 assert.equal(f.claudeAuth.key,'');

 // 4. Claude sign-in in Lazybox, carried over on for the saved accounts and said so; turned off,
 // Subscriptions offers no Claude sign-in, and a saved Claude subscription says it is not used.
 const signIn=claude.getByRole('switch',{name:'Claude sign-in in Lazybox'});
 await expect(signIn).toBeChecked();
 await expect(claude.getByRole('note')).toContainText('Kept on so the Claude accounts you had saved keep working.');
 await signIn.uncheck();
 await expect.poll(()=>saves().at(-1)?.claudeSubscriptionSignIn??false).toBe(false);
 await expect(signIn).not.toBeChecked();
 await page.getByRole('tab',{name:'Subscriptions'}).click();
 // The account imported from a token: marked, and removed on request.
 const old=page.getByRole('article',{name:'Old subscription'});
 await expect(old).toContainText('Imported token; not recommended');
 await old.getByRole('button',{name:'Remove'}).click();
 const confirm=page.getByRole('dialog',{name:'Remove Old?'});
 await expect(confirm).toContainText('deletes its saved credential');
 await confirm.getByRole('button',{name:'Remove'}).click();
 await expect.poll(()=>f.mutations.findLast(m=>m.method==='RemoveSubscription')?.body).toEqual({profileId:'claude-old'});
 await expect(old).toHaveCount(0);
 await expect(page.getByRole('article',{name:'Work subscription'})).toContainText('Not used: Claude sign-in in Lazybox is off');
 await page.getByRole('button',{name:'Add subscription',exact:true}).click();
 const add=page.getByRole('dialog',{name:'Connect a subscription'});
 await add.getByLabel('Provider',{exact:true}).selectOption('claude');
 await add.getByLabel('Account name',{exact:true}).fill('Home');
 await expect(add.getByRole('note')).toContainText('Lazybox does not offer a Claude sign-in');
 await expect(add.getByRole('button',{name:'Continue with Claude'})).toBeDisabled();
 await page.screenshot({path:'.cache/claude-compliance-add-gated.png'});
 await add.getByRole('button',{name:'Agents & workspaces → Claude'}).click();
 await expect(add).not.toBeVisible();
 await expect(page.getByRole('tab',{name:'Agents & workspaces'})).toHaveAttribute('aria-selected','true');
 assert.equal(f.mutations.filter(m=>m.method==='AddSubscription').length,0,'a Claude sign-in started');
 // Turned on: saved at once, and said plainly.
 await signIn.check();
 await expect.poll(()=>saves().at(-1)?.claudeSubscriptionSignIn).toBe(true);
 await expect(claude.getByRole('note')).toHaveText('Claude sign-in in Lazybox is on. Anthropic has not confirmed that this is allowed; an API key is the way it documents.');
 await signIn.uncheck();
 await expect.poll(()=>saves().at(-1)?.claudeSubscriptionSignIn??false).toBe(false);

 // 5. Install Claude Code (a release without it, nothing on the PATH): Anthropic's installer, typed into a shell the person watches.
 f.availability.claude.installed=false;
 // The section reads where Claude Code is each time it is shown.
 await page.getByRole('tab',{name:'Subscriptions'}).click();
 await page.getByRole('tab',{name:'Agents & workspaces'}).click();
 await claude.getByRole('button',{name:'Check again'}).click();
 await expect(claude).toContainText('curl -fsSL https://claude.ai/install.sh | bash');
 await claude.getByRole('button',{name:'Install Claude Code'}).click();
 await expect.poll(()=>f.mutations.filter(m=>m.method==='OpenTerminal').length).toBe(1);
 const terminal=[...f.terminals.values()].at(-1);
 assert.equal(terminal.collectionId,'alpha');
 await expect.poll(()=>terminal.input.join('')).toBe('curl -fsSL https://claude.ai/install.sh | bash\r');
 await expect(page.locator('.panel-terminal, [data-panel-kind="terminal"]').first()).toBeVisible();
 await expect(page.getByText('ran: curl -fsSL https://claude.ai/install.sh | bash').first()).toBeVisible();
 await page.screenshot({path:'.cache/claude-compliance-install.png'});
 f.availability.claude.installed=true;

 // 6. Subscription rotation: added and allowed in Settings → Extensions, with the warning for Claude; then automatic switching shows.
 await page.getByRole('button',{name:'Settings',exact:true}).click();
 await page.getByRole('tab',{name:'Subscriptions'}).click();
 await expect(page.getByRole('switch',{name:'Automatic switching'})).toHaveCount(0);
 await page.getByRole('tab',{name:'Extensions'}).click();
 const examples=page.getByRole('list',{name:'Example extensions'});
 await examples.getByRole('listitem').filter({hasText:'Subscription rotation'}).getByRole('button',{name:'Add'}).click();
 const sheet=page.getByRole('dialog',{name:'Allow Subscription rotation?'});
 await expect(sheet).toContainText('It has no panel and runs no code: Lazybox follows its account policy.');
 await expect(sheet.getByRole('heading',{name:'At a limit'})).toBeVisible();
 await expect(sheet).toContainText('another of your signed-in accounts of the same provider');
 await expect(sheet.getByRole('list',{name:'What it can do'})).toContainText('Move Claude conversations to another of your Claude accounts');
 await expect(sheet.getByRole('note')).toContainText('Anthropic’s terms may not allow moving between Claude subscriptions');
 await expect(sheet.getByRole('link',{name:'Anthropic’s terms'})).toHaveAttribute('href','https://code.claude.com/docs/en/legal-and-compliance');
 await page.screenshot({path:'.cache/claude-compliance-rotation-sheet.png'});
 await sheet.getByRole('button',{name:'Allow and turn on'}).click();
 await expect(sheet).toHaveCount(0);
 const card=page.getByRole('list',{name:'Your extensions'}).getByRole('listitem',{name:'Subscription rotation'});
 await expect(card).toContainText('On');
 await expect(card.getByRole('note')).toContainText('Anthropic’s terms may not allow');
 assert.deepEqual(f.mutations.findLast(m=>m.method==='ApproveExtension').body.grants,['switch:codex','switch:claude']);
 await page.getByRole('tab',{name:'Subscriptions'}).click();
 await page.evaluate(()=>window.dispatchEvent(new Event('lazybox-settings-saved')));
 await expect(page.getByRole('article',{name:'Saved Codex subscription'}).getByRole('switch',{name:'Automatic switching'})).toBeChecked();
 assert.deepEqual(errors,[]);
 console.log('PASS notices of what the migration kept (shown, dismissed for good, open where to review it), Claude sign-in carried over on and turned off, an imported-token account marked and removed, Claude sign-in line, an API key saved by its last four characters and forgotten (a subscription token refused), Claude sign-in in Lazybox off by default (no Claude sign-in offered, saved Claude subscriptions marked unused) and on with a warning, Install Claude Code in the shell, Subscription rotation allowed with its Claude warning turning automatic switching on. Fixture only.');
}finally{await browser?.close();await server.close();await f.close();}
