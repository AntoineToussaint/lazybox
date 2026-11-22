// Fixture only: slash commands in the message box. `/` opens the menu
// (Lazybox · Claude · Snippets), it filters as you type, Enter runs Lazybox's
// own commands for real (permissions, a live model switch and one that needs a
// restart), the agent's own commands go as the prompt, a snippet is inserted,
// an unknown /word goes to the agent unchanged, a path is just a message, Vim
// insert mode works, and ⌘K, the ⋯ menu and the header reach the same pickers.
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from 'vite';import react from '@vitejs/plugin-react';import {webkit,expect} from './fixtures/playwright.mjs';import {fileURLToPath} from 'node:url';import {mkdir} from 'node:fs/promises';import assert from 'node:assert/strict';import {startRunnerFixture} from './fixtures/runner-ui.mjs';
const f=await startRunnerFixture();
const data=value=>Buffer.from(JSON.stringify(value)).toString('base64');
const catalog={commands:[{name:'compact',description:'Clear history but keep a summary',argumentHint:'<instructions>',builtin:true},{name:'review-pr',description:'Review a pull request',argumentHint:'<number>'},{name:'init',description:'Initialize a CLAUDE.md',argumentHint:'',builtin:true},{name:'model',description:'native',argumentHint:''}],models:[{value:'claude-opus-5-5',displayName:'Opus 5.5',description:'Most capable'},{value:'claude-sonnet-5',displayName:'Sonnet 5',description:'Fast and capable'}]};
// Alpha switches models live; beta's agent was started by an older Lazybox: it lists no models and restarts to switch.
// The Codex conversation lists its app-server's models and switches live.
Object.assign(f.sessions[0],{title:'Fix the flaky login test',provider:'claude',model:'claude-opus-5-5',modelReported:true,busy:false,status:'ready',conversationId:'alpha-conversation',capabilities:{...f.sessions[0].capabilities,liveModel:true,resume:true},agents:[{id:'root',status:'ready'}]});
Object.assign(f.sessions[1],{title:'Release notes',provider:'claude',model:'claude-opus-5-5',modelReported:true,status:'ready',capabilities:{...f.sessions[1].capabilities,liveModel:false,livePermission:false,resume:true},agents:[{id:'root',status:'ready'}]});
const alpha=f.openEvents('alpha-session');alpha.push({type:'command_catalog',providerDataJson:data(catalog)});alpha.push({type:'operation_accepted',text:'Why is the login test flaky?'});alpha.push({type:'message_delta',text:'The test races the session cookie.'});alpha.push({type:'turn_ended',status:'completed'});
f.openEvents('beta-session');
f.sessions.push({...structuredClone(f.sessions[1]),id:'codex-session',conversationId:'codex-conversation',title:'Codex refactor',provider:'codex',profileId:'codex-default',model:'gpt-6.1-sol',modelReported:true,busy:false,status:'ready',capabilities:{...f.sessions[1].capabilities,liveModel:true,resume:true,steer:true}});
f.openEvents('codex-session').push({type:'command_catalog',providerDataJson:data({models:[{value:'gpt-6.1-sol',displayName:'GPT-6.1-Sol',description:'Workhorse'},{value:'gpt-6-astra',displayName:'GPT-6-Astra',description:'Frontier'},{value:'gpt-6-luna',displayName:'GPT-6-Luna',description:'Fast',effortLevels:['low','high']},{value:'gpt-5.5',displayName:'GPT-5.5',description:'Legacy'}]})});
// A Codex conversation of the "Work" subscription in Full access, on Codex's own default model (none asked for, none reported
// yet), whose app-server lists a dozen models: the ⋯ menu marks Full access and Default as current, and switches for real.
const manyModels=['gpt-6.1-sol','gpt-6-astra','gpt-6-luna','gpt-5.5','gpt-5.5-mini','gpt-5.4','gpt-5.4-mini','gpt-5.3','gpt-5.2','gpt-5.1','gpt-5','gpt-5-mini'].map(value=>({value,displayName:value.toUpperCase(),description:''}));
f.sessions.push({...structuredClone(f.sessions[1]),id:'codex-default-session',conversationId:'codex-default-conversation',title:'Work refactor',provider:'codex',profileId:'codex-default',model:'',modelReported:false,permission:'yolo',busy:false,status:'ready',capabilities:{...f.sessions[1].capabilities,liveModel:true,livePermission:true,resume:true,steer:true}});
f.openEvents('codex-default-session').push({type:'command_catalog',providerDataJson:data({models:manyModels})});
const server=await createServer({...testViteCache(import.meta.url),configFile:false,logLevel:'error',root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:f.url}}}});let browser;let releasePermissionRestart=()=>{};
const mod='Meta';
const applies=()=>f.mutations.filter(m=>m.method==='Apply');
try{
 await server.listen();await mkdir('.cache',{recursive:true});browser=await webkit.launch();const page=await browser.newPage({viewport:{width:1440,height:900}});page.setDefaultTimeout(15000);
 // Leaving a page cuts its in-flight polls, which WebKit reports as page errors ("access control checks"): not failures.
 let navigating=false;const errors=[];page.on('pageerror',e=>{if(!navigating)errors.push(String(e));});
 const go=async url=>{navigating=true;await page.goto(url);await page.waitForLoadState('load');navigating=false;};
 const base=`http://127.0.0.1:${server.httpServer.address().port}`;
 await page.goto(`${base}/?view=workspace&collection=alpha&session=alpha-session#token=${f.token}`);
 const chat=page.locator('.embedded-chat:visible').first();
 const message=chat.getByRole('textbox',{name:'Message',exact:true});
 const menu=chat.getByRole('listbox',{name:'Commands'});
 await expect(message).toBeEnabled();
 await expect(message).toHaveAttribute('placeholder',/\/ for commands/);

 // `/` opens the menu: grouped, each entry with its icon, name, hint and words.
 await message.click();await page.keyboard.type('/');
 await expect(menu).toBeVisible();
 await expect(chat.locator('.slash-group-label')).toHaveText(['Lazybox','Claude','Snippets']);
 const lazyboxNames=['model','permissions','subscription','compact','clear','cost','context','review','artifact','mission','run','stop','detail','rename','help'];
 for(const name of lazyboxNames)await expect(menu.getByRole('option',{name:new RegExp(`^/${name}( |$)`)}).first()).toBeVisible();
 await expect(menu.getByRole('option',{name:/\/review-pr\s*<number>\s*Review a pull request/})).toBeVisible();
 await expect(menu.getByRole('option',{name:/^\/model( |$)/})).toHaveCount(1,'Lazybox\'s /model stands in for the agent\'s');
 await expect(menu.getByRole('option').filter({hasText:'Run tests'})).toBeVisible();
 assert.equal(await message.getAttribute('aria-controls'),await menu.getAttribute('id'),'the box names its menu');
 await page.screenshot({path:'.cache/slash-menu.png'});

 // Filtering as you type; ↑/↓ move; Esc closes and leaves the text.
 await page.keyboard.type('mo');
 await expect(menu.getByRole('option',{selected:true})).toContainText('/model');
 await page.keyboard.press('Escape');await expect(menu).toBeHidden();await expect(message).toHaveValue('/mo');
 await message.fill('');

 // The composer permission button opens the same picker and preserves a draft.
 await message.fill('Draft to keep');
 await chat.getByRole('button',{name:'Change permissions: Ask for approval'}).click();
 await expect(chat.getByRole('listbox',{name:'/permissions choices'})).toBeVisible();
 await page.keyboard.press('Escape');
 await expect(message).toHaveValue('Draft to keep');
 await message.fill('');

 // /permissions: Enter shows the choices, Enter on Full access switches live and says so in one line.
 await page.keyboard.type('/perm');await expect(menu.getByRole('option',{selected:true})).toContainText('/permissions');
 await page.keyboard.press('Enter');
 const choices=chat.getByRole('listbox',{name:'/permissions choices'});
 await expect(choices).toBeVisible();await expect(message).toHaveValue('/permissions ');
 await expect(choices.getByRole('option',{name:/Ask for approval.*Current/})).toBeVisible();
 await page.keyboard.type('yo');await expect(choices.getByRole('option',{selected:true})).toContainText('Full access');
 await page.keyboard.press('Enter');
 await expect.poll(()=>applies().at(-1)?.body.permission?.mode).toBe('yolo');
 await expect(message).toHaveValue('');
 await expect(chat.locator('.command-line').filter({hasText:'Permissions: Full access'})).toBeVisible();
 assert.equal(await chat.locator('.message.operation_accepted').filter({hasText:'/permissions'}).count(),0,'no fake user message');

 // /model: the provider's models in display order, the current one marked; a live switch.
 await page.keyboard.type('/model ');
 const models=chat.getByRole('listbox',{name:'/model choices'});
 await expect(models.getByRole('option')).toHaveText([/Opus 5\.5.*Current/,/Fable 5\.2/,/Haiku 5\.5/,/^Sonnet 5claude-sonnet-5/,/Sonnet 5\.5.*claude-sonnet-5-5/]);
 await expect(models.getByRole('option',{selected:true})).toContainText('Fable 5.2','keyboard selection starts on the first alternative');
 await page.screenshot({path:'.cache/slash-model.png'});
 // Choosing the current model does nothing: the menu closes, no card, nothing applied.
 const appliedBefore=applies().length;
 await page.keyboard.press('ArrowUp');await expect(models.getByRole('option',{selected:true})).toContainText('Current');
 await page.keyboard.press('Enter');
 await expect(models).toBeHidden();await expect(message).toHaveValue('');
 await expect(chat.locator('.restart-model')).toHaveCount(0);
 assert.equal(applies().length,appliedBefore,'choosing the current model applies nothing');
 await page.keyboard.type('/model sonnet 5.5');await expect(models.getByRole('option',{selected:true})).toContainText('Sonnet 5.5');
 await page.keyboard.press('Enter');
 await expect.poll(()=>applies().at(-1)?.body.model?.model).toBe('claude-sonnet-5-5');
 await expect(chat.locator('.command-line').filter({hasText:'Switched model to claude-sonnet-5-5'})).toBeVisible();
 await expect(chat.locator('.restart-model')).toHaveCount(0,'a live switch asks nothing');
 // The header says the model as people do; the id stays in its tooltip.
 await expect(chat.locator('.chat-heading .chat-model')).toHaveText('Claude · Sonnet 5.5');await expect(chat.locator('.chat-heading .chat-model')).toHaveAttribute('title',/claude-sonnet-5-5/);

 // The agent's own command: Enter completes it for its argument, Enter sends it as the prompt.
 await message.fill('');await page.keyboard.type('/review-pr');
 await expect(menu.getByRole('option',{selected:true})).toContainText('/review-pr');
 await page.keyboard.press('Enter');await expect(message).toHaveValue('/review-pr ');
 await expect(chat.locator('.slash-menu-hint')).toContainText('<number>');
 await page.keyboard.type('12');await page.keyboard.press('Enter');
 await expect.poll(()=>applies().at(-1)?.body.message?.text).toBe('/review-pr 12');
 await expect(message).toHaveValue('');
 // A command without an argument goes at once.
 await page.keyboard.type('/ini');await page.keyboard.press('Enter');
 await expect.poll(()=>applies().at(-1)?.body.message?.text).toBe('/init');
 // The box empties after a send; typing before it has would lose the first keys.
 await expect(message).toHaveValue('');
 // /compact is Claude's own /compact, sent without touching the draft.
 await page.keyboard.type('/compact');await page.keyboard.press('Enter');
 await expect.poll(()=>applies().at(-1)?.body.message?.text).toBe('/compact');
 await expect(message).toHaveValue('');
 await message.fill('/mcp');
 await message.press('Enter');
 await expect.poll(() => applies().at(-1)?.body.listMcpServers).toEqual({});
 await expect(chat.locator('.command-line').filter({hasText:'MCP servers: fixture (2 tools)'})).toBeVisible();
 await expect(message).toHaveValue('');

 // A snippet is inserted, not sent.
 const before=applies().length;
 await page.keyboard.type('/test');await expect(menu.getByRole('option',{selected:true})).toContainText('Run tests');
 await page.keyboard.press('Enter');await expect(message).toHaveValue('Run the relevant tests.');
 assert.equal(applies().length,before,'inserting a snippet sends nothing');
 await message.fill('');

 // An unknown /word goes to the agent on the first Enter.
 await page.keyboard.type('/deploy now');await expect(menu).toBeHidden();
 // Under load the box can lag the keys: Enter only once it holds the whole line.
 await expect(message).toHaveValue('/deploy now');
 await page.keyboard.press('Enter');
 await expect.poll(()=>applies().at(-1)?.body.message?.text).toBe('/deploy now');
 await expect(message).toHaveValue('');
 // An unselected fuzzy match must not turn the typed input into a different command.
 await message.fill('/mdl');
 await expect(menu.getByRole('option').filter({ hasText: '/model' })).toBeVisible();
 await message.press('Enter');
 await expect.poll(() => applies().at(-1)?.body.message?.text).toBe('/mdl');
 await expect(message).toHaveValue('');
 // A path is a message like any other.
 await page.keyboard.type('/usr/local/bin is missing');await expect(menu).toBeHidden();
 await expect(message).toHaveValue('/usr/local/bin is missing');
 await page.keyboard.press('Enter');
 await expect.poll(()=>applies().at(-1)?.body.message?.text).toBe('/usr/local/bin is missing');

 // /detail and /help do the real thing.
 // The box takes no keys while the send above is in flight, and empties once it is answered: type after that.
 await expect(message).toHaveValue('');await page.keyboard.type('/detail verbose');await page.keyboard.press('Enter');
 await expect(page.locator('.toast').filter({hasText:'Activity detail: Verbose'})).toBeVisible();
 await page.keyboard.type('/help');await page.keyboard.press('Enter');
 await expect(page.getByRole('dialog',{name:/Keyboard shortcuts/})).toBeVisible();
 await page.keyboard.press('Escape');

 // ⌘K lists the commands; one that takes a choice borrows the box and gives the draft back.
 await message.click();await message.fill('half a draft');
 await page.keyboard.press(`${mod}+k`);
 const palette=page.getByRole('dialog',{name:'Command palette'});
 await palette.getByRole('combobox',{name:'Search commands'}).fill('switch model');
 await expect(palette.getByRole('option').first()).toContainText('Switch model…');
 for(const title of ['Change permissions…','Compact the context','Show context and token cost','Open the review tray','Rename the conversation…','Start over in a new conversation']){
  await palette.getByRole('combobox',{name:'Search commands'}).fill(title);await expect(palette.getByRole('option').filter({hasText:title}).first()).toBeVisible();
 }
 // What cannot run now is not offered: nothing to interrupt while the agent is idle.
 await palette.getByRole('combobox',{name:'Search commands'}).fill('Interrupt the agent');await expect(palette.getByRole('option').filter({hasText:'Interrupt the agent'})).toHaveCount(0);
 await palette.getByRole('combobox',{name:'Search commands'}).fill('switch model');
 await page.keyboard.press('Enter');
 await expect(models).toBeVisible();await expect(message).toHaveValue('/model ');
 await page.keyboard.press('Escape');await expect(message).toHaveValue('half a draft');
 // A command without a choice runs from ⌘K and leaves the draft, and its saved copy, as they were.
 const sentBefore=applies().length;
 await page.keyboard.press(`${mod}+k`);
 await palette.getByRole('combobox',{name:'Search commands'}).fill('Compact the context');
 await expect(palette.getByRole('option').first()).toContainText('Compact the context');
 await page.keyboard.press('Enter');
 await expect.poll(()=>applies().slice(sentBefore).at(-1)?.body.message?.text).toBe('/compact');
 await expect(message).toHaveValue('half a draft');
 await go(`${base}/?view=workspace&collection=alpha&session=alpha-session#token=${f.token}`);
 await expect(message).toHaveValue('half a draft','the saved draft survived');

 // The ⋯ menu has the same models; the header's model name opens the picker.
 await chat.locator('summary[aria-label="Session actions"]').click();
 const sessionMenu=chat.locator('.session-menu');
 await expect(sessionMenu.getByRole('group',{name:'Model'}).getByRole('button',{name:'Sonnet 5.5',exact:true})).toHaveAttribute('aria-pressed','true');
 // No debug facts: the direct gateway, and an account the CLI does not report, go unsaid.
 await expect(sessionMenu).not.toContainText('Gateway');
 await expect(sessionMenu).not.toContainText('Account not reported');
 await expect(sessionMenu.getByRole('button',{name:'Copy session ID'})).toBeVisible();
 await expect(sessionMenu).not.toContainText('alpha-session');
 await page.screenshot({path:'.cache/slash-session-menu.png'});
 await sessionMenu.getByRole('group',{name:'Model'}).getByRole('button',{name:'Opus 5.5',exact:true}).click();
 await expect.poll(()=>applies().at(-1)?.body.model?.model).toBe('claude-opus-5-5');
 await page.keyboard.press('Escape');
 await message.fill('');
 await chat.getByRole('button',{name:/switch model/}).click();
 await expect(models).toBeVisible();await expect(message).toHaveValue('/model ');
 await page.keyboard.press('Escape');await expect(message).toHaveValue('','a borrowed box is given back empty');
 // The model in the message box's info row is /model too, from the keyboard alone: Tab to it, Enter,
 // ↑/↓ through the same choices (the current one marked), Enter switches, and the box keeps the keyboard.
 await message.focus();
 const infoModel=chat.locator('.composer-info-model');
 for(let i=0;i<15&&!(await infoModel.evaluate(node=>node===document.activeElement));i++)await page.keyboard.press('Tab');
 await expect(infoModel).toBeFocused();
 await page.keyboard.press('Enter');
 await expect(models).toBeVisible();await expect(message).toBeFocused();await expect(message).toHaveValue('/model ');
 await expect(models.getByRole('option').filter({hasText:'Current'})).toContainText('Opus 5.5');
 await expect(models.getByRole('option',{selected:true})).toContainText('Fable 5.2');
 await page.keyboard.press('ArrowDown');await expect(models.getByRole('option',{selected:true})).toContainText('Haiku 5.5');
 await page.keyboard.press('ArrowUp');await expect(models.getByRole('option',{selected:true})).toContainText('Fable 5.2');
 await page.keyboard.press('Enter');
 await expect.poll(()=>applies().at(-1)?.body.model?.model).toBe('claude-fable-5-2');
 await expect(models).toBeHidden();await expect(message).toHaveValue('');await expect(message).toBeFocused();
 // Space on it does the same.
 for(let i=0;i<15&&!(await infoModel.evaluate(node=>node===document.activeElement));i++)await page.keyboard.press('Tab');
 await page.keyboard.press('Space');
 await expect(models).toBeVisible();await expect(message).toBeFocused();
 await page.keyboard.press('Escape');await expect(message).toHaveValue('');

 // ⌘K "Open the review tray" opens the tray; it starts no review.
 await page.keyboard.press(`${mod}+k`);
 await palette.getByRole('combobox',{name:'Search commands'}).fill('Open the review tray');
 await expect(palette.getByRole('option').first()).toContainText('Open the review tray');
 await page.keyboard.press('Enter');
 await expect(page.getByRole('complementary',{name:'Review'})).toBeVisible();
 assert.equal(f.mutations.filter(m=>m.method==='StartReview').length,0,'no review started');

 // An agent started by an older Lazybox lists no models and cannot switch live: the
 // Runner's list stands in, the current model asks nothing, another asks once to restart.
 await go(`${base}/?view=workspace&collection=beta&session=beta-session#token=${f.token}`);
 const betaChat=page.locator('.embedded-chat:visible').first(),betaMessage=betaChat.getByRole('textbox',{name:'Message',exact:true});
 await expect(betaMessage).toBeEnabled();
 // From here to the restart, the keyboard alone: no click anywhere.
 await betaMessage.focus();
 // /permissions yolo where the agent must restart to switch: the card takes the keyboard, Enter restarts, the box has it back.
 await page.keyboard.type('/permissions yolo');await page.keyboard.press('Enter');
 const permissionCard=betaChat.getByRole('alert').filter({hasText:'Switch to Full access?'});
 await expect(permissionCard.getByRole('button',{name:'Restart',exact:true})).toBeFocused();
 await page.keyboard.press('Escape');
 await expect(permissionCard).toHaveCount(0);await expect(betaMessage).toBeFocused();
 assert.equal(f.mutations.filter(m=>m.method==='RestartWithPermission').length,0,'Esc restarts nothing');
 // Hold the restart response: the old composer must not take input that a
 // replacement session would discard, nor may Restart submit twice.
 let permissionRequested;
 const requested=new Promise(resolve=>{permissionRequested=resolve;});
 const permissionResponse=new Promise(resolve=>{releasePermissionRestart=resolve;});
 await page.route('**/RestartWithPermission',async route=>{permissionRequested();await permissionResponse;await route.continue();});
 await page.keyboard.type('/permissions yolo');await page.keyboard.press('Enter');
 await expect(permissionCard.getByRole('button',{name:'Restart',exact:true})).toBeFocused();
 await page.keyboard.press('Enter');
 await requested;
 await expect(betaMessage).toBeDisabled();
 await expect(permissionCard).toContainText('Restarting');
 releasePermissionRestart();
 await expect.poll(()=>f.mutations.findLast(m=>m.method==='RestartWithPermission')?.body).toEqual({sessionId:'beta-session',mode:'yolo'});
 await expect(permissionCard).toHaveCount(0);await expect(betaMessage).toBeFocused();
 await page.keyboard.type('/model ');
 const betaModels=betaChat.getByRole('listbox',{name:'/model choices'});
 await expect(betaModels.getByRole('option')).toHaveText([/Opus 5\.5.*Current/,/Fable 5\.2/,/Haiku 5\.5/,/Sonnet 5\.5/]);
 await page.keyboard.press('ArrowUp');await page.keyboard.press('Enter');
 await expect(betaModels).toBeHidden();await expect(betaChat.locator('.restart-model')).toHaveCount(0,'the current model asks nothing');
 await page.keyboard.type('/model sonnet 5.5');await expect(betaModels.getByRole('option',{selected:true})).toContainText('Sonnet 5.5');
 await page.keyboard.press('Enter');
 const restart=betaChat.getByRole('alert').filter({hasText:'Switch to Sonnet 5.5?'});
 await expect(restart).toHaveText(/^Switch to Sonnet 5\.5\? An older Lazybox started this conversation's agent, so switching restarts it and your conversation continues\./);
 await expect(betaChat.locator('.restart-model')).toHaveCount(1);
 await page.screenshot({path:'.cache/slash-model-restart.png'});
 // The card has the keyboard: Esc cancels and gives the box back; Enter restarts and gives it back.
 const restartNow=restart.getByRole('button',{name:'Restart now'});
 await expect(restartNow).toBeFocused();
 await page.keyboard.press('Escape');
 await expect(betaChat.locator('.restart-model')).toHaveCount(0);await expect(betaMessage).toBeFocused();
 assert.equal(f.mutations.filter(m=>m.method==='RestartWithModel').length,0,'Esc restarts nothing');
 await page.keyboard.type('/model son');await expect(betaModels.getByRole('option',{selected:true})).toContainText('Sonnet 5.5');
 await page.keyboard.press('Enter');
 await expect(restartNow).toBeFocused();
 await page.keyboard.press('Enter');
 await expect.poll(()=>f.mutations.findLast(m=>m.method==='RestartWithModel')?.body?.model).toBe('claude-sonnet-5-5');
 assert.match(f.mutations.findLast(m=>m.method==='RestartWithModel').body.sessionId,/^beta-session-restarted-/,'the conversation as it continued after the permission restart');
 await expect(betaChat.locator('.restart-model')).toHaveCount(0);
 await expect(betaMessage).toBeFocused();

 // Codex: its app-server's models (merged with the Runner's), the current one marked; a live switch.
 await go(`${base}/?view=workspace&collection=beta&session=codex-session#token=${f.token}`);
 await page.getByText('Codex refactor',{exact:true}).first().click();
 const codexChat=page.locator('.embedded-chat:visible').filter({hasText:'Codex refactor'}).first(),codexMessage=codexChat.getByRole('textbox',{name:'Message',exact:true});
 await expect(codexMessage).toBeEnabled();
 await codexMessage.click();await page.keyboard.type('/model ');
 const codexModels=codexChat.getByRole('listbox',{name:'/model choices'});
 await expect(codexModels.getByRole('option')).toHaveText([/GPT-6\.1-Sol.*Current/,/GPT-5\.5/,/GPT-6-Astra/,/GPT-6-Luna/,/GPT-6-Luna · high/,/GPT-6-Luna · low/]);
 await page.keyboard.press('ArrowUp');await page.keyboard.press('Enter');
 await expect(codexModels).toBeHidden();await expect(codexChat.locator('.restart-model')).toHaveCount(0);
 assert.equal(applies().filter(m=>m.body.sessionId==='codex-session').length,0,'the current Codex model applies nothing');
 await page.keyboard.type('/model luna high');await page.keyboard.press('Enter');
 await expect.poll(()=>applies().findLast(m=>m.body.sessionId==='codex-session')?.body.model?.model).toBe('gpt-6-luna');
 assert.equal(applies().findLast(m=>m.body.sessionId==='codex-session').body.model.effort,'high');
 await expect(codexChat.locator('.command-line').filter({hasText:'Switched model to gpt-6-luna'})).toBeVisible();
 await codexChat.locator('summary[aria-label="Session actions"]').click();
 const efforts=codexChat.locator('.session-menu').getByRole('group',{name:'Reasoning effort'});
 await expect(efforts.getByRole('button',{name:'high',exact:true})).toHaveAttribute('aria-pressed','true');
 await efforts.getByRole('button',{name:'low',exact:true}).click();
 await expect.poll(()=>applies().findLast(m=>m.body.sessionId==='codex-session')?.body.model?.effort).toBe('low');
 await expect(codexChat.locator('.restart-model')).toHaveCount(0);

 // /mcp is a provider inventory request, even while composing another turn.
 await codexMessage.fill('/mcp');
 await codexMessage.press('Enter');
 await expect.poll(() => applies().at(-1)?.body.listMcpServers).toEqual({});
 await expect(codexChat.locator('.command-line').filter({hasText:'MCP servers: fixture (2 tools)'})).toBeVisible();
 await expect(codexMessage).toHaveValue('');
 assert.equal(applies().some(m => m.body.message?.text === '/mcp'), false);

 for (const section of ['skills', 'apps', 'plugins']) {
   await codexMessage.fill('/' + section);
   await codexMessage.press('Enter');
   await expect.poll(() => applies().at(-1)?.body.listProviderCatalog?.section).toBe(section);
   await expect(codexChat.locator('.command-line').filter({ hasText: `Codex ${section}: fixture` })).toBeVisible();
 }
 // Manual Codex compaction uses a native control request.
 await codexMessage.fill('/compact');
 await codexMessage.press('Enter');
 await expect.poll(() => applies().at(-1)?.body.compactContext).toEqual({});
 await expect(codexMessage).toHaveValue('');
 assert.equal(applies().some(m => m.body.sessionId === 'codex-session' && m.body.message?.text === '/compact'), false);

 // An unavailable suggestion must not swallow Tab or the normal send flow.
 const partialInterrupt = '/interrupt'.slice(0, -1);
 await codexMessage.fill(partialInterrupt);
 const codexCommands = codexChat.getByRole('listbox', {name:'Commands', exact:true});
 await expect(codexCommands.getByRole('option', {selected:true})).toHaveAttribute('aria-disabled','true');
 await codexMessage.press('Tab');
 await expect(codexMessage).not.toBeFocused();
 await expect(codexMessage).toHaveValue(partialInterrupt);
 await codexMessage.fill('');
 await codexMessage.fill(partialInterrupt);
 await expect(codexCommands).toBeVisible();
 await codexMessage.press('Enter');
 await expect.poll(()=>applies().at(-1)?.body.message?.text).toBe(partialInterrupt);
 await expect(codexMessage).toHaveValue('');

 // The ⋯ menu says what is current: Full access is selected (pressed, not greyed out), and the provider's
 // Default is the current model when the conversation names none. Twelve models: the first few, then More.
 await go(`${base}/?view=workspace&collection=beta&session=codex-default-session#token=${f.token}`);
 await page.getByText('Work refactor',{exact:true}).first().click();
 const workChat=page.locator('.embedded-chat:visible').filter({hasText:'Work refactor'}).first();
 await expect(workChat.getByRole('textbox',{name:'Message',exact:true})).toBeEnabled();
 await workChat.locator('summary[aria-label="Session actions"]').click();
 const workMenu=workChat.locator('.session-menu');
 const permissionGroup=workMenu.getByRole('group',{name:'Permissions'}),modelGroup=workMenu.getByRole('group',{name:'Model'});
 const yolo=permissionGroup.getByRole('button',{name:'Full access',exact:true});
 await expect(yolo).toHaveAttribute('aria-pressed','true');await expect(yolo).toBeEnabled();
 assert.equal(await yolo.getAttribute('aria-disabled'),null,'the current mode is not unavailable');
 await expect(permissionGroup.getByRole('button',{name:'Ask for approval',exact:true})).toHaveAttribute('aria-pressed','false');
 await expect(modelGroup.getByRole('button',{name:'Default',exact:true})).toHaveAttribute('aria-pressed','true');
 await expect(modelGroup.locator('button[aria-pressed=true]')).toHaveCount(1);
 await expect(modelGroup.locator('button[data-model]')).toHaveCount(4);
 const more=modelGroup.getByRole('button',{name:'More (9)'});
 await page.screenshot({path:'.cache/slash-session-menu-default.png'});
 // Choosing the current mode changes nothing.
 const workApplied=applies().filter(m=>m.body.sessionId==="codex-default-session").length;
 await yolo.click();
 await workChat.locator('summary[aria-label="Session actions"]').click();
 assert.equal(applies().filter(m=>m.body.sessionId==='codex-default-session').length,workApplied,"the current mode applies nothing");
 // More shows the rest in place (the menu stays open) and the keyboard moves to the first one it revealed; a model there switches live.
 await more.click();
 await expect(modelGroup.locator('button[data-model]')).toHaveCount(13);
 await expect(modelGroup.getByRole('button',{name:'GPT-5.5',exact:true})).toBeFocused();
 await modelGroup.getByRole('button',{name:'GPT-5.4',exact:true}).click();
 await expect.poll(()=>applies().findLast(m=>m.body.sessionId==='codex-default-session')?.body.model?.model).toBe('gpt-5.4');
 await expect(workChat.locator('.command-line').filter({hasText:'Switched model to gpt-5.4'})).toBeVisible();
 // The model it switched to is then the current one, and Default is gone.
 await workChat.locator('summary[aria-label="Session actions"]').click();
 await expect(modelGroup.getByRole('button',{name:'GPT-5.4',exact:true})).toHaveAttribute('aria-pressed','true');
 await expect(modelGroup.getByRole('button',{name:'Default',exact:true})).toHaveCount(0);
 await page.keyboard.press('Escape');

 // Vim: in insert mode the menu works and Esc closes it first; normal mode keeps its keys.
 await go(`${base}/?view=workspace&collection=alpha&session=alpha-session#token=${f.token}`);
 await page.getByRole('button',{name:'Settings',exact:true}).click();await page.getByRole('tab',{name:'Shortcuts'}).click();
 await page.getByRole('switch',{name:'Vim editing in the message box'}).check();
 await page.getByRole('navigation',{name:'Pages'}).getByRole('button', { name: 'Work', exact: true }).click();
 await message.click();const chip=chat.locator('.vim-mode-chip');await expect(chip).toHaveText('INSERT');
 await page.keyboard.type('/perm');await expect(menu).toBeVisible();
 await page.keyboard.press('Escape');await expect(menu).toBeHidden();await expect(chip).toHaveText('INSERT','the first Esc only closes the menu');
 await page.keyboard.press('Escape');await expect(chip).toHaveText('NORMAL');
 await expect(menu).toBeHidden();
 await page.keyboard.press('i');await expect(chip).toHaveText('INSERT');await page.keyboard.press('ControlOrMeta+a');await page.keyboard.press('Backspace');await expect(message).toHaveValue('');
 await page.keyboard.type('/permissions read');
 await expect(chat.getByRole('listbox',{name:'/permissions choices'}).getByRole('option',{selected:true})).toContainText('Read only');
 await page.keyboard.press('Enter');
 await expect.poll(()=>applies().at(-1)?.body.permission?.mode).toBe('read-only');

 assert.deepEqual(errors,[]);
 console.log('PASS: / opens the grouped command menu (Lazybox · Claude · Snippets) with icons, names, hints and descriptions; it filters and ranks as you type, Esc closes it; /permissions and /model run for real (for Claude and Codex: the provider\'s models, the Runner\'s list where the agent lists none, the current one marked and a no-op, a live switch, one clear restart card where needed) and leave one line in the conversation; a restart card (model or permissions) takes the keyboard, Enter restarts, Esc cancels, and the message box has the keyboard back, with no click; the agent\'s commands complete for arguments and go as the prompt; a snippet is inserted; an unknown /word passes through; a path is a message; /detail and /help act; ⌘K lists the commands and gives a borrowed draft back, a command run from it keeps the draft (and its saved copy), and its Open the review tray opens the tray without starting a review; the ⋯ menu offers the models; the header\'s model name and the message box\'s model label open the /model choices, by click or Tab and Enter; Vim insert mode works. Fixture only.');
}finally{releasePermissionRestart();await browser?.close();await server.close();await f.close?.();}
