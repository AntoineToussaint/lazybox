/**
 * Voice, the pure part (VOICE.md): preferences, spoken commands and
 * punctuation in English and French, how a dictation's stretches of speech
 * become text, how that text joins a draft, how a spoken palette query ranks
 * against the palette's items, and what a reply sounds like read aloud.
 *
 * Nothing here touches a microphone, a DOM or a clock.
 */

// ---- Preferences -----------------------------------------------------------

export type EngineChoice = 'auto' | 'system' | 'browser' | 'wispr';
/** How the dictation key behaves: held down, pressed to start and again to stop, or both (a tap toggles, a hold talks). */
export type KeyBehaviour = 'both' | 'hold' | 'toggle';
export type SpokenCommand = 'send' | 'newline' | 'cancel' | 'stop';
export type ReadReplies = 'off' | 'summary' | 'full';
export type VoicePreferences = {
  engine: EngineChoice;
  /** A BCP 47 tag, or '' for the system's language. */
  language: string;
  keyBehaviour: KeyBehaviour;
  /** The recognizer punctuates (macOS 13+). */
  autoPunctuation: boolean;
  /** "comma", "virgule", "question mark"… become punctuation. */
  spokenPunctuation: boolean;
  /** "send", "new line", "cancel", "stop" at the end of what you said do that. */
  spokenCommands: boolean;
  /** Saying the send word sends the message. Off: it only stops, and you press ↵. */
  autoSend: boolean;
  words: Record<SpokenCommand, string[]>;
  /** macOS: use Apple's server for a language this Mac cannot recognize itself. */
  allowServer: boolean;
  readReplies: ReadReplies;
  readNeedsYou: boolean;
  /** A voice id, or '' for the system's default. */
  voice: string;
  /** 0.5–2, 1 is normal. */
  rate: number;
};
export const defaultCommandWords: Record<SpokenCommand, string[]> = {
  send: ['send', 'send it', 'envoie', 'envoyer', 'envoie-le'],
  newline: ['new line', 'nouvelle ligne', 'à la ligne'],
  cancel: ['cancel', 'annule', 'annuler'],
  stop: ['stop', 'stop listening', 'arrête', 'arrête la dictée'],
};
export const defaultVoicePreferences: VoicePreferences = {
  engine: 'auto', language: '', keyBehaviour: 'both', autoPunctuation: true, spokenPunctuation: true,
  spokenCommands: true, autoSend: false, words: defaultCommandWords, allowServer: false,
  readReplies: 'off', readNeedsYou: false, voice: '', rate: 1,
};
export const commandIds: SpokenCommand[] = ['send', 'newline', 'cancel', 'stop'];

