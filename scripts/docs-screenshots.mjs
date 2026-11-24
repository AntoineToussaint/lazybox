// Docs screenshots: every image in docs/images/, taken from the real web
// client (apps/web through Vite, as the test-*.mjs suites run it) against the
// explicitly fake Runner (scripts/fixtures/runner-ui.mjs), so they are the
// same on any machine, without credentials, Git or a model. What each image
// shows is in scripts/docs-shots.mjs.
//
//   npm run docs:screenshots                          # every image, dark and light
//   npm run docs:screenshots hero inbox               # only these shots
//   npm run docs:screenshots -- --variants=dark       # only the dark theme
//   npm run docs:screenshots -- --variants=light,narrow
//   npm run docs:screenshots -- --out=audits/2026-11-16/shots
//
// --variants takes themes (dark, light) and sizes (desktop, narrow); a group
// left out means all of it. --out writes elsewhere than docs/images/ (an audit's
// own folder, say). Each image gets a fresh fake Runner and a fresh browser
// context, a fixed clock and time zone, no animation, and is written as a
// WebP encoded by Chromium itself (no image tools needed). Shots run one at a
// time in one browser. A failed shot leaves .cache/docs-shot-failed-<file>.png.
import { createServer as createHttpServer, request } from 'node:http';
import { fileURLToPath } from 'node:url';
import { resolve } from 'node:path';
import { mkdir, writeFile } from 'node:fs/promises';
import { createServer } from 'vite';
import react from '@vitejs/plugin-react';
import { chromium } from '@playwright/test';
import { startRunnerFixture } from './fixtures/runner-ui.mjs';
import { testViteCache } from './fixtures/vite-cache.mjs';
import { shots, CLOCK, TIMEZONE, DESKTOP, NARROW, settle } from './docs-shots.mjs';
import { collectPageErrors } from './fixtures/page-errors.mjs';

// ---- Arguments ---------------------------------------------------------------

const args = process.argv.slice(2);
const option = name => args.find(a => a.startsWith(`--${name}=`))?.slice(name.length + 3);
const wanted = args.filter(a => !a.startsWith('--'));
const unknown = wanted.filter(name => !shots[name]);
if (unknown.length) { console.error(`Unknown shot: ${unknown.join(', ')}. Known: ${Object.keys(shots).join(', ')}`); process.exit(2); }
const variants = (option('variants') || '').split(',').filter(Boolean);
const badVariant = variants.filter(v => !['dark', 'light', 'desktop', 'narrow'].includes(v));
if (badVariant.length) { console.error(`Unknown variant: ${badVariant.join(', ')}. Known: dark, light, desktop, narrow`); process.exit(2); }
const themes = ['dark', 'light'].filter(t => !variants.some(v => v === 'dark' || v === 'light') || variants.includes(t));
const sizes = ['desktop', 'narrow'].filter(s => !variants.some(v => v === 'desktop' || v === 'narrow') || variants.includes(s));
const out = option('out') ? resolve(option('out')) + '/' : fileURLToPath(new URL('../docs/images/', import.meta.url));

// ---- A fixed clock -----------------------------------------------------------

// The fake Runner stamps what it records with Date.now(); every shot starts at
// CLOCK, and the page's clock stands still there, so "12 min ago", a reset
// time or a timeline reads the same on every run.
const realNow = Date.now.bind(Date);
let clockOffset = 0;
Date.now = () => realNow() + clockOffset;
const resetClock = () => { clockOffset = CLOCK - realNow(); };
/** For a shot's setup: what the fake Runner records next happened `ms` before the picture. */
const clock = { ago: ms => { clockOffset = CLOCK - ms - realNow(); } };

// ---- The servers -------------------------------------------------------------

