// Renders the disk image window's background (packaging/dmg/background.html)
// at 1x and 2x with the Playwright Chromium the UI tests already use. The
// PNG files are committed; run this after editing the HTML:
//
//   node scripts/release/dmg-background.mjs
import { chromium } from '@playwright/test';
import { resolve, dirname } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');
const dir = resolve(root, 'packaging', 'dmg');
const browser = await chromium.launch();
try {
  for (const [scale, name] of [[1, 'background.png'], [2, 'background@2x.png']]) {
    const page = await browser.newPage({ viewport: { width: 660, height: 400 }, deviceScaleFactor: scale });
    await page.goto(pathToFileURL(resolve(dir, 'background.html')).href);
    await page.screenshot({ path: resolve(dir, name), omitBackground: false });
    await page.close();
    console.log(`Wrote packaging/dmg/${name}`);
  }
} finally {
  await browser.close();
}
