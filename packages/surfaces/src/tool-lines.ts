/**
 * Tool calls as lines of the conversation: what each one did, in words
 * ("Read src/app.ts", "Ran pnpm test · exit 0 · 4.2s", "Edited 3 files +12 −4"),
 * folded from the Runner journal's normalized tool events. Pure, so the
 * transcript fold and any view (the chat, sub-agent cards) share it.
 *
 * What is kept per call is bounded: a long conversation holds hundreds of
 * calls, and the journal (and the Activity dialog) keep the full evidence.
 */

import { browserVerb, describeBrowserCall } from './browser-model';
import { plainLazybox } from './lazybox-cli';

/** `stopped` and `denied`: not run because the person (or Lazybox) stopped it or declined it; see tool-stops.ts. */
export type ToolStatus = 'running' | 'completed' | 'failed' | 'unknown' | 'stopped' | 'denied';
export type DiffStat = { added: number; removed: number; files: string[] };
export type ToolEntry = {
  id: string;
  name: string;
  status: ToolStatus;
  /** The call's input, strings bounded. */
  input?: Record<string, unknown> | string;
  /** Output as text: its head and tail when long. */
  output?: string;
  /** Lines in the output the provider reported (before bounding). */
  outputLines?: number;
  outputImages?: number;
  exitCode?: number;
  /** Reported by the provider (Codex); otherwise taken from journal times. */
  durationMs?: number;
  cwd?: string;
  /** Codex's parse of a shell command: a read, a search, a listing. */
  actions?: { type: string; name?: string; path?: string | null; query?: string | null; command?: string }[];
  startedAt?: number;
  endedAt?: number;
  diff?: DiffStat;
  /** A unified diff of the change, when the input says what changed. */
  patch?: string;
};

const object = (value: unknown): Record<string, any> => value && typeof value === 'object' && !Array.isArray(value) ? value as Record<string, any> : {};
const STRING_LIMIT = 4000, OUTPUT_HEAD = 6000, OUTPUT_TAIL = 10000, PATCH_LIMIT = 12000;

function boundStrings(value: unknown, depth = 0): unknown {
  if (typeof value === 'string') return value.length > STRING_LIMIT ? value.slice(0, STRING_LIMIT) + '\n[…]' : value;
  if (depth > 4) return undefined;
  if (Array.isArray(value)) return value.slice(0, 40).map(v => boundStrings(v, depth + 1));
  if (value && typeof value === 'object') return Object.fromEntries(Object.entries(value).slice(0, 40).map(([k, v]) => [k, boundStrings(v, depth + 1)]));
  return value;
}

/** Tool output as text: Claude's content blocks, Codex's strings, anything else as JSON. */
export function outputText(value: unknown): { text: string; images: number } {
  if (value === undefined || value === null) return { text: '', images: 0 };
  if (typeof value === 'string') return { text: value, images: 0 };
  if (Array.isArray(value)) {
    let images = 0; const parts: string[] = [];
    for (const block of value) {
      const b = object(block);
      if (b.type === 'text' && typeof b.text === 'string') parts.push(b.text);
      else if (b.type === 'image') images++;
      else if (b.type === 'inputText' && typeof b.text === 'string') parts.push(b.text);
      else parts.push(JSON.stringify(block));
    }
    return { text: parts.join('\n'), images };
  }
  const o = object(value);
  if (Array.isArray(o.content)) return outputText(o.content);
  return { text: JSON.stringify(value, null, 2), images: 0 };
}

const countLines = (text: string) => text ? text.split('\n').length - (text.endsWith('\n') ? 1 : 0) : 0;
function boundOutput(text: string): string {
  if (text.length <= OUTPUT_HEAD + OUTPUT_TAIL) return text;
  const head = text.slice(0, OUTPUT_HEAD), tail = text.slice(-OUTPUT_TAIL);
  const omitted = countLines(text.slice(OUTPUT_HEAD, -OUTPUT_TAIL));
  return `${head}\n… ${omitted.toLocaleString()} lines not kept here …\n${tail}`;
}

/** Lines removed and added between two texts, outside their common start and end. */
export function lineChange(before: string, after: string): { removed: string[]; added: string[]; context: string[] } {
  const a = before ? before.split('\n') : [], b = after ? after.split('\n') : [];
  let start = 0; while (start < a.length && start < b.length && a[start] === b[start]) start++;
  let end = 0; while (end < a.length - start && end < b.length - start && a[a.length - 1 - end] === b[b.length - 1 - end]) end++;
  return { removed: a.slice(start, a.length - end), added: b.slice(start, b.length - end), context: a.slice(Math.max(0, start - 2), start) };
}

function editPatch(path: string, edits: { old: string; next: string }[]): { patch: string; added: number; removed: number } {
  let patch = `--- ${path}\n+++ ${path}\n`, added = 0, removed = 0;
  for (const { old, next } of edits) {
    const change = lineChange(old, next);
    added += change.added.length; removed += change.removed.length;
    patch += '@@\n' + [...change.context.map(l => ' ' + l), ...change.removed.map(l => '-' + l), ...change.added.map(l => '+' + l)].join('\n') + '\n';
  }
  return { patch: patch.length > PATCH_LIMIT ? patch.slice(0, PATCH_LIMIT) + '\n[…]' : patch, added, removed };
}

/** Counts + and − lines of a unified diff, skipping its file headers. */
export function patchStat(patch: string): { added: number; removed: number } {
  let added = 0, removed = 0;
  for (const line of patch.split('\n')) {
    if (line.startsWith('+++') || line.startsWith('---')) continue;
    if (line.startsWith('+')) added++; else if (line.startsWith('-')) removed++;
  }
  return { added, removed };
}

/** What a file change does, from the call's input, before its strings are bounded. */
export function changeOf(name: string, input: unknown): { diff: DiffStat; patch?: string } | undefined {
  const i = object(input);
  const path = typeof i.file_path === 'string' ? i.file_path : typeof i.path === 'string' ? i.path : typeof i.notebook_path === 'string' ? i.notebook_path : '';
  if (name === 'Edit' && path && typeof i.old_string === 'string' && typeof i.new_string === 'string') {
    const p = editPatch(path, [{ old: i.old_string, next: i.new_string }]);
    return { diff: { added: p.added, removed: p.removed, files: [path] }, patch: p.patch };
  }
  if (name === 'MultiEdit' && path && Array.isArray(i.edits)) {
    const p = editPatch(path, i.edits.map((e: any) => ({ old: String(e?.old_string ?? ''), next: String(e?.new_string ?? '') })));
    return { diff: { added: p.added, removed: p.removed, files: [path] }, patch: p.patch };
  }
  if (name === 'Write' && path && typeof i.content === 'string') {
    const p = editPatch(path, [{ old: '', next: i.content }]);
    return { diff: { added: p.added, removed: 0, files: [path] }, patch: p.patch };
  }
  // Codex: a list of { path, kind, diff }.
  const changes = Array.isArray(input) ? input : Array.isArray(i.changes) ? i.changes : undefined;
  if (changes && changes.every(c => typeof object(c).path === 'string')) {
    let added = 0, removed = 0, patch = '';
    for (const c of changes) {
      const diff = typeof c.diff === 'string' ? c.diff : '';
      const kind = object(c.kind).type || c.kind;
      const stat = patchStat(diff);
      // An added file's diff is its content.
      if (kind === 'add' && !/^[+-]/m.test(diff)) stat.added = countLines(diff);
      added += stat.added; removed += stat.removed;
      patch += `--- ${c.path}\n+++ ${c.path}\n${diff}\n`;
    }
    return { diff: { added, removed, files: changes.map(c => c.path) }, patch: patch.length > PATCH_LIMIT ? patch.slice(0, PATCH_LIMIT) + '\n[…]' : patch };
  }
  return undefined;
}

