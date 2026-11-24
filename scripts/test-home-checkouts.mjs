// Home checkouts against the fake Runner. The person gives a repository a home
// (its own clone outside the workspace) from the workspace's repositories
// dialog, updates it to the latest main, and opens a shell there in the
// Terminal panel (a real terminal). What the agent hands back — a login that
// needs a TTY, written as a fenced `sh` block with `env -u`, or "run in the
// home checkout of api: `make playground`" — runs there, exactly as written,
// in a shell already idle there or a new one, never sent to the agent; Paste
// types it without running. A shell that starts slowly gets nothing before
// its prompt: the command shows once and runs. When a handoff run ends, Lazybox reports its exit automatically. Everything is reachable from the keyboard.
// Settings → Repositories lists the homes.
// WebKit by default (the desktop app's engine); TEST_CHROMIUM=1 runs Chromium.
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from './fixtures/vite-cache.mjs';
import react from '@vitejs/plugin-react';
import {webkit, chromium, expect} from './fixtures/playwright.mjs';
import {fileURLToPath} from 'node:url';
import assert from 'node:assert/strict';
import {mkdir} from 'node:fs/promises';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import { collectPageErrors } from './fixtures/page-errors.mjs';

const fixture=await startRunnerFixture();
fixture.library.push({id:'infra',name:'infra-base',path:'/fixture/repos/infra-base',gitIdentity:'github:fixture-team/infra-base'});
fixture.homes.set('infra',{path:'/fixture/home/src/infra-base'});
const session=fixture.sessions.find(s=>s.id==='alpha-session');session.agents=[];session.status='ready';session.busy=false;
const channel=fixture.openEvents(session.id);
const push=event=>channel.push({turnId:'t1',...event});
const login='cd /tmp/api-main && env -u ACME_TOKEN ./bin/acme login staging';
push({type:'operation_accepted',text:'Add the playground target'});
push({type:'message_delta',text:'I ran `go test ./...` and it passes.'});
push({type:'message_delta',text:[
 'Done: the playground target is in.',
 '',
 'The staging sign-in opens a browser and needs a real terminal (TTY), which my commands do not have. Run this yourself:',
 '',
 '```sh',login,'```',
 '',
 'To try it, run in the home checkout of api: `make playground`, and `make stop` when done.',
 'The sandbox needs your cloud credentials: run in your home folder: `gcloud auth application-default login`',
].join('\n')});

