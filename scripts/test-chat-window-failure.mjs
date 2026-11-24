// A chat that cannot get its own window (the browser blocks it) comes back
// where it left: on the page, or, when the person switched workspace while the
// window was being opened, in the workspace it left, never in the one now on
// screen. Likewise the shell Settings → Install Claude Code opens in a workspace
// the person left meanwhile: it waits there, and its panel opens on return.
// WebKit (the desktop app's engine), the browser build's host, against the fake
// Runner fixture.
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from './fixtures/vite-cache.mjs';
import react from '@vitejs/plugin-react';
import {webkit,expect} from './fixtures/playwright.mjs';
import {fileURLToPath} from 'node:url';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import { collectPageErrors } from './fixtures/page-errors.mjs';

const fixture=await startRunnerFixture();
const server=await createServer({...testViteCache(import.meta.url),configFile:false,root:fileURLToPath(new URL('../apps/web',import.meta.url)),logLevel:'warn',plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
let browser;
try{
 await server.listen();
 browser=await webkit.launch();
 const context=await browser.newContext({viewport:{width:1440,height:900}});
 const page=await context.newPage();page.setDefaultTimeout(15000);
 const errors=await collectPageErrors(page);
 // The browser refuses every chat window. Just before it does, a scenario may act as the person
 // would in that moment (switch workspace), so the refusal lands after the switch, every time.
 await page.addInitScript(()=>{
  const open=window.open.bind(window);
  window.open=(url,name,...rest)=>{if(String(name||'').startsWith('chat-')){window.__beforeRefusal?.();return null;}return open(url,name,...rest);};
 });
 await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`);
 const chat=page.locator('.page-layout .panel-chat');
 const nav=page.getByRole('navigation',{name:'Workspaces'});
 const alphaLink=nav.locator('.workspace-row').filter({hasText:'Alpha feature'}).locator('.workspace-link');
 const betaLink=nav.locator('.workspace-row').filter({hasText:'Beta release'}).locator('.workspace-link');
 const sessionChoice=page.getByRole('combobox',{name:'Session',exact:true});
 await expect(chat.getByRole('textbox',{name:'Message',exact:true})).toBeEnabled();
 await expect(sessionChoice).toHaveValue('alpha-session');

 // 1. Refused on the same page: the chat is back on it, and the refusal is said.
 await chat.getByRole('button',{name:'Open chat window ↗',exact:true}).click();
 await expect(page.getByRole('alert').filter({hasText:'The chat window was blocked by the browser'})).toBeVisible();
 await expect(chat.getByRole('textbox',{name:'Message',exact:true})).toBeEnabled();
 await expect(sessionChoice).toHaveValue('alpha-session');

 // 2. Refused after a switch to Beta: Beta keeps its own page; Alpha gets its chat back.
 await page.evaluate(()=>{window.__beforeRefusal=()=>{window.__beforeRefusal=undefined;[...document.querySelectorAll('.workspace-row')].find(row=>row.textContent.includes('Beta release'))?.querySelector('.workspace-link')?.click();};});
 await chat.getByRole('button',{name:'Open chat window ↗',exact:true}).click();
 await expect(betaLink).toHaveAttribute('aria-current','page');
 await expect(page.getByRole('alert').filter({hasText:'The chat window was blocked by the browser'})).toBeVisible();
 await expect(page.getByRole('combobox',{name:'Workspace',exact:true})).toHaveValue('beta');
 await expect(sessionChoice,'Beta shows its own conversation, not the chat that left Alpha').not.toHaveValue('alpha-session');
 await alphaLink.click();
 await expect(alphaLink).toHaveAttribute('aria-current','page');
 await expect(sessionChoice).toHaveValue('alpha-session');
 await expect(chat.getByRole('textbox',{name:'Message',exact:true}),'Alpha has its chat back on the page').toBeEnabled();
 await expect(page.getByRole('region',{name:'In other windows'})).toHaveCount(0);

 // 3. Settings → Install Claude Code opens a shell in Alpha; the person switches to Beta before it
 // is open: Beta stays on screen, and the shell (its installer typed in) waits in Alpha.
 fixture.availability.claude.installed=false;
 await page.getByRole('button',{name:'Settings',exact:true}).click();
 await page.getByRole('tab',{name:'Agents & workspaces'}).click();
 const claude=page.locator('section.claude-settings');
 await claude.getByRole('button',{name:'Check again'}).click();
 const opening=Promise.withResolvers(),asked=Promise.withResolvers();
 await page.route('**/OpenTerminal',async route=>{asked.resolve();await opening.promise;await route.continue();});
 await claude.getByRole('button',{name:'Install Claude Code'}).click();
 await asked.promise;
 await page.keyboard.press('Meta+k');const palette=page.getByRole('dialog',{name:'Command palette'});await palette.getByRole('combobox',{name:'Search commands'}).fill('Beta release');await palette.getByRole('option',{name:/Beta release/}).first().click();
 await expect(betaLink).toHaveAttribute('aria-current','page');
 opening.resolve();
 const installer=()=>[...fixture.terminals.values()].find(t=>t.input.join('').includes('install.sh'));
 await expect.poll(()=>installer()?.input.join('')).toBe('curl -fsSL https://claude.ai/install.sh | bash\r');
 assert.equal(installer().collectionId,'alpha');
 await expect(betaLink,'the shell did not take the person back, or elsewhere').toHaveAttribute('aria-current','page');
 await expect(page.locator('.panel-terminal')).toHaveCount(0);
 await page.unroute('**/OpenTerminal');
 fixture.availability.claude.installed=true;
 // Back in Alpha, the shell is there.
 await alphaLink.click();
 await expect(page.locator('.panel-terminal').getByTestId('terminal')).toContainText('ran: curl -fsSL https://claude.ai/install.sh | bash');
 assert.deepEqual(errors,[]);
 console.log('PASS: a chat whose window is refused comes back on its page, and after a workspace switch meanwhile, in the workspace it left, not the one on screen; an installer shell opened after a switch waits in its workspace and shows on return. Fixture only.');
}finally{await browser?.close();await server.close();await fixture.close();}
