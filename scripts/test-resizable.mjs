// The dividers inside panels (Resizer.tsx), against the fake Runner: in
// Changes (the file list, the review tray, and the narrow Files / Review
// column), the Artifacts list and its version history, Settings and the
// Documentation, and the agent browser's steps under its page. Each one drags, resets on double-click, moves 16 px a press
// (64 with Shift), goes to its minimum and maximum with Home and End, keeps
// the content beside it readable, and is remembered across a reload and a
// workspace switch. A narrow panel keeps the divider on the edge, a phone has
// none. WebKit, the desktop engine.
import {createServer} from 'vite';
import react from '@vitejs/plugin-react';
import {webkit,expect} from '@playwright/test';
import {fileURLToPath} from 'node:url';
import {mkdir} from 'node:fs/promises';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import {testViteCache} from './fixtures/vite-cache.mjs';
import {seedDesignDoc} from './fixtures/artifact-sample.mjs';
import {monacoVimCompat} from './monaco-vim-compat.mjs';
import {SPLITS} from '../packages/surfaces/src/resizable-model.ts';

const fixture=await startRunnerFixture();
for(const id of ['alpha','beta']){
 const api=fixture.shipState(id,'api');
 api.files=[{path:'src/handler.go',indexStatus:' ',worktreeStatus:'M'},{path:'README.md',indexStatus:' ',worktreeStatus:'M'}];
 fixture.diffs.set(`${id}/api/src/handler.go`,'diff --git a/src/handler.go b/src/handler.go\n--- a/src/handler.go\n+++ b/src/handler.go\n@@ -1,3 +1,3 @@\n package api\n-func Handle() {}\n+func Handle() error { return nil }\n \n');
}
const doc=seedDesignDoc(fixture);
const server=await createServer({...testViteCache(import.meta.url),configFile:false,root:fileURLToPath(new URL('../apps/web',import.meta.url)),logLevel:'warn',plugins:[react(),monacoVimCompat()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
let browser;
const shot=(page,name)=>page.screenshot({path:`.cache/resizable-${name}.png`});
try{
 await mkdir('.cache',{recursive:true});await server.listen();
 browser=await webkit.launch({headless:true});
 const context=await browser.newContext({viewport:{width:1600,height:950}});
 const page=await context.newPage();page.setDefaultTimeout(15000);
 // A reload aborts the Runner's open streams, which WebKit reports: not this page's errors.
 let navigating=false;
 const errors=[];page.on('pageerror',e=>{if(!navigating)errors.push(String(e));});
 const reload=async()=>{navigating=true;await page.reload();await page.waitForLoadState('load');await page.waitForTimeout(500);navigating=false;};
 page.on('console',message=>{if(message.type()==='error'&&/React|Warning|ResizeObserver/.test(message.text()))errors.push(message.text());});
 const base=`http://127.0.0.1:${server.httpServer.address().port}`;
 await page.goto(`${base}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`);
 await expect(page.locator('.panel-chat').getByRole('textbox',{name:'Message',exact:true})).toBeVisible();

 const width=locator=>locator.evaluate(n=>Math.round(n.getBoundingClientRect().width));
 const height=locator=>locator.evaluate(n=>Math.round(n.getBoundingClientRect().height));
 const drag=async(separator,dx,dy=0)=>{
  const b=await separator.boundingBox();const x=b.x+b.width/2,y=b.y+b.height/2;
  await page.mouse.move(x,y);await page.mouse.down();await page.mouse.move(x+dx,y+dy,{steps:6});await page.mouse.up();
 };
 const near=(actual,expected,what,slack=2)=>assert.ok(Math.abs(actual-expected)<=slack,`${what}: ${actual}, expected ${expected}`);
 /** The divider sits between the side and the content beside it, and is thin. */
 const between=async(separator,side,other,sign,axis='x')=>{
  const [s,o,r]=await Promise.all([side.boundingBox(),other.boundingBox(),separator.boundingBox()]);
  const [at,extent,thick]=axis==='x'?['x','width','width']:['y','height','height'];
  const centre=r[at]+r[thick]/2,[from,to]=sign>0?[s[at]+s[extent],o[at]]:[o[at]+o[extent],s[at]];
  assert.ok(centre>=from-1&&centre<=to+1,`divider at ${centre} is between ${from} and ${to}`);
  assert.ok(r[thick]<=12,'a thin divider');
 };
 /**
  * One divider, through everything it does. `sign` is +1 when dragging right
  * (or down) grows the side (a list on the left), -1 when it shrinks it (a
  * tray on the right, a log below).
  */
 const exercise=async(what,{separator,side,other,spec,sign=1})=>{
  const axis=spec.axis||'x',size=axis==='x'?width:height;
  const move=(by)=>axis==='x'?drag(separator,by):drag(separator,0,by);
  const [back,forward]=axis==='x'?['ArrowLeft','ArrowRight']:['ArrowUp','ArrowDown'];
  const grow=sign>0?forward:back,shrink=sign>0?back:forward;
  await expect(separator).toBeVisible();
  await expect(separator).toHaveAttribute('aria-orientation',axis==='x'?'vertical':'horizontal');
  await expect(separator).toHaveAttribute('aria-valuemin',String(spec.min));
  const initial=await size(side);
  await expect(separator).toHaveAttribute('aria-valuenow',String(initial));
  await between(separator,side,other,sign,axis);
  // Drag: the side follows the pointer.
  await move(60*sign);
  await expect.poll(()=>size(side),{message:`${what}: drag grows it`}).toBeGreaterThan(initial+55);
  near(await size(side),initial+60,`${what} after a drag`);
  await between(separator,side,other,sign,axis);
  // Double-click: the default again.
  await separator.dblclick();
  await expect.poll(()=>size(side),{message:`${what}: double-click resets`}).toBe(initial);
  // Keys on the focused divider.
  await separator.focus();
  await page.keyboard.press(grow);
  await expect.poll(()=>size(side),{message:`${what}: arrow`}).toBe(initial+16);
  await expect(separator).toHaveAttribute('aria-valuenow',String(initial+16));
  await page.keyboard.press(`Shift+${shrink}`);
  await expect.poll(()=>size(side),{message:`${what}: Shift+arrow`}).toBe(Math.max(spec.min,initial+16-64));
  await page.keyboard.press('Home');
  await expect.poll(()=>size(side),{message:`${what}: Home`}).toBe(spec.min);
  await page.keyboard.press('End');
  const max=Number(await separator.getAttribute('aria-valuemax'));
  assert.ok(max<=spec.max,`${what}: maximum within its limit`);
  await expect.poll(()=>size(side),{message:`${what}: End`}).toBe(max);
  assert.ok(await size(other)>=spec.keep-2,`${what}: the content beside keeps ${spec.keep}px at the maximum (${await size(other)})`);
  // Dragged past either end: held at its limits.
  await move(800*sign);
  near(await size(side),max,`${what} dragged past its maximum`);
  if(!spec.collapsible){await move(-900*sign);near(await size(side),spec.min,`${what} dragged past its minimum`);}
  await separator.dblclick();
  await expect.poll(()=>size(side)).toBe(initial);
  return initial;
 };

 // 1. Changes (its own page, wide): the file list.
 await page.getByRole('button',{name:'Review changes',exact:true}).click();
 const changes=page.getByRole('region',{name:'Workspace changes'});
 // A located element, not a role: folded, the list is hidden from the accessibility tree.
 const files=changes.locator('.changes-columns>nav');
 await changes.getByRole('navigation',{name:'Changed files'}).getByRole('button',{name:/src\/handler\.go/}).click();
 const diff=changes.locator('.diff-view');
 const fileSeparator=changes.getByRole('separator',{name:'Resize the file list'});
 await exercise('Changes file list',{separator:fileSeparator,side:files,other:diff,spec:SPLITS.changesFiles});
 // Folding: dragged far left it becomes a rail; Enter, a click or a drag brings it back.
 await drag(fileSeparator,40);
 const kept=await width(files);
 await drag(fileSeparator,-kept);
 await expect(fileSeparator).toHaveAttribute('data-collapsed','');
 await expect(fileSeparator).toHaveAttribute('aria-valuenow','0');
 await expect(files).toHaveCSS('visibility','hidden');
 assert.ok(await width(diff)>(await width(changes))-60,'the diff takes the room');
 await shot(page,'changes-folded');
 await fileSeparator.focus();await page.keyboard.press('Enter');
 await expect.poll(()=>width(files)).toBe(kept);
 await expect(files).toHaveCSS('visibility','visible');
 await page.keyboard.press('Enter');await expect(fileSeparator).toHaveAttribute('data-collapsed','');
 await fileSeparator.click();
 await expect.poll(()=>width(files)).toBe(kept);
 await expect(fileSeparator).not.toHaveAttribute('data-collapsed','');

 // The review tray, on the right: dragging left widens it.
 await changes.getByRole('button',{name:/^Review comments/}).click();
 const tray=changes.getByRole('complementary',{name:'Review'});
 await expect(tray).toBeVisible();
 const traySeparator=changes.getByRole('separator',{name:'Resize the review tray'});
 await exercise('Changes review tray',{separator:traySeparator,side:tray,other:diff,spec:SPLITS.changesReview,sign:-1});
 await between(fileSeparator,files,diff,1);
 await shot(page,'changes-wide');

 // Remembered: a reload, and another workspace.
 await drag(fileSeparator,72);
 const chosen=await width(files);
 near(chosen,kept+72,'Changes file list chosen');
 await drag(traySeparator,-40);
 const trayChosen=await width(tray);
 await reload();
 await expect(files).toBeVisible();
 await expect.poll(()=>width(files),{message:'the file list after a reload'}).toBe(chosen);
 if(!await tray.isVisible())await changes.getByRole('button',{name:/^Review comments/}).click();
 await expect.poll(()=>width(tray),{message:'the tray after a reload'}).toBe(trayChosen);
 await page.getByRole('combobox',{name:'Workspace',exact:true}).selectOption('beta');
 if(!await files.isVisible())await page.getByRole('button',{name:'Review changes',exact:true}).click();
 await expect(files).toBeVisible();
 await expect.poll(()=>width(files),{message:'the file list in another workspace'}).toBe(chosen);

 // 2. A narrow Changes: the Files / Review column beside the diff; the divider stays on its edge.
 if(!await tray.isVisible())await changes.getByRole('button',{name:/^Review comments/}).click();
 await page.setViewportSize({width:1100,height:900});
 await page.waitForTimeout(300);
 await expect(changes.getByRole('tablist',{name:'Left column'})).toBeVisible();
 await expect(fileSeparator).toBeVisible();
 await expect(traySeparator).toBeHidden();
 await between(fileSeparator,files,diff,1);
 await fileSeparator.focus();await page.keyboard.press('End');
 assert.ok(await width(diff)>=SPLITS.changesFiles.keep-2,`narrow: the diff keeps its share (${await width(diff)})`);
 await page.keyboard.press('Home');
 await expect.poll(()=>width(files)).toBe(SPLITS.changesFiles.min);
 await between(fileSeparator,files,diff,1);
 await shot(page,'changes-narrow');
 await fileSeparator.dblclick();
 // A phone keeps its stacked layout: no divider.
 await page.setViewportSize({width:420,height:860});
 await page.waitForTimeout(300);
 assert.equal(await page.locator('.resizer:not([data-off])').count(),0,'no divider on a phone');
 await page.setViewportSize({width:1600,height:950});
 await page.waitForTimeout(300);
 // Reset: the panel's own default (a share of its width) again, nothing pinned.
 assert.equal(await changes.locator('.changes-columns').evaluate(n=>n.style.getPropertyValue('--split-changes-files')),'','reset leaves the default');
 await between(fileSeparator,files,diff,1);

 // 3. Artifacts: the list beside the page, and the version history beside the reader.
 await page.getByRole('combobox',{name:'Workspace',exact:true}).selectOption('alpha');
 await page.getByRole('navigation',{name:'Pages'}).getByRole('button',{name:'Work'}).click();
 await page.locator('summary[aria-label="Panels"]').click();
 await page.getByRole('button',{name:'Show Artifacts',exact:true}).click();
 const list=page.locator('.artifacts-list:visible');
 await expect(list).toBeVisible({timeout:15000});
 await list.locator(`[data-artifact-id="${doc.id}"]`).click();
 await page.getByRole('tab',{name:'Artifacts',exact:true}).dblclick();
 const article=page.locator('article.artifact-page:visible');
 await expect(article).toBeVisible();
 const listSeparator=page.getByRole('separator',{name:'Resize the artifact list'});
 await exercise('Artifacts list',{separator:listSeparator,side:list,other:article,spec:SPLITS.artifactsList});
 await article.getByRole('button',{name:/^History/}).click();
 const history=article.getByRole('complementary',{name:'Version history'});
 await expect(history).toBeVisible();
 const historySeparator=page.getByRole('separator',{name:'Resize the version history'});
 await exercise('Artifact history',{separator:historySeparator,side:history,other:article.locator('.artifact-main'),spec:SPLITS.artifactHistory,sign:-1});
 await drag(listSeparator,48);
 const listChosen=await width(list);
 await shot(page,'artifacts');
 await reload();
 // Narrow, the open artifact takes the whole panel and the history floats over it: no divider for either.
 await expect(page.locator('article.artifact-page')).toBeVisible();
 await expect(page.locator('.artifacts-list')).toBeHidden();
 await expect.poll(()=>page.locator('.resizer:not([data-off])').evaluateAll(ns=>ns.map(n=>n.getAttribute('aria-label'))),{message:'no divider in a narrow Artifacts panel'}).toEqual([]);
 await page.getByRole('tab',{name:'Artifacts',exact:true}).dblclick();
 await expect(list).toBeVisible();
 await expect.poll(()=>width(list),{message:'the artifact list after a reload'}).toBe(listChosen);
 await page.getByRole('button',{name:'Restore layout'}).first().click();

 // 4. Settings: the sections beside the page.
 await page.getByRole('button',{name:'Settings',exact:true}).click();
 const settingsNav=page.locator('.settings-sidebar'),settingsContent=page.locator('.settings-content');
 await expect(settingsNav).toBeVisible();
 const settingsSeparator=page.getByRole('separator',{name:'Resize the settings sections'});
 await exercise('Settings sections',{separator:settingsSeparator,side:settingsNav,other:settingsContent,spec:SPLITS.settingsNav});
 await drag(settingsSeparator,40);
 const settingsChosen=await width(settingsNav);
 await shot(page,'settings');
 await reload();
 if(!await settingsNav.isVisible())await page.getByRole('button',{name:'Settings',exact:true}).click();
 await expect.poll(()=>width(settingsNav),{message:'the settings sections after a reload'}).toBe(settingsChosen);
 await settingsSeparator.dblclick();
 await page.getByRole('button',{name:'Settings',exact:true}).click().catch(()=>{});

 // 5. Documentation: the page list beside the page.
 await page.locator('summary[aria-label="Help"]').first().click();
 await page.getByRole('button',{name:'Documentation'}).click();
 const docs=page.getByRole('dialog',{name:'Documentation'});
 await expect(docs.locator('.docs-page h1')).toBeVisible();
 const docsNav=docs.locator('.docs-nav'),docsPage=docs.locator('.docs-page');
 const docsSeparator=docs.getByRole('separator',{name:'Resize the page list'});
 await exercise('Documentation pages',{separator:docsSeparator,side:docsNav,other:docsPage,spec:SPLITS.docsNav});
 await drag(docsSeparator,-30);
 const docsChosen=await width(docsNav);
 await shot(page,'docs');
 await docs.getByRole('button',{name:'Close documentation'}).click();
 await page.locator('summary[aria-label="Help"]').first().click();
 await page.getByRole('button',{name:'Documentation'}).click();
 await expect.poll(()=>width(docsNav),{message:'the docs list reopened'}).toBe(docsChosen);

 await docs.getByRole('button',{name:'Close documentation'}).click();

 // 6. The agent browser: its steps under the page, a divider across.
 await page.getByRole('navigation',{name:'Pages'}).getByRole('button',{name:'Work'}).click();
 fixture.browser.setState('alpha-session',{status:'open',controller:'agent',url:'https://docs.example.com/guide',title:'Guide',canGoBack:false});
 fixture.browser.act('alpha-session',{kind:'open',summary:'Opened docs.example.com',url:'https://docs.example.com/guide'});
 const browserPanel=page.locator('section.workspace-panel.panel-browser');
 await expect(browserPanel).toBeVisible();
 await page.locator('.dv-tab').filter({has:page.locator('.dock-tab-title',{hasText:'docs.example.com'})}).dblclick();
 const steps=browserPanel.locator('.browser-log'),view=browserPanel.locator('.browser-view,.browser-empty').first();
 await expect(steps).toBeVisible();
 const stepsSeparator=browserPanel.getByRole('separator',{name:'Resize the browser steps'});
 await exercise('Browser steps',{separator:stepsSeparator,side:steps,other:view,spec:SPLITS.browserLog,sign:-1});
 // Folded by its own toggle, the steps have no divider; open again, they keep their size.
 await drag(stepsSeparator,0,-50);
 const stepsChosen=await height(steps);
 await browserPanel.locator('.browser-log-toggle').click();
 await expect(stepsSeparator).toHaveCount(0);
 await browserPanel.locator('.browser-log-toggle').click();
 await expect.poll(()=>height(steps)).toBe(stepsChosen);
 await shot(page,'browser');

 assert.deepEqual(errors,[]);
 console.log('PASS resizable: Changes (file list with fold to a rail, review tray, narrow column, none on a phone), Artifacts list and history, Settings, Documentation and the agent browser steps: drag, double-click reset, arrows/Shift/Home/End, limits that keep the content readable, remembered across reload and workspace switch. WebKit, fixture Runner.');
}finally{await browser?.close();await server.close();await fixture.close();}
