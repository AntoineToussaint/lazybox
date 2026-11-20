// The artifact reader (ARTIFACTS.md → The page): an agent's long design doc
// (fixtures/artifact-sample.mjs) reads as a document. The toolbar names its
// kind; its title block the author, the version and the reading time, and the repeated
// h1 is dropped; the outline follows the reader (scroll-spy) and jumps;
// callouts, tables with status pills, task lists, code, a Mermaid diagram
// that fits the column, and footnotes render. History is closed until H or a
// click opens it, and each window remembers that; an older version comes
// from the version picker; Changes shows the two versions rendered side by
// side with the changed rows marked, and the line diff on demand. Print
// shows the document alone, with a title block, page margins and numbers.
// The Artifacts panel marks an artifact updated since you looked. Then
// screenshots at 1440 and 390, dark and light:
// .cache/artifact-reader-<view>-<width>-<scheme>.png. Fixture only; Chromium
// (print to PDF is Chromium's).
import { createServer } from 'vite';
import react from '@vitejs/plugin-react';
import { chromium, expect } from './fixtures/playwright.mjs';
import { fileURLToPath } from 'node:url';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import assert from 'node:assert/strict';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';
import { testViteCache } from './fixtures/vite-cache.mjs';
import { designDocV2, seedDesignDoc } from './fixtures/artifact-sample.mjs';

const fixture = await startRunnerFixture();
const session = fixture.sessions[0];
const doc = seedDesignDoc(fixture, { sessionId: session.id });
const shellDoc = fixture.artifacts.agentPut({ collectionId: 'alpha', sessionId: session.id, title: 'Shell instructions',
  content: ['```sh', 'printf workspace', '```', '', 'For browser sign-in, run this in your home folder:', '',
    '`example-tool login --profile dev`', '', 'Use `/mcp` in the agent.', '', '```sh', 'printf first', 'printf second', '```', '', '```json', '{}', '```'].join('\n') });
const todo = fixture.artifacts.agentPut({ collectionId: 'alpha', sessionId: session.id, title: 'Tasks', kind: 'todo-list',
  content: JSON.stringify({ schema: 'todo-list.v1', items: [{ id: 'verify', text: 'Verify phone', status: 'todo' }] }) });
const structuredDocs = [];
for (const kind of ['plan', 'spec', 'report', 'notes', 'other']) {
  const content = await readFile(new URL('../services/runner/code/internal/runner/artifacts/templates/' + kind + '.json', import.meta.url), 'utf8');
  const record = fixture.artifacts.agentPut({ collectionId: 'alpha', sessionId: session.id, title: 'Structured ' + kind, kind, format: 'json', content });
  structuredDocs.push({ record, field: Object.keys(JSON.parse(content))[1] });
}
const htmlSource = '<!doctype html><html><head><style>h2 { color: rgb(12, 34, 56) }</style></head><body>' +
  '<h2>Deploy preview</h2><details><summary>Metrics</summary>Latency</details>' +
  '<script>document.body.textContent = "script ran"</script>' +
  '<a href="https://example.invalid/">External link</a><img src="https://example.invalid/pixel">' +
  '<iframe src="https://example.invalid/frame"></iframe></body></html>';
