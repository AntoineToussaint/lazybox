// Record the real drawing UI against a local fixture. The public clip has no audio.
import { mkdir } from 'node:fs/promises';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import react from '@vitejs/plugin-react';
import { createServer, testViteCache } from './fixtures/vite-cache.mjs';
import { chromium, expect } from './fixtures/playwright.mjs';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';
import { drawingAssets } from './drawing-assets.mjs';

const fixture = await startRunnerFixture();
const session = fixture.sessions[0];
const artifact = fixture.artifacts.agentPut({ collectionId: 'alpha', sessionId: session.id,
  title: 'Image upload flow', content: '# Image upload flow\n\n```mermaid\nflowchart LR\n  A[Choose image] --> B[Upload]\n  B --> C[Preview]\n```\n' });
const server = await createServer({ ...testViteCache(import.meta.url), configFile: false,
  root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react(), drawingAssets()],
  server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: fixture.url } } } });
const output = 'docs-site/public/demos';
const size = { width: 1120, height: 720 };
let browser;
try {
  await mkdir(output, { recursive: true });
  await server.listen();
  browser = await chromium.launch();
  const context = await browser.newContext({ viewport: size, colorScheme: 'dark',
    recordVideo: { dir: '.cache/drawing-demo', size } });
  const page = await context.newPage();
  const start = Date.now();
  const pause = ms => page.waitForTimeout(ms); // Pacing for readers watching the recording.
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/?view=artifact&collection=alpha&artifact=${artifact.id}#token=${fixture.token}`);
  const annotate = page.getByRole('button', { name: 'Annotate', exact: true });
  await expect(annotate).toBeVisible();
  const trim = (Date.now() - start) / 1000;
  await pause(1000);
  await annotate.click();
  const dialog = page.getByRole('dialog', { name: 'Drawing editor' });
  const canvas = dialog.locator('canvas.interactive');
  await expect(canvas).toBeVisible();
  await pause(900);
  const bounds = await canvas.boundingBox();
  const x = bounds.x + bounds.width / 2, y = bounds.y + bounds.height / 2;
  for (let step = 0; step <= 40; step++) {
    const theta = step * Math.PI / 20;
    await page.mouse.move(x + 54 * Math.cos(theta), y + 37 * Math.sin(theta));
    if (!step) await page.mouse.down();
    await pause(23);
  }
  await page.mouse.up();
  await pause(650);
  await page.keyboard.press('Control+z');
  await pause(800);
  await page.keyboard.press('Control+Shift+z');
  await pause(900);
  await page.screenshot({ path: '.cache/drawing-demo/poster.png' });
  await dialog.getByRole('button', { name: 'Attach drawing' }).click();
  const chat = page.getByRole('complementary', { name: 'Agent companion' });
  await chat.getByRole('combobox', { name: 'Conversation' }).selectOption(session.id);
  await expect(chat.getByRole('button', { name: 'Annotate diagram.excalidraw.png' })).toBeVisible();
  const draft = chat.getByRole('textbox', { name: 'Message', exact: true });
  await draft.press('ControlOrMeta+End');
  await draft.press('Shift+Enter');
  await draft.pressSequentially('Add a retry if upload fails.', { delay: 50 });
  await expect(draft).toHaveValue(/Add a retry if upload fails\./);
  await pause(1700);
  const video = page.video();
  await context.close();
  execFileSync('ffmpeg', ['-y', '-loglevel', 'error', '-ss', String(trim), '-i', await video.path(),
    '-an', '-c:v', 'libx264', '-crf', '21', '-pix_fmt', 'yuv420p', '-movflags', '+faststart', output + '/annotate-diagram.mp4']);
  execFileSync('cwebp', ['-quiet', '.cache/drawing-demo/poster.png', '-o', output + '/annotate-diagram.webp']);
} finally {
  await browser?.close();
  await server.close();
  await fixture.close();
}
