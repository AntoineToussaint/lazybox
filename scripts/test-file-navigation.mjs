// Files panel, Editor panel (tabs, splits, persistence), Go to file, and how
// file links an agent writes resolve. WebKit against the fake Runner fixture.
import {testViteCache} from './fixtures/vite-cache.mjs';
import {createServer} from 'vite';
import react from '@vitejs/plugin-react';
import {webkit,expect} from './fixtures/playwright.mjs';
import {fileURLToPath} from 'node:url';
import {mkdir} from 'node:fs/promises';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import {monacoVimCompat} from './monaco-vim-compat.mjs';

const fixture=await startRunnerFixture();
const channel=fixture.openEvents('alpha-session');
const server=await createServer({...testViteCache(import.meta.url),configFile:false,root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react(),monacoVimCompat()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
let browser;
try{
 await server.listen();await mkdir('.cache',{recursive:true});
 browser=await webkit.launch({headless:true});const page=await browser.newPage({viewport:{width:1600,height:950}});page.setDefaultTimeout(20000);
 const errors=[];page.on('pageerror',error=>errors.push(String(error)));
 const url=`http://127.0.0.1:${server.httpServer.address().port}/?view=workspace&collection=alpha&session=alpha-session#token=${fixture.token}`;
 await page.goto(url);
 await expect(page.locator('.workspace-agent-label').first()).toBeVisible();
 // Nothing ever says "Loading editor…".
 const noLoadingText=async()=>assert.equal(await page.getByText(/Loading editor/).count(),0,'no "Loading editor" text');

 // Files: the toolbar opens the tree with both repositories open, focused.
 await page.getByRole('button',{name:'Browse files',exact:true}).click();
 const tree=page.getByRole('tree',{name:'Workspace files'});
 await expect(tree.locator('[data-path="api"]')).toHaveAttribute('aria-expanded','true');
 await expect(tree.locator('[data-path="web"]')).toHaveAttribute('aria-expanded','true');
 await expect(tree.locator('[data-path="api"]')).toBeFocused();
 await expect(tree.locator('[data-path="api"] .is-repository')).toHaveCount(1);
 // Keyboard: down into api, open src with →, step in, Enter opens and focuses the editor.
 await page.keyboard.press('ArrowDown');await expect(tree.locator('[data-path="api/scripts"]')).toBeFocused();
 await page.keyboard.press('ArrowDown');await expect(tree.locator('[data-path="api/src"]')).toBeFocused();
 await page.keyboard.press('ArrowRight');await expect(tree.locator('[data-path="api/src"]')).toHaveAttribute('aria-expanded','true');
 await page.keyboard.press('ArrowRight');await expect(tree.locator('[data-path="api/src/components"]')).toBeFocused();
 await page.keyboard.press('ArrowDown');await expect(tree.locator('[data-path="api/src/main.go"]')).toBeFocused();
 await page.keyboard.press('ArrowLeft');await expect(tree.locator('[data-path="api/src"]')).toBeFocused();
 await page.keyboard.press('ArrowRight');await page.keyboard.press('ArrowDown');await page.keyboard.press('Enter');
 const editorPanel=page.locator('section.workspace-panel.panel-editor');
 await expect(editorPanel).toBeVisible();
 const tabs=group=>editorPanel.locator('.editor-group').nth(group).getByRole('tab');
 await expect(tabs(0)).toHaveCount(1);await expect(tabs(0).first()).toContainText('main.go');
 const source=editorPanel.getByRole('textbox',{name:'File source editor'}).first();
 await expect(source).toBeFocused();await noLoadingText();
 // Files sits beside the editor, the pair to the right of the conversation.
 await expect.poll(async()=>{const files=await page.locator('section.workspace-panel.panel-files').boundingBox(),editor=await editorPanel.boundingBox(),chat=await page.locator('section.workspace-panel.panel-chat').boundingBox();return chat.x<files.x&&files.x<editor.x&&editor.width>500;},{message:'conversation | files | editor, the editor wide enough to read code'}).toBe(true);

 // A single click opens a preview tab that the next single click replaces; a double click keeps it.
 await tree.locator('[data-path="api/src/routes.ts"]').click();
 await expect(tabs(0)).toHaveCount(2);await expect(tabs(0).nth(1)).toHaveClass(/is-preview/);
 await tree.locator('[data-path="api/package.json"]').click();
 await expect(tabs(0)).toHaveCount(2);await expect(tabs(0).nth(1)).toContainText('package.json');
 await tabs(0).nth(1).dblclick();await expect(tabs(0).nth(1)).not.toHaveClass(/is-preview/);
 // Same-named files are told apart in their tabs.
 await tree.locator('[data-path="api/README.md"]').dblclick();await tree.locator('[data-path="web/README.md"]').dblclick();
 await expect(tabs(0).filter({hasText:'README.md'})).toHaveCount(2);await expect(tabs(0).filter({hasText:'README.md'}).first().locator('.editor-tab-hint')).toHaveText('api');
 // Drag a tab to the front (both in view, so the strip does not scroll mid-drag).
 await tabs(0).nth(0).click();
 await tabs(0).nth(1).dragTo(tabs(0).nth(0),{targetPosition:{x:4,y:10}});
 // A tab is told by its path (data-tab); its title is set aside while the pointer
 // rests on it, as the window's tooltip shows it (tooltips.ts).
 await expect(tabs(0).nth(0)).toHaveAttribute('data-tab','api/package.json');

 // Edit: the tab and the tree mark unsaved changes; Cmd/Ctrl+S saves through the Runner.
 await tabs(0).filter({hasText:'main.go'}).click();
 await source.focus();await page.keyboard.press('Meta+a');await page.keyboard.insertText('package main // edited\n');
 await expect(tabs(0).filter({hasText:'main.go'})).toHaveClass(/is-dirty/);await expect(tree.locator('[data-path="api/src/main.go"] .tree-dirty')).toHaveCount(1);
 await page.keyboard.press('Meta+s');
 await expect(tabs(0).filter({hasText:'main.go'})).not.toHaveClass(/is-dirty/);
 await expect.poll(()=>fixture.workspaceFiles.get('alpha/api/src/main.go').text).toBe('package main // edited\n');
 // Closing an unsaved file asks; Cancel keeps it, Discard closes it.
 await source.focus();await page.keyboard.insertText('// draft ');
 await tabs(0).filter({hasText:'main.go'}).getByRole('button',{name:'Close main.go'}).click();
 const prompt=page.getByRole('alertdialog',{name:'Unsaved changes'});await expect(prompt).toContainText('main.go');
 await prompt.getByRole('button',{name:'Cancel'}).click();await expect(prompt).toHaveCount(0);await expect(tabs(0).filter({hasText:'main.go'})).toHaveCount(1);
 await source.focus();await page.keyboard.press('Alt+w');await prompt.getByRole('button',{name:'Discard changes'}).click();
 await expect(tabs(0).filter({hasText:'main.go'})).toHaveCount(0);

 // Split right with the button, down with the keyboard; each group keeps its own tabs.
 const before=await tabs(0).allTextContents();
 await editorPanel.getByRole('button',{name:'Split editor right'}).first().click();
 await expect(editorPanel.locator('.editor-group')).toHaveCount(2);await expect(editorPanel.locator('.editor-split.split-row')).toHaveCount(1);
 await expect(tabs(1)).toHaveCount(1);
 await tree.locator('[data-path="api/src/routes.ts"]').dblclick();
 await expect(tabs(1)).toHaveCount(2);await expect(tabs(1).nth(1)).toContainText('routes.ts');
 await expect(tabs(0)).toHaveText(before);
 // The editor's keys are its own only while it has the focus: from the Files tree, ⌘\ and ⌘⌥\ do not split it.
 await tree.locator('[data-path="api/src/routes.ts"]').focus();
 await page.keyboard.press('Meta+Alt+Backslash');await page.keyboard.press('Meta+Backslash');
 await expect(editorPanel.locator('.editor-group')).toHaveCount(2);
 await page.keyboard.press('Escape');
 await tabs(1).nth(1).click();
 await page.keyboard.press('Meta+Alt+Backslash');
 await expect(editorPanel.locator('.editor-group')).toHaveCount(3);await expect(editorPanel.locator('.editor-split.split-column')).toHaveCount(1);
 // Reorder within a group from the keyboard.
 await editorPanel.locator('.editor-group').nth(1).getByRole('tab').first().click();
 await page.keyboard.press('Meta+Alt+Shift+ArrowRight');
 await expect(tabs(1).nth(1)).toHaveClass(/is-active/);
 const order=await tabs(1).allTextContents();
 await page.keyboard.press('Meta+Alt+ArrowLeft');await expect(tabs(1).nth(0)).toHaveClass(/is-active/);
 await page.screenshot({path:'.cache/file-navigation-split.png'});

 // The arrangement survives a reload.
 await page.reload();
 await expect(page.locator('section.workspace-panel.panel-editor .editor-group')).toHaveCount(3);
 await expect(tabs(1)).toHaveText(order);
 await expect(page.locator('section.workspace-panel.panel-files')).toBeVisible();await noLoadingText();
 // Close the extra groups from their menus.
 for(const n of [2,1]){await editorPanel.getByLabel(`Editor group ${n+1} actions`,{exact:true}).click();await page.getByRole('button',{name:'Close group'}).click();}
 await expect(editorPanel.locator('.editor-group')).toHaveCount(1);

 // Go to file: Cmd/Ctrl+P, fuzzy, recent files with no query, name:line.
 await page.locator('[data-agent-composer]:visible').first().focus();
 await page.keyboard.press('Meta+p');
 const finder=page.getByRole('dialog',{name:'Go to file'});await expect(finder).toBeVisible();
 await expect(finder).toContainText('Recently opened');
 const finderBounds = () => Promise.all([finder, finder.locator('footer')].map(locator => locator.boundingBox()));
 const recentBounds = await finderBounds();
 const fileSearch = finder.getByRole('combobox', {name:'File name'});
 await fileSearch.fill('zzqxyunmatched');
 await expect(finder.getByText('No matching files.')).toBeVisible();
 assert.deepEqual(await finderBounds(), recentBounds, 'Go to file keeps its bounds and footer through async search');
 await fileSearch.fill('rts');
 await expect(finder.getByRole('option').first()).toContainText('routes.ts');
 await expect(finder.getByRole('option').first().locator('mark')).not.toHaveCount(0);
 await page.keyboard.press('ArrowDown');await page.keyboard.press('Enter');
 await expect(finder).toHaveCount(0);
 await expect(tabs(0).filter({hasText:'routes.ts'})).toHaveAttribute('data-tab','web/src/routes.ts');
 assert.ok(fixture.calls.some(call=>call.method==='SearchWorkspaceFiles'&&call.body.query==='rts'));

 // Links an agent writes resolve on the Runner: a bare name opens its only match...
 channel.push({type:'message_delta',text:'See [routes list](undeclared-routes.json), [the config](config.toml) and [gone](missing.json).'});
 await page.getByRole('button',{name:'routes list',exact:true}).click();
 await expect(tabs(0).filter({hasText:'undeclared-routes.json'})).toHaveCount(1);
 await expect(tree.locator('[data-path="api/scripts/undeclared-routes.json"]')).toHaveAttribute('aria-selected','true');
 // ...several matches are a choice, never an error...
 await page.getByRole('button',{name:'the config',exact:true}).click();
 const chooser=page.getByRole('dialog',{name:/Which “config.toml”/});await expect(chooser).toBeVisible();
 await expect(chooser.getByRole('listitem')).toHaveCount(2);await expect(chooser.getByRole('listitem').first()).toBeFocused();
 await page.keyboard.press('ArrowDown');await page.keyboard.press('Enter');
 await expect(chooser).toHaveCount(0);
 await expect(tabs(0).filter({hasText:'config.toml'})).toHaveCount(1);await expect(tabs(0).filter({hasText:'config.toml'})).toHaveAttribute('data-tab','web/config.toml');
 // ...and only a reference to nothing is reported.
 await page.getByRole('button',{name:'gone',exact:true}).click();
 await expect(page.getByRole('alert').filter({hasText:'Nothing named “missing.json”'})).toBeVisible();
 // Alt-click is a quick look that can move into the editor.
 await page.getByRole('button',{name:'routes list',exact:true}).click({modifiers:['Alt']});
 const look=page.getByRole('dialog',{name:'File preview'});await expect(look).toBeVisible();
 await look.getByRole('button',{name:'Close file preview'}).click();await expect(look).toHaveCount(0);

 // Reveal the active file from the Files panel.
 await tree.locator('[data-path="api"]').click();await expect(tree.locator('[data-path="api"]')).toHaveAttribute('aria-expanded','false');
 await page.getByRole('button',{name:'Reveal active file'}).click();
 await expect(tree.locator('[data-path="web/config.toml"]')).toHaveAttribute('aria-selected','true');
 await page.screenshot({path:'.cache/file-navigation.png'});
 await noLoadingText();
 assert.deepEqual(errors,[],'page errors: '+errors.join(' | '));
 console.log('PASS: Files tree with repositories and keyboard navigation, preview/pinned tabs, drag reorder, dirty markers and save, unsaved-close guard, split right/down with per-group tabs, keyboard reorder, persisted layout, Cmd/Ctrl+P fuzzy finder, link resolution (unique, chooser, missing), quick look, reveal. WebKit fixtures only.');
}finally{await browser?.close();await server.close();await fixture.close();}