const html = fixture.artifacts.agentPut({ collectionId: 'alpha', sessionId: session.id, title: 'Deploy preview', format: 'html', content: htmlSource });
fixture.artifacts.agentPut({ collectionId: 'alpha', sessionId: session.id, artifactId: html.id, content: htmlSource.replace('Latency', 'Throughput') });
const server = await createServer({ ...testViteCache(import.meta.url), configFile: false, logLevel: 'error', root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: fixture.url } } } });
let browser;
try {
  await server.listen(); await mkdir('.cache', { recursive: true });
  browser = await chromium.launch({ headless: true });
  const base = `http://127.0.0.1:${server.httpServer.address().port}`;
  const standalone = `${base}/?view=artifact&collection=alpha&artifact=${doc.id}#token=${fixture.token}`;
  const errors = [];
  const context = await browser.newContext({ viewport: { width: 1440, height: 900 }, colorScheme: 'dark' });
  // The system print dialog is stubbed: the print sheet stays for the print media checks.
  await context.addInitScript(() => { window.__printed = 0; window.print = () => { window.__printed++; }; });
  const page = await context.newPage(); page.setDefaultTimeout(15000); page.on('pageerror', e => errors.push(String(e)));
  for (const { record, field } of structuredDocs) {
    await page.goto(standalone.replace(doc.id, record.id));
    await expect(page.getByRole('heading', { name: field[0].toUpperCase() + field.slice(1), exact: true })).toBeVisible();
  }
  await page.goto(standalone.replace(doc.id, todo.id));
  const checkbox = page.getByRole('checkbox', { name: 'Verify phone' });
  await checkbox.click();
  await expect(page.getByLabel('Version', { exact: true })).toHaveValue('2');
  await expect(checkbox).toBeChecked();
  await page.getByLabel('Version', { exact: true }).selectOption('1');
  await expect(checkbox).toBeDisabled();
  await expect(checkbox).not.toBeChecked();
  await page.goto(standalone.replace(doc.id, html.id));
  const preview = page.frameLocator('iframe[title="HTML artifact preview"]');
  await expect(preview.getByRole('heading', { name: 'Deploy preview' })).toHaveCSS('color', 'rgb(12, 34, 56)');
  await preview.getByText('Metrics').click();
  await expect(preview.getByText('Throughput')).toBeVisible();
  await expect(preview.locator('script, iframe, [href], [src]')).toHaveCount(0);
  await expect(page.locator('iframe[title="HTML artifact preview"]')).toHaveAttribute('sandbox', '');
  await page.getByRole('tab', { name: 'Source' }).click();
  await expect(page.locator('.artifact-html-source')).toContainText('<script>');
  await page.getByRole('tab', { name: 'Changes' }).click();
  await expect(page.getByRole('region', { name: 'Changes from v1 to v2' })).toContainText('Throughput');
  await page.getByRole('tab', { name: 'Preview' }).click();
  await page.getByLabel('Version', { exact: true }).selectOption('1');
  await expect(preview.getByText('Latency')).toHaveCount(1);
  await page.getByRole('tab', { name: 'Source' }).click();
  await page.locator('.artifact-html-source').evaluate(node => {
    const range = document.createRange();
    range.selectNodeContents(node);
    getSelection().removeAllRanges();
    getSelection().addRange(range);
  });
  const sendsBefore = fixture.mutations.filter(call => call.method === 'Apply').length;
  await page.getByRole('button', { name: 'Discuss selection in chat' }).click();
  const chat = page.getByRole('complementary', { name: 'Artifact chat' });
  await expect(chat.getByRole('combobox', { name: 'Conversation' })).toHaveValue(session.id);
  const draft = chat.getByRole('textbox', { name: 'Message', exact: true });
  await expect(draft).toHaveValue(/--version 1[\s\S]*Selected excerpt:\n> .*Latency/);
  await expect(page.getByRole('separator', { name: 'Resize the chat' })).toBeVisible();
  await page.setViewportSize({ width: 650, height: 900 });
  await expect(chat).toBeVisible();
  await expect(page.getByRole('separator', { name: 'Resize the chat' })).toBeHidden();
  await page.setViewportSize({ width: 1440, height: 900 });
  await chat.getByRole('button', { name: 'Close artifact chat' }).click();
  await expect(chat).toHaveCount(0);
  await expect(page.getByLabel('Version', { exact: true })).toHaveValue('1');
  assert.equal(fixture.mutations.filter(call => call.method === 'Apply').length, sendsBefore, 'opening artifact chat must not send a prompt');
  await page.getByRole('tab', { name: 'Preview' }).click();
  await page.getByLabel('Export', { exact: true }).click();
  const downloadPromise = page.waitForEvent('download');
  await page.getByRole('button', { name: 'Download .html' }).click();
  const download = await downloadPromise;
  assert.equal(download.suggestedFilename(), 'deploy-preview.html');
  assert.equal(await readFile(await download.path(), 'utf8'), htmlSource);
  await page.getByRole('button', { name: 'Republish v1' }).click();
  await expect(page.getByLabel('Version', { exact: true })).toHaveValue('3');
  await page.getByRole('tab', { name: 'Source' }).click();
  await expect(page.locator('.artifact-html-source')).toHaveText(htmlSource);

  // An artifact without an originating conversation uses the workspace’s new-chat form.
  const standaloneNote = fixture.artifacts.agentPut({ collectionId: 'alpha', title: 'Workspace note', content: 'Notes for a new discussion.' });
  await page.goto(standalone.replace(doc.id, standaloneNote.id));
  await page.getByRole('button', { name: 'Discuss in chat' }).click();
  await expect(chat.getByRole('combobox', { name: 'Conversation' })).toHaveValue('');
  await expect(chat).toContainText('The artifact reference will be added to your draft');
  assert.equal(fixture.mutations.filter(call => call.method === 'OpenSession').length, 0, 'the chat pane does not start an agent');
  await chat.getByRole('button', { name: /^Start Claude/ }).click();
  await expect(draft).toHaveValue(/Workspace note/);
  const started = fixture.mutations.find(call => call.method === 'OpenSession');
  assert.equal(started?.body.collectionId, 'alpha');
  assert.equal(fixture.mutations.filter(call => call.method === 'Apply').length, sendsBefore, 'starting the selected conversation must leave the reference in the draft');

  const noteDraft = await draft.inputValue();
  await draft.fill(noteDraft + '\n\nCould you check this?');
  await chat.getByRole('button', { name: 'Close artifact chat' }).click();
  await expect(chat).toHaveCount(0);
  await page.getByRole('button', { name: 'Discuss in chat' }).click();
  await expect(draft).toHaveValue(noteDraft + '\n\nCould you check this?');
  await chat.getByRole('button', { name: 'Send message', exact: true }).click();
  await expect.poll(() => fixture.mutations.filter(call => call.method === 'Apply' && call.body.message).length).toBe(sendsBefore + 1);
  const discussion = fixture.mutations.find(call => call.method === 'Apply' && call.body.message);
  assert.match(JSON.stringify(discussion.body.message), /Workspace note.*Could you check this/);
  assert.equal(fixture.sessions.find(item => item.id === discussion.body.sessionId)?.collectionId, 'alpha');

  // A discussion pins its source revision and keeps the chat beside the diff.
  const reviewDoc = fixture.artifacts.agentPut({ collectionId: 'alpha', sessionId: session.id, title: 'Revision review', content: '## Scope\n\nKeep the original wording.' });
  await page.goto(standalone.replace(doc.id, reviewDoc.id));
  const reviewPage = page.getByRole('article', { name: 'Revision review' });
  await reviewPage.getByRole('button', { name: 'Discuss in chat' }).click();
  await expect(draft).toHaveValue(/Revision review/);
  await draft.fill('Could we make this clearer?');
  const publishReview = wording => fixture.artifacts.agentPut({ collectionId: 'alpha', sessionId: session.id, artifactId: reviewDoc.id, content: '## Scope\n\n' + wording });
  publishReview('Use clearer wording.');
  await expect(reviewPage.getByRole('button', { name: 'Review update' })).toBeVisible({ timeout: 10000 });
  await expect(reviewPage.locator('.artifact-document')).toContainText('Keep the original wording.');
  await reviewPage.getByRole('button', { name: 'Review update' }).click();
  const comparison = reviewPage.getByRole('region', { name: 'Rendered changes' });
  await expect(reviewPage.getByRole('heading', { name: 'Changes', exact: true })).toBeFocused();
  await expect(comparison).toContainText('Use clearer wording.');
  await expect(comparison).toContainText('Keep the original wording.');
  await expect(reviewPage.getByRole('combobox', { name: 'Compare with' })).toHaveValue('1');
  await expect(draft).toHaveValue('Could we make this clearer?');
  publishReview('Use the final wording.');
  await expect(reviewPage.getByRole('button', { name: 'Review update' })).toBeVisible({ timeout: 10000 });
  await expect(comparison).toContainText('Use clearer wording.');
  await expect(comparison).not.toContainText('Use the final wording.');
  await reviewPage.getByRole('button', { name: 'Review update' }).click();
  await expect(comparison).toContainText('Use the final wording.');
  await reviewPage.getByRole('button', { name: 'Read latest' }).click();
  await expect(reviewPage.locator('.artifact-document')).toContainText('Use the final wording.');
  await expect(draft).toHaveValue('Could we make this clearer?');

  await page.goto(standalone.replace(doc.id, shellDoc.id));
  const shellReviews = page.getByRole('button', { name: 'Review in terminal' });
  await expect(shellReviews).toHaveCount(3);
  assert.equal(fixture.terminals.size, 0, 'reading an artifact does not start a shell');
  await shellReviews.first().click();
  const shellDock = page.getByRole('region', { name: 'Artifact terminal' });
  await expect(shellDock).toBeVisible();
  await expect.poll(() => [...fixture.terminals.values()][0]?.input).toEqual(['printf workspace']);
  const workspaceShell = [...fixture.terminals.values()][0];
  assert.equal(workspaceShell.collectionId, 'alpha');
  await shellDock.locator('.xterm-helper-textarea').press('Enter');
  await expect.poll(() => workspaceShell.input).toEqual(['printf workspace', '\r']);
  assert.match(workspaceShell.output.toString(), /ran: printf workspace/);
  await page.getByRole('button', { name: 'Hide artifact terminal' }).click();
  await expect(shellDock).toHaveCount(0);
  await shellReviews.nth(1).click();
  await expect.poll(() => [...fixture.terminals.values()].find(t => t.place === 'user-home')?.input)
    .toEqual(['example-tool login --profile dev']);
  await shellReviews.last().click();
  await expect(page.getByRole('alert').filter({ hasText: 'cannot hold a multi-line paste' })).toBeVisible();
  assert.deepEqual([...fixture.terminals.values()].at(-1)?.input, [], 'a shell without bracketed paste receives no script');


  await page.goto(standalone);
  const article = page.getByRole('article', { name: 'Datasource migration' });
  const reader = article.locator('.artifact-scroll');

  // The kind in the toolbar; the title once, who wrote it, when, which version, how long.
  await expect(article.getByRole('heading', { name: 'Datasource migration', level: 1 })).toBeVisible();
  await expect(article.locator('.doc-body h1')).toHaveCount(0);
  await expect(article.getByLabel('Kind', { exact: true })).toHaveValue('spec');
  await expect(article.locator('.doc-meta')).toContainText('Published by an agent');
  await expect(article.locator('.doc-meta')).toContainText(/\d+ min read/);
  await expect(article.getByLabel('Version', { exact: true })).toHaveValue('2');
  // The reading column keeps a readable measure, in the bundled reading face.
  const measure = await article.locator('.doc-body > p').first().evaluate(p => ({ width: p.getBoundingClientRect().width, size: parseFloat(getComputedStyle(p).fontSize), font: getComputedStyle(p).fontFamily }));
  assert.ok(measure.width / measure.size >= 40 && measure.width / measure.size <= 50, `measure of ${measure.width / measure.size}em`);
  assert.ok(measure.size >= 15, `body text at ${measure.size}px`);
  assert.match(measure.font, /Inter Variable/);
  await expect.poll(() => page.evaluate(() => document.fonts.check('16px "Inter Variable"') && document.fonts.check('40px "Newsreader Variable"'))).toBe(true);

  const edges = await article.evaluate(node => {
    const left = selector => node.querySelector(selector).getBoundingClientRect().left;
    return ['.doc-column', '.doc-title', '.doc-body > p', '.doc-body > h2', '.doc-table', '.doc-body > .code-block']
      .map(left);
  });
  assert.ok(Math.max(...edges) - Math.min(...edges) <= 1, `document blocks share one left edge: ${edges}`);

  // Callouts: the label leaves the text and becomes the callout's head.
  await expect(article.locator('.doc-callout')).toHaveCount(3);
  for (const kind of ['Decision', 'Warning', 'Note']) await expect(article.getByRole('note', { name: kind, exact: true })).toBeVisible();
  await expect(article.getByRole('note', { name: 'Decision' })).toContainText('Move reads to the new datasource first');
  await expect(article.locator('.doc-callout')).not.toContainText(['**']);
  assert.equal(await article.locator('.doc-body blockquote').count(), 0, 'no callout is left as a plain quote');

  // Tables: their own frame, a sticky header, status pills.
  const rollout = article.getByRole('region', { name: 'Table' }).first();
  await expect(rollout.locator('thead th').first()).toHaveCSS('position', 'sticky');
  await expect(rollout.locator('.doc-pill.tone-ok').first()).toHaveText('Done');
  await expect(rollout.locator('.doc-pill.tone-info')).toHaveText('In progress');
  // Task lists, code with its language and copy, footnotes.
  await expect(article.locator('.task-list-item input[type=checkbox]')).toHaveCount(5);
  await expect(article.locator('.task-list-item input[type=checkbox]:checked')).toHaveCount(3);
  const goBlock = article.locator('.code-block').filter({ hasText: 'TenantStore reads and writes' });
  await expect(goBlock.locator('header')).toContainText('Go');
  await expect(goBlock.getByRole('button', { name: 'Copy code' })).toHaveCount(1);
  await expect(article.locator('.doc-footnotes')).toContainText('Measured over the four weeks');
  // The Mermaid diagram renders at once and fits the column.
  const diagram = article.getByRole('img', { name: 'Mermaid diagram' });
  await expect.poll(() => diagram.evaluate(img => img.complete && img.naturalWidth > 0).catch(() => false)).toBe(true);
  const fit = await article.locator('.mermaid-block').evaluate(figure => ({ figure: figure.getBoundingClientRect().width, image: figure.querySelector('img').getBoundingClientRect().width }));
  assert.ok(fit.image <= fit.figure + 1, `diagram ${fit.image}px in a ${fit.figure}px column`);

  // The outline: "On this page" follows the reading position and jumps.
  const toc = article.getByRole('navigation', { name: 'On this page' });
  await expect(toc.getByRole('link')).toHaveCount(9);
  await expect(toc.locator('[aria-current=location]')).toHaveText('Context');
  await reader.evaluate(node => { const h = node.querySelector('#interface'); node.scrollTop += h.getBoundingClientRect().top - node.getBoundingClientRect().top - 40; });
  await expect(toc.locator('[aria-current=location]')).toHaveText('Interface');
  await toc.getByRole('link', { name: 'Risks' }).click();
  await expect(toc.locator('[aria-current=location]')).toHaveText('Risks');
  await expect.poll(() => article.locator('#risks').evaluate(h => Math.abs(h.getBoundingClientRect().top - h.closest('.artifact-scroll').getBoundingClientRect().top))).toBeLessThan(80);
  await expect(page.locator(':focus')).toHaveAttribute('id', 'risks');
  // The scrolled toolbar names the document.
  await expect(article.locator('.artifact-crumb')).toContainText('Datasource migration');
  // A footnote reference jumps to its note.
  await reader.evaluate(node => { node.scrollTop = 0; });
  await article.locator('sup a').first().click();
  await expect.poll(() => article.locator('.doc-footnotes').evaluate(f => f.getBoundingClientRect().top < innerHeight)).toBe(true);
  await reader.evaluate(node => { node.scrollTop = 0; });

  // History is closed by default; H opens it, the window remembers, H closes it.
  const versions = article.getByRole('listbox', { name: 'Versions' });
  await expect(versions).toHaveCount(0);
  await expect(article.locator('.doc-column')).toBeVisible();
  const columnBefore = await article.locator('.artifact-main').evaluate(n => n.getBoundingClientRect().width);
  await page.locator('body').focus(); await page.keyboard.press('h');
  await expect(versions.getByRole('option')).toHaveCount(2);
  assert.ok(await article.locator('.artifact-main').evaluate(n => n.getBoundingClientRect().width) < columnBefore, 'the reader gives the history its room');
  await page.reload();
  await expect(page.getByRole('article', { name: 'Datasource migration' }).getByRole('listbox', { name: 'Versions' })).toBeVisible();
  await article.getByRole('button', { name: /^History · 2 versions/ }).click();
  await expect(versions).toHaveCount(0);
  await article.getByRole('button', { name: /^History · 2 versions/ }).click();
  await expect(versions).toBeVisible();
  await versions.focus().catch(() => {});
  await article.locator('.doc-title').click(); await page.keyboard.press('h');
  await expect(versions).toHaveCount(0);
  await page.reload();
  await expect(page.getByRole('article', { name: 'Datasource migration' }).getByRole('heading', { level: 1 })).toBeVisible();
  await expect(versions).toHaveCount(0);

  // An older version from the picker, with a way back.
  await article.getByLabel('Version', { exact: true }).selectOption('1');
  await expect(article.locator('.artifact-old-version')).toContainText('You are viewing v1 of 2.');
  await expect(article.locator('.task-list-item input:checked')).toHaveCount(2);
  await article.getByLabel('Export', { exact: true }).click();
  const [documentDownload] = await Promise.all([
    page.waitForEvent('download'),
    page.getByRole('button', { name: 'Download .html', exact: true }).click(),
  ]);
  assert.equal(documentDownload.suggestedFilename(), 'datasource-migration.html');
  const exportedFile = new URL('../.cache/artifact-reader-export.html', import.meta.url);
  await documentDownload.saveAs(fileURLToPath(exportedFile));
  const exported = await context.newPage();
  await exported.goto(exportedFile.href);
  await expect(exported.getByRole('heading', { name: 'Datasource migration', exact: true })).toBeVisible();
  await expect(exported.locator('.print-meta')).toContainText('Version 1 of 2');
  await expect(exported.getByRole('table')).not.toHaveCount(0);
  await expect(exported.locator('pre')).not.toHaveCount(0);
  await expect(exported.locator('.task-list-item input:checked:disabled')).toHaveCount(2);
  await expect(exported.locator('script, iframe, button, [role=toolbar]')).toHaveCount(0);
  await expect(exported.locator('.mermaid-block img')).toHaveAttribute('src', /^data:image\/svg\+xml/);
  await expect.poll(() => exported.locator('.mermaid-block img').evaluate(img => img.complete && img.naturalWidth > 0)).toBe(true);
  await exported.close();
  await article.getByRole('button', { name: 'Back to latest' }).click();
  await expect(article.locator('.task-list-item input:checked')).toHaveCount(3);

  // Changes, rendered side by side: replaced blocks paired, changed rows marked.
  await article.getByRole('tab', { name: 'Changes' }).click();
  const rendered = article.getByRole('region', { name: 'Rendered changes' });
  await expect(rendered.locator('.doc-diff-row.is-changed')).toHaveCount(4);
  await expect(rendered.locator('.doc-diff-head')).toContainText('v1');
  await expect(rendered.locator('.doc-diff-head')).toContainText('v2');
  await expect(rendered.locator('.doc-diff-block.is-removed tr.is-marked')).toHaveCount(2);
  await expect(rendered.locator('.doc-diff-block.is-added tr.is-marked').first()).toContainText('2. Dual reads');
  await expect(rendered.locator('.doc-diff-block.is-added tr.is-marked')).toHaveCount(3);
  await expect(rendered.locator('.doc-diff-fold').first()).toContainText('unchanged block');
  await page.screenshot({ path: '.cache/artifact-reader-changes-check.png' });
  await article.getByRole('radio', { name: 'Text' }).click();
  await expect(article.getByRole('region', { name: 'Changes from v1 to v2' }).locator('.artifact-diff-line.is-add')).toHaveCount(5);
  await article.getByRole('radio', { name: 'Rendered' }).click();
  // From the history: compare a version with the one before it.
  await article.getByRole('tab', { name: 'Document' }).click();
  await page.keyboard.press('h');
  await versions.getByRole('button', { name: 'Compare with v1' }).click();
  await expect(rendered).toBeVisible();
  await page.keyboard.press('h');

  // Print from Changes: switches back to the selected document for print.
  await page.evaluate(() => {
    window.__printFailure = true;
    window.print = async () => {
      window.__printed++;
      if (window.__printFailure) throw new Error('Native print unavailable');
    };
  });
  const previousTitle = await page.title();
  await article.getByLabel('Export', { exact: true }).click();
  await page.getByRole('button', { name: 'Print or save as PDF…' }).click();
  await expect(article.getByRole('alert')).toContainText('Could not open the print dialog: Native print unavailable');
  await expect(article.getByRole('tab', { name: 'Document' })).toHaveAttribute('aria-selected', 'true');
  await expect(page.locator('.artifact-print-sheet')).toHaveCount(0);
  assert.equal(await page.title(), previousTitle);
  await page.evaluate(() => { window.__printFailure = false; });

  // Retry: the document alone, as a document.
  await article.getByLabel('Export', { exact: true }).click();
  await page.getByRole('button', { name: 'Print or save as PDF…' }).click();
  await expect.poll(() => page.evaluate(() => window.__printed)).toBe(2);
  // A native dialog can close without delivering a browser afterprint event.
  await article.getByLabel('Export', { exact: true }).click();
  await page.getByRole('button', { name: 'Print or save as PDF…' }).click();
  await expect.poll(() => page.evaluate(() => window.__printed)).toBe(3);
  await expect(page.locator('.artifact-print-sheet')).toHaveCount(1);
  await page.emulateMedia({ media: 'print' });
  const print = await page.evaluate(() => {
    const sheet = document.querySelector('.artifact-print-sheet');
    const shown = [...document.body.children].filter(n => getComputedStyle(n).display !== 'none');
    const rule = [...document.styleSheets].flatMap(s => { try { return [...s.cssRules]; } catch { return []; } }).flatMap(r => r instanceof CSSMediaRule ? [...r.cssRules] : [r]).find(r => r instanceof CSSPageRule);
    const body = sheet.querySelector('.doc-body');
    return {
      shown: shown.map(n => n.className), eyebrow: sheet.querySelector('.print-eyebrow')?.textContent, title: sheet.querySelector('h1')?.textContent, meta: sheet.querySelector('.print-meta')?.textContent,
      buttons: sheet.querySelectorAll('button, [role=toolbar], .doc-anchor').length, diagram: !!sheet.querySelector('.mermaid-block img'), margin: rule?.style.margin || '',
      bodySize: parseFloat(getComputedStyle(body).fontSize), background: getComputedStyle(document.body).backgroundColor, toc: !!sheet.querySelector('.doc-toc'),
    };
  });
  assert.deepEqual(print.shown, ['artifact-print-sheet'], 'only the print sheet is printed: no app chrome');
  assert.equal(print.eyebrow, 'Spec'); assert.equal(print.title, 'Datasource migration');
  assert.match(print.meta, /Version 2/); assert.match(print.meta, /Published by an agent/);
  assert.equal(print.buttons, 0, 'no buttons, toolbars or anchors on paper');
  assert.ok(print.diagram, 'the diagram is printed, not a placeholder'); assert.ok(!print.toc, 'the outline is not printed');
  assert.equal(print.margin, '22mm 20mm 24mm', 'page margins of a real document');
  assert.ok(print.bodySize >= 13 && print.bodySize <= 15, `body text at ${print.bodySize}px (10.5pt)`);
  assert.equal(print.background, 'rgb(255, 255, 255)');
  await page.screenshot({ path: '.cache/artifact-reader-print.png', fullPage: true });
  const pdf = await page.pdf({ format: 'A4', printBackground: true, preferCSSPageSize: true });
  await writeFile('.cache/artifact-reader-print.pdf', pdf);
  const pages = (pdf.toString('latin1').match(/\/Type\s*\/Page[^s]/g) || []).length;
  assert.ok(pages >= 2 && pages <= 8, `${pages} printed pages`);
  await page.emulateMedia({ media: null });
  await page.evaluate(() => window.dispatchEvent(new Event('afterprint')));
  await expect(page.locator('.artifact-print-sheet')).toHaveCount(0);
  assert.equal(await page.title(), previousTitle);
  await context.close();

  // The Artifacts panel: kind icons, author and time, and the "updated since you looked" dot.
  fixture.artifacts.agentPut({ collectionId: 'alpha', sessionId: session.id, title: 'Review findings', kind: 'report', content: '# Review findings\n\nNo blockers.\n' });
  const panelContext = await browser.newContext({ viewport: { width: 1440, height: 900 }, colorScheme: 'light' });
  const panelPage = await panelContext.newPage(); panelPage.setDefaultTimeout(15000); panelPage.on('pageerror', e => errors.push(String(e)));
  await panelPage.goto(`${base}/?view=workspace&collection=alpha&session=${session.id}#token=${fixture.token}`);
  await panelPage.locator('summary[aria-label="Panels"]').click();
  await panelPage.getByRole('button', { name: 'Show Artifacts', exact: true }).click();
  const list = panelPage.getByRole('listbox', { name: 'Artifacts' });
  await list.getByRole('option', { name: /Shell instructions/ }).click();
  await panelPage.getByRole('button', { name: 'Review in terminal' }).first().click();
  await expect(panelPage.getByRole('region', { name: 'Artifact terminal' })).toBeVisible();
  await expect.poll(() => [...fixture.terminals.values()].at(-1)?.input).toEqual(['printf workspace']);
  await panelPage.getByRole('button', { name: 'Hide artifact terminal' }).click();
  await panelPage.getByRole('button', { name: 'Back to artifacts' }).click();
  const row = list.getByRole('option', { name: /Datasource migration/ });
  await expect(row).toContainText('Spec · v2 · by an agent');
  await expect(row.locator('.artifact-kind-icon')).toBeVisible();
  await expect(row.getByRole('img', { name: 'Updated since you last looked' })).toBeVisible();
  await expect(list.getByRole('option', { name: /Review findings/ }).getByRole('img', { name: 'Updated since you last looked' })).toBeVisible();
  await panelPage.screenshot({ path: '.cache/artifact-reader-panel-list-1440-light.png' });
  await row.click();
  await expect(panelPage.getByRole('article', { name: 'Datasource migration' }).getByRole('heading', { level: 1 })).toBeVisible();
  const back = panelPage.getByRole('button', { name: 'Back to artifacts' });
  if (await back.isVisible()) await back.click();
  await expect(row.getByRole('img', { name: 'Updated since you last looked' })).toHaveCount(0);
  // A new version from the agent brings the dot back.
  fixture.artifacts.agentPut({ collectionId: 'alpha', sessionId: session.id, artifactId: doc.id, content: designDocV2 + '\n## Appendix\n\nLoad test results.\n' });
  await expect(row.getByRole('img', { name: 'Updated since you last looked' })).toBeVisible({ timeout: 10000 });
  await expect(row).toContainText('v3');
  await panelContext.close();

  // Phone: no outline rail; On this page jumps instead. History opens over the page.
  const phone = await browser.newContext({ viewport: { width: 390, height: 844 }, colorScheme: 'dark', deviceScaleFactor: 2, hasTouch: true });
  const mobile = await phone.newPage(); mobile.setDefaultTimeout(15000); mobile.on('pageerror', e => errors.push(String(e)));
  await mobile.goto(standalone);
  const small = mobile.getByRole('article', { name: 'Datasource migration' });
  await expect(small.getByRole('heading', { level: 1 })).toBeVisible();
  await expect(small.getByRole('navigation', { name: 'On this page' })).toBeHidden();
  await small.locator('.doc-contents-menu > summary').click();
  await expect(mobile.getByRole('group', { name: 'On this page menu' })).toBeVisible();
  await mobile.getByRole('button', { name: 'Risks' }).click();
  await expect.poll(() => small.locator('#risks').evaluate(h => h.getBoundingClientRect().top)).toBeLessThan(200);
  assert.equal(await mobile.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true, 'no horizontal page scroll');
  await phone.close();

  // Screenshots for review: 1440 and 390, dark and light.
  for (const scheme of ['dark', 'light']) for (const [width, height] of [[1440, 900], [390, 844]]) {
    const shots = await browser.newContext({ viewport: { width, height }, colorScheme: scheme, deviceScaleFactor: width < 600 ? 2 : 1 });
    const shot = await shots.newPage(); shot.setDefaultTimeout(15000); shot.on('pageerror', e => errors.push(String(e)));
    await shot.goto(standalone);
    const view = shot.getByRole('article', { name: 'Datasource migration' });
    await expect.poll(() => view.getByRole('img', { name: 'Mermaid diagram' }).evaluate(img => img.complete && img.naturalWidth > 0).catch(() => false)).toBe(true);
    await shot.evaluate(() => document.fonts.ready); await shot.waitForTimeout(600);
    const name = what => `.cache/artifact-reader-${what}-${width}-${scheme}.png`;
    await shot.screenshot({ path: name('top') });
    await view.locator('#architecture').evaluate(h => h.scrollIntoView({ block: 'start' })); await shot.waitForTimeout(300);
    await shot.screenshot({ path: name('diagram') });
    await view.locator('#rollout-plan').evaluate(h => h.scrollIntoView({ block: 'start' })); await shot.waitForTimeout(300);
    await shot.screenshot({ path: name('tables') });
    await view.locator('#milestones').evaluate(h => h.scrollIntoView({ block: 'start' })); await shot.waitForTimeout(300);
    await shot.screenshot({ path: name('end') });
    await view.getByRole('button', { name: /^History/ }).click(); await shot.waitForTimeout(300);
    await shot.screenshot({ path: name('history') });
    await view.getByRole('button', { name: 'Close history' }).click();
    await view.getByRole('tab', { name: 'Changes' }).click(); await shot.waitForTimeout(800);
    await shot.screenshot({ path: name('changes') });
    await shots.close();
  }
  assert.deepEqual(errors, []);
  console.log('PASS: the artifact reader shows a long design doc as a document (title block, outline with scroll-spy, callouts, tables, task lists, code, Mermaid, footnotes), keeps History closed until asked and remembers it per window, compares versions rendered side by side, prints the document alone with page margins, and marks artifacts updated since you looked. Screenshots in .cache/artifact-reader-*.png. Fixture only.');
} finally { await browser?.close(); await server.close(); await fixture.close(); }
