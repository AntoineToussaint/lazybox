import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from './fixtures/vite-cache.mjs';import react from '@vitejs/plugin-react';import {webkit,expect} from './fixtures/playwright.mjs';import {fileURLToPath} from 'node:url';import assert from 'node:assert/strict';import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import { collectPageErrors, drainPageErrors } from './fixtures/page-errors.mjs';
const f=await startRunnerFixture();Object.assign(f.library[0],{name:'handbook',gitIdentity:'github:acme/handbook'});Object.assign(f.library[1],{name:'module-datasource',gitIdentity:'github:codefly-dev/module-datasource'});Object.assign(f.sessions[0],{closed:true,status:'stopped',title:'what is this repo?',provider:'claude',model:'claude-fable-5-1',modelReported:true});
const server=await createServer({...testViteCache(import.meta.url),configFile:false,root:process.env.TEST_SOURCE?process.env.TEST_SOURCE+'/apps/web':fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:f.url}}}});let browser;
try{await server.listen();browser=await webkit.launch();const page=await browser.newPage({viewport:{width:1440,height:900}});const errors=await collectPageErrors(page);await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${f.token}`);
const workspace=page.locator('.workspace-row').first();await expect(workspace).toContainText('acme/handbook');await expect(workspace).toContainText('codefly-dev/module-datasource');
const row=workspace;await expect(row.locator('.workspace-agent-label')).toHaveText('what is this repo?');await expect(row.locator('.workspace-agent')).toHaveAttribute('title',/claude-fable-5-1/);await expect(row).not.toContainText('Local CLI sign-in');await expect(row).not.toContainText('Session ended');assert.ok((await row.boundingBox()).height<100);
const history=page.getByRole('button',{name:'Conversation history',exact:true});
const rowBounds=await row.boundingBox(),historyBounds=await history.boundingBox();
assert.ok(Math.abs(rowBounds.x-historyBounds.x)<1&&Math.abs(rowBounds.width-historyBounds.width)<1,'history and workspace hit areas share a full-width grid');
const selection=await row.evaluate(node=>getComputedStyle(node).backgroundColor);
await row.locator('.workspace-link').hover();
assert.equal(await row.evaluate(node=>getComputedStyle(node).backgroundColor),selection,'hover does not replace the selected row state');
assert.equal(await row.locator('.workspace-link').evaluate(node=>getComputedStyle(node).backgroundColor),'rgba(0, 0, 0, 0)','row owns the background, not nested buttons');
await history.hover();await page.screenshot({path:'/tmp/lazybox-sidebar-history-hover.png'});
await row.locator('summary').click();const r=await row.boundingBox(),a=await row.locator('summary').boundingBox();assert.ok(a.x+a.width<=r.x+r.width&&a.y>=r.y,'action must be inside row');const menu=row.locator('details>div');const menuBounds=await menu.boundingBox();assert.ok(menuBounds.y>=0&&menuBounds.y+menuBounds.height<=page.viewportSize().height,'all workspace actions fit in the viewport');await page.screenshot({path:'/tmp/lazybox-sidebar-menu.png'});
await row.getByRole('button',{name:'Delete conversation'}).click();
await expect(page.locator('.workspace-agent-label').filter({hasText:'what is this repo?'})).toHaveCount(0);
await expect(page.getByRole('dialog',{name:'Delete conversation'})).toHaveCount(0);
await page.locator('.toast').getByRole('button',{name:'Undo',exact:true}).click();
await expect(row.locator('.workspace-agent-label')).toHaveText('what is this repo?');
// A timed-out history refresh must retain saved entries and the next successful retry clears the warning.
let historyReads = 0;
let transientFailure = true;
let historyFails = false;
await page.route('**/ListHistory', route => {
  const request = route.request().postDataJSON();
  if (request.limit !== 50 || request.collectionId) return route.continue();
  historyReads++;
  if (transientFailure) { transientFailure = false; return route.fulfill({ status: 503, json: { code: 'unavailable', message: 'fixture briefly unavailable' } }); }
  if (!historyFails) return route.fulfill({ json: { sessions: [...f.sessions, { id: 'saved-only', collectionId: 'alpha', title: 'Saved history entry', historical: true, closed: true }] } });
  return route.fulfill({ status: 503, contentType: 'application/json', body: JSON.stringify({ code: 'unavailable', message: 'fixture history unavailable' }) });
});
await expect.poll(() => historyReads, { timeout: 20000 }).toBeGreaterThan(1);
await expect(page.getByText('Could not refresh conversation history.', { exact: true })).toHaveCount(0);
historyFails = true;
const warning = page.getByRole('status').filter({ hasText: 'Could not refresh conversation history' });
await expect(warning).toBeVisible({ timeout: 20000 });
await history.click();
const savedEntry = page.getByText('Saved history entry', { exact: true });
await expect(savedEntry.first()).toBeVisible();
await page.keyboard.press('Escape');
historyFails = false;
await warning.getByRole('button', { name: 'Retry history refresh' }).click();
await expect(warning).toHaveCount(0);
await page.setViewportSize({width:900,height:700});assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth),false);await page.screenshot({path:'/tmp/lazybox-sidebar-compact.png'});Object.assign(f.github,{state:'connected',authMethod:'github_app',login:'fixture-user',clientId:'fixture-client'});await page.getByRole('button',{name:'Settings',exact:true}).click();await page.getByRole('tab',{name:'Repositories',exact:true}).click();await page.getByRole('button',{name:'Connections & folders',exact:true}).first().click();await expect(page.getByText('GitHub App access: only repositories allowed by its installations.',{exact:false})).toBeVisible();await page.getByRole('button',{name:'Use personal token…'}).click();await expect(page.getByRole('button',{name:'Disconnect this Runner',exact:true})).toBeVisible();await page.getByRole('button',{name:'Keep connected',exact:true}).click();assert.deepEqual(f.mutations.map(call=>call.method),['ScheduleDeletion','UndoDeletion']);await drainPageErrors(errors);assert.deepEqual(errors,[],'no page errors');console.log('PASS sidebar repository identity, compact row/menu geometry, conversation deletion and Undo, narrow layout.');
}finally{await browser?.close();await server.close();await f.close();}
