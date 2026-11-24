// Runs scenarios: one Vite server for the web client (apps/web, as the UI
// suites run it), a fresh fake Runner and browser context per scenario, the
// steps in order with their checks, and — unless it runs as a test — the
// recording, its encoding, a contact sheet and a report.
import { createServer as createHttpServer, request, type IncomingMessage, type ServerResponse } from 'node:http';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { basename, join } from 'node:path';
import { mkdir, readdir, writeFile } from 'node:fs/promises';
import { createServer, type ViteDevServer } from 'vite';
import react from '@vitejs/plugin-react';
import { chromium, devices, type Browser, type BrowserContext, type Page } from '../../scripts/fixtures/playwright.mjs';
import { startRunnerFixture, type RunnerFixture } from '../../scripts/fixtures/runner-ui.mjs';
import { testViteCache } from '../../scripts/fixtures/vite-cache.mjs';
import { collectPageErrors } from '../../scripts/fixtures/page-errors.mjs';
import { ScenarioClock } from './clock.ts';
import { World, ConversationHandle, WorkspaceHandle } from './world.ts';
import { Overlay, installOverlay, type PointerStyle } from './overlay.ts';
import { FrameRecorder, contactSheet, describeVideo, findFfmpeg, savePlaywrightVideo, sha256 } from './recorder.ts';
import type { Scenario, Theme, Viewport } from './dsl.ts';
import type { Step, StepContext } from './step.ts';

const here = (path: string) => fileURLToPath(new URL(path, import.meta.url));
export const SCENARIOS_DIR = here('../');
export const OUTPUT_DIR = here('../../.cache/scenarios/');

export const DESKTOP = { width: 1440, height: 900 };
export const PHONE = { width: 390, height: 844 };

export interface RunOptions {
  /** Record a video; false runs the scenario as a fast end-to-end test. */
  video: boolean;
  gif: boolean;
  webm: boolean;
  phone?: boolean;
  theme?: Theme;
  /** Device pixels per CSS pixel in the recording (default 2). */
  scale: number;
  /** Per-assertion wait, ms. */
  timeout: number;
}

export interface StepReport { index: number; kind: string; label: string; status: 'passed' | 'failed' | 'skipped'; atMs?: number; durationMs?: number; checks: { description: string; passed: boolean }[]; error?: string }
export interface ScenarioReport {
  name: string; title: string; story: string; tags: string[]; viewport: { width: number; height: number }; theme: Theme; mode: 'video' | 'test';
  status: 'passed' | 'failed'; durationMs: number; steps: StepReport[]; checks: number; pageErrors: string[];
  outputs: { file: string; bytes: number; seconds?: number }[]; keyframes: { label: string; sha256: string }[]; error?: string;
}

/** Every scenario in scenarios, by name (the file name without `.scenario.ts`). */
export async function listScenarios(): Promise<Map<string, string>> {
  const files = (await readdir(SCENARIOS_DIR)).filter(f => f.endsWith('.scenario.ts')).sort();
  return new Map(files.map(f => [basename(f, '.scenario.ts'), join(SCENARIOS_DIR, f)]));
}
export async function loadScenario(file: string): Promise<Scenario> {
  const module = await import(pathToFileURL(file).href) as { default?: Scenario };
  if (module.default?.kind !== 'scenario') throw new Error(`${file} must export default scenario({...})`);
  return module.default;
}

// ---- Servers ----------------------------------------------------------------

export class Stage {
  current = { main: '' };
  private forwarder = createHttpServer((req, res) => this.forward(req, res));
  vite?: ViteDevServer;
  browser?: Browser;
  origin = '';
  private forward(req: IncomingMessage, res: ServerResponse) {
    const upstream = new URL(this.current.main);
    const proxied = request({ host: upstream.hostname, port: upstream.port, path: req.url, method: req.method, headers: req.headers }, answer => { res.writeHead(answer.statusCode || 502, answer.headers); answer.pipe(res); });
    proxied.on('error', () => { if (!res.headersSent) res.writeHead(502); res.end(); });
    req.pipe(proxied);
  }
  async start() {
    await new Promise<void>(done => this.forwarder.listen(0, '127.0.0.1', done));
    const address = this.forwarder.address();
    const target = `http://127.0.0.1:${typeof address === 'object' && address ? address.port : 0}`;
    this.vite = await createServer({ ...testViteCache(new URL('scenarios.mjs', import.meta.url).href), configFile: false, logLevel: 'error', root: here('../../apps/web'), plugins: [react()],
      server: { host: '127.0.0.1', port: 0, proxy: { '/lazybox.bench.v1.RunnerService/': { target }, '/remote/v1/': { target } } } });
    await this.vite.listen();
    const vite = this.vite.httpServer?.address();
    this.origin = `http://127.0.0.1:${typeof vite === 'object' && vite ? vite.port : 0}`;
    this.browser = await chromium.launch({ headless: true });
  }
  async stop() { await this.browser?.close(); await this.vite?.close(); this.forwarder.close(); }
}

