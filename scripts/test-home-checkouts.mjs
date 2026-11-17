// Home checkouts against the fake Runner. The person gives a repository a home
// (its own clone outside the workspace) from the workspace's repositories
// dialog, updates it to the latest main, and opens a shell there in the
// Terminal panel (a real terminal). What the agent hands back — a login that
// needs a TTY, written as a fenced `sh` block with `env -u`, or "run in the
// home checkout of api: `make playground`" — opens a shell there with the exact
// command at the prompt, never run by itself and never sent to the agent; once
// the person ran it, "Tell Claude it's done" sends how it ended. Everything is
// reachable from the keyboard. Settings → Repositories lists the homes.
// WebKit by default (the desktop app's engine); TEST_CHROMIUM=1 runs Chromium.
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from 'vite';
import react from '@vitejs/plugin-react';
import {webkit, chromium, expect} from '@playwright/test';
import {fileURLToPath} from 'node:url';
import assert from 'node:assert/strict';
import {mkdir} from 'node:fs/promises';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';

const fixture=await startRunnerFixture();
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
 const errors=[];page.on('pageerror',e=>errors.push(String(e)));
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
 assert.equal(loginItems[0],'Run in the terminal');
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

 // ── The login that needs a TTY: Run in api home opens a shell in the home with the exact command at the prompt.
 const loginOffer=offers.locator('.home-run').nth(0);
 await expect(loginOffer).toHaveText(`Run in api home${login}`);
 await loginOffer.click();
 await expect.poll(()=>calls('OpenTerminal').length).toBe(2);
 assert.equal(calls('OpenTerminal')[1].body.repositoryHome,'backend');
 const second=[...fixture.terminals.values()].at(-1);
 // The temporary checkout's cd is gone (the home stands for it); env -u and every argument are kept; Return is the person's.
 await expect.poll(()=>second.input.join('')).toBe('env -u ACME_TOKEN ./bin/acme login staging');
 assert.equal(second.line,'env -u ACME_TOKEN ./bin/acme login staging','typed, not run');
 assert.equal(applies().length,0,'nothing went to the agent');
 const loginStatus=offers.locator('.home-run-row').nth(0).getByRole('status').first();
 await expect(loginStatus).toHaveText('In the terminal (api home): press Return to run it');
 await expect(screen.locator('textarea')).toBeFocused();
 await page.screenshot({path:`${shots}/home-checkouts-run.png`});
 // The person runs it in the terminal; the shell marks its end, and the offer says so.
 await page.keyboard.press('Enter');
 await expect(screen).toContainText('ran: env -u ACME_TOKEN ./bin/acme login staging');
 await expect(loginStatus).toHaveText('Finished in api home · exit 0');
 // Tell Claude it's done: the command and its exit, sent to the conversation.
 await offers.locator('.home-run-row').nth(0).getByRole('button',{name:'Tell Claude it’s done'}).click();
 await expect.poll(()=>applies().length).toBe(1);
 assert.equal(applies()[0].body.message.text,'Done: `env -u ACME_TOKEN ./bin/acme login staging` finished (exit 0).');
 await expect(offers.locator('.home-run-row').nth(0)).toContainText('Told Claude');
 await page.screenshot({path:`${shots}/home-checkouts-told.png`});

 // ── The offer's ⌄: another home, the person's own terminal, Copy.
 await offers.getByRole('button',{name:'Other ways to run make playground'}).click();
 const other=await labels();
 assert.ok(other.includes('Run in web home')&&other.includes('Run in your home folder')&&other.includes('Open in Terminal (your app)')&&other.includes('Copy command'),`offer menu: ${other}`);
 await page.keyboard.press('Escape');

 // ── From the keyboard: ⌥Tab (WebKit) or Tab reaches the next offer, Enter opens the home folder's shell with it.
 const runLogin=offers.getByRole('button',{name:/^Run in your home folder /});
 await offers.locator('.home-run').nth(1).focus();
 const tab=process.env.TEST_CHROMIUM?'Tab':'Alt+Tab';
 await page.keyboard.press(tab);await page.keyboard.press(tab);
 await expect(runLogin).toBeFocused();
 await page.keyboard.press('Enter');
 await expect.poll(()=>calls('OpenTerminal').length).toBe(3);
 assert.equal(calls('OpenTerminal')[2].body.userHome,true);
 const third=[...fixture.terminals.values()].at(-1);
 await expect.poll(()=>third.input.join('')).toBe('gcloud auth application-default login');
 await expect(panel.getByRole('tab',{name:'~',exact:true})).toHaveAttribute('aria-selected','true');

 // ── Any code block's Run is the terminal, not the agent: the shell in the conversation's folder, the command at the prompt.
 push({turnId:'t2',type:'operation_accepted',text:'And the seed data?'});
 push({turnId:'t2',type:'message_delta',text:'Seed it yourself:\n\n```sh\nmake seed\n```\n'});
 const block=chat.locator('.code-block.is-runnable').last();
 await expect(block).toContainText('make seed');
 await block.getByRole('button',{name:'More ways to run'}).click();
 const blockItems=await labels();
 assert.deepEqual(blockItems.slice(0,4),['Run in the terminal','Run in api home','Run in web home','Run in your home folder'],`block menu: ${blockItems}`);
 assert.ok(blockItems.includes('Ask Claude to run it (no terminal)')&&blockItems.includes('Run as command output (no terminal)…'),'asking the agent is there, last and labelled');
 await page.keyboard.press('Escape');
 await block.getByRole('button',{name:'Run: open a terminal with this command ready'}).click();
 await expect.poll(()=>calls('OpenTerminal').length).toBe(4);
 assert.deepEqual([calls('OpenTerminal')[3].body.repositoryHome,calls('OpenTerminal')[3].body.userHome],[undefined,undefined],'the workspace, not a home');
 const fourth=[...fixture.terminals.values()].at(-1);
 await expect.poll(()=>fourth.input.join('')).toBe('make seed');
 assert.equal(applies().length,1,'Run sent nothing to the agent');
 // The new last message takes the offers; the earlier one no longer shows them.
 await expect(chat.getByRole('group',{name:'Commands for you to run'})).toHaveCount(1);
 await expect(chat.getByRole('group',{name:'Commands for you to run'}).locator('.home-run')).toHaveText([/Run in api home\s*make seed/]);

 // ── Settings → Repositories lists every home with its state.
 await page.getByRole('button',{name:'Settings',exact:true}).click();
 await page.getByRole('tab',{name:'Repositories',exact:true}).click();
 const settings=page.locator('.home-checkouts-settings');
 await expect(settings.getByRole('listitem',{name:'backend home checkout'})).toContainText('main · clean');
 await expect(settings.getByRole('listitem',{name:'frontend home checkout'})).toContainText('/fixture/home/src/web');
 await settings.scrollIntoViewIfNeeded();
 await page.screenshot({path:`${shots}/home-checkouts-settings.png`});
 assert.deepEqual(errors,[],'no page errors');
 console.log('home checkouts: set and updated, opened in the shell, an agent\'s login typed exactly in its home (never sent to the agent), told done, keyboard reachable, listed in Settings');
} finally {
 await browser?.close();await server.close();await fixture.close();
}
