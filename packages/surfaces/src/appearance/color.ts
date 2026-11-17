/**
 * Colour arithmetic for themes: parsing, OKLab/OKLCH mixing and WCAG
 * contrast. Pure functions, no DOM, so the token sweep script, the unit tests
 * and the runtime share one implementation.
 */
export type Rgba = { r: number; g: number; b: number; a: number };
export type Oklch = { l: number; c: number; h: number; a: number };

const HEX = /^#([0-9a-f]{3}|[0-9a-f]{4}|[0-9a-f]{6}|[0-9a-f]{8})$/i;
const FUNCTIONAL = /^(rgba?|hsla?)\(\s*([^)]*)\)$/i;

/**
 * Parses the colour syntaxes a theme file may use: #rgb, #rgba, #rrggbb,
 * #rrggbbaa, rgb()/rgba() and hsl()/hsla(). Anything else (names, var(),
 * url(), expressions) is refused, so a theme value can never inject CSS.
 */
export function parseColor(input: string): Rgba | undefined {
  const text = input.trim().toLowerCase();
  const hex = HEX.exec(text);
  if (hex) {
    let digits = hex[1];
    if (digits.length <= 4) digits = [...digits].map(d => d + d).join('');
    const n = (i: number) => parseInt(digits.slice(i, i + 2), 16) / 255;
    return { r: n(0), g: n(2), b: n(4), a: digits.length === 8 ? n(6) : 1 };
  }
  const fn = FUNCTIONAL.exec(text);
  if (!fn) return undefined;
  const parts = fn[2].split(/\s*[,/]\s*|\s+/).filter(Boolean);
  if (parts.length < 3 || parts.length > 4) return undefined;
  const alpha = parts[3] === undefined ? 1 : unit(parts[3], 1);
  if (alpha === undefined) return undefined;
  if (fn[1].startsWith('rgb')) {
    const [r, g, b] = parts.slice(0, 3).map(p => unit(p, 255));
    if (r === undefined || g === undefined || b === undefined) return undefined;
    return clampRgba({ r, g, b, a: alpha });
  }
  const h = Number(parts[0].replace(/deg$/, '')), s = unit(parts[1], 100), l = unit(parts[2], 100);
  if (!Number.isFinite(h) || s === undefined || l === undefined || !parts[1].endsWith('%') || !parts[2].endsWith('%')) return undefined;
  return clampRgba({ ...hslToRgb(((h % 360) + 360) % 360, s, l), a: alpha });
}
/** A number or percentage as a 0–1 fraction of `scale`. */
function unit(text: string, scale: number): number | undefined {
  if (!/^-?(\d+\.?\d*|\.\d+)%?$/.test(text)) return undefined;
  const value = text.endsWith('%') ? Number(text.slice(0, -1)) / 100 : Number(text) / scale;
  return Number.isFinite(value) ? Math.max(0, Math.min(1, value)) : undefined;
}
function hslToRgb(h: number, s: number, l: number) {
  const k = (n: number) => (n + h / 30) % 12, a = s * Math.min(l, 1 - l);
  const f = (n: number) => l - a * Math.max(-1, Math.min(k(n) - 3, 9 - k(n), 1));
  return { r: f(0), g: f(8), b: f(4) };
}
const clamp01 = (v: number) => Math.max(0, Math.min(1, v));
const clampRgba = (c: Rgba): Rgba => ({ r: clamp01(c.r), g: clamp01(c.g), b: clamp01(c.b), a: clamp01(c.a) });

export function isColor(text: string): boolean { return parseColor(text) !== undefined; }

const OKLAB_FUNCTIONAL = /^(oklch|oklab)\(\s*([^)]*)\)$/i;
/**
 * CSS oklch() and oklab() as sRGB, out-of-gamut colours brought inside it.
 * Deliberately not part of parseColor: theme files keep their own syntaxes.
 * This is for consumers that must turn whatever they are handed into a
 * colour they can draw with (Mermaid, which reads hex and rgb() only).
 */