// One Vite server for every shot. It forwards the Runner API to the shot's
// fake Runner, other Runners' paths to theirs, and lists the Runners when a
// shot has several (as the desktop host does; MULTI_RUNNER.md).
let current = { main: '', runners: undefined, others: new Map() };
const forward = (req, res, target, path) => {
  const upstream = new URL(target);
  const proxied = request({ host: upstream.hostname, port: upstream.port, path, method: req.method, headers: req.headers }, answer => { res.writeHead(answer.statusCode, answer.headers); answer.pipe(res); });
  proxied.on('error', () => { if (!res.headersSent) res.writeHead(502); res.end(); });
  req.pipe(proxied);
};
const forwarder = createHttpServer((req, res) => {
  const other = req.url.match(/^\/__lazybox\/runner\/([^/]+)(\/.*)$/);
  if (!other) return forward(req, res, current.main, req.url);
  const fixture = current.others.get(other[1]);
  if (!fixture) { res.writeHead(404); res.end(); return; }
  forward(req, res, fixture.url, other[2]);
});
await new Promise(done => forwarder.listen(0, '127.0.0.1', done));
const forwarderUrl = `http://127.0.0.1:${forwarder.address().port}`;
const runnerList = { name: 'runner-list', configureServer(server) {
  server.middlewares.use('/__lazybox/runners', (_req, res) => {
    if (!current.runners) { res.statusCode = 404; res.end(); return; }
    res.setHeader('Content-Type', 'application/json'); res.end(JSON.stringify({ runners: current.runners }));
  });
} };


const server = await createServer({ ...testViteCache(import.meta.url), configFile: false, logLevel: 'error', root: fileURLToPath(new URL('../apps/web', import.meta.url)), plugins: [react(), runnerList],
  server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target: forwarderUrl }, '/remote/v1/': { target: forwarderUrl }, '/__lazybox/runner/': { target: forwarderUrl } } } });
await server.listen();
const origin = `http://127.0.0.1:${server.httpServer.address().port}`;
const browser = await chromium.launch({ headless: true });
await mkdir(out, { recursive: true });

// ---- Pictures ----------------------------------------------------------------

// Stills, not motion: no transitions, animations or blinking carets.
const still = '*,*::before,*::after{transition:none!important;animation:none!important;caret-color:transparent!important}';
/** Every page of a shot: the still style, the fixed clock. */
async function prepare(page) {
  await page.clock.setFixedTime(CLOCK);
  // settle() reads when the page last asked for something: keep every entry.
  await page.addInitScript(() => performance.setResourceTimingBufferSize(100_000));
  await page.addInitScript(css => { addEventListener('DOMContentLoaded', () => { const s = document.createElement('style'); s.textContent = css; document.head.append(s); }); }, still);
}

const encoder = await browser.newPage();
const toWebp = (png, quality = 0.8) => encoder.evaluate(async ({ source, quality }) => {
  const image = new Image(); image.src = 'data:image/png;base64,' + source; await image.decode();
  const canvas = document.createElement('canvas'); canvas.width = image.naturalWidth; canvas.height = image.naturalHeight;
  canvas.getContext('2d').drawImage(image, 0, 0);
  return canvas.toDataURL('image/webp', quality).split(',')[1];
}, { source: png.toString('base64'), quality }).then(data => Buffer.from(data, 'base64'));
/** Phone screens side by side, rounded like the device, on a transparent ground. */
const compose = (pngs, scheme) => encoder.evaluate(async ({ sources, scheme }) => {
  const images = await Promise.all(sources.map(async source => { const image = new Image(); image.src = 'data:image/png;base64,' + source; await image.decode(); return image; }));
  const scale = 2, pad = 40 * scale, gap = 48 * scale, radius = 44 * scale, w = images[0].naturalWidth, h = images[0].naturalHeight;
  const canvas = document.createElement('canvas'); canvas.width = pad * 2 + images.length * w + (images.length - 1) * gap; canvas.height = pad * 2 + h;
  const g = canvas.getContext('2d');
  images.forEach((image, i) => {
    const x = pad + i * (w + gap), y = pad;
    g.save(); g.shadowColor = scheme === 'dark' ? 'rgba(0,0,0,0.45)' : 'rgba(15,23,42,0.18)'; g.shadowBlur = 28 * scale; g.shadowOffsetY = 8 * scale;
    g.beginPath(); g.roundRect(x, y, w, h, radius); g.fillStyle = scheme === 'dark' ? '#000' : '#fff'; g.fill(); g.restore();
    g.save(); g.beginPath(); g.roundRect(x, y, w, h, radius); g.clip(); g.drawImage(image, x, y); g.restore();
    g.beginPath(); g.roundRect(x + 0.5, y + 0.5, w - 1, h - 1, radius); g.lineWidth = 2; g.strokeStyle = scheme === 'dark' ? 'rgba(255,255,255,0.14)' : 'rgba(15,23,42,0.14)'; g.stroke();
  });
  return canvas.toDataURL('image/png').split(',')[1];
}, { sources: pngs.map(png => png.toString('base64')), scheme }).then(data => Buffer.from(data, 'base64'));

