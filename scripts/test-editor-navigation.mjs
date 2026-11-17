import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from 'vite';
import react from '@vitejs/plugin-react';
import {webkit,expect} from '@playwright/test';
import {fileURLToPath} from 'node:url';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import {monacoVimCompat} from './monaco-vim-compat.mjs';
const fixture=await startRunnerFixture();fixture.sessions[0].model='model-fixture';fixture.sessions[0].modelReported=true;fixture.sessions[0].provider='claude';fixture.sessions[0].profileId='claude-work';fixture.profiles.push({id:'claude-work',provider:'claude',label:'Work account',isolation:'environment'});
const channel=fixture.openEvents('alpha-session');channel.push({type:'message_delta',text:'Open [README](api/README.md).'});
const server=await createServer({...testViteCache(import.meta.url),configFile:false,root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react(),monacoVimCompat()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
let browser;
try{
 await server.listen();browser=await webkit.launch({headless:true});const page=await browser.newPage({viewport:{width:1440,height:900}});page.setDefaultTimeout(20000);
 const errors=[];page.on('pageerror',error=>errors.push(String(error)));
 await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`);
 // The row names the task; provider, model and account are its tooltip.
 await expect(page.locator('.workspace-agent-label').first()).not.toHaveText(/^Claude/);await expect(page.locator('.workspace-agent').first()).toHaveAttribute('title',/Claude · model-fixture · Work account/);await expect(page.locator('.workspace-agent-status').first()).toHaveText(/^Running/);
 const message=page.getByRole('textbox',{name:'Message',exact:true});await message.fill('Retain this draft');
 const watches=fixture.calls.filter(call=>call.method==='WatchEvents'&&call.body.sessionId==='alpha-session').length;
 await page.getByLabel('Actions for Alpha feature',{exact:true}).click();await page.getByRole('button',{name:'Rename workspace',exact:true}).click();await page.getByLabel('Workspace name',{exact:true}).fill('Renamed');
 const save=page.getByRole('button',{name:'Save name',exact:true});assert.ok((await save.boundingBox()).height<40,'Save must not wrap');await save.click();await expect(page.locator('.workspace-link strong').filter({hasText:'Renamed'})).toBeVisible();
 await expect(message).toHaveValue('Retain this draft');assert.equal(fixture.calls.filter(call=>call.method==='WatchEvents'&&call.body.sessionId==='alpha-session').length,watches,'rename must not restart the reader');
 await page.locator('.workspace-row').filter({hasText:'Beta release'}).locator('button').first().click();await page.locator('.workspace-row').filter({hasText:'Renamed'}).locator('button').first().click();
 await expect(page.locator('.messages:visible')).toContainText('Open');await expect(message).toHaveValue('Retain this draft');
 await expect.poll(()=>fixture.calls.filter(call=>call.method==='WatchEvents'&&call.body.sessionId==='alpha-session').at(-1)?.body.after).toBe('1');
 await page.screenshot({path:'/tmp/lazybox-navigation-verified.png'});await page.getByRole('button',{name:'README',exact:true}).click({modifiers:['Alt']});const dialog=page.getByRole('dialog',{name:'File preview',exact:true});
 await expect(dialog).toContainText('A retained file.');assert.ok((await dialog.boundingBox()).height>700);
 await dialog.getByRole('button',{name:'Edit file',exact:true}).click();const editor=dialog.getByRole('textbox',{name:'File source editor',exact:true});await expect(editor).toBeVisible();await editor.focus();await page.keyboard.press('ControlOrMeta+a');await page.keyboard.insertText('edited from Monaco\n');
 await dialog.getByRole('button',{name:'Save',exact:true}).click();await expect(dialog).toContainText('Saved');assert.equal(fixture.workspaceFiles.get('alpha/api/README.md').text,'edited from Monaco\n');
 await editor.focus();await page.keyboard.press('ControlOrMeta+a');await page.keyboard.insertText('unsaved draft');fixture.workspaceFiles.get('alpha/api/README.md').version='changed-on-disk';
 await dialog.getByRole('button',{name:'Save',exact:true}).click();await expect(dialog.getByRole('alert')).toContainText('File changed');await expect(dialog.locator('.view-lines')).toContainText('unsaved draft');
 await page.keyboard.press('Escape');await expect(dialog).toBeVisible();await expect(dialog.getByText('Discard your unsaved changes?')).toBeVisible();await dialog.getByRole('button',{name:'Keep editing'}).click();
 await dialog.getByRole('button',{name:'Maximize file window'}).click();assert.ok((await dialog.boundingBox()).width>1400);
 await page.screenshot({path:'/tmp/lazybox-editor-verified.png'});
 await dialog.getByRole('button',{name:'Close file preview'}).click();await dialog.getByRole('button',{name:'Discard changes',exact:true}).click();await expect(dialog).toHaveCount(0);
 await page.getByLabel('Actions for Renamed',{exact:true}).click();await page.getByRole('button',{name:'Manage repositories…',exact:true}).click();await expect(page.getByRole('dialog',{name:'Repositories in Renamed'})).toBeVisible();await expect(page.getByRole('button',{name:'Add repositories to Renamed',exact:true})).toBeVisible();await page.keyboard.press('Escape');
 await page.getByLabel('Actions for Renamed',{exact:true}).click();await page.getByRole('button',{name:'Delete conversation…',exact:true}).first().click();
 const deletion=page.getByRole('dialog',{name:'Delete conversation',exact:true});await expect(deletion.getByRole('button',{name:'Stop and delete'})).toBeDisabled();
 // The confirmation reads as one: the name is a subtitle (one heading), the box and its words share a line,
 // and the danger button is plainly off until the box is ticked, then a full danger fill.
 {
  await expect(deletion.getByRole('button',{name:'Cancel',exact:true})).toBeFocused();await expect(deletion.getByRole('heading')).toHaveCount(1);await expect(deletion.getByRole('heading')).toHaveText('Delete conversation?');
  const token=name=>page.evaluate(name=>{const probe=document.createElement('span');probe.style.background=`var(--${name})`;document.body.append(probe);const value=getComputedStyle(probe).backgroundColor;probe.remove();return value;},name);
  const label=deletion.locator('label').filter({hasText:'Stop its agent and running commands before deleting'}),box=label.getByRole('checkbox');
  await expect(box).toHaveAccessibleName('Stop its agent and running commands before deleting');
  const b=await box.boundingBox(),l=await label.boundingBox(),text=await label.locator('.confirm-check-text').boundingBox();
  const line=await label.evaluate(node=>parseFloat(getComputedStyle(node).lineHeight));
  assert.ok(text.height<line*1.5,`the label is one line (${text.height}px for ${line}px lines)`);
  assert.ok(b.x+b.width<=text.x&&text.x-(b.x+b.width)<14,'the box sits just left of its words');
  assert.ok(Math.abs((b.y+b.height/2)-(text.y+line/2))<1.5,'the box is centred on the first line of its words');
  assert.ok(l.height<line*1.5+2,'box and words: one row');
  const confirm=deletion.getByRole('button',{name:'Stop and delete'});
  assert.equal(await confirm.evaluate(node=>getComputedStyle(node).opacity),'1','off is plain grey, not faded red');
  assert.equal(await confirm.evaluate(node=>getComputedStyle(node).backgroundColor),await token('surface-strong'));
  await page.mouse.move(0,0);
  for(const scheme of ['light','dark']){await page.emulateMedia({colorScheme:scheme});await page.waitForTimeout(200);await deletion.screenshot({path:`.cache/confirm-delete-conversation-${scheme}.png`});}
  await box.check();
  await expect(confirm).toBeEnabled();
  // Buttons ease their background (workspace.css): the fill settles within a frame or two.
  await expect.poll(()=>confirm.evaluate(node=>getComputedStyle(node).backgroundColor),{message:'ticked: a full danger fill'}).toBe(await token('danger-solid'));
  for(const scheme of ['light','dark']){await page.emulateMedia({colorScheme:scheme});await page.waitForTimeout(200);await deletion.screenshot({path:`.cache/confirm-delete-conversation-ready-${scheme}.png`});}
  await page.emulateMedia({colorScheme:null});
 }await deletion.getByRole('button',{name:'Stop and delete'}).click();await expect(deletion).toHaveCount(0);await expect(page.locator('.workspace-agent-label').filter({hasText:'model-fixture'})).toHaveCount(0);
 assert.equal(fixture.mutations.filter(call=>call.method==='DeleteConversation').length,1);assert.equal(fixture.collections.length,2);
 assert.deepEqual(errors,[]);console.log('PASS: metadata, rename without replay, cached navigation/draft, scoped repository management, tall Monaco editing, version conflict, unsaved guard, deletion. WebKit fixtures only.');
}finally{await browser?.close();await server.close();await fixture.close();}
