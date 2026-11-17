// One dismissal rule for dialogs, popovers and pickers, against the fake
// Runner in WebKit (the desktop app's engine; TEST_CHROMIUM=1 for Chromium):
// Escape and a click outside close them; a press that starts inside (a text
// selection dragged out) never does; a dialog holding typed edits stays open on
// a click outside and nudges its footer, keeping what was typed; focus goes
// back where it was. Covers the prompt history ("Your prompts"), the
// conversation history dialog, the command palette, the shortcuts overlay,
// the slash-command menu and Track as artifact.
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from 'vite';
import react from '@vitejs/plugin-react';
import {webkit,chromium,expect} from '@playwright/test';
import {fileURLToPath} from 'node:url';
import {mkdir} from 'node:fs/promises';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';

const fixture=await startRunnerFixture();
const alpha=fixture.openEvents('alpha-session');
alpha.push({type:'operation_accepted',operationId:'op-1',text:'Plan the release'});
alpha.push({type:'message_delta',text:'# Release plan\n\nShip the parser first, then the routes.'});alpha.push({type:'turn_ended'});
alpha.push({type:'operation_accepted',operationId:'op-2',text:'And the changelog?'});
alpha.push({type:'message_delta',text:'The changelog lists both, newest first.'});alpha.push({type:'turn_ended'});
const server=await createServer({...testViteCache(import.meta.url),configFile:false,logLevel:'error',root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
let browser;
try{
 await server.listen();await mkdir('.cache',{recursive:true});
 browser=await(process.env.TEST_CHROMIUM?chromium:webkit).launch({headless:true});
 const context=await browser.newContext({viewport:{width:1400,height:900}});
 // A Mac, as far as the keymap is concerned (⌘ is Mod).
 await context.addInitScript(()=>{Object.defineProperty(navigator,'platform',{get:()=>'MacIntel'});});
 const page=await context.newPage();page.setDefaultTimeout(15000);
 const errors=[];page.on('pageerror',e=>errors.push(String(e)));
 await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`);
 const chat=page.locator('.panel-chat');
 const composer=chat.getByRole('textbox',{name:'Message',exact:true});
 await expect(composer).toBeEnabled();
 await expect(chat.locator('article.message.message_delta').filter({hasText:'newest first'})).toBeVisible();

 // A spot on the modal backdrop, clear of every dialog.
 const backdrop={x:6,y:450};
 const clickBackdrop=()=>page.mouse.click(backdrop.x,backdrop.y);
 // A text selection dragged from inside an element to the backdrop.
 const dragOut=async locator=>{
  const box=await locator.boundingBox();
  await page.mouse.move(box.x+4,box.y+box.height/2);await page.mouse.down();
  await page.mouse.move(box.x+box.width/2,box.y+box.height/2,{steps:4});
  await page.mouse.move(backdrop.x,backdrop.y,{steps:6});await page.mouse.up();
 };
 // A transcript spot outside the popovers: text, nothing focusable.
 const transcriptText=chat.locator('article.message.message_delta').filter({hasText:'newest first'}).locator('p').first();
 const clickTranscript=async()=>{const box=await transcriptText.boundingBox();await page.mouse.click(box.x+3,box.y+box.height/2);};

 // ── The prompt history ("Your prompts") — what the report was about.
 const chip=page.getByRole('button',{name:'Your prompts',exact:true});
 const prompts=page.getByRole('dialog',{name:'Jump to a prompt'});
 await composer.click();await expect(composer).toBeFocused();
 await chip.click();
 await expect(prompts).toBeVisible();
 await expect(prompts.getByRole('searchbox',{name:'Find a prompt'})).toBeFocused();
 await page.screenshot({path:'.cache/dismiss-prompts-open.png'});
 // Escape closes it, and the focus is back in the message box.
 await page.keyboard.press('Escape');
 await expect(prompts).toHaveCount(0);
 await expect(composer).toBeFocused();
 // A click outside closes it. On the transcript the focus goes to the transcript, as that click always does;
 // on something that takes no focus, back to the message box rather than nowhere.
 await chip.click();await expect(prompts).toBeVisible();
 await clickTranscript();
 await expect(prompts).toHaveCount(0);
 await expect(chat.locator('.messages:visible')).toBeFocused();
 await composer.click();
 await chip.click();await expect(prompts).toBeVisible();
 await page.locator('.workspace-brand').click();
 await expect(prompts).toHaveCount(0);
 await expect(composer).toBeFocused();
 // Its own chip toggles it shut (the press that closes is not a second open).
 await chip.click();await expect(prompts).toBeVisible();
 await chip.click();await expect(prompts).toHaveCount(0);
 // A selection dragged out of its search field leaves it open.
 await chip.click();await expect(prompts).toBeVisible();
 await prompts.getByRole('searchbox',{name:'Find a prompt'}).fill('changelog');
 await dragOut(prompts.getByRole('searchbox',{name:'Find a prompt'}));
 await expect(prompts).toBeVisible();
 // A click outside that lands on a button closes the list, and the button still works.
 await page.getByRole('button',{name:'Conversation history',exact:true}).click();
 await expect(prompts).toHaveCount(0);
 await expect(page.getByRole('dialog',{name:'Conversation history'})).toBeVisible();
 await page.keyboard.press('Escape');await expect(page.getByRole('dialog',{name:'Conversation history'})).toHaveCount(0);

 // A context menu over the list: Escape closes the menu, not the list under it; a second Escape the list.
 await composer.click();await chip.click();await expect(prompts).toBeVisible();
 const contextMenu=page.locator('.context-menu-layer .context-menu');
 await prompts.getByRole('option').first().click({button:'right'});
 await expect(contextMenu).toBeVisible();
 await page.keyboard.press('Escape');
 await expect(contextMenu).toHaveCount(0);
 await expect(prompts).toBeVisible();
 // A press outside the menu closes the menu alone.
 await prompts.getByRole('option').first().click({button:'right'});await expect(contextMenu).toBeVisible();
 await page.locator('.workspace-brand').click();
 await expect(contextMenu).toHaveCount(0);
 await expect(prompts).toBeVisible();
 await page.keyboard.press('Escape');await expect(prompts).toHaveCount(0);

 // ── The slash-command menu: a click outside the menu and the box puts it away; the text stays.
 await composer.click();await composer.fill('/');
 const slash=chat.getByRole('listbox',{name:'Commands'});
 await expect(slash).toBeVisible();
 // A press on the message box itself keeps it.
 await composer.click();await expect(slash).toBeVisible();
 // The menu covers the end of the transcript: the first prompt is clear of it.
 {const box=await chat.locator('article.message.operation_accepted p').first().boundingBox();await page.mouse.click(box.x+3,box.y+box.height/2);}
 await expect(slash).toHaveCount(0);
 await expect(composer).toHaveValue('/');
 await composer.fill('');await composer.fill('/');await expect(slash).toBeVisible();
 await composer.press('Escape');await expect(slash).toHaveCount(0);
 await composer.fill('');

 // ── The conversation history dialog (DraggableDialog): backdrop and Escape close it; a drag out does not.
 const historyButton=page.getByRole('button',{name:'Conversation history',exact:true});
 const history=page.getByRole('dialog',{name:'Conversation history'});
 await historyButton.click();await expect(history).toBeVisible();
 await page.screenshot({path:'.cache/dismiss-history-open.png'});
 await dragOut(history.locator('p').first());
 await expect(history).toBeVisible();
 // Typing a search is a query, not work: a click outside still closes it.
 await history.getByRole('searchbox').fill('alpha');
 await clickBackdrop();
 await expect(history).toHaveCount(0);
 await historyButton.click();await expect(history).toBeVisible();
 await page.keyboard.press('Escape');
 await expect(history).toHaveCount(0);
 // A press on the backdrop released inside the dialog is not a click outside.
 await historyButton.click();await expect(history).toBeVisible();
 {const box=await history.locator('p').first().boundingBox();
  await page.mouse.move(backdrop.x,backdrop.y);await page.mouse.down();await page.mouse.move(box.x+10,box.y+box.height/2,{steps:6});await page.mouse.up();}
 await expect(history).toBeVisible();
 await page.keyboard.press('Escape');await expect(history).toHaveCount(0);

 // A context menu over a dialog: a click on the backdrop closes the menu, not the dialog; so does Escape.
 await historyButton.click();await expect(history).toBeVisible();
 const menuOver=page.locator('.context-menu-layer .context-menu');
 await history.locator('p').first().click({button:'right'});
 await expect(menuOver).toBeVisible();
 await clickBackdrop();
 await expect(menuOver).toHaveCount(0);
 await expect(history).toBeVisible();
 await history.locator('p').first().click({button:'right'});await expect(menuOver).toBeVisible();
 await page.keyboard.press('Escape');
 await expect(menuOver).toHaveCount(0);
 await expect(history).toBeVisible();
 await page.keyboard.press('Escape');await expect(history).toHaveCount(0);

 // ── The command palette (its own <dialog>): backdrop closes it, focus returns; a drag out does not.
 const palette=page.getByRole('dialog',{name:'Command palette'});
 const search=palette.getByRole('combobox',{name:'Search commands'});
 await composer.click();await expect(composer).toBeFocused();
 await page.keyboard.press('Meta+k');await expect(palette).toBeVisible();
 await search.fill('settings and more');
 await dragOut(search);
 await expect(palette).toBeVisible();
 await clickBackdrop();
 await expect(palette).toHaveCount(0);
 await expect(composer).toBeFocused();
 await page.keyboard.press('Meta+k');await expect(palette).toBeVisible();
 await page.keyboard.press('Escape');await expect(palette).toHaveCount(0);

 // ── The shortcuts overlay: backdrop and Escape.
 const overlay=page.getByRole('dialog',{name:'Keyboard shortcuts'});
 await page.locator('.workspace-brand').click();
 await page.keyboard.press('?');await expect(overlay).toBeVisible();
 await clickBackdrop();await expect(overlay).toHaveCount(0);
 await page.locator('.workspace-brand').click();
 await page.keyboard.press('?');await expect(overlay).toBeVisible();
 await page.keyboard.press('Escape');await expect(overlay).toHaveCount(0);

 // ── Track as artifact: untouched, a click outside closes it; with a typed title it stays and nudges.
 const agentMessage=chat.locator('article.message.message_delta').filter({hasText:'Release plan'});
 const menu=page.getByRole('menu',{name:'Message actions'});
 const track=page.getByRole('dialog',{name:'Track as artifact'});
 const openTrack=async()=>{await agentMessage.locator('p').first().click({button:'right',position:{x:4,y:6}});await menu.getByRole('menuitem',{name:'Track as artifact…'}).click();await expect(track).toBeVisible();};
 await openTrack();
 await expect(track.getByLabel('Title')).toHaveValue('Release plan');
 // A selection dragged out of the preview keeps it open.
 await dragOut(track.getByRole('region',{name:'Preview'}).locator('p').first());
 await expect(track).toBeVisible();
 await clickBackdrop();
 await expect(track).toHaveCount(0);
 await openTrack();
 await track.getByLabel('Title').fill('Release plan for 2.0');
 await clickBackdrop();
 // The edits are kept: the dialog stays, its footer shakes, the title is what was typed.
 await expect(track).toBeVisible();
 await expect(track.locator('footer.dismiss-nudge')).toHaveCount(1);
 await page.screenshot({path:'.cache/dismiss-track-nudge.png'});
 await expect(track.getByLabel('Title')).toHaveValue('Release plan for 2.0');
 await expect(track.locator('footer.dismiss-nudge')).toHaveCount(0,{timeout:2000});
 assert.equal(fixture.mutations.filter(m=>m.method==='PutArtifact').length,0,'nothing was tracked');
 // Escape (or Cancel) is deliberate, and closes it.
 await page.keyboard.press('Escape');
 await expect(track).toHaveCount(0);

 assert.deepEqual(errors,[]);
 console.log('PASS: Escape and a click outside close the prompt history, the slash menu, the conversation history, the command palette, the shortcuts overlay and Track as artifact; a selection dragged out never does; typed edits keep their dialog open and nudge it; focus returns to the message box; a context menu over a dialog or a popover closes alone, on a press outside it or Escape');
}finally{await browser?.close();await server.close();await fixture.close();}
