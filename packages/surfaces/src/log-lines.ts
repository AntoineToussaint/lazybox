import { parseAnsiLine, type Segment, type Style } from './ansi';
import { fileReference, looksLikeFileName } from './file-reference';

/**
 * Reading command output as a log: where each line's timestamp, source
 * (`api/web  > …`, `web-1  | …`, `[codefly] …`), level and message are, which
 * lines announce that something is up (`Running on :8080`), and what in them
 * can be opened (web addresses, local ports, files with a line).
 *
 * Every rule is a heuristic and none is lossy: a line keeps its text and its
 * terminal colours, and only says where its parts are, so the console can
 * lay them out in columns, colour and fold them, and Copy still gives the
 * bytes as the program wrote them. Lines are read once, as they arrive
 * (`LogModel`); what is shown (filters, folding) is derived from them
 * cheaply on every change (`logRows`).
 */

export type Level = 'error' | 'warn' | 'info' | 'debug';
export type Span = { start: number; end: number };
export type LinkToken = Span & (
  | { kind: 'url'; url: string }
  | { kind: 'port'; url: string; port: number }
  | { kind: 'file'; path: string; line?: number; column?: number });
/** A long bracketed list (or run of addresses) the console shows folded. `start`/`end` include the brackets. */
export type ListSpan = Span & { items: number; label: string; bracketed: boolean };
export type LogLine = {
  /** Stable for the life of the line, also while it is still arriving. */
  id: number;
  /** What a person reads: no escape sequences. */
  text: string;
  segments: Segment[];
  time?: Span & { label: string };
  /** The source's name, and the separator after it (`>`, `|`, `:` or `]`), shown in the source column. */
  source?: Span & { name: string; sep: string; open?: string };
  /** `>>` before a milestone. */
  marker?: Span;
  /** Where the message starts. */
  message: number;
  level?: Level;
  /** The level as written (`(WARN)`, `level=warn`): shown as a badge; `label` is what the badge says. */
  levelSpan?: Span & { label: string };
  milestone: boolean;
  /** An indented line under another (a stack frame, a wrapped value): it belongs to the line above. */
  continuation: boolean;
  /** The source the line belongs to, for filtering: its own, or the one above for a continuation. */
  owner?: string;
  links: LinkToken[];
  lists: ListSpan[];
  /** The line without its timestamp: equal keys fold into ×N. */
  key: string;
  /** For a warning, its kind (`(Flow.checkLoopback) …` with numbers and lists left out): repeats are noise. */
  noiseKey: string;
  /** The key with numbers left out: progress lines ("still starting… 10s") fold when noise is hidden. */
  progressKey: string;
};

/** How many source colours the theme has (`--log-source-1` … `--log-source-6`). */
export const LOG_SOURCE_COLOURS = 6;

const LEVEL_WORD = 'ERROR|ERR|WARN(?:ING)?|INFO|DEBUG|DBG|TRACE|FATAL|PANIC|NOTICE|CRIT(?:ICAL)?';
const LEVEL_NAME = new RegExp(`^(?:${LEVEL_WORD})$`, 'i');
function levelOf(word: string): Level | undefined {
  const w = word.toUpperCase();
  if (/^(ERROR|ERR|ERR!|FATAL|PANIC|CRIT|CRITICAL|FAIL|E)$/.test(w)) return 'error';
  if (/^(WARN|WARNING|W)$/.test(w)) return 'warn';
  if (/^(INFO|NOTICE|I)$/.test(w)) return 'info';
  if (/^(DEBUG|DBG|TRACE|D)$/.test(w)) return 'debug';
  return undefined;
}