const STATUS: Record<string, ToolStatus> = { running: 'running', completed: 'completed', failed: 'failed' };

/**
 * One journal tool event applied to the call it is about (or a new one).
 * Returns undefined when the payload is not a tool call.
 */
export function applyToolPayload(previous: ToolEntry | undefined, payload: Record<string, any>, at: number | undefined): ToolEntry | undefined {
  let id: string | undefined, name: string | undefined, status: ToolStatus | undefined, input: unknown, output: unknown, error = false;
  if (payload.type === 'tool_call') {
    id = payload.id; name = payload.name; status = STATUS[payload.status] || 'unknown'; input = payload.input; output = payload.output;
  } else if (payload.type === 'tool_use') { // Journals written before normalized tool calls.
    id = payload.id; name = payload.name; status = 'running'; input = payload.input;
  } else if (payload.type === 'tool_result') {
    id = payload.tool_use_id; status = payload.is_error ? 'failed' : 'completed'; output = payload.content; error = !!payload.is_error;
  }
  if (typeof id !== 'string' || !id) return;
  const next: ToolEntry = previous ? { ...previous } : { id, name: name || 'Tool', status: status || 'running' };
  if (name) next.name = name;
  // A result never goes back to running.
  if (status && !(previous && ['completed', 'failed', 'stopped', 'denied'].includes(previous.status) && status === 'running')) next.status = status;
  if (input !== undefined && input !== null) {
    const change = changeOf(next.name, input);
    if (change) { next.diff = change.diff; next.patch = change.patch; }
    next.input = boundStrings(typeof input === 'string' ? input : object(input)) as ToolEntry['input'];
    if (Array.isArray(input)) next.input = { changes: boundStrings(input) } as Record<string, unknown>;
  }
  if (output !== undefined && output !== null) {
    const { text, images } = outputText(output);
    next.output = boundOutput(text); next.outputLines = countLines(text); next.outputImages = images || undefined;
    // Claude reports a failed command as "Exit code N" on the first line.
    const exit = /^Exit code (\d+)/.exec(text);
    if (exit) next.exitCode = Number(exit[1]);
  }
  if (typeof payload.exitCode === 'number') next.exitCode = payload.exitCode;
  if (typeof payload.durationMs === 'number') next.durationMs = payload.durationMs;
  if (typeof payload.cwd === 'string') next.cwd = payload.cwd;
  if (Array.isArray(payload.actions)) next.actions = payload.actions.slice(0, 20);
  if (error && next.status !== 'failed') next.status = 'failed';
  if (next.startedAt === undefined && at !== undefined) next.startedAt = at;
  if (next.endedAt === undefined && at !== undefined && (next.status === 'completed' || next.status === 'failed')) next.endedAt = at;
  // A shell command that ended without an error exited 0 (Claude says so only on failure).
  if (next.status === 'completed' && next.exitCode === undefined && kindOf(next) === 'command' && next.output !== undefined) next.exitCode = 0;
  return next;
}

/**
 * Calls the conversation shows in another form: an answered question's line,
 * a sub-agent's card (SubagentCard, from the spawning call).
 */
const HIDDEN = new Set(['AskUserQuestion', 'Task', 'Agent', 'Sub-agent']);
export const hiddenTool = (name: string) => HIDDEN.has(name);

export type ToolKind = 'read' | 'search' | 'list' | 'edit' | 'command' | 'web' | 'browser' | 'agent' | 'plan' | 'thinking' | 'mcp' | 'other';
/** The name of the line that stands for the agent thinking (its reasoning stays private). */
export const THINKING = 'Thinking';
const KINDS: Record<string, ToolKind> = {
  Read: 'read', NotebookRead: 'read', 'View image': 'read', Grep: 'search', Glob: 'list', LS: 'list',
  Edit: 'edit', MultiEdit: 'edit', Write: 'edit', NotebookEdit: 'edit', 'File changes': 'edit', apply_patch: 'edit',
  Bash: 'command', 'Shell command': 'command', exec_command: 'command', local_shell: 'command', mcp__lazybox__command: 'command', lazybox_command: 'command',
  WebFetch: 'web', WebSearch: 'web', 'Web search': 'web', Task: 'agent', Agent: 'agent', 'Sub-agent': 'agent', TodoWrite: 'plan', update_plan: 'plan', [THINKING]: 'thinking',
};
export function kindOf(entry: Pick<ToolEntry, 'name' | 'actions' | 'input'>): ToolKind {
  // The agent browser's calls (BROWSER.md), from Claude's MCP server or Codex's tools.
  if (browserVerb(entry.name)) return 'browser';
  const kind = KINDS[entry.name];
  if (kind === 'command') {
    // Codex parses what a command does: all reads is a read, all searches a search.
    const types = new Set((entry.actions || []).map(a => a.type));
    if (types.size === 1 && types.has('read')) return 'read';
    if (types.size === 1 && types.has('search')) return 'search';
    if (types.size === 1 && types.has('listFiles')) return 'list';
    // The managed command tool reads output too; only a start runs something.
    const input = object(entry.input);
    if ((entry.name === 'mcp__lazybox__command' || entry.name === 'lazybox_command') && input.action && input.action !== 'start' && input.action !== 'start_action') return 'other';
  }
  if (kind) return kind;
  return entry.name.startsWith('mcp__') ? 'mcp' : 'other';
}

