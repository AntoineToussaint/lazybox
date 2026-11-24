// The Workspaces sidebar has one fixed width: switching workspace or resizing
// the window never moves it, and dragging its edge changes it everywhere.
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from 'vite';
import react from '@vitejs/plugin-react';
import {webkit,expect} from './fixtures/playwright.mjs';
import {fileURLToPath} from 'node:url';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import {monacoVimCompat} from './monaco-vim-compat.mjs';
import { collectPageErrors } from './fixtures/page-errors.mjs';

const fixture=await startRunnerFixture();
const server=await createServer({...testViteCache(import.meta.url),configFile:false,root:fileURLToPath(new URL('../apps/web',import.meta.url)),logLevel:'warn',plugins:[react(),monacoVimCompat()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
await server.listen();
let browser;
try{
 browser=await webkit.launch({headless:true});
 const page=await browser.newPage({viewport:{width:1400,height:850}});page.setDefaultTimeout(15000);
 const errors=[];await collectPageErrors(page, errors);
 const [first,second]=fixture.sessions;
 await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=${first.collectionId}#token=${fixture.token}`);
 const nav=page.getByRole('navigation',{name:'Workspaces'});
 await expect(nav).toBeVisible();
 // The sidebar is the dock group holding Workspaces (and Agents) at the page's edge.
 const sidebar=page.locator('.dv-groupview').filter({has:page.locator('[data-kind="collections"]')});
 const width=()=>sidebar.evaluate(n=>Math.round(n.getBoundingClientRect().width));
 const initial=await width();
 const cells=()=>page.locator('.dv-groupview').evaluateAll(nodes=>nodes.map(n=>{const b=n.getBoundingClientRect();return `${Math.round(b.x)},${Math.round(b.y)} ${Math.round(b.width)}x${Math.round(b.height)}`;}).sort().join(' '));
 // Open the output and move the chat/output divider first: the other workspace must take the same sizes.
 await page.getByRole('tab',{name:'Command output',exact:true}).click();
 const inner=page.getByRole('separator',{name:/Resize Agent chat and Command output/});
 await expect(inner).toBeVisible();
 {const b=await inner.boundingBox();await page.mouse.move(b.x+b.width/2,b.y+b.height/2);await page.mouse.down();await page.mouse.move(b.x+b.width/2,b.y+b.height/2-60,{steps:4});await page.mouse.up();}
 await page.waitForTimeout(400);
 const arranged=await cells();
 assert.equal(initial,280,'default sidebar width');
 // Switch workspace: same width, same panels in the same places.
 await page.locator('.workspace-row').filter({hasText:'Beta release'}).locator('button').first().click();
 await page.waitForTimeout(600);
 assert.equal(await width(),initial,'switching workspace kept the width');
 assert.equal(await cells(),arranged,'switching workspace keeps every panel where it was, at the same size');
 // Resize the window: same width.
 await page.setViewportSize({width:1100,height:850});await page.waitForTimeout(250);
 assert.equal(await width(),initial,'resizing the window kept the width');
 await page.setViewportSize({width:1400,height:850});await page.waitForTimeout(250);
 // Drag the edge: the new width holds in the other workspace and after reload.
 const handle=page.getByRole('separator',{name:'Resize Workspaces sidebar'});
 const box=await handle.boundingBox();
 await page.mouse.move(box.x+box.width/2,box.y+box.height/2);await page.mouse.down();
 await page.mouse.move(box.x+box.width/2+80,box.y+box.height/2,{steps:5});await page.mouse.up();
 await page.waitForTimeout(600);
 const dragged=await width();
 assert.ok(Math.abs(dragged-(initial+80))<=4,`dragged to ${dragged}`);
 await page.goto(page.url());
 await expect(nav).toBeVisible();
 await page.waitForTimeout(300);
 assert.equal(await width(),dragged,'the dragged width applies to every workspace');
 // Keyboard on the edge, which says its width.
 await expect(handle).toHaveAttribute('aria-valuenow',String(dragged));
 await handle.focus();await page.keyboard.press('ArrowLeft');
 await expect.poll(width).toBe(dragged-16);
 await expect(handle).toHaveAttribute('aria-valuenow',String(dragged-16));
 // A panel dropped beside the sidebar does not squeeze it.
 const chat=await page.locator('[data-dock-panel]:has(> .panel-host > .panel-chat)').boundingBox(),tab=await page.getByRole('tab',{name:'Command output',exact:true}).boundingBox();
 await page.mouse.move(tab.x+tab.width/2,tab.y+tab.height/2);await page.mouse.down();await page.mouse.move(tab.x+30,tab.y+30,{steps:4});
 await page.mouse.move(chat.x+24,chat.y+chat.height/2,{steps:10});await page.waitForTimeout(150);await page.mouse.up();
 await page.waitForTimeout(500);
 assert.equal(await width(),dragged-16,'a drop beside the sidebar keeps its width');
 assert.deepEqual(errors,[]);
 console.log(`PASS: sidebar fixed at ${initial}px across workspaces and window sizes; drag (${dragged}px) and arrows apply everywhere; a drop beside it keeps it.`);
}finally{await browser?.close();await server.close();await fixture.close();}
