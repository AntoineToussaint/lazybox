import {createServer} from 'vite';
import react from '@vitejs/plugin-react';
import {webkit, chromium, expect} from '@playwright/test';
import {fileURLToPath} from 'node:url';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
const fixture=await startRunnerFixture();
const server=await createServer({configFile:false,root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
let browser;
try {
 await server.listen();browser=await (process.env.TEST_CHROMIUM?chromium:webkit).launch();
 const page=await browser.newPage({viewport:{width:1440,height:900}});const errors=[];page.on('pageerror',e=>errors.push(String(e)));
 await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`);
 const chat=page.locator('.panel-chat');const composer=chat.getByRole('textbox',{name:'Message',exact:true});
 await expect(composer).toBeVisible();await composer.fill('Keep this draft through navigation and resizing');
 await chat.evaluate(node=>node.dataset.qaIdentity='retained');
 const streams=fixture.calls.filter(c=>c.method==='WatchEvents').length;
 await page.evaluate(()=>new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve))));
 await page.screenshot({path:'/tmp/lazybox-polish-desktop.png',animations:'disabled'});
 for(const width of [1024,768,390]) {
  await page.setViewportSize({width,height:844});
  await expect(composer).toHaveValue('Keep this draft through navigation and resizing');
  assert.equal(await chat.getAttribute('data-qa-identity'),'retained','resize preserves mounted chat');
  assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth),false,'no horizontal page overflow');
  const composerBox=await composer.boundingBox();assert.ok(composerBox.y>=0&&composerBox.y+composerBox.height<=844,'composer remains on screen');
  if(width===390) {
   await expect(page.locator('.page-layout .panel-collections')).not.toBeVisible();
   const chatBox=await chat.boundingBox();assert.ok(chatBox.y<160&&chatBox.height>600,'phone gives the workspace to the active panel');
   await page.getByRole('button',{name:'Open workspace navigator'}).click();
   const drawer=page.getByRole('dialog',{name:'Workspace navigator'});await expect(drawer).toBeVisible();
   await expect(drawer.getByRole('button',{name:'Close workspace navigator'})).toBeFocused();
   await drawer.getByLabel('Actions for Alpha feature',{exact:true}).click();
   await expect(drawer.getByRole('button',{name:'Delete workspace…',exact:true})).toBeVisible();
   await page.screenshot({path:'/tmp/lazybox-polish-phone-navigation.png'});
   await page.keyboard.press('Escape');await expect(drawer).toBeVisible();await page.keyboard.press('Escape');await expect(drawer).toHaveCount(0);
   await page.getByRole('navigation',{name:'Workspace panels'}).getByRole('button',{name:'Command output',exact:true}).click();
   await expect(chat).not.toBeVisible();
   await page.getByRole('navigation',{name:'Workspace panels'}).getByRole('button',{name:'Agent chat',exact:true}).click();await expect(composer).toHaveValue('Keep this draft through navigation and resizing');
   await page.getByRole('button',{name:'New conversation',exact:true}).click();
   const setup=page.getByRole('dialog',{name:'New conversation',exact:true});await expect(setup).toBeVisible();
   await expect(setup.getByRole('button',{name:'Start with last settings'})).toBeVisible();await setup.getByRole('button',{name:'Close new conversation'}).click();
   await expect(composer).toHaveValue('Keep this draft through navigation and resizing');
   const send=await chat.locator('.send-button').boundingBox();assert.ok(send.x>300,'send stays at the trailing edge');await page.screenshot({path:'/tmp/lazybox-polish-phone.png'});
   await page.getByRole('button',{name:'Open workspace navigator'}).click();
   await drawer.getByRole('button',{name:'New conversation in Alpha feature'}).click();
   await expect(drawer).toHaveCount(0);await expect(setup).toBeVisible();
   await page.screenshot({path:'/tmp/lazybox-polish-phone-new-conversation.png'});
   await setup.getByRole('button',{name:'Close new conversation'}).click();
   await page.getByRole('button',{name:'Open workspace navigator'}).click();await page.setViewportSize({width:1024,height:844});await expect(drawer).toHaveCount(0);
  } else await page.screenshot({path:`/tmp/lazybox-polish-tablet-${width}.png`});
 }
 assert.equal(fixture.calls.filter(c=>c.method==='WatchEvents').length,streams,'navigation and panels must not reconnect the chat');
 assert.equal(fixture.mutations.length,0,'presentation does not create or stop agent execution');assert.deepEqual(errors,[]);
 console.log('PASS responsive shell: desktop/tablet/phone, visible composer, modal navigation, nested menus, panel switching, retained DOM/draft/event reader, no execution mutations.');
} finally {await browser?.close();await server.close();await fixture.close();}
