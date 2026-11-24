// Mermaid diagrams render wherever and however they arrive, and say why when
// they cannot: the sequence diagram an agent sent that once showed only
// "Diagram preview unavailable"; a diagram pushed into a folded or hidden
// panel; three diagrams streamed at once; a theme with colours Mermaid cannot
// read; a renderer that fails to load and then can (what the desktop dev build
// hit when its Vite dependency cache was deleted under it); a real syntax
// error. The page runs under the desktop app's CSP (tauri.conf.json), relaxed
// only where Vite's dev server needs it, so the diagram image URL is checked
// against the policy the app ships. Fixture only; WebKit by default (the
// desktop engine), TEST_CHROMIUM=1 for Chromium.
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from 'vite';
import react from '@vitejs/plugin-react';
import {webkit,chromium,expect} from './fixtures/playwright.mjs';
import {fileURLToPath} from 'node:url';
import {readFile,mkdir} from 'node:fs/promises';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import { collectPageErrors } from './fixtures/page-errors.mjs';

const sequence=`sequenceDiagram
    participant U as User
    participant C as Claude (claude.ai / Desktop)
    participant F as Front door (/mcp + OAuth)
    participant I as Their sign-in (WorkOS / IdP)
    C->>F: POST /mcp (no token)
    F-->>C: 401 + where to sign in
    C->>U: open the browser
    U->>F: authorize (PKCE)
    F->>I: usual sign-in
    I-->>F: who they are
    F-->>C: code, then a token for /mcp alone + refresh token
    C->>F: POST /mcp with that token
    F-->>C: their agents, as them`;
const flows=['flowchart LR\n  Plan --> Build --> Ship','flowchart TD\n  A[Request] --> B{Valid?}\n  B -->|yes| C[Serve]\n  B -->|no| D[Reject]','sequenceDiagram\n  participant R as Runner\n  participant A as Agent\n  R->>A: start\n  A-->>R: done'];
const broken='sequenceDiagram\n  A->>B: hi\n  B-->>: oops';

