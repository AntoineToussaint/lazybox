// Run on what the agent wrote. A shell block's Run is the person's: a shell of
// the Terminal panel runs a one-line command (a script, or one with a
// placeholder, is typed at the prompt for review), never a message to the
// agent. Its ▾ also asks the agent (no terminal: the exact block goes as a
// message, at once or queued behind the turn in progress, and the block says
// how the agent's run went) or runs it as command output (a confirm card with
// the script and the folder, then a command of the conversation in the
// console, its outcome on the block, and a failure handed to the agent through
// the message box). Placeholders must be filled in first; a read-only
// conversation warns; inline commands offer the same from their menu; the
// palette and ] ] r open the last block in the terminal. Fake Runner only.
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from 'vite';import react from '@vitejs/plugin-react';import {chromium,expect} from './fixtures/playwright.mjs';import {fileURLToPath} from 'node:url';import assert from 'node:assert/strict';import {mkdir} from 'node:fs/promises';import {startRunnerFixture} from './fixtures/runner-ui.mjs';

const b64=value=>Buffer.from(JSON.stringify(value)).toString('base64');
const fixture=await startRunnerFixture();const session=fixture.sessions[0];session.agents=[];session.status='ready';
const root=session.workingDirectory;
const channel=fixture.openEvents(session.id);
const push=event=>channel.push({turnId:'t1',...event});
const setup='acme connect --profile dev\nclaude mcp add acme -- acme mcp';
push({type:'operation_accepted',text:'How do I connect acme?'});
push({type:'message_delta',text:[
 'Sign in, then register the MCP server:',
 '','```bash',setup,'```','',
 'Then run `bin/devbox check`, then `build`, then `up --solutions none`.',
 '','With a token instead:','','```sh','acme connect --token <your-token>','```','',
 'And the tests:','','```console','$ make test','PASS ok','```','',
 '```json','{"profile":"dev"}','```',
].join('\n')});

