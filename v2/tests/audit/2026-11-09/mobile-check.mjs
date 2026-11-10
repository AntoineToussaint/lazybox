import {createServer} from 'vite';
import react from '@vitejs/plugin-react';
import {webkit,expect} from '@playwright/test';
import {fileURLToPath} from 'node:url';
import {startRunnerFixture} from '../../../scripts/fixtures/runner-ui.mjs';
const fixture=await startRunnerFixture();
const server=await createServer({configFile:false,root:fileURLToPath(new URL('../../../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
let browser;
try {
 await server.listen(); browser=await webkit.launch();
 const page=await browser.newPage({viewport:{width:390,height:844},isMobile:true,hasTouch:true,deviceScaleFactor:1});
 await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`);
 await expect(page.locator('.workspace-row').first()).toBeVisible();
 await expect(page.getByRole('textbox',{name:'Message',exact:true})).toBeAttached();
 console.log(JSON.stringify(await page.evaluate(()=>({viewport:{width:innerWidth,height:innerHeight},documentWidth:document.documentElement.scrollWidth,header:document.querySelector('.workspace-header')?.getBoundingClientRect().toJSON(),composer:document.querySelector('.composer')?.getBoundingClientRect().toJSON(),chat:document.querySelector('.messages')?.getBoundingClientRect().toJSON()}))));
 await page.screenshot({path:fileURLToPath(new URL('../../../.cache/audit-2026-11-09/phone-current.png',import.meta.url)),fullPage:true});
} finally { await browser?.close(); await server.close(); await fixture.close(); }
