// Conversation and per-response display settings against the fixture Runner.
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from './fixtures/vite-cache.mjs';import react from '@vitejs/plugin-react';import {chromium,webkit,expect} from './fixtures/playwright.mjs';import {fileURLToPath} from 'node:url';import assert from 'node:assert/strict';import {mkdir} from 'node:fs/promises';import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import { collectPageErrors, drainPageErrors } from './fixtures/page-errors.mjs';

const b64=value=>Buffer.from(JSON.stringify(value)).toString('base64');
const fixture=await startRunnerFixture();const session=fixture.sessions[0];session.busy=true;session.status='running';session.agents=[{id:'root',status:'running'}];
const root=session.workingDirectory;
const channel=fixture.openEvents(session.id);
let clock=Date.now()-10*60_000;const at=(seconds=0)=>new Date(clock+=seconds*1000).toISOString();
const push=(event,seconds=0)=>channel.push({turnId:'t1',...event},at(seconds));
const tool=(id,fields,seconds=0)=>push({type:'tool_activity',providerDataJson:b64({type:'tool_call',id,...fields})},seconds);
const said=text=>push({type:'message_delta',text},1);

push({type:'operation_accepted',text:'Earlier question',turnId:'previous'});
push({type:'message_delta',text:'Earlier answer',turnId:'previous'});
push({type:'context',providerDataJson:b64({model:'fixture-model',used:250,capacity:1000})});
push({type:'operation_accepted',text:'Why can’t the owner read their own runtime?'});
said('I’ll trace how the runtime resolves access.');
// The run a person saw: thinking, a multi-line shell script, exit 0, 0.1s — eight times.
const files=['runtime_access.go','policy.go','owner.go','session.go','grants.go','store.go','handler.go','errors.go'];
files.forEach((file,i)=>{
 push({type:'activity',status:'thinking'},2);
 const command=i%3===2?`cd ${root}\nsed -n '1,200p' services/runner/code/internal/runtime/${file}\necho ---\nrg -n "resolveAccess" services/runner/code/internal/runtime`:`cd ${root} && sed -n '1,200p' services/runner/code/internal/runtime/${file}`;
 tool(`sh-${i}`,{name:'Bash',status:'running',input:i===0?{command,description:'Read the access check'}:{command}},1.4);
 tool(`sh-${i}`,{status:'completed',output:`package runtime\n// ${file}\n`},0.1);
});
for(const [i,file] of ['internal/runtime/policy_test.go','README.md'].entries()){tool(`read-${i}`,{name:'Read',status:'running',input:{file_path:`${root}/${file}`}},1);tool(`read-${i}`,{status:'completed',output:'a\nb\nc\n'},0.1);}
tool('fail',{name:'Bash',status:'running',input:{command:'npm test',description:'Run the unit tests'}},1);
tool('fail',{status:'failed',output:[{type:'text',text:'Exit code 1\nFAIL src/access.test.ts › owner can read'}]},3);
tool('edit',{name:'Edit',status:'running',input:{file_path:`${root}/services/runner/code/internal/runtime/runtime_access.go`,old_string:'return false',new_string:'return owner == caller'}},1);
tool('edit',{status:'completed',output:'ok'},0.1);
said('Found it: the owner check compared the wrong ids. Running the tests now.');
tool('diff',{name:'Bash',status:'running',input:{command:'git diff --stat'}},1);tool('diff',{status:'completed',output:' 1 file changed'},0.1);
tool('ls',{name:'Bash',status:'running',input:{command:'ls services/runner/code/internal/runtime'}},1);tool('ls',{status:'completed',output:'a.go'},0.1);
tool('gotest',{name:'Bash',status:'running',input:{command:'cd services/runner/code && go test ./internal/runtime/... -count 1',description:'Run the runtime tests'}},1);

