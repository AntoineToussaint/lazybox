// ACP agents in the UI, against the fake Runner (fixtures/runner-ui.mjs and
// acp-ui-fake.mjs): Settings → Agents (an installed agent with its version, a
// missing one with its install command, a custom agent added, edited and
// removed with its environment values never shown again), the new
// conversation's agent picker (icons, keyboard), and an ACP conversation
// replaying what the real ACP adapter journals for the fake ACP agent
// (fixtures/acp-journal.json, kept current by TestACPJournalFixtureIsCurrent):
// tool lines, the plan, the command it ran, its reply, and the approval card
// with the agent's own "Always allow". No credentials, no inference.
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from 'vite';import react from '@vitejs/plugin-react';import {chromium,expect} from './fixtures/playwright.mjs';import {fileURLToPath} from 'node:url';import assert from 'node:assert/strict';import {mkdir} from 'node:fs/promises';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import {acpJournal} from './fixtures/acp-ui-fake.mjs';

const fixture=await startRunnerFixture();
fixture.acp.install('gemini','0.68.0');
const launchProblem="OpenCode's help does not mention `acp`, which Lazybox starts it with: this version may not run in ACP mode. Update it (`npm install -g opencode-ai`), or edit how Lazybox starts it.";
fixture.acp.install('opencode','0.9.0',launchProblem);
const collection=fixture.collections[0],root=collection.root;
const {events,pending}=acpJournal(root);
const session={id:'acp-session',collectionId:collection.id,workId:collection.id,profileId:'acp-gemini',provider:'acp:gemini',workingDirectory:root,title:'Fake tour',
  capabilities:{files:true,images:true,approvals:true,livePermission:true,liveModel:true,resume:true},permission:'ask',status:'waiting_approval',busy:true,pending,agents:[{id:'root',name:'Main agent',status:'waiting_approval'}]};
fixture.sessions.push(session);
const channel=fixture.openEvents(session.id);for(const event of events)channel.push(event);
const applied=kind=>fixture.mutations.filter(m=>m.method==='Apply'&&m.body[kind]).map(m=>m.body[kind]);

