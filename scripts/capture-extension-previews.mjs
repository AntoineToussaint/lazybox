// Capture bundled listings with the same WebKit app and fake Runner as the extension UI suite.
import {createServer, testViteCache} from './fixtures/vite-cache.mjs';
import {webkit, expect} from './fixtures/playwright.mjs';
import {report} from './fixtures/cost-dashboard.mjs';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import react from '@vitejs/plugin-react';
import {mkdir, readdir, readFile} from 'node:fs/promises';
import {parseManifest} from './fixtures/extensions-fake.mjs';
import {fileURLToPath} from 'node:url';

const root = new URL('../services/runner/code/internal/power/extensions/examples/', import.meta.url);
const inspect = process.argv.includes('--inspect');
const review = inspect || process.argv.includes('--review');
const light = process.argv.includes('--light');
const colorScheme = light ? 'light' : 'dark';
const names = process.argv.slice(2).filter(arg => !['--review', '--inspect', '--light'].includes(arg));
const examples = await readdir(root);
for (const id of names) if (!examples.includes(id)) throw new Error('Unknown bundled extension: '+id);
const output = new URL(light ? '../.cache/extension-review/light/' : '../.cache/extension-review/', import.meta.url);
if (review) await mkdir(output, {recursive:true});
async function reviewPage(id, page, target) {
  await page.mouse.move(0, 0);
  await target.screenshot({path:fileURLToPath(new URL(id+'.png',output))});
  console.log(id, fileURLToPath(new URL(id+'.png',output)));
  if (inspect) await page.pause();
}
const fixture = await startRunnerFixture();
fixture.telemetry = body => report(body.workspace);
fixture.artifacts.agentPut({collectionId:'alpha',sessionId:'alpha-session',kind:'notes',title:'Deployment checklist',content:'Verify the release candidate before promoting it.'});
Object.assign(fixture.sessions.find(item => item.id === 'beta-session'), {title:'Review the migration plan',status:'waiting_input',pending:[{type:'question',requestId:'migration',text:'Which schema should we deploy?'}]});
const server = await createServer({...testViteCache(import.meta.url), configFile:false, root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
const browser = await webkit.launch({headless:!inspect});
try {
  await server.listen();
  const page = await browser.newPage({viewport:{width:1280,height:800},colorScheme});
  const workspaceURL = 'http://127.0.0.1:'+server.httpServer.address().port+'/?view=workspace&collection=alpha&session=alpha-session#token='+fixture.token;
  await page.goto(workspaceURL);
  await page.locator('summary[aria-label="Panels"]').click();
  await page.getByRole('button',{name:'New Extensions panel',exact:true}).click();
  for (const id of examples) {
    if (names.length && !names.includes(id)) continue;
    if (id === 'disk-cleanup') await fixture.extensions.showSampleDiskReport();
    const manifest = parseManifest(await readFile(new URL(id+'/extension.yaml',root),'utf8'));
    const picker = page.getByRole('combobox',{name:'Extension panel'});
    if (await picker.count()) await picker.selectOption({label:'All extensions…'});
    const row = page.locator('.extension-home li').filter({has:page.getByText(manifest.name,{exact:true})});
    await row.getByRole('button',{name:'Install',exact:true}).click();
    const sheet = page.getByRole('dialog',{name:'Allow '+manifest.name+'?'});
    let target = sheet;
    if (id === 'compound-engineering') {
      const doc = fixture.artifacts.agentPut({collectionId:'alpha',sessionId:'alpha-session',kind:'report',
        title:'Compound Engineering · documentation review',
        content:'## Proposed change to AGENTS.md\nThe API team repeated the same verification step in two conversations.\n\n**Add to api/AGENTS.md**\n\nRun the API contract tests after changing a response field. The procedure is in docs/testing.md.\n\n**Evidence** · API handoff, event 14.'});
      const reader = await browser.newPage({viewport:{width:560,height:450},colorScheme});
      await reader.goto('http://127.0.0.1:'+server.httpServer.address().port+'/?view=artifact&collection=alpha&artifact='+doc.id+'#token='+fixture.token);
      await expect(reader.getByRole('heading',{name:'Proposed change to AGENTS.md'})).toBeVisible();
      if (review) await reviewPage(id, reader, reader);
      else await reader.screenshot({path:fileURLToPath(new URL(id+'/preview.jpg',root)),type:'jpeg',quality:35});
      await reader.close();
      await sheet.getByRole('button',{name:'Not now'}).click();
      continue;
    }
    if (id === 'clone-me') {
      await sheet.getByRole('button',{name:'Allow and turn on'}).click();
      await expect(sheet).toHaveCount(0);
      await page.getByRole('button',{name:'Teach Clone Me'}).click();
      target = page.getByRole('dialog',{name:'Teach Clone Me'});
      await target.getByRole('button',{name:'I prefer explicit errors.'}).click();
    } else if (manifest.panels?.length) {
      await sheet.getByRole('button',{name:'Allow and turn on'}).click();
      target = page.locator('iframe.extension-frame');
      const inner = page.frameLocator('iframe.extension-frame').frameLocator('iframe');
      await expect(inner.locator('body')).toBeVisible();
      await expect(inner.locator('body')).not.toContainText('Loading…');
      if (id === 'reminders') {
        await inner.getByLabel('What should you check?').fill('Check the staging deployment');
        await inner.getByRole('button',{name:'Set reminder'}).click();
        await expect(inner.getByRole('listitem').first()).toBeVisible();
      }
      if (id === 'cost-dashboard') await expect(inner.locator('#total')).toHaveText(/\$[1-9]/);
      if (id === 'disk-cleanup') await expect(inner.getByRole('heading', { name: /^Cleanup candidates/ })).toBeVisible();
    }
    if (id === 'subscription-rotation') {
      await sheet.getByRole('button',{name:'Allow and turn on'}).click();
      const settings = await browser.newPage({viewport:{width:1280,height:800},colorScheme});
      await expect(sheet).toHaveCount(0);
      await settings.goto(workspaceURL);
      await settings.getByRole('textbox',{name:'Message',exact:true}).waitFor();
      await settings.keyboard.press('Meta+k');
      const palette = settings.getByRole('dialog',{name:'Command palette'});
      await palette.getByRole('combobox').fill('plugins');
      await palette.getByRole('option',{name:/Extensions settings/}).click();
      await settings.getByRole('button',{name:'Manage rotation accounts'}).click();
      await expect(settings.getByRole('heading',{name:'Subscriptions',exact:true})).toBeVisible();
      if (review) await reviewPage(id, settings, settings);
      else await settings.getByRole('article',{name:'Saved Codex subscription'}).screenshot({
        path:fileURLToPath(new URL(id+'/preview.jpg',root)),type:'jpeg',quality:55,
      });
      await settings.close();
      continue;
    }
    if (review) {
      if (manifest.panels?.length) target = page.locator('.extension-panel').filter({has:page.locator('iframe.extension-frame')});
      await reviewPage(id, page, target);
    } else {
      const box = await target.boundingBox();
      const clip = {x:box.x,y:box.y,width:Math.min(box.width,560),height:Math.min(box.height,360)};
      await page.screenshot({path:fileURLToPath(new URL(id+'/preview.jpg',root)),type:'jpeg',quality:35,clip});
    }
    if (id === 'clone-me') {
      await target.getByRole('button',{name:'Save preferences'}).click();
      await expect(target).toHaveCount(0);
    } else if (!manifest.panels?.length) await sheet.getByRole('button',{name:'Not now'}).click();
  }
} catch (error) { console.error(error); throw error; } finally { await browser.close(); await server.close(); await fixture.close(); }
