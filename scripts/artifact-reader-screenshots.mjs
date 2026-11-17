// Screenshots of the artifact reader for design review, against the fake
// Runner: the sample design doc and the sample report
// (fixtures/artifact-sample.mjs) in their own window at 1000, 1400 and 420
// wide, dark and light, at the top and scrolled to a table, and the design
// doc in the Artifacts panel beside the chat. Prints what the density work
// watches: the text column's width, the title's size, how far below the
// toolbar the title and the body's first line start, the outline's width.
// Writes .cache/artifact-reader-<label>-<doc>-<view>-<width>-<scheme>.png.
//   node scripts/artifact-reader-screenshots.mjs [label]
import { createServer } from 'vite';
import react from '@vitejs/plugin-react';
import { chromium } from '@playwright/test';
import { fileURLToPath } from 'node:url';
import { mkdir } from 'node:fs/promises';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';
import { testViteCache } from './fixtures/vite-cache.mjs';
import { seedDesignDoc, seedReport } from './fixtures/artifact-sample.mjs';

const label = process.argv[2] || 'current';
const fixture = await startRunnerFixture();
const docs = { design: { artifact: seedDesignDoc(fixture), ready: 'Move reads to the new datasource first', table: '#rollout-plan' }, report: { artifact: seedReport(fixture), ready: 'This is the final pass', table: '#who-wins-what' } };
const server = await createServer({ ...testViteCache(import.meta.url), configFile: false, logLevel: 'error', root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react()], server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: fixture.url } } } });
let browser;
/** The numbers the density work is judged by, in CSS pixels. */
const measure = page => page.evaluate(() => {
  const box = node => node?.getBoundingClientRect();
  const toolbar = box(document.querySelector('.artifact-toolbar')), scroller = box(document.querySelector('.artifact-scroll'));
  const title = document.querySelector('.doc-title'), first = document.querySelector('.doc-body > *'), paragraph = document.querySelector('.doc-body > p');
  const toc = document.querySelector('.doc-toc-rail');
  const tocShown = toc && getComputedStyle(toc).display !== 'none';
  return {
    text: Math.round(box(paragraph)?.width || 0),
    gutter: Math.round((box(paragraph)?.left || 0) - scroller.left),
    titleSize: title ? parseFloat(getComputedStyle(title).fontSize) : 0,
    titleLines: title ? Math.round(box(title).height / parseFloat(getComputedStyle(title).lineHeight)) : 0,
    titleTop: Math.round((box(title)?.top || 0) - toolbar.bottom),
    bodyTop: Math.round((box(first)?.top || 0) - toolbar.bottom),
    toc: tocShown ? Math.round(box(toc).width) : 0,
    pill: !!document.querySelector('.doc-title-block .doc-kind'),
    h1s: document.querySelectorAll('.artifact-page h1').length,
    overflow: document.documentElement.scrollWidth > innerWidth,
  };
});
try {
  await server.listen(); await mkdir('.cache', { recursive: true });
  browser = await chromium.launch({ headless: true });
  const base = `http://127.0.0.1:${server.httpServer.address().port}`;
  const rows = [];
  for (const [doc, { artifact, ready, table }] of Object.entries(docs)) for (const scheme of ['dark', 'light']) for (const [width, height] of [[1000, 800], [1400, 900], [420, 860]]) {
    const context = await browser.newContext({ viewport: { width, height }, colorScheme: scheme, deviceScaleFactor: width < 600 ? 2 : 1 });
    const page = await context.newPage(); page.setDefaultTimeout(15000);
    await page.goto(`${base}/?view=artifact&collection=alpha&artifact=${artifact.id}#token=${fixture.token}`);
    await page.getByText(ready).first().waitFor();
    await page.evaluate(() => document.fonts.ready); await page.waitForTimeout(1200);
    const name = view => `.cache/artifact-reader-${label}-${doc}-${view}-${width}-${scheme}.png`;
    if (scheme === 'dark') rows.push({ doc, width, ...await measure(page) });
    await page.screenshot({ path: name('top') });
    await page.locator(table).first().evaluate(node => node.scrollIntoView({ block: 'start' }));
    await page.waitForTimeout(500);
    await page.screenshot({ path: name('table') });
    if (doc === 'design' && width > 600) {
      await page.goto(`${base}/?view=workspace&collection=alpha&session=${fixture.sessions[0].id}#token=${fixture.token}`);
      await page.locator('summary[aria-label="Panels"]').click();
      await page.getByRole('button', { name: 'Show Artifacts', exact: true }).click();
      await page.getByRole('listbox', { name: 'Artifacts' }).getByRole('option', { name: /Datasource migration/ }).click();
      await page.getByText(ready).first().waitFor();
      await page.waitForTimeout(1200);
      await page.screenshot({ path: name('panel') });
    }
    await context.close();
  }
  console.table(rows);
  console.log(`Screenshots in .cache/artifact-reader-${label}-*.png`);
} finally { await browser?.close(); await server.close(); await fixture.close(); }
