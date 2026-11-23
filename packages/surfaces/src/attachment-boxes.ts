/**
 * Attachment boxes: one model for everything a message carries, whatever its
 * kind and whatever agent receives it. A large paste becomes a text file in a
 * box rather than a wall of text in the message box; an image keeps its
 * thumbnail; a PDF and any other file are named boxes. The Runner decides how
 * each reaches the agent, by one rule for every provider (agent.Delivery):
 * the bubble says which.
 */
import { formatBytes, isLocalImage } from './attachment-images';

/** A paste longer than this many characters becomes a box. */
export const PASTE_BOX_CHARS = 800;
/** A paste of more lines than this becomes a box, however short. */
export const PASTE_BOX_LINES = 12;
/** Byte limit of a saved native view-state value; larger draft attachments live in IndexedDB. */
export const DRAFT_TEXT_BYTES = 256 * 1024;
/** The most text a box previews; the file itself is sent whole. */
export const PREVIEW_CHARS = 64 * 1024;
/** A sent text file at most this large is read back to preview it. */
export const SENT_TEXT_PREVIEW_BYTES = 1024 * 1024;

export type BoxKind = 'image' | 'text' | 'pdf' | 'file';

/** Lines of a text; a final newline does not start one more. */
export function lineCount(text: string): number {
  if (!text) return 0;
  let lines = 1;
  for (let i = 0; i < text.length; i++) if (text.charCodeAt(i) === 10 && i < text.length - 1) lines++;
  return lines;
}

/** Whether a text paste becomes a box: more than PASTE_BOX_CHARS characters or PASTE_BOX_LINES lines. */
export function shouldBoxPaste(text: string): boolean {
  return text.length > PASTE_BOX_CHARS || lineCount(text) > PASTE_BOX_LINES;
}

/** File extensions read as text (and their fence language), beside any text/* type. */
const TEXT_EXTENSIONS: Record<string, string> = {
  txt: '', log: 'log', md: 'markdown', csv: 'csv', tsv: '', go: 'go', ts: 'typescript', tsx: 'tsx', js: 'javascript', jsx: 'jsx', mjs: 'javascript',
  py: 'python', rs: 'rust', rb: 'ruby', java: 'java', kt: 'kotlin', swift: 'swift', c: 'c', h: 'c', cc: 'cpp', cpp: 'cpp', cs: 'csharp', php: 'php',
  sh: 'sh', bash: 'bash', zsh: 'zsh', json: 'json', yaml: 'yaml', yml: 'yaml', toml: 'toml', xml: 'xml', html: 'html', css: 'css', sql: 'sql',
  diff: 'diff', patch: 'diff', proto: 'protobuf', ini: 'ini', env: '',
};
const extension = (name: string) => { const dot = name.lastIndexOf('.'); return dot > 0 ? name.slice(dot + 1).toLowerCase() : ''; };

/** The language a file's name says, for its monospace preview; '' for plain text. */
export function languageOfName(name: string): string { return TEXT_EXTENSIONS[extension(name)] || ''; }

/** What kind of box a file is, from its type and name. */
export function boxKind(file: { name: string; type?: string; mediaType?: string }): BoxKind {
  const type = (file.type ?? file.mediaType ?? '').toLowerCase();
  if (isLocalImage({ type })) return 'image';
  if (type === 'application/pdf' || extension(file.name) === 'pdf') return 'pdf';
  if (type.startsWith('text/') || /(json|xml|yaml|toml|javascript|typescript)/.test(type) || extension(file.name) in TEXT_EXTENSIONS) return 'text';
  return 'file';
}

/** A box's icon. */
export function boxIcon(kind: BoxKind, name = ''): string {
  if (kind === 'image') return 'image';
  if (kind === 'pdf') return 'file-pdf';
  if (kind === 'text') return languageOfName(name) && !['log', 'markdown', 'csv'].includes(languageOfName(name)) ? 'file-code' : 'file-text';
  return 'file';
}

type Language = { language: string; extension: string };
/**
 * The language of pasted code, when it is clear; null for prose, logs and
 * anything uncertain (which stays plain text). Only strong signals count:
 * a misread language is worse than none.
 */