const oneOf = <T extends string>(value: unknown, allowed: readonly T[], fallback: T): T => allowed.includes(value as T) ? value as T : fallback;
const flag = (value: unknown, fallback: boolean) => typeof value === 'boolean' ? value : fallback;
/** The words of a command list, as typed in Settings ("send, envoie"): trimmed, distinct, at most 12. */
export function commandWordList(value: unknown): string[] | undefined {
  const raw = typeof value === 'string' ? value.split(',') : Array.isArray(value) ? value : undefined;
  if (!raw) return undefined;
  const words: string[] = [];
  for (const item of raw) {
    if (typeof item !== 'string') continue;
    const word = item.trim().replace(/\s+/g, ' ').slice(0, 40);
    if (word && !words.some(w => normalize(w) === normalize(word))) words.push(word);
  }
  return words.slice(0, 12);
}
/** Saved preferences, whatever their shape: anything unknown takes its default. */
export function parseVoicePreferences(raw: unknown): VoicePreferences {
  const value = raw && typeof raw === 'object' ? raw as Record<string, unknown> : {};
  const d = defaultVoicePreferences;
  const words = value.words && typeof value.words === 'object' ? value.words as Record<string, unknown> : {};
  const rate = typeof value.rate === 'number' && Number.isFinite(value.rate) ? Math.min(2, Math.max(0.5, value.rate)) : d.rate;
  const language = typeof value.language === 'string' && /^(?:[a-zA-Z]{2,3}(?:-[a-zA-Z0-9]{1,8}){0,3})?$/.test(value.language) ? value.language : '';
  return {
    engine: oneOf(value.engine, ['auto', 'system', 'browser', 'wispr'], d.engine),
    language,
    keyBehaviour: oneOf(value.keyBehaviour, ['both', 'hold', 'toggle'], d.keyBehaviour),
    autoPunctuation: flag(value.autoPunctuation, d.autoPunctuation),
    spokenPunctuation: flag(value.spokenPunctuation, d.spokenPunctuation),
    spokenCommands: flag(value.spokenCommands, d.spokenCommands),
    autoSend: flag(value.autoSend, d.autoSend),
    words: Object.fromEntries(commandIds.map(id => [id, commandWordList(words[id]) ?? d.words[id]])) as Record<SpokenCommand, string[]>,
    allowServer: flag(value.allowServer, d.allowServer),
    readReplies: oneOf(value.readReplies, ['off', 'summary', 'full'], d.readReplies),
    readNeedsYou: flag(value.readNeedsYou, d.readNeedsYou),
    voice: typeof value.voice === 'string' ? value.voice.slice(0, 256) : '',
    rate,
  };
}

// ---- Words -----------------------------------------------------------------