// [21:28:14], 21:28:14.123, 2026-11-16T21:28:14Z, 2026/11/16 21:28:14 (Go's log package).
const TIME_PLAIN = /^\[?(?:\d{4}[-/]\d{2}[-/]\d{2}[T ])?(\d{2}:\d{2}:\d{2})(?:[.,]\d{1,9})?(?:Z|[+-]\d{2}:?\d{2})?\]?(?=\s|$)/;
// docker compose: `web-1  | …` (v2) or `web_1  | …` (v1); foreman/overmind `web.1  | …`.
const COMPOSE = /^([A-Za-z0-9][\w.-]*?[-_.]\d+)( +)(\|)( |$)/;
// Codefly and other runners: `fte-tracker/web     > …`, `authority/edge      | …`, `web    | …`.
const SERVICE = /^([A-Za-z@][\w.@-]*(?:\/[\w.@-]+)*)( +)([>|])( |$)/;
// A runner's own announcement: `>> fte-tracker/web: Running on :57771`.
const ANNOUNCE = /^(>>)( +)([A-Za-z@][\w.@/-]*)(:)( |$)/;
// `[codefly] …`, `[web] …`, concurrently's `[0] …`.
const BRACKET = /^(\[)([A-Za-z][\w.@/:-]{0,39}|\d{1,2})(\])( +|$)/;
// pnpm -r / --parallel: `packages/web dev: …`, `apps/api build$ tsc`.
const PNPM = /^((?:@[\w.-]+\/)?[\w.-]+(?:\/[\w.-]+)+) ([\w:.-]+)([:$])( |$)/;
// turbo: `web:dev: …`.
const TURBO = /^([A-Za-z@][\w@.-]*(?:\/[\w.-]+)?:[A-Za-z][\w-]*)(:)( |$)/;

