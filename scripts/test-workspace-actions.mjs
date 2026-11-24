import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from './fixtures/vite-cache.mjs';
import react from '@vitejs/plugin-react';
import {chromium,webkit,expect} from './fixtures/playwright.mjs';
import {fileURLToPath} from 'node:url';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
const fixture=await startRunnerFixture();
const regression=process.env.REPRO_OLD_BLUR?{name:'reproduce-old-blur',enforce:'pre',transform(code,id){if(id.endsWith('/CollectionPanel.tsx'))return code.replace('<details className="workspace-actions"','<details className="workspace-actions" onBlur={event=>{if(!event.currentTarget.contains(event.relatedTarget as Node))event.currentTarget.open=false;}}');}}:null;
const server=await createServer({...testViteCache(import.meta.url),configFile:false,root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[regression,react()].filter(Boolean),server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
let browser;
try{
 await server.listen();browser=await(process.env.TEST_WEBKIT?webkit:chromium).launch({headless:true});const page=await browser.newPage({viewport:{width:1250,height:900}});page.setDefaultTimeout(8000);
 await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`,{waitUntil:'domcontentloaded',timeout:30000});
 const rows=page.locator('.workspace-row');await expect(rows).toHaveCount(2);
 for(const row of await rows.all()){const name=await row.locator('.workspace-label strong').boundingBox(),bounds=await row.boundingBox(),action=await row.locator('summary').boundingBox();assert.ok(Math.abs((name.y+name.height/2)-(action.y+action.height/2))<2,'actions must align with workspace name');assert.ok(action.x+action.width<=bounds.x+bounds.width,'actions remain inside the row at its right edge');}
 await expect(page.locator('.workspace-conversations')).toHaveCount(0);await expect(rows.first().getByRole('button',{name:'New conversation in Alpha feature'})).toHaveCount(1);
 // Exercise a different workspace without changing the currently viewed chat.
 const actions=page.getByLabel('Actions for Beta release',{exact:true});await actions.click();
 // A short, scrolling sidebar must not clip actions below its boundary.
 await page.addStyleTag({content:'.collection-panel{max-height:155px!important;overflow:auto!important}'});
 const deleteAction=page.getByRole('button',{name:'Delete workspace…',exact:true});
 await expect(deleteAction).toBeVisible();
 await expect.poll(()=>deleteAction.evaluate(node=>{const r=node.getBoundingClientRect();return node.contains(document.elementFromPoint(r.x+r.width/2,r.y+r.height/2));}),{timeout:2000}).toBe(true);
 await page.getByRole('button',{name:'Delete workspace…',exact:true}).click();
 const dialog=page.getByRole('dialog',{name:'Delete workspace',exact:true});await expect(dialog).toBeVisible();await expect(dialog).toContainText('Beta release');await expect(dialog.getByRole('heading')).toHaveText('Delete workspace?');await expect(dialog.getByRole('button',{name:'Cancel',exact:true})).toBeFocused();await page.screenshot({path:'/tmp/lazybox-workspace-delete-dirty.png'});await page.getByRole('button',{name:'Cancel workspace deletion'}).click();await expect(dialog).toHaveCount(0);assert.equal(fixture.mutations.length,0);
 await actions.click();await page.getByRole('button',{name:'Delete workspace…',exact:true}).click();await expect(dialog).toBeVisible();await expect(dialog.getByRole('button',{name:'Delete workspace',exact:true})).toBeDisabled();
 const removeArtifacts=dialog.getByRole('checkbox',{name:'Permanently delete workspace artifacts and all their versions'});
 await expect(removeArtifacts).not.toBeChecked();
 await removeArtifacts.check();
 await expect(dialog).toContainText('Artifacts will be removed');
 await expect(dialog).toContainText('running session');await expect(dialog).toContainText('Uncommitted or untracked files');await expect(dialog.getByRole('textbox')).toHaveCount(0);
 const stopSessions=dialog.getByLabel(/Stop .* active session/),discardFiles=dialog.getByLabel('Discard these workspace files permanently');
 // Check all covers the two consents deleting needs (stop, discard): there while one is unticked, back when one is unticked again.
 const checkAll=dialog.getByRole('button',{name:'Check all',exact:true});
 await expect(checkAll).toBeVisible();
 await stopSessions.check();await discardFiles.check();await expect(checkAll).toHaveCount(0);
 await discardFiles.uncheck();await expect(checkAll).toBeVisible();await discardFiles.check();await expect(checkAll).toHaveCount(0);
 await page.route('**/ScheduleDeletion',route=>{
   assert.equal(!!route.request().postDataJSON().workspace.keepArtifacts,false);
   return route.fulfill({status:400,json:{code:'failed_precondition',message:'Member is not the expected worktree root'}});
 },{times:1});
 const confirmDelete=dialog.getByRole('button',{name:'Delete workspace',exact:true});
 await confirmDelete.click();
 await expect(dialog.getByRole('alert')).toContainText('Member is not the expected worktree root');
 await expect(confirmDelete).toBeEnabled();
 assert.equal(fixture.mutations.filter(m=>m.method==='ScheduleDeletion').length,0);
 // Deleting the artifacts is a choice, not a consent: unticking it does not bring Check all back.
 await removeArtifacts.uncheck();await expect(dialog).toContainText('Keep artifacts and their version history');await expect(checkAll).toHaveCount(0);
 // Check all, by keyboard, ticks the consents only and hands the focus to Delete; Check all itself never deletes.
 await stopSessions.uncheck();await discardFiles.uncheck();await expect(confirmDelete).toBeDisabled();
 await checkAll.focus();await page.keyboard.down('Enter');
 await expect(stopSessions).toBeChecked();await expect(discardFiles).toBeChecked();await expect(removeArtifacts).not.toBeChecked();
 await expect(checkAll).toHaveCount(0);await expect(confirmDelete).toBeFocused();
 // Enter still held: its auto-repeat reaches Delete and must not confirm (confirming shows Deleting… in the same event).
 await page.keyboard.down('Enter');
 assert.equal(await dialog.count(),1,'a repeated Enter does not confirm (the dialog closed)');
 assert.equal(await dialog.locator('footer button.danger').textContent(),'Delete workspace','a repeated Enter does not confirm (it shows Deleting…)');
 await page.keyboard.up('Enter');
 await expect(dialog).toBeVisible();await expect(confirmDelete).toBeFocused();
 assert.equal(fixture.mutations.filter(m=>m.method==='ScheduleDeletion').length,0);
 await page.clock.install();
 await page.keyboard.press('Enter');await expect(dialog).toHaveCount(0);await expect(page.getByRole('navigation',{name:'Workspaces',exact:true})).not.toContainText('Beta release');await expect(page.locator('.workspace-row button[aria-current=page]')).toContainText('Alpha feature');
 assert.equal(fixture.mutations.filter(m=>m.method==='ScheduleDeletion').length,1);
 const scheduled=fixture.mutations.find(m=>m.method==='ScheduleDeletion').body.workspace;
 assert.equal(scheduled.keepArtifacts,true);assert.equal(scheduled.stopSessions,true);assert.equal(scheduled.discardChanges,true);
 const undoNotice=page.getByText('Workspace moved to Recently deleted',{exact:true});
 await expect(undoNotice).toBeVisible();
 await page.clock.fastForward(30_000);
 await expect(undoNotice).toBeVisible();
 await page.clock.fastForward(30_001);
 await expect(undoNotice).toHaveCount(0);
 // Recently deleted survives a reload and Undo restores the actual workspace.
 await page.reload();
 const trash = page.locator('.recently-deleted');
 await expect(trash).toBeVisible();
 assert.ok(await trash.evaluate(node => node === node.parentElement.lastElementChild && node.parentElement === node.closest('.collection-panel').lastElementChild && !node.closest('nav')), 'Recently deleted is a separate section at the end of the sidebar');
 await trash.getByText('Recently deleted (1)').click();
 await expect(trash.getByRole('listitem')).toContainText('Beta release');
 await expect(trash.getByRole('listitem')).toContainText('Workspace · api · web');
 await trash.getByRole('button',{name:'Undo deletion of Beta release'}).click();
 await expect(page.getByRole('navigation',{name:'Workspaces',exact:true})).toContainText('Beta release');
 await expect(trash).toHaveCount(0);
 assert.equal(fixture.mutations.filter(m=>m.method==='UndoDeletion').length,1);
 // One consent (discard) beside the artifacts choice needs no Check all.
 await page.route('**/PreviewWorkspaceDeletion',route=>route.fulfill({json:{supportsKeepArtifacts:true,activeSessionIds:[],dirtyRepositories:['api'],filesOutsideRepositories:false,blocked:''}}));
 await page.getByLabel('Actions for Alpha feature',{exact:true}).click();await page.getByRole('button',{name:'Delete workspace…',exact:true}).click();await expect(dialog).toBeVisible();
 await expect(dialog.getByRole('checkbox')).toHaveCount(2);await expect(dialog.getByRole('button',{name:'Check all',exact:true})).toHaveCount(0);
 await dialog.getByRole('button',{name:'Cancel',exact:true}).click();await expect(dialog).toHaveCount(0);
 // Still blocked once the ownership panel settles: Check all ticks both consents, Delete stays off, and the focus goes to Cancel.
 await page.route('**/PreviewWorkspaceDeletion',route=>route.fulfill({json:{supportsKeepArtifacts:true,activeSessionIds:['alpha-session'],dirtyRepositories:['api'],filesOutsideRepositories:false,blocked:'Previous process ownership is unconfirmed; reconcile the workspace before deleting it.'}}));
 await page.getByLabel('Actions for Alpha feature',{exact:true}).click();await page.getByRole('button',{name:'Delete workspace…',exact:true}).click();await expect(dialog).toBeVisible();
 await expect(dialog.getByRole('alert')).toContainText('reconcile the workspace');
 await dialog.getByRole('button',{name:'Check all',exact:true}).click();
 await expect(dialog.getByLabel(/Stop .* active session/)).toBeChecked();await expect(dialog.getByLabel('Discard these workspace files permanently')).toBeChecked();
 await expect(dialog.getByRole('button',{name:'Delete workspace',exact:true})).toBeDisabled();await expect(dialog.getByRole('button',{name:'Cancel',exact:true})).toBeFocused();
 await page.keyboard.press('Enter');await expect(dialog).toHaveCount(0);
 // A clean, idle workspace needs only the confirmation button, with no typing.
 await page.route('**/PreviewWorkspaceDeletion',route=>route.fulfill({json:{activeSessionIds:[],dirtyRepositories:[],filesOutsideRepositories:false,blocked:''}}));
 await page.getByLabel('Actions for Alpha feature',{exact:true}).click();await page.getByRole('button',{name:'Delete workspace…',exact:true}).click();await expect(dialog).toBeVisible();await expect(dialog.getByRole('textbox')).toHaveCount(0);await expect(dialog.getByRole('button',{name:'Delete workspace',exact:true})).toBeEnabled();await page.screenshot({path:'/tmp/lazybox-workspace-delete-clean.png'});await dialog.getByRole('button',{name:'Cancel',exact:true}).click();await expect(dialog).toHaveCount(0);assert.equal(fixture.mutations.filter(m=>m.method==='ScheduleDeletion').length,1);
// Right-click, keyboard dismissal, outside click, and narrow viewport placement.
const alpha=rows.first();
// A right-click opens the row's actions (the ⋯ menu's list) at the pointer, as a Lazybox menu.
await alpha.click({button:'right'});
const contextMenu=page.locator('.context-menu[data-depth="0"]'),contextDelete=contextMenu.getByRole('menuitem',{name:'Delete workspace…',exact:true});
await expect(contextMenu).toBeVisible();await expect(alpha.locator('.action-popover')).not.toBeVisible();
await page.keyboard.press('End');await expect(contextDelete).toBeFocused();
await page.keyboard.press('Escape');await expect(contextMenu).toHaveCount(0);
await alpha.locator('summary').click();await page.locator('.explorer-heading').first().click();
await expect(alpha.locator('.action-popover')).not.toBeVisible();
for(const viewport of [{width:900,height:420},{width:390,height:640}]){
 await page.setViewportSize(viewport);if(viewport.width<761)await page.getByRole('button',{name:'Open workspace navigator'}).click();const visibleAlpha=viewport.width<761?page.getByRole('dialog',{name:'Workspace navigator'}).locator('.workspace-row').first():alpha;await visibleAlpha.locator('summary').click();
 await expect(deleteAction).toBeVisible();
 const box=await visibleAlpha.locator('.action-popover').boundingBox();
 assert.ok(box.x>=0&&box.y>=0&&box.x+box.width<=viewport.width&&box.y+box.height<=viewport.height,'menu stays inside viewport');
 await expect.poll(()=>deleteAction.evaluate(node=>{const r=node.getBoundingClientRect();return node.contains(document.elementFromPoint(r.x+r.width/2,r.y+r.height/2));})).toBe(true);
 const contextRects=await page.locator('.workspace-context').evaluate(node=>[...node.children].map(child=>{const r=child.getBoundingClientRect();return {x:r.x,y:r.y,right:r.right,bottom:r.bottom,width:r.width,height:r.height};}));
 for(let i=0;i<contextRects.length;i++)for(let j=i+1;j<contextRects.length;j++){
  const a=contextRects[i],b=contextRects[j];if(!a.width||!a.height||!b.width||!b.height)continue;
  assert.ok(a.right<=b.x||b.right<=a.x||a.bottom<=b.y||b.bottom<=a.y,'workspace toolbar items do not overlap');
 }
 await page.screenshot({path:`/tmp/lazybox-workspace-menu-${viewport.width}.png`});
 await page.keyboard.press('Escape');if(viewport.width<761)await page.getByRole('button',{name:'Close workspace navigator'}).click();
}
 await page.setViewportSize({width:1250,height:900});
 await page.getByLabel('Actions for Alpha feature',{exact:true}).click();
 await page.getByRole('button',{name:'Manage repositories…'}).click();
 const repositories=page.getByRole('dialog',{name:'Repositories in Alpha feature'});
 const removeApi=repositories.getByRole('button',{name:'Remove api from Alpha feature'});
 await removeApi.click();
 await expect(repositories.getByRole('button',{name:'Cancel',exact:true})).toBeFocused();
 await repositories.getByRole('button',{name:'Cancel',exact:true}).click();
 assert.equal(fixture.mutations.filter(m=>m.method==='RemoveCollectionRepository').length,0);
 await page.route('**/RemoveCollectionRepository',route=>route.fulfill({status:400,json:{code:'failed_precondition',message:'worktree contains changed files'}}),{times:1});
 await removeApi.click();
 await repositories.getByRole('button',{name:'Remove repository',exact:true}).click();
 await expect(repositories.getByRole('alert')).toContainText('worktree contains changed files');
 await expect(removeApi).toBeVisible();
 await repositories.getByRole('button',{name:'Remove repository',exact:true}).click();
 await expect(removeApi).toHaveCount(0);
 await expect(repositories.getByRole('button',{name:'Remove web from Alpha feature'})).toBeVisible();
 assert.deepEqual(fixture.mutations.filter(m=>m.method==='RemoveCollectionRepository').map(m=>m.body),[{collectionId:'alpha',memberName:'api'}]);
console.log('PASS: reachable top-layer actions in clipped/narrow panels; right-click, keyboard and outside dismissal; no toolbar overlap; focused cancel, dirty/running guards, Check all ticking the required consents (not the artifacts choice) and focusing Delete (Cancel while blocked), a held Enter not confirming, scoped workspace deletion and repository removal. Fixture only.');
}finally{await browser?.close();await server.close();await fixture.close();}
