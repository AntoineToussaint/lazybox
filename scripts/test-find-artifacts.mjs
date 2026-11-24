// Finding artifacts (ARTIFACTS.md, "Finding an artifact"): an agent's
// `lazybox artifact put` shows a card in the conversation that opens the page,
// and an update offers its changes; the chat header's chip counts this
// conversation's artifacts and opens the panel on them; ⌘K finds an artifact
// of another workspace and opens it there; "All workspaces" groups every
// workspace's artifacts; with none, nothing shows. Fixture only; WebKit by
// default (the desktop engine).
import {createServer} from './fixtures/vite-cache.mjs';
import react from '@vitejs/plugin-react';
import {webkit,chromium,expect} from './fixtures/playwright.mjs';
import {fileURLToPath} from 'node:url';
import {mkdir} from 'node:fs/promises';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import {testViteCache} from './fixtures/vite-cache.mjs';
import {monacoVimCompat} from './monaco-vim-compat.mjs';
import { collectPageErrors } from './fixtures/page-errors.mjs';

const fixture=await startRunnerFixture();const session=fixture.sessions[0];
Object.assign(session,{busy:false,status:'done',conversationId:'conversation-alpha',agents:[{id:'root',status:'done'}]});
const channel=fixture.openEvents(session.id);
const said=(title,id,version,kind)=>`Published “${title}” as artifact ${id}, version ${version} (${kind}). The user sees it in Lazybox → Artifacts. Publish a new version with --id ${id}.\n`;
let calls=0;
const tool=data=>channel.push({type:'tool_activity',turnId:'t1',providerDataJson:Buffer.from(JSON.stringify(data)).toString('base64')});
/** The agent runs a shell command and it completes with this output, as Claude reports it. */
const bash=(command,output,description)=>{const id=`toolu_${++calls}`;
 tool({type:'tool_call',id,name:'Bash',status:'running',input:{command,...(description?{description}:{})}});
 tool({type:'tool_call',id,name:'Bash',status:'completed',output:[{type:'text',text:output}]});};
