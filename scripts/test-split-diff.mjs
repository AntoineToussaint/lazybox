import {openWorkspaceChanges} from './fixtures/workspace-changes.mjs';
// The Changes diff side by side, against the fake Runner: the Unified/Split
// toggle (remembered, and unified when the pane is too narrow), the changed
// words marked, a long unchanged run folded and the lines between hunks shown
// from the file, n/p between hunks and Shift+N/P between files, review
// comments on both sides of the split view (the same threads in unified), and
// Viewed files that stay viewed across a reload until their diff changes.
import {createServer} from 'vite';
import react from '@vitejs/plugin-react';
import {chromium,expect} from './fixtures/playwright.mjs';
import {fileURLToPath} from 'node:url';
import {mkdir} from 'node:fs/promises';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import {testViteCache} from './fixtures/vite-cache.mjs';

const fixture=await startRunnerFixture();
const api=fixture.shipState('alpha','api');
api.files=[{path:'src/handler.go',indexStatus:' ',worktreeStatus:'M'},{path:'README.md',indexStatus:' ',worktreeStatus:'M'}];
// 35 lines; the change is on line 3 and line 34. The first hunk carries more
// context than git's default, so its long unchanged tail folds.
const NEW=['package api','','func Handle() error {',...Array.from({length:30},(_,i)=>`\tstep${i+1}()`),'\treturn nil','}'];
const OLD=NEW.map((line,i)=>i===2?'func Handle() {':i===33?'\treturn':line);
const context=(from,to)=>NEW.slice(from-1,to).map(line=>' '+line);
const DIFF=['diff --git a/src/handler.go b/src/handler.go','--- a/src/handler.go','+++ b/src/handler.go',
 '@@ -1,16 +1,16 @@',...context(1,2),'-'+OLD[2],'+'+NEW[2],...context(4,16),
 '@@ -31,5 +31,5 @@ func Handle() error {',...context(31,33),'-'+OLD[33],'+'+NEW[33],...context(35,35)].join('\n')+'\n';
fixture.diffs.set('alpha/api/src/handler.go',DIFF);
fixture.workspaceFiles.set('alpha/api/src/handler.go',{path:'api/src/handler.go',text:NEW.join('\n')+'\n',version:'v1',binary:false,truncated:false});

