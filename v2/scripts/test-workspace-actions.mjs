import {createServer} from 'vite';
import react from '@vitejs/plugin-react';
import {chromium,webkit,expect} from '@playwright/test';
import {fileURLToPath} from 'node:url';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
const fixture=await startRunnerFixture();
const regression=process.env.REPRO_OLD_BLUR?{name:'reproduce-old-blur',enforce:'pre',transform(code,id){if(id.endsWith('/CollectionPanel.tsx'))return code.replace('<details className="workspace-actions"','<details className="workspace-actions" onBlur={event=>{if(!event.currentTarget.contains(event.relatedTarget as Node))event.currentTarget.open=false;}}');}}:null;
const server=await createServer({configFile:false,root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[regression,react()].filter(Boolean),server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
let browser;
try{
 await server.listen();browser=await(process.env.TEST_WEBKIT?webkit:chromium).launch({headless:true});const page=await browser.newPage({viewport:{width:1250,height:900}});page.setDefaultTimeout(8000);
 await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`);
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
 await expect(dialog).toContainText('running session');await expect(dialog).toContainText('Uncommitted or untracked files');await expect(dialog.getByRole('textbox')).toHaveCount(0);await dialog.getByLabel(/Stop .* active session/).check();await dialog.getByLabel('Discard these workspace files permanently').check();
 await dialog.getByRole('button',{name:'Delete workspace',exact:true}).click();await expect(dialog).toHaveCount(0);await expect(page.getByRole('navigation',{name:'Workspaces',exact:true})).not.toContainText('Beta release');await expect(page.locator('.workspace-row button[aria-current=page]')).toContainText('Alpha feature');
 assert.equal(fixture.mutations.filter(m=>m.method==='DeleteWorkspace').length,1);
 // A clean, idle workspace needs only the confirmation button, with no typing.
 await page.route('**/PreviewWorkspaceDeletion',route=>route.fulfill({json:{activeSessionIds:[],dirtyRepositories:[],filesOutsideRepositories:false,blocked:''}}));
 await page.getByLabel('Actions for Alpha feature',{exact:true}).click();await page.getByRole('button',{name:'Delete workspace…',exact:true}).click();await expect(dialog).toBeVisible();await expect(dialog.getByRole('textbox')).toHaveCount(0);await expect(dialog.getByRole('button',{name:'Delete workspace',exact:true})).toBeEnabled();await page.screenshot({path:'/tmp/lazybox-workspace-delete-clean.png'});await dialog.getByRole('button',{name:'Cancel',exact:true}).click();await expect(dialog).toHaveCount(0);assert.equal(fixture.mutations.filter(m=>m.method==='DeleteWorkspace').length,1);
// Right-click, keyboard dismissal, outside click, and narrow viewport placement.
const alpha=rows.first();
await alpha.click({button:'right'});
await expect(alpha.locator('.action-popover')).toBeVisible();
await page.keyboard.press('End');await expect(deleteAction).toBeFocused();
await page.keyboard.press('Escape');await expect(alpha.locator('summary')).toBeFocused();
await expect(alpha.locator('.action-popover')).not.toBeVisible();
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
console.log('PASS: reachable top-layer actions in clipped/narrow panels; right-click, keyboard and outside dismissal; no toolbar overlap; focused cancel, dirty/running guards and scoped deletion. Fixture only.');
}finally{await browser?.close();await server.close();await fixture.close();}