const server=await createServer({...testViteCache(import.meta.url),configFile:false,root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});let browser;
try{
 await server.listen();browser=await chromium.launch({headless:true});const page=await browser.newPage({viewport:{width:1450,height:1000}});page.setDefaultTimeout(10000);
 const errors=[];page.on('pageerror',e=>{errors.push(String(e));console.error('Browser error:',e);});
 page.on('dialog',dialog=>void dialog.accept());
 await page.addInitScript(()=>localStorage.setItem('lazybox:chat.activity-detail','verbose'));
 const base=`http://127.0.0.1:${server.httpServer.address().port}`;
 await mkdir('.cache',{recursive:true});

 // ── An ACP conversation: what the real adapter journaled, read as Lazybox lines.
 await page.goto(`${base}/?view=workspace&collection=${collection.id}&session=${session.id}#token=${fixture.token}`);
 const messages=page.locator('.messages:visible').first();
 await expect(messages).toContainText('Done: read the README, wrote notes.txt and ran the check.');
 await expect(messages.locator('.tool-line').filter({hasText:'Read'}).filter({hasText:'README.md'}).first()).toBeVisible();
 await expect(messages.locator('.tool-line').filter({hasText:'notes.txt'}).first()).toContainText('Wrote');
 await expect(messages.locator('.tool-line').filter({hasText:'echo fake-tour-check'}).first()).toContainText('exit 0');
 await expect(messages.getByRole('button',{name:/Updated the plan · 3\/3 done/})).toBeVisible();
 await expect(messages.getByRole('list',{name:'Plan'}).last().getByRole('listitem')).toHaveText([/Read the README/,/Write the notes/,/Run the check/]);
 await expect(messages).not.toContainText('private reasoning');
 await expect(page.getByRole('textbox',{name:'Message'})).toHaveAttribute('placeholder',/Message Gemini CLI/);
 // The agent's permission request is an approval card: exactly what runs, its own Always option.
 const card=page.locator('.request.approval-card');
 await expect(card).toHaveCount(1);
 await expect(card.getByLabel('Command',{exact:true})).toContainText('make deploy');
 await expect(card.locator('.decision-row')).toHaveText([/Allow once/,/Always allow `?make deploy`? commands/,/Deny…/]);
 await page.screenshot({path:'.cache/acp-conversation.png'});
 await card.getByRole('button',{name:/Always allow/}).click();
 await expect.poll(()=>applied('approval').at(-1)).toEqual({requestId:pending[0].requestId,decision:'DECISION_ACCEPT',always:'allow-always'});

 // ── Settings → Agents: what is installed, what is missing and how to get it.
 await page.getByRole('button',{name:'Settings',exact:true}).click();await page.getByRole('tab',{name:'Agents & workspaces',exact:true}).click();
 const section=page.getByRole('region',{name:'Agent programs'});
 await expect(section).toBeVisible();
 const gemini=section.locator('.acp-agent').filter({hasText:'Gemini CLI'});
 await expect(gemini).toContainText('0.68.0');await expect(gemini).toContainText('Ready');await expect(gemini.locator('code')).toHaveText('gemini --acp');
 // ACP agents are a preview, and a built-in whose --help does not name its launch flags says so.
 await expect(section.locator('.acp-preview')).toHaveText('Preview');
 await expect(gemini.getByRole('note')).toHaveCount(0);
 await expect(section.locator('.acp-agent').filter({hasText:'OpenCode'}).getByRole('note')).toHaveText(launchProblem);
 const goose=section.locator('.is-missing .acp-agent').filter({hasText:'Goose'});
 await expect(goose.locator('code')).toHaveText('brew install block-goose-cli');
 await expect(goose.getByRole('button',{name:'Copy the command that installs Goose'})).toBeVisible();
 // Add a custom agent by its command line; its environment value is written once.
 await section.getByRole('button',{name:'Add an ACP agent…'}).click();
 const form=section.getByRole('form',{name:'Add an ACP agent'});
 await expect(form.getByLabel('Name')).toBeFocused();
 await form.getByLabel('Name').fill('Team agent');
 await form.getByLabel('Command line').fill('npx "@team/agent acp" --verbose');
 await form.getByLabel('Environment').fill('# keys\nTEAM_TOKEN=secret-value-123');
 await page.screenshot({path:'.cache/acp-add-agent.png'});
 await form.getByRole('button',{name:'Add agent'}).click();
 await expect(section.getByRole('status')).toContainText('Team agent saved');
 let saved=fixture.mutations.findLast(m=>m.method==='SaveAcpAgent').body;
 assert.deepEqual({title:saved.title,command:saved.command,args:saved.args,env:saved.env},{title:'Team agent',command:'npx',args:['@team/agent acp','--verbose'],env:[{name:'TEAM_TOKEN',value:'secret-value-123'}]});
 const team=section.locator('.acp-agent').filter({hasText:'Team agent'});
 await expect(team).toContainText('Added by you');await expect(team).toContainText('Environment: TEAM_TOKEN');
 await expect(page.locator('body')).not.toContainText('secret-value-123');
 // Editing keeps the saved value unless a new one is typed; Escape cancels.
 await team.getByRole('button',{name:'Change how Team agent starts'}).click();
 const edit=section.getByRole('form',{name:'Change Team agent'});
 await expect(edit.getByLabel('Environment')).toHaveValue('TEAM_TOKEN=');
 await expect(edit.getByLabel('Command line')).toHaveValue('npx "@team/agent acp" --verbose');
 await page.keyboard.press('Escape');await expect(edit).toHaveCount(0);
 await team.getByRole('button',{name:'Change how Team agent starts'}).click();
 await section.getByRole('form',{name:'Change Team agent'}).getByRole('button',{name:'Save'}).click();
 await expect(section.getByRole('status')).toContainText('Team agent saved');
 saved=fixture.mutations.findLast(m=>m.method==='SaveAcpAgent').body;
 assert.deepEqual(saved.env.map(e=>[e.name,e.value||'']),[['TEAM_TOKEN','']]);assert.equal(saved.id,'team-agent');
 assert.equal(fixture.acp.custom.get('team-agent').env.TEAM_TOKEN,'secret-value-123','an empty value keeps the saved one');
 await page.screenshot({path:'.cache/acp-settings.png'});

 // ── New conversation: the picker lists every agent the Runner can start, with icons; keyboard only.
 await page.getByRole('navigation',{name:'Pages'}).getByRole('button', { name: 'Work', exact: true }).click();
 await page.getByRole('button',{name:'New conversation',exact:true}).click();
 const setup=page.getByRole('dialog',{name:'New conversation',exact:true});
 const picker=setup.getByRole('combobox',{name:'Agent',exact:true});
 await picker.focus();await page.keyboard.press('ArrowDown');
 const list=setup.getByRole('listbox',{name:'Agent'});
 await expect(list.getByRole('option')).toHaveText([/^Claude/,/^Codex/,/Gemini CLI\s*0\.68\.0\s*Preview/,/OpenCode\s*0\.9\.0\s*Preview/,/Team agent\s*Preview/]);
 await expect(list.locator('.agent-picker-group')).toHaveText('ACP agents · Preview');
 await expect(list.getByRole('option').filter({hasText:/^(Claude|Codex)/}).locator('.agent-preview')).toHaveCount(0);
 await expect(list.locator('svg').first()).toBeVisible();
 await page.screenshot({path:'.cache/acp-picker.png'});
 await page.keyboard.press('g');await expect(picker).toHaveAttribute('aria-activedescendant',/option-2$/);
 await page.keyboard.press('Enter');await expect(list).toHaveCount(0);await expect(picker).toBeFocused();
 await expect(picker).toContainText('Gemini CLI');await expect(setup.getByLabel('Account profile')).toHaveValue('acp-gemini');
 await expect(picker.locator('.agent-preview')).toHaveText('Preview');
 // Escape closes the list without changing the agent.
 await page.keyboard.press('ArrowDown');await expect(list).toBeVisible();await page.keyboard.press('Escape');await expect(list).toHaveCount(0);await expect(picker).toContainText('Gemini CLI');
 await setup.getByRole('button',{name:'Start Gemini CLI ↗',exact:true}).click();
 await expect.poll(()=>fixture.mutations.findLast(m=>m.method==='OpenSession')?.body.profileId).toBe('acp-gemini');

 // A newly registered agent launches through the same picker and profile path.
 await page.getByRole('button',{name:'New conversation',exact:true}).click();
 await picker.click();
 await list.getByRole('option',{name:/Team agent/}).click();
 await expect(setup.getByLabel('Account profile')).toHaveValue('acp-team-agent');
 await setup.getByRole('button',{name:'Start Team agent ↗',exact:true}).click();
 await expect.poll(()=>fixture.mutations.findLast(m=>m.method==='OpenSession')?.body.profileId).toBe('acp-team-agent');

 // "Add an agent…" in the picker opens Settings at Agent programs, with the form ready.
 await page.getByRole('button',{name:'New conversation',exact:true}).click();
 await setup.getByRole('combobox',{name:'Agent',exact:true}).click();await setup.getByRole('button',{name:'Add an agent…'}).click();
 await expect(page.getByRole('form',{name:'Add an ACP agent'}).getByLabel('Name',{exact:true})).toBeFocused();
 await page.keyboard.press('Escape');await expect(page.getByRole('form',{name:'Add an ACP agent'})).toHaveCount(0);

 // ── Removing the custom agent takes it out of the picker.
 await page.getByRole('region',{name:'Agent programs'}).locator('.acp-agent').filter({hasText:'Team agent'}).getByRole('button',{name:'Remove'}).click();
 await expect.poll(()=>fixture.mutations.findLast(m=>m.method==='RemoveAcpAgent')?.body.id).toBe('team-agent');
 await expect(page.getByRole('region',{name:'Agent programs'}).locator('.acp-agent').filter({hasText:'Team agent'})).toHaveCount(0);
 assert.deepEqual(errors,[]);
 console.log('PASS: ACP agents labelled Preview (picker, Settings) and a failed launch-flag check shown in Settings; ACP conversation from the real adapter journal (tool lines, plan, command, reply, approval with the agent’s Always), Settings → Agents (installed with version, missing with install command, custom agent added/edited/removed, env values never shown), keyboard agent picker with icons. Fixture only.');
}finally{await browser?.close();await server.close();await fixture.close();}