const server=await createServer({...testViteCache(import.meta.url),configFile:false,root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});let browser;
try{
 await server.listen();browser=await chromium.launch({headless:true});const page=await browser.newPage({viewport:{width:1400,height:1000}});page.setDefaultTimeout(15000);const errors=[];page.on('pageerror',e=>errors.push(String(e)));
 await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`);
 await mkdir('.cache',{recursive:true});
 const chat=page.locator('.agent-window').first();
 const messages=chat.locator('.messages:visible');
 const composer=chat.getByRole('textbox',{name:'Message',exact:true});
 const applies=()=>fixture.mutations.filter(m=>m.method==='Apply'&&m.body.message);
 const starts=()=>fixture.mutations.filter(m=>m.method==='StartCommand');
 const menu=page.locator('.context-menu[data-depth="0"]');
 const labels=()=>menu.locator('.context-menu-label').allTextContents();

 // ── Only shell blocks of the agent's message can run; JSON and the person's own words cannot.
 const blocks=messages.locator('.code-block.is-runnable');
 await expect(blocks).toHaveCount(3);
 await expect(messages.locator('.code-block:not(.is-runnable)')).toHaveCount(1);
 const [first,token,tests]=[blocks.nth(0),blocks.nth(1),blocks.nth(2)];
 await expect(first.getByRole('button',{name:'Run in the terminal',exact:true})).toBeVisible();
 await expect(first.getByRole('button',{name:'Copy code'})).toBeVisible();
 await page.screenshot({path:'.cache/code-run-blocks.png'});

 // ── Asking the agent (▾, no terminal) sends the exact block, beside the draft, which stays.
 const ask=async(block,name='Ask Claude to run it (no terminal)')=>{await block.getByRole('button',{name:'More ways to run'}).click();await menu.getByRole('menuitem',{name}).click();};
 await composer.fill('a draft I am still writing');
 await ask(first);
 await expect.poll(()=>applies().length).toBe(1);
 const request=`Run this and tell me the result:\n\`\`\`bash\n${setup}\n\`\`\``;
 assert.equal(applies()[0].body.message.text,request);
 await expect(composer).toHaveValue('a draft I am still writing');
 await expect(first.locator('.code-run-chip')).toHaveText('Sent to Claude');
 // The agent runs it: the block follows its tool call.
 push({type:'operation_accepted',text:request,operationId:applies()[0].body.operationId});
 push({type:'tool_activity',providerDataJson:b64({type:'tool_call',id:'run-1',name:'Bash',status:'running',input:{command:`cd ${root} && lazybox run -- acme connect --profile dev`}})});
 await expect(first.locator('.code-run-chip')).toHaveText('Sent to Claude · running…');
 push({type:'tool_activity',providerDataJson:b64({type:'tool_call',id:'run-1',status:'completed',output:'Signed in as dev',exitCode:0})});
 await expect(first.locator('.code-run-chip')).toHaveText('Sent to Claude · ran ✓');
 push({type:'message_delta',text:'Signed in.'});

 // ── While a turn runs, asking the agent queues behind it and says so.
 session.busy=true;session.queued=1;await page.waitForTimeout(1700);
 await ask(tests);
 await expect.poll(()=>applies().length).toBe(2);
 assert.equal(applies()[1].body.message.text,'Run this and tell me the result:\n```sh\nmake test\n```','a console block goes as the script it runs');
 await expect(tests.locator('.code-run-chip')).toHaveText('Queued — runs after the current turn');
 await page.screenshot({path:'.cache/code-run-queued.png'});
 session.queued=0;session.busy=false;
 await expect(tests.locator('.code-run-chip')).toHaveText('Sent to Claude',{timeout:5000});

 // ── Run myself from ▾: confirm card with the script and the folder; Enter runs it in the chosen repository.
 fixture.power.holdCommands=true;
 await first.getByRole('button',{name:'More ways to run'}).click();
 assert.deepEqual(await labels(),['Run in the terminal','Paste in terminal (don’t run)','Run in your home folder','Open in Terminal (your app)','Copy command','Ask Claude to run it (no terminal)','Run as command output (no terminal)…']);
 await menu.getByRole('menuitem',{name:/command output/}).click();
 const card=first.getByRole('form',{name:'Run this yourself'});
 await expect(card).toBeVisible();
 await expect(card.getByRole('textbox',{name:'Script'})).toHaveValue(setup);
 const folder=card.getByRole('combobox',{name:'Folder'});
 await expect(folder).toHaveValue(root);
 assert.deepEqual(await folder.locator('option').allTextContents(),[`Workspace root — ${root}`,`api — ${root}/api`,`web — ${root}/web`]);
 await expect(card.getByRole('button',{name:/^Run/})).toBeFocused();
 await page.screenshot({path:'.cache/code-run-confirm.png'});
 await folder.selectOption(`${root}/api`);
 await card.getByRole('textbox',{name:'Script'}).focus();await page.keyboard.press('Enter');
 await expect.poll(()=>starts().length).toBe(1);
 assert.deepEqual(starts()[0].body,{sessionId:session.id,title:'acme connect --profile dev',argv:['/bin/sh','-lc',setup],cwd:`${root}/api`});
 await expect(card).toHaveCount(0);
 const mine=first.locator('.code-run-self .code-run-chip');
 await expect(mine).toHaveText(/^Running/);
 await expect(mine.locator('small')).toHaveText('in api');
 // The console opens on it, live, with Stop.
 const run=fixture.commands.at(-1);
 await expect(page.locator('.command-panel select')).toHaveValue(run.id);
 await expect(page.locator('.command-panel .console-stop')).toBeVisible();
 Object.assign(run,{done:true,exitCode:0});
 await expect(mine).toHaveText(/^✓ exit 0/,{timeout:5000});
 fixture.power.holdCommands=false;

 // ── A failure (⌥Enter on the focused block opens the card) hands the end of its output to the agent through the message box.
 fixture.power.commandExits.set('make test',2);
 await tests.focus();await page.keyboard.press('Alt+Enter');
 const failing=tests.getByRole('form',{name:'Run this yourself'});
 await failing.getByRole('button',{name:/^Run/}).click();
 await expect(tests.locator('.code-run-self .code-run-chip')).toHaveText(/^✗ exit 2/,{timeout:5000});
 await page.screenshot({path:'.cache/code-run-failed.png'});
 await tests.getByRole('button',{name:'Ask Claude to fix'}).click();
 await expect(composer).toHaveValue(/^a draft I am still writing\n\nI ran this myself and it failed \(exit 2\):\n```sh\nmake test\n```\nIts output:\n```text\n\$ make test\nexit 2\n```\nFind out why and fix it\.$/);
 await expect(composer).toBeFocused();
 assert.equal(applies().length,2,'the fix request is not sent for the person');
 await composer.fill('');

 // ── Placeholders: Run myself asks to fill them in first, and runs the edited script.
 await token.locator('pre').click({button:'right'});
 const tokenItems=await labels();
 assert.equal(tokenItems[0],'Run in the terminal');
 assert.ok(tokenItems.includes('Copy code')&&tokenItems.includes('Run as command output (no terminal)…'),`block menu: ${tokenItems}`);
 await menu.getByRole('menuitem',{name:/command output/}).click();
 const fill=token.getByRole('form',{name:'Run this yourself'});
 await expect(fill.getByRole('note').filter({hasText:'Fill in'})).toHaveText('Fill in <your-token> before running.');
 const script=fill.getByRole('textbox',{name:'Script'});
 await expect(script).toBeFocused();
 assert.equal(await script.evaluate(t=>t.value.slice(t.selectionStart,t.selectionEnd)),'<your-token>','the placeholder is selected to type over');
 await expect(fill.getByRole('button',{name:/^Run/})).toHaveAttribute('aria-disabled','true');
 await page.keyboard.press('Enter');
 await expect(fill.getByRole('alert')).toHaveText('Replace <your-token> first.');
 assert.equal(starts().length,2);
 await page.keyboard.type('tok_123');
 await expect(fill.getByRole('note').filter({hasText:'Placeholders'})).toHaveText('Placeholders filled in.');
 await page.keyboard.press('Enter');
 await expect.poll(()=>starts().length).toBe(3);
 assert.equal(starts()[2].body.argv[2],'acme connect --token tok_123');
 await expect(token.locator('.code-run-self .code-run-chip')).toHaveText(/^✓ exit 0/);

 // ── Inline commands: Run from their menu only; a bare subcommand reads as the program beside it.
 await expect(messages.locator('.code-run-main')).toHaveCount(3);
 const inline=text=>messages.locator('p code',{hasText:new RegExp(`^${text}$`)});
 await inline('bin/devbox check').click({button:'right'});
 assert.deepEqual((await labels()).slice(0,3),['Run `bin/devbox check` in the terminal','Paste in terminal (don’t run)','Run in your home folder']);
 await page.keyboard.press('Escape');
 await inline('build').click({button:'right'});
 await menu.getByRole('menuitem',{name:'Ask Claude to run it (no terminal)'}).click();
 await expect.poll(()=>applies().length).toBe(3);
 assert.equal(applies()[2].body.message.text,'Run this and tell me the result:\n```sh\nbin/devbox build\n```');
 const inlineRuns=messages.locator('.inline-run');
 await expect(inlineRuns.filter({hasText:'bin/devbox build'}).locator('.code-run-chip')).toHaveText('Sent to Claude');
 await inline('up --solutions none').click({button:'right'});
 await menu.getByRole('menuitem',{name:/command output/}).click();
 const inlineCard=messages.getByRole('form',{name:'Run this yourself'});
 await expect(inlineCard.getByRole('textbox',{name:'Script'})).toHaveValue('bin/devbox up --solutions none');
 await page.keyboard.press('Escape');
 await expect(inlineCard).toHaveCount(0);

 // ── A read-only conversation still lets the person run it, and says what that means.
 session.permission='read-only';await page.waitForTimeout(1700);
 await first.focus();await page.keyboard.press('Alt+Enter');
 const readOnly=first.getByRole('form',{name:'Run this yourself'});
 await expect(readOnly.getByRole('note').first()).toHaveText('This conversation is read-only: Claude may not change anything here, but this command runs as you and can.');
 await page.screenshot({path:'.cache/code-run-read-only.png'});
 await readOnly.getByRole('button',{name:/^Run/}).click();
 await expect.poll(()=>starts().length).toBe(4);
 await expect(readOnly).toHaveCount(0);
 session.permission='ask';

 // ── Run, the palette and ] ] r open the last command block in the Terminal panel: the command at the prompt, nothing sent to the agent.
 push({type:'message_delta',text:'Now:\n\n```bash\nnpm run lint\n```'});
 await expect(blocks).toHaveCount(4);
 const opened=()=>fixture.calls.filter(c=>c.method==='OpenTerminal').length;
 const lastShell=()=>[...fixture.terminals.values()].at(-1);
 await blocks.nth(3).getByRole('button',{name:'Run in the terminal',exact:true}).click();
 await expect.poll(opened).toBe(1);
 // One line, nothing to fill in: it runs, in the shell the person watches.
 await expect.poll(()=>lastShell().input.join('')).toBe('npm run lint\r');
 await expect(page.locator('.panel-terminal').getByTestId('terminal')).toContainText('ran: npm run lint');
 await expect(blocks.nth(3).locator('.code-run-chip').first()).toHaveText('Finished in the workspace · exit 0');
 // A placeholder is typed at the prompt for the person to fill in, in the same shell (idle again), not run.
 await token.getByRole('button',{name:'Run in the terminal',exact:true}).click();
 await expect.poll(()=>lastShell().line).toBe('acme connect --token <your-token>');
 assert.equal(opened(),1,'the idle shell is reused');
 await expect(token.locator('.code-run-terminal .code-run-chip').first()).toHaveText('Typed in the terminal (the workspace): review and press Return');
 await expect(page.locator('.context-menu-notice').filter({hasText:'Typed in the terminal: review and press Return'}).first()).toBeVisible();
 // The shell takes the keyboard, so Return runs it there.
 const shellFocused=()=>expect(page.locator('.panel-terminal').getByTestId('terminal').locator('textarea')).toBeFocused();
 await shellFocused();
 await messages.click({position:{x:5,y:5}});
 await page.keyboard.press('Meta+k');
 const palette=page.getByRole('dialog',{name:'Command palette'});
 await palette.getByRole('combobox',{name:'Search commands'}).fill('>run the last command');
 await expect(palette.getByRole('option',{name:/Run the last command block as command output/})).toBeVisible();
 await palette.getByRole('option',{name:/^Run the last command block in the terminal/}).click();
 // The workspace's shell holds the typed command: the next run opens another.
 await expect.poll(opened).toBe(2);
 await expect.poll(()=>lastShell().input.join('')).toBe('npm run lint\r');
 await expect(blocks.nth(3).locator('.code-run-chip').first()).toHaveText('Finished in the workspace · exit 0');
 await shellFocused();
 await messages.click({position:{x:5,y:5}});
 await page.keyboard.press(']');await page.keyboard.press(']');await page.keyboard.press('r');
 // That one is idle again: reused.
 await expect.poll(()=>lastShell().input.join('')).toBe('npm run lint\r\x15npm run lint\r');
 assert.equal(opened(),2,'the idle shell is reused');
 await shellFocused();
 assert.equal(applies().length,3,'none of them asked the agent');

 // ── Servers and watchers never end: Run asks to start one in the background, and the block follows it there.
 push({type:'message_delta',text:'\n\nStart the app:\n\n```bash\nnpx vite --port 5173\n```\n\nAnd the API:\n\n```bash\nnpm run dev\n```'});
 await expect(blocks).toHaveCount(6);
 const vite=blocks.nth(4),api=blocks.nth(5);
 await ask(vite,'Ask Claude to start it in the background (no terminal)');
 await expect.poll(()=>applies().length).toBe(4);
 const startRequest='Start this in the background (it keeps running) and tell me when it is ready:\n```bash\nnpx vite --port 5173\n```';
 assert.equal(applies()[3].body.message.text,startRequest);
 await expect(vite.locator('.code-run-chip')).toHaveText('Sent to Claude');
 push({type:'operation_accepted',text:startRequest,operationId:applies()[3].body.operationId});
 push({type:'tool_activity',providerDataJson:b64({type:'tool_call',id:'vite-1',name:'Bash',status:'running',input:{command:'lazybox run -- npx vite --port 5173'}})});
 await expect(vite.locator('.code-run-chip')).toHaveText('Sent to Claude · starting…');
 push({type:'tool_activity',providerDataJson:b64({type:'tool_call',id:'vite-1',status:'completed',exitCode:0,output:'[lazybox] npx vite --port 5173 → running in the background as command VITE1 · listening on http://localhost:5173 · output: /tmp/vite.log · stop: `lazybox stop VITE1`'})});
 await expect(vite.locator('.code-run-chip')).toHaveText('Sent to Claude · running in the background · port 5173');
 await expect(vite.getByRole('button',{name:'Open preview'})).toBeVisible();
 // A workspace action that runs the same command: Run asks for the action, so the agent uses start_action.
 await ask(api,'Ask Claude to start it in the background (no terminal)');
 await expect.poll(()=>applies().length).toBe(5);
 assert.equal(applies()[4].body.message.text,'Start the workspace action `api/dev` (it runs this) and tell me when it is ready:\n```bash\nnpm run dev\n```');

 // ── Run myself on a server: it never waits for an exit; it says where it listens, with Open preview and Stop.
 fixture.power.holdCommands=true;
 await vite.focus();await page.keyboard.press('Alt+Enter');
 const serverCard=vite.getByRole('form',{name:'Run this yourself'});
 await expect(serverCard.getByRole('note').first()).toHaveText('This keeps running (a server or a watcher). It runs in the background; stop it here or in Processes.');
 await serverCard.getByRole('button',{name:/^Run/}).click();
 await expect.poll(()=>starts().length).toBe(5);
 const serverRun=fixture.commands.at(-1);
 const serverChip=vite.locator('.code-run-self .code-run-chip');
 await expect(serverChip).toHaveText(/^Running · starting…/);
 serverRun.ports=[5173];
 await expect(serverChip).toHaveText(/^Running · port 5173/,{timeout:5000});
 const selfRun=vite.locator('.code-run-self');
 await expect(selfRun.getByRole('button',{name:'Open preview'})).toBeVisible();
 await page.screenshot({path:'.cache/code-run-server.png'});
 await selfRun.getByRole('button',{name:'Open preview'}).click();
 await expect(page.getByRole('textbox',{name:'Address',exact:true}).first()).toHaveValue('http://localhost:5173/');
 await selfRun.getByRole('button',{name:'Stop'}).click();
 await expect.poll(()=>fixture.mutations.findLast(m=>m.method==='CancelCommand')?.body.commandId).toBe(serverRun.id);
 await expect(serverChip).toHaveText(/^■ Stopped \(SIGTERM\)/,{timeout:5000});
 fixture.power.holdCommands=false;

 // ── Run myself on the action's command starts the workspace action: its own port, shown in Run and Preview.
 await api.focus();await page.keyboard.press('Alt+Enter');
 const actionCard=api.getByRole('form',{name:'Run this yourself'});
 await expect(actionCard.getByRole('note').first()).toHaveText(`Starts the workspace action api/dev on port ${fixture.previewPort}: it gets the workspace’s own port, and shows in Run and Preview.`);
 await expect(actionCard.getByRole('combobox',{name:'Folder'})).toHaveCount(0);
 await actionCard.getByRole('button',{name:/^Start action/}).click();
 await expect.poll(()=>fixture.mutations.filter(m=>m.method==='StartWorkspaceAction').at(-1)?.body).toEqual({collectionId:'alpha',action:'api/dev'});
 assert.equal(starts().length,5,'the action ran, not a command');
 const actionChip=api.locator('.code-run-self .code-run-chip');
 await expect(actionChip).toHaveText(`Running · port ${fixture.previewPort}action api/dev`);
 await page.screenshot({path:'.cache/code-run-action.png'});
 await api.locator('.code-run-self').getByRole('button',{name:'Stop'}).click();
 await expect.poll(()=>fixture.mutations.some(m=>m.method==='StopWorkspaceAction'&&m.body.action==='api/dev')).toBe(true);
 await expect(actionChip).toHaveText(/^■ Stopped/,{timeout:5000});
 assert.deepEqual(errors,[]);
 console.log('PASS: shell blocks in agent messages get Run (the Terminal panel: one line runs, a placeholder is typed for review, an idle shell is reused), asking the agent from ▾ (the exact block sent at once or queued behind the turn, the agent\'s run followed to ✓) and command output (confirm card with script and folder, started in the chosen repository, console opened, outcome on the block, a failure handed to the message box); placeholders must be filled in; read-only warns and still runs; inline commands run from their menu, bare subcommands with their program; palette and ] ] r run the last block; servers: Run asks to start them in the background (by the workspace action when one runs the same command) and follows them there with their port; Run myself never waits, shows Running · port, Open preview and Stop, and starts the matching workspace action instead. Fixture only.');
}finally{await browser?.close();await server.close();await fixture.close();}
