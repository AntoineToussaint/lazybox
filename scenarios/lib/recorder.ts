// Recording: frames of the page at device scale (2× by default, so text is
// crisp), captured as fast as the browser renders them (about 20 per second
// at 2880×1800), each with its time, and streamed straight into ffmpeg on a
// 30 fps timeline: nothing is written to disk but the MP4. The WebM and the
// GIF are made from it afterwards. Playwright's own video records at 1× with a
// low bitrate, so it is only the fallback when ffmpeg is missing.
import { spawn, spawnSync, type ChildProcess } from 'node:child_process';
import { writeFile, rename, stat } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import type { Browser, BrowserContext, CDPSession, Page } from '@playwright/test';

export type Frame = { image: Buffer; at: number };
const FPS = 30;

/** The system's ffmpeg, if there is one. */
export function findFfmpeg(): string | undefined {
  const probe = spawnSync(process.env.FFMPEG || 'ffmpeg', ['-hide_banner', '-version'], { encoding: 'utf8' });
  return probe.status === 0 ? process.env.FFMPEG || 'ffmpeg' : undefined;
}

export class FrameRecorder {
  /** Distinct frames captured. */
  frames = 0;
  private cdp?: CDPSession;
  private running = false;
  private loop?: Promise<void>;
  private started = 0;
  private encoder?: ChildProcess;
  private encoded?: Promise<void>;
  private last?: Frame;
  private written = 0;
  constructor(private context: BrowserContext, private page: Page, private viewport: { width: number; height: number }, private scale: number, private ffmpeg: string, readonly mp4: string) {}

  async start() {
    const even = 'scale=trunc(iw/2)*2:trunc(ih/2)*2';
    this.encoder = spawn(this.ffmpeg, ['-hide_banner', '-loglevel', 'error', '-y', '-f', 'image2pipe', '-framerate', String(FPS), '-c:v', 'mjpeg', '-i', '-',
      '-vf', `${even},format=yuv420p`, '-c:v', 'libx264', '-preset', 'medium', '-crf', '18', '-movflags', '+faststart', this.mp4], { stdio: ['pipe', 'ignore', 'pipe'] });
    let errors = '';
    this.encoder.stderr!.on('data', chunk => { errors += chunk; });
    this.encoder.stdin!.on('error', () => {});
    this.encoded = new Promise((resolve, reject) => this.encoder!.on('close', code => code === 0 ? resolve() : reject(new Error(`ffmpeg exited with ${code}: ${errors.trim().split('\n').slice(-2).join(' | ')}`))));
    await this.attach();
    this.running = true; this.started = performance.now();
    const clip = { x: 0, y: 0, width: this.viewport.width, height: this.viewport.height, scale: 1 };
    this.loop = (async () => {
      while (this.running) {
        const before = performance.now();
        try {
          // A capture never answered (it happens across a reload) must not stall the recording.
          const capture = this.cdp!.send('Page.captureScreenshot', { format: 'jpeg', quality: 92, clip, optimizeForSpeed: true } as never) as Promise<{ data: string }>;
          const answer = await Promise.race([capture, new Promise<undefined>(resolve => setTimeout(resolve, 2500))]);
          if (!answer) { capture.catch(() => {}); if (this.running) await this.attach(); continue; }
          this.add({ image: Buffer.from(answer.data, 'base64'), at: (before + performance.now()) / 2 - this.started });
        } catch {
          if (!this.running) break;
          // A navigation can drop one capture; the next one follows.
          await new Promise(resolve => setTimeout(resolve, 30));
        }
      }
    })();
  }
  /**
   * A CDP session of our own. It sees the page at 1× unless it asks for the
   * device metrics Playwright emulates; then a capture is at device pixels.
   */
  private async attach() {
    await this.cdp?.detach().catch(() => {});
    this.cdp = await this.context.newCDPSession(this.page);
    await this.cdp.send('Emulation.setDeviceMetricsOverride', { width: this.viewport.width, height: this.viewport.height, deviceScaleFactor: this.scale, mobile: false });
  }
  /** Puts a frame on the 30 fps timeline: the previous one fills every slot until this one's time. */
  private add(frame: Frame) {
    this.fill(frame.at);
    this.last = frame; this.frames++;
  }
  private fill(until: number) {
    if (!this.last) return;
    // Duplicates share one buffer: the pipe holds references, not copies.
    while (this.written * 1000 / FPS < until) { this.encoder!.stdin!.write(this.last.image); this.written++; }
  }
  /** Milliseconds since recording started. */
  elapsed() { return this.started ? performance.now() - this.started : 0; }
  /** The newest frame (a key frame of the contact sheet). */
  latest(): Frame | undefined { return this.last; }
  /** Stops capturing, holds the last frame `endHold` seconds, and finishes the MP4. */
  async stop(endHold = 1.2) {
    this.running = false;
    await this.loop;
    await this.cdp?.detach().catch(() => {});
    if (!this.last) { this.encoder?.kill(); throw new Error('no frames were recorded'); }
    this.fill(this.elapsed() + endHold * 1000);
    this.encoder!.stdin!.end();
    await this.encoded;
  }
  /** Stops without a video (the scenario failed). */
  async abort() { this.running = false; await this.loop; await this.cdp?.detach().catch(() => {}); this.encoder?.kill(); await this.encoded?.catch(() => {}); }

