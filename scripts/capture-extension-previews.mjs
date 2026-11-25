// Capture bundled listings with the same WebKit app and fake Runner as the extension UI suite.
import {createServer, testViteCache} from './fixtures/vite-cache.mjs';
import {webkit, expect} from './fixtures/playwright.mjs';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import react from '@vitejs/plugin-react';
import {readdir, readFile} from 'node:fs/promises';
import {parseManifest} from './fixtures/extensions-fake.mjs';
import {fileURLToPath} from 'node:url';

const root = new URL('../services/runner/code/internal/power/extensions/examples/', import.meta.url);
const fixture = await startRunnerFixture();
const server = await createServer({...testViteCache(import.meta.url), configFile:false, root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
const browser = await webkit.launch();
try {
  await server.listen();
  const page = await browser.newPage({viewport:{width:1280,height:800},colorScheme:'dark'});
  await page.goto('http://127.0.0.1:'+server.httpServer.address().port+'/?view=workspace&collection=alpha&session=alpha-session#token='+fixture.token);
  await page.locator('summary[aria-label="Panels"]').click();
  await page.getByRole('button',{name:'New Extensions panel',exact:true}).click();
  for (const id of await readdir(root)) {
    const manifest = parseManifest(await readFile(new URL(id+'/extension.yaml',root),'utf8'));
    const picker = page.getByRole('combobox',{name:'Extension panel'});
    if (await picker.count()) await picker.selectOption({label:'All extensions…'});
    const row = page.locator('.extension-home li').filter({has:page.getByText(manifest.name,{exact:true})});
    await row.getByRole('button',{name:'Add',exact:true}).click();
    const sheet = page.getByRole('dialog',{name:'Allow '+manifest.name+'?'});
    let target = sheet;
    if (id === 'compound-engineering') {
      const doc = fixture.artifacts.agentPut({collectionId:'alpha',sessionId:'alpha-session',kind:'report',
        title:'Compound Engineering · documentation review',
        content:'## Proposed change to AGENTS.md\nThe API team repeated the same verification step in two conversations.\n\n**Add to api/AGENTS.md**\n\nRun the API contract tests after changing a response field. The procedure is in docs/testing.md.\n\n**Evidence** · API handoff, event 14.'});
      const reader = await browser.newPage({viewport:{width:560,height:450},colorScheme:'dark'});
      await reader.goto('http://127.0.0.1:'+server.httpServer.address().port+'/?view=artifact&collection=alpha&artifact='+doc.id+'#token='+fixture.token);
      await expect(reader.getByRole('heading',{name:'Proposed change to AGENTS.md'})).toBeVisible();
      await reader.screenshot({path:fileURLToPath(new URL(id+'/preview.jpg',root)),type:'jpeg',quality:35});
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
    }
    const box = await target.boundingBox();
    const clip = {x:box.x,y:box.y,width:Math.min(box.width,560),height:Math.min(box.height,360)};
    await page.screenshot({path:fileURLToPath(new URL(id+'/preview.jpg',root)),type:'jpeg',quality:35,clip});
    if (id === 'clone-me') {
      await target.getByRole('button',{name:'Save preferences'}).click();
      await expect(target).toHaveCount(0);
    } else if (!manifest.panels?.length) await sheet.getByRole('button',{name:'Not now'}).click();
  }
} catch (error) { console.error(error); throw error; } finally { await browser.close(); await server.close(); await fixture.close(); }
