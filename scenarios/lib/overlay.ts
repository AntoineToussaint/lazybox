// The narration layer drawn over the app: a pointer (headless video has
// none) with a ripple on each press, captions, keys pressed and highlights.
// It lives in a shadow root on <html>, ignores the pointer (pointer-events:
// none) and is hidden from accessibility, so the app and its locators never
// see it. Colours come from the app's theme tokens, so it follows dark and
// light. `installOverlay` runs in the page; `Overlay` drives it from Node.
import type { Locator, Page } from '@playwright/test';

export type PointerStyle = 'arrow' | 'touch';
export type CaptionPosition = 'top' | 'bottom';

/** Runs in the page (addInitScript): installs `window.__scenario`. */
export function installOverlay(options: { pointer: PointerStyle; compact: boolean }) {
  const w = window as unknown as { __scenario?: unknown };
  if (w.__scenario) return;
  const host = document.createElement('div');
  host.id = '__scenario-overlay';
  host.setAttribute('aria-hidden', 'true');
  host.style.cssText = 'position:fixed;inset:0;pointer-events:none;z-index:2147483647;contain:layout style;';
  const root = host.attachShadow({ mode: 'open' });
  const compact = options.compact;
  root.innerHTML = `<style>
    :host { all: initial; }
    * { box-sizing: border-box; }
    .layer { position: fixed; inset: 0; pointer-events: none; font-family: var(--scenario-font, -apple-system, BlinkMacSystemFont, "Inter", system-ui, sans-serif); -webkit-font-smoothing: antialiased; }
    .caption { position: absolute; left: 50%; ${compact ? 'bottom: 18px;' : 'bottom: 30px;'} display: flex; align-items: stretch; gap: ${compact ? 10 : 14}px;
      max-width: min(${compact ? 'calc(100% - 28px)' : '820px'}, calc(100% - 32px)); width: max-content;
      padding: ${compact ? '10px 14px' : '13px 22px 13px 16px'}; border-radius: ${compact ? 14 : 16}px;
      background: color-mix(in srgb, var(--surface-raised, #1d1f24) 94%, transparent);
      color: var(--text-strong, #f4f5f7); border: 1px solid var(--line-strong, rgba(255,255,255,0.16));
      box-shadow: 0 18px 48px rgba(0,0,0,0.38), 0 2px 8px rgba(0,0,0,0.22);
      backdrop-filter: blur(14px) saturate(1.2); -webkit-backdrop-filter: blur(14px) saturate(1.2);
      font-size: ${compact ? 15 : 19}px; line-height: 1.42; font-weight: 550; letter-spacing: -0.005em; text-align: left;
      opacity: 0; transform: translate(-50%, 10px) scale(0.985); transition: opacity 260ms ease, transform 320ms cubic-bezier(.2,.8,.2,1); }
    .caption.top { bottom: auto; top: ${compact ? 14 : 22}px; transform: translate(-50%, -10px) scale(0.985); }
    .caption.show { opacity: 1; transform: translate(-50%, 0) scale(1); }
    .caption .bar { flex: none; width: 4px; border-radius: 3px; background: var(--accent, #7c9cff); }
    .caption .text { padding: 1px 0; }
    .caption .text b { color: var(--accent-strong, var(--accent, #9db4ff)); font-weight: 650; }
    .keys { position: absolute; right: 24px; bottom: 30px; display: flex; gap: 6px; opacity: 0; transform: translateY(6px); transition: opacity 180ms ease, transform 220ms ease; }
    .keys.show { opacity: 1; transform: none; }
    .keys kbd { min-width: 34px; height: 34px; padding: 0 10px; display: inline-flex; align-items: center; justify-content: center;
      font: 600 17px/1 inherit; font-family: inherit; color: var(--text-strong, #f4f5f7); background: var(--surface-raised, #26282e);
      border: 1px solid var(--line-strong, rgba(255,255,255,0.18)); border-bottom-width: 3px; border-radius: 8px; box-shadow: 0 8px 22px rgba(0,0,0,0.3); }
    .ring { position: absolute; border-radius: 12px; border: 2px solid var(--accent, #7c9cff); opacity: 0; transform: scale(1.06);
      box-shadow: 0 0 0 5px color-mix(in srgb, var(--accent, #7c9cff) 22%, transparent), 0 0 28px color-mix(in srgb, var(--accent, #7c9cff) 30%, transparent);
      transition: opacity 220ms ease, transform 320ms cubic-bezier(.2,.8,.2,1), left 260ms ease, top 260ms ease, width 260ms ease, height 260ms ease; }
    .ring.show { opacity: 1; transform: none; }
    .ring .tag { position: absolute; left: -2px; top: -34px; padding: 4px 10px; border-radius: 8px; white-space: nowrap; font-size: 13px; font-weight: 600;
      background: var(--accent-solid, #4c6ef5); color: var(--on-accent-solid, #fff); }
    .ring .tag:empty { display: none; }
    .pointer { position: absolute; left: 0; top: 0; opacity: 0; transition: opacity 200ms ease; will-change: transform; }
    .pointer.show { opacity: 1; }
    .pointer svg { display: block; filter: drop-shadow(0 2px 3px rgba(0,0,0,0.35)); transition: transform 120ms ease; transform-origin: 3px 2px; }
    .pointer.down svg { transform: scale(0.88); }
    .touch { position: absolute; left: -17px; top: -17px; width: 34px; height: 34px; border-radius: 50%; background: rgba(255,255,255,0.32); border: 2px solid rgba(255,255,255,0.75); box-shadow: 0 2px 10px rgba(0,0,0,0.35); transition: transform 140ms ease, background 140ms ease; }
    .pointer.down .touch { transform: scale(0.8); background: rgba(255,255,255,0.5); }
    .ripple { position: absolute; width: 16px; height: 16px; margin: -8px 0 0 -8px; border-radius: 50%; border: 2px solid var(--accent, #7c9cff);
      background: color-mix(in srgb, var(--accent, #7c9cff) 25%, transparent); animation: ripple 520ms cubic-bezier(.2,.7,.3,1) forwards; }
    @keyframes ripple { from { transform: scale(0.6); opacity: 0.9; } to { transform: scale(3.4); opacity: 0; } }
  </style><div class="layer"><div class="ring"><span class="tag"></span></div><div class="caption"><span class="bar"></span><span class="text"></span></div><div class="keys"></div><div class="pointer">${options.pointer === 'touch' ? '<div class="touch"></div>'
    : '<svg width="26" height="30" viewBox="0 0 26 30"><path d="M3 2 L3 24 L8.6 19 L12.6 28 L16.4 26.4 L12.5 17.6 L20 17.6 Z" fill="#fff" stroke="#111" stroke-width="1.6" stroke-linejoin="round"/></svg>'}</div></div>`;
  const $ = (selector: string) => root.querySelector(selector) as HTMLElement;
  const caption = $('.caption'), captionText = $('.caption .text'), keys = $('.keys'), ring = $('.ring'), tag = $('.ring .tag'), pointer = $('.pointer'), layer = $('.layer');
  const mount = () => {
    if (!document.documentElement) return;
    if (!host.isConnected) document.documentElement.appendChild(host);
    const font = document.body && getComputedStyle(document.body).fontFamily;
    if (font) layer.style.setProperty('--scenario-font', font);
  };
  mount();
  document.addEventListener('DOMContentLoaded', mount);
  addEventListener('load', mount);

  let x = -100, y = -100;
  const place = () => { pointer.style.transform = `translate(${x}px, ${y}px)`; };
  addEventListener('mousemove', e => { x = e.clientX; y = e.clientY; pointer.classList.add('show'); place(); }, { capture: true, passive: true });
  addEventListener('mousedown', e => {
    x = e.clientX; y = e.clientY; place(); pointer.classList.add('show', 'down');
    const ripple = document.createElement('div'); ripple.className = 'ripple'; ripple.style.left = `${x}px`; ripple.style.top = `${y}px`;
    layer.appendChild(ripple); setTimeout(() => ripple.remove(), 600);
  }, { capture: true, passive: true });
  addEventListener('mouseup', () => pointer.classList.remove('down'), { capture: true, passive: true });

  let ringFrame = 0;
  let captionTimer: ReturnType<typeof setTimeout> | undefined, keysTimer: ReturnType<typeof setTimeout> | undefined;
  const escape = (value: string) => value.replace(/[&<>]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;' }[c]!));
  // **bold** in a caption is drawn in the accent colour.
  const rich = (value: string) => escape(value).replace(/\*\*(.+?)\*\*/g, '<b>$1</b>');
  w.__scenario = {
    caption(value: string, position: 'top' | 'bottom') {
      mount(); clearTimeout(captionTimer);
      const show = () => { captionText.innerHTML = rich(value); caption.classList.toggle('top', position === 'top'); requestAnimationFrame(() => caption.classList.add('show')); };
      if (caption.classList.contains('show')) { caption.classList.remove('show'); captionTimer = setTimeout(show, 240); } else show();
    },
    clearCaption() { clearTimeout(captionTimer); caption.classList.remove('show'); },
    keys(labels: string[]) {
      mount(); clearTimeout(keysTimer);
      keys.innerHTML = labels.map(label => `<kbd>${escape(label)}</kbd>`).join('');
      requestAnimationFrame(() => keys.classList.add('show'));
      keysTimer = setTimeout(() => keys.classList.remove('show'), 1300);
    },
    highlight(box: { x: number; y: number; width: number; height: number } | null, label: string) {
      mount();
      cancelAnimationFrame(ringFrame);
      if (!box) { ring.classList.remove('show'); return; }
      const pad = 6;
      Object.assign(ring.style, { left: `${box.x - pad}px`, top: `${box.y - pad}px`, width: `${box.width + pad * 2}px`, height: `${box.height + pad * 2}px` });
      tag.textContent = label;
      tag.style.top = box.y < 48 ? `${box.height + pad * 2 + 6}px` : '-34px';
      ringFrame = requestAnimationFrame(() => ring.classList.add('show'));
    },
    pointer(nx: number, ny: number) { x = nx; y = ny; pointer.classList.add('show'); place(); },
  };
}

type PageApi = { __scenario?: { caption(text: string, position: CaptionPosition): void; clearCaption(): void; keys(labels: string[]): void; highlight(box: { x: number; y: number; width: number; height: number } | null, label: string): void } };

const ease = (t: number) => t < 0.5 ? 4 * t * t * t : 1 - Math.pow(-2 * t + 2, 3) / 2;
const sleep = (ms: number) => new Promise(resolve => setTimeout(resolve, ms));

/** Drives the overlay and the pointer from Node. In a fast test, motion is instant. */
export class Overlay {
  x: number; y: number;
  caption?: { text: string; position: CaptionPosition; shownAt: number };
  constructor(private page: Page, private video: boolean, viewport: { width: number; height: number }) { this.x = viewport.width * 0.62; this.y = viewport.height * 0.58; }
  setPage(page: Page) { this.page = page; }
  private api() { return this.page; }
  async showCaption(text: string, position: CaptionPosition) {
    this.caption = { text, position, shownAt: performance.now() };
    await this.restore();
  }
  async clearCaption() { this.caption = undefined; await this.api().evaluate(() => (window as unknown as PageApi).__scenario?.clearCaption()).catch(() => {}); }
  /** Puts the caption back after a navigation (the overlay is new). */
  async restore() {
    if (!this.caption) return;
    await this.api().evaluate(([text, position]) => (window as unknown as PageApi).__scenario?.caption(text, position as CaptionPosition), [this.caption.text, this.caption.position] as const).catch(() => {});
  }
  async keys(labels: string[]) { if (this.video) await this.api().evaluate(value => (window as unknown as PageApi).__scenario?.keys(value), labels).catch(() => {}); }
  async highlight(box: { x: number; y: number; width: number; height: number } | null, label = '') {
    await this.api().evaluate(([value, tag]) => (window as unknown as PageApi).__scenario?.highlight(value, tag), [box, label] as const).catch(() => {});
  }
  /** Glides the pointer to a point, easing in and out, as a hand would. */
  async moveTo(x: number, y: number) {
    if (!this.video) { this.x = x; this.y = y; await this.page.mouse.move(x, y); return; }
    const distance = Math.hypot(x - this.x, y - this.y);
    if (distance < 2) { await this.page.mouse.move(x, y); return; }
    const duration = Math.min(900, Math.max(260, 220 + distance * 0.55));
    const from = { x: this.x, y: this.y }, started = performance.now();
    // A slight arc reads as a hand, not a robot.
    const bend = Math.min(60, distance * 0.08) * (x > from.x ? -1 : 1);
    for (;;) {
      const t = Math.min(1, (performance.now() - started) / duration), k = ease(t);
      const arc = Math.sin(Math.PI * k) * bend;
      const nx = from.x + (x - from.x) * k, ny = from.y + (y - from.y) * k + arc;
      await this.page.mouse.move(nx, ny);
      if (t >= 1) break;
      await sleep(14);
    }
    this.x = x; this.y = y;
  }
  /** Moves to a target's centre (scrolled into view first). */
  async moveToLocator(locator: Locator) {
    await locator.scrollIntoViewIfNeeded().catch(() => {});
    const box = await locator.boundingBox();
    if (!box) return;
    await this.moveTo(box.x + box.width / 2, box.y + Math.min(box.height / 2, 18));
  }
  /** Shows where the pointer is (after a navigation, the new overlay has none). */
  async park() {
    await this.page.mouse.move(this.x, this.y);
    await this.page.evaluate(([x, y]) => (window as unknown as { __scenario?: { pointer(x: number, y: number): void } }).__scenario?.pointer(x, y), [this.x, this.y] as const).catch(() => {});
  }
}