  /** The WebM and the GIF, made from the MP4. */
  async derive(outputs: { webm?: string; gif?: string }) {
    const source = ['-hide_banner', '-loglevel', 'error', '-y', '-i', this.mp4];
    if (outputs.webm) await run(this.ffmpeg, [...source, '-c:v', 'libvpx-vp9', '-b:v', '0', '-crf', '34', '-row-mt', '1', '-deadline', 'good', '-cpu-used', '5', outputs.webm]);
    if (outputs.gif) await run(this.ffmpeg, [...source, '-vf', 'fps=12,scale=1200:-2:flags=lanczos,split[a][b];[a]palettegen=max_colors=200:stats_mode=diff[p];[b][p]paletteuse=dither=sierra2_4a:diff_mode=rectangle', outputs.gif]);
  }
}

function run(command: string, args: string[]) {
  return new Promise<void>((resolve, reject) => {
    const child = spawn(command, args, { stdio: ['ignore', 'ignore', 'pipe'] });
    let errors = '';
    child.stderr.on('data', chunk => { errors += chunk; });
    child.on('error', reject);
    child.on('close', code => code === 0 ? resolve() : reject(new Error(`${command} exited with ${code}: ${errors.trim().split('\n').slice(-3).join(' | ')}`)));
  });
}

/** Video length and size of an encoded file (ffprobe), for the report. */
export async function describeVideo(file: string): Promise<{ file: string; bytes: number; seconds?: number }> {
  const bytes = (await stat(file)).size;
  const probe = spawnSync('ffprobe', ['-v', 'error', '-show_entries', 'format=duration', '-of', 'default=nw=1:nk=1', file], { encoding: 'utf8' });
  const seconds = probe.status === 0 ? Number(probe.stdout.trim()) : undefined;
  return { file, bytes, ...(seconds ? { seconds: Math.round(seconds * 10) / 10 } : {}) };
}

/** Moves Playwright's own recording (the fallback) to its place, once its context closed. */
export async function savePlaywrightVideo(page: Page, file: string) {
  const video = page.video();
  if (!video) return undefined;
  await rename(await video.path(), file);
  return file;
}

export const sha256 = (data: Buffer) => createHash('sha256').update(data).digest('hex').slice(0, 16);

/**
 * The contact sheet: the key frames in a grid, each under its caption, with
 * the story on top. Drawn on a canvas in the browser (no image tools).
 */
export async function contactSheet(browser: Browser, file: string, input: { title: string; story: string; frames: { image: Buffer; label: string }[]; dark: boolean; columns?: number }) {
  if (!input.frames.length) return;
  const page = await browser.newPage();
  try {
    const png = await page.evaluate(async ({ title, story, frames, dark, columns }) => {
      const images = await Promise.all(frames.map(async f => { const image = new Image(); image.src = 'data:image/jpeg;base64,' + f.image; await image.decode(); return { image, label: f.label }; }));
      const first = images[0].image, cols = Math.min(columns, images.length), tileW = first.naturalWidth > first.naturalHeight ? 960 : 390;
      const tileH = Math.round(tileW * first.naturalHeight / first.naturalWidth), pad = 36, gap = 28, labelH = 64, head = 132;
      const rows = Math.ceil(images.length / cols);
      const canvas = document.createElement('canvas');
      canvas.width = pad * 2 + cols * tileW + (cols - 1) * gap; canvas.height = head + pad + rows * (tileH + labelH) + (rows - 1) * gap;
      const g = canvas.getContext('2d')!;
      g.fillStyle = dark ? '#111214' : '#f4f5f7'; g.fillRect(0, 0, canvas.width, canvas.height);
      g.fillStyle = dark ? '#f2f3f5' : '#14161a'; g.font = '600 34px -apple-system, system-ui, sans-serif'; g.fillText(title, pad, 62);
      g.fillStyle = dark ? '#9aa0aa' : '#5b616b'; g.font = '400 20px -apple-system, system-ui, sans-serif'; g.fillText(story, pad, 100, canvas.width - pad * 2);
      images.forEach(({ image, label }, i) => {
        const x = pad + (i % cols) * (tileW + gap), y = head + Math.floor(i / cols) * (tileH + labelH + gap);
        g.save(); g.beginPath(); g.roundRect(x, y, tileW, tileH, 12); g.clip(); g.drawImage(image, x, y, tileW, tileH); g.restore();
        g.strokeStyle = dark ? 'rgba(255,255,255,0.12)' : 'rgba(0,0,0,0.12)'; g.lineWidth = 1; g.beginPath(); g.roundRect(x + 0.5, y + 0.5, tileW - 1, tileH - 1, 12); g.stroke();
        g.fillStyle = dark ? '#c9ccd2' : '#2a2e35'; g.font = '500 18px -apple-system, system-ui, sans-serif';
        const text = `${i + 1}. ${label}`;
        let line = '', lines: string[] = [];
        for (const word of text.split(' ')) { const next = line ? line + ' ' + word : word; if (g.measureText(next).width > tileW) { lines.push(line); line = word; } else line = next; }
        lines.push(line); lines = lines.slice(0, 2);
        lines.forEach((l, k) => g.fillText(l, x, y + tileH + 26 + k * 24));
      });
      return canvas.toDataURL('image/png').split(',')[1];
    }, { title: input.title, story: input.story, dark: input.dark, columns: input.columns || 3, frames: input.frames.map(f => ({ image: f.image.toString('base64'), label: f.label })) });
    await writeFile(file, Buffer.from(png, 'base64'));
  } finally { await page.close(); }
}
