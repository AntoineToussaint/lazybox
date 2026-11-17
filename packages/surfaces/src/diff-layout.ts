import { isLine, type DiffLine, type DiffRow } from './review-model';

/** How a diff is laid out on the Changes page: the pure half. Side-by-side
 * rows built from parseDiff's rows (each cell keeps its row index, so review
 * comments anchor exactly as in the unified view), word-level highlights
 * between a removed line and the line that replaced it, long unchanged runs
 * folded, and the unchanged lines between hunks counted and, given the file,
 * expanded. Rendering lives in ReviewDiff.tsx and SplitDiff.tsx. */

export type DiffMode = 'unified' | 'split';
export const isDiffMode = (value: unknown): value is DiffMode => value === 'unified' || value === 'split';

// Side by side --------------------------------------------------------------

/** One visual row of the split view: a header spanning both columns, or the
 * old line on the left and the new line on the right (an index into rows;
 * a context line is the same index on both sides). */
export type SplitRow = { kind: 'full'; row: number } | { kind: 'pair'; old?: number; new?: number };

export function splitRows(rows: DiffRow[]): SplitRow[] {
  const out: SplitRow[] = [];
  let dels: number[] = [], adds: number[] = [];
  const flush = () => {
    for (let i = 0; i < Math.max(dels.length, adds.length); i++) out.push({ kind: 'pair', old: dels[i], new: adds[i] });
    dels = []; adds = [];
  };
  rows.forEach((row, index) => {
    if (!isLine(row)) { flush(); out.push({ kind: 'full', row: index }); return; }
    if (row.type === 'del') { if (adds.length) flush(); dels.push(index); return; }
    if (row.type === 'add') { adds.push(index); return; }
    flush(); out.push({ kind: 'pair', old: index, new: index });
  });
  flush();
  return out;
}

/** The row indexes a split row shows, in diff order, without repeats. */
export const splitIndexes = (row: SplitRow): number[] => row.kind === 'full' ? [row.row] : [...new Set([row.old, row.new].filter((value): value is number => value !== undefined))];

// Word highlights -----------------------------------------------------------

export type Range = { start: number; end: number };
const TOKEN = /[\p{L}\p{N}_]+|\s+|[^\p{L}\p{N}_\s]/gu;
export const tokenize = (text: string) => text.match(TOKEN) || [];
const MAX_LINE = 1000, MAX_CELLS = 60000;

/** What changed between a removed line and the line that replaced it, as
 * character ranges on each side. Undefined when there is nothing useful to
 * mark: identical lines, lines too long to compare, or lines so different
 * that marking words would only be noise (the whole line is the change). */
export function wordDiff(before: string, after: string): { old: Range[]; new: Range[] } | undefined {
  if (before === after || before.length > MAX_LINE || after.length > MAX_LINE) return;
  const a = tokenize(before), b = tokenize(after);
  if (!a.length || !b.length || a.length * b.length > MAX_CELLS) return;
  // Longest common subsequence of tokens, back to front.
  const width = b.length + 1, table = new Uint16Array((a.length + 1) * width);
  for (let i = a.length - 1; i >= 0; i--) for (let j = b.length - 1; j >= 0; j--)
    table[i * width + j] = a[i] === b[j] ? table[(i + 1) * width + j + 1] + 1 : Math.max(table[(i + 1) * width + j], table[i * width + j + 1]);
  const keepA = new Array<boolean>(a.length).fill(false), keepB = new Array<boolean>(b.length).fill(false);
  for (let i = 0, j = 0; i < a.length && j < b.length;) {
    if (a[i] === b[j]) { keepA[i++] = true; keepB[j++] = true; }
    else if (table[(i + 1) * width + j] >= table[i * width + j + 1]) i++;
    else j++;
  }
  const common = a.reduce((sum, token, i) => sum + (keepA[i] && token.trim() ? token.length : 0), 0);
  const meaningful = Math.max(before.replace(/\s+/g, '').length, after.replace(/\s+/g, '').length);
  if (!meaningful || common / meaningful < 0.3) return;
  const ranges = (tokens: string[], keep: boolean[]) => {
    // A space between two changed words is part of the change: one mark, not two.
    const changed = tokens.map((token, i) => !keep[i] || (!token.trim() && i > 0 && i < tokens.length - 1 && !keep[i - 1] && !keep[i + 1]));
    const out: Range[] = [];
    let at = 0;
    tokens.forEach((token, i) => {
      if (changed[i]) { const last = out.at(-1); if (last && last.end === at) last.end += token.length; else out.push({ start: at, end: at + token.length }); }
      at += token.length;
    });
    return out;
  };
  const old = ranges(a, keepA), next = ranges(b, keepB);
  if (!old.length && !next.length) return;
  return { old, new: next };
}

/** Word highlights for every removed line paired with the added line that
 * replaced it (the i-th removed line of a run with the i-th added line). */
