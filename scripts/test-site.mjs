// Browser smoke test for a built Pages artifact, including the legacy JS search.
import assert from 'node:assert/strict';
import { createReadStream, existsSync, statSync } from 'node:fs';
import { createServer } from 'node:http';
import { extname, resolve, sep } from 'node:path';
import { chromium } from 'playwright';

const base = process.env.DOCS_BASE || '/';
const root = resolve('.cache/site');
const mime = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.json': 'application/json', '.svg': 'image/svg+xml', '.webp': 'image/webp', '.mp4': 'video/mp4' };
const server = createServer((req, res) => {
  const path = new URL(req.url, 'http://localhost').pathname;
  const file = resolve(root, '.' + path.slice(base.length - 1));
  if (path.startsWith(base) && (file === root || file.startsWith(root + sep))) {
    const asset = [file, file + '.html', resolve(file, 'index.html')].find(p => existsSync(p) && statSync(p).isFile());
    if (asset) {
      res.writeHead(200, { 'content-type': mime[extname(asset)] || 'application/octet-stream' });
      createReadStream(asset).pipe(res);
      return;
    }
  }
  res.writeHead(404).end();
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
let browser;
try {
  const url = 'http://127.0.0.1:' + server.address().port + base;
  browser = await chromium.launch();
  const page = await browser.newPage();
  await page.goto(url);
  assert.equal(await page.locator('h1').innerText(), 'A workspace for\nyour coding agents.');
  assert.ok(await page.locator('.hero img').evaluate(img => img.complete && img.naturalWidth > 0));
  await page.getByRole('link', { name: 'Guides', exact: true }).click();
  await page.waitForURL(url + 'guide/');
  await page.goto(url + 'guides/chat#conversation');
  await page.waitForURL(url + 'guide/guides/chat#conversation');
  await page.goto(url + 'docs/tutorials/quickstart/');
  await page.locator('button[data-open-modal]').click();
  await page.locator('#starlight__search input').fill('workspace');
  const result = page.locator('#starlight__search a[href]').first();
  await result.waitFor();
  const href = await result.evaluate(a => a.href);
  assert.ok(href.startsWith(url + 'docs/'), href);
  assert.equal((await page.goto(href)).status(), 200);
  console.log('Homepage, V2 guides, legacy redirect and archive search passed at ' + base);
} finally {
  await browser?.close();
  await new Promise(resolve => server.close(resolve));
}