const server=await createServer({...testViteCache(import.meta.url),configFile:false,root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
const calls=method=>fixture.calls.filter(call=>call.method===method);
const mutations=method=>fixture.mutations.filter(m=>m.method===method);
const applies=()=>fixture.mutations.filter(m=>m.method==='Apply'&&m.body.message);
const shots='.cache';
let browser;
try {
 await server.listen();await mkdir(shots,{recursive:true});
 browser=await (process.env.TEST_CHROMIUM?chromium:webkit).launch();
 const page=await (await browser.newContext({viewport:{width:1440,height:900}})).newPage();page.setDefaultTimeout(15000);
 const errors=[];await collectPageErrors(page, errors);
 await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`);
 const chat=page.locator('.panel-chat');
 await expect(chat.getByRole('textbox',{name:'Message',exact:true})).toBeVisible();
 const menu=page.locator('.context-menu[data-depth="0"]');
 const labels=()=>menu.locator('.context-menu-label').allTextContents();

 // ── The agent's last message hands three commands back, each exactly as written; earlier words offer nothing.
 const offers=chat.getByRole('group',{name:'Commands for you to run'});
 await expect(offers).toHaveCount(1);
 await expect(offers.locator('.home-run')).toHaveCount(3);
 // No home yet: the repository's offers say they clone one first; the global login runs in the home folder.
 await expect(offers.locator('.home-run').nth(0)).toHaveText(`Create api home and run${login}`);
 await expect(offers.locator('.home-run').nth(1)).toHaveText('Create api home and runmake playground');
 await expect(offers.locator('.home-run').nth(2)).toHaveText('Run in your home foldergcloud auth application-default login');
 // The login block needs a terminal: Run opens one; asking Claude or capturing its output is not offered.
 const loginBlock=chat.locator('.code-block.is-runnable').filter({hasText:'./bin/acme login staging'});
 await expect(loginBlock.locator('.code-run-tag')).toHaveText('needs a terminal');
 await loginBlock.getByRole('button',{name:'More ways to run'}).click();
 const loginItems=await labels();
 assert.deepEqual(loginItems.slice(0,2),['Run in the terminal','Paste in terminal (don’t run)']);
 assert.ok(loginItems.includes('Open in Terminal (your app)')&&loginItems.includes('Copy command'),`login menu: ${loginItems}`);
 assert.ok(!loginItems.some(l=>/Ask Claude|command output/.test(l)),`a login is never sent to the agent nor captured: ${loginItems}`);
 await page.keyboard.press('Escape');
 assert.equal(calls('OpenTerminal').length,0,'showing offers starts nothing');

 // ── Set a home: the workspace's repositories dialog lists each member's home.
 await page.getByRole('button',{name:'2 repositories',exact:true}).click();
 const dialog=page.getByRole('dialog',{name:'Repositories in Alpha feature'});
 const apiRow=dialog.getByRole('listitem',{name:'api home checkout'});
 await expect(apiRow).toContainText('No home checkout');
 await page.screenshot({path:`${shots}/home-checkouts-dialog-empty.png`});
 await apiRow.getByRole('button',{name:/^Create at /}).click();
 await expect.poll(()=>mutations('SetRepositoryHome').length).toBe(1);
 assert.deepEqual(mutations('SetRepositoryHome')[0].body,{repositoryId:'backend',action:'create'},'created at the suggested place');
 await expect(apiRow).toContainText('/fixture/home/lazybox/repos/fixture-team/backend');
 await expect(apiRow).toContainText('main · clean');
 // Using an existing folder needs an absolute path; a wrong one is said in words.
 const webRow=dialog.getByRole('listitem',{name:'web home checkout'});
 await webRow.getByRole('button',{name:'Use an existing folder…'}).click();
 await webRow.getByLabel('Existing checkout of frontend').fill('relative/web');
 await webRow.getByLabel('Existing checkout of frontend').press('Enter');
 await expect(webRow.getByRole('alert')).toContainText('A home checkout needs an absolute path.');
 await webRow.getByLabel('Existing checkout of frontend').fill('~/src/web');
 await webRow.getByRole('button',{name:'Use folder',exact:true}).click();
 await expect(webRow).toContainText('/fixture/home/src/web');

 // ── Update to latest main: behind origin, one click fast-forwards; with changes, it says why and touches nothing.
 fixture.homes.get('backend').behind=3;
 await page.evaluate(()=>window.dispatchEvent(new Event('focus')));
 await expect(apiRow).toContainText('3 behind origin/main');
 await apiRow.getByRole('button',{name:'Update to latest main'}).click();
 await expect.poll(()=>mutations('UpdateRepositoryHome').length).toBe(1);
 await expect(apiRow).toContainText('main · clean');
 await expect(apiRow).not.toContainText('behind');
 await page.screenshot({path:`${shots}/home-checkouts-dialog.png`});
 fixture.homes.get('backend').dirty=2;
 // ── Keyboard: the row's ⋯ opens its menu with the first item focused; Finder is one arrow away.
 await apiRow.getByRole('button',{name:'More for api home'}).focus();
 await page.keyboard.press('Enter');
 await expect(menu).toBeVisible();
 assert.deepEqual((await labels()).slice(0,3),['Open in terminal',/Mac/.test(await page.evaluate(()=>navigator.platform))?'Open in Finder':'Open in file manager','Open in VS Code']);
 assert.ok((await labels()).includes('Update to latest main'));
 await page.keyboard.press('ArrowDown');await page.keyboard.press('Enter');
 await expect.poll(()=>mutations('OpenRepositoryHome').length).toBe(1);
 assert.deepEqual(mutations('OpenRepositoryHome')[0].body,{repositoryId:'backend',app:'finder'});
 await apiRow.getByRole('button',{name:'More for api home'}).click();
 await menu.getByRole('menuitem',{name:'Update to latest main'}).click();
 await expect(apiRow.getByRole('alert')).toContainText('Not updated: main has uncommitted changes');
 fixture.homes.get('backend').dirty=0;

 // ── Open the home in the shell panel: the dialog steps aside, the shell starts there, focused.
 await apiRow.getByRole('button',{name:'Open in terminal',exact:true}).click();
 await expect(dialog).toHaveCount(0);
 const panel=page.locator('.panel-terminal');
 const screen=panel.getByTestId('terminal');
 await expect(panel.getByRole('tab',{name:'api home',exact:true})).toHaveAttribute('aria-selected','true');
 await expect(screen).toContainText('fixture shell in /fixture/home/lazybox/repos/fixture-team/backend');
 assert.equal(calls('OpenTerminal').length,1,'one shell, no extra workspace shell');
 assert.equal(calls('OpenTerminal')[0].body.repositoryHome,'backend');
 await expect(screen.locator('textarea')).toBeFocused();
 await page.screenshot({path:`${shots}/home-checkouts-shell.png`});
 const directories=await panel.getByLabel('Terminal directory').locator('option').allTextContents();
 assert.ok(directories.includes('api home')&&directories.includes('web home')&&directories.includes('Home folder (~)'),`directories: ${directories}`);

 // ── The login that needs a TTY: Run in api home runs it in the api home shell just opened, idle at its prompt: no second shell.
 const first=[...fixture.terminals.values()].at(-1);
 const loginOffer=offers.locator('.home-run').nth(0);
 await expect(loginOffer).toHaveText(`Run in api home${login}`);
 await loginOffer.click();
 // The temporary checkout's cd is gone (the home stands for it); env -u and every argument are kept; the prompt's line is cleared first.
 await expect.poll(()=>first.input.join('')).toBe('\x15env -u ACME_TOKEN ./bin/acme login staging\r');
 assert.equal(calls('OpenTerminal').length,1,'the idle shell in api home is reused');
 await expect(screen).toContainText('ran: env -u ACME_TOKEN ./bin/acme login staging');
 const loginStatus=offers.locator('.home-run-row').nth(0).getByRole('status').first();
 await expect(loginStatus).toHaveText('Finished in api home · exit 0');
 await expect(screen.locator('textarea')).toBeFocused();
 await page.screenshot({path:`${shots}/home-checkouts-run.png`});
 // The exit reaches the conversation without another click.
 await expect.poll(()=>applies().length).toBe(1);
 assert.equal(applies()[0].body.message.text,'`env -u ACME_TOKEN ./bin/acme login staging` completed successfully.');
 await expect(offers.locator('.home-run-row').nth(0)).toContainText('Told Claude');
 await page.screenshot({path:`${shots}/home-checkouts-told.png`});

 // ── Paste (don't run): typed at the prompt of the same shell, waiting for the person's Return.
 await offers.getByRole('button',{name:`Other ways to run ${login}`}).click();
 await menu.getByRole('menuitem',{name:'Paste in terminal (don’t run)'}).click();
 await expect.poll(()=>first.line).toBe('env -u ACME_TOKEN ./bin/acme login staging');
 assert.equal(calls('OpenTerminal').length,1,'pasted into the idle shell');
 await expect(loginStatus).toHaveText('Typed in the terminal (api home): review and press Return');
 await page.waitForTimeout(400);
 assert.equal(first.line,'env -u ACME_TOKEN ./bin/acme login staging','typed, not run');
 // The person runs it in the terminal; the shell marks its end, and the offer says so.
 await screen.click();await page.keyboard.press('Enter');
 await expect(loginStatus).toHaveText('Finished in api home · exit 0');
 await expect(offers.locator('.home-run-row').nth(0).getByRole('button',{name:'Tell Claude it’s done'})).toBeVisible();
 assert.equal(applies().length,1,'an editable pasted command only reports after manual confirmation');

 // ── The offer's ⌄: paste, another home, the person's own terminal, Copy.
 await offers.getByRole('button',{name:'Other ways to run make playground'}).click();
 const other=await labels();
 assert.ok(other.includes('Paste in terminal (don’t run)')&&other.includes('Run in web home')&&other.includes('Run in your home folder')&&other.includes('Open in Terminal (your app)')&&other.includes('Copy command'),`offer menu: ${other}`);
 await page.keyboard.press('Escape');

 // ── From the keyboard: ⌥Tab (WebKit) or Tab reaches the next offer, Enter opens the home folder's shell and runs it there.
 const runLogin=offers.getByRole('button',{name:/^Run in your home folder /});
 await offers.locator('.home-run').nth(1).focus();
 const tab=process.env.TEST_CHROMIUM?'Tab':'Alt+Tab';
 await page.keyboard.press(tab);await page.keyboard.press(tab);
 await expect(runLogin).toBeFocused();
 await page.keyboard.press('Enter');
 await expect.poll(()=>calls('OpenTerminal').length).toBe(2);
 assert.equal(calls('OpenTerminal')[1].body.userHome,true);
 const homeShell=[...fixture.terminals.values()].at(-1);
 await expect.poll(()=>homeShell.input.join('')).toBe('gcloud auth application-default login\r');
 await expect(panel.getByRole('tab',{name:'~',exact:true})).toHaveAttribute('aria-selected','true');
 // Run again: the shell there is idle, so it is the one used; no third tab.
 const homeRun=offers.locator('.home-run-row').nth(2);
 await expect(homeRun).toContainText('Told Claude');
 assert.equal(applies().length,2,'gcloud completion is sent without clicking Tell');
 assert.equal(applies().at(-1).body.message.text,'`gcloud auth application-default login` completed successfully.');
 let failedReportId;
 await page.route('**/Apply',async route=>{
  const body=route.request().postDataJSON();
  if (!failedReportId) {
   failedReportId=body.operationId;
   await route.fulfill({status:400,json:{code:'failed_precondition',message:'Fixture report could not be sent'}});
  } else {
   assert.equal(body.operationId,failedReportId,'the manual retry cannot deliver a duplicate report');
   await route.continue();
  }
 });
 await runLogin.click();
 await expect.poll(()=>homeShell.input.join('')).toBe('gcloud auth application-default login\r\x15gcloud auth application-default login\r');
 await homeRun.getByRole('button',{name:'Tell Claude it’s done'}).click();
 await expect(homeRun).toContainText('Told Claude');
 await page.unroute('**/Apply');
 assert.equal(applies().length,3,'one completion report per run');
 assert.equal(calls('OpenTerminal').length,2,'the second Run reuses the idle shell');
 await expect(panel.getByRole('tab',{name:'~',exact:true})).toHaveCount(1);

 // ── A shell that starts slowly (a heavy .zshrc): Run waits for its prompt, so the command shows once, and runs.
 fixture.terminalStartDelayMs=1500;
 push({turnId:'t2',type:'operation_accepted',text:'And the seed data?'});
 push({turnId:'t2',type:'message_delta',text:'Seed it yourself:\n\n```sh\nmake seed\n```\n\nOr keep the tests running:\n\n```sh\nwatch make test\n```\n'});
 const block=chat.locator('.code-block.is-runnable').filter({hasText:'make seed'}).last();
 const watcher=chat.locator('.code-block.is-runnable').filter({hasText:'watch make test'}).last();
 await expect(block).toContainText('make seed');
 await block.getByRole('button',{name:'More ways to run'}).click();
 const blockItems=await labels();
 assert.deepEqual(blockItems.slice(0,5),['Run in the terminal','Paste in terminal (don’t run)','Run in api home','Run in web home','Run in your home folder'],`block menu: ${blockItems}`);
 assert.ok(blockItems.includes('Ask Claude to run it (no terminal)')&&blockItems.includes('Run as command output (no terminal)…'),'asking the agent is there, last and labelled');
 await page.keyboard.press('Escape');
 await block.getByRole('button',{name:'Run in the terminal',exact:true}).click();
 await expect.poll(()=>calls('OpenTerminal').length).toBe(3);
 assert.deepEqual([calls('OpenTerminal')[2].body.repositoryHome,calls('OpenTerminal')[2].body.userHome],[undefined,undefined],'the workspace, not a home');
 const slow=[...fixture.terminals.values()].at(-1);
 await expect(block.locator('.code-run-chip').first()).toHaveText('Waiting for the shell (the workspace)…');
 await page.waitForTimeout(800);
 assert.deepEqual(slow.input,[],'nothing is written while the shell starts');
 await expect.poll(()=>slow.input.join(''),{timeout:10000}).toBe('make seed\r');
 const output=slow.output.toString();
 const prompt=output.indexOf('\x1b]133;B');
 assert.ok(prompt>=0&&output.indexOf('make seed')>prompt,'the command reached the prompt, not the starting terminal');
 await expect(screen).toContainText('ran: make seed');
 assert.equal((await screen.innerText()).split('make seed').length-1,2,'the command shows once, then its output');
 await expect(block.locator('.code-run-chip').first()).toHaveText('Finished in the workspace · exit 0');
 assert.equal(applies().length,3,'ordinary workspace commands send nothing to the agent');
 fixture.terminalStartDelayMs=0;

 // ── A busy shell is never typed into: the watcher keeps the workspace shell running, so the next Run opens another.
 await watcher.getByRole('button',{name:'Run in the terminal',exact:true}).click();
 await expect.poll(()=>slow.input.join('')).toBe('make seed\r\x15watch make test\r');
 assert.equal(calls('OpenTerminal').length,3,'the idle workspace shell runs the watcher');
 await expect(watcher.locator('.code-run-chip').first()).toHaveText('Running in the terminal (the workspace)');
 await block.getByRole('button',{name:'Run in the terminal',exact:true}).click();
 await expect.poll(()=>calls('OpenTerminal').length).toBe(4);
 await expect.poll(()=>[...fixture.terminals.values()].at(-1).input.join('')).toBe('make seed\r');
 await page.screenshot({path:`${shots}/home-checkouts-slow-shell.png`});
 // A later message keeps earlier handoffs available.
 await expect(offers).toHaveCount(2);
 await expect(offers.last().locator('.home-run').first()).toHaveText(/Run in api home\s*make seed/);

 // A named checkout also routes the fence's own Run button. A compound
 // login must wait for Return so the Git steps cannot execute on click.
 const updateAndLogin = 'git fetch origin main && git switch main && env -u ACME_TOKEN bin/acme login staging';
 push({turnId:'t3',type:'operation_accepted',text:'Can I sign in from infra-base?'});
 push({turnId:'t3',type:'message_delta',text:['Run in the home checkout of infra-base:', '', '```sh', updateAndLogin, '```'].join('\n')});
 const addressed=chat.locator('.code-block.is-runnable').filter({hasText:updateAndLogin}).last();
 const openCount=calls('OpenTerminal').length;
 await addressed.getByRole('button',{name:'Review in infra-base home',exact:true}).click();
 await expect(addressed.getByRole('status').first()).toHaveText('Typed in the terminal (infra-base home): review and press Return');
 assert.equal(calls('OpenTerminal').length,openCount+1);
 assert.equal(calls('OpenTerminal').at(-1).body.repositoryHome,'infra','resolved the named registered home outside the workspace');
 const addressedShell=[...fixture.terminals.values()].at(-1);
 await expect.poll(()=>addressedShell.line).toBe(updateAndLogin);
 assert.ok(!addressedShell.input.join('').includes('\r'),'no Return was sent');
 assert.equal(applies().length,3,'the pasted login did not send a completion report');

 const longCommand = `bash -o pipefail -c 'printf "%s" ${'x'.repeat(300)} | jq "length > 0"'`;
 push({turnId:'t4',type:'operation_accepted',text:'Check the inventory'});
 push({turnId:'t4',type:'message_delta',text:'Run in the home checkout of infra-base: `'+longCommand+'`'});
 const longOffer=chat.locator('.home-run').filter({hasText:longCommand});
 await expect(longOffer).toContainText('Run in infra-base home');
 await longOffer.click();
 await expect.poll(()=>[...fixture.terminals.values()].at(-1).input.join('')).toContain(longCommand+'\r');
 assert.equal(calls('OpenTerminal').at(-1).body.repositoryHome,'infra');
 push({turnId:'t5',type:'operation_accepted',text:'Check another repository'});
 push({turnId:'t5',type:'message_delta',text:'Run in the home checkout of absent-repo: `make inspect`'});
 await expect(chat.getByRole('button',{name:/No checkout for absent-repo/})).toBeDisabled();

 push({turnId:'t6',type:'operation_accepted',text:'How do I verify it?'});
 const commands=['make step-one','make fail','make step-two','make step-three'];
 push({turnId:'t6',type:'message_delta',text:commands.map(c=>'Run in the home checkout of api: `'+c+'`').join('\n')});
 const sequence=offers.filter({hasText:'make step-one'});
 await sequence.getByRole('button',{name:'Select commands…'}).click();
 for(const c of [commands[3],commands[0],commands[2]]) await sequence.getByRole('checkbox',{name:'Include '+c,exact:true}).check();
 await sequence.getByRole('button',{name:'Run selected in order (3)'}).click();
 await expect(sequence.getByRole('status').filter({hasText:'Finished 3 commands'})).toBeVisible();
 const writes=calls('WriteTerminal').map(m=>Buffer.from(m.body.data,'base64').toString().replace(/^\x15/,'').trim()).filter(c=>commands.includes(c));
 assert.deepEqual(writes,[commands[0],commands[2],commands[3]]);
 await sequence.getByRole('button',{name:'Select commands…'}).click();
 for(const c of commands) await sequence.getByRole('checkbox',{name:'Include '+c,exact:true}).check();
 const beforeFailure=calls('WriteTerminal').length;
 await sequence.getByRole('button',{name:'Run selected in order (4)'}).click();
 await expect(sequence.getByRole('status').filter({hasText:'Stopped at command 2 · exit 1'})).toBeVisible();
 assert.equal(calls('WriteTerminal').length,beforeFailure+2,'later steps were never written to the terminal');

 await expect(sequence.getByRole('checkbox',{name:'Include make step-one',exact:true})).not.toBeChecked();

 push({turnId:'t7',type:'operation_accepted',text:'What should I run next?'});
 push({turnId:'t7',type:'message_delta',text:'Run in your home folder: `watch make check`\nRun in your home folder: `make after-watch`'});
 const watching=offers.filter({hasText:'make after-watch'});
 await watching.getByRole('button',{name:'Select commands…'}).click();
 for(const box of await watching.getByRole('checkbox').all()) await box.check();
 await watching.getByRole('button',{name:'Run selected in order (2)'}).click();
 const watched=()=>[...fixture.terminals.values()].find(t=>t.input.join('').includes('watch make check'));
 await expect.poll(()=>watched()?.running).toBe(true);
 await watching.getByRole('button',{name:'Cancel remaining commands'}).click();
 const terminal=watched();
 fixture.terminalOutput(terminal,'\x1b]133;D;0\x07$ \x1b]133;B\x07');
 await expect(watching.getByRole('status').filter({hasText:'Finished in your home folder'})).toBeVisible();
 assert.ok(![...fixture.terminals.values()].some(t=>t.input.join('').includes('make after-watch')));

 // ── Settings → Repositories lists every home with its state.
 await page.getByRole('button',{name:'Settings',exact:true}).click();
 await page.getByRole('tab',{name:'Repositories',exact:true}).click();
 const settings=page.locator('.home-checkouts-settings');
 await expect(settings.getByRole('listitem',{name:'backend home checkout'})).toContainText('main · clean');
 await expect(settings.getByRole('listitem',{name:'frontend home checkout'})).toContainText('/fixture/home/src/web');
 await settings.scrollIntoViewIfNeeded();
 await page.screenshot({path:`${shots}/home-checkouts-settings.png`});
 assert.deepEqual(errors,[],'no page errors');
 console.log('home checkouts: set and updated, opened in the shell, an agent\'s login run exactly in its home (the idle shell reused, never sent to the agent), pasted without running, told done, a slow shell waited for (the command shown once, and run), a busy shell left alone, keyboard reachable, listed in Settings');
} finally {
 await browser?.close();await server.close();await fixture.close();
}
