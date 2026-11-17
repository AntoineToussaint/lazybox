import { memo, type CSSProperties, type ReactNode } from 'react';

/**
 * Terminal output as people see it in a terminal: SGR colours and styles are
 * rendered, other control sequences (cursor moves, erase line, titles) are
 * dropped, and a carriage return overwrites the line like a progress bar does.
 */

export type Style = { fg?: string; bg?: string; bold?: boolean; dim?: boolean; italic?: boolean; underline?: boolean; inverse?: boolean };
export type Segment = { text: string; style: Style };

// The 16 named colours come from the theme (tokens ansi-0 … ansi-15), so
// output stays readable in light themes; 256-colour and truecolour values are
// the program's own and are kept.
const base16 = Array.from({ length: 8 }, (_, i) => `var(--ansi-${i})`);
const bright16 = Array.from({ length: 8 }, (_, i) => `var(--ansi-${i + 8})`);

function color256(n: number): string | undefined {
  if (n < 0 || n > 255) return undefined;
  if (n < 8) return base16[n];
  if (n < 16) return bright16[n - 8];
  if (n < 232) {
    const v = n - 16, steps = [0, 95, 135, 175, 215, 255];
    return `rgb(${steps[Math.floor(v / 36)]},${steps[Math.floor(v / 6) % 6]},${steps[v % 6]})`;
  }
  const level = 8 + (n - 232) * 10;
  return `rgb(${level},${level},${level})`;
}

function applySGR(style: Style, params: number[]): Style {
  const next = { ...style };
  if (!params.length) params = [0];
  for (let i = 0; i < params.length; i++) {
    const p = params[i];
    if (p === 0) { for (const key of Object.keys(next) as (keyof Style)[]) delete next[key]; }
    else if (p === 1) next.bold = true;
    else if (p === 2) next.dim = true;
    else if (p === 3) next.italic = true;
    else if (p === 4) next.underline = true;
    else if (p === 7) next.inverse = true;
    else if (p === 22) { delete next.bold; delete next.dim; }
    else if (p === 23) delete next.italic;
    else if (p === 24) delete next.underline;
    else if (p === 27) delete next.inverse;
    else if (p >= 30 && p <= 37) next.fg = base16[p - 30];
    else if (p >= 90 && p <= 97) next.fg = bright16[p - 90];
    else if (p === 39) delete next.fg;
    else if (p >= 40 && p <= 47) next.bg = base16[p - 40];
    else if (p >= 100 && p <= 107) next.bg = bright16[p - 100];
    else if (p === 49) delete next.bg;
    else if (p === 38 || p === 48) {
      const key = p === 38 ? 'fg' : 'bg';
      if (params[i + 1] === 5) { const c = color256(params[i + 2]); if (c) next[key] = c; i += 2; }
      else if (params[i + 1] === 2) { const [r, g, b] = params.slice(i + 2, i + 5); if ([r, g, b].every(v => v >= 0 && v <= 255)) next[key] = `rgb(${r},${g},${b})`; i += 4; }
    }
  }
  return next;
}

// CSI (ESC [ … final), OSC (ESC ] … BEL or ESC \), and other two-byte escapes.
const ESCAPES = /\x1b\[([0-9;:?<>=]*)([@-~])|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[@-Z\\-_]/g;

/**
 * One line (no newline in it) of styled segments, starting in `style`; returns
 * the style in force after it, so a caller can parse output a line at a time
 * as it streams. A carriage return keeps only what was written last.
 */
export function parseAnsiLine(rawLine: string, style: Style = {}): { line: Segment[]; style: Style } {
  // A progress bar redraws its line after \r: what is visible is the last redraw.
  const parts = rawLine.split('\r');
  let visible = parts.pop() ?? '';
  while (visible === '' && parts.length) visible = parts.pop() ?? '';
  // Styles set in overwritten parts still apply afterwards.
  for (const hidden of parts) for (const match of hidden.matchAll(ESCAPES)) if (match[2] === 'm') style = applySGR(style, match[1].split(/[;:]/).filter(Boolean).map(Number));
  const line: Segment[] = [];
  let last = 0;
  for (const match of visible.matchAll(ESCAPES)) {
    if (match.index! > last) line.push({ text: visible.slice(last, match.index), style });
    if (match[2] === 'm') style = applySGR(style, match[1].split(/[;:]/).filter(Boolean).map(Number));
    last = match.index! + match[0].length;
  }
  if (last < visible.length) line.push({ text: visible.slice(last), style });
  return { line, style };
}

/** Lines of styled segments. Carriage returns keep only what was written last on a line. */
export function parseAnsi(input: string): Segment[][] {
  const lines: Segment[][] = [];
  let style: Style = {};
  for (const rawLine of input.split('\n')) { const parsed = parseAnsiLine(rawLine, style); style = parsed.style; lines.push(parsed.line); }
  return lines;
}

/** The text a person reads, without escape sequences: what search and copy use. */
export function stripAnsi(input: string): string {
  return parseAnsi(input).map(line => line.map(segment => segment.text).join('')).join('\n');
}

/** The inline style of a segment: theme tokens for the 16 named colours. */
export const segmentStyle = (style: Style): CSSProperties | undefined => {
  if (!Object.keys(style).length) return undefined;
  const fg = style.inverse ? style.bg || 'var(--code-bg)' : style.fg, bg = style.inverse ? style.fg || 'var(--code-text)' : style.bg;
  return { color: fg, background: bg, fontWeight: style.bold ? 600 : undefined, opacity: style.dim ? 0.65 : undefined, fontStyle: style.italic ? 'italic' : undefined, textDecoration: style.underline ? 'underline' : undefined };
};

/** Renders terminal output; `filter` keeps only lines whose visible text contains it. */
export const AnsiText = memo(function AnsiText({ text, filter }: { text: string; filter?: string }) {
  const needle = filter?.trim().toLocaleLowerCase();
  const out: ReactNode[] = [];
  parseAnsi(text).forEach((line, index) => {
    if (needle && !line.map(s => s.text).join('').toLocaleLowerCase().includes(needle)) return;
    if (out.length) out.push('\n');
    out.push(<span key={index}>{line.map((segment, k) => <span key={k} style={segmentStyle(segment.style)}>{segment.text}</span>)}</span>);
  });
  return <>{out}</>;
});