channel.push({type:'operation_accepted',text:'Write the rollout plan'});
channel.push({type:'message_delta',text:'Looking at what exists first.',turnId:'t1'});
// A read of artifacts is no artifact.
bash('lazybox artifact list','No artifacts in this workspace yet.\n','List artifacts');
channel.push({type:'message_delta',text:'Nothing yet; I will write one.',turnId:'t1'});
const server=await createServer({...testViteCache(import.meta.url),configFile:false,root:fileURLToPath(new URL('../apps/web',import.meta.url)),logLevel:'warn',plugins:[react(),monacoVimCompat()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
let browser;
try{
 await mkdir('.cache',{recursive:true});await server.listen();browser=await (process.env.TEST_CHROMIUM?chromium:webkit).launch({headless:true});
 const context=await browser.newContext({viewport:{width:1500,height:920}});
 const page=await context.newPage();page.setDefaultTimeout(15000);
 const errors=[];await collectPageErrors(page, errors);
 await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=${session.id}#token=${fixture.token}`);
 const messages=page.locator('.messages:visible');
 await expect(messages.getByText('Nothing yet; I will write one.')).toBeVisible();

 // Nothing to find: no card, no chip, and the panel says in one sentence how artifacts appear.
 const chip=page.locator('.chat-heading').getByRole('button',{name:/artifacts? .*: show them$/});
 await expect(messages.locator('.artifact-card')).toHaveCount(0);
 await expect(chip).toHaveCount(0);
 const palette=page.getByRole('dialog',{name:'Command palette'}),search=palette.getByRole('combobox',{name:'Search commands'});
 const composer=page.locator('[data-agent-composer]:visible');
 await expect(composer).toBeEnabled();await composer.focus();
 await page.keyboard.press('Meta+k');await expect(palette).toBeVisible();
 await search.fill('*');
 await expect(palette.locator('.command-palette-scope')).toHaveText('Artifacts');
 await expect(palette.getByRole('status')).toHaveText('No artifacts in any workspace yet.');
 await search.fill('Show artifacts');await page.keyboard.press('Enter');
 const panel=page.getByRole('region',{name:'Artifacts'});
 await expect(panel.locator('.artifacts-empty')).toContainText('No artifacts yet');
 await expect(panel.locator('.artifacts-empty p')).toHaveText(/^Plans and reports show up here when an agent publishes one, or when you right-click a message or a Markdown file → Track as artifact\.$/);
 await page.screenshot({path:'.cache/find-artifacts-empty.png'});

 // The agent publishes: a card in the conversation, and the chip in the header.
 const plan='# Rollout plan\n\nShip in three phases.\n\n1. Phase one\n2. Phase two\n';
 const v1=fixture.artifacts.agentPut({collectionId:'alpha',sessionId:session.id,conversationId:'conversation-alpha',title:'Rollout plan',kind:'plan',content:plan});
 bash('lazybox artifact put PLAN.md --title "Rollout plan" --kind plan',said('Rollout plan',v1.id,1,'plan'),'Publish the plan');
 channel.push({type:'message_delta',text:'Published the plan.',turnId:'t1'});
 const card=messages.getByRole('group',{name:'Artifact “Rollout plan”, v1'});
 await expect(card).toBeVisible();
 await expect(card).toContainText('Plan · v1 · published by the agent');
 await expect(card.getByRole('button',{name:/^Compare/})).toHaveCount(0);
 await expect(chip).toHaveText('1 artifact',{timeout:20000});
 await page.screenshot({path:'.cache/find-artifacts-card.png'});
 // Open: the page in the workspace's Artifacts panel.
 await card.getByRole('button',{name:'Open'}).click();
 const article=page.getByRole('article',{name:'Rollout plan',exact:true});
 await expect(article.locator('.artifact-document')).toContainText('Ship in three phases.');

 // An update: its card offers the changes, and opens on them.
 const v2=fixture.artifacts.agentPut({collectionId:'alpha',sessionId:session.id,conversationId:'conversation-alpha',artifactId:v1.id,content:plan+'3. Phase three\n'});
 assert.equal(v2.contentVersion,2);
 channel.push({type:'operation_accepted',text:'Add phase three'});
 bash(`lazybox artifact put - --id ${v1.id} < PLAN.md`,said('Rollout plan',v1.id,2,'plan'));
 const updated=messages.getByRole('group',{name:'Artifact “Rollout plan”, v2'});
 await expect(updated).toContainText('Plan · v2 · updated by the agent');
 await updated.getByRole('button',{name:'Compare with v1'}).click();
 await expect(article.getByRole('tab',{name:'Changes'})).toHaveAttribute('aria-selected','true');
 await expect(article.getByRole('region',{name:'Rendered changes'})).toBeVisible();
 // Open on the older card: that version, with the way back to the latest.
 await card.getByRole('button',{name:'Open'}).click();
 await expect(article.getByRole('tab',{name:'Document'})).toHaveAttribute('aria-selected','true');
 await expect(article.getByRole('note')).toContainText('You are viewing v1 of 2.');

 // The chip counts this conversation's artifacts and opens the panel on them.
 fixture.artifacts.agentPut({collectionId:'alpha',sessionId:'other-session',conversationId:'conversation-other',title:'Security review',kind:'report',content:'# Security review\n\nNo blockers.\n'});
 await expect(chip).toHaveAttribute('title',/1 artifact from this conversation · 2 in this workspace/,{timeout:20000});
 await chip.click();
 const filter=panel.locator('.artifacts-conversation-filter');
 await expect(filter).toContainText('From');
 const rows=panel.getByRole('listbox',{name:'Artifacts'});
 await expect(rows.getByRole('option')).toHaveCount(1);
 await expect(rows.getByRole('option',{name:/Rollout plan/})).toBeVisible();
 await expect(panel.locator('.artifacts-list-header strong')).toHaveText('1 artifact from this conversation');
 await page.screenshot({path:'.cache/find-artifacts-chip.png'});
 // The workspace command resets the conversation filter even with the panel already open.
 await composer.focus();
 await page.keyboard.press('Meta+k');await expect(palette).toBeVisible();
 await search.fill('Show artifacts');await page.keyboard.press('Enter');
 await expect(filter).toHaveCount(0);
 await expect(rows.getByRole('option')).toHaveCount(2);

 // ⌘K finds an artifact of another workspace by its words or its workspace's, and opens it there.
 fixture.artifacts.agentPut({collectionId:'beta',sessionId:'beta-session',conversationId:'conversation-beta',title:'Beta design spec',kind:'spec',content:'# Beta design spec\n\nThe beta design.\n'});
 await composer.focus();
 await page.keyboard.press('Meta+k');await expect(palette).toBeVisible();
 await search.fill('*');
 const listed=palette.getByRole('option');
 await expect(listed).toHaveCount(3,{timeout:20000});
 await expect(listed.first()).toContainText('Beta design spec');
 await expect(listed.first()).toContainText('Spec · v1 · Beta release');
 await expect(palette.getByRole('option',{name:/Rollout plan/})).toContainText('This workspace');
 await search.fill('*beta spec');
 await expect(listed.first()).toContainText('Beta design spec');
 await page.keyboard.press('Enter');
 await expect(palette).toHaveCount(0);
 await expect(page.getByRole('combobox',{name:'Workspace',exact:true})).toHaveValue('beta');
 const beta=page.getByRole('article',{name:'Beta design spec'});
 await expect(beta.locator('.artifact-document')).toContainText('The beta design.');
 await page.screenshot({path:'.cache/find-artifacts-palette.png'});

 // All workspaces: every workspace's artifacts, grouped by workspace, the one changed last first.
 const back=beta.getByRole('button',{name:'Back to artifacts'});
 if(await back.isVisible())await back.click();
 await panel.getByRole('radiogroup',{name:'Show artifacts of'}).getByRole('radio',{name:'All workspaces'}).click();
 const everywhere=panel.getByRole('listbox',{name:'Artifacts of every workspace'});
 const groups=everywhere.getByRole('group');
 await expect(groups).toHaveCount(2,{timeout:20000});
 await expect(groups.nth(0)).toHaveAccessibleName('Beta release');
 await expect(groups.nth(1)).toHaveAccessibleName('Alpha feature');
 await expect(groups.nth(1).getByRole('option')).toHaveCount(2);
 await expect(groups.nth(1).getByRole('option').first()).toContainText('Security review');
 await expect(panel.locator('.artifacts-list-header strong')).toHaveText('3 artifacts in 2 workspaces');
 await page.screenshot({path:'.cache/find-artifacts-all.png'});
 // From the keyboard: the rows run across workspaces, and Enter reads one where it is.
 await groups.nth(0).getByRole('option').first().focus();
 await page.keyboard.press('ArrowDown');
 await expect(groups.nth(1).getByRole('option').first()).toBeFocused();
 await page.keyboard.press('Enter');
 await expect(page.getByRole('article',{name:'Security review'}).locator('.artifact-document')).toContainText('No blockers.');

 // Enough artifacts to filter: the header is one row (the count, then a search field with its icon
 // inside), the scope switch under it at one height, and no box drawn inside another.
 for(const [i,collectionId] of ['alpha','beta','alpha'].entries())fixture.artifacts.agentPut({collectionId,sessionId:`${collectionId}-session`,conversationId:`conversation-${collectionId}`,title:`Note ${i+1}`,kind:'plan',content:`# Note ${i+1}\n`});
 const backToList=page.getByRole('article',{name:'Security review'}).getByRole('button',{name:'Back to artifacts'});
 if(await backToList.isVisible())await backToList.click();
 const header=panel.locator('.artifacts-list-header'),artifactFilter=header.getByRole('searchbox',{name:'Filter artifacts'});
 await expect(header.locator('strong')).toHaveText('6 artifacts in 2 workspaces',{timeout:20000});
 await expect(artifactFilter).toBeVisible();
 const headerReport=()=>header.evaluate(node=>{
  const box=el=>el.getBoundingClientRect(),bordered=el=>{const s=getComputedStyle(el);return ['Top','Right','Bottom','Left'].some(side=>parseFloat(s[`border${side}Width`])>0&&s[`border${side}Style`]!=='none'&&!/^(transparent|rgba\(.*, 0\))$/.test(s[`border${side}Color`]));};
  const nested=[...node.querySelectorAll('*')].filter(el=>el.getClientRects().length&&bordered(el)&&[...el.querySelectorAll('*')].some(child=>child.getClientRects().length&&bordered(child))).map(el=>el.className);
  const count=box(node.querySelector('strong')),field=box(node.querySelector('.artifacts-search')),input=box(node.querySelector('.artifacts-search input')),icon=box(node.querySelector('.artifacts-search svg'));
  const radios=[...node.querySelectorAll('[role=radio]')].map(el=>Math.round(box(el).height));
  return {nested,countMid:count.top+count.height/2,fieldMid:field.top+field.height/2,countRight:count.right,field:{left:field.left,right:field.right,top:field.top,bottom:field.bottom},input:{left:input.left,right:input.right},icon:{left:icon.left,right:icon.right,top:icon.top,bottom:icon.bottom},radios,scope:node.querySelector('.artifacts-scope')&&box(node.querySelector('.artifacts-scope')).top,header:{left:box(node).left,right:box(node).right},overflow:node.scrollWidth>node.clientWidth+1};
 });
 for(const [width,label] of [[0,'wide'],[230,'narrow']]){
  if(width)await panel.locator('.artifacts-list').evaluate((node,width)=>{node.style.setProperty('width',`${width}px`,'important');node.style.setProperty('flex','none','important');},width);await page.waitForTimeout(100);
  for(const scheme of ['dark','light']){await page.emulateMedia({colorScheme:scheme});await page.waitForTimeout(150);await header.screenshot({path:`.cache/find-artifacts-header-${label}-${scheme}.png`});}
  await page.emulateMedia({colorScheme:null});
  const r=await headerReport();
  assert.deepEqual(r.nested,[],`${label}: no bordered box inside another`);
  assert.ok(r.icon.left>=r.field.left&&r.icon.right<=r.field.right&&r.icon.top>=r.field.top&&r.icon.bottom<=r.field.bottom,`${label}: the search icon is inside the field`);
  assert.ok(r.icon.right<=r.input.left+1,`${label}: the icon leads the text`);
  assert.ok(r.field.right<=r.header.right+1&&r.field.left>=r.header.left-1&&!r.overflow,`${label}: the field stays in the header`);
  assert.ok(new Set(r.radios).size===1,`${label}: the scope switch is one height (${r.radios})`);
  assert.ok(r.scope>r.field.bottom-1,`${label}: the scope switch is under the count and search`);
  if(label==='wide'){
   assert.ok(Math.abs(r.countMid-r.fieldMid)<2,'the count and the search share a row');
   assert.ok(r.field.left>r.countRight,'the search is right of the count');
   assert.ok(Math.abs(r.field.right-(r.header.right-12))<2,'the search ends at the header\'s padding');
  }
 }
 await artifactFilter.fill('Note 2');
 await expect(panel.getByRole('option')).toHaveCount(1);
 await artifactFilter.fill('');

 // Consolidating (ARTIFACTS.md, "Retiring and merging"): the agent merges three overlapping artifacts into one
 // with `put --supersedes`. The conversation says so with Undo; the panel hides the retired ones behind
 // "Show retired (N)"; a retired page says what superseded it, with Restore. All of it from the keyboard.
 await panel.locator('.artifacts-list').evaluate(node=>{node.style.removeProperty('width');node.style.removeProperty('flex');});
 // Back to the conversation's workspace, by ⌘K.
 await composer.focus();
 await page.keyboard.press('Meta+k');await expect(palette).toBeVisible();
 await search.fill('*Rollout plan');await expect(listed.first()).toContainText('Rollout plan');await page.keyboard.press('Enter');
 await expect(page.getByRole('combobox',{name:'Workspace',exact:true})).toHaveValue('alpha');
 const rollout=page.getByRole('article',{name:'Rollout plan',exact:true});
 await expect(rollout).toBeVisible();
 // A narrow panel shows the page instead of the list.
 if(await rollout.getByRole('button',{name:'Back to artifacts'}).isVisible())await rollout.getByRole('button',{name:'Back to artifacts'}).click();
 await panel.getByRole('radiogroup',{name:'Show artifacts of'}).getByRole('radio',{name:'This workspace'}).click();
 const agent={collectionId:'alpha',sessionId:session.id,conversationId:'conversation-alpha'};
 const overlapping=['Milestone plan','What\'s left','Limitations and what\'s next'].map(title=>fixture.artifacts.agentPut({...agent,title,kind:'plan',content:`# ${title}\n\nOverlapping.\n`}));
 await expect(panel.getByRole('option',{name:/Limitations and what's next/})).toBeVisible({timeout:20000});
 const status=fixture.artifacts.agentPut({...agent,title:'Project status',kind:'plan',content:'# Project status\n\nEverything in one place.\n',supersedes:overlapping.map(a=>a.id)});
 const retiredSaid=overlapping.map(a=>`Retired “${a.title}” (artifact ${a.id}), superseded by “Project status” (artifact ${status.id}).`).join('\n');
 channel.push({type:'operation_accepted',text:'Merge the overlapping documents'});
 bash(`lazybox artifact put STATUS.md --title "Project status" --kind plan --supersedes ${overlapping.map(a=>a.id).join(',')}`,said('Project status',status.id,1,'plan')+retiredSaid+'\nRetired artifacts keep every version; the person can restore them in Lazybox → Artifacts.\n');
 const merged=messages.getByRole('group',{name:'Merged 3 artifacts into “Project status”'});
 await expect(merged).toBeVisible();
 await expect(merged).toContainText('Retired “Milestone plan”, “What\'s left”, “Limitations and what\'s next” · kept with their versions');
 // The merge is its own card: no second card for the artifact it published.
 await expect(messages.getByRole('group',{name:'Artifact “Project status”, v1'})).toHaveCount(0);
 const undo=merged.getByRole('button',{name:'Undo: restore 3 artifacts'});
 await expect(undo).toBeVisible({timeout:20000});
 await page.screenshot({path:'.cache/find-artifacts-merged-card.png'});

 // The panel: only the live ones, and "Show retired (3)", reached and pressed from the keyboard.
 const workspaceRows=panel.getByRole('listbox',{name:'Artifacts'});
 await expect(workspaceRows.getByRole('option',{name:/Project status/})).toBeVisible({timeout:20000});
 await expect(workspaceRows.getByRole('option',{name:/Milestone plan/})).toHaveCount(0);
 const toggle=panel.getByRole('button',{name:'Show retired (3)'});
 await expect(toggle).toHaveAttribute('aria-pressed','false');
 await toggle.focus();await page.keyboard.press('Enter');
 await expect(panel.getByRole('button',{name:'Hide retired (3)'})).toHaveAttribute('aria-pressed','true');
 const retiredGroup=workspaceRows.getByRole('group',{name:'Retired'});
 await expect(retiredGroup.getByRole('option')).toHaveCount(3);
 await expect(retiredGroup.getByRole('option',{name:/Milestone plan/})).toContainText('Retired · superseded by “Project status”');
 for(const scheme of ['dark','light']){await page.emulateMedia({colorScheme:scheme});await page.waitForTimeout(150);await page.locator('.artifacts-panel').screenshot({path:`.cache/find-artifacts-retired-list-${scheme}.png`});}
 await page.emulateMedia({colorScheme:null});
 // ↑/↓ run from the live rows into the retired ones; Enter opens one, with its banner.
 const liveRows=workspaceRows.getByRole('option').filter({hasNot:page.locator('.artifact-row-retired')});
 await liveRows.last().focus();
 await page.keyboard.press('ArrowDown');
 await expect(retiredGroup.getByRole('option').first()).toBeFocused();
 const firstRetired=(await retiredGroup.getByRole('option').first().locator('strong').textContent()).trim();
 await page.keyboard.press('Enter');
 const retiredPage=page.getByRole('article',{name:firstRetired,exact:true});
 const banner=retiredPage.getByRole('note',{name:'Retired'});
 await expect(banner).toContainText('Retired: superseded by Project status');
 await expect(banner).toContainText('by an agent');
 await expect(retiredPage.locator('.artifact-document')).toContainText('Overlapping.');
 for(const scheme of ['dark','light']){await page.emulateMedia({colorScheme:scheme});await page.waitForTimeout(150);await retiredPage.screenshot({path:`.cache/find-artifacts-retired-banner-${scheme}.png`});}
 await page.emulateMedia({colorScheme:null});
 // The successor's name opens it.
 await banner.getByRole('button',{name:'Project status'}).focus();await page.keyboard.press('Enter');
 await expect(page.getByRole('article',{name:'Project status',exact:true}).locator('.artifact-document')).toContainText('Everything in one place.');
 // Back on the retired page, Restore (from the keyboard) brings it back to the list; the toggle counts the rest.
 // A narrow panel shows one page at a time: Back returns to the list.
 const toList=async()=>{const back=panel.locator('.artifact-back:visible');if(await back.count())await back.first().click();};
 await toList();
 await retiredGroup.getByRole('option',{name:firstRetired}).click();
 await banner.getByRole('button',{name:'Restore'}).focus();await page.keyboard.press('Enter');
 await expect(banner).toHaveCount(0);
 await expect(retiredPage.locator('.artifact-status')).toHaveText('Restored. It is back in the list.');
 await expect(retiredPage.locator('.artifact-more-menu > summary')).toBeFocused();
 await toList();
 await expect(panel.getByRole('button',{name:'Hide retired (2)'})).toBeVisible({timeout:20000});
 const restores=()=>fixture.mutations.filter(m=>m.method==='RetireArtifact'&&m.body.restore).map(m=>m.body.artifactId);
 assert.equal(restores().length,1);

 // The card's Undo restores the rest, and then says they are back.
 await merged.getByRole('button',{name:'Undo: restore 2 artifacts'}).focus();await page.keyboard.press('Enter');
 await expect(merged.locator('small[role=status]')).toContainText('Undone: “Milestone plan”, “What\'s left”, “Limitations and what\'s next” are back in the list',{timeout:20000});
 await expect(merged.getByRole('button',{name:/^Undo/})).toHaveCount(0);
 assert.deepEqual(new Set(restores()),new Set(overlapping.map(a=>a.id)));
 await expect(panel.getByRole('button',{name:/retired \(/})).toHaveCount(0,{timeout:20000});
 await page.screenshot({path:'.cache/find-artifacts-merged-undone.png'});

 // `lazybox artifact retire` alone: its own card, with Undo.
 fixture.artifacts.agentRetire({...agent,artifactId:overlapping[2].id,supersededBy:status.id});
 bash(`lazybox artifact retire ${overlapping[2].id} --superseded-by ${status.id}`,`Retired “Limitations and what's next” (artifact ${overlapping[2].id}), superseded by “Project status” (artifact ${status.id}). Retired artifacts keep every version; the person can restore them in Lazybox → Artifacts.\n`);
 const retiredCard=messages.getByRole('group',{name:'Retired “Limitations and what\'s next”'});
 await expect(retiredCard).toContainText('Superseded by “Project status” · kept with its versions');
 await expect(retiredCard.getByRole('button',{name:'Undo: restore “Limitations and what\'s next”'})).toBeVisible({timeout:20000});

 // The person retires and restores from the page's ⋯ menu.
 await toList();
 await workspaceRows.getByRole('option',{name:/^Project status/}).click();
 const statusPage=page.getByRole('article',{name:'Project status',exact:true});
 const more=statusPage.locator('.artifact-more-menu > summary');
 await more.click();
 await page.getByRole('group',{name:'Artifact actions menu'}).getByRole('button',{name:'Retire',exact:true}).click();
 await expect(statusPage.getByRole('note',{name:'Retired'})).toContainText('by you');
 await more.click();
 await page.getByRole('group',{name:'Artifact actions menu'}).getByRole('button',{name:'Restore',exact:true}).click();
 await expect(statusPage.getByRole('note',{name:'Retired'})).toHaveCount(0);
 assert.deepEqual(errors,[]);
 console.log('PASS: an agent\'s artifact put shows a card that opens the page (and an update its changes); a merge (put --supersedes) shows "Merged 3 artifacts into …" with Undo, the panel hides retired artifacts behind "Show retired (N)", a retired page says what superseded it with Restore, retire alone has its card, and the person retires and restores from ⋯, all from the keyboard; the header chip counts this conversation\'s artifacts and opens the panel on them; ⌘K (and its * prefix) finds an artifact of another workspace and opens it there; All workspaces groups by workspace; with none, no card or chip and a one-sentence empty state. Fixture only.');
}finally{await browser?.close();await server.close();await fixture.close();}