export function parseOklabColor(input: string): Rgba | undefined {
  const fn = OKLAB_FUNCTIONAL.exec(input.trim().toLowerCase());
  if (!fn) return undefined;
  const parts = fn[2].split(/\s*\/\s*|\s+/).filter(Boolean);
  if (parts.length < 3 || parts.length > 4) return undefined;
  // A percentage is of `full`: 100% lightness is 1, 100% chroma or a/b is 0.4.
  const num = (text: string, full: number) => text === 'none' ? 0 : /^-?(\d+\.?\d*|\.\d+)%?$/.test(text) ? (text.endsWith('%') ? Number(text.slice(0, -1)) / 100 * full : Number(text)) : undefined;
  const l = num(parts[0], 1), alpha = parts[3] === undefined ? 1 : unit(parts[3], 1);
  if (l === undefined || alpha === undefined) return undefined;
  if (fn[1] === 'oklch') {
    const c = num(parts[1], 0.4), h = parts[2] === 'none' ? 0 : Number(parts[2].replace(/deg$/, ''));
    if (c === undefined || !Number.isFinite(h)) return undefined;
    return fromOklch({ l: clamp01(l), c: Math.max(0, c), h: ((h % 360) + 360) % 360, a: alpha });
  }
  const a = num(parts[1], 0.4), b = num(parts[2], 0.4);
  if (a === undefined || b === undefined) return undefined;
  return fromOklab(clamp01(l), a, b, alpha);
}

/** Lower-case #rrggbb, or #rrggbbaa when not opaque. */
export function toHex(c: Rgba): string {
  const h = (v: number) => Math.round(clamp01(v) * 255).toString(16).padStart(2, '0');
  return `#${h(c.r)}${h(c.g)}${h(c.b)}${c.a < 0.999 ? h(c.a) : ''}`;
}
/** Normalises any accepted colour to hex; throws on an unaccepted one. */
export function normalizeColor(text: string): string {
  const c = parseColor(text);
  if (!c) throw new Error(`Not a colour: ${text}`);
  return toHex(c);
}

const toLinear = (v: number) => v <= 0.04045 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4;
const fromLinear = (v: number) => v <= 0.0031308 ? 12.92 * v : 1.055 * v ** (1 / 2.4) - 0.055;

export function toOklab(c: Rgba): { L: number; A: number; B: number } {
  const r = toLinear(c.r), g = toLinear(c.g), b = toLinear(c.b);
  const l = Math.cbrt(0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b);
  const m = Math.cbrt(0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b);
  const s = Math.cbrt(0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b);
  return { L: 0.2104542553 * l + 0.793617785 * m - 0.0040720468 * s, A: 1.9779984951 * l - 2.428592205 * m + 0.4505937099 * s, B: 0.0259040371 * l + 0.7827717662 * m - 0.808675766 * s };
}
export function fromOklab(L: number, A: number, B: number, alpha = 1): Rgba {
  const l = (L + 0.3963377774 * A + 0.2158037573 * B) ** 3;
  const m = (L - 0.1055613458 * A - 0.0638541728 * B) ** 3;
  const s = (L - 0.0894841775 * A - 1.291485548 * B) ** 3;
  return clampRgba({
    r: fromLinear(4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s),
    g: fromLinear(-1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s),
    b: fromLinear(-0.0041960863 * l - 0.7034186147 * m + 1.707614701 * s),
    a: alpha,
  });
}
export function toOklch(c: Rgba): Oklch {
  const { L, A, B } = toOklab(c);
  return { l: L, c: Math.hypot(A, B), h: ((Math.atan2(B, A) * 180) / Math.PI + 360) % 360, a: c.a };
}
export function fromOklch({ l, c, h, a }: Oklch): Rgba {
  // Reduce chroma until the colour is inside sRGB, keeping lightness and hue.
  for (let chroma = c; chroma >= 0; chroma -= 0.002) {
    const rad = (h * Math.PI) / 180;
    const rgb = rawOklab(l, chroma * Math.cos(rad), chroma * Math.sin(rad));
    if ([rgb.r, rgb.g, rgb.b].every(v => v >= -0.001 && v <= 1.001)) return clampRgba({ ...rgb, a });
  }
  return fromOklab(l, 0, 0, a);
}
function rawOklab(L: number, A: number, B: number) {
  const l = (L + 0.3963377774 * A + 0.2158037573 * B) ** 3;
  const m = (L - 0.1055613458 * A - 0.0638541728 * B) ** 3;
  const s = (L - 0.0894841775 * A - 1.291485548 * B) ** 3;
  return {
    r: fromLinear(4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s),
    g: fromLinear(-1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s),
    b: fromLinear(-0.0041960863 * l - 0.7034186147 * m + 1.707614701 * s),
  };
}

