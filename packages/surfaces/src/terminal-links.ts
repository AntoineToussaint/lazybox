/**
 * Web links in a terminal's output. A long address (a sign-in URL of several
 * hundred characters) wraps across rows of the terminal: it is found in the
 * whole logical line, the rows the terminal joined, so hovering or clicking
 * any part of it gives the whole address. Pure: `TerminalPanel.tsx` reads the
 * rows from xterm.js, `scripts/terminal-links.test.ts` pins it.
 */

/** One row of the terminal as text, and the cell (0-based column) each character of it is in. */
export type TerminalRow = { text: string; cells: number[] };
/** A link: its address, and its first and last cells as (row index in the line, column), inclusive. */
export type TerminalLink = { url: string; start: { row: number; col: number }; end: { row: number; col: number } };

const URL = /\bhttps?:\/\/[^\s<>"'`\u0000-\u001f\u007f]+/gi;
const CLOSERS: Record<string, string> = { ')': '(', ']': '[', '}': '{' };

/** An address as written in text, less what ends the sentence around it: trailing punctuation, an unmatched closing bracket. */
export function trimLink(text: string): string {
  let url = text;
  for (;;) {
    const last = url.at(-1) || '';
    if (/[.,;:!?*'"]/.test(last)) { url = url.slice(0, -1); continue; }
    const open = CLOSERS[last];
    if (open && url.split(open).length <= url.split(last).length - 1) { url = url.slice(0, -1); continue; }
    return url;
  }
}

/** The web links in a logical line, given as the rows it wraps across, oldest first. */
export function linksInRows(rows: readonly TerminalRow[]): TerminalLink[] {
  const text = rows.map(row => row.text).join('');
  // Where each character of the joined text is: its row, and its cell.
  const at: { row: number; col: number }[] = [];
  rows.forEach((row, index) => { for (let i = 0; i < row.text.length; i++) at.push({ row: index, col: row.cells[i] ?? i }); });
  const out: TerminalLink[] = [];
  for (const match of text.matchAll(URL)) {
    const url = trimLink(match[0]);
    if (url.length <= 'https://'.length) continue;
    try { new globalThis.URL(url); } catch { continue; }
    const from = match.index!, to = from + url.length - 1;
    out.push({ url, start: at[from], end: at[to] });
  }
  return out;
}
