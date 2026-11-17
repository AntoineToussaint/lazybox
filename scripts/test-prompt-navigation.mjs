// Jump to any of your prompts without scrolling: the rail, the searchable
// list and Alt+↑/↓, including prompts in history that is not mounted yet.
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from 'vite';
import react from '@vitejs/plugin-react';
import {webkit,expect} from './fixtures/playwright.mjs';
import {fileURLToPath} from 'node:url';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import {longConversation} from './fixtures/long-conversation.mjs';
import {monacoVimCompat} from './monaco-vim-compat.mjs';

const turns=60;
const fixture=await startRunnerFixture();
const session=fixture.sessions[0];Object.assign(session,{closed:true,status:'stopped',busy:false,providerReference:'saved-claude-conversation',agents:[{id:'root',status:'done'}],provider:'claude'});
const channel=fixture.openEvents(session.id);
for(const event of longConversation({turns,toolOutputBytes:600}))channel.push(event);channel.complete();
const root=fileURLToPath(new URL('../apps/web',import.meta.url));
const server=await createServer({...testViteCache(import.meta.url),configFile:false,root,logLevel:'warn',plugins:[react(),monacoVimCompat()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
await server.listen();
let browser;
try{
 browser=await webkit.launch({headless:true});
 const page=await browser.newPage({viewport:{width:1300,height:850}});page.setDefaultTimeout(30000);
 const errors=[];page.on('pageerror',e=>errors.push(String(e)));
 await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=${session.id}#token=${fixture.token}`);
 const pane=page.locator('.messages:visible');
 await expect(pane).toContainText(`ANSWER_END_${turns-1}`,{timeout:60000});
 // The prompt is shown at the top of the conversation view.
 const atTop=async question=>expect.poll(()=>page.evaluate(q=>{
  const view=document.querySelector('.messages');const node=[...document.querySelectorAll('.messages .message.operation_accepted')].find(n=>n.textContent.includes(q));
  if(!view||!node)return 'missing';const offset=node.getBoundingClientRect().top-view.getBoundingClientRect().top;return offset>=0&&offset<120?'top':`offset ${Math.round(offset)}`;
 },question)).toBe('top');

 const rail=page.getByRole('navigation',{name:'Your prompts'});
 await expect(rail.getByRole('button')).toHaveCount(turns);
 // A prompt far back in history, not mounted when the conversation opened.
 assert.equal(await pane.locator('.message.operation_accepted').filter({hasText:'Question 3:'}).count(),0,'early prompts start unmounted');
 // The chip is one of the chat header's tools: it covers no part of the transcript, and says what it counts.
 {const chip=page.getByRole('button',{name:'Your prompts',exact:true});await expect(page.locator('.chat-heading').getByRole('button',{name:'Your prompts',exact:true})).toHaveText(/^\d+ prompts$/);
  const c=await chip.boundingBox(),m=await page.locator('.messages:visible').first().boundingBox();if(c.y+c.height>m.y+1&&c.y<m.y+m.height)throw new Error('the prompts chip overlaps the transcript');}
 await page.getByRole('button',{name:'Your prompts',exact:true}).click();
 const list=page.getByRole('dialog',{name:'Jump to a prompt'});
 await expect(list.getByRole('searchbox',{name:'Find a prompt'})).toBeFocused();
 await list.getByRole('searchbox').fill('Question 3:');
 await page.keyboard.press('Enter');
 await expect(list).toHaveCount(0);
 await atTop('Question 3:');
 // Alt+↓ / Alt+↑ step through prompts from anywhere in the chat, an empty composer included.
 await pane.click({position:{x:40,y:200}});
 await page.keyboard.press('Alt+ArrowDown');await atTop('Question 4:');
 await page.keyboard.press('Alt+ArrowDown');await atTop('Question 5:');
 const composer=page.getByRole('textbox',{name:'Message',exact:true});
 await composer.focus();await page.keyboard.press('Alt+ArrowUp');await atTop('Question 4:');
 // With a draft, Alt+arrows belong to the text, not to navigation.
 await composer.fill('draft in progress');const before=await pane.evaluate(n=>n.scrollTop);await page.keyboard.press('Alt+ArrowUp');await page.waitForTimeout(300);assert.equal(await pane.evaluate(n=>n.scrollTop),before,'Alt+↑ over a draft must not navigate');await expect(composer).toHaveValue('draft in progress');
 // The rail jumps straight to a prompt.
 await rail.getByRole('button',{name:/^Prompt 20:/}).click();await atTop('Question 19:');
 assert.deepEqual(errors,[]);
 console.log(`PASS: prompt rail (${turns} markers), searchable prompt list with Enter, Alt+↑/↓ from the chat and an empty composer (not over a draft), jumping into unmounted history.`);
}finally{await browser?.close();await server.close();await fixture.close();}
