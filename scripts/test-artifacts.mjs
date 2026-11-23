// Artifacts (ARTIFACTS.md): an agent's plan in a chat message becomes its own
// page from the message's right-click menu or its hover action; a later
// message updates it after a reviewed diff; the Artifacts panel lists it with
// its versions; it opens in its own window; it exports (copy, download); a
// Markdown file is tracked from Files and follows changes on disk; an agent's
// `lazybox artifact put` shows up; deleting asks first; a report reads
// densely (one title, the kind once, a wide column, the outline as a toggle
// below 900 px, no horizontal scroll at 360 px). Fixture only;
// WebKit by default (the desktop engine).
import {createServer} from 'vite';
import react from '@vitejs/plugin-react';
import {webkit,chromium,expect} from './fixtures/playwright.mjs';
import {fileURLToPath} from 'node:url';
import {mkdir,readFile} from 'node:fs/promises';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import {testViteCache} from './fixtures/vite-cache.mjs';
import {monacoVimCompat} from './monaco-vim-compat.mjs';
import {reportTitle,seedReport} from './fixtures/artifact-sample.mjs';

const fixture=await startRunnerFixture();const session=fixture.sessions[0];
Object.assign(session,{busy:false,status:'done',conversationId:'conversation-alpha',agents:[{id:'root',status:'done'}]});
fixture.collections.push({id:'former',name:'Former project',state:'deleted',keepArtifacts:true,members:[]});
fixture.artifacts.agentPut({collectionId:'former',title:'Kept after deletion',kind:'report',content:'Historical results'});
const channel=fixture.openEvents(session.id);
const plan1='# Datasource migration plan\n\nMove reads first, then writes.\n\n## Steps\n1. Phase one: dual read\n2. Phase two: dual write\n3. Phase three: cut over\n';
const plan2=plan1.replace('2. Phase two: dual write','2. Phase two: dual write behind a flag').replace('3. Phase three: cut over','3. Phase three: cut over\n4. Phase four: remove the old path');
channel.push({type:'operation_accepted',text:'Write the migration plan'});
channel.push({type:'message_delta',text:plan1,turnId:'t1'});
const server=await createServer({...testViteCache(import.meta.url),configFile:false,root:fileURLToPath(new URL('../apps/web',import.meta.url)),logLevel:'warn',plugins:[react(),monacoVimCompat()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
let browser;
try{
 await mkdir('.cache',{recursive:true});await server.listen();browser=await (process.env.TEST_CHROMIUM?chromium:webkit).launch({headless:true});
 const context=await browser.newContext({viewport:{width:1400,height:900},acceptDownloads:true});
 const page=await context.newPage();page.setDefaultTimeout(15000);
 const errors=[];page.on('pageerror',e=>errors.push(String(e)));
 // Copies are recorded; the rest of the real clipboard (which the editor uses) is left alone.
 await page.addInitScript(()=>{window.__copies=[];if(navigator.clipboard)navigator.clipboard.writeText=async text=>{window.__copies.push(text);};else Object.defineProperty(navigator,'clipboard',{configurable:true,value:{writeText:async text=>{window.__copies.push(text);}}});});
 // Printing is counted, not shown: the system dialog would block the page.
 await page.addInitScript(()=>{window.__prints=0;window.print=()=>{window.__prints++;setTimeout(()=>dispatchEvent(new Event('afterprint')));};});
 await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=${session.id}#token=${fixture.token}`);
 const message=page.locator('.messages:visible article.message.message_delta').filter({hasText:'Datasource migration plan'}).first();
 await expect(message).toBeVisible();

 // The hover action is quiet until the message is hovered; prompts have none.
 const hover=message.getByRole('button',{name:'Track as artifact…'});
 await message.hover();await expect(hover).toBeVisible();
 await expect(page.locator('.messages:visible article.message.operation_accepted').getByRole('button',{name:'Track as artifact…'})).toHaveCount(0);

 // Right-click → Track as artifact…: title from the first heading, kind suggested.
 await message.click({button:'right'});
 const menu=page.getByRole('menu',{name:'Message actions'});
 // The app's message menu, with the artifact items the feature contributes to it.
 await expect(menu.getByRole('menuitem',{name:'Track as artifact…'})).toBeVisible();await expect(menu.getByRole('menuitem',{name:'Copy as Markdown'})).toBeVisible();
 await menu.getByRole('menuitem',{name:'Track as artifact…'}).click();
 const track=page.getByRole('dialog',{name:'Track as artifact'});
 await expect(track.getByLabel('Title')).toHaveValue('Datasource migration plan');
 await expect(track.getByRole('radio',{name:'Plan'})).toHaveAttribute('aria-checked','true');
 await expect(track.getByRole('region',{name:'Preview'})).toContainText('Phase three: cut over');
 await track.getByRole('button',{name:'Track artifact'}).click();
 await expect(track).toHaveCount(0);
 const put=fixture.mutations.find(m=>m.method==='PutArtifact');
 assert.equal(put.body.source.kind,'message');assert.equal(put.body.source.sessionId,session.id);assert.equal(put.body.source.conversationId,'conversation-alpha');assert.ok(put.body.source.messageId);

 // The Artifacts panel opens on the new page.
 const artifactPage=page.getByRole('article',{name:'Datasource migration plan'});
 await expect(artifactPage.getByRole('heading',{name:'Datasource migration plan',level:1})).toBeVisible();
 await expect(artifactPage.locator('.artifact-document')).toContainText('Phase one: dual read');
 await expect(artifactPage.locator('.artifact-meta')).toContainText('From a chat message');
 await expect(artifactPage.getByLabel('Version',{exact:true})).toHaveValue('1');
 await page.screenshot({path:'.cache/artifacts-tracked.png'});

 // A later message of the same conversation offers an update, shown as a diff first.
 channel.push({type:'operation_accepted',text:'Refine it'});
 channel.push({type:'message_delta',text:plan2,turnId:'t2'});
 const newer=page.locator('.messages:visible article.message.message_delta').filter({hasText:'Phase four'});
 await expect(newer).toBeVisible();
 await newer.click({button:'right'});
 await page.getByRole('menuitem',{name:'Update “Datasource migration plan” from this message…'}).click();
 const update=page.getByRole('dialog',{name:'Update artifact'});
 const diff=update.getByRole('region',{name:'Changes from v1'});
 await expect(diff.locator('.artifact-diff-line.is-add')).toHaveCount(2);
 await expect(diff.locator('.artifact-diff-line.is-remove')).toHaveCount(1);
 await expect(update.locator('.artifact-diff-stats')).toContainText('+2−1');
 await page.screenshot({path:'.cache/artifacts-update-diff.png'});
 await update.getByRole('button',{name:'Save as v2'}).click();
 await expect(update).toHaveCount(0);
 await expect(artifactPage.locator('.artifact-document')).toContainText('Phase four: remove the old path');
 // History is beside the document when the page is wide, and opened on demand over it when narrow.
 const versions=artifactPage.getByRole('listbox',{name:'Versions'});
 if(!await versions.isVisible())await artifactPage.getByRole('button',{name:'2 versions'}).click();
 await expect(versions.getByRole('option')).toHaveCount(2);
 if(await artifactPage.getByRole('button',{name:'Close history'}).isVisible())await artifactPage.getByRole('button',{name:'Close history'}).click();
 // The original message has nothing newer to offer for this artifact.
 await message.click({button:'right'});
 await expect(page.getByRole('menu',{name:'Message actions'}).getByRole('menuitem',{name:'Track as artifact…'})).toBeVisible();
 await expect(page.getByRole('menu',{name:'Message actions'}).getByRole('menuitem',{name:/^Update “/})).toHaveCount(0);
 await page.keyboard.press('Escape');await expect(page.getByRole('menu',{name:'Message actions'})).toHaveCount(0);

 // Changes between versions, and an older version with a way back.
 await artifactPage.getByRole('tab',{name:'Changes'}).click();
 // Rendered side by side by default; the line diff is one click away.
 await expect(artifactPage.getByRole('region',{name:'Rendered changes'}).locator('.doc-diff-row.is-changed')).toHaveCount(1);
 await artifactPage.getByRole('radio',{name:'Text'}).click();
 await expect(artifactPage.getByRole('region',{name:'Changes from v1 to v2'}).locator('.artifact-diff-line.is-add')).toHaveCount(2);
 // The chosen one shows: Text is filled, Rendered is not.
 await expect.poll(async()=>{const bg=locator=>locator.evaluate(node=>getComputedStyle(node).backgroundColor);return (await bg(artifactPage.getByRole('radio',{name:'Text'})))!==(await bg(artifactPage.getByRole('radio',{name:'Rendered'})));}).toBe(true);
 // Print from Changes shows the document and prints it once, not again and again.
 await artifactPage.getByLabel('Export',{exact:true}).click();
 await page.getByRole('button',{name:'Print or save as PDF…'}).click();
 await expect(artifactPage.getByRole('tab',{name:'Document'})).toHaveAttribute('aria-selected','true');
 // Print waits (up to 4 s) for diagrams still drawing, so give it that long.
 await expect.poll(()=>page.evaluate(()=>window.__prints),{timeout:10000}).toBe(1);
 await page.waitForTimeout(600);
 assert.equal(await page.evaluate(()=>window.__prints),1,'printed once');
 await artifactPage.getByRole('tab',{name:'Document'}).click();
 if(!await versions.isVisible())await artifactPage.getByRole('button',{name:'2 versions'}).click();
 // ↑/↓ move through the versions from the keyboard.
 await versions.getByRole('option',{name:/^v2/}).focus();
 await page.keyboard.press('ArrowDown');
 await expect(versions.getByRole('option',{name:/^v1/})).toBeFocused();
 await page.keyboard.press('Enter');
 await expect(artifactPage.getByRole('note')).toContainText('You are viewing v1 of 2.');
 await expect(artifactPage.locator('.artifact-document')).not.toContainText('Phase four');
 await artifactPage.getByLabel('Export',{exact:true}).click();
 const [olderMarkdown]=await Promise.all([page.waitForEvent('download'),page.getByRole('button',{name:'Download .md',exact:true}).click()]);
 assert.equal(await readFile(await olderMarkdown.path(),'utf8'),plan1,'Markdown export uses the selected revision');
 await artifactPage.getByRole('button',{name:'Back to latest'}).click();
 await expect(artifactPage.locator('.artifact-document')).toContainText('Phase four');

 // Export: copy as Markdown and download a .md file.
 await artifactPage.getByLabel('Export',{exact:true}).click();
 await page.getByRole('button',{name:'Copy as Markdown'}).click();
 await expect.poll(()=>page.evaluate(()=>window.__copies.at(-1))).toBe(plan2);
 await artifactPage.getByLabel('Export',{exact:true}).click();
 const [download]=await Promise.all([page.waitForEvent('download'),page.getByRole('button',{name:'Download .md'}).click()]);
 assert.equal(download.suggestedFilename(),'datasource-migration-plan.md');
 assert.equal(await readFile(await download.path(),'utf8'),plan2);

 // Rename in place.
 await artifactPage.getByRole('heading',{name:'Datasource migration plan',level:1}).dblclick();
 await page.getByLabel('Artifact title').fill('Datasource rollout plan');await page.keyboard.press('Enter');
 const renamed=page.getByRole('article',{name:'Datasource rollout plan'});
 await expect(renamed.getByRole('heading',{name:'Datasource rollout plan',level:1})).toBeVisible();

 // Its own window: the same page, following the same artifact.
 const [detached]=await Promise.all([context.waitForEvent('page'),renamed.getByRole('button',{name:'Open in its own window'}).click()]);
 await detached.waitForLoadState();
 await expect(detached.getByRole('heading',{name:'Datasource rollout plan',level:1})).toBeVisible();
 await expect(detached.locator('.artifact-document')).toContainText('Phase four');
 assert.match(detached.url(),/view=artifact/);
 await detached.screenshot({path:'.cache/artifacts-window.png'});
 // Focus from the same opener, then from a separate browsing context in
 // the same browser session. The selected historical version survives both.
 await detached.getByLabel('Version',{exact:true}).selectOption('1');
 await expect(detached.locator('.artifact-old-version')).toContainText('v1 of 2');
 await detached.evaluate(()=>{window.__focusCalls=0;window.focus=()=>{window.__focusCalls++;};});
 const detach=renamed.getByRole('button',{name:'Open in its own window'});
 await detach.click();
 await expect.poll(()=>detached.evaluate(()=>window.__focusCalls)).toBe(1);
 const other=await context.newPage();
 await other.goto(new URL('/popout.html',page.url()).href);
 const target=new URL(detached.url()).searchParams;
 await other.evaluate(async ({ token, collectionId, artifactId })=>{
  const { browserWorkspaceHost }=await import('/src/browser-host.ts');
  const digest=await crypto.subtle.digest('SHA-256',new TextEncoder().encode(location.origin+'\0'+token));
  const scope=Array.from(new Uint8Array(digest),byte=>byte.toString(16).padStart(2,'0')).join('');
  await browserWorkspaceHost(scope,token).openArtifact({ collectionId,artifactId });
 },{token:fixture.token,collectionId:target.get('collection'),artifactId:target.get('artifact')});
 await expect.poll(()=>detached.evaluate(()=>window.__focusCalls)).toBe(2);
 assert.equal(context.pages().length,3,'a second opener must focus the same artifact window');
 await expect(detached.getByLabel('Version',{exact:true})).toHaveValue('1');
 await other.close();
 await detached.close();
 const [reopened]=await Promise.all([context.waitForEvent('page'),detach.click()]);
 await expect(reopened.getByRole('heading',{name:'Datasource rollout plan',level:1})).toBeVisible();
 await reopened.close();


 // An agent's `lazybox artifact put` appears in the list.
 fixture.artifacts.agentPut({collectionId:'alpha',sessionId:session.id,conversationId:'conversation-alpha',title:'Review findings',kind:'report',content:'# Review findings\n\nNo blockers.\n'});
 const back=renamed.getByRole('button',{name:'Back to artifacts'});
 if(await back.isVisible())await back.click();
 const list=page.getByRole('listbox',{name:'Artifacts'});
 await expect(list.getByRole('option',{name:/Review findings/})).toContainText('by an agent',{timeout:10000});

 // A Markdown file tracked from Files follows changes on disk.
 await page.locator('summary[aria-label="Panels"]').click();
 await page.getByRole('button', { name: 'Show Files', exact: true }).click();
 const tree=page.getByRole('tree',{name:'Workspace files'});
 const readme=tree.getByRole('treeitem',{name:/README\.md/}).first();
 await readme.click({button:'right'});
 const fileMenu=page.getByRole('menu',{name:/README\.md actions/});
 await expect(fileMenu.getByRole('menuitem',{name:'Track as artifact'})).toBeEnabled();
 await fileMenu.getByRole('menuitem',{name:'Track as artifact'}).click();
 await expect.poll(()=>fixture.mutations.some(m=>m.method==='TrackArtifactFile')).toBe(true);
 const tracked=fixture.mutations.find(m=>m.method==='TrackArtifactFile');assert.match(tracked.body.path,/README\.md$/);
 const filePage=page.getByRole('article',{name:'File preview'});
 await expect(filePage.locator('.doc-title-block')).toContainText(`Synced with ${tracked.body.path}`);
 fixture.artifacts.fileChanged('alpha',tracked.body.path,'# File preview\nEdited by an agent on disk.\n');
 await expect(filePage.locator('.artifact-document')).toContainText('Edited by an agent on disk.',{timeout:10000});
 await expect(filePage.locator('.artifact-meta')).toContainText('File changed on disk');
 await expect(filePage.getByLabel('Version',{exact:true})).toHaveValue('2');

 // Deleting asks first, then the page and its row are gone.
 await filePage.getByLabel('Artifact actions',{exact:true}).click();
 await page.getByRole('button',{name:'Delete…'}).click();
 const confirm=page.getByRole('dialog',{name:'Delete artifact'});
 await expect(confirm).toContainText('stays in the repository');
 await confirm.getByRole('button',{name:'Delete artifact'}).click();
 await expect(page.getByRole('article',{name:'File preview'})).toHaveCount(0);
 await expect(list.getByRole('option',{name:/File preview/})).toHaveCount(0);
 await expect(list.getByRole('option')).toHaveCount(2);
 // An artifact made in another workspace is one click away, not lost: All workspaces lists it under its workspace.
 fixture.artifacts.agentPut({collectionId:'beta',sessionId:'beta-session',conversationId:'conversation-beta',title:'Plan from Beta',kind:'plan',content:'# Plan from Beta\n\nSteps.\n'});
 const scopes=page.getByRole('radiogroup',{name:'Show artifacts of'});
 await scopes.getByRole('radio',{name:'All workspaces'}).click();
 const elsewhere=page.getByRole('listbox',{name:'Artifacts of every workspace'});
 await expect(elsewhere.getByRole('group',{name:'Beta release'}).getByRole('option',{name:/Plan from Beta/})).toBeVisible({timeout:20000});
 const kept=elsewhere.getByRole('group',{name:'Former project (deleted workspace)'}).getByRole('option',{name:/Kept after deletion/});
 await kept.click();
 const keptPage=page.getByRole('article',{name:'Kept after deletion'});
 await expect(keptPage).toContainText('Historical results');
 if(await keptPage.getByRole('button',{name:'Back to artifacts'}).isVisible())await keptPage.getByRole('button',{name:'Back to artifacts'}).click();
 await elsewhere.getByRole('option',{name:/Plan from Beta/}).click();
 await expect(page.getByRole('article',{name:'Plan from Beta'})).toBeVisible();
 await page.screenshot({path:'.cache/artifacts-panel.png'});
 const fromBeta=page.getByRole('article',{name:'Plan from Beta'});
 await fromBeta.getByLabel('Artifact actions',{exact:true}).click();
 await expect(fromBeta.getByRole('button',{name:'Copy mission proposal brief'})).toBeVisible();
 await expect(fromBeta.getByRole('button',{name:'Draft mission proposal in chat…'})).toHaveCount(0);
 await fromBeta.getByLabel('Artifact actions',{exact:true}).click();
 if(await fromBeta.getByRole('button',{name:'Back to artifacts'}).isVisible())await fromBeta.getByRole('button',{name:'Back to artifacts'}).click();
 await scopes.getByRole('radio',{name:'This workspace'}).click();
 await expect(fromBeta).toHaveCount(0);

 // A Markdown file an agent names is tracked from its link: the artifact follows the file, not the line of chat.
 fixture.artifacts.fileChanged('alpha','docs/PLAN.md','# Rollout plan\n\nShip it in three phases.\n');
 channel.push({type:'operation_accepted',text:'Write the rollout plan to a file'});
 channel.push({type:'message_delta',text:'Wrote `docs/PLAN.md`; the old draft in `src/main.go` and `/elsewhere/missing/NOTES.md` stays.',turnId:'t3'});
 const wrote=page.locator('.messages:visible article.message.message_delta').filter({hasText:'Wrote docs/PLAN.md'});
 const planLink=wrote.getByRole('button',{name:'docs/PLAN.md'});
 await planLink.click({button:'right'});
 const linkMenu=page.getByRole('menu');
 // The link's own item, then the message's, which says it is about the message.
 await expect(linkMenu.getByRole('menuitem',{name:'Track as artifact',exact:true})).toBeEnabled();
 await expect(linkMenu.getByRole('menuitem',{name:'Track this message as artifact…'})).toBeVisible();
 await expect(linkMenu.getByRole('menuitem',{name:'Track as artifact…'})).toHaveCount(0);
 const putsBefore=fixture.mutations.filter(m=>m.method==='PutArtifact').length;
 await linkMenu.getByRole('menuitem',{name:'Track as artifact',exact:true}).click();
 await expect.poll(()=>fixture.mutations.filter(m=>m.method==='TrackArtifactFile').map(m=>m.body.path)).toContain('docs/PLAN.md');
 const planPage=page.getByRole('article',{name:'Rollout plan'});
 await expect(planPage.locator('.doc-title-block')).toContainText('Synced with docs/PLAN.md');
 await expect(planPage.locator('.artifact-document')).toContainText('Ship it in three phases.');
 assert.equal(fixture.mutations.filter(m=>m.method==='PutArtifact').length,putsBefore,'the file is tracked, not the message');
 // Only Markdown can be tracked, as in the Files tree.
 await wrote.getByRole('button',{name:'src/main.go'}).click({button:'right'});
 await expect(page.getByRole('menu').getByRole('menuitem',{name:/^Track as artifact/}).first()).toBeDisabled();
 await expect(page.getByRole('menu')).toContainText('Markdown only');
 await page.keyboard.press('Escape');
 // A file outside the workspace: the Runner's refusal, in the file-link notice.
 await wrote.getByRole('button',{name:'/elsewhere/missing/NOTES.md'}).click({button:'right'});
 await page.getByRole('menu').getByRole('menuitem',{name:'Track as artifact',exact:true}).click();
 await expect(page.locator('.file-link-notice[role="alert"]')).toContainText('file is outside this workspace');
 await page.locator('.file-link-notice').getByRole('button',{name:'Dismiss'}).click();

 // The message's own Track: a short message naming a Markdown file offers the file first, chosen already.
 await wrote.hover();await wrote.getByRole('button',{name:'Track as artifact…'}).click();
 const trackWrote=page.getByRole('dialog',{name:'Track as artifact'});
 const fileChoice=trackWrote.getByRole('radio',{name:/^Track docs\/PLAN\.md/});
 await expect(fileChoice).toBeChecked();await expect(fileChoice).toBeFocused();
 await expect(trackWrote.getByLabel('Title')).toHaveCount(0);
 await expect(trackWrote.getByRole('button',{name:'Track file'})).toBeEnabled();
 await page.screenshot({path:'.cache/artifacts-track-file-choice.png'});
 await trackWrote.getByRole('radio',{name:/this message’s text/}).check();
 await expect(trackWrote.getByLabel('Title')).toBeVisible();
 await trackWrote.getByRole('button',{name:'Cancel'}).click();await expect(trackWrote).toHaveCount(0);
 // From the link, the message's item tracks the message: its text is chosen, the file still offered.
 await planLink.click({button:'right'});
 await page.getByRole('menuitem',{name:'Track this message as artifact…'}).click();
 await expect(trackWrote.getByRole('radio',{name:/this message’s text/})).toBeChecked();
 await expect(trackWrote.getByRole('radio',{name:/^Track docs\/PLAN\.md/})).not.toBeChecked();
 await trackWrote.getByRole('button',{name:'Cancel'}).click();await expect(trackWrote).toHaveCount(0);
 // A long reply is its own content: no file to choose from.
 await message.hover();await message.getByRole('button',{name:'Track as artifact…'}).click();
 await expect(trackWrote.getByRole('radio',{name:/^Track /})).toHaveCount(0);
 await trackWrote.getByRole('button',{name:'Cancel'}).click();
 // From the keyboard: the chosen file has the focus, and Enter tracks it.
 const fileTracks=()=>fixture.mutations.filter(m=>m.method==='TrackArtifactFile').length,tracksBefore=fileTracks();
 await wrote.hover();await wrote.getByRole('button',{name:'Track as artifact…'}).click();
 await expect(fileChoice).toBeFocused();await page.keyboard.press('Enter');
 await expect(trackWrote).toHaveCount(0);
 assert.equal(fileTracks(),tracksBefore+1);assert.equal(fixture.mutations.filter(m=>m.method==='TrackArtifactFile').at(-1).body.path,'docs/PLAN.md');
 await expect(planPage.locator('.doc-title-block')).toContainText('Synced with docs/PLAN.md');

 // Density: a report whose first heading nearly repeats its title, in its own window.
 const report=seedReport(fixture);
 const reader=await context.newPage();reader.setDefaultTimeout(15000);reader.on('pageerror',e=>errors.push(String(e)));
 await reader.setViewportSize({width:1000,height:800});
 await reader.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=artifact&collection=alpha&artifact=${report.id}#token=${fixture.token}`);
 const reportPage=reader.getByRole('article',{name:reportTitle});
 // One title: the artifact's; the document's own near-copy of it is not shown.
 await expect(reportPage.getByRole('heading',{level:1})).toHaveCount(1);
 await expect(reportPage.getByRole('heading',{level:1})).toHaveText(reportTitle);
 await expect(reportPage.locator('.doc-body')).not.toContainText('Auth compared');
 await expect(reportPage.getByRole('note',{name:'Note'})).toContainText('This is the final pass');
 // The kind is named once, in the toolbar, where it changes.
 await expect(reportPage.getByLabel('Kind',{exact:true})).toHaveValue('report');
 await expect(reportPage.getByText('Report',{exact:true}).filter({visible:true})).toHaveCount(1);
 await expect(reportPage.locator('.doc-title-block')).not.toContainText('Report');
 // At 1000 px the title has room to breathe and the outline stays alongside the prose.
 const layout=()=>reportPage.evaluate(article=>{
  const box=node=>node.getBoundingClientRect(),scroller=article.querySelector('.artifact-scroll'),toolbar=box(article.querySelector('.artifact-toolbar')),title=article.querySelector('.doc-title'),rail=article.querySelector('.doc-toc-rail');
  return {text:box(article.querySelector('.doc-body > p')).width,title:parseFloat(getComputedStyle(title).fontSize),titleTop:box(title).top-toolbar.bottom,rail:getComputedStyle(rail).display==='none'?0:box(rail).width,overflow:scroller.scrollWidth-scroller.clientWidth,pageOverflow:document.documentElement.scrollWidth-innerWidth};
 });
 const wide=await layout();
 assert.ok(wide.text>=660,`a ${wide.text}px text column at 1000 px`);
 assert.ok(wide.title>=38&&wide.title<=48,`a ${wide.title}px title`);
 assert.ok(wide.titleTop>=34&&wide.titleTop<=58,`the title ${wide.titleTop}px under the toolbar`);
 assert.ok(wide.rail>0&&wide.rail<=200,`a ${wide.rail}px outline`);
 await expect(reportPage.getByRole('navigation',{name:'On this page'})).toBeVisible();
 await expect(reportPage.locator('.doc-contents-menu')).toBeHidden();
 const sideOutline = reportPage.getByRole('navigation', {name:'On this page'});
 await sideOutline.locator('summary').click();
 await expect(sideOutline.getByRole('link')).toHaveCount(0);
 await sideOutline.locator('summary').click();
 await expect(sideOutline.getByRole('link').first()).toBeVisible();
 await reader.setViewportSize({width:1500,height:800});
 const widths = await reportPage.evaluate(article => ({
   prose: article.querySelector('.doc-body > p').getBoundingClientRect().width,
   wide: article.querySelector('.doc-body > .mermaid-block, .doc-body > .doc-table, .doc-body > .code-block')?.getBoundingClientRect().width,
 }));
 assert.ok(widths.wide > widths.prose, 'wide blocks use space beyond the prose measure');
 // Below 900 px of reader the outline is a toggle, and the text takes its room.
 await reader.setViewportSize({width:820,height:800});
 await expect(reportPage.getByRole('navigation',{name:'On this page'})).toBeHidden();
 const narrow=await layout();
 assert.equal(narrow.rail,0);
 assert.ok(narrow.text>=820-2*50,`the text column is ${narrow.text}px without the outline (${wide.text}px beside it)`);
 await reportPage.locator('.doc-contents-menu > summary').click();
 const outline=reader.getByRole('group',{name:'On this page menu'});
 await expect(outline.getByRole('button')).toHaveCount(7);
 await outline.getByRole('button',{name:'Risks'}).click();
 // The jump lands on the section, or as near as the end of a short report lets it.
 await expect.poll(()=>reportPage.locator('#risks').evaluate(h=>{const s=h.closest('.artifact-scroll'),top=h.getBoundingClientRect().top-s.getBoundingClientRect().top;return s.scrollTop>0&&top<s.clientHeight&&(top<80||s.scrollTop+s.clientHeight>=s.scrollHeight-2);})).toBe(true);
 // 360 px, as beside a chat: no horizontal scroll, no outline.
 await reader.setViewportSize({width:360,height:760});
 await reportPage.locator('.artifact-scroll').evaluate(node=>{node.scrollTop=0;});
 await expect(reportPage.getByRole('navigation',{name:'On this page'})).toBeHidden();
 const phone=await layout();
 assert.ok(phone.overflow<=0&&phone.pageOverflow<=0,`no horizontal scroll at 360 px (${phone.overflow}, ${phone.pageOverflow})`);
 assert.ok(phone.title>=26&&phone.title<=32,`a ${phone.title}px title at 360 px`);
 await expect(reportPage.getByLabel('Kind',{exact:true})).toBeVisible();
 await reader.screenshot({path:'.cache/artifacts-report-360.png'});
 await reader.close();
 assert.deepEqual(errors,[]);
 console.log('PASS: track a chat message as an artifact (right-click and hover), update it after a reviewed diff, browse versions and changes, copy and download, rename, open in its own window, list an agent\'s artifact, track a Markdown file that follows disk changes, print from Changes (once), keyboard through the history, delete with confirmation, track a file an agent links (from the link, and offered first by a short message\'s Track), and read a report densely: one title, the kind once, a wide column, the outline as a toggle when narrow, no horizontal scroll at 360 px. Fixture only.');
}finally{await browser?.close();await server.close();await fixture.close();}