const server=await createServer({...testViteCache(import.meta.url),configFile:false,root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});let browser;
try{
 await server.listen();browser=await (process.env.TEST_WEBKIT?webkit:chromium).launch({headless:true});const page=await browser.newPage({viewport:{width:1450,height:1050}});page.setDefaultTimeout(15000);const errors=[];await collectPageErrors(page, errors);
 const url=`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`;
 await page.goto(url);
 await mkdir('.cache',{recursive:true});
 const messages=page.locator('.messages:visible').first();
 await expect(messages.locator('.prompt-sent-time').last()).toHaveAttribute('title',/fixture-model.*250 \/ 1,000 tokens/);
 const details=messages.locator('.prompt-metadata').last();
 await details.getByLabel('Prompt details').click();
 await expect(details.locator('p')).toContainText(/fixture-model.*250 \/ 1,000 tokens/);
 await details.getByLabel('Prompt details').press('Escape');
 await expect(details.locator('p')).toBeHidden();
 const override=messages.getByLabel('Response detail for this prompt').last();
 await override.selectOption('quiet');
 // Reopen a response preference written by an earlier release.
 await page.evaluate(()=>{
  const key=Object.keys(localStorage).find(k=>k.includes('chat.response-detail.') && localStorage.getItem(k)==='quiet');
  if(!key)throw new Error('Expected the saved response preference');
  localStorage.setItem(key,'smart');
 });
 await page.reload();
 await expect(override).toHaveValue('quiet');
 await expect(messages.locator('.tool-lines').first()).toHaveAttribute('data-detail','quiet');
 await expect(messages.getByLabel('Response detail for this prompt').first()).toHaveValue('');
 await override.selectOption('hidden');
 await page.reload();
 await expect(override).toHaveValue('hidden');
 await expect(messages.getByText('Earlier answer',{exact:true})).toBeVisible();
 await expect(messages.getByText('I’ll trace how the runtime resolves access.')).toHaveCount(0);
 await override.selectOption('verbose');
 await page.reload();
 await expect(override).toHaveValue('verbose');
 await expect(messages.locator('.tool-lines').first()).toHaveAttribute('data-detail','verbose');
 await override.selectOption('');
 await page.reload();
 await expect(override).toHaveValue('');
 const cycle=page.getByTitle('Cycle conversation activity detail').first();
 await cycle.click(); await expect(cycle).toHaveText('Verbose');
 await cycle.click(); await expect(cycle).toHaveText('Quiet');
 const runs=messages.locator('.tool-lines');
 await expect(messages.locator('.tool-turn')).toHaveCount(1);
 await expect(messages.locator('[data-tool-id=gotest] .tool-spinner')).toHaveCount(1);
 tool('gotest',{status:'completed',output:'ok  runtime 2.1s'},4);
 // ── This conversation, Verbose, from its ⋯ menu: every line, remembered.
 const chat=page.locator('.agent-window').first();
 await chat.getByLabel('Session actions',{exact:true}).click();
 const choice=page.getByRole('group',{name:'Activity detail'});
 await expect(choice.getByRole('button',{name:'Quiet'})).toHaveAttribute('aria-pressed','true');
 await expect(choice.locator('strong')).toHaveText('Activity detail · default');
 await choice.getByRole('button',{name:'Verbose'}).click();
 await expect(messages.locator('.tool-meta span',{hasText:/^exit 0$/})).toHaveCount(0);
 await expect(messages.getByText(/^Thought for 1\.4s$/)).toHaveCount(8);
 await expect(messages.locator('[data-tool-id=edit] .tool-inline-diff')).toHaveCount(1);
 await page.reload();
 await expect(messages.getByText(/^Thought for 1\.4s$/)).toHaveCount(8);
 await expect(messages.locator('.tool-meta span',{hasText:/^exit 0$/})).toHaveCount(0);
 await chat.getByLabel('Session actions',{exact:true}).click();
 await expect(choice.locator('strong')).toHaveText('Activity detail · this conversation');
 await expect(choice.getByRole('button',{name:'Verbose'})).toHaveAttribute('aria-pressed','true');
 await page.keyboard.press('Escape');

 // ── Quiet, from the command palette: one line per turn, edits kept, the failure counted.
 await messages.click({position:{x:5,y:5}});
 await page.keyboard.press('Meta+k');
 const palette=page.getByRole('dialog',{name:'Command palette'});
 await palette.getByRole('combobox',{name:'Search commands'}).fill('>activity detail');
 await expect(palette.getByRole('option',{name:/Activity detail: Verbose/})).toHaveCount(0);
 await palette.getByRole('option',{name:/Activity detail: Quiet/}).click();
 await expect(page.locator('.toast',{hasText:'Activity detail: Quiet'})).toContainText('This conversation follows the default.');
 const turn=messages.locator('.tool-turn');
 await expect(turn).toHaveCount(1);
 await expect(turn.locator(':scope > .tool-line-row')).toHaveText(/^Worked for (\d+m )?\d+s15 steps1 failed$/);
 await expect(messages.locator('[data-tool-id=fail]')).toHaveCount(0);
 await expect(messages.locator('[data-tool-id=edit] > .tool-line-row')).toHaveText(/^Edited/);
 await expect(messages.locator('.tool-line-row')).toHaveCount(2);
 await page.screenshot({path:'.cache/activity-detail-quiet.png'});
 await turn.getByRole('button',{name:/^Worked for/}).click();
 await expect(turn.getByRole('list',{name:'Steps'}).locator(':scope > .tool-line')).toHaveCount(14);
 await expect(turn.locator('[data-tool-id=fail] .tool-line-problem')).toHaveText('FAIL src/access.test.ts › owner can read');

 await messages.click({position:{x:5,y:5}});
 await page.keyboard.press(']');await page.keyboard.press(']');await page.keyboard.press('v');
 await expect(page.locator('.toast',{hasText:'Activity detail: Verbose'})).toContainText('This conversation only.');

 await cycle.click(); await expect(cycle).toHaveText('Quiet');
 // ── The default, in Settings → Chat, with a live preview; remembered across a reload.
 await page.getByRole('button',{name:'Settings',exact:true}).click();
 await page.getByRole('tab',{name:'Chat'}).click();
 const settings=page.getByRole('tabpanel',{name:'Chat'});
 await expect(settings.getByRole('radio',{name:/Quiet/})).toBeChecked();
 const preview=settings.getByLabel(/^Preview of/);
 await expect(preview.locator('.tool-turn')).toHaveCount(1);
 await settings.getByRole('radio',{name:/Verbose/}).check();
 await expect(preview).toHaveAttribute('aria-label','Preview of Verbose');
 await expect(preview.locator('.tool-meta span',{hasText:/^exit 0$/})).toHaveCount(0);
 await page.screenshot({path:'.cache/activity-detail-settings.png'});
 await page.reload();
 await expect(messages.getByText(/^Thought for 1\.4s$/)).toHaveCount(8);
 await expect(messages.locator('.tool-meta span',{hasText:/^exit 0$/})).toHaveCount(0);
 await chat.getByLabel('Session actions',{exact:true}).click();
 await expect(choice.locator('strong')).toHaveText('Activity detail · default');
 await expect(choice.getByRole('button',{name:'Verbose'})).toHaveAttribute('aria-pressed','true');
 await drainPageErrors(errors);assert.deepEqual(errors,[]);
}finally{await browser?.close();await server.close();await fixture.close();}