/** A path as short as it stays unambiguous: relative to the workspace when inside it. */
export function shortPath(path: string, root = ''): string {
  if (root && path.startsWith(root.endsWith('/') ? root : root + '/')) return path.slice(root.length).replace(/^\/+/, '');
  if (root && path === root) return '.';
  return path.replace(/^\/(?:Users|home)\/[^/]+\//, '~/');
}

export function durationOf(entry: ToolEntry): number | undefined {
  if (entry.durationMs !== undefined) return entry.durationMs;
  if (entry.startedAt === undefined || entry.endedAt === undefined || entry.endedAt < entry.startedAt) return;
  return entry.endedAt - entry.startedAt;
}
export function shortDuration(ms: number | undefined): string {
  if (ms === undefined || !Number.isFinite(ms) || ms < 0) return '';
  if (ms < 1000) return `${Math.max(0, Math.round(ms / 100) / 10) || '0.1'}s`.replace(/^0s$/, '<0.1s');
  if (ms < 60000) return `${(ms / 1000).toFixed(ms < 10000 ? 1 : 0).replace(/\.0$/, '')}s`;
  const m = Math.floor(ms / 60000); return `${m}m ${Math.round((ms % 60000) / 1000)}s`;
}

/** The command a call runs, as one line. */
export function commandText(entry: Pick<ToolEntry, 'name' | 'input'>): string {
  const input = entry.input;
  if (typeof input === 'string') return input;
  const i = object(input);
  if (typeof i.command === 'string') return i.command;
  if (Array.isArray(i.command)) return i.command.join(' ');
  if (Array.isArray(i.argv)) return i.argv.join(' ');
  if (typeof i.cmd === 'string') return i.cmd;
  return '';
}

/** An artifact an agent published with `lazybox artifact put`, as the CLI reported it. */
export type PublishedArtifact = { id: string; title: string; version: number; kind: string };
// `lazybox artifact put` prints this line for each version it adds (cmd/lazybox/artifact.go); an unchanged put prints another.
const PUBLISHED = /Published “([^”\n]*)” as artifact ([A-Za-z0-9_-]+), version (\d+) \(([a-z]*)\)/g;
const ARTIFACT_PUT = /\blazybox\s+artifact\s+put\b/;
/**
 * The artifacts a call published: a shell command that ran `lazybox artifact
 * put` (however the CLI was called) and whose output says what it published.
 * A chained command that failed after publishing still published.
 */
export function publishedArtifacts(entry: Pick<ToolEntry, 'name' | 'input' | 'output'>): PublishedArtifact[] {
  if (!entry.output || KINDS[entry.name] !== 'command') return [];
  if (!ARTIFACT_PUT.test(plainLazybox(commandText(entry)))) return [];
  const found: PublishedArtifact[] = [];
  for (const [, title, id, version, kind] of entry.output.matchAll(PUBLISHED)) {
    if (!found.some(p => p.id === id && p.version === Number(version))) found.push({ id, title, version: Number(version), kind });
  }
  return found;
}

/** An artifact an agent retired (`lazybox artifact retire`, or `put --supersedes`), as the CLI reported it. */
export type RetiredArtifact = { id: string; title: string; supersededBy?: { id: string; title: string } };
// `lazybox artifact retire` and `put --supersedes` print this line for each artifact they retire (cmd/lazybox/artifact.go, retiredLine).
const RETIRED = /Retired “([^”\n]*)” \(artifact ([A-Za-z0-9_-]+)\)(?:, superseded by “([^”\n]*)” \(artifact ([A-Za-z0-9_-]+)\))?\./g;
const ARTIFACT_RETIRE = /\blazybox\s+artifact\s+(retire|put)\b/;
/**
 * The artifacts a call retired: a shell command that ran `lazybox artifact
 * retire` or `put` (with --supersedes) and whose output says what it retired.
 */
export function retiredArtifacts(entry: Pick<ToolEntry, 'name' | 'input' | 'output'>): RetiredArtifact[] {
  if (!entry.output || KINDS[entry.name] !== 'command') return [];
  if (!ARTIFACT_RETIRE.test(plainLazybox(commandText(entry)))) return [];
  const found: RetiredArtifact[] = [];
  for (const [, title, id, byTitle, byId] of entry.output.matchAll(RETIRED)) {
    if (!found.some(r => r.id === id)) found.push({ id, title, ...(byId ? { supersededBy: { id: byId, title: byTitle } } : {}) });
  }
  return found;
}

/**
 * What a call did to artifacts, as the conversation shows it: what it
 * published, the merges (a put that superseded others: "Merged 3 artifacts
 * into “Status”"), and what it retired otherwise.
 */
export type ArtifactMerge = { into: PublishedArtifact; retired: RetiredArtifact[]; text: string };
export function artifactOutcomes(entry: Pick<ToolEntry, 'name' | 'input' | 'output'>): { published: PublishedArtifact[]; merges: ArtifactMerge[]; retired: RetiredArtifact[] } {
  const published = publishedArtifacts(entry), retired = retiredArtifacts(entry);
  const merges: ArtifactMerge[] = [];
  for (const into of published) {
    const replaced = retired.filter(r => r.supersededBy?.id === into.id);
    if (!replaced.length) continue;
    // An update of an existing artifact merged it too.
    const count = replaced.length + (into.version > 1 ? 1 : 0);
    merges.push({ into, retired: replaced, text: count > 1 ? `Merged ${count} artifacts into “${into.title}”` : `Replaced “${replaced[0].title}” with “${into.title}”` });
  }
  const merged = new Set(merges.flatMap(m => m.retired.map(r => r.id)));
  return { published: published.filter(p => !merges.some(m => m.into.id === p.id)), merges, retired: retired.filter(r => !merged.has(r.id)) };
}

export type ToolTitle = {
  /** The verb, in the tense of the call's state: "Read", "Running". */
  verb: string;
  /** What it acted on: a path, a command, a query. */
  target: string;
  /** Shown monospaced: paths and commands. */
  code: boolean;
  /** A file to open from the line. */
  file?: string;
  /** Short facts after the title: "exit 0", "4.2s", "+12 −4". */
  meta: string[];
  /** One line saying what went wrong. */
  problem?: string;
};

const firstLine = (text: string) => text.split('\n').find(l => l.trim())?.trim() || '';
const hostOf = (url: string) => { try { const u = new URL(url); return u.host + (u.pathname === '/' ? '' : u.pathname); } catch { return url; } };

/** A call as a person would say it. */
export function describeTool(entry: ToolEntry, root = ''): ToolTitle {
  const i = object(entry.input), running = entry.status === 'running', failed = entry.status === 'failed';
  const path = typeof i.file_path === 'string' ? i.file_path : typeof i.notebook_path === 'string' ? i.notebook_path : typeof i.path === 'string' ? i.path : '';
  const kind = kindOf(entry);
  const meta: string[] = [];
  const problem = failed ? firstLine(entry.output || '') || 'Failed' : undefined;
  const tense = (past: string, present: string) => running ? present : past;
  switch (kind) {
    case 'read': {
      const action = entry.actions?.find(a => a.type === 'read');
      const file = path || action?.path || action?.name || '';
      if (typeof i.offset === 'number' || typeof i.limit === 'number') meta.push(`lines ${(i.offset || 0) + 1}–${(i.offset || 0) + (i.limit || 0) || '…'}`);
      else if (entry.outputLines && !running && entry.name !== 'View image') meta.push(plural(entry.outputLines, 'line'));
      return { verb: tense('Read', 'Reading'), target: file ? shortPath(file, root) : commandText(entry), code: true, file: file || undefined, meta, problem };
    }
    case 'search': {
      const action = entry.actions?.find(a => a.type === 'search');
      const pattern = typeof i.pattern === 'string' ? i.pattern : action?.query || commandText(entry);
      const where = typeof i.path === 'string' ? i.path : action?.path || '';
      return { verb: tense('Searched for', 'Searching for'), target: pattern + (where ? ` in ${shortPath(where, root)}` : ''), code: true, meta, problem };
    }
    case 'list': {
      const action = entry.actions?.find(a => a.type === 'listFiles');
      const target = typeof i.pattern === 'string' ? i.pattern : shortPath(path || action?.path || '', root) || commandText(entry) || '.';
      if (entry.outputLines && !running) meta.push(`${entry.outputLines} found`);
      return { verb: tense('Listed', 'Listing'), target, code: true, meta, problem };
    }
    case 'edit': {
      const files = entry.diff?.files || (path ? [path] : []);
      if (entry.diff) meta.push(`+${entry.diff.added} −${entry.diff.removed}`);
      const verb = entry.name === 'Write' ? tense('Wrote', 'Writing') : tense('Edited', 'Editing');
      return { verb, target: files.length === 1 ? shortPath(files[0], root) : `${files.length} files`, code: files.length === 1, file: files.length === 1 ? files[0] : undefined, meta, problem };
    }
    case 'command': {
      const command = commandText(entry);
      if (!running) {
        if (entry.exitCode !== undefined) meta.push(`exit ${entry.exitCode}`);
        // Under a second is not worth a word on every line.
        const ms = durationOf(entry);
        if (ms !== undefined && ms >= 1000) meta.push(shortDuration(ms));
      }
      if (i.run_in_background === true) return { verb: tense('Started', 'Starting'), target: command, code: true, meta: ['in the background'], problem };
      const why = failed && entry.exitCode !== undefined && entry.exitCode !== 0 ? firstLine((entry.output || '').replace(/^Exit code \d+\n?/, '')) || `Exited with ${entry.exitCode}` : problem;
      return { verb: tense('Ran', 'Running'), target: command || entry.name, code: !!command, meta, problem: why };
    }
    case 'web': {
      if (typeof i.url === 'string') return { verb: tense('Fetched', 'Fetching'), target: hostOf(i.url), code: false, meta, problem };
      const query = typeof i.query === 'string' ? i.query : typeof entry.input === 'string' ? entry.input : '';
      return { verb: tense('Searched the web for', 'Searching the web for'), target: query ? `“${query}”` : '', code: false, meta, problem };
    }
    case 'browser': {
      const said = describeBrowserCall(browserVerb(entry.name) || '', i, running);
      return { verb: said.verb, target: said.target, code: false, meta, problem };
    }
    case 'agent': {
      const what = typeof i.description === 'string' ? i.description : typeof i.prompt === 'string' ? firstLine(i.prompt) : '';
      return { verb: tense('Delegated', 'Delegating'), target: what || 'a task to a sub-agent', code: false, meta, problem };
    }
    case 'thinking': {
      const took = shortDuration(durationOf(entry));
      return { verb: running ? 'Thinking…' : took ? `Thought for ${took}` : 'Thought', target: '', code: false, meta, problem: undefined };
    }
    case 'plan': {
      const todos = Array.isArray(i.todos) ? i.todos : Array.isArray(i.plan) ? i.plan : [];
      const done = todos.filter((t: any) => t?.status === 'completed').length;
      if (todos.length) meta.push(`${done}/${todos.length} done`);
      return { verb: tense('Updated the plan', 'Updating the plan'), target: '', code: false, meta, problem };
    }
    case 'mcp': {
      const [, server = '', tool = entry.name] = /^mcp__(.+?)__(.+)$/.exec(entry.name) || [];
      return { verb: tense('Used', 'Using'), target: `${server.replace(/_/g, ' ')} · ${tool.replace(/_/g, ' ')}`, code: false, meta, problem };
    }
    default: {
      if (entry.name === 'BashOutput' || entry.name === 'TaskOutput') return { verb: tense('Checked', 'Checking'), target: 'background output', code: false, meta, problem };
      const detail = [i.description, i.title, i.query, path, i.action].find(v => typeof v === 'string' && v.trim());
      return { verb: tense('Used', 'Using'), target: entry.name.replace(/_/g, ' ') + (detail ? ` · ${String(detail).slice(0, 120)}` : ''), code: false, meta, problem };
    }
  }
}

export type ToolItem = { kind: 'call'; entry: ToolEntry } | { kind: 'explore' | 'edits'; entries: ToolEntry[] };
const EXPLORE = new Set<ToolKind>(['read', 'search', 'list']);

/**
 * Consecutive reads and searches become one line ("Read 6 files, searched 2
 * patterns"), consecutive edits one ("Edited 3 files +12 −4"). Anything else,
 * and anything failed, stays its own line.
 */
export function groupTools(entries: readonly ToolEntry[]): ToolItem[] {
  const items: ToolItem[] = [];
  for (const entry of entries) {
    if (hiddenTool(entry.name)) continue;
    const kind = kindOf(entry);
    const group = EXPLORE.has(kind) ? 'explore' : kind === 'edit' ? 'edits' : undefined;
    const last = items.at(-1);
    if (group && entry.status !== 'failed' && last && last.kind === group) last.entries.push(entry);
    else if (group && entry.status !== 'failed') items.push({ kind: group, entries: [entry] });
    else items.push({ kind: 'call', entry });
  }
  // A group of one is just that call.
  return items.map(item => item.kind !== 'call' && item.entries.length === 1 ? { kind: 'call', entry: item.entries[0] } : item);
}

const plural = (n: number, one: string, many = one + 's') => `${n} ${n === 1 ? one : many}`;

/** A group's line: "Read 6 files, searched 2 patterns", "Editing 3 files +12 −4", "Read src/app.ts ×2". */
export function describeGroup(item: Exclude<ToolItem, { kind: 'call' }>, root = ''): { text: string; meta: string[]; running: boolean } {
  const running = item.entries.some(e => e.status === 'running');
  if (item.kind === 'edits') {
    const files = new Set(item.entries.flatMap(e => e.diff?.files || [String(object(e.input).file_path || e.id)]));
    const added = item.entries.reduce((n, e) => n + (e.diff?.added || 0), 0), removed = item.entries.reduce((n, e) => n + (e.diff?.removed || 0), 0);
    return { text: `${running ? 'Editing' : 'Edited'} ${plural(files.size, 'file')}`, meta: [`+${added} −${removed}`], running };
  }
  const reads = new Set<string>(); let searches = 0, lists = 0;
  for (const e of item.entries) {
    const kind = kindOf(e), i = object(e.input);
    if (kind === 'read') reads.add(String(i.file_path || i.path || e.actions?.find(a => a.type === 'read')?.path || e.id));
    else if (kind === 'search') searches++;
    else lists++;
  }
  // The same file read again and again is that file, so many times; not "Read 1 file".
  if (reads.size === 1 && !searches && !lists) {
    const [path] = reads;
    return { text: `${running ? 'Reading' : 'Read'} ${shortPath(path, root)} ×${item.entries.length}`, meta: [], running };
  }
  const parts: string[] = [];
  if (reads.size) parts.push(`${running ? 'reading' : 'read'} ${plural(reads.size, 'file')}`);
  if (searches) parts.push(`${running ? 'searching' : 'searched'} ${plural(searches, 'pattern')}`);
  if (lists) parts.push(`${running ? 'listing' : 'listed'} ${plural(lists, 'folder')}`);
  const text = parts.join(', ');
  return { text: text.charAt(0).toUpperCase() + text.slice(1), meta: [], running };
}

/** The id of the conversation's latest plan update (TodoWrite, update_plan), or ''. */
export function latestPlanId(messages: readonly { tools?: ToolEntry[] }[]): string {
  for (let i = messages.length - 1; i >= 0; i--) {
    const tools = messages[i].tools;
    if (!tools) continue;
    for (let j = tools.length - 1; j >= 0; j--) if (kindOf(tools[j]) === 'plan') return tools[j].id;
  }
  return '';
}

// ---- Activity detail ------------------------------------------------------
//
// How much of the agent's tool activity a conversation shows. Verbose is every
// call on its own line. Smart (the default) folds a run of calls between two
// pieces of the agent's prose into one summary line ("Explored the codebase ·
// 8 commands, 3 files read · 58s") and keeps on their own lines only what a
// person must see: failures, edits, the live plan. Quiet folds a whole turn's
// activity into "Worked for 2m 10s · 14 steps · 1 failed". The journal and the
// Activity dialog keep every call in every mode.

export type ActivityDetail = 'quiet' | 'smart' | 'verbose';
export const ACTIVITY_DETAILS: readonly ActivityDetail[] = ['quiet', 'smart', 'verbose'];
export const DEFAULT_ACTIVITY_DETAIL: ActivityDetail = 'smart';
export const isActivityDetail = (value: unknown): value is ActivityDetail => value === 'quiet' || value === 'smart' || value === 'verbose';
/** Thinking shorter than this is not worth a word in Smart and Quiet. */
export const THINKING_WORTH_MS = 10_000;
/** A duration shorter than this is not worth a word in Smart and Quiet. */
const QUICK_MS = 1000;

/** A call that failed: an error, or a command that exited non-zero. */
export function isFailure(entry: ToolEntry): boolean {
  return entry.status === 'failed' || (entry.status !== 'running' && entry.exitCode !== undefined && entry.exitCode !== 0);
}

/**
 * A shell line as segments of words, split at `&&`, `||`, `;`, `|` and `&`,
 * quotes removed. Good enough to name what a command does; never used to run one.
 */
function shellSegments(line: string): string[][] {
  const segments: string[][] = []; let words: string[] = [], word = '', quoted = false, quote = '';
  const endWord = () => { if (word || quoted) words.push(word); word = ''; quoted = false; };
  const endSegment = () => { endWord(); if (words.length) segments.push(words); words = []; };
  for (let i = 0; i < line.length; i++) {
    const c = line[i];
    if (quote) { if (c === quote) quote = ''; else if (c === '\\' && quote === '"' && i + 1 < line.length) word += line[++i]; else word += c; continue; }
    if (c === '"' || c === "'") { quote = c; quoted = true; continue; }
    if (c === '\\' && i + 1 < line.length) { word += line[++i]; continue; }
    if (c === ' ' || c === '\t') { endWord(); continue; }
    if (c === '&' && line[i + 1] === '&' || c === '|' && line[i + 1] === '|') { endSegment(); i++; continue; }
    // `2>&1` is a redirect, not a background job.
    if (c === '&' && /[<>]$/.test(word)) { word += c; continue; }
    if (c === ';' || c === '|' || c === '&') { endSegment(); continue; }
    if (c === '#' && !word) break;
    word += c;
  }
  endSegment();
  return segments;
}

const HEREDOC = /<<-?\s*['"]?([A-Za-z_][\w-]*)['"]?/;
/** The logical lines of a script: continuations joined, comments and heredoc bodies skipped. */
function logicalLines(command: string): string[] {
  const lines: string[] = []; let heredoc = '';
  for (const raw of command.replace(/\\\r?\n/g, ' ').split(/\r?\n/)) {
    const line = raw.trim();
    if (heredoc) { if (line === heredoc) heredoc = ''; continue; }
    if (!line || line.startsWith('#')) continue;
    lines.push(line.replace(new RegExp(HEREDOC.source, 'g'), ''));
    heredoc = HEREDOC.exec(line)?.[1] || '';
  }
  return lines;
}
const firstLogicalLine = (command: string) => logicalLines(command)[0] || '';

const SETUP = new Set(['cd', 'pushd', 'popd', 'export', 'set', 'source', '.', 'true', 'unset', 'shopt', 'trap', 'ulimit', 'umask']);
const NOISE = new Set(['echo', 'printf', 'sleep', 'clear']);
const WRAPPERS = new Set(['sudo', 'time', 'nohup', 'env', 'command', 'exec', 'nice', 'npx', 'bunx', 'stdbuf']);
/** Programs whose first words say what they do (`git status`, `go test ./...`): how many. */
const SUBCOMMANDS: Record<string, number> = { git: 1, npm: 1, pnpm: 1, yarn: 1, bun: 1, cargo: 1, go: 2, make: 1, docker: 1, kubectl: 2, gh: 2, uv: 1, pip: 1, pip3: 1, poetry: 1, brew: 1, deno: 1, terraform: 1, helm: 1, buf: 1, swift: 1, dotnet: 1, mvn: 1, gradle: 1, gradlew: 1, rustup: 1, apt: 1, 'apt-get': 1, systemctl: 1, lazybox: 2, codefly: 2, nx: 1, turbo: 1, just: 1, mise: 1, gem: 1, bundle: 1 };
/** Options that take the next word as their value. */
const VALUED = new Set(['-C', '-c', '-f', '-o', '-w', '-m', '-A', '-B', '--prefix', '--workspace', '--filter', '--manifest-path', '-p', '--package', '--config', '-F', '--dir', '--cwd']);
const SEARCHERS = new Set(['rg', 'grep', 'egrep', 'fgrep', 'ag', 'ack']);
const REDIRECT = /^\d*(?:[<>]|>>|&>)&?\d*$/;
const basename = (path: string) => path.replace(/\/+$/, '').split('/').pop() || path;
const looksLikePath = (word: string) => !/^\d+(,\d+)?[pd]?$/.test(word) && !/[=<>$`]/.test(word) && (word.includes('/') || /\.[A-Za-z]\w{0,7}$/.test(word));
const clip = (text: string, limit: number) => text.length > limit ? text.slice(0, limit - 1) + '…' : text;
/** A long path as its file, a short one (or a package pattern) whole: `…/runtime_access.go`, `./internal/...`. */
const briefPath = (path: string) => path.length > 24 && path.includes('/') && /[A-Za-z]/.test(basename(path)) ? `…/${basename(path)}` : clip(path, 40);

export type ParsedCommand = { program: string; words: string[]; written?: string };
/** What a shell command mainly runs: the program and its words, past setup (`cd`, env) and wrappers. */
export function parseCommand(command: string): ParsedCommand | undefined {
  // The Lazybox CLI by its full path or $LAZYBOX_V2_CLI reads as `lazybox`.
  let text = plainLazybox(command.trim());
  // `/bin/zsh -lc "…"` (how Codex reports commands) is what is inside it.
  const wrapped = /^(?:\S*\/)?(?:ba|z|da)?sh\s+-l?c\s+(['"])([\s\S]*)\1\s*$/.exec(text);
  if (wrapped) text = wrapped[2];
  // A script's lines run one after another, like `;`.
  const segments = logicalLines(text).flatMap(shellSegments);
  const meaningful = segments.filter(s => !SETUP.has(s[0]) && !(s.length === 1 && /^\w+=/.test(s[0])));
  const main = meaningful.find(s => !NOISE.has(s[0])) || meaningful[0];
  if (!main) return;
  let words: string[] = [], written: string | undefined;
  for (let i = 0; i < main.length; i++) {
    const w = main[i];
    // A redirect and its target are not arguments: `cat > out.txt`.
    if (REDIRECT.test(w)) { if (w.includes('>') && !w.includes('&') && main[i + 1]) written = main[i + 1]; i++; continue; }
    if (/^\d*>/.test(w) || /^</.test(w)) { if (/^>[^&]/.test(w)) written = w.slice(1); continue; }
    words.push(w);
  }
  for (;;) {
    if (/^\w+=/.test(words[0] || '') || WRAPPERS.has(words[0])) words = words.slice(1);
    else if (words[0] === 'timeout' || words[0] === 'gtimeout') words = words.slice(words[1]?.startsWith('-') ? 3 : 2);
    else break;
  }
  if (!words.length) return;
  return { program: basename(words[0]), words: words.slice(1), written };
}

/** The words of a command that are not options or their values. */
function plainWords(program: string, words: readonly string[]): string[] {
  const plain: string[] = [], searcher = SEARCHERS.has(program);
  for (let i = 0; i < words.length; i++) {
    const w = words[i];
    if (searcher && (w === '-e' || w === '--regexp')) { if (words[i + 1] !== undefined) plain.push(words[++i]); continue; }
    if (VALUED.has(w) || (!searcher && w === '-e')) { i++; continue; }
    if (w.startsWith('-') || w === '') continue;
    if (searcher && /^\d+$/.test(w)) continue;
    plain.push(w);
  }
  return plain;
}

/**
 * A command as short as it stays recognisable: the program and what it acts
 * on (`sed …/runtime_access.go`, `npm run test:client`, `rg parent_tool_use_id`),
 * never a multi-line script. The full command stays one click away.
 */
export function shortCommand(command: string): string {
  const parsed = parseCommand(command);
  if (!parsed) return clip(firstLogicalLine(command) || command.trim(), 60);
  const { program, words, written } = parsed;
  // `lazybox run [options] -- go test ./...` reads as what it runs: `lazybox run go test ./...`.
  if (program === 'lazybox' && words[0] === 'run') {
    const at = words.indexOf('--'), inner = at >= 0 ? words.slice(at + 1) : words.slice(1).filter(w => !w.startsWith('-'));
    if (inner.length) return clip(`lazybox run ${shortCommand(inner.join(' '))}`, 60);
  }
  const plain = plainWords(program, words);
  let rest: string[];
  if (program in SUBCOMMANDS) {
    const count = ['npm', 'pnpm', 'yarn', 'bun', 'uv', 'docker', 'cargo'].includes(program) && ['run', 'exec', 'compose', 'pip', 'x', 'run-script'].includes(plain[0]) ? 2 : SUBCOMMANDS[program];
    rest = plain.slice(0, count).map(briefPath);
  } else if (SEARCHERS.has(program)) {
    rest = plain.slice(0, 1).map(p => clip(p, 32));
  } else {
    const path = [...plain].reverse().find(looksLikePath);
    rest = path ? [briefPath(path)] : plain.slice(0, 1).map(p => clip(p, 32));
  }
  if (!rest.length && written) rest = ['>', briefPath(written)];
  return clip([program, ...rest].join(' '), 60);
}

/** The agent's own words for a call, when it gave them (Claude's Bash `description`). */
export function callReason(entry: Pick<ToolEntry, 'input'>): string {
  const i = object(entry.input);
  const said = [i.description, i.reason, i.justification].find(v => typeof v === 'string' && v.trim()) as string | undefined;
  if (!said) return '';
  const text = firstLine(said).replace(/[.:]$/, '');
  return clip(text.charAt(0).toUpperCase() + text.slice(1), 90);
}

/**
 * A call's line in Smart and Quiet: a command in the agent's own words, else
 * its short form; no "exit 0", no duration under a second. `aside`: the target
 * is the command beside the agent's words, shown quieter.
 */
export function describeToolBrief(entry: ToolEntry, root = ''): ToolTitle & { aside?: boolean } {
  const title = describeTool(entry, root);
  const took = durationOf(entry);
  const quick = took !== undefined && took < QUICK_MS ? shortDuration(took) : '';
  const meta = title.meta.filter(m => m !== 'exit 0' && (!quick || m !== quick));
  if (kindOf(entry) !== 'command') return { ...title, meta };
  const command = commandText(entry);
  const short = command ? shortCommand(command) : title.target;
  const reason = callReason(entry);
  if (object(entry.input).run_in_background === true) return { ...title, target: reason || short, code: !reason, meta };
  if (reason) return { ...title, verb: reason, target: short, code: true, meta, aside: true };
  return { ...title, target: short, meta };
}

export type ActivityCategory = 'explore' | 'test' | 'check' | 'build' | 'install' | 'git' | 'web' | 'browser' | 'command' | 'tool' | 'plan' | 'edit' | 'thinking';
const EXPLORING = new Set(['sed', 'cat', 'head', 'tail', 'less', 'more', 'nl', 'wc', 'ls', 'tree', 'find', 'fd', 'rg', 'grep', 'egrep', 'fgrep', 'ag', 'ack', 'file', 'stat', 'du', 'jq', 'yq', 'awk', 'diff', 'pwd', 'which', 'type', 'realpath', 'readlink', 'sort', 'uniq', 'cut', 'column', 'xxd', 'od', 'strings', 'bat']);
const GIT_READS = new Set(['status', 'log', 'diff', 'show', 'blame', 'branch', 'grep', 'ls-files', 'rev-parse', 'remote', 'describe', 'shortlog', 'reflog', 'ls-tree', 'cat-file', 'merge-base']);
const INSTALLS = /^(install|i|ci|add|sync|get|fetch|download|deps|setup|bootstrap|restore)$/;
const word = (pattern: string) => new RegExp(`(^|[\\s:/_.-])(${pattern})([\\s:_.-]|$)`, 'i');
const TESTS = word('e2e|tests?|spec|pytest|jest|vitest|mocha|playwright|nextest|ctest'), CHECKS = word('lint|typecheck|check|clippy|vet|fmt|format|eslint|prettier|ruff|mypy|tsc'), BUILDS = word('build|compile|bundle|webpack|esbuild|rollup|cmake|ninja');
/** What a call was for, to say what a run of them did. */
export function categoryOf(entry: ToolEntry): ActivityCategory {
  const kind = kindOf(entry);
  if (kind === 'read' || kind === 'search' || kind === 'list') return 'explore';
  if (kind === 'web' || kind === 'browser' || kind === 'plan' || kind === 'edit' || kind === 'thinking') return kind;
  if (kind !== 'command') return 'tool';
  const parsed = parseCommand(commandText(entry));
  if (!parsed) return 'command';
  const { program, words } = parsed, plain = plainWords(program, words), first = plain[0] || '';
  if (program === 'git') return GIT_READS.has(first) ? 'explore' : 'git';
  if (program === 'gh') return 'git';
  if (EXPLORING.has(program)) return 'explore';
  const said = [program, ...plain.slice(0, 3)].join(' ');
  if (TESTS.test(said) || (program === 'node' && words.includes('--test'))) return 'test';
  if (program in SUBCOMMANDS && INSTALLS.test(first)) return program === 'make' && !/^(deps|setup|bootstrap)$/.test(first) ? 'build' : 'install';
  if (CHECKS.test(said)) return 'check';
  if (BUILDS.test(said) || program === 'make') return 'build';
  return 'command';
}

const PHRASES: Record<ActivityCategory, [string, string]> = {
  explore: ['Explored the codebase', 'Exploring the codebase'], test: ['Ran tests', 'Running tests'], check: ['Ran checks', 'Running checks'],
  build: ['Built the project', 'Building the project'], install: ['Installed dependencies', 'Installing dependencies'], git: ['Worked with git', 'Working with git'],
  web: ['Searched the web', 'Searching the web'], browser: ['Used the browser', 'Using the browser'], command: ['Ran commands', 'Running commands'], tool: ['Used tools', 'Using tools'],
  plan: ['Updated the plan', 'Updating the plan'], edit: ['Edited files', 'Editing files'], thinking: ['Thought', 'Thinking'],
};
/** The order categories are named in, when they are as frequent. */
const NAMED_FIRST: ActivityCategory[] = ['test', 'check', 'build', 'install', 'explore', 'git', 'web', 'browser', 'command', 'tool', 'edit', 'plan'];
const SIGNIFICANT = new Set<ActivityCategory>(['test', 'check', 'build', 'install']);
const lower = (text: string) => text.charAt(0).toLowerCase() + text.slice(1);

/** The calls of a run: thinking is time, not a step. */
export const stepsOf = (entries: readonly ToolEntry[]) => entries.filter(e => kindOf(e) !== 'thinking' && !hiddenTool(e.name));
const thinkingMs = (entries: readonly ToolEntry[]) => entries.reduce((n, e) => kindOf(e) === 'thinking' && e.status !== 'running' ? n + (durationOf(e) || 0) : n, 0);
/** Wall time from the first call's start to the last one's end. */
export function spanOf(entries: readonly ToolEntry[]): number | undefined {
  let start = Infinity, end = -Infinity;
  for (const e of entries) {
    if (e.startedAt !== undefined) start = Math.min(start, e.startedAt);
    const ended = e.endedAt ?? (e.startedAt !== undefined && e.durationMs !== undefined ? e.startedAt + e.durationMs : undefined);
    if (ended !== undefined) end = Math.max(end, ended);
  }
  if (Number.isFinite(start) && Number.isFinite(end) && end >= start) return end - start;
  return entries.reduce((n, e) => n + (e.durationMs || 0), 0) || undefined;
}

/** The counts a summary carries: "8 commands, 3 files read, 2 searches". */
export function stepCounts(entries: readonly ToolEntry[]): string {
  const reads = new Set<string>(); let commands = 0, searches = 0, web = 0, browser = 0, edits = 0, other = 0;
  for (const e of stepsOf(entries)) {
    const kind = kindOf(e), i = object(e.input);
    if (kind === 'read') reads.add(String(i.file_path || i.path || e.actions?.find(a => a.type === 'read')?.path || e.id));
    else if (kind === 'search' || kind === 'list') searches++;
    else if (kind === 'command') commands++;
    else if (kind === 'web') web++;
    else if (kind === 'browser') browser++;
    else if (kind === 'edit') edits++;
    else if (kind !== 'plan') other++;
  }
  return [commands && plural(commands, 'command'), reads.size && `${plural(reads.size, 'file')} read`, searches && plural(searches, 'search', 'searches'), web && plural(web, 'web lookup'), browser && plural(browser, 'browser step'), edits && `${plural(edits, 'file')} edited`, other && plural(other, 'other tool')].filter(Boolean).join(', ');
}

/** A run's summary: "Explored the codebase and ran tests · 8 commands, 3 files read · 58s". */
export function summarizeRun(entries: readonly ToolEntry[]): { text: string; meta: string[]; running: boolean; category: ActivityCategory } {
  const steps = stepsOf(entries);
  const running = entries.some(e => e.status === 'running');
  const counts = new Map<ActivityCategory, number>();
  for (const e of steps) { const c = categoryOf(e); counts.set(c, (counts.get(c) || 0) + 1); }
  // While it works, what it does now is named first.
  const current = running ? [...steps].reverse().find(e => e.status === 'running') : undefined;
  const now = current ? categoryOf(current) : undefined;
  const ranked = [...counts.keys()].sort((a, b) => Number(b === now) - Number(a === now)
    || Number(SIGNIFICANT.has(b)) - Number(SIGNIFICANT.has(a)) || counts.get(b)! - counts.get(a)! || NAMED_FIRST.indexOf(a) - NAMED_FIRST.indexOf(b));
  const tense = running ? 1 : 0;
  const phrase = (c: ActivityCategory) => {
    const mine = steps.filter(e => categoryOf(e) === c);
    if (c === 'explore' && mine.every(e => kindOf(e) === 'search' || kindOf(e) === 'list')) return running ? 'Searching the codebase' : 'Searched the codebase';
    if (c === 'web' && mine.every(e => typeof object(e.input).url === 'string')) return running ? 'Browsing the web' : 'Browsed the web';
    if (c === 'command' && mine.length === 1) return running ? 'Running a command' : 'Ran a command';
    if (c === 'tool' && mine.length === 1) return running ? 'Using a tool' : 'Used a tool';
    return PHRASES[c][tense];
  };
  const named = ranked.slice(0, 2).map(phrase);
  const text = named.length ? named[0] + (named[1] ? ` and ${lower(named[1])}` : '') : running ? 'Thinking' : 'Thought';
  const meta: string[] = [];
  const counted = stepCounts(steps);
  if (counted) meta.push(counted);
  const thought = thinkingMs(entries);
  if (thought >= THINKING_WORTH_MS) meta.push(`thought for ${shortDuration(thought)}`);
  const span = running ? undefined : spanOf(entries);
  if (span !== undefined && span >= QUICK_MS) meta.push(shortDuration(span));
  return { text, meta, running, category: ranked[0] || 'thinking' };
}

/** A turn's line in Quiet: "Worked for 2m 10s · 14 steps · 1 failed". */
export function summarizeTurn(entries: readonly ToolEntry[]): { text: string; meta: string[]; running: boolean; failed: number; steps: number } {
  const steps = stepsOf(entries), running = entries.some(e => e.status === 'running');
  const failed = steps.filter(isFailure).length;
  const span = running ? undefined : spanOf(entries);
  const text = running ? 'Working' : span !== undefined && span >= QUICK_MS ? `Worked for ${shortDuration(span)}` : 'Worked';
  return { text, meta: [plural(steps.length, 'step'), ...(failed ? [`${failed} failed`] : [])], running, failed, steps: steps.length };
}

/**
 * The lines of a run of calls in Smart: what needs no attention is one
 * summary (`run`) where its first call was; failures, edits and the live plan
 * keep their own lines, in order (consecutive edits as one). A run of one call
 * is just that call. Quiet keeps only edits and the live plan (`quiet`).
 */
export type BriefItem = { kind: 'run'; entries: ToolEntry[] } | { kind: 'call'; entry: ToolEntry } | { kind: 'edits'; entries: ToolEntry[] };
/** What Smart never folds away. */
export function stands(entry: ToolEntry, latestPlan = ''): boolean {
  const kind = kindOf(entry);
  return isFailure(entry) || kind === 'edit' || (kind === 'plan' && !!latestPlan && entry.id === latestPlan);
}
const isEdit = (entry: ToolEntry) => kindOf(entry) === 'edit' && !isFailure(entry);
/** Thinking worth a line: running at the end of the run, or long. */
const thinkingShown = (entry: ToolEntry, last: boolean) => entry.status === 'running' ? last : (durationOf(entry) || 0) >= THINKING_WORTH_MS;
/** What Quiet keeps beside a turn's one line: edits and the live plan. */
export const keptInQuiet = (entry: ToolEntry, latestPlan = '') => isEdit(entry) || (kindOf(entry) === 'plan' && !!latestPlan && entry.id === latestPlan);
export function briefItems(entries: readonly ToolEntry[], latestPlan = '', quiet = false): BriefItem[] {
  const visible = entries.filter(e => !hiddenTool(e.name));
  // Quiet still shows the step running now, where the agent is.
  const now = quiet && visible.at(-1)?.status === 'running' ? visible.at(-1) : undefined;
  const keep = quiet ? (e: ToolEntry) => e === now || keptInQuiet(e, latestPlan) : (e: ToolEntry) => stands(e, latestPlan);
  const folded = visible.filter(e => !keep(e));
  const fold = !quiet && stepsOf(folded).length >= 2;
  const items: BriefItem[] = [];
  let run: BriefItem | undefined;
  visible.forEach((entry, index) => {
    if (!keep(entry)) {
      if (quiet) return;
      if (fold) { if (!run) items.push(run = { kind: 'run', entries: folded }); return; }
      if (kindOf(entry) === 'thinking' && !thinkingShown(entry, index === visible.length - 1)) return;
      items.push({ kind: 'call', entry });
      return;
    }
    const last = items.at(-1);
    if (isEdit(entry) && last && (last.kind === 'edits' || (last.kind === 'call' && isEdit(last.entry)))) {
      if (last.kind === 'edits') last.entries.push(entry); else items[items.length - 1] = { kind: 'edits', entries: [last.entry, entry] };
      return;
    }
    items.push({ kind: 'call', entry });
  });
  return items;
}

/** The calls a summary shows when opened: short thinking says nothing. */
export const unfolded = (entries: readonly ToolEntry[]) => entries.filter((e, i) => !hiddenTool(e.name) && (kindOf(e) !== 'thinking' || thinkingShown(e, i === entries.length - 1)));

/**
 * Quiet: every tool call of a turn, keyed by the turn's first run of calls
 * on screen (from `firstShown`), where its one line goes. Arrays are reused while their parts are, so a row
 * whose turn did not change does not render again.
 */
export type TurnCache = Map<string, { parts: ToolEntry[][]; entries: ToolEntry[] }>;
export function turnActivity(messages: readonly { id: string; type: string; turnId: string; tools?: ToolEntry[] }[], cache: TurnCache, firstShown = 0): Map<string, ToolEntry[]> {
  const turns = new Map<string, { anchor: string; parts: ToolEntry[][] }>();
  messages.forEach((m, index) => {
    if (m.type !== 'tools' || !m.tools) return;
    const key = m.turnId || `message:${m.id}`;
    const turn = turns.get(key);
    if (!turn) turns.set(key, { anchor: index >= firstShown ? m.id : '', parts: [m.tools] });
    else { turn.parts.push(m.tools); if (!turn.anchor && index >= firstShown) turn.anchor = m.id; }
  });
  const anchors = new Map<string, ToolEntry[]>();
  for (const [key, turn] of turns) {
    const kept = cache.get(key);
    if (kept && kept.parts.length === turn.parts.length && kept.parts.every((p, i) => p === turn.parts[i])) { anchors.set(turn.anchor, kept.entries); continue; }
    const entries = turn.parts.length === 1 ? turn.parts[0] : turn.parts.flat();
    cache.set(key, { parts: turn.parts, entries });
    anchors.set(turn.anchor, entries);
  }
  for (const key of [...cache.keys()]) if (!turns.has(key)) cache.delete(key);
  return anchors;
}