/** Lowercase, without accents, apostrophes made straight: what words are compared as. */
export function normalize(text: string): string {
  return text.normalize('NFD').replace(/\p{M}+/gu, '').replace(/[’‘`]/g, "'").toLowerCase();
}
type Token = { word: string; start: number; end: number };
/** The words of a text with where each one is; a hyphen or an apostrophe splits ("envoie-le", "l'agent"). */
export function tokens(text: string): Token[] {
  const out: Token[] = [];
  for (const match of text.matchAll(/[\p{L}\p{N}]+/gu)) out.push({ word: normalize(match[0]), start: match.index!, end: match.index! + match[0].length });
  return out;
}
const words = (text: string) => tokens(text).map(t => t.word);
const negations = new Set(['dont', 'not', 'never', 'pas', 'ne', 'n', 'jamais', 'no', 'non']);

/**
 * A spoken command at the end of an utterance: the text before it, and which
 * one. "Fix the login bug. Send." → "Fix the login bug." + send. A negation
 * just before ("don't send", "n'envoie pas" does not end with it) keeps the
 * words as text. The longest phrase wins ("send it" over "send").
 */
export function parseSpokenCommand(text: string, wordsFor: Record<SpokenCommand, string[]> = defaultCommandWords): { text: string; command?: SpokenCommand } {
  const said = tokens(text);
  let best: { command: SpokenCommand; length: number } | undefined;
  for (const command of commandIds) for (const phrase of wordsFor[command] || []) {
    const wanted = words(phrase);
    if (!wanted.length || wanted.length > said.length) continue;
    const tail = said.slice(said.length - wanted.length);
    if (!tail.every((t, i) => t.word === wanted[i])) continue;
    if (!best || wanted.length > best.length) best = { command, length: wanted.length };
  }
  if (!best) return { text };
  const first = said[said.length - best.length];
  const before = said[said.length - best.length - 1];
  if (before && negations.has(before.word)) return { text };
  // "don't" splits into "don" + "t".
  if (before?.word === 't' && said[said.length - best.length - 2]?.word === 'don') return { text };
  const rest = text.slice(0, first.start).replace(/[\s,;:—–-]+$/u, '').trimEnd();
  return { text: rest, command: best.command };
}

// ---- Punctuation -----------------------------------------------------------

type Mark = { phrase: string; mark: string };
const englishMarks: Mark[] = [
  { phrase: 'new paragraph', mark: '\n\n' }, { phrase: 'new line', mark: '\n' },
  { phrase: 'question mark', mark: '?' }, { phrase: 'exclamation mark', mark: '!' }, { phrase: 'exclamation point', mark: '!' },
  { phrase: 'full stop', mark: '.' }, { phrase: 'period', mark: '.' }, { phrase: 'comma', mark: ',' },
  { phrase: 'semicolon', mark: ';' }, { phrase: 'colon', mark: ':' },
  { phrase: 'open parenthesis', mark: '(' }, { phrase: 'close parenthesis', mark: ')' },
];
const frenchMarks: Mark[] = [
  { phrase: 'nouveau paragraphe', mark: '\n\n' }, { phrase: 'nouvelle ligne', mark: '\n' }, { phrase: 'a la ligne', mark: '\n' },
  { phrase: "point d'interrogation", mark: '?' }, { phrase: "point d'exclamation", mark: '!' },
  { phrase: 'point virgule', mark: ';' }, { phrase: 'deux points', mark: ':' },
  { phrase: 'point final', mark: '.' }, { phrase: 'virgule', mark: ',' }, { phrase: 'point', mark: '.' },
  { phrase: 'ouvrez la parenthese', mark: '(' }, { phrase: 'fermez la parenthese', mark: ')' },
];
/** The spoken marks for a language: English, French, or both where it is not known ("point" only in French). */
export function spokenMarks(language: string): Mark[] {
  const code = language.toLowerCase();
  if (code.startsWith('fr')) return frenchMarks;
  if (code.startsWith('en')) return englishMarks;
  return [...englishMarks, ...frenchMarks.filter(m => m.phrase !== 'point')];
}
const sentenceEnd = /[.!?…]["»)\]]?\s*$/u;
const upperFirst = (text: string) => text.replace(/^(\s*["«(]?\s*)(\p{Ll})/u, (_, lead, letter) => lead + letter.toUpperCase());

/**
 * Spoken punctuation as marks: "fix the bug comma then test it question mark"
 * → "fix the bug, then test it?". A mark joins the word before it (French
 * puts a space before ? ! : ;), and the next sentence starts with a capital.
 */
export function applySpokenPunctuation(text: string, language: string): string {
  const marks = spokenMarks(language);
  const french = language.toLowerCase().startsWith('fr');
  const said = tokens(text);
  if (!said.length) return text;
  let out = '', cursor = 0, i = 0;
  while (i < said.length) {
    const found = marks.find(m => { const w = words(m.phrase); return said.slice(i, i + w.length).length === w.length && w.every((word, k) => said[i + k].word === word); });
    if (!found) { i++; continue; }
    const length = words(found.phrase).length;
    const start = said[i].start, end = said[i + length - 1].end;
    out += text.slice(cursor, start).replace(/[\s,]+$/u, '');
    const mark = found.mark;
    if (mark.startsWith('\n')) out += mark;
    else if (mark === '(') out += (out && !/\s$/.test(out) ? ' ' : '') + '(';
    else out += (french && /[?!:;]/.test(mark) ? ' ' : '') + mark;
    cursor = end;
    // The words after a mark: no space after "(" or a new line, one space after the rest.
    const after = text.slice(cursor).replace(/^[\s,.]+/u, '');
    cursor = text.length - after.length;
    if (after && mark !== '(' && !mark.startsWith('\n')) out += ' ';
    if (/[.!?]$|\n$/.test(mark) && after) { out += upperFirst(after.slice(0, 1)); cursor += 1; }
    i += length;
  }
  return (out + text.slice(cursor)).replace(/[ \t]+\n/g, '\n');
}

// ---- A dictation: stretches of speech to text ------------------------------

export type DictationOptions = {
  language: string;
  spokenPunctuation: boolean;
  spokenCommands: boolean;
  words: Record<SpokenCommand, string[]>;
};
type Segment = {
  id: number;
  /** What the recognizer reports for this stretch so far. */
  raw: string;
  final: boolean;
  /** Text already settled from this stretch (a command was said part-way). */
  pieces: string[];
  /** The part of `raw` those pieces came from. */
  consumed: string;
};
export type Dictation = { segments: Segment[] };
export type DictationStep = { dictation: Dictation; command?: Exclude<SpokenCommand, 'newline'> };
/** What a dictation reads as now: all of it, and where the part still being recognized starts. */
export type DictationView = { text: string; liveStart: number };

export const emptyDictation = (): Dictation => ({ segments: [] });

const remainder = (segment: Segment) => segment.raw.startsWith(segment.consumed) ? segment.raw.slice(segment.consumed.length) : segment.raw.slice(Math.min(segment.consumed.length, segment.raw.length));
const render = (text: string, options: DictationOptions) => {
  const trimmed = text.trim();
  return options.spokenPunctuation ? applySpokenPunctuation(trimmed, options.language).trim() : trimmed;
};
/**
 * Settles a stretch's unsettled words. Its command, if any, applies only
 * with `commands` (the stretch is the end of what was said); otherwise the
 * words stay text.
 */
function settleSegment(segment: Segment, options: DictationOptions, onlyWithCommand: boolean, commands = true): { segment: Segment; command?: SpokenCommand } {
  const rest = remainder(segment);
  const parsed = options.spokenCommands && commands ? parseSpokenCommand(rest, options.words) : { text: rest };
  if (onlyWithCommand && !parsed.command) return { segment };
  const pieces = [...segment.pieces];
  const text = render(parsed.text, options);
  if (text) pieces.push(text);
  if (parsed.command === 'newline') pieces.push('\n');
  return { segment: { ...segment, pieces, consumed: segment.raw }, command: parsed.command };
}
/** Something was said after the stretch at `index`: a command in it is not at the end. */
const saidAfter = (segments: readonly Segment[], index: number) => segments.slice(index + 1).some(s => s.pieces.length > 0 || !!remainder(s).trim());

/**
 * One recognizer result. Each stretch of speech is a segment of its own,
 * kept in order: a later one never replaces an earlier one (macOS starts a
 * new one after a pause). A final stretch settles, and its trailing command
 * applies when nothing was said after it. "new line" adds a line and
 * dictation goes on; send, stop and cancel are returned for the caller to
 * act on.
 */
export function applyResult(dictation: Dictation, result: { segment: number; text: string; final: boolean }, options: DictationOptions): DictationStep {
  const index = dictation.segments.findIndex(s => s.id === result.segment);
  const previous = index >= 0 ? dictation.segments[index] : { id: result.segment, raw: '', final: false, pieces: [], consumed: '' };
  // A settled stretch does not change again (a late duplicate final).
  if (previous.final) return { dictation };
  const updated: Segment = { ...previous, raw: result.text, final: result.final };
  const segments = (index >= 0 ? dictation.segments.map((s, i) => i === index ? updated : s) : [...dictation.segments, updated]).sort((a, b) => a.id - b.id);
  if (!result.final) return { dictation: { segments } };
  const at = segments.indexOf(updated);
  const { segment, command } = settleSegment(updated, options, false, !saidAfter(segments, at));
  segments[at] = segment;
  return { dictation: { segments }, command: command === 'newline' ? undefined : command };
}

/**
 * A pause: the words still being recognized have not changed for a moment.
 * If the last words said end with a command, it applies now (an engine that
 * reports one growing stretch never sends a final until it stops).
 */
export function settleDictation(dictation: Dictation, options: DictationOptions): DictationStep {
  if (!options.spokenCommands) return { dictation };
  let index = -1;
  dictation.segments.forEach((s, i) => { if (s.pieces.length || remainder(s).trim()) index = i; });
  if (index < 0 || dictation.segments[index].final || !remainder(dictation.segments[index]).trim()) return { dictation };
  const { segment, command } = settleSegment(dictation.segments[index], options, true);
  if (!command) return { dictation };
  return { dictation: { segments: dictation.segments.map((s, i) => i === index ? segment : s) }, command: command === 'newline' ? undefined : command };
}

/** Everything still unsettled counts as said (the person stopped and the recognizer had no final in time). */
export function settleAll(dictation: Dictation, options: DictationOptions): DictationStep {
  let current = dictation, command: DictationStep['command'];
  for (const segment of dictation.segments) {
    if (segment.final) continue;
    const step = applyResult(current, { segment: segment.id, text: segment.raw, final: true }, options);
    current = step.dictation; command ||= step.command;
  }
  return { dictation: current, command };
}

/** Joins two pieces of dictated text: a space between words, none before a mark, a capital after a sentence or a line. */
export function joinSpoken(before: string, piece: string): string {
  if (!piece) return before;
  if (piece.startsWith('\n')) return before.replace(/[ \t]+$/, '') + piece;
  if (!before) return upperFirst(piece);
  if (before.endsWith('\n')) return before + upperFirst(piece);
  if (/^[,.;:!?)…]/.test(piece)) return before + piece;
  return before + ' ' + (sentenceEnd.test(before) ? upperFirst(piece) : piece);
}

export function dictationView(dictation: Dictation, options: DictationOptions): DictationView {
  let committed = '';
  const live: string[] = [];
  for (const segment of dictation.segments) {
    for (const piece of segment.pieces) committed = joinSpoken(committed, piece);
    if (!segment.final) { const text = render(remainder(segment), options); if (text) live.push(text); }
  }
  let text = committed;
  for (const piece of live) text = joinSpoken(text, piece);
  const liveStart = !live.length ? text.length : committed && text[committed.length] === ' ' ? committed.length + 1 : committed.length;
  return { text, liveStart };
}

// ---- Joining a draft ---------------------------------------------------------

/**
 * Dictated text put into a draft between `before` and `after` (the selection
 * at the start): spaces where words meet, a capital at the start of a
 * sentence, a lowercase first word inside one (unless it is "I" or an
 * acronym). `start`/`end` bound the dictated text in the result.
 */
export function mergeDictation(before: string, after: string, dictated: string): { text: string; start: number; end: number } {
  if (!dictated) return { text: before + after, start: before.length, end: before.length };
  let piece = dictated;
  const startsSentence = !before.trim() || /[.!?…]["»)\]]?\s*$/u.test(before) || /\n\s*$/.test(before);
  if (startsSentence) piece = upperFirst(piece);
  else {
    const first = piece.match(/^\p{L}[\p{L}'’]*/u)?.[0] || '';
    const keep = /^I(?:'|’|$)/.test(first) || (first.length > 1 && first === first.toUpperCase());
    if (!keep) piece = piece.replace(/^\p{Lu}/u, letter => letter.toLowerCase());
  }
  const lead = before && !/\s$/.test(before) && !/^[,.;:!?)…\n]/.test(piece) ? ' ' : '';
  const trail = after && !/^\s/.test(after) && !/^[,.;:!?)…]/.test(after) && !piece.endsWith('\n') ? ' ' : '';
  const start = before.length + lead.length;
  return { text: before + lead + piece + trail + after, start, end: start + piece.length };
}

// ---- Voice in the command palette --------------------------------------------

/** Words people say for what the palette names otherwise, in English and French. */
const synonyms: Record<string, string> = {
  me: 'you', moi: 'you', agent: 'conversation', agents: 'conversation', chat: 'conversation', chats: 'conversation', session: 'conversation', discussion: 'conversation',
  suivant: 'next', suivante: 'next', prochain: 'next', prochaine: 'next', precedent: 'previous', precedente: 'previous', last: 'previous',
  reglages: 'settings', parametres: 'settings', preferences: 'settings', options: 'settings',
  modifications: 'changes', changements: 'changes', diff: 'changes', diffs: 'changes',
  fichier: 'file', fichiers: 'files', espace: 'workspace', projet: 'workspace', project: 'workspace',
  raccourcis: 'shortcuts', clavier: 'keyboard', nouveau: 'new', nouvelle: 'new', boite: 'inbox', reception: 'inbox',
  attend: 'needs', attendent: 'needs', waiting: 'needs', need: 'needs', besoin: 'needs', terminal: 'terminal', theme: 'appearance', sombre: 'dark', clair: 'light',
  artefacts: 'artifacts', artefact: 'artifacts', missions: 'missions', web: 'web', page: 'page', navigateur: 'browser',
};
/** Words that carry no meaning for the match: articles, "please", and the verbs of going somewhere. */
const fillers = new Set(['the', 'a', 'an', 'to', 'please', 'go', 'switch', 'open', 'show', 'jump', 'take', 'me', 'my', 'that', 'which', 'who', 'for', 'of', 'in', 'on', 'and',
  'le', 'la', 'les', 'l', 'un', 'une', 'des', 'du', 'de', 'd', 'a', 'au', 'aux', 'vers', 'ouvre', 'ouvrir', 'montre', 'affiche', 'va', 'aller', 'passe', 'qui', 'que', 'stp', 'merci', 'mon', 'ma', 'mes', 'sur', 'dans', 'et']);
const meaningful = (text: string, keepFillers = false) => words(text).filter(word => keepFillers || !fillers.has(word)).map(word => synonyms[word] || word);
const similar = (a: string, b: string) => a === b || (a.length >= 4 && b.length >= 4 && (a.startsWith(b) || b.startsWith(a))) || (a.length >= 5 && b.length >= 5 && editDistance(a, b) <= 1);
function editDistance(a: string, b: string): number {
  const row = Array.from({ length: b.length + 1 }, (_, i) => i);
  for (let i = 1; i <= a.length; i++) {
    let diagonal = row[0]; row[0] = i;
    for (let j = 1; j <= b.length; j++) { const next = row[j]; row[j] = Math.min(row[j] + 1, row[j - 1] + 1, diagonal + (a[i - 1] === b[j - 1] ? 0 : 1)); diagonal = next; }
  }
  return row[b.length];
}

/**
 * How well a spoken query names a palette item, 0–100: the share of the
 * query's meaningful words found in the item (its title weighs more than its
 * keywords), plus a little for how much of the title was said. Typed
 * matching is a subsequence of letters; speech is words, often other words
 * ("next agent that needs me" for "Next conversation that needs you").
 */
export function spokenScore(utterance: string, title: string, extra = ''): number {
  const said = meaningful(utterance);
  if (!said.length) return 0;
  const titleWords = meaningful(title, true), extraWords = meaningful(extra, true);
  let found = 0, inTitle = 0;
  for (const word of said) {
    if (titleWords.some(w => similar(word, w))) { found++; inTitle++; }
    else if (extraWords.some(w => similar(word, w))) found += 0.6;
  }
  if (!found) return 0;
  const titleCore = meaningful(title);
  const coverage = titleCore.length ? Math.min(1, inTitle / titleCore.length) : 0;
  return Math.round((found / said.length) * 80 + coverage * 20);
}

/** A palette item as voice sees it. `confirm`: never run it without the person saying so. */
export type SpokenCandidate = { key: string; title: string; extra?: string; score: number; confirm?: boolean };
/** What a spoken palette query does: run the best match, ask first, or show the matches. */
export type SpokenDecision = { kind: 'run'; key: string } | { kind: 'confirm'; key: string } | { kind: 'choose' } | { kind: 'none' };
/**
 * Runs only a clear winner: a strong match, ahead of the next one. A match
 * that needs confirmation (something that cannot be undone, or the person's
 * own action) is asked about instead; anything less clear stays in the
 * palette for the person to pick.
 */
export function spokenDecision(ranked: readonly SpokenCandidate[]): SpokenDecision {
  if (!ranked.length || ranked[0].score <= 0) return { kind: 'none' };
  const [first, second] = ranked;
  const clear = first.score >= 70 && (!second || first.score - second.score >= 12);
  if (!clear) return { kind: 'choose' };
  return first.confirm ? { kind: 'confirm', key: first.key } : { kind: 'run', key: first.key };
}
/** "yes" / "oui" or "no" / "non" to a question the palette asked. */
export function spokenAnswer(text: string): 'yes' | 'no' | undefined {
  const said = words(text);
  if (said.some(w => ['yes', 'yeah', 'yep', 'oui', 'ouais', 'confirm', 'confirme', 'run', 'vas', 'ok', 'okay', 'go', 'daccord'].includes(w))) return said.some(w => ['no', 'non', 'not', 'pas'].includes(w)) ? 'no' : 'yes';
  if (said.some(w => ['no', 'non', 'cancel', 'annule', 'stop', 'nope'].includes(w))) return 'no';
  return undefined;
}

// ---- Read aloud ----------------------------------------------------------------

const READ_LIMIT = 4000;
/**
 * A reply as it should sound: Markdown marks gone, code blocks named instead
 * of read, links as their text, a URL as "a link". `summary` is the first
 * paragraph (with the heading before it, if the reply starts with one).
 */
export function speakableText(markdown: string, mode: 'summary' | 'full'): string {
  const blocks: string[] = [];
  const text = markdown.replace(/\r\n?/g, '\n').replace(/```[^\n]*\n[\s\S]*?(?:```|$)/g, '\n\n(code block)\n\n');
  for (const raw of text.split(/\n\s*\n/)) {
    const lines = raw.split('\n').filter(line => !/^\s*\|/.test(line) && !/^\s*([-*_])\s*\1\s*\1[\s\-*_]*$/.test(line));
    const cleaned = lines.map(line => line
      .replace(/^\s{0,3}#{1,6}\s+/, '')
      .replace(/^\s*(?:[-*+]|\d+[.)])\s+/, '')
      .replace(/^\s*>\s?/, '')
      .replace(/!\[([^\]]*)\]\([^)]*\)/g, '$1')
      .replace(/\[([^\]]+)\]\([^)]*\)/g, '$1')
      .replace(/<?https?:\/\/[^\s>)]+>?/g, 'a link')
      .replace(/`([^`]+)`/g, '$1')
      .replace(/(\*\*|__)(.+?)\1/g, '$2')
      .replace(/(^|[^\w*])[*_]([^*_\n]+)[*_](?=[^\w*]|$)/g, '$1$2')
      .replace(/~~(.+?)~~/g, '$1')
      .trim()).filter(Boolean);
    if (!cleaned.length) continue;
    // A list reads as sentences.
    blocks.push(cleaned.map(line => /[.!?:;,]$/.test(line) ? line : line + (cleaned.length > 1 ? '.' : '')).join(' '));
  }
  if (!blocks.length) return '';
  let chosen = blocks;
  if (mode === 'summary') {
    const heading = /^#{1,6}\s/.test(markdown.trimStart());
    chosen = heading && blocks.length > 1 ? blocks.slice(0, 2) : blocks.slice(0, 1);
  }
  const joined = chosen.map(b => /[.!?:]$/.test(b) ? b : b + '.').join(' ').replace(/\s+/g, ' ').trim();
  return joined.length > READ_LIMIT ? joined.slice(0, READ_LIMIT).replace(/\s+\S*$/, '') + '…' : joined;
}

/** Elapsed time as the pill shows it: 0:07, 1:32. */
export function elapsed(ms: number): string {
  const seconds = Math.max(0, Math.floor(ms / 1000));
  return `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, '0')}`;
}
