// Peek: Alt-click or Space on a row opens the live item over the page without
// navigating; ← and → step through the rows around it; ⌘↵ opens it here; Space
// or Esc closes it and focus returns to the row last shown. A conversation can
// be answered from the peek; a process shows its output and can be stopped;
// a file in Files reads in place; "Peek" is in the rows' context menus.
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from 'vite';
import react from '@vitejs/plugin-react';
import {webkit,chromium,expect} from './fixtures/playwright.mjs';
import {fileURLToPath} from 'node:url';
import {mkdir} from 'node:fs/promises';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import {monacoVimCompat} from './monaco-vim-compat.mjs';
import { collectPageErrors } from './fixtures/page-errors.mjs';

const fixture=await startRunnerFixture();
const [alphaSession,betaSession]=fixture.sessions;
alphaSession.title='Harden the request handler';betaSession.title='Ship the release';
const alphaEvents=fixture.openEvents(alphaSession.id);
alphaEvents.push({type:'operation_accepted',text:'Make the handler time out after 30 seconds.'});
alphaEvents.push({type:'message_delta',text:'Added a 30 second deadline to the handler and a test for it.'});
// A mission task worked on in Alpha's conversation (step 10).
const mission=fixture.missions.create('Ship the parser split');const task=fixture.missions.addTask(mission,'Split the lexer');fixture.missions.assign(mission,task,alphaSession,false);
const server=await createServer({...testViteCache(import.meta.url),configFile:false,root:fileURLToPath(new URL('../apps/web',import.meta.url)),logLevel:'warn',plugins:[react(),monacoVimCompat()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
await server.listen();
let browser;
try{
 browser=await (process.env.TEST_CHROMIUM?chromium:webkit).launch({headless:true});
 const page=await browser.newPage({viewport:{width:1400,height:900}});page.setDefaultTimeout(15000);
 const errors=[];await collectPageErrors(page, errors);
 await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha#token=${fixture.token}`);
 const nav=page.getByRole('navigation',{name:'Workspaces'});
 const alphaLink=nav.locator('.workspace-row').filter({hasText:'Alpha feature'}).locator('.workspace-link');
 const betaLink=nav.locator('.workspace-row').filter({hasText:'Beta release'}).locator('.workspace-link');
 await expect(alphaLink).toHaveAttribute('aria-current','page');
 await mkdir('.cache',{recursive:true});

 // 1. Alt-click a row: the overlay, not a navigation.
 await betaLink.click({modifiers:['Alt']});
 let peek=page.getByRole('dialog',{name:'Peek: Ship the release'});
 await expect(peek).toBeVisible();
 await expect(alphaLink,'Alt-click peeks instead of navigating').toHaveAttribute('aria-current','page');
 await expect(peek).toContainText('Conversation · 2 of 2');
 await expect(peek.getByRole('textbox',{name:'Message'})).toBeVisible();

 // 2. ← steps to the previous row; the overlay follows the live conversation.
 await page.keyboard.press('ArrowLeft');
 peek=page.getByRole('dialog',{name:'Peek: Harden the request handler'});
 await expect(peek).toContainText('Conversation · 1 of 2');
 await expect(peek).toContainText('Added a 30 second deadline to the handler');
 await expect(peek.getByRole('button',{name:'Previous'})).toBeDisabled();
 // A live update arrives while peeking.
 alphaEvents.push({type:'turn_completed'});alphaEvents.push({type:'operation_accepted',text:'Run the suite.'});alphaEvents.push({type:'message_delta',text:'The full suite passes.'});
 await expect(peek).toContainText('The full suite passes.');
 await page.screenshot({path:'.cache/peek-conversation.png'});

 // 3. Answer from the peek: Enter sends to that conversation, never elsewhere.
 const field=peek.getByRole('textbox',{name:'Message'});
 await field.fill('Also log when the deadline hits.');
 await field.press('Enter');
 await expect(peek.getByRole('status')).toContainText('Sent');
 assert.ok(fixture.calls.some(c=>c.method==='Apply'&&c.body.sessionId===alphaSession.id&&JSON.stringify(c.body).includes('Also log when the deadline hits.')),'the message went to the peeked conversation');
 // Typing in the field never moves the peek: ← stays in the text.
 await field.fill('abc');await field.press('ArrowLeft');
 await expect(page.getByRole('dialog',{name:'Peek: Harden the request handler'})).toBeVisible();
 await field.fill('');

 // 4. Esc closes; focus returns to the row last shown (Alpha's, after ←).
 await page.keyboard.press('Escape');
 await expect(page.locator('.peek-dialog')).toHaveCount(0);
 await expect(alphaLink).toBeFocused();

 // 5. Space on a focused row opens it (Quick Look); Space again closes.
 await betaLink.focus();await page.keyboard.press('Space');
 peek=page.getByRole('dialog',{name:'Peek: Ship the release'});
 await expect(peek).toBeVisible();
 await expect(alphaLink,'Space peeks instead of selecting').toHaveAttribute('aria-current','page');
 await page.keyboard.press('Space');
 await expect(page.locator('.peek-dialog')).toHaveCount(0);
 await expect(betaLink).toBeFocused();

 // 6. ⌘↵ opens the peeked item here, for real.
 await betaLink.press('Space');
 await expect(page.getByRole('dialog',{name:'Peek: Ship the release'})).toBeVisible();
 await page.keyboard.press('Meta+Enter');
 await expect(page.locator('.peek-dialog')).toHaveCount(0);
 await expect(betaLink).toHaveAttribute('aria-current','page');

 // 8. A process: what it is, its output, and Stop behind a confirmation.
 // Processes comes from the Panels menu in the header (the dock's list of every panel).
 await page.locator('summary[aria-label="Panels"]').click();
 await page.getByRole('button',{name:'Show Processes',exact:true}).click();
 const processes=page.getByRole('region',{name:'Hosted processes',exact:true});
 const command=processes.locator('.process-row').filter({hasText:'go test ./...'});
 // 7. "Peek" in a row's context menu (the workspace row's menu stays its ⋯ actions).
 await processes.locator('.process-row').filter({hasText:'Fix the parser'}).click({button:'right'});
 await page.getByRole('menuitem',{name:/^Peek/}).click();
 await expect(page.getByRole('dialog',{name:'Peek: Fix the parser'})).toBeVisible();
 await page.getByRole('button',{name:'Close peek'}).click();
 await expect(page.locator('.peek-dialog')).toHaveCount(0);
 await command.focus();await page.keyboard.press('Space');
 peek=page.getByRole('dialog',{name:'Peek: go test ./...'});
 await expect(peek).toBeVisible();
 await expect(peek).toContainText(/Process · \d of 3/);
 await expect(peek.locator('.peek-facts')).toContainText('97.5%');
 await expect(peek.getByTestId('console-output')).toBeVisible();
 // → moves to the next process in the panel's order.
 await page.keyboard.press('ArrowRight');
 await expect(page.locator('.peek-dialog h2')).not.toHaveText('go test ./...');
 await page.keyboard.press('ArrowLeft');
 peek=page.getByRole('dialog',{name:'Peek: go test ./...'});
 await peek.getByRole('button',{name:'Stop…'}).click();
 await expect(peek.getByRole('group',{name:'Confirm stop'})).toContainText('The command and everything it started are stopped');
 await peek.getByRole('button',{name:'Stop',exact:true}).click();
 await expect(peek).toContainText('This process is no longer running.');
 assert.ok(fixture.mutations.some(m=>m.method==='StopProcess'&&m.body.id==='host-command-beta'),'Stop stopped that process');
 await page.screenshot({path:'.cache/peek-process.png'});
 await page.keyboard.press('Escape');
 await expect(command).toBeFocused();

 // 9. A file in Files reads in place, read-only.
 await page.getByRole('button',{name:'Browse files',exact:true}).click();
 const tree=page.getByRole('tree',{name:'Workspace files'});
 await tree.getByRole('treeitem',{name:/api/}).first().click();
 const readme=tree.getByRole('treeitem',{name:'README.md'}).first();
 await readme.focus();await page.keyboard.press('Space');
 peek=page.getByRole('dialog',{name:/^Peek: .*README\.md$/});
 await expect(peek).toContainText('File preview');
 await expect(peek.locator('textarea, [contenteditable=true]')).toHaveCount(0);
 await page.keyboard.press('Escape');

 // 10. A mission task whose conversation is already known when its peek opens: ⌘↵ opens that
 // conversation (in Alpha), not the Missions page; what the peek learned on mount is kept.
 await expect(betaLink).toHaveAttribute('aria-current','page');
 await page.getByRole('navigation',{name:'Pages'}).getByRole('button',{name:'Missions',exact:true}).click();
 const taskCard=page.locator(`.mission-card[data-task="${task.id}"]`);
 await taskCard.focus();await page.keyboard.press('Space');
 await expect(page.getByRole('dialog',{name:'Peek: Split the lexer'})).toBeVisible();
 await page.keyboard.press('Meta+Enter');
 await expect(page.locator('.peek-dialog')).toHaveCount(0);
 await expect(alphaLink,'Open here went to the task’s conversation').toHaveAttribute('aria-current','page');

 // The rows' git line answered meanwhile (peek and the sidebar's poll share the window).
 await expect(nav.locator('.workspace-git[data-git-line]').first()).toBeVisible();
 assert.deepEqual(errors,[]);
 console.log('peek: Alt-click, Space, arrows, send, open here, context menu, process stop, file — ok');
}finally{await browser?.close();await server.close();await fixture.close();}