// ---- One scenario -----------------------------------------------------------

const viewportOf = (v: Viewport | undefined, phone: boolean) => phone || v === 'phone' ? PHONE : v && typeof v === 'object' ? v : DESKTOP;
const seeded = (seed: string) => { let h = 2166136261; for (const c of seed) h = Math.imul(h ^ c.charCodeAt(0), 16777619); return () => { h += 0x6d2b79f5; let t = h; t = Math.imul(t ^ (t >>> 15), t | 1); t ^= t + Math.imul(t ^ (t >>> 7), t | 61); return ((t ^ (t >>> 14)) >>> 0) / 4294967296; }; };

/** Ready for the camera: fonts loaded, and no request started for 500 ms (open streams aside). */
async function settle(page: Page) {
  await page.evaluate(() => document.fonts.ready).catch(() => {});
  const started = performance.now();
  for (;;) {
    const quiet = await page.evaluate(() => performance.now() - Math.max(0, ...performance.getEntriesByType('resource').map(e => e.startTime))).catch(() => 1000);
    if (quiet >= 500 || performance.now() - started > 6000) break;
    await page.waitForTimeout(Math.max(80, 500 - quiet));
  }
}

export async function runScenario(stage: Stage, name: string, scenario: Scenario, options: RunOptions): Promise<ScenarioReport> {
  const browser = stage.browser!;
  const phone = !!options.phone || scenario.viewport === 'phone';
  const viewport = viewportOf(scenario.viewport, !!options.phone);
  const theme: Theme = options.theme || scenario.theme || 'dark';
  const variant = `${name}${options.phone && scenario.viewport !== 'phone' ? '-phone' : ''}${options.theme && options.theme !== (scenario.theme || 'dark') ? `-${options.theme}` : ''}`;
  await mkdir(OUTPUT_DIR, { recursive: true });
  const report: ScenarioReport = { name: variant, title: scenario.title, story: `As ${scenario.story.as}, I want ${scenario.story.want}, so that ${scenario.story.soThat}.`, tags: scenario.tags || [], viewport, theme, mode: options.video ? 'video' : 'test',
    status: 'passed', durationMs: 0, steps: [], checks: 0, pageErrors: [], outputs: [], keyframes: [] };
  const started = performance.now();
  const clock = new ScenarioClock();
  const fixture: RunnerFixture = await startRunnerFixture();
  stage.current.main = fixture.url;
  const ffmpeg = options.video ? findFfmpeg() : undefined;
  let context: BrowserContext | undefined;
  let recorder: FrameRecorder | undefined;
  try {
    const world = new World(fixture, clock);
    const steps = await scenario.build(world);
    if (world.phone) Object.assign(fixture.remote, { tailscale: { ...fixture.remote.tailscale, enabled: true, state: 'serving' }, phoneUrl: stage.origin });
    const scale = options.video ? options.scale : 1;
    context = await browser.newContext({
      viewport, deviceScaleFactor: scale, colorScheme: theme, timezoneId: 'UTC', locale: 'en-US',
      ...(phone ? { userAgent: devices['iPhone 13'].userAgent, hasTouch: true, isMobile: false } : {}),
      ...(options.video && !ffmpeg ? { recordVideo: { dir: join(OUTPUT_DIR, '.playwright-video'), size: viewport } } : {}),
    });
    // tsx compiles with esbuild's keepNames: a function sent to the page may call its __name helper.
    await context.addInitScript({ content: 'globalThis.__name = globalThis.__name || (fn => fn);' });
    await context.addInitScript(() => performance.setResourceTimingBufferSize(100_000));
    for (const script of world.initScripts) await context.addInitScript(script);
    await context.addInitScript(installOverlay, { pointer: (phone ? 'touch' : 'arrow') as PointerStyle, compact: phone });
    const page = await context.newPage();
    page.setDefaultTimeout(options.timeout);
    let during = 'opening the app';
    await collectPageErrors(page, [], { onError: (message, stack) => { report.pageErrors.push(`${message.split('\n')[0]} (during: ${during})`); if (process.env.SCENARIO_DEBUG) console.error(stack || message); } });
    if (process.env.SCENARIO_DEBUG) page.on('console', message => { if (message.type() === 'error') console.error('console:', message.text().slice(0, 2000)); });
    // The page's clock is the scenario's (SCENARIO_REAL_CLOCK=1 leaves it real, to tell a clock artefact from a bug).
    const syncPage = () => process.env.SCENARIO_REAL_CLOCK ? Promise.resolve() : page.clock.setFixedTime(clock.now);
    await syncPage();
    const overlay = new Overlay(page, options.video, viewport);
    const random = seeded(name);
    const withToken = (path: string) => world.phone ? path : `${path}${path.includes('#') ? '&' : '#'}token=${fixture.token}`;

    // Open where the scenario starts, before the camera rolls.
    const opened = world.opened;
    if (opened === 'phone') await pairPhone(page, fixture);
    else {
      const path = opened instanceof ConversationHandle ? (() => { const s = fixture.sessions.find(v => v.id === opened.ref.id)!; return `/?view=workspace&collection=${s.collectionId}&session=${s.id}`; })()
        : opened instanceof WorkspaceHandle ? `/?view=workspace&collection=${opened.id}` : '/?view=workspace';
      await page.goto(stage.origin + withToken(path));
    }
    await page.locator('#root > *').first().waitFor({ timeout: options.timeout });
    await settle(page);
    await overlay.park();

    const out = (ext: string) => join(OUTPUT_DIR, `${variant}.${ext}`);
    if (options.video && ffmpeg) { recorder = new FrameRecorder(context, page, viewport, scale, ffmpeg, out('mp4')); await recorder.start(); }
    const sleep = (ms: number) => new Promise(resolve => setTimeout(resolve, ms));
    let checks: StepReport['checks'] = [];
    let captionOpen: string | undefined;
    const keyframes: { image: Buffer; label: string }[] = [];
    const keyframe = async (label: string) => {
      if (!options.video) return;
      if (recorder) await sleep(60);
      const image = recorder?.latest()?.image || await page.screenshot({ type: 'jpeg', quality: 90 });
      keyframes.push({ image, label }); report.keyframes.push({ label, sha256: sha256(image) });
    };
    const ctx: StepContext = {
      page, fixture, world, clock, overlay, video: options.video, random, timeout: options.timeout,
      async pause(seconds) {
        let left = Math.max(0, seconds * 1000);
        if (!options.video) { clock.advance(left); await syncPage(); return; }
        while (left > 0) { const chunk = Math.min(1000, left); await sleep(chunk); clock.advance(chunk); left -= chunk; await syncPage(); }
      },
      async hold(seconds) {
        let left = Math.max(0, seconds * 1000);
        while (left > 0) { const chunk = Math.min(1000, left); await sleep(chunk); clock.advance(chunk); left -= chunk; await syncPage(); }
      },
      async beat(seconds) { if (options.video && seconds > 0) await sleep(seconds * 1000); },
      async check(description, assertion) {
        try { await assertion(); checks.push({ description, passed: true }); }
        catch (error) { checks.push({ description, passed: false }); throw new Error(`${description}: ${String((error as Error).message || error).split('\n').find(Boolean)}`); }
      },
      async nudge() { await page.evaluate(() => dispatchEvent(new Event('focus'))).catch(() => {}); },
      async goto(path) { await page.goto(stage.origin + withToken(path)); await page.locator('#root > *').first().waitFor(); await overlay.restore(); await overlay.park(); },
    };

    let failed: Error | undefined;
    for (const [index, item] of steps.entries()) {
      if (failed) { report.steps.push({ index, kind: item.kind, label: item.label, status: 'skipped', checks: [] }); continue; }
      checks = []; during = `step ${index + 1}, ${item.label}`;
      if (process.env.SCENARIO_DEBUG) console.error(`${name}: ${during}`);
      if (item.caption) { if (captionOpen) await keyframe(captionOpen); captionOpen = item.caption; }
      const at = recorder ? Math.round(recorder.elapsed()) : undefined, t0 = performance.now();
      try {
        await item.run(ctx);
        await syncPage();
        report.steps.push({ index, kind: item.kind, label: item.label, status: 'passed', atMs: at, durationMs: Math.round(performance.now() - t0), checks });
      } catch (error) {
        failed = error as Error;
        report.steps.push({ index, kind: item.kind, label: item.label, status: 'failed', atMs: at, durationMs: Math.round(performance.now() - t0), checks, error: String(failed.message).split('\n')[0] });
        await page.screenshot({ path: join(OUTPUT_DIR, `${variant}-failed.png`) }).catch(() => {});
        // What a locator sees, to fix the step: the page's accessibility tree.
        await writeFile(join(OUTPUT_DIR, `${variant}-failed.aria.yaml`), await page.locator('body').ariaSnapshot().catch(() => '')).catch(() => {});
      }
    }
    if (!failed && options.video) { await ctx.beat(1.4); if (captionOpen) await keyframe(captionOpen); else await keyframe('The end'); }
    report.checks = report.steps.reduce((n, s) => n + s.checks.length, 0);
    if (failed) { report.status = 'failed'; report.error = failed.message.split('\n')[0]; }
    else if (report.pageErrors.length) { report.status = 'failed'; report.error = `the page threw: ${report.pageErrors[0]}`; }

    // The recording, its files and the contact sheet.
    if (recorder && failed) { await recorder.abort(); recorder = undefined; }
    else if (recorder) {
      await recorder.stop();
      await recorder.derive({ ...(options.webm ? { webm: out('webm') } : {}), ...(options.gif ? { gif: out('gif') } : {}) });
      for (const ext of ['mp4', ...(options.webm ? ['webm'] : []), ...(options.gif ? ['gif'] : [])]) report.outputs.push(await describeVideo(out(ext)));
    } else if (options.video) {
      const file = join(OUTPUT_DIR, `${variant}.webm`);
      await context.close(); context = undefined;
      if (await savePlaywrightVideo(page, file)) report.outputs.push(await describeVideo(file));
    }
    if (options.video && keyframes.length && !failed) {
      const sheet = out('contact.png');
      await contactSheet(browser, sheet, { title: scenario.title, story: report.story, dark: theme === 'dark', columns: phone ? 4 : 3, frames: keyframes });
      report.outputs.push(await describeVideo(sheet).catch(() => ({ file: sheet, bytes: 0 })));
    }
  } catch (error) {
    await recorder?.abort();
    report.status = 'failed'; report.error = String((error as Error).message || error).split('\n')[0];
  } finally {
    await context?.close().catch(() => {});
    await fixture.close();
    report.durationMs = Math.round(performance.now() - started);
    await writeFile(join(OUTPUT_DIR, `${variant}.report.json`), JSON.stringify(report, null, 2) + '\n');
  }
  return report;
}

/** Pairs the page as a phone (REMOTE.md) and opens its Inbox, before the video starts. */
async function pairPhone(page: Page, fixture: RunnerFixture) {
  const response = await fetch(`${fixture.url}/lazybox.bench.v1.RunnerService/BeginDevicePairing`, { method: 'POST', headers: { authorization: `Bearer ${fixture.token}`, 'content-type': 'application/json' }, body: JSON.stringify({ grants: ['respond', 'start'] }) });
  if (!response.ok) throw new Error(`BeginDevicePairing: ${response.status} ${await response.text()}`);
  const offer = await response.json() as { url: string };
  await page.goto(offer.url);
  await page.getByRole('textbox', { name: 'Name this phone' }).fill('iPhone');
  await page.getByRole('button', { name: 'Pair this phone' }).click();
  await page.getByRole('button', { name: 'Open inbox' }).click();
  await page.getByRole('heading', { name: 'Inbox' }).waitFor();
  // The phone asks once about notifications; answered "Not now" before the story starts.
  await page.getByRole('complementary', { name: 'Notifications' }).getByRole('button', { name: /Not now|Dismiss|Later/ }).click({ timeout: 2000 }).catch(() => {});
}