const server=await createServer({...testViteCache(import.meta.url),configFile:false,root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
let browser;
try{
 await server.listen();browser=await chromium.launch({headless:true});
 const url=`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace#token=${fixture.token}`;
 const context=await browser.newContext({viewport:{width:1600,height:1000}});
 const page=await context.newPage();page.setDefaultTimeout(10000);
 const errors=[];page.on('pageerror',e=>errors.push(String(e)));
 // React's warnings (duplicate keys, bad markup) are failures here too.
 page.on('console',message=>{if(message.type()==='error'&&/key|React|Warning/.test(message.text()))errors.push(message.text());});
 await page.goto(url);
 await page.getByRole('combobox',{name:'Workspace',exact:true}).selectOption('alpha');await page.getByRole('combobox',{name:'Session',exact:true}).selectOption('alpha-session');
 await openWorkspaceChanges(page);
 const changes=page.getByRole('region',{name:'Workspace changes'});
 const diff=changes.getByRole('grid',{name:'File diff'});
 const files=changes.getByRole('navigation',{name:'Changed files'});
 await files.getByRole('button',{name:/src\/handler\.go/}).click();
 await expect(diff).toContainText('func Handle() error {');

 // 1. Unified by default; the words that changed are marked, nothing else.
 await expect(diff).toHaveClass(/is-unified/);
 const layout=changes.getByRole('group',{name:'Diff layout'});
 await expect(layout.getByRole('button',{name:'Unified'})).toHaveAttribute('aria-pressed','true');
 // Syntax highlighting splits a mark at token boundaries: compare their text together.
 await expect.poll(()=>diff.locator('mark.diff-word').allTextContents().then(t=>t.join(''))).toBe('error  nil');

 // 2. A long unchanged run is folded; a click shows it.
 const fold=diff.getByRole('button',{name:'Show 10 unchanged lines, lines 7–16'});
 await expect(fold).toBeVisible();
 await expect(diff).toContainText('step3()');await expect(diff).not.toContainText('step10()');

 // 3. n and p move between hunks, from the keyboard alone.
 await files.getByRole('button',{name:/src\/handler\.go/}).focus();
 await page.keyboard.press('n');await expect(diff.locator('[data-diff-hunk="1"]')).toBeFocused();
 await page.keyboard.press('n');await expect(diff.locator('[data-diff-hunk="2"]')).toBeFocused();
 await page.keyboard.press('p');await expect(diff.locator('[data-diff-hunk="1"]')).toBeFocused();
 await fold.click();await expect(diff).toContainText('step10()');await expect(fold).toHaveCount(0);

 // 4. The 14 lines between the hunks come from the file; the hunks join.
 await expect(diff).not.toContainText('step20()');
 await diff.getByRole('button',{name:'Show 14 unchanged lines between changes'}).click();
 await expect(diff).toContainText('step20()');
 await expect(diff.locator('[data-diff-hunk]')).toHaveCount(1);
 // The file ends where the diff does: nothing more to show below.
 await expect(diff.getByRole('button',{name:'Show the rest of the file'})).toHaveCount(0);

 // 5. Side by side: the old line on the left, the new one on the right.
 await layout.getByRole('button',{name:'Split'}).click();
 await expect(diff).toHaveClass(/is-split/);
 const signature=diff.locator('.review-split-row').filter({hasText:'func Handle() error {'});
 await expect(signature.locator('.review-split-side').first()).toContainText('func Handle() {');
 await expect(signature.locator('.review-split-side').nth(1)).toContainText('func Handle() error {');
 await expect.poll(()=>diff.locator('mark.diff-word').allTextContents().then(t=>t.join(''))).toBe('error  nil');
 await mkdir('.cache',{recursive:true});await page.screenshot({path:'.cache/split-diff.png'});

 // 6. Comments in split: a new-side line from the right, a removed line from the left.
 await signature.hover();
 await diff.getByRole('button',{name:'Comment on line 3',exact:true}).click();
 let composer=changes.getByRole('form',{name:'Comment on line 3'});
 await composer.getByRole('textbox',{name:'Comment'}).fill('Return a typed error.');
 await composer.getByRole('textbox',{name:'Comment'}).press('Meta+Enter');await expect(composer).toHaveCount(0);
 const adds=()=>fixture.mutations.filter(m=>m.method==='AddReviewComment').map(m=>m.body);
 assert.deepEqual((({side,startLine,snippet,view})=>({side,startLine,snippet,view}))(adds()[0]),{side:'new',startLine:3,snippet:['func Handle() error {'],view:'uncommitted'});
 await diff.getByRole('button',{name:'Comment on removed line 34'}).focus();await page.keyboard.press('Enter');
 composer=changes.getByRole('form',{name:'Comment on removed line 34'});
 await composer.getByRole('textbox',{name:'Comment'}).fill('A bare return hid the error.');
 await composer.getByRole('button',{name:'Add comment'}).click();await expect(composer).toHaveCount(0);
 assert.deepEqual((({side,startLine,snippet})=>({side,startLine,snippet}))(adds()[1]),{side:'old',startLine:34,snippet:['\treturn']});
 // A context line is commented on from the right only.
 const contextRow=diff.locator('.review-split-row').filter({hasText:'step2()'}).first();
 await expect(contextRow.getByRole('button',{name:/^Comment on/})).toHaveCount(1);
 await expect(changes.getByRole('article',{name:'Comment on line 3'})).toBeVisible();
 await expect(changes.getByRole('article',{name:'Comment on removed line 34'})).toBeVisible();
 // A drag over two right-hand lines selects a range, as in unified.
 await diff.locator('.review-split-row').filter({hasText:'step5()'}).first().hover();
 const from=diff.getByRole('button',{name:'Comment on line 8',exact:true});
 const to=diff.locator('.review-split-row').filter({hasText:'step6()'}).first();
 const box=await from.boundingBox();await page.mouse.move(box.x+5,box.y+5);await page.mouse.down();
 const toBox=await to.locator('.review-split-side').nth(1).boundingBox();await page.mouse.move(toBox.x+100,toBox.y+toBox.height/2,{steps:4});await page.mouse.up();
 composer=changes.getByRole('form',{name:'Comment on lines 8–9'});await expect(composer).toBeVisible();
 await composer.getByRole('button',{name:'Cancel'}).click();

 // 7. The same threads in unified.
 await layout.getByRole('button',{name:'Unified'}).click();
 await expect(diff).toHaveClass(/is-unified/);
 await expect(changes.getByRole('article',{name:'Comment on line 3'})).toBeVisible();
 await expect(changes.getByRole('article',{name:'Comment on removed line 34'})).toBeVisible();
 await layout.getByRole('button',{name:'Split'}).click();

 // 8. Viewed: v marks the file and moves to the next one still to review.
 await expect(changes.getByRole('status').filter({hasText:'viewed'})).toHaveText('0 of 2 viewed');
 await files.getByRole('button',{name:/src\/handler\.go/}).focus();
 await page.keyboard.press('v');
 await expect(files.getByRole('checkbox',{name:'Viewed src/handler.go'})).toBeChecked();
 await expect(files.getByRole('button',{name:/README\.md/})).toHaveClass(/selected/);
 await expect(changes.getByRole('status').filter({hasText:'viewed'})).toHaveText('1 of 2 viewed');
 // Shift+P and Shift+N move between files.
 await page.keyboard.press('Shift+P');await expect(files.getByRole('button',{name:/src\/handler\.go/})).toHaveClass(/selected/);
 await page.keyboard.press('Shift+N');await expect(files.getByRole('button',{name:/README\.md/})).toHaveClass(/selected/);
 await files.getByRole('checkbox',{name:'Viewed README.md'}).check();
 await expect(changes.getByRole('status').filter({hasText:'viewed'})).toHaveText('2 of 2 viewed');

 // 9. A reload keeps the layout and what was viewed.
 await page.reload();
 if(!await changes.isVisible())await openWorkspaceChanges(page);
 await expect(files.getByRole('checkbox',{name:'Viewed src/handler.go'})).toBeChecked();
 await files.getByRole('button',{name:/src\/handler\.go/}).click();
 await expect(diff).toHaveClass(/is-split/);
 await expect(changes.getByRole('status').filter({hasText:'viewed'})).toHaveText('2 of 2 viewed');

 // 10. The agent changes the file again: it is no longer viewed once its new diff is read.
 fixture.diffs.set('alpha/api/src/handler.go',DIFF.replace('+\treturn nil','+\treturn errors.New("x")'));
 await changes.getByRole('button',{name:'Refresh changes'}).click();
 await expect(files.getByRole('checkbox',{name:'Viewed src/handler.go'})).not.toBeChecked();
 await expect(changes.getByRole('status').filter({hasText:'viewed'})).toHaveText('1 of 2 viewed');

 // 11. A pane too narrow for two columns shows unified, and says why.
 await page.setViewportSize({width:1000,height:900});
 await expect(changes.getByRole('status').filter({hasText:'Unified while the pane is narrow'})).toBeVisible();
 await expect(diff).toHaveClass(/is-unified/);
 await expect(layout.getByRole('button',{name:'Split'})).toHaveAttribute('aria-pressed','true');
 await page.setViewportSize({width:1600,height:1000});
 await expect(diff).toHaveClass(/is-split/);

 assert.deepEqual(errors,[]);
 console.log('PASS: unified/split toggle remembered and narrow fallback, word marks, folded and expanded unchanged lines, n/p hunks, Shift+N/P files, comments on both sides of the split view, viewed files kept until their diff changes.');
}finally{await browser?.close();await server.close();await fixture.close();}