const fixture=await startRunnerFixture();const session=fixture.sessions[0];
Object.assign(session,{busy:false,status:'done',agents:[{id:'root',status:'done'}]});
const channel=fixture.openEvents(session.id);
const {app:{security:{csp:appCsp}}}=JSON.parse(await readFile(new URL('../apps/desktop/src-tauri/tauri.conf.json',import.meta.url),'utf8'));
const server=await createServer({...testViteCache(import.meta.url),configFile:false,root:fileURLToPath(new URL('../apps/web',import.meta.url)),logLevel:'warn',plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
const surfaces=fileURLToPath(new URL('../packages/surfaces/src',import.meta.url));
let browser;
const settle=ms=>new Promise(resolve=>setTimeout(resolve,ms));
try{
 await server.listen();
 const origin=`http://127.0.0.1:${server.httpServer.address().port}`;
 // The app's policy; Vite's dev server adds an inline preamble (Tauri hashes the shipped app's) and its HMR socket.
 const csp=appCsp.replace("default-src 'self'","default-src 'self'; script-src 'self' 'unsafe-inline'").replace('connect-src ',`connect-src 'self' ws://127.0.0.1:* `);
 assert.match(csp,/img-src[^;]*\sdata:(?:\s|;|$)/,'the app policy still allows data: images, which the diagram image is');
 browser=await (process.env.TEST_CHROMIUM?chromium:webkit).launch({headless:true});
 const open=async({preferences,blockRenderer}={})=>{
  const context=await browser.newContext({viewport:{width:1280,height:900}});
  const page=await context.newPage();page.setDefaultTimeout(15000);
  const log={errors:[],console:[],violations:[]};
  await collectPageErrors(page, log.errors);
  page.on('console',m=>{log.console.push(`${m.type()}: ${m.text()}`);if(/Content Security Policy|Refused to/i.test(m.text()))log.violations.push(m.text());});
  await page.route(url=>url.origin===origin&&url.pathname==='/',async route=>{const response=await route.fetch();await route.fulfill({response,headers:{...response.headers(),'content-security-policy':csp}});});
  if(preferences)await page.addInitScript(value=>localStorage.setItem('lazybox:appearance:last',value),JSON.stringify(preferences));
  const blocked={on:!!blockRenderer,hits:0};
  // The pre-bundled Mermaid module answers as Vite did once its cache was deleted under the running dev app.
  await page.route(/\/deps\/mermaid\.js/,route=>{if(!blocked.on)return route.continue();blocked.hits++;return route.fulfill({status:504,body:''});});
  await page.goto(`${origin}/?view=workspace&collection=alpha&session=${session.id}#token=${fixture.token}`);
  await expect(page.locator('.messages:visible')).toBeVisible();
  return {page,log,blocked,close:()=>context.close()};
 };
 const block=(page,marker)=>page.locator('.message').filter({hasText:marker}).locator('.mermaid-block');
 const rendered=locator=>expect.poll(()=>locator.locator('img[alt="Mermaid diagram"]').evaluateAll(images=>images.length>0&&images.every(img=>img.complete&&img.naturalWidth>0&&img.src.startsWith('data:image/svg+xml'))),{timeout:20000}).toBe(true);
 const say=(turn,text)=>{channel.push({type:'operation_accepted',text:turn});channel.push({type:'message_delta',text,turnId:turn});};

 // 1. The agent's sequence diagram renders, under the app's CSP.
 const main=await open();const {page}=main;
 say('seq','Flow:\n\n```mermaid\n'+sequence+'\n```\n\nMARK_SEQUENCE');
 await expect(page.locator('.messages:visible')).toContainText('MARK_SEQUENCE');
 const seq=block(page,'MARK_SEQUENCE');await rendered(seq);
 const size=await seq.locator('img').evaluate(img=>({width:img.naturalWidth,height:img.naturalHeight}));
 assert.ok(size.width>400&&size.height>300,`the sequence diagram has a real size (${size.width}×${size.height})`);
 // Mermaid drew off screen and left nothing behind: no stray SVG, one measuring box at most.
 assert.equal(await page.locator('body > div[id^="dlazybox-mermaid-"], body > svg').count(),0);
 assert.ok(await page.locator('.mermaid-measure').count()<=1);
 assert.equal(await page.locator('.mermaid-measure').evaluate(node=>node.childElementCount),0,'the measuring box is emptied after each render');

 // 2. Pushed into a group that is folded (the app's own minimized state), then unfolded.
 // The panel is pinned by its id: once hidden below, `.messages:visible` no longer finds it.
 const shownPanel=page.locator('.messages:visible').locator('xpath=ancestor::*[contains(concat(" ",normalize-space(@class)," ")," dock-panel-content ")][1]');
 assert.equal(await shownPanel.count(),1,'the conversation sits in a dock panel');
 const content=page.locator(`.dock-panel-content[data-dock-panel="${await shownPanel.getAttribute('data-dock-panel')}"]`);
 assert.equal(await content.count(),1,'the dock panel is found by its id');
 // What DockLayout does to a minimized group's always-rendered panel content.
 await content.evaluate(node=>node.classList.add('dock-content-hidden'));
 say('folded','```mermaid\n'+flows[0]+'\n```\n\nMARK_FOLDED');
 await settle(1500);
 await content.evaluate(node=>node.classList.remove('dock-content-hidden'));
 await expect(page.locator('.messages:visible')).toContainText('MARK_FOLDED');await rendered(block(page,'MARK_FOLDED'));
 // …and into one that is not displayed at all (a background tab, a window not yet laid out).
 await content.evaluate(node=>{node.style.display='none';});
 say('hidden','```mermaid\n'+flows[1]+'\n```\n\nMARK_HIDDEN');
 await settle(1500);
 await content.evaluate(node=>{node.style.display='';});
 await rendered(block(page,'MARK_HIDDEN'));

 // 3. Three diagrams streamed at once, in small chunks, into one answer.
 const three='Three:\n\n'+flows.map((flow,i)=>'```mermaid\n'+flow+'\n```\n\nPart '+(i+1)+'\n\n').join('')+'MARK_THREE';
 channel.push({type:'operation_accepted',text:'three'});
 for(let i=0;i<three.length;i+=17){channel.push({type:'message_delta',text:three.slice(i,i+17),turnId:'three'});await settle(15);}
 await expect(page.locator('.messages:visible')).toContainText('MARK_THREE');
 const streamed=block(page,'MARK_THREE');await expect(streamed).toHaveCount(3);await rendered(streamed);

 // 4. A real syntax error: the reason, the source, and the whole error in the console once.
 say('broken','```mermaid\n'+broken+'\n```\n\nMARK_BROKEN');
 const bad=block(page,'MARK_BROKEN');
 await expect(bad.getByRole('status')).toHaveText('Mermaid could not read line 3 near “: oops”.');
 await expect(bad.locator('pre')).toContainText('B-->>: oops');
 await expect(bad.getByRole('button',{name:'Copy Mermaid source'})).toBeVisible();
 await expect(bad.getByRole('button',{name:'Try again'})).toHaveCount(0);
 await expect.poll(()=>main.log.console.filter(line=>line.startsWith('error: Mermaid diagram not rendered (syntax)')).length).toBe(1);
 await settle(2500);
 assert.equal(main.log.console.filter(line=>line.startsWith('error: Mermaid diagram not rendered (syntax)')).length,1,'logged once, not on every render');
 // Not retried: a parse error is the text's.
 assert.equal(await bad.getByRole('status').textContent(),'Mermaid could not read line 3 near “: oops”.');
 await mkdir('.cache',{recursive:true});await page.screenshot({path:'.cache/mermaid-robust.png'});

 // 5. Theme colours Mermaid's parser refuses. Mermaid itself throws on them…
 const viaApp=await page.evaluate(async({surfaces,sequence})=>{
  const I=await import(`/@fs${surfaces}/appearance/integrations.ts`), A=await import(`/@fs${surfaces}/appearance/apply.ts`), R=await import(`/@fs${surfaces}/mermaid-render.ts`);
  const state=A.getAppearance();
  const tokens={...state.theme.tokens,'accent-bg-strong':'oklch(0.42 0.09 290)','warn-bg':'color-mix(in srgb, var(--warn) 20%, var(--surface))','text':'var(--text-strong)'};
  const vars=I.mermaidTheme({...state,theme:{...state.theme,tokens}});
  let raw='rendered';try{await R.renderMermaid(sequence,{...vars,primaryColor:'oklch(0.42 0.09 290)'});}catch(error){raw=String(error.message);}
  const sanitized=await R.renderMermaid(sequence,vars);
  return {raw,width:sanitized.width,primary:vars.primaryColor,note:vars.noteBkgColor,text:vars.textColor};
 },{surfaces,sequence});
 assert.equal(viaApp.raw,'Unsupported color format: "oklch(0.42 0.09 290)"','Mermaid refuses oklch() handed to it as is');
 // …but the theme Lazybox hands it is sanitized, so the diagram renders.
 assert.ok(viaApp.width>400,'the diagram renders with the sanitized theme');
 for(const colour of [viaApp.primary,viaApp.note,viaApp.text])assert.match(colour,/^#[0-9a-f]{6}([0-9a-f]{2})?$/);
 assert.deepEqual(main.log.errors,[]);assert.deepEqual(main.log.violations,[],'no CSP violation');
 await main.close();
 // The same colours as a person's overrides, through Settings' stored preferences.
 const themed=await open({preferences:{mode:'dark',darkTheme:'dark',lightTheme:'light',typography:{},minContrast:4.5,motion:'system',accent:'oklch(0.7 0.15 150)',tokens:{'accent-bg-strong':'oklch(0.42 0.09 290)','warn-bg':'color-mix(in srgb, red 20%, black)'}}});
 say('themed','```mermaid\n'+sequence+'\n```\n\nMARK_THEMED');
 await expect(themed.page.locator('.messages:visible')).toContainText('MARK_THEMED');await rendered(block(themed.page,'MARK_THEMED'));
 assert.deepEqual(themed.log.errors,[]);
 await themed.close();

 // 6. The renderer fails to load: the reason shows, and it renders once it can load again.
 const offline=await open({blockRenderer:true});
 say('offline','```mermaid\n'+sequence+'\n```\n\nMARK_OFFLINE');
 await expect(offline.page.locator('.messages:visible')).toContainText('MARK_OFFLINE');
 const waiting=block(offline.page,'MARK_OFFLINE');
 await expect(waiting.getByRole('status')).toHaveText('The diagram renderer could not be loaded. Restart Lazybox if this persists.');
 await expect(waiting.locator('pre')).toContainText('participant F as Front door');
 assert.ok(offline.blocked.hits>0);
 offline.blocked.on=false;
 await rendered(waiting);
 await offline.close();
 console.log(`PASS: Mermaid diagrams render robustly (${process.env.TEST_CHROMIUM?'Chromium':'WebKit'}, the app's CSP): the agent's sequence diagram; a diagram pushed into a folded group and into an undisplayed panel renders when shown; three streamed at once; a theme with oklch()/color-mix()/var() colours (Mermaid alone refuses them); a renderer that failed to load recovers; a syntax error names its line, keeps the source and logs once. Fixture only.`);
}finally{await browser?.close();await server.close();await fixture.close();}
