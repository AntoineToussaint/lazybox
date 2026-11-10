import {createServer} from 'vite';
import react from '@vitejs/plugin-react';
import {webkit,expect} from '@playwright/test';
import {fileURLToPath} from 'node:url';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
const fixture=await startRunnerFixture();fixture.sessions[0].model='requested-alias';fixture.sessions[0].modelReported=false;fixture.sessions[0].provider='claude';
const channel=fixture.openEvents('alpha-session');
channel.push({type:'session_ready',providerDataJson:Buffer.from(JSON.stringify({model:'claude-reported-model'})).toString('base64')});
for(let i=0;i<30;i++){channel.push({type:'operation_accepted',text:`Question ${i}`});channel.push({type:'message_delta',text:`Answer ${i}\n\n${'Long answer. '.repeat(40)}`});}
const server=await createServer({configFile:false,root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
let browser;
try{
 await server.listen();browser=await webkit.launch({headless:true});const page=await browser.newPage({viewport:{width:1440,height:900}});page.setDefaultTimeout(15000);
 const errors=[];page.on('pageerror',e=>errors.push(String(e)));
 await page.route('**/GetSession',async route=>{await new Promise(resolve=>setTimeout(resolve,500));await route.continue();});
 await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`);
 const pane=page.locator('.messages');const gap=()=>pane.evaluate(node=>node.scrollHeight-node.clientHeight-node.scrollTop);
 await expect(pane).toContainText('Answer 29');await expect(page.locator('.conversation-title')).toBeVisible();
 await expect.poll(gap,{message:'Late session header must not leave the last lines below the viewport'}).toBeLessThan(3);
 await page.setViewportSize({width:1100,height:650});
 await expect.poll(gap,{message:'Resizing a followed chat must keep its real bottom visible'}).toBeLessThan(3);
 channel.push({type:'operation_accepted',text:'Show a tall diagram'});
 channel.push({type:'message_delta',text:'```mermaid\nflowchart TD\nA-->B\nB-->C\nC-->D\nD-->E\nE-->F\nF-->G\nG-->H\nH-->I\n```\n\nREAL_END'});
 const diagram=page.getByRole('img',{name:'Mermaid diagram'});await expect.poll(()=>diagram.evaluate(img=>img.complete&&img.naturalHeight>0)).toBe(true);
 await expect.poll(gap,{message:'Delayed Mermaid layout must reach the actual chat end'}).toBeLessThan(3);
 await expect(page.locator('.chat-heading')).toContainText('claude-reported-model');await expect(page.locator('.chat-heading')).not.toContainText('claude-default');
 // Deliberately read earlier messages, then stream and resize without snapping back.
 await pane.evaluate(node=>{node.scrollTop=200;node.dispatchEvent(new Event('scroll'));});await expect(page.getByRole('button',{name:'Jump to latest'})).toBeVisible();
 const reading=await pane.evaluate(node=>node.scrollTop);channel.push({type:'message_delta',text:'\n\nMore text while reading earlier.'});await expect(pane).toContainText('More text while reading earlier.');await page.setViewportSize({width:1100,height:750});
 assert.ok(Math.abs((await pane.evaluate(node=>node.scrollTop))-reading)<3,'stream and layout must preserve intentional reading position');
 await page.locator('.workspace-row').filter({hasText:'Beta release'}).locator('button').first().click();await page.locator('.workspace-row').filter({hasText:'Alpha feature'}).locator('button').first().click();await expect.poll(()=>pane.evaluate(node=>node.scrollTop)).toBe(reading);
 await page.getByRole('button',{name:'Jump to latest'}).click();await expect.poll(gap).toBeLessThan(3);
 await pane.evaluate(node=>{node.scrollTop=200;node.dispatchEvent(new Event('scroll'));});
 await page.getByRole('textbox',{name:'Message',exact:true}).fill('Continue from here');await page.getByRole('button',{name:'Send message',exact:true}).click();
 await expect.poll(gap,{message:'Sending should return to the latest turn'}).toBeLessThan(3);
 channel.push({type:'operation_accepted',text:'Continue from here'});channel.push({type:'message_delta',text:'LATEST_SENT_TURN'});await expect(pane).toContainText('LATEST_SENT_TURN');await expect.poll(gap).toBeLessThan(3);
 await page.screenshot({path:'/tmp/lazybox-chat-scroll.png'});
 assert.deepEqual(errors,[]);console.log('PASS: true bottom after late header, resize and Mermaid; manual reading preserved across streaming/navigation; actual model in header. WebKit fixture.');
}finally{await browser?.close();await server.close();await fixture.close();}
