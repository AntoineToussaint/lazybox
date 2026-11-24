// Opening a long saved conversation must be fast. This replays a realistic
// long journal (tens of thousands of streamed deltas, large tool outputs, code
// blocks and Mermaid diagrams) from the fixture Runner and measures, in the
// page, when the latest answer is on screen and when the main thread is quiet
// again (no frame longer than 50 ms for one second: the Lighthouse TTI idea).
//
// Wall-clock time depends on the machine and what else runs on it, so the gate
// does not judge it alone. Each run also opens a short saved conversation in
// the same app, server and browser (the calibration: boot, connection and
// first render, under the same load) and the gate bounds the long one
// relative to it, by medians. The absolute budgets are reported, and enforced
// only with PERF_STRICT=1 for a measurement on a quiet machine. The
// latest-first structure (only the tail mounted) is checked on every run.
//
// PERF_TURNS, PERF_BROWSER=chromium|webkit, PERF_BUILD=1 (production bundle),
// PERF_RUNS and PERF_STRICT tune a manual measurement; the gate uses the defaults.
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer,build,preview} from 'vite';
import react from '@vitejs/plugin-react';
import {webkit,chromium,expect} from './fixtures/playwright.mjs';
import {fileURLToPath} from 'node:url';
import {mkdtemp,rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import {longConversation} from './fixtures/long-conversation.mjs';
import {monacoVimCompat} from './monaco-vim-compat.mjs';
import { collectPageErrors } from './fixtures/page-errors.mjs';

const turns=Number(process.env.PERF_TURNS||200),runs=Number(process.env.PERF_RUNS||3),strict=!!process.env.PERF_STRICT;
// For a quiet machine (PERF_STRICT=1); the unfixed reader took several times
// longer than this on the default fixture.
const budget={visible:Number(process.env.PERF_VISIBLE_BUDGET||5000),interactive:Number(process.env.PERF_INTERACTIVE_BUDGET||7000)};
// How many times the calibration's time the long conversation may take. It
// measured 2.1-3.0x, idle and with four copies running at load 80-90; the
// unfixed reader took several times the 5 s absolute budget, against a
// calibration under 0.5 s.
const ratioBudget={visible:Number(process.env.PERF_VISIBLE_RATIO||6),interactive:Number(process.env.PERF_INTERACTIVE_RATIO||6)};
const fixture=await startRunnerFixture();
const session=fixture.sessions[0];Object.assign(session,{closed:true,status:'stopped',busy:false,providerReference:'saved-claude-conversation',agents:[{id:'root',status:'done'}],provider:'claude'});
const channel=fixture.openEvents(session.id);
const events=longConversation({turns});for(const event of events)channel.push(event);channel.complete();
// Deliver the replay in many network chunks, as the Runner's per-record stream does.
channel.pace=Number(process.env.PERF_PACE||50);channel.paceMs=Number(process.env.PERF_PACE_MS||1);
// The calibration: a short saved conversation in another workspace, replayed the same way.
const calibration=fixture.sessions[1];Object.assign(calibration,{closed:true,status:'stopped',busy:false,providerReference:'saved-claude-calibration',agents:[{id:'root',status:'done'}],provider:'claude'});
const calibrationTurns=3,calibrationChannel=fixture.openEvents(calibration.id);
for(const event of longConversation({turns:calibrationTurns}))calibrationChannel.push(event);calibrationChannel.complete();
calibrationChannel.pace=channel.pace;calibrationChannel.paceMs=channel.paceMs;
const root=fileURLToPath(new URL('../apps/web',import.meta.url));
const proxy={'/lazybox.bench.v1.RunnerService/':{target:fixture.url}};
let server,out,browser;
try{
 if(process.env.PERF_BUILD){out=await mkdtemp(join(tmpdir(),'lazybox-perf-'));await build({configFile:false,root,logLevel:'warn',plugins:[react(),monacoVimCompat()],build:{outDir:out,emptyOutDir:true}});server=await preview({configFile:false,root,build:{outDir:out},preview:{host:'127.0.0.1',port:0,proxy}});}
 else{server=await createServer({...testViteCache(import.meta.url),configFile:false,root,logLevel:'warn',plugins:[react(),monacoVimCompat()],server:{host:'127.0.0.1',port:0,proxy}});await server.listen();}
 const port=(server.httpServer.address()).port;
 browser=await (process.env.PERF_BROWSER==='chromium'?chromium:webkit).launch({headless:true});
 const results=[],calibrations=[];
 const open=async({target,collection,count,long})=>{
  const page=await browser.newPage({viewport:{width:1440,height:900}});page.setDefaultTimeout(60000);
  const errors=[];await collectPageErrors(page, errors);
  const marker=`ANSWER_END_${count-1}`;
  await page.addInitScript(marker=>{
   const perf={first:undefined,visible:undefined,frames:[],marker};window.__perf=perf;let last=performance.now();
   const frame=now=>{const gap=now-last;last=now;if(gap>50)perf.frames.push({end:now,gap});
    if(perf.first===undefined&&document.querySelector('.messages .message'))perf.first=now;
    if(perf.visible===undefined&&document.querySelector('.messages')?.textContent?.includes(marker))perf.visible=now;
    requestAnimationFrame(frame);};
   requestAnimationFrame(frame);
  },marker);
  await page.goto(`http://127.0.0.1:${port}/?view=workspace&collection=${collection}&session=${target.id}#token=${fixture.token}`);
  await expect.poll(()=>page.evaluate(()=>window.__perf.visible),{timeout:120000,intervals:[50]}).not.toBeUndefined();
  // Interactive: one full second without a long frame after the answer is visible.
  await expect.poll(()=>page.evaluate(()=>{const p=window.__perf,now=performance.now(),longest=p.frames.at(-1)?.end??0;return now-Math.max(longest,p.visible)>=1000;}),{timeout:120000,intervals:[100]}).toBe(true);
  const measured=await page.evaluate(()=>{const p=window.__perf,lastLong=p.frames.filter(f=>f.end>=0).at(-1)?.end??0;
   return{first:Math.round(p.first),visible:Math.round(p.visible),interactive:Math.round(Math.max(p.visible,lastLong)),longFrames:p.frames.length,blocking:Math.round(p.frames.reduce((sum,f)=>sum+f.gap-50,0)),worstFrame:Math.round(Math.max(0,...p.frames.map(f=>f.gap)))};});
  // The page answers input once interactive: typing reaches the composer promptly.
  const composer=page.getByRole('textbox',{name:'Message',exact:true});
  const started=Date.now();await composer.pressSequentially('ok');await expect(composer).toHaveValue('ok');measured.typing=Date.now()-started;
  await expect(page.locator('.messages:visible')).toContainText(marker);
  if(long){
  // Latest first: only the tail is mounted; earlier history waits for the reader.
  const mounted=await page.locator('.messages:visible .message').count();
  assert.ok(mounted>0&&mounted<=40,`a long conversation opens with its latest messages only (${mounted} mounted)`);
  await expect(page.getByRole('button',{name:'Show earlier messages'})).toBeVisible();
  }
  assert.deepEqual(errors,[]);
  await page.close();
  return measured;
 };
 for(let run=0;run<=runs;run++){
  // Run 0 warms Vite's dependency optimisation and module cache; it is not
  // measured. The two pages alternate, so both see the same load.
  const calibrated=await open({target:calibration,collection:'beta',count:calibrationTurns,long:false});
  const measured=await open({target:session,collection:'alpha',count:turns,long:true});
  if(run>0){results.push(measured);calibrations.push(calibrated);}
 }
 const medianOf=(rows,key)=>rows.map(r=>r[key]).sort((a,b)=>a-b)[Math.floor(rows.length/2)];
 const median=key=>medianOf(results,key);
 const summary={turns,events:events.length,browser:process.env.PERF_BROWSER||'webkit',bundle:process.env.PERF_BUILD?'production':'development',runs:results,median:{first:median('first'),visible:median('visible'),interactive:median('interactive'),blocking:median('blocking'),worstFrame:median('worstFrame')},calibration:{turns:calibrationTurns,visible:medianOf(calibrations,'visible'),interactive:medianOf(calibrations,'interactive')}};
 // A calibration faster than one frame says nothing about the machine; floor it.
 const base={visible:Math.max(summary.calibration.visible,100),interactive:Math.max(summary.calibration.interactive,100)};
 summary.ratio={visible:+(summary.median.visible/base.visible).toFixed(2),interactive:+(summary.median.interactive/base.interactive).toFixed(2)};
 console.log(JSON.stringify(summary));
 assert.ok(summary.ratio.visible<=ratioBudget.visible,`latest answer took ${summary.ratio.visible}x the calibration's time to appear (${summary.median.visible} vs ${summary.calibration.visible} ms; budget ${ratioBudget.visible}x)`);
 assert.ok(summary.ratio.interactive<=ratioBudget.interactive,`page took ${summary.ratio.interactive}x the calibration's time to become interactive (${summary.median.interactive} vs ${summary.calibration.interactive} ms; budget ${ratioBudget.interactive}x)`);
 const overBudget=summary.median.visible>budget.visible||summary.median.interactive>budget.interactive;
 if(overBudget){
  const note=`absolute budgets ${budget.visible}/${budget.interactive} ms exceeded (${summary.median.visible}/${summary.median.interactive} ms) while the calibration took ${summary.calibration.visible} ms: a loaded machine, not the reader`;
  assert.ok(!strict,note);console.warn(`NOTE: ${note}.`);
 }
 console.log(`PASS: a ${events.length}-event saved conversation shows its latest answer in ${summary.median.visible} ms and is interactive in ${summary.median.interactive} ms, ${summary.ratio.visible}x/${summary.ratio.interactive}x a ${calibrationTurns}-turn calibration (budgets ${ratioBudget.visible}x/${ratioBudget.interactive}x; absolute ${budget.visible}/${budget.interactive} ms${strict?', enforced':', reported'}). Fixture only.`);
}finally{await browser?.close();await server?.close?.();if(server?.httpServer)server.httpServer.close();await fixture.close();if(out)await rm(out,{recursive:true,force:true});}