export function wordHighlights(rows: DiffRow[]): Map<number, Range[]> {
  const out = new Map<number, Range[]>();
  for (const row of splitRows(rows)) {
    if (row.kind !== 'pair' || row.old === undefined || row.new === undefined || row.old === row.new) continue;
    const diff = wordDiff((rows[row.old] as DiffLine).text, (rows[row.new] as DiffLine).text);
    if (!diff) continue;
    if (diff.old.length) out.set(row.old, diff.old);
    if (diff.new.length) out.set(row.new, diff.new);
  }
  return out;
}

/** Wraps the characters in ranges of a highlighted line (HTML from
 * highlight.js: tags and escaped text) in <mark>, splitting the mark around
 * tags so the markup stays well formed. Positions count text characters; an
 * entity is one. */
export function markRanges(html: string, ranges: Range[], className = 'diff-word'): string {
  if (!ranges.length) return html;
  const open = `<mark class="${className}">`;
  let out = '', position = 0, next = 0, marking = false, i = 0;
  while (i < html.length) {
    if (html[i] === '<') {
      const end = html.indexOf('>', i);
      const tag = html.slice(i, end < 0 ? html.length : end + 1);
      out += marking ? `</mark>${tag}${open}` : tag;
      i += tag.length;
      continue;
    }
    let character = html[i];
    if (character === '&') { const end = html.indexOf(';', i); if (end > i && end - i < 10) character = html.slice(i, end + 1); }
    i += character.length;
    while (next < ranges.length && position >= ranges[next].end) next++;
    const inside = next < ranges.length && position >= ranges[next].start;
    if (inside !== marking) { out += inside ? open : '</mark>'; marking = inside; }
    out += character; position++;
  }
  if (marking) out += '</mark>';
  return out.replaceAll(`${open}</mark>`, '');
}

export const escapeHtml = (text: string) => text.replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]!);

// Folding long unchanged runs --------------------------------------------------

/** Unchanged lines inside a hunk, hidden until asked for. Identified by the
 * new-side lines they cover, so a fold survives expanding the gaps around it. */
export type Fold = { id: string; startLine: number; endLine: number; count: number };

/** Runs of unchanged lines longer than `context` lines either side of a
 * change fold away when at least `minimum` lines would be hidden. Git's own
 * hunks already leave out most unchanged code; this folds what a diff with
 * more context, or a large expanded gap, still shows. Rows in `keep` (a
 * comment, a selection) are never hidden. */
export function contextFolds(rows: DiffRow[], context = 3, minimum = 4, keep: ReadonlySet<number> = new Set()): Fold[] {
  const out: Fold[] = [];
  let start = -1;
  const close = (end: number) => {
    if (start < 0) return;
    const first = start; start = -1;
    const before = isLine(rows[first - 1]) && rows[first - 1].hunk === rows[first].hunk;
    const after = isLine(rows[end + 1]) && rows[end + 1].hunk === rows[end].hunk;
    const from = first + (before ? context : 0), to = end - (after ? context : 0);
    if (to - from + 1 < minimum) return;
    for (let i = from; i <= to; i++) if (keep.has(i)) return;
    const startLine = (rows[from] as DiffLine).newLine!, endLine = (rows[to] as DiffLine).newLine!;
    out.push({ id: `fold:${startLine}`, startLine, endLine, count: to - from + 1 });
  };
  rows.forEach((row, index) => {
    if (isLine(row) && row.type === 'context') { if (start < 0 || rows[index - 1]?.hunk !== row.hunk) { close(index - 1); start = index; } return; }
    close(index - 1);
  });
  close(rows.length - 1);
  return out;
}

/** Which rows a set of folds hides, and the first row of each (where its "N unchanged lines" row goes). */
export function foldedRows(rows: DiffRow[], folds: Fold[], expanded: ReadonlySet<string>): { hidden: Set<number>; starts: Map<number, Fold> } {
  const hidden = new Set<number>(), starts = new Map<number, Fold>();
  const active = folds.filter(fold => !expanded.has(fold.id));
  if (!active.length) return { hidden, starts };
  const seen = new Set<string>();
  rows.forEach((row, index) => {
    if (!isLine(row) || row.type !== 'context') return;
    const fold = active.find(f => row.newLine! >= f.startLine && row.newLine! <= f.endLine);
    if (!fold) return;
    if (!seen.has(fold.id)) { seen.add(fold.id); starts.set(index, fold); }
    hidden.add(index);
  });
  return { hidden, starts };
}

// Hunks and the gaps between them -----------------------------------------

const HUNK = /^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@(.*)$/;
/** A hunk with its first old and new line; for a side it has no lines on, the line after the change. */
type Hunk = { oldFirst: number; newFirst: number; section: string; lines: string[] };
type Parsed = { preamble: string[]; hunks: Hunk[] };