// ---- The run -----------------------------------------------------------------

const jobs = (wanted.length ? wanted : Object.keys(shots)).flatMap(name => {
  const entry = shots[name];
  return sizes.filter(size => size === 'desktop' || entry.narrow).flatMap(size => themes.map(scheme => ({
    name, entry, scheme, viewport: size === 'narrow' ? NARROW : entry.viewport || DESKTOP,
    file: `${name}${size === 'narrow' ? '-narrow' : ''}${scheme === 'light' ? '-light' : ''}.webp`,
  })));
});
const failed = [];
let total = 0;
for (const { name, entry, scheme, viewport, file } of jobs) {
  resetClock();
  const f = await startRunnerFixture();
  const others = [];
  current = { main: f.url, runners: undefined, others: new Map() };
  const context = await browser.newContext({ viewport, deviceScaleFactor: 1, reducedMotion: 'reduce', colorScheme: scheme, timezoneId: TIMEZONE, locale: 'en-US' });
  const errors = [];
  let page;
  try {
    const open = async (path, { runners } = {}) => {
      resetClock();
      current.runners = runners;
      const next = await context.newPage(); next.setDefaultTimeout(15000);
      await collectPageErrors(next, errors);
      await prepare(next);
      await next.goto(`${origin}${path}#token=${f.token}`);
      return next;
    };
    const addRunner = async id => { const other = await startRunnerFixture(); others.push(other); current.others.set(id, other); return other; };
    /** One call to the fake Runner, as the app would make it. */
    const rpc = async (method, body = {}) => {
      const response = await fetch(`${f.url}/lazybox.bench.v1.RunnerService/${method}`, { method: 'POST', headers: { authorization: `Bearer ${f.token}`, 'content-type': 'application/json' }, body: JSON.stringify(body) });
      if (!response.ok) throw new Error(`${method}: ${response.status} ${await response.text()}`);
      return response.json();
    };
    const result = await entry.run({ f, open, addRunner, rpc, clock, context, browser, scheme, viewport, origin, prepare, compose });
    let png;
    if (result?.png) png = result.png;
    else {
      page = result;
      // Nothing hovered, nothing focused by accident (a shot that wants focus keeps it: the caret is hidden).
      await page.mouse.move(-10, -10);
      await settle(page);
      if (process.env.DOCS_SNAPSHOT) console.log(await page.locator('body').ariaSnapshot());
      if (process.env.DOCS_EVAL) console.log(await page.evaluate(process.env.DOCS_EVAL));
      png = await page.screenshot({ type: 'png' });
    }
    const webp = await toWebp(png);
    await writeFile(out + file, webp);
    total += webp.length;
    console.log(`${file.padEnd(34)} ${(webp.length / 1024).toFixed(0).padStart(4)} KB${errors.length ? `  (page errors: ${errors.join(' | ')})` : ''}`);
  } catch (error) {
    failed.push(file);
    console.error(`FAILED ${file}: ${error.message.split('\n')[0]}`);
    // What the page showed when it failed, to fix the shot.
    const last = context.pages().at(-1);
    if (last) {
      await mkdir('.cache', { recursive: true });
      await last.screenshot({ path: `.cache/docs-shot-failed-${file.replace(/\.webp$/, '')}.png` }).catch(() => {});
      if (process.env.DOCS_SNAPSHOT) console.error(await last.locator('body').ariaSnapshot().catch(() => ''));
    }
  } finally {
    await context.close(); await f.close();
    for (const other of others) await other.close();
  }
}
await browser.close(); await server.close(); forwarder.close();
console.log(`${jobs.length - failed.length} of ${jobs.length} images, ${(total / 1024).toFixed(0)} KB, in ${out}`);
if (failed.length) { console.error(`Failed: ${failed.join(', ')}`); process.exit(1); }