const LEVEL_WRAPPED = new RegExp(`^(?:\\((${LEVEL_WORD})\\)|\\[(${LEVEL_WORD})\\])(?=[\\s:]|$)`, 'i');
// Bare and upper case (`WARN something`, logrus's `INFO[0000]`), or any case with a colon (`error: …`, `error[E0308]:`).
const LEVEL_BARE = new RegExp(`^(?:(${LEVEL_WORD})(?:\\[\\d+\\])?:?(?=\\s|$)|(error|warning|warn|info|debug|fatal|panic)(?:\\[[\\w-]+\\])?:(?=\\s|$))`);
const LEVEL_NPM = /^npm (ERR!|WARN|warn|error|notice)(?=\s|$)/;
const LEVEL_GO_TEST = /^(?:--- (FAIL)\b|(FAIL)\b|(panic):)/;
const LEVEL_FIELD = /(?<![\w-])(?:(level|lvl|severity)=("?)([A-Za-z]+)\2|"(level|severity)"\s*:\s*"([A-Za-z]+)")/i;

const MILESTONE = /\b(?:running on|running at|listening on|listening at|serving on|serving at|ready in|ready on|started (?:server )?on|server started|server running|available at|accepting connections)\b|\bLocal:\s+https?:\/\//i;

const URL_RE = /\bhttps?:\/\/[^\s<>"'`]+/g;
const LOCAL_PORT = /(?<![\w.:/\]\[@-])(localhost|127\.0\.0\.1|0\.0\.0\.0|\[::1?\])?:(\d{2,5})(?![\w.:/])/g;
const PORT_WORD = /\bport\b[\s:=]+(\d{2,5})\b/gi;
const FILE_RE = /(?<![\w./@~+-])((?:~|\.{1,2})?\/?(?:[\w.@+-]+\/)*[\w@+-][\w.@+-]*\.[A-Za-z][\w-]*|(?:[\w.@+-]+\/)+(?:Dockerfile|Makefile)):(\d+)(?::(\d+))?(?![\d.])/g;
const ADDRESS = /^(?:\d{1,3}(?:\.\d{1,3}){3}|\[[^\]\s]+\]|[0-9A-Fa-f]*:[0-9A-Fa-f:]*[0-9A-Fa-f]|localhost)(?::\d{1,5})?$/;
const ADDRESS_RUN = /(?<![\w.:])(?:\d{1,3}(?:\.\d{1,3}){3}(?::\d{1,5})?,?\s+){3,}\d{1,3}(?:\.\d{1,3}){3}(?::\d{1,5})?(?![\w.:])/g;
const ELLIPSIS = /^(?:…|\.\.\.)$/;

function trimUrl(url: string): string {
  let out = url.replace(/[.,;:!?'"]+$/, '');
  // A closing bracket belongs to the address only when it opened one.
  for (const [open, close] of [['(', ')'], ['[', ']'], ['{', '}']] as const) {
    while (out.endsWith(close) && out.split(open).length < out.split(close).length) out = out.slice(0, -1).replace(/[.,;:!?'"]+$/, '');
  }
  return out;
}

/** Long bracketed lists (`answers=[a b c d …]`), and runs of four or more addresses. */
function findLists(text: string, from: number): ListSpan[] {
  const lists: ListSpan[] = [];
  for (let i = text.indexOf('[', from); i >= 0 && i < text.length; i = text.indexOf('[', i + 1)) {
    let depth = 0, close = -1;
    for (let j = i; j < text.length; j++) {
      const c = text[j];
      if (c === '[') depth++;
      else if (c === ']' && --depth === 0) { close = j; break; }
    }
    if (close < 0) break;
    const inner = text.slice(i + 1, close);
    const items: string[] = []; let item = '', level = 0;
    for (const c of inner) {
      if (c === '[') level++; else if (c === ']') level--;
      if (level === 0 && /[\s,]/.test(c)) { if (item) items.push(item); item = ''; } else item += c;
    }
    if (item) items.push(item);
    const real = items.filter(value => !ELLIPSIS.test(value));
    if ((real.length >= 4 && inner.length >= 36) || (real.length >= 3 && inner.length >= 48)) {
      const addresses = real.every(value => ADDRESS.test(value));
      lists.push({ start: i, end: close + 1, items: real.length, label: `${real.length} ${addresses ? 'addresses' : 'items'}`, bracketed: true });
      i = close;
    }
  }
  for (const match of text.slice(from).matchAll(ADDRESS_RUN)) {
    const start = from + match.index!, end = start + match[0].length;
    if (lists.some(list => start < list.end && end > list.start)) continue;
    const count = match[0].trim().split(/[,\s]+/).length;
    lists.push({ start, end, items: count, label: `${count} addresses`, bracketed: false });
  }
  return lists.sort((a, b) => a.start - b.start);
}

function findLinks(text: string, from: number): LinkToken[] {
  const links: LinkToken[] = [];
  const free = (start: number, end: number) => !links.some(link => start < link.end && end > link.start);
  const message = text.slice(from);
  for (const match of message.matchAll(URL_RE)) {
    const url = trimUrl(match[0]);
    if (url.length > 'https://'.length) links.push({ kind: 'url', start: from + match.index!, end: from + match.index! + url.length, url });
  }
  for (const match of message.matchAll(FILE_RE)) {
    const start = from + match.index!, end = start + match[0].length, path = match[1];
    if (!free(start, end)) continue;
    const name = path.split('/').pop() || path;
    if (!path.includes('/') && !looksLikeFileName(name)) continue;
    const reference = fileReference(`${path}:${match[2]}`, 'automatic');
    if (!reference) continue;
    links.push({ kind: 'file', start, end, path: reference.path, line: reference.line, column: match[3] ? Number(match[3]) : undefined });
  }
  for (const match of message.matchAll(LOCAL_PORT)) {
    const port = Number(match[2]), start = from + match.index!, end = start + match[0].length;
    if (port < 1 || port > 65535 || !free(start, end)) continue;
    links.push({ kind: 'port', start, end, port, url: `http://localhost:${port}/` });
  }
  for (const match of message.matchAll(PORT_WORD)) {
    const port = Number(match[1]), end = from + match.index! + match[0].length, start = end - match[1].length;
    if (port < 1 || port > 65535 || !free(start, end)) continue;
    links.push({ kind: 'port', start, end, port, url: `http://localhost:${port}/` });
  }
  return links.sort((a, b) => a.start - b.start);
}

function findLevel(text: string, at: number): { level: Level; span: Span & { label: string } } | undefined {
  const rest = text.slice(at);
  let match = LEVEL_WRAPPED.exec(rest);
  if (match) { const word = match[1] || match[2]; return { level: levelOf(word)!, span: { start: at, end: at + match[0].length, label: word.toUpperCase() } }; }
  match = LEVEL_BARE.exec(rest);
  if (match) {
    const word = match[1] || match[2];
    const level = levelOf(word);
    if (level) return { level, span: { start: at, end: at + match[0].length, label: match[0].replace(/:$/, '') } };
  }
  match = LEVEL_NPM.exec(rest);
  if (match) { const start = at + 4; return { level: levelOf(match[1])!, span: { start, end: start + match[1].length, label: match[1] } }; }
  match = LEVEL_GO_TEST.exec(rest);
  if (match) {
    const word = match[1] || match[2] || match[3], start = at + match[0].indexOf(word);
    return { level: 'error', span: { start, end: start + word.length, label: word } };
  }
  match = LEVEL_FIELD.exec(rest);
  if (match) {
    const value = match[3] || match[5], level = levelOf(value);
    if (level) { const start = at + match.index, end = start + match[0].length; return { level, span: { start, end, label: match[0] } }; }
  }
  return undefined;
}

function noiseClass(text: string, from: number, lists: ListSpan[]): string {
  let message = '';
  let at = from;
  for (const list of lists) { if (list.start < at) continue; message += text.slice(at, list.start) + '[…]'; at = list.end; }
  message += text.slice(at);
  message = message.trim();
  const tag = /^[([][\w.:/-]+[)\]]/.exec(message);
  if (tag) return tag[0];
  return message.replace(/"[^"]*"|'[^']*'/g, '""').replace(/\b0x[0-9a-f]+\b/gi, '#').replace(/\d+/g, '#').replace(/\s+/g, ' ').slice(0, 72);
}

/** What one line of output is made of. `text` has no newline; `segments` are its styled parts. */
export function analyzeLine(text: string, segments: Segment[], id: number, previous?: LogLine): LogLine {
  let at = 0;
  let time: LogLine['time'], source: LogLine['source'], marker: Span | undefined;
  const readTime = () => {
    const match = TIME_PLAIN.exec(text.slice(at));
    if (!match) return false;
    time = { start: at, end: at + match[0].length, label: match[1] };
    at += match[0].length; while (text[at] === ' ' || text[at] === '\t') at++;
    return true;
  };
  const readSource = (): boolean => {
    const rest = text.slice(at);
    let match = COMPOSE.exec(rest);
    if (match) { source = { start: at, end: at + match[1].length, name: match[1], sep: '|' }; at += match[0].length; return true; }
    match = ANNOUNCE.exec(rest);
    if (match) {
      marker = { start: at, end: at + 2 };
      const nameAt = at + match[1].length + match[2].length;
      source = { start: nameAt, end: nameAt + match[3].length, name: match[3], sep: ':' };
      at += match[0].length; return true;
    }
    match = BRACKET.exec(rest);
    if (match && !LEVEL_NAME.test(match[2])) { source = { start: at + 1, end: at + 1 + match[2].length, name: match[2], sep: ']', open: '[' }; at += match[0].length; return true; }
    match = SERVICE.exec(rest);
    if (match && !LEVEL_NAME.test(match[1])) { source = { start: at, end: at + match[1].length, name: match[1], sep: match[3] }; at += match[0].length; return true; }
    match = PNPM.exec(rest);
    if (match && !/\.\w+$/.test(match[1]) && !LEVEL_NAME.test(match[2])) { const name = `${match[1]} ${match[2]}`; source = { start: at, end: at + name.length, name, sep: match[3] }; at += match[0].length; return true; }
    match = TURBO.exec(rest);
    if (match && !LEVEL_NAME.test(match[1].split(':')[0])) { source = { start: at, end: at + match[1].length, name: match[1], sep: ':' }; at += match[0].length; return true; }
    return false;
  };
  if (!readSource()) { readTime(); readSource(); }
  if (source && !time) readTime();
  const message = at;
  const found = findLevel(text, message);
  const lists = findLists(text, message);
  const links = findLinks(text, message).filter(link => !lists.some(list => link.start < list.end && link.end > list.start && !(link.start > list.start && link.end < list.end)));
  const level = found?.level;
  const continuation = !time && !source && /^\s+\S/.test(text);
  const milestone = !!marker || (level !== 'error' && level !== 'warn' && MILESTONE.test(text.slice(message)));
  const key = time ? text.slice(0, time.start) + text.slice(time.end).trimStart() : text;
  return {
    id, text, segments, time, source, marker, message, level, levelSpan: found?.span, milestone, continuation,
    owner: source?.name ?? (continuation || (!time && !text.trim()) ? previous?.owner : undefined),
    links, lists, key,
    noiseKey: level === 'warn' ? `${source?.name ?? ''}\u0000${noiseClass(text, found!.span.start === message ? found!.span.end : message, lists)}` : '',
    progressKey: key.replace(/\d+(?:\.\d+)?/g, '#'),
  };
}

/** The visible text of segments. */
const textOf = (segments: Segment[]) => segments.map(segment => segment.text).join('');

/** One raw line (escape sequences and all) read on its own. */
export function readLine(raw: string, id = 0, previous?: LogLine, style: Style = {}): LogLine {
  const { line } = parseAnsiLine(raw, style);
  return analyzeLine(textOf(line), line, id, previous);
}


/** A short, stable hash (FNV-1a) of a text. */
export function hashText(text: string): number {
  let h = 2166136261;
  for (let i = 0; i < text.length; i++) { h ^= text.charCodeAt(i); h = Math.imul(h, 16777619); }
  return h >>> 0;
}

/**
 * Gives each source a colour: the one its name hashes to, so a service keeps
 * its colour from run to run, unless another source of this log already has
 * it, then the next free one.
 */
export class SourceColours {
  private byName = new Map<string, number>();
  private used = new Map<number, number>();
  colour(name: string): number {
    let colour = this.byName.get(name);
    if (colour !== undefined) return colour + 1;
    const preferred = hashText(name) % LOG_SOURCE_COLOURS;
    colour = preferred;
    for (let i = 0; i < LOG_SOURCE_COLOURS; i++) {
      const candidate = (preferred + i) % LOG_SOURCE_COLOURS;
      if (!this.used.get(candidate)) { colour = candidate; break; }
    }
    // All taken: share the least used.
    if (this.used.get(colour)) colour = [...Array(LOG_SOURCE_COLOURS).keys()].sort((a, b) => (this.used.get(a) || 0) - (this.used.get(b) || 0))[0];
    this.byName.set(name, colour); this.used.set(colour, (this.used.get(colour) || 0) + 1);
    return colour + 1;
  }
  get(name: string): number | undefined { const colour = this.byName.get(name); return colour === undefined ? undefined : colour + 1; }
}

/**
 * Output as it streams: each complete line is read once, the line still
 * arriving is re-read on its own, and the oldest lines go once the output
 * passes `limit` characters. `raw` is exactly what was received (what Copy uses).
 */
export class LogModel {
  lines: LogLine[] = [];
  /** The last line, while it has no newline yet. */
  partial?: LogLine;
  raw = '';
  /** Sources in the order they first appeared. */
  sources: string[] = [];
  colours = new SourceColours();
  hasTime = false;
  /** The longest source name, in characters (the source column's width). */
  sourceWidth = 0;
  private pending = '';
  private style: Style = {};
  private nextId = 0;
  private sizes: number[] = [];
  constructor(readonly limit = 524288) {}

  append(chunk: string): void {
    if (!chunk) return;
    this.raw += chunk;
    const pieces = (this.pending + chunk).split('\n');
    this.pending = pieces.pop() ?? '';
    for (const raw of pieces) {
      const parsed = parseAnsiLine(raw, this.style);
      this.style = parsed.style;
      const line = analyzeLine(textOf(parsed.line), parsed.line, this.nextId++, this.lines.at(-1));
      this.note(line);
      this.lines.push(line); this.sizes.push(raw.length + 1);
    }
    if (this.pending.length > this.limit) this.pending = this.pending.slice(-this.limit);
    this.partial = this.pending ? this.readPartial() : undefined;

    this.trim();
  }

  private readPartial(): LogLine {
    const parsed = parseAnsiLine(this.pending, this.style);
    return analyzeLine(textOf(parsed.line), parsed.line, this.nextId, this.lines.at(-1));
  }

  private note(line: LogLine) {
    if (line.time) this.hasTime = true;
    if (line.source) {
      if (!this.sources.includes(line.source.name)) { this.sources.push(line.source.name); this.colours.colour(line.source.name); }
      this.sourceWidth = Math.max(this.sourceWidth, Math.min(line.source.name.length, 28));
    }
  }

  /** Drops whole lines from the start until the output fits; a single line longer than the limit keeps its end. */
  private trim() {
    const excess = this.raw.length - this.limit;
    if (excess <= 0) return;
    let drop = 0, chars = 0;
    while (drop < this.lines.length && chars < excess) chars += this.sizes[drop++];
    if (drop) { this.lines.splice(0, drop); this.sizes.splice(0, drop); }
    this.raw = this.raw.slice(chars);
    if (this.raw.length > this.limit) this.raw = this.raw.slice(-this.limit);
  }

  /** Every line, the one still arriving last: a new array each time, so a view can tell it changed. */
  all(): LogLine[] { return this.partial ? [...this.lines, this.partial] : this.lines.slice(); }
}

export type LogRow = {
  line: LogLine;
  /** How many lines this row stands for (×N), 1 for a single line. */
  count: number;
  /** The first line folded into it, for "first at …" and expanding. */
  first: LogLine;
  /** Shown one by one: this is the last of an expanded fold. */
  expanded?: boolean;
};
export type RowOptions = {
  /** Text a line must contain (any case). */
  search?: string;
  /** Sources whose lines are hidden; '' is lines with no source. */
  hidden?: ReadonlySet<string>;
  /** Hide repeated warnings after the first of their kind, and fold progress lines. */
  hideNoise?: boolean;
  /** Folds shown line by line (the id of their first line). */
  expanded?: ReadonlySet<number>;
};
/** The rows; how many lines repeat a kind of warning already seen (hidden or not); lines per source. */
export type Rows = { rows: LogRow[]; noise: number; counts: Map<string, number> };

/**
 * What the console shows: lines filtered by search and source, repeated
 * warnings left out when noise is hidden, and runs of the same line folded
 * into one row with a count.
 */
export function logRows(lines: readonly LogLine[], options: RowOptions = {}): Rows {
  const needle = options.search?.trim().toLocaleLowerCase() || '';
  const counts = new Map<string, number>();
  const seen = new Set<string>();
  const groups: LogLine[][] = [];
  let noise = 0;
  for (const line of lines) {
    const owner = line.owner ?? '';
    counts.set(owner, (counts.get(owner) || 0) + 1);
    if (options.hidden?.has(owner)) continue;
    if (needle && !line.text.toLocaleLowerCase().includes(needle)) continue;
    if (line.noiseKey) {
      if (seen.has(line.noiseKey)) { noise++; if (options.hideNoise) continue; }
      seen.add(line.noiseKey);
    }
    const group = groups.at(-1), last = group?.at(-1);
    const same = !!last && !!line.text.trim() && (last.key === line.key || (!!options.hideNoise && last.progressKey === line.progressKey && last.progressKey.includes('#')));
    if (same) group!.push(line); else groups.push([line]);
  }
  const rows: LogRow[] = [];
  for (const group of groups) {
    const first = group[0], last = group.at(-1)!;
    if (group.length > 1 && options.expanded?.has(first.id)) {
      group.forEach((line, i) => rows.push({ line, count: 1, first: line, expanded: i === group.length - 1 ? true : undefined }));
      rows.at(-1)!.count = group.length; rows.at(-1)!.first = first;
    } else rows.push({ line: last, count: group.length, first });
  }
  return { rows, noise, counts };
}

/** The last `count` lines of output, raw, without trailing blank lines: what a short tail shows. */
export function tailLines(text: string, count: number): string[] {
  const lines = text.split('\n');
  // eslint-disable-next-line no-control-regex
  const blank = (line: string) => !line.replace(/\x1b\[[0-9;?]*[ -/]*[@-~]|\r/g, '').trim();
  while (lines.length && blank(lines.at(-1)!)) lines.pop();
  return lines.slice(-count);
}

/** The text of a line between two offsets, as styled segments. */
export function sliceSegments(segments: readonly Segment[], start: number, end: number): Segment[] {
  const out: Segment[] = [];
  let at = 0;
  for (const segment of segments) {
    const next = at + segment.text.length;
    if (next > start && at < end) out.push({ text: segment.text.slice(Math.max(0, start - at), Math.min(segment.text.length, end - at)), style: segment.style });
    at = next;
    if (at >= end) break;
  }
  return out;
}