const counts = (lines: string[]) => {
  let old = 0, next = 0;
  for (const line of lines) { if (line.startsWith('\\')) continue; if (line[0] !== '+') old++; if (line[0] !== '-') next++; }
  return { old, new: next };
};
function parseHunks(text: string): Parsed | undefined {
  if (text.startsWith('New file: ')) return;
  const preamble: string[] = [], raw: { oldStart: number; newStart: number; section: string; lines: string[] }[] = [];
  for (const line of text.replace(/\n$/, '').split('\n')) {
    const header = HUNK.exec(line);
    if (header) { raw.push({ oldStart: Number(header[1]), newStart: Number(header[3]), section: header[5], lines: [] }); continue; }
    if (raw.length) raw.at(-1)!.lines.push(line); else preamble.push(line);
  }
  if (!raw.length) return;
  // Git numbers an empty side by the line before the change.
  return { preamble, hunks: raw.map(({ oldStart, newStart, section, lines }) => { const n = counts(lines); return { oldFirst: n.old ? oldStart : oldStart + 1, newFirst: n.new ? newStart : newStart + 1, section, lines }; }) };
}
/** The last new-side line a hunk covers (for one without new lines, the line before it). */
const newEnd = (hunk: Hunk) => hunk.newFirst + counts(hunk.lines).new - 1;

/** Unchanged lines hidden before each hunk, by hunk number (from 1). */
export function hunkGaps(text: string): Map<number, number> {
  const out = new Map<number, number>();
  const parsed = parseHunks(text);
  if (!parsed) return out;
  parsed.hunks.forEach((hunk, index) => {
    const gap = hunk.newFirst - (index ? newEnd(parsed.hunks[index - 1]) : 0) - 1;
    if (gap > 0) out.set(index + 1, gap);
  });
  return out;
}
/** Lines after the last hunk, given the file's line count. */
export function trailingGap(text: string, fileLines: number): number {
  const parsed = parseHunks(text);
  if (!parsed) return 0;
  const last = parsed.hunks.at(-1)!;
  if (last.lines.some(line => line.startsWith('\\'))) return 0;
  return Math.max(0, fileLines - newEnd(last));
}

export const fileLines = (text: string) => text === '' ? [] : text.replace(/\n$/, '').split('\n');

/** The diff with the unchanged lines of gap `gap` (before hunk `gap`, from 1;
 * one past the last hunk is the rest of the file) shown as context, taken
 * from the file's new side. Hunks that meet are joined. Throws when the file
 * no longer matches the diff (it changed since the diff was read). */
export function expandGap(text: string, file: string[], gap: number): string {
  const parsed = parseHunks(text);
  if (!parsed) return text;
  const hunks = parsed.hunks;
  // The diff's new side must be the file the lines are read from.
  const same = (line: string | undefined, row: string) => line !== undefined && line.replace(/\r$/, '') === row.replace(/\r$/, '');
  for (const hunk of hunks) {
    let line = hunk.newFirst;
    for (const row of hunk.lines) {
      if (row[0] === '-' || row.startsWith('\\')) continue;
      if (!same(file[line - 1], row.slice(1))) throw new Error('The file changed since this diff was read. Refresh the changes to see it.');
      line++;
    }
  }
  const context = (from: number, to: number) => file.slice(from - 1, to).map(line => ' ' + line);
  if (gap > hunks.length) {
    const last = hunks.at(-1)!;
    last.lines.push(...context(newEnd(last) + 1, file.length));
  } else {
    const index = gap - 1, hunk = hunks[index];
    const from = index ? newEnd(hunks[index - 1]) + 1 : 1, added = hunk.newFirst - from;
    if (added > 0) { hunk.lines.unshift(...context(from, hunk.newFirst - 1)); hunk.oldFirst -= added; hunk.newFirst -= added; }
    if (index) { hunks[index - 1].lines.push(...hunk.lines); hunks.splice(index, 1); }
  }
  const header = (hunk: Hunk) => { const n = counts(hunk.lines); return `@@ -${n.old ? hunk.oldFirst : hunk.oldFirst - 1},${n.old} +${n.new ? hunk.newFirst : hunk.newFirst - 1},${n.new} @@${hunk.section}`; };
  return [...parsed.preamble, ...hunks.flatMap(hunk => [header(hunk), ...hunk.lines])].join('\n') + '\n';
}

/** Rows of hunk headers, in order: where n and p go. */
export const hunkRows = (rows: DiffRow[]) => rows.flatMap((row, index) => row.kind === 'hunk' ? [index] : []);

/** A short, stable key for a diff's text: a file whose diff changes since it was marked viewed is no longer viewed. */
export function contentKey(text: string): string {
  let hash = 0x811c9dc5;
  for (let i = 0; i < text.length; i++) { hash ^= text.charCodeAt(i); hash = Math.imul(hash, 0x01000193); }
  return (hash >>> 0).toString(16).padStart(8, '0') + ':' + text.length.toString(36);
}
