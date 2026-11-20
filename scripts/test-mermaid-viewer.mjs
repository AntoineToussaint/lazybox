// Mermaid diagrams in a conversation can be read: zoom (buttons, keyboard,
// Ctrl/⌘ + wheel), pan (drag, arrow keys), fit, actual size, full screen and
// copy of the source and the SVG. Off-screen diagrams are not rendered until
// the reader nears them. Fixture only; WebKit by default (the desktop engine).
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from 'vite';
import react from '@vitejs/plugin-react';
import {webkit,chromium,expect} from './fixtures/playwright.mjs';
import {fileURLToPath} from 'node:url';
import {mkdir} from 'node:fs/promises';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';

const fixture=await startRunnerFixture();const session=fixture.sessions[0];
Object.assign(session,{busy:false,status:'done',agents:[{id:'root',status:'done'}]});
const channel=fixture.openEvents(session.id);
const wide='flowchart LR\n'+Array.from({length:14},(_,i)=>`  N${i}[Step ${i} of the pipeline] --> N${i+1}[Step ${i+1} of the pipeline]`).join('\n');
const offscreen='flowchart TD\n  Early --> Diagram';
channel.push({type:'operation_accepted',text:'Draw the early diagram'});
channel.push({type:'message_delta',text:'```mermaid\n'+offscreen+'\n```',turnId:'t0'});
for(let i=0;i<12;i++){channel.push({type:'operation_accepted',text:`Question ${i}`});channel.push({type:'message_delta',text:`Answer ${i}\n\n${'Long prose that pushes the early diagram far above the viewport. '.repeat(30)}`,turnId:`t${i+1}`});}
channel.push({type:'operation_accepted',text:'Draw the pipeline'});
channel.push({type:'message_delta',text:'Here is the pipeline:\n\n```mermaid\n'+wide+'\n```\n\nDIAGRAM_END',turnId:'last'});
const server=await createServer({...testViteCache(import.meta.url),configFile:false,root:fileURLToPath(new URL('../apps/web',import.meta.url)),logLevel:'warn',plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
let browser;
try{
 await mkdir('.cache',{recursive:true});await server.listen();browser=await (process.env.TEST_CHROMIUM?chromium:webkit).launch({headless:true});
 const context=await browser.newContext({viewport:{width:1280,height:900}});
 const page=await context.newPage();page.setDefaultTimeout(15000);
 const errors=[];page.on('pageerror',e=>errors.push(String(e)));
 await page.addInitScript(()=>{window.__copies=[];Object.defineProperty(navigator,'clipboard',{configurable:true,value:{writeText:async text=>{window.__copies.push(text);}}});});
 await page.addInitScript(()=>{
  const NativeObserver=window.IntersectionObserver;
  window.IntersectionObserver=class extends NativeObserver {
   constructor(callback,options){
    super((entries,observer)=>{
     const ready=entries.filter(entry=>{
      if(window.__holdEarlyDiagram&&options?.rootMargin==='600px 0px'&&entry.isIntersecting&&entry.target===document.querySelector('.mermaid-block')){
       window.__releaseEarlyDiagram=()=>{window.__holdEarlyDiagram=false;callback([entry],observer);};
       return false;
      }
      return true;
     });
     if(ready.length)callback(ready,observer);
    },options);
   }
  };
 });
 await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=${session.id}#token=${fixture.token}`);
 await expect(page.locator('.messages:visible')).toContainText('DIAGRAM_END');
 const figure=page.locator('.mermaid-block').filter({hasText:'Mermaid diagram'}).last();
 const viewer=figure.getByRole('group',{name:'Mermaid diagram viewer'});
 const image=viewer.getByRole('img',{name:'Mermaid diagram'});
 await expect.poll(()=>image.evaluate(img=>img.complete&&img.naturalWidth>0)).toBe(true);
 // Only the diagram near the screen is rendered; the early one waits for the reader.
 await expect(page.locator('.mermaid-block').first().getByRole('status')).toHaveText('Rendering diagram…');
 const level=figure.getByRole('status',{name:'Zoom level'}).or(figure.locator('output[aria-label="Zoom level"]'));
 const zoom=async()=>Number((await level.first().textContent()).replace('%',''));
 const box=()=>image.evaluate(img=>{const r=img.getBoundingClientRect(),v=img.parentElement.getBoundingClientRect();return{x:r.left-v.left,y:r.top-v.top,width:r.width,height:r.height,viewport:v.width,viewportHeight:v.height};});

 // Fit: a wide diagram fits the width of the conversation and is centred vertically.
 const fitted=await box();
 assert.ok(fitted.width<=fitted.viewport+1,`fitted diagram is within the viewport (${fitted.width} > ${fitted.viewport})`);
 assert.ok(Math.abs(fitted.width-fitted.viewport)<2||fitted.height>=fitted.viewportHeight-2,'fit uses the available width or height');
 const fit=await zoom();assert.ok(fit<100,'a wide diagram starts below actual size');
 await expect(figure.getByRole('button',{name:'Fit diagram'})).toHaveAttribute('aria-pressed','true');
 await page.screenshot({path:'.cache/mermaid-viewer-fit.png'});

 // Buttons zoom in steps and centre on the viewport; actual size is 100%.
 await figure.getByRole('button',{name:'Zoom in'}).click();
 await expect.poll(async()=>Math.abs(await zoom()-fit*1.25)<=1).toBe(true);
 await expect(figure.getByRole('button',{name:'Fit diagram'})).toHaveAttribute('aria-pressed','false');
 await figure.getByRole('button',{name:'Actual size'}).click();await expect.poll(zoom).toBe(100);
 const actual=await box();
 assert.ok(actual.width>actual.viewport,'at 100% the wide diagram overflows and can be panned');

 // Drag pans; the diagram cannot be dragged entirely out of view.
 const view=await viewer.boundingBox();
 await page.mouse.move(view.x+view.width/2,view.y+view.height/2);await page.mouse.down();
 await page.mouse.move(view.x+view.width/2-200,view.y+view.height/2,{steps:6});await page.mouse.up();
 const dragged=await box();assert.ok(Math.abs(dragged.x-(actual.x-200))<2,`drag pans by the pointer movement (${actual.x} → ${dragged.x})`);
 await page.mouse.move(view.x+view.width/2,view.y+view.height/2);await page.mouse.down();
 await page.mouse.move(view.x+view.width/2-20000,view.y+view.height/2,{steps:4});await page.mouse.up();
 const clamped=await box();assert.ok(Math.abs(clamped.x+clamped.width-clamped.viewport)<2,'panning stops at the diagram edge');

 // Keyboard: arrows pan, + and − zoom, 0 fits, 1 is actual size.
 await viewer.focus();
 await page.keyboard.press('ArrowLeft');const left=await box();assert.ok(Math.abs(left.x-(clamped.x+48))<2,'ArrowLeft pans by one step');
 await page.keyboard.press('Shift+ArrowLeft');assert.ok(Math.abs((await box()).x-(left.x+192))<2,'Shift+Arrow pans by a large step');
 await page.keyboard.press('+');await expect.poll(zoom).toBe(125);
 await page.keyboard.press('-');await expect.poll(zoom).toBe(100);
 await page.keyboard.press('0');await expect.poll(zoom).toBe(fit);
 await page.keyboard.press('1');await expect.poll(zoom).toBe(100);
 await page.keyboard.press('0');

 // Ctrl/⌘ + wheel zooms about the pointer; a plain wheel keeps scrolling the conversation.
 const pane=page.locator('.messages:visible');
 await pane.evaluate(node=>{node.scrollTop=node.scrollHeight;});
 const before=await pane.evaluate(node=>node.scrollTop);
 await page.mouse.move(view.x+view.width/2,view.y+view.height/2);
 await page.keyboard.down('Control');await page.mouse.wheel(0,-200);await page.keyboard.up('Control');
 await expect.poll(zoom).toBeGreaterThan(fit);
 assert.equal(await pane.evaluate(node=>node.scrollTop),before,'Ctrl+wheel zooms the diagram, not the conversation');
 await page.mouse.wheel(0,-300);await expect.poll(()=>pane.evaluate(node=>node.scrollTop)).toBeLessThan(before);
 await pane.evaluate(node=>{node.scrollTop=node.scrollHeight;});
 // Double-click zooms in about the pointer.
 await viewer.getByRole('img').evaluate(()=>{});await figure.getByRole('button',{name:'Fit diagram'}).click();await expect.poll(zoom).toBe(fit);
 const rect=await viewer.boundingBox();await page.mouse.dblclick(rect.x+rect.width/2,rect.y+rect.height/2);await expect.poll(async()=>Math.abs(await zoom()-fit*2)<=1,{message:'double-click zooms in twofold'}).toBe(true);

 // Copy the Mermaid source and the rendered SVG.
 await figure.getByRole('button',{name:'Copy Mermaid source'}).click();
 await figure.getByRole('button',{name:'Copy SVG'}).click();
 const copies=await page.evaluate(()=>window.__copies);
 assert.equal(copies.at(-2),wide);assert.match(copies.at(-1),/^<svg[^>]*width="[\d.]+"[^>]*height="[\d.]+"/);
 await expect(figure.getByRole('button',{name:'Copy SVG'})).toHaveText('Copied');

 // Source view and back.
 await figure.getByRole('button',{name:'Source',exact:true}).click();await expect(figure.locator('pre')).toContainText('Step 13 of the pipeline');
 await figure.getByRole('button',{name:'Diagram',exact:true}).click();await expect(viewer).toBeVisible();

 // Full screen: opens fitted to the window, zooms and pans, Esc closes and returns focus.
 await figure.getByRole('button',{name:'Open full screen'}).click();
 const full=page.getByRole('dialog',{name:'Mermaid diagram, full screen'});
 await expect(full).toBeVisible();
 const fullViewer=full.getByRole('group',{name:'Mermaid diagram viewer, full screen'});await expect(fullViewer).toBeFocused();
 const fullImage=fullViewer.getByRole('img',{name:'Mermaid diagram'});
 await expect.poll(()=>fullImage.evaluate(img=>img.getBoundingClientRect().width)).toBeGreaterThan(fitted.width);
 const fullBox=await fullViewer.boundingBox();assert.ok(fullBox.width>=1270&&fullBox.height>700,'full screen uses the window');
 const fullZoom=async()=>Number((await full.locator('output[aria-label="Zoom level"]').textContent()).replace('%',''));
 const opened=await fullZoom();await page.keyboard.press('+');await expect.poll(async()=>Math.abs(await fullZoom()-opened*1.25)<=1).toBe(true);
 await page.screenshot({path:'.cache/mermaid-viewer-fullscreen.png'});
 // In full screen the plain wheel pans, since there is no page behind it to scroll.
 const fx=await fullImage.evaluate(img=>img.getBoundingClientRect().left);
 await page.mouse.move(fullBox.x+fullBox.width/2,fullBox.y+fullBox.height/2);await page.mouse.wheel(120,0);
 await expect.poll(()=>fullImage.evaluate(img=>img.getBoundingClientRect().left)).toBeLessThan(fx-100);
 await page.keyboard.press('Escape');await expect(full).toHaveCount(0);
 await expect(figure.getByRole('button',{name:'Open full screen'})).toBeFocused();
 // Focusing the full-screen viewer must not scroll the dock to a hidden panel parked below it.
 assert.equal(await page.evaluate(()=>document.querySelector('.dock-container')?.scrollTop??0),0,'full screen scrolled the dock');
 // Enter on the focused viewer opens full screen too.
 await viewer.focus();await page.keyboard.press('Enter');await expect(full).toBeVisible();
 await full.getByRole('button',{name:'Close full screen'}).click();await expect(full).toHaveCount(0);

 // Scrolling toward the early diagram renders it just before it is on screen,
 // and the text being read does not move when the diagram above it grows.
 const pending=page.locator('.mermaid-block').first();
 // The reader scrolls up with the wheel until the unrendered diagram is 300px above the pane.
 await page.evaluate(()=>{window.__holdEarlyDiagram=true;});
 const distance=await pane.evaluate(node=>node.querySelector('.mermaid-block').getBoundingClientRect().bottom-node.getBoundingClientRect().top+300);
 const paneBox=await pane.boundingBox();await page.mouse.move(paneBox.x+paneBox.width/2,paneBox.y+40);
 const target=await pane.evaluate((node,d)=>node.scrollTop+d,distance);await page.mouse.wheel(0,distance);
 await expect.poll(()=>pane.evaluate(node=>node.scrollTop)).toBeLessThan(target+2);
 await expect(page.getByRole('button',{name:'Jump to latest'})).toBeVisible();
 const reading=()=>pane.evaluate(node=>{const top=node.getBoundingClientRect().top;const row=[...node.querySelectorAll('.message')].find(m=>m.getBoundingClientRect().bottom>top);return{text:row.textContent.slice(0,40),offset:Math.round(row.getBoundingClientRect().top-top)};});
 const held=await reading();
 // Hold the observer notification until wheel movement is measured. Otherwise
 // rendering can legitimately change scrollTop before the wheel assertion.
 await expect.poll(()=>page.evaluate(()=>typeof window.__releaseEarlyDiagram)).toBe('function');
 await page.evaluate(()=>window.__releaseEarlyDiagram());
 await expect.poll(()=>page.locator('.mermaid-block img').count(),{timeout:10000}).toBe(2);
 const grown=await pending.evaluate(node=>node.getBoundingClientRect().height);assert.ok(grown>150,`the early diagram rendered above the reader (${grown}px)`);
 const after=await reading();assert.equal(after.text,held.text);assert.ok(Math.abs(after.offset-held.offset)<=2,`reading position held while a diagram above rendered (${held.offset} → ${after.offset})`);
 await pane.evaluate(node=>{node.scrollTop=0;node.dispatchEvent(new Event('scroll'));});await expect(pending.getByRole('img',{name:'Mermaid diagram'})).toBeVisible();

 // Narrow window: the toolbar wraps instead of overflowing.
 await page.setViewportSize({width:700,height:800});await pane.evaluate(node=>{node.scrollTop=node.scrollHeight;});
 const overflow=await figure.evaluate(node=>node.scrollWidth-node.clientWidth);assert.ok(overflow<=1,`diagram block fits a narrow window (overflow ${overflow}px)`);
 await mkdir('.cache',{recursive:true});await page.screenshot({path:'.cache/mermaid-viewer-narrow.png'});
 assert.deepEqual(errors,[]);
 console.log('PASS: Mermaid diagrams fit, zoom (buttons, keys, Ctrl+wheel, double-click), pan (drag, arrows, clamped), copy source/SVG, toggle source, open full screen (wheel pans, Esc returns focus) and render only near the viewport. Fixture only.');
}finally{await browser?.close();await server.close();await fixture.close();}
