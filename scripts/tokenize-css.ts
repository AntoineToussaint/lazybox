// Rewrites literal colours and pixel font sizes in stylesheets to design
// tokens (packages/surfaces/src/appearance). Each colour is mapped to the
// nearest token of the Dark theme that fits how it is used: a `color` to a
// text token, a `border` to a line token, a `background` to a surface or
// tinted fill, by hue family. Text on a filled background becomes that fill's
// `on-*` token. Translucent colours keep their alpha through color-mix().
//
//   node --import tsx scripts/tokenize-css.ts [--check] [--report out.json] <file.css>...
//
// With no files it rewrites every stylesheet under packages/surfaces/src.
// --check rewrites nothing and exits 1 when a file still has a literal.
// Review the report's largest distances: they are where the nearest token
// is visibly different from the literal it replaced.
import { readFileSync, readdirSync, statSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { distance, parseColor, toHex, toOklch } from '../packages/surfaces/src/appearance/color.ts';
import { BUILTIN_BY_ID, resolveTheme } from '../packages/surfaces/src/appearance/themes.ts';
import { TOKEN_NAMES, FAMILIES, type Family } from '../packages/surfaces/src/appearance/tokens.ts';

const dark = resolveTheme(BUILTIN_BY_ID.get('dark')!).tokens;
type Role = 'fg' | 'bg' | 'line';
type Mapping = { file: string; selector: string; property: string; literal: string; token: string; distance: number };
const report: Mapping[] = [];

const surfaces = ['bg', 'surface-sunken', 'surface', 'surface-raised', 'surface-hover', 'surface-active', 'surface-selected', 'surface-strong'];
const lines = ['line-subtle', 'line', 'line-strong', 'line-bold'];
const texts = ['text-strong', 'text', 'text-soft', 'text-secondary', 'text-tertiary', 'text-subtle', 'text-muted', 'text-faint', 'text-disabled'];
const neutralGroups: Record<Role, string[]> = { bg: surfaces, line: lines, fg: texts };
const familyLevels = { fg: ['', '-soft', '-strong'], bg: ['-bg', '-bg-strong', '-solid', '-solid-hover', ''], line: ['-line-subtle', '-line', ''] } as const;

/** The hue family of a colour, or undefined for a neutral grey. */
function familyOf(hex: string, role: Role): Family | undefined {
  const { c, h, l } = toOklch(parseColor(hex)!);
  // The greys lean blue-violet; a dark warm or green tint is a status background.
  const coolGrey = h > 230 && h < 300;
  if (c < 0.012 || (coolGrey && c < 0.024) || (c < 0.02 && l > 0.45) || (role === 'fg' && c < 0.035 && l > 0.45)) return undefined;
  if (h >= 345 || h < 45) return 'danger';
  if (h < 110) return 'warn';
  // Lime and green both mean "on", "ready", "passed".
  if (h < 200) return 'ok';
  if (h < 270) return 'info';
  return 'accent';
}

function nearest(hex: string, candidates: string[], penalties: Record<string, number> = {}): { token: string; distance: number } {
  let best = { token: candidates[0], distance: Infinity, score: Infinity };
  for (const token of candidates) {
    const d = distance(hex, dark[token]), score = d + (penalties[token] || 0);
    if (score < best.score) best = { token, distance: d, score };
  }
  return best;
}

const ADD = /(\bis-add|addition|\badded\b|diff-add|\.add\b|insert|sign=add)/, DEL = /(\bis-del|is-remove|deletion|\bremoved\b|diff-del|diff-remove|\.del\b|sign=del)/;

/** The token for one opaque colour used in `role` by `selector`. */
function tokenFor(hex: string, role: Role, selector: string, fill?: string, property = ''): { token: string; distance: number } {
  const lightness = toOklch(parseColor(hex)!).l;
  // Dark text on a light fill: the fill's own text token.
  if (role === 'fg' && fill) {
    const match = /^(accent|ok|warn|danger|info)(-solid(-hover)?)?$/.exec(fill) || /^(accent|ok|warn|danger|info)-(soft|strong)$/.exec(fill);
    if (match) {
      const solid = !!match[2] && match[2].startsWith('-solid');
      const token = `on-${match[1]}${solid ? '-solid' : ''}`;
      if (distance(hex, dark[token]) < 12) return { token, distance: distance(hex, dark[token]) };
    }
  }
  const fam = familyOf(hex, role);
  // Focus rings are one token, whatever shade of accent each file picked.
  if (fam === 'accent' && (/^outline/.test(property) || (/:focus|is-focused|drop-(before|after)/.test(selector) && role !== 'bg'))) return { token: 'focus', distance: distance(hex, dark.focus) };
  if (fam === 'ok' || fam === 'danger' || (fam === undefined && role === 'bg')) {
    const kind = ADD.test(selector) ? 'add' : DEL.test(selector) ? 'del' : '';
    if (kind && (fam === (kind === 'add' ? 'ok' : 'danger') || fam === undefined)) {
      const set = role === 'bg' ? [`diff-${kind}-bg`] : role === 'fg' ? [`diff-${kind}-text`, `diff-${kind}-mark`] : [];
      if (set.length) { const hit = nearest(hex, set); if (hit.distance < 6) return hit; }
    }
  }
  if (!fam) {
    const penalties: Record<string, number> = {};
    for (const [group, tokens] of Object.entries(neutralGroups)) for (const token of tokens) penalties[token] = group === role ? 0 : 4;
    // Dark text is text on a light fill: say so, rather than reuse a surface.
    if (role === 'fg' && lightness < 0.4) return nearest(hex, ['on-accent', 'on-ok', 'on-warn', 'on-danger', 'on-info']);
    return nearest(hex, [...surfaces, ...lines, ...texts], penalties);
  }
  const candidates: string[] = [], penalties: Record<string, number> = {};
  for (const [group, suffixes] of Object.entries(familyLevels)) for (const suffix of suffixes) {
    const token = fam + suffix;
    if (!candidates.includes(token)) { candidates.push(token); penalties[token] = group === role ? 0 : 3; }
    else if (group === role) penalties[token] = 0;
  }
  if (fam === 'ok') { candidates.push('running'); penalties.running = 0; }
  if (role === 'fg' && lightness < 0.4) return { token: `on-${fam}`, distance: distance(hex, dark[`on-${fam}`]) };
  const hit = nearest(hex, candidates, penalties);
  // A tinted grey reads as text: keep it neutral when a text token is closer.
  if (role === 'fg') { const grey = nearest(hex, texts); if (grey.distance + 1.5 < hit.distance) return grey; }
  return hit;
}

function roleOf(property: string): Role {
  if (/^(color|fill|stroke|caret-color|accent-color|-webkit-text-fill-color|text-decoration-color)$/.test(property)) return 'fg';
  if (/^(border|outline|box-shadow|column-rule|text-shadow)/.test(property)) return 'line';
  return 'bg';
}
function roleOfVariable(name: string): Role {
  if (/(text|muted|faint|fg|ink|working|failed|waiting|done|input|review|idle|claimed|observed|verified|accepted|accent|ok|warn|bad)$/.test(name) && !/-bg$/.test(name)) return 'fg';
  if (/line|border/.test(name)) return 'line';
  return 'bg';
}

const COLOR = /#[0-9a-fA-F]{3,8}\b|rgba?\([^)]*\)|hsla?\([^)]*\)/g;
function rewriteValue(value: string, role: Role, ctx: { file: string; selector: string; property: string; fill?: string }): string {
  return value.replace(COLOR, literal => {
    const color = parseColor(literal);
    if (!color) return literal;
    const opaque = toHex({ ...color, a: 1 });
    const percent = Math.round(color.a * 100);
    const { l, c } = toOklch(color);
    let out: string, token: string, d: number;
    if (color.a < 1 && l < 0.12 && c < 0.03) {
      // Shadows and backdrops: black, made gentler by light themes.
      token = 'shadow'; d = 0;
      out = `color-mix(in srgb,var(--shadow) calc(${percent}% * var(--shadow-strength)),transparent)`;
    } else {
      const effectiveRole = role === 'line' && ctx.property === 'box-shadow' && color.a < 1 ? 'bg' : role;
      ({ token, distance: d } = tokenFor(opaque, effectiveRole, ctx.selector, ctx.fill, ctx.property));
      out = color.a < 1 ? `color-mix(in srgb,var(--${token}) ${percent}%,transparent)` : `var(--${token})`;
    }
    report.push({ ...ctx, literal, token, distance: Math.round(d * 10) / 10 });
    return out;
  });
}

/** Splits a stylesheet into rules, rewriting colour declarations in each. */
export function tokenizeCss(text: string, file = ''): string {
  let out = '', i = 0;
  const stack: string[] = [];
  let start = 0;
  while (i < text.length) {
    const ch = text[i];
    if (ch === '/' && text[i + 1] === '*') { const end = text.indexOf('*/', i + 2); i = end < 0 ? text.length : end + 2; continue; }
    if (ch === '"' || ch === "'") { const end = text.indexOf(ch, i + 1); i = end < 0 ? text.length : end + 1; continue; }
    if (ch === '{') { out += text.slice(start, i + 1); stack.push(text.slice(start, i).trim()); start = i + 1; i++; continue; }
    if (ch === '}') {
      const body = text.slice(start, i);
      const selector = stack.pop() || '';
      // Paper is white whatever the theme: print styles keep their literals.
      const printing = stack.some(s => /^@media\s+print/.test(s));
      out += printing || (selector.startsWith('@') && /[{]/.test(body)) ? body : rewriteBlock(body, selector, file);
      out += '}'; start = i + 1; i++; continue;
    }
    i++;
  }
  return out + text.slice(start);
}
function rewriteBlock(body: string, selector: string, file: string): string {
  if (selector.startsWith('@')) return body;
  if (/^(from|to|\d+%)/.test(selector)) selector = `@keyframes ${selector}`;
  const declarations = splitDeclarations(body);
  // Backgrounds first, so the text of the same rule can follow its fill.
  let fill: string | undefined;
  for (const d of declarations) {
    if (!/^background(-color)?$/.test(d.property)) continue;
    const m = COLOR.exec(d.value); COLOR.lastIndex = 0;
    const c = m && parseColor(m[0]);
    if (c && c.a === 1) fill = tokenFor(toHex(c), 'bg', selector).token;
  }
  // Text sizes follow the UI text size setting; code follows the code font size.
  const code = CODE_SELECTOR.test(selector) || declarations.some(d => /^font(-family)?$/.test(d.property) && /--font-mono|monospace/.test(d.value));
  const scale = code ? '--code-scale' : '--text-scale';
  return declarations.map(d => {
    if (!d.property || /^(-webkit-)?mask/.test(d.property)) return d.raw;
    if (d.property === 'font-size' || d.property === 'line-height') return d.prefix + d.property + ':' + scalePixels(d.value, scale) + d.suffix;
    if (d.property === 'font') return d.prefix + d.property + ':' + scaleFontShorthand(d.value, scale) + d.suffix;
    if (d.property.startsWith('--')) {
      const name = d.property.slice(2);
      // A local copy of a global token would shadow the theme: the global one applies.
      const literal = COLOR.test(d.value); COLOR.lastIndex = 0;
      if (TOKEN_NAMES.has(name) && literal) { report.push({ file, selector, property: d.property, literal: d.value.trim(), token: `(removed; global --${name})`, distance: 0 }); return ''; }
      return d.prefix + d.property + ':' + rewriteValue(d.value, roleOfVariable(name), { file, selector, property: d.property }) + d.suffix;
    }
    return d.prefix + d.property + ':' + rewriteValue(d.value, roleOf(d.property), { file, selector, property: d.property, fill }) + d.suffix;
  }).join('');
}
const CODE_SELECTOR = /(^|[\s>+~,(])(pre|code|kbd|samp)\b|\.mono\b|hljs|console-output|\.output\b|terminal|diff-line|review-code|review-row|code-block/;
const PIXELS = /(^|[\s/(,])(\d*\.?\d+)px\b/g;
/** `12px` → `calc(12px * var(--text-scale))`; values already scaled or not in px are kept. */
function scalePixels(value: string, scale: string): string {
  if (/var\(|calc\(/.test(value)) return value;
  return value.replace(PIXELS, (_m, lead, n) => `${lead}calc(${n}px * var(${scale}))`);
}
/** The size (and px line height) of a `font:` shorthand such as `600 14px/20px var(--font-ui)`; a unitless line height is kept. */
function scaleFontShorthand(value: string, scale: string): string {
  if (/calc\(/.test(value)) return value;
  return value.replace(/(^|\s)(\d*\.?\d+px)(?:\/(\d*\.?\d+)(px)?)?(?=\s)/, (_m, lead, size, line, px) => `${lead}calc(${size} * var(${scale}))${line ? px ? `/calc(${line}px * var(${scale}))` : `/${line}` : ''}`);
}
/** Pixel font sizes left in a stylesheet (print excluded), which no text size setting reaches: `font-size: 12px`, or a `font:` shorthand's `11px/1.5`. */
export function literalFontSizes(text: string): string[] {
  const visible = withoutPrint(text.replace(/\/\*[\s\S]*?\*\//g, ''));
  return [
    ...[...visible.matchAll(/font-size\s*:\s*(\d*\.?\d+px)/g)].map(m => m[1]),
    ...[...visible.matchAll(/(?:^|[;{\s])font\s*:\s*([^;}]*)/g)].flatMap(m => /calc\(/.test(m[1]) ? [] : [/(?:^|\s)(\d*\.?\d+px)(?=[\s/])/.exec(m[1])?.[1]].filter((size): size is string => !!size)),
  ];
}
type Declaration = { raw: string; prefix: string; property: string; value: string; suffix: string };
function splitDeclarations(body: string): Declaration[] {
  const parts: Declaration[] = [];
  let depth = 0, from = 0;
  const push = (raw: string) => {
    const m = /^(\s*)([-a-zA-Z0-9_]+)\s*:([\s\S]*?)(\s*;?\s*)$/.exec(raw);
    parts.push(m ? { raw, prefix: m[1], property: m[2].toLowerCase(), value: m[3], suffix: m[4] } : { raw, prefix: '', property: '', value: '', suffix: '' });
  };
  for (let i = 0; i < body.length; i++) {
    const ch = body[i];
    if (ch === '(') depth++; else if (ch === ')') depth--;
    else if (ch === ';' && depth === 0) { push(body.slice(from, i + 1)); from = i + 1; }
  }
  if (from < body.length) push(body.slice(from));
  return parts;
}

/** Literal colours left in a stylesheet (comments excluded). */
export function literalColors(text: string): string[] {
  const visible = withoutPrint(text.replace(/\/\*[\s\S]*?\*\//g, '')).replace(/(-webkit-)?mask[-a-z]*\s*:[^;}]*/g, '');
  return (visible.match(COLOR) || []).filter(c => parseColor(c));
}
/** The stylesheet without its `@media print` blocks. */
function withoutPrint(text: string): string {
  for (let at = text.search(/@media\s+print/); at >= 0; at = text.search(/@media\s+print/)) {
    let i = text.indexOf('{', at), depth = 0;
    for (; i < text.length; i++) { if (text[i] === '{') depth++; else if (text[i] === '}' && --depth === 0) break; }
    text = text.slice(0, at) + text.slice(i + 1);
  }
  return text;
}

function stylesheets(dir: string): string[] {
  return readdirSync(dir).flatMap(name => { const path = join(dir, name); return statSync(path).isDirectory() ? stylesheets(path) : path.endsWith('.css') ? [path] : []; });
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  const args = process.argv.slice(2);
  const check = args.includes('--check');
  const reportIndex = args.indexOf('--report');
  const reportPath = reportIndex >= 0 ? args[reportIndex + 1] : '';
  let files = args.filter((a, i) => !a.startsWith('--') && (reportIndex < 0 || i !== reportIndex + 1));
  if (!files.length) files = stylesheets(fileURLToPath(new URL('../packages/surfaces/src', import.meta.url)));
  let left = 0;
  for (const file of files) {
    const text = readFileSync(file, 'utf8');
    if (check) {
      const found = literalColors(text), sizes = literalFontSizes(text);
      if (found.length) { left += found.length; console.log(`${file}: ${found.length} literal colours (${[...new Set(found)].slice(0, 6).join(' ')})`); }
      if (sizes.length) { left += sizes.length; console.log(`${file}: ${sizes.length} pixel font sizes (${[...new Set(sizes)].slice(0, 6).join(' ')})`); }
      continue;
    }
    const next = tokenizeCss(text, file.replace(/^.*\/src\//, ''));
    if (next !== text) writeFileSync(file, next);
  }
  if (reportPath) writeFileSync(reportPath, JSON.stringify(report.sort((a, b) => b.distance - a.distance), null, 1));
  if (!check) console.log(`Mapped ${report.length} colours in ${files.length} stylesheets.`);
  if (check && left) process.exit(1);
}
export { FAMILIES };
