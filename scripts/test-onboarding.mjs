// Onboarding against the fake Runner: the first-run welcome on a fresh
// install (shown once, its buttons work, reopened from Help), no welcome for an
// existing user, and What's new after an update (shown once per release,
// never stealing focus, reopened from Help).
import {createServer} from './fixtures/vite-cache.mjs';
import react from '@vitejs/plugin-react';
import {chromium,expect} from './fixtures/playwright.mjs';
import {fileURLToPath} from 'node:url';
import {createHash} from 'node:crypto';
import {mkdir} from 'node:fs/promises';
import assert from 'node:assert/strict';
import {startRunnerFixture} from './fixtures/runner-ui.mjs';
import {testViteCache} from './fixtures/vite-cache.mjs';
import {releaseNotes} from '../packages/surfaces/src/release-notes.ts';
import { collectPageErrors, drainPageErrors } from './fixtures/page-errors.mjs';

const fixture=await startRunnerFixture();
const server=await createServer({...testViteCache(import.meta.url),configFile:false,root:fileURLToPath(new URL('../apps/web',import.meta.url)),plugins:[react()],server:{host:'127.0.0.1',port:0,proxy:{'/lazybox.bench.v1.RunnerService/':{target:fixture.url}}}});
let browser;
try{
 await server.listen();browser=await chromium.launch({headless:true});
 const origin=`http://127.0.0.1:${server.httpServer.address().port}`;
 const url=`${origin}/?view=workspace#token=${fixture.token}`;
 // The browser host keeps a workspace window's preferences in localStorage, under the Runner's scope.
 const scope=createHash('sha256').update(origin+'\0'+fixture.token).digest('hex');
 const welcomeKey=`${scope}:onboarding.welcome`,whatsNewKey=`${scope}:onboarding.whats-new`;
 const stored=(page,key)=>page.evaluate(key=>localStorage.getItem(key),key);
 const open=async context=>{const page=await context.newPage();page.setDefaultTimeout(10000);const errors=[];await collectPageErrors(page, errors);await page.goto(url);return {page,errors};};
 const help=async(page,item)=>{await page.locator('summary[aria-label="Help"]').click();await page.getByRole('button',{name:item,exact:true}).click();};
 await mkdir('.cache',{recursive:true});

 // (a) A fresh install: no workspace, no conversation.
 const savedCollections=fixture.collections.splice(0),savedSessions=fixture.sessions.splice(0);
 {
  const context=await browser.newContext({viewport:{width:1400,height:900}});
  const {page,errors}=await open(context);
  const welcome=page.getByRole('dialog',{name:'Welcome to Lazybox'});
  await expect(welcome).toBeVisible();
  await expect(welcome.getByRole('button',{name:'Get started'})).toBeFocused();
  await expect(welcome.getByRole('status').filter({hasText:'Runner connected and ready'})).toBeVisible();
  await expect(welcome.getByRole('radio',{name:'Standard'})).toBeChecked();
  await welcome.getByRole('radio',{name:'Focused'}).check();
  await expect(welcome.getByLabel('Focused preview')).toContainText('Tests passed');
  await expect(page.locator('main.workspace-shell')).toHaveAttribute('data-workspace-details','false');
  await welcome.getByLabel('Agent personality').selectOption('hands-on-v2');
  await expect(welcome.getByLabel('Personality preview')).toContainText('make a reversible fix');
  // The five keys, from the default (vim) keymap, and Go to file for this platform.
  const list=welcome.locator('dl[aria-label="The five keys that matter"]');
  await expect(list.locator('dt')).toHaveCount(5);
  await expect(list).toContainText('Leave the agent');await expect(list).toContainText('New task');await expect(list).toContainText('Go to file');await expect(list).toContainText('Inbox');await expect(list).toContainText('Next / previous');
  assert.match(await list.locator('div').nth(1).locator('kbd').textContent(),/^(⌘N|Ctrl\+N)$/);
  assert.deepEqual(await list.locator('div').nth(4).locator('kbd').allTextContents(),['j','k']);
  assert.match((await list.locator('div').nth(2).locator('kbd').textContent()),/^(⌘P|Ctrl\+P)$/);
  // The documentation ships with the app: a button, never a web link.
  await expect(welcome.getByRole('button',{name:/Read the documentation/})).toBeVisible();
  await expect(welcome.getByRole('link',{name:/Read the documentation/})).toHaveCount(0);
  await page.screenshot({path:'.cache/onboarding-welcome.png'});
  // What's new is never shown on a first run: the newest release is recorded silently.
  await expect(page.getByRole('complementary',{name:"What's new"})).toHaveCount(0);
  assert.equal(await stored(page,whatsNewKey),releaseNotes[0].id);
  // Claude: the API key is the recommendation (CLAUDE_COMPLIANCE.md), with the
  // person's own sign-in said as unconfirmed; Codex signs in with its plan.
  await expect(welcome).toContainText('Claude: add an Anthropic API key. It is the way Anthropic documents for apps built on its Agent SDK');
  await expect(welcome).toContainText('Anthropic has not confirmed it for such apps');
  await expect(welcome.getByRole('button',{name:'Sign in to Codex'})).toBeVisible();
  await expect(welcome).not.toContainText('Lazybox switches when one reaches its limit');
  await welcome.getByRole('button',{name:'Add an API key'}).click();
  await expect(welcome).toHaveCount(0);
  await expect(page.getByRole('tab',{name:'Agents & workspaces'})).toHaveAttribute('aria-selected','true');
  await expect(page.getByRole('textbox',{name:'Use an Anthropic API key'})).toBeVisible();
  assert.equal(await stored(page,welcomeKey),'seen');
  // Shown once: a reload does not show it again.
  await page.reload();
  await expect(page.getByText('Start a task: its repositories, an agent and your first message, in a workspace of its own.')).toBeVisible();
  await page.waitForTimeout(600);
  await expect(page.getByRole('dialog',{name:'Welcome to Lazybox'})).toHaveCount(0);
  // Help reopens it; Escape closes it; New task opens the New task sheet (one name for starting work).
  await help(page,'Welcome & getting started');
  await expect(welcome).toBeVisible();
  await page.keyboard.press('Escape');
  await expect(welcome).toHaveCount(0);
  await help(page,'Welcome & getting started');
  await welcome.getByRole('button',{name:'New task',exact:true}).click();
  await expect(welcome).toHaveCount(0);
  const task=page.getByRole('dialog',{name:'New task'});
  await expect(task).toBeVisible();
  await expect(task.getByLabel('Personality')).toHaveValue('hands-on-v2');
  await page.keyboard.press('Escape');await expect(page.getByRole('dialog',{name:'New task'})).toHaveCount(0);
  // Help → Keyboard shortcuts opens the shortcuts overlay (Customize there leads to Settings).
  await help(page,'Keyboard shortcuts');
  const overlay=page.getByRole('dialog',{name:'Keyboard shortcuts'});await expect(overlay).toBeVisible();await page.keyboard.press('Escape');await expect(overlay).toHaveCount(0);
  // Help → Documentation opens the bundled docs, offline: pages, links between
  // them, anchors, Back, and search; Escape closes.
  await help(page,'Documentation');
  const docs=page.getByRole('dialog',{name:'Documentation'});await expect(docs).toBeVisible();
  await expect(docs.locator('.docs-page h1')).toContainText('Lazybox documentation');
  await docs.locator('.docs-page a.doc-link',{hasText:'Getting started'}).first().click();
  await expect(docs.locator('.docs-page h1')).toContainText('Getting started');
  await docs.getByRole('button',{name:'Back'}).click();
  await expect(docs.locator('.docs-page h1')).toContainText('Lazybox documentation');
  await docs.getByRole('searchbox',{name:'Search the documentation'}).fill('keyboard shortcuts');
  await expect(docs.getByRole('navigation',{name:'Search results'}).getByRole('button').first()).toBeVisible();
  await docs.getByRole('searchbox',{name:'Search the documentation'}).press('Enter');
  await expect(docs.getByRole('navigation',{name:'Pages'})).toBeVisible();
  await page.screenshot({path:'.cache/onboarding-docs.png'});
  await page.keyboard.press('Escape');await expect(docs).toHaveCount(0);
  await drainPageErrors(errors);assert.deepEqual(errors,[]);
  await context.close();
 }
 fixture.collections.push(...savedCollections);fixture.sessions.push(...savedSessions);

 // (b) An existing user: workspaces already there. No welcome, recorded silently; nothing new to show.
 const context=await browser.newContext({viewport:{width:1400,height:900}});
 {
  const {page,errors}=await open(context);
  await expect(page.getByRole('navigation',{name:'Workspaces'})).toContainText('Alpha feature');
  await expect.poll(()=>stored(page,welcomeKey)).toBe('existing');
  await expect(page.getByRole('dialog',{name:'Welcome to Lazybox'})).toHaveCount(0);
  await expect(page.getByRole('complementary',{name:"What's new"})).toHaveCount(0);
  assert.equal(await stored(page,whatsNewKey),releaseNotes[0].id);

  // (c) After an update: the last release seen is older than the newest, or
  // one this build no longer lists (Dev and Working saw dated sections that a
  // release folded into its own): either way the newest shows.
  const older=releaseNotes.length>1?releaseNotes.at(-1).id:'2026-11-17.27';
  await page.evaluate(([key,value])=>localStorage.setItem(key,value),[whatsNewKey,older]);
  await page.reload();
  const card=page.getByRole('complementary',{name:"What's new"});
  await expect(card).toBeVisible();
  await expect(card).toContainText(releaseNotes[0].title);
  for(const item of releaseNotes[0].items)await expect(card).toContainText(item.title);
  // A big release shows its groups as headings.
  for(const group of new Set(releaseNotes[0].items.map(item=>item.group).filter(Boolean)))await expect(card.getByRole('heading',{name:group,exact:true})).toBeVisible();
  const between=releaseNotes.length-2;
  if(between>0)await expect(card.locator('summary')).toHaveText(`${between} more since you last looked`);
  // It does not take focus from work.
  assert.equal(await card.evaluate(node=>node.contains(document.activeElement)),false);
  await page.waitForTimeout(300);await page.screenshot({path:'.cache/onboarding-whats-new.png'});
  await card.getByRole('button',{name:'Got it'}).click();
  await expect(card).toHaveCount(0);
  assert.equal(await stored(page,whatsNewKey),releaseNotes[0].id);
  // Once per release.
  await page.reload();
  await expect(page.getByRole('navigation',{name:'Workspaces'})).toContainText('Alpha feature');
  await page.waitForTimeout(600);
  await expect(card).toHaveCount(0);
  // Help shows every release, focused; Escape dismisses it.
  await help(page,"What's new");
  await expect(card).toBeVisible();await expect(card).toBeFocused();
  if(releaseNotes.length>1)await expect(card.locator('summary')).toHaveText('Earlier releases');
  await page.keyboard.press('Escape');
  await expect(card).toHaveCount(0);
  await drainPageErrors(errors);assert.deepEqual(errors,[]);
 }
 await context.close();
 console.log('onboarding: welcome once on a fresh install, never for an existing user, What\'s new once per release, Help reopens both');
} finally {
 await browser?.close();await server.close();await fixture.close();
}