export function detectLanguage(text: string): Language | null {
  const trimmed = text.trim();
  if (!trimmed) return null;
  if (/^[[{]/.test(trimmed)) { try { JSON.parse(trimmed); return { language: 'json', extension: 'json' }; } catch { /* not JSON */ } }
  if (/^diff --git |^--- \S.*\n\+\+\+ \S/m.test(trimmed) && /^@@ /m.test(trimmed)) return { language: 'diff', extension: 'diff' };
  const first = trimmed.split('\n', 1)[0];
  if (/^#!.*\b(ba|z)?sh\b/.test(first)) return { language: 'sh', extension: 'sh' };
  if (/^#!.*\bpython/.test(first)) return { language: 'python', extension: 'py' };
  if (/^#!.*\bnode\b/.test(first)) return { language: 'javascript', extension: 'js' };
  const lines = trimmed.split('\n');
  const score = (patterns: RegExp[]) => lines.filter(line => patterns.some(pattern => pattern.test(line))).length;
  const candidates: [Language, number][] = [
    [{ language: 'go', extension: 'go' }, /^package \w+\s*$/m.test(trimmed) || /^func (\(\w+ \*?\w+\) )?\w+\(/m.test(trimmed) ? 10 : score([/^\s*if err != nil \{/, /:= /])],
    [{ language: 'rust', extension: 'rs' }, score([/^\s*(pub )?fn \w+.*[{)]\s*$/, /^\s*let mut /, /^use [\w:]+(::\{.*\})?;$/, /^\s*impl\b.*\{$/])],
    [{ language: 'python', extension: 'py' }, score([/^\s*def \w+\(.*\):\s*$/, /^\s*class \w+(\(.*\))?:\s*$/, /^(from [\w.]+ )?import [\w., ]+$/, /^\s*(elif|else|try|except.*|finally):\s*$/])],
    [{ language: 'typescript', extension: 'ts' }, score([/^\s*(export )?(interface|type) \w+.*[={]/, /:\s*(string|number|boolean)(\[\])?[;,)=]/, /^import .* from ['"].+['"];?$/])],
    [{ language: 'javascript', extension: 'js' }, score([/^\s*(const|let) \w+ = /, /=> \{?$/, /^\s*function \w+\(/, /^(import .* from|export (default|const|function)) /, /require\(['"]/])],
    [{ language: 'sql', extension: 'sql' }, score([/^\s*(SELECT|INSERT INTO|UPDATE|DELETE FROM|CREATE TABLE|ALTER TABLE|WITH)\b/i, /^\s*(FROM|WHERE|JOIN|GROUP BY|ORDER BY)\b/i])],
    [{ language: 'html', extension: 'html' }, /^<!doctype html|^<html[\s>]/i.test(trimmed) ? 10 : score([/^\s*<(div|span|p|ul|li|section|head|body|script|style)[\s>]/i])],
    [{ language: 'yaml', extension: 'yaml' }, /^[\w-]+:\s*$/m.test(trimmed) && score([/^\s*[\w-]+: \S/, /^\s*- [\w"']/]) >= Math.max(3, lines.length * 0.6) ? lines.length : 0],
  ];
  // TypeScript is JavaScript with types: its own signals decide between them.
  const ts = candidates[3], js = candidates[4];
  if (ts[1] > 0 && js[1] > 0) { ts[1] += js[1]; js[1] = 0; }
  const [best, points] = candidates.reduce((top, next) => next[1] > top[1] ? next : top);
  return points >= Math.min(3, Math.max(2, Math.ceil(lines.length / 10))) ? best : null;
}

const pad = (value: number) => String(value).padStart(2, '0');
const stamp = (now: Date) => `${now.getFullYear()}-${pad(now.getMonth() + 1)}-${pad(now.getDate())} at ${pad(now.getHours())}.${pad(now.getMinutes())}.${pad(now.getSeconds())}`;

/** Pasted text, by the box file that holds it: its text is known without reading the file. */
const pastedTexts = new WeakMap<File, string>();

/** A box file for pasted text: "Pasted text <when>.<ext>", plain text, its extension the language when detected. */
export function pastedTextFile(text: string, now = new Date(), lastModified = now.getTime()): File {
  const language = detectLanguage(text);
  const file = new File([text], `Pasted text ${stamp(now)}.${language?.extension || 'txt'}`, { type: 'text/plain', lastModified });
  pastedTexts.set(file, text);
  return file;
}

/** A box file with the same name and new text (an edit in the viewer). */
export function replaceText(file: File, text: string): File {
  const next = new File([text], file.name, { type: file.type || 'text/plain', lastModified: file.lastModified });
  pastedTexts.set(next, text);
  return next;
}

/** A restored pasted box (from a saved draft). */
export function restorePastedText(name: string, text: string, lastModified: number, type = 'text/plain'): File {
  const file = new File([text], name, { type, lastModified });
  pastedTexts.set(file, text);
  return file;
}

/** The text of a box made from a paste (or edited in the viewer); undefined for any other file. */
export function pastedTextOf(file: File): string | undefined { return pastedTexts.get(file); }

/** Whether a name is one a paste gave (a pasted box keeps it once sent). */
export const isPastedName = (name: string) => /^Pasted text \d{4}-\d\d-\d\d at /.test(name);

/** A text box's title and size: its first line, and how many lines. */
export function textSummary(text: string): { title: string; lines: number } {
  const first = text.split('\n').find(line => line.trim()) ?? '';
  const title = first.trim().replace(/\s+/g, ' ');
  return { title: title.length > 80 ? title.slice(0, 79) + '…' : title, lines: lineCount(text) };
}

const plural = (count: number, word: string) => `${count} ${word}${count === 1 ? '' : 's'}`;

/** The line under a box's title: "Pasted text · 142 lines", "PDF · 12 pages · 1.2 MiB", "notes.txt · 3 KiB". */
export function boxSubtitle(box: { name: string; size: number; kind: BoxKind; lines?: number; pages?: number }): string {
  if (box.kind === 'text' && isPastedName(box.name)) {
    const language = languageOfName(box.name);
    return [language ? `Pasted ${languageLabel(language)}` : 'Pasted text', box.lines !== undefined ? plural(box.lines, 'line') : formatBytes(box.size)].join(' · ');
  }
  if (box.kind === 'pdf') return ['PDF', box.pages ? plural(box.pages, 'page') : '', formatBytes(box.size)].filter(Boolean).join(' · ');
  if (box.kind === 'text' && box.lines !== undefined) return `${plural(box.lines, 'line')} · ${formatBytes(box.size)}`;
  return formatBytes(box.size);
}

const LANGUAGE_LABELS: Record<string, string> = { typescript: 'TypeScript', javascript: 'JavaScript', tsx: 'TSX', jsx: 'JSX', python: 'Python', go: 'Go', rust: 'Rust', json: 'JSON', yaml: 'YAML', sql: 'SQL', html: 'HTML', sh: 'shell', bash: 'shell', zsh: 'shell', diff: 'diff', markdown: 'Markdown' };
/** "Python", "TypeScript": how a box names its language. */
export const languageLabel = (language: string) => LANGUAGE_LABELS[language] || language;

/**
 * Pages of a PDF, read from its page tree without a PDF engine: the largest
 * `/Count` of a `/Type /Pages` node (the root's), else the `/Type /Page`
 * objects. A PDF whose objects are all compressed reports none (undefined).
 */
export function pdfPageCount(bytes: Uint8Array): number | undefined {
  if (bytes.length < 5 || String.fromCharCode(...bytes.subarray(0, 5)) !== '%PDF-') return undefined;
  // latin1 keeps one character per byte, so the dictionary syntax survives.
  let text = '';
  for (let i = 0; i < bytes.length; i += 0x8000) text += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  let most = 0;
  for (const match of text.matchAll(/\/Type\s*\/Pages\b[^>]*?\/Count\s+(\d+)|\/Count\s+(\d+)[^>]*?\/Type\s*\/Pages\b/g)) most = Math.max(most, Number(match[1] ?? match[2]));
  if (most > 0) return most;
  const pages = text.match(/\/Type\s*\/Page(?![a-zA-Z])/g)?.length ?? 0;
  return pages || undefined;
}

export type AttachmentLimits = { maxFiles: number; maxFileBytes: number; maxMessageBytes: number };
/** A file that was not attached, and why, shown as an error box. */
export type RejectedFile = { name: string; size: number; reason: string };

/** "25 MiB of 20": a size against its limit, in the limit's unit. */
function against(size: number, limit: number): string {
  const [unit, scale] = limit >= 1 << 20 ? ['MiB', 1 << 20] : limit >= 1024 ? ['KiB', 1024] : ['B', 1];
  const show = (bytes: number) => { const value = bytes / scale; return Number.isInteger(value) ? String(value) : value < 10 ? value.toFixed(1) : String(Math.round(value)); };
  return `${show(size)} ${unit} of ${show(limit)}`;
}

/**
 * Which new files fit beside those already attached (`held`, the files a
 * reloaded draft still names included), in order; the rest are error boxes
 * saying which limit they cross: "Too large: 25 MiB of 20".
 */
export function admitFiles(held: readonly { size: number }[], incoming: readonly File[], limits: AttachmentLimits): { accepted: File[]; rejected: RejectedFile[] } {
  const accepted: File[] = [], rejected: RejectedFile[] = [];
  let count = held.length, total = held.reduce((sum, file) => sum + file.size, 0);
  for (const file of incoming) {
    const reason = file.size > limits.maxFileBytes ? `Too large: ${against(file.size, limits.maxFileBytes)}`
      : count >= limits.maxFiles ? `Too many: ${limits.maxFiles} files per message`
      : total + file.size > limits.maxMessageBytes ? `Over the message limit: ${against(total + file.size, limits.maxMessageBytes)}`
      : '';
    if (reason) { rejected.push({ name: file.name, size: file.size, reason }); continue; }
    accepted.push(file); count++; total += file.size;
  }
  return { accepted, rejected };
}

/** What a sent box says about how the agent received it (File.Delivery); '' when the Runner did not say. */
export function deliveryNote(delivery: string | undefined): string {
  switch (delivery) {
    case 'image': return 'The agent saw it as an image';
    case 'text': return 'The agent got its text';
    case 'file': return 'The agent got a link to it, to open as needed';
  }
  return '';
}

/** `list` with the item at `from` moved to `to`. */
export function moveItem<T>(list: readonly T[], from: number, to: number): T[] {
  if (from === to || from < 0 || from >= list.length || to < 0 || to >= list.length) return [...list];
  const next = [...list];
  const [item] = next.splice(from, 1);
  next.splice(to, 0, item);
  return next;
}

/** `message` with `text` put in place of the selection [start, end), and where the caret goes after it. */
export function insertText(message: string, text: string, start = message.length, end = start): { message: string; caret: number } {
  const from = Math.max(0, Math.min(start, message.length)), to = Math.max(from, Math.min(end, message.length));
  return { message: message.slice(0, from) + text + message.slice(to), caret: from + text.length };
}
