/**
 * Pure helpers for reading an artifact as a document: its outline (table of
 * contents with stable heading ids), callouts, reading time, the title it
 * repeats, and a block-level diff for the rendered comparison. No React.
 */
import { lineDiff } from './artifact-model';

export type TocEntry = { id: string; text: string; level: number; line: number };

const FENCE = /^\s{0,3}(`{3,}|~{3,})/;
/** Inline Markdown to its plain text: emphasis, code, links, images, footnote references, HTML. */
export function plainText(markdown: string): string {
  return markdown
    .replace(/!\[([^\]]*)\]\([^)]*\)/g, '$1')
    .replace(/\[\^[^\]]+\]/g, '')
    .replace(/\[([^\]]*)\]\([^)]*\)/g, '$1')
    .replace(/<[^>]+>/g, '')
    .replace(/(`+)(.*?)\1/g, '$2')
    .replace(/(\*\*|__|~~)(.*?)\1/g, '$2')
    .replace(/(^|[^\w*])[*_](\S(?:.*?\S)?)[*_](?=[^\w*]|$)/g, '$1$2')
    .replace(/\\([\\`*_{}[\]()#+\-.!~|>])/g, '$1')
    .replace(/\s+/g, ' ')
    .trim();
}

/** A heading id: lowercase words joined by dashes, unique within the document. */
export function slugger() {
  const seen = new Map<string, number>();
  return (text: string) => {
    const base = text.toLowerCase().normalize('NFKD').replace(/[̀-ͯ]/g, '').replace(/[^\p{L}\p{N}\s-]/gu, '').trim().replace(/\s+/g, '-') || 'section';
    const count = seen.get(base) || 0;
    seen.set(base, count + 1);
    return count ? `${base}-${count}` : base;
  };
}

/**
 * Every ATX and setext heading outside code fences, with the 1-based source
 * line the renderer reports for it, so a rendered heading finds its id by line.
 */
export function extractToc(markdown: string): TocEntry[] {
  const lines = markdown.split('\n'), slug = slugger(), out: TocEntry[] = [];
  let fence = '';
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    const open = FENCE.exec(line);
    if (open) { if (!fence) fence = open[1][0]; else if (open[1][0] === fence) fence = ''; continue; }
    if (fence) continue;
    const atx = /^\s{0,3}(#{1,6})\s+(.*?)(?:\s+#+)?\s*$/.exec(line);
    let level = 0, text = '';
    if (atx) { level = atx[1].length; text = plainText(atx[2]); }
    else if (line.trim() && !/^\s{0,3}([-*+>]|\d+[.)]|\|)/.test(line) && /^\s{0,3}(=+|-+)\s*$/.test(lines[i + 1] || '') && (i === 0 || !lines[i - 1].trim())) {
      level = lines[i + 1].trim()[0] === '=' ? 1 : 2; text = plainText(line);
    }
    if (level && text) out.push({ id: slug(text), text, level, line: i + 1 });
  }
  return out;
}

/** The entries a table of contents shows: the two levels below the top one used, when there are enough to need one. */
export function tocOutline(entries: TocEntry[], minimum = 3): TocEntry[] {
  // A single leading h1 is the document's title, not a section.
  const sections = entries.filter((e, i) => !(e.level === 1 && entries.filter(x => x.level === 1).length === 1 && i === 0));
  if (!sections.length) return [];
  const top = Math.min(...sections.map(e => e.level));
  const shown = sections.filter(e => e.level <= top + 1);
  return shown.length >= minimum ? shown : [];
}

/** Minutes to read at about 230 words a minute; code counts at a third. */
export function readingMinutes(markdown: string): number {
  let words = 0, code = 0, fence = '';
  for (const line of markdown.split('\n')) {
    const open = FENCE.exec(line);
    if (open) { if (!fence) fence = open[1][0]; else if (open[1][0] === fence) fence = ''; continue; }
    const count = line.split(/\s+/).filter(word => /[\p{L}\p{N}]/u.test(word)).length;
    if (fence) code += count; else words += count;
  }
  return Math.max(1, Math.round((words + code / 3) / 230));
}

/**
 * A title as words for comparing: lowercase, accents, punctuation and
 * parenthesised or bracketed asides ("(final)", "[draft]") gone.
 */
function titleWords(text: string): string[] {
  return plainText(text).toLowerCase().normalize('NFKD').replace(/[̀-ͯ]/g, '')
    .replace(/\([^)]*\)|\[[^\]]*\]/g, ' ')
    .replace(/[^\p{L}\p{N}\s]/gu, ' ')
    .split(/\s+/).filter(Boolean);
}

/**
 * Whether a document's opening heading says the same as the artifact's title,
 * so the page would show the same title twice. Agents often publish a title
 * that dresses the heading up or down: "(final)" added, a subtitle after a
 * colon or a dash, a word changed. The heading repeats the title when, as
 * words: they are equal; one starts with the other and the shorter has at
 * least two words and half of the longer's; or they share at least 80% of
 * their words (Dice). "Plan" against "Plan for the rollout" is not a repeat.
 */
export function repeatsTitle(heading: string, title: string): boolean {
  const a = titleWords(heading), b = titleWords(title);
  if (!a.length || !b.length) return false;
  if (a.join(' ') === b.join(' ')) return true;
  const [short, long] = a.length <= b.length ? [a, b] : [b, a];
  if (short.length >= 2 && short.length * 2 >= long.length && short.every((word, i) => word === long[i])) return true;
  const counts = new Map<string, number>();
  for (const word of long) counts.set(word, (counts.get(word) || 0) + 1);
  let shared = 0;
  for (const word of short) { const left = counts.get(word) || 0; if (left) { shared++; counts.set(word, left - 1); } }
  return short.length >= 3 && (2 * shared) / (a.length + b.length) >= 0.8;
}

/**
 * When the document opens with an h1 that repeats the artifact's title (the
 * same, or nearly: see repeatsTitle), the page shows one title, the
 * artifact's, in its title block: the h1's line is blanked (keeping line
 * numbers, so headings still find their ids by line). An h1 that says
 * something else stays.
 */
export function withoutRepeatedTitle(markdown: string, title: string): string {
  const lines = markdown.split('\n');
  const first = lines.findIndex(line => line.trim());
  if (first < 0) return markdown;
  const atx = /^\s{0,3}#\s+(.*?)(?:\s+#+)?\s*$/.exec(lines[first]);
  const setext = !atx && /^\s{0,3}=+\s*$/.test(lines[first + 1] || '');
  const text = atx ? atx[1] : setext ? lines[first] : '';
  if (!text || !repeatsTitle(text, title)) return markdown;
  lines[first] = '';
  if (setext) lines[first + 1] = '';
  return lines.join('\n');
}

export const CALLOUT_KINDS = ['note', 'tip', 'important', 'warning', 'caution', 'decision'] as const;
export type CalloutKind = (typeof CALLOUT_KINDS)[number];
export const calloutLabel: Record<CalloutKind, string> = { note: 'Note', tip: 'Tip', important: 'Important', warning: 'Warning', caution: 'Caution', decision: 'Decision' };
const calloutWords: Record<string, CalloutKind> = { note: 'note', info: 'note', tip: 'tip', hint: 'tip', important: 'important', warning: 'warning', warn: 'warning', caution: 'caution', danger: 'caution', decision: 'decision', decided: 'decision' };

/**
 * A callout from the start of a blockquote's text: GitHub's `[!NOTE]` or a
 * bold label (`**Warning**`, `**Decision:**`). `label` is the bold text when
 * it says more than the kind ("Decision: ship it"), `rest` what follows it.
 */
export function parseCallout(start: string): { kind: CalloutKind; title: string; rest: string } | undefined {
  const github = /^\s*\[!(\w+)\]\s*(.*)$/s.exec(start);
  if (github) { const kind = calloutWords[github[1].toLowerCase()]; return kind && { kind, title: '', rest: github[2] }; }
  const bold = /^\s*(?:\*\*|__)([^*_\n]+?)(?:\*\*|__)\s*:?\s*(.*)$/s.exec(start);
  if (!bold) return undefined;
  const label = bold[1].trim().replace(/:$/, '');
  const word = /^(\w+)\b[\s:—–-]*(.*)$/.exec(label);
  const kind = word && calloutWords[word[1].toLowerCase()];
  return kind ? { kind, title: word![2].trim(), rest: bold[2] } : undefined;
}

/** Table cells that read as a status or a level get a quiet pill. */
const tones: Record<string, 'ok' | 'info' | 'warn' | 'danger' | 'muted'> = {
  done: 'ok', complete: 'ok', completed: 'ok', shipped: 'ok', merged: 'ok', yes: 'ok', passed: 'ok', resolved: 'ok', accepted: 'ok', approved: 'ok',
  'in progress': 'info', 'in review': 'info', active: 'info', ongoing: 'info', started: 'info',
  planned: 'muted', todo: 'muted', 'to do': 'muted', 'not started': 'muted', proposed: 'muted', draft: 'muted', 'n/a': 'muted',
  blocked: 'danger', failed: 'danger', 'at risk': 'warn', 'on hold': 'warn', rejected: 'danger', no: 'danger',
  low: 'ok', medium: 'warn', high: 'danger', critical: 'danger',
};
export const cellTone = (text: string) => tones[text.trim().toLowerCase()];

/**
 * Markdown split into top-level blocks (paragraphs, headings, lists, tables,
 * fenced code, quotes), each with its source text. Blank lines separate
 * blocks except inside a fence; a list or quote keeps its blank-line runs.
 */
export function markdownBlocks(markdown: string): string[] {
  const lines = markdown.replace(/\n+$/, '').split('\n'), out: string[] = [];
  let current: string[] = [], fence = '';
  const flush = () => { if (current.some(line => line.trim())) out.push(current.join('\n')); current = []; };
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i], open = FENCE.exec(line);
    if (fence) { current.push(line); if (open && open[1][0] === fence) { fence = ''; flush(); } continue; }
    if (open) { flush(); fence = open[1][0]; current.push(line); continue; }
    if (/^\s{0,3}#{1,6}\s/.test(line)) { flush(); current.push(line); flush(); continue; }
    if (!line.trim()) {
      // A list item continues after a blank line when the next line is indented or another item.
      const next = lines.slice(i + 1).find(l => l.trim()) || '';
      const inList = current.length && /^\s{0,3}([-*+]|\d+[.)])\s/.test(current[0]);
      if (inList && /^(\s{2,}|\s{0,3}([-*+]|\d+[.)])\s)/.test(next)) { current.push(line); continue; }
      flush(); continue;
    }
    current.push(line);
  }
  flush();
  return out;
}

export type BlockChange = { kind: 'same'; before: string; after: string } | { kind: 'changed'; before: string[]; after: string[] };
/**
 * A block-level diff for the rendered comparison: unchanged blocks, and runs
 * of removed blocks paired with the added blocks that replaced them.
 */
export function blockDiff(before: string, after: string): BlockChange[] {
  // Each block becomes one "line" for the line diff, its own newlines escaped.
  const encode = (blocks: string[]) => blocks.map(block => block.replace(/\\/g, '\\\\').replace(/\n/g, '\\n')).join('\n');
  const decode = (line: string) => line.replace(/\\(\\|n)/g, (_, c) => c === 'n' ? '\n' : '\\');
  const a = markdownBlocks(before), b = markdownBlocks(after);
  const out: BlockChange[] = [];
  let pending: { kind: 'changed'; before: string[]; after: string[] } | undefined;
  for (const line of lineDiff(encode(a), encode(b))) {
    const text = decode(line.text);
    if (line.kind === 'same') { pending = undefined; out.push({ kind: 'same', before: text, after: text }); continue; }
    if (!pending) { pending = { kind: 'changed', before: [], after: [] }; out.push(pending); }
    (line.kind === 'remove' ? pending.before : pending.after).push(text);
  }
  return out;
}