/** `amount` of `b` mixed into `a`, in OKLab (perceptually even). */
export function mix(a: string, b: string, amount: number): string {
  const x = parseColor(a), y = parseColor(b);
  if (!x || !y) throw new Error(`Cannot mix ${a} and ${b}`);
  const p = toOklab(x), q = toOklab(y), t = clamp01(amount);
  return toHex(fromOklab(p.L + (q.L - p.L) * t, p.A + (q.A - p.A) * t, p.B + (q.B - p.B) * t, x.a + (y.a - x.a) * t));
}
/** The colour with its OKLCH lightness, chroma or hue changed. */
export function adjust(color: string, change: { l?: number; c?: number; scaleC?: number; h?: number }): string {
  const c = parseColor(color);
  if (!c) throw new Error(`Not a colour: ${color}`);
  const lch = toOklch(c);
  return toHex(fromOklch({ ...lch, l: change.l ?? lch.l, c: change.c ?? lch.c * (change.scaleC ?? 1), h: change.h ?? lch.h }));
}
export function lightness(color: string): number { const c = parseColor(color); return c ? toOklab(c).L : 0; }
/** Perceptual distance (Euclidean OKLab, ×100 so 2 is about one just-noticeable step). */
export function distance(a: string, b: string): number {
  const x = parseColor(a), y = parseColor(b);
  if (!x || !y) return Infinity;
  const p = toOklab(x), q = toOklab(y);
  return 100 * Math.hypot(p.L - q.L, p.A - q.A, p.B - q.B);
}

/** WCAG 2 relative luminance. A translucent colour is composited on `over`. */
export function luminance(color: string, over = '#000000'): number {
  let c = parseColor(color);
  if (!c) return 0;
  if (c.a < 1) { const o = parseColor(over) || { r: 0, g: 0, b: 0, a: 1 }; c = { r: c.r * c.a + o.r * (1 - c.a), g: c.g * c.a + o.g * (1 - c.a), b: c.b * c.a + o.b * (1 - c.a), a: 1 }; }
  return 0.2126 * toLinear(c.r) + 0.7152 * toLinear(c.g) + 0.0722 * toLinear(c.b);
}
/** WCAG 2 contrast ratio, 1–21. */
export function contrast(foreground: string, background: string): number {
  const bg = luminance(background), fg = luminance(foreground, background);
  return (Math.max(fg, bg) + 0.05) / (Math.min(fg, bg) + 0.05);
}
/**
 * The colour moved in OKLCH lightness, away from the background, just far
 * enough to reach `ratio` against it, keeping hue and as much chroma as fits.
 * A colour that already reaches it is returned unchanged.
 */
export function ensureContrast(foreground: string, background: string, ratio: number): string {
  if (contrast(foreground, background) >= ratio) return foreground;
  const fg = parseColor(foreground);
  if (!fg) return foreground;
  const lch = toOklch(fg), darker = luminance(background) > 0.18;
  const candidate = (l: number) => toHex(fromOklch({ ...lch, l, a: 1 }));
  // Binary search toward black or white; take white/black when even that falls short.
  let lo = lch.l, hi = darker ? 0 : 1;
  if (contrast(candidate(hi), background) < ratio) return darker ? '#000000' : '#ffffff';
  for (let i = 0; i < 24; i++) { const mid = (lo + hi) / 2; if (contrast(candidate(mid), background) >= ratio) hi = mid; else lo = mid; }
  return candidate(hi);
}
/** Black or white, whichever reads better on `background`. */
export function readableOn(background: string, dark = '#111114', light = '#ffffff'): string {
  return contrast(dark, background) >= contrast(light, background) ? dark : light;
}
