// Synthetic authority only: account selection and workspace intent, no inference.
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from 'vite';
import react from '@vitejs/plugin-react';
import {chromium,expect} from './fixtures/playwright.mjs';
import {fileURLToPath} from 'node:url';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
const fixture=await startRunnerFixture();
fixture.profiles.push(...['Gmail','Work subscription'].map((label,i)=>({id:`claude-${i}`,provider:'claude',label,managed:true,isolation:'environment'})));
const server=await createServer({...testViteCache(import.meta.url),configFile:false,root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
let browser;
try{
 await server.listen();browser=await chromium.launch({headless:true});const page=await browser.newPage();
 await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace#token=${fixture.token}`);
 await page.getByRole('button',{name:'Settings',exact:true}).click();
 for(const [i,label] of ['Gmail','Work subscription'].entries()){
  await page.getByRole('button',{name:'New conversation',exact:true}).click();
  const dialog=page.getByRole('dialog',{name:'New conversation',exact:true});
  await expect(dialog.getByText('Starts in a new workspace without repositories.',{exact:true})).toBeVisible();
  await expect(dialog.getByLabel('Working directory')).toHaveCount(0);
  await dialog.getByLabel('Account profile').selectOption({label});
  if(i===1){
   await dialog.getByLabel('Behavior profile').selectOption('check-in-first-v1');
   await expect(dialog.getByRole('combobox',{name:'Permissions',exact:true})).toHaveValue('read-only');
   await expect(dialog.getByRole('combobox',{name:'Permissions',exact:true})).toBeDisabled();
  }
  const before=fixture.mutations.length;
  await dialog.getByRole('button',{name:'Start Claude ↗',exact:true}).click();
  await expect(dialog).not.toBeVisible();await expect(page.getByRole('textbox',{name:'Message',exact:true})).toBeVisible();
  const request=fixture.mutations.at(-1);assert.equal(fixture.mutations.length,before+1);assert.equal(request.method,'OpenSession');assert.equal(request.body.profileId,`claude-${i}`);assert.equal(request.body.emptyWorkspace,true);assert.ok(!request.body.cwd&&!request.body.collectionId&&!request.body.resumeReference);assert.ok(!request.body.options.gatewayId);assert.equal(request.body.options.permission||'',i===1?'read-only':'');assert.equal(request.body.options.behaviorProfile||'',i===1?'check-in-first-v1':'');
  await expect(page.getByRole('combobox',{name:'Session',exact:true})).toContainText(label);
 }
 await page.getByRole('button',{name:'New conversation',exact:true}).click();
 const next=page.getByRole('dialog',{name:'New conversation',exact:true});
 await expect(next.getByRole('combobox',{name:'Behavior profile'})).toHaveValue('check-in-first-v1');
 await next.getByRole('combobox',{name:'Agent',exact:true}).click();
 await next.getByRole('option',{name:/^Codex/}).click();
 await expect(next.getByRole('combobox',{name:'Behavior profile'})).toHaveValue('check-in-first-v1');
 await next.getByRole('button',{name:'Close new conversation'}).click();
 const empty=fixture.sessions.filter(s=>s.id.startsWith('empty-'));assert.equal(empty.length,2);assert.notEqual(empty[0].workingDirectory,empty[1].workingDirectory);
 await page.reload();await expect(page.getByRole('combobox',{name:'Session',exact:true})).toHaveValue(empty[1].id);
 await page.getByRole('combobox',{name:'Session',exact:true}).selectOption(empty[0].id);await expect(page.getByText(`Retained conversation for ${empty[0].id}`,{exact:true})).toBeVisible();
 assert.equal(fixture.calls.filter(c=>c.method==='CloseSession').length,0);
 console.log('PASS: New session from Settings/Work, empty workspace with either saved account, distinct workspaces, retained sessions and native permission defaults. Fixture only.');
}finally{await browser?.close();await server.close();await fixture.close();}
