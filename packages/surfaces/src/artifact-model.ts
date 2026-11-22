import type { Artifact, ArtifactVersion } from '@lazybox/protocol';
import { fileReference } from './file-reference';

/** Pure helpers for artifacts: titles, kinds, line diffs, summaries. No React, no Runner. */

export const ARTIFACT_KINDS = ['plan', 'report', 'spec', 'notes', 'todo-list', 'other'] as const;
export type ArtifactKind = (typeof ARTIFACT_KINDS)[number];
export const kindLabel: Record<ArtifactKind, string> = { plan: 'Plan', report: 'Report', spec: 'Spec', notes: 'Notes', 'todo-list': 'TODO list', other: 'Document' };
/** An icon name from Icons.tsx for each kind. */
export const kindIcon: Record<ArtifactKind, string> = { plan: 'checklist', report: 'report', spec: 'spec', notes: 'notes', 'todo-list': 'checklist', other: 'file' };
export const asKind = (value: string): ArtifactKind => (ARTIFACT_KINDS as readonly string[]).includes(value) ? value as ArtifactKind : 'other';

const clean = (line: string) => line.replace(/[*_`~]|\[([^\]]*)\]\([^)]*\)/g, (_, text) => text ?? '').replace(/\s+/g, ' ').trim();

/** The first ATX or setext heading outside code fences, else the first prose line, at most 120 characters. */
export function titleFromMarkdown(text: string, fallback = 'Untitled'): string {
  const lines = text.split('\n');
  let fenced = false, firstLine = '';
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    if (/^\s*(```|~~~)/.test(line)) { fenced = !fenced; continue; }
    if (fenced) continue;
    const atx = /^\s{0,3}#{1,6}\s+(.*?)\s*#*\s*$/.exec(line);
    if (atx && clean(atx[1])) return shorten(clean(atx[1]));
    if (line.trim() && /^\s*(=+|-+)\s*$/.test(lines[i + 1] || '') && !/^\s*[-*+]\s/.test(line)) return shorten(clean(line));
    if (!firstLine && line.trim() && !/^\s*([-*_]\s*){3,}$/.test(line)) firstLine = clean(line.replace(/^\s*([-*+>]|\d+\.)\s+/, ''));
  }
  return firstLine ? shorten(firstLine.replace(/[:.]$/, '')) : fallback;
}
const shorten = (value: string, limit = 120) => value.length <= limit ? value : value.slice(0, limit - 1).trimEnd() + '…';

/** A kind suggested by the title and the shape of the text; the person can change it. */
export function guessKind(title: string, text = ''): ArtifactKind {
  const t = title.toLowerCase();
  if (/\b(plan|roadmap|steps|migration|rollout|approach)\b/.test(t)) return 'plan';
  if (/\b(spec|specification|design|rfc|proposal|contract)\b/.test(t)) return 'spec';
  if (/\b(report|review|audit|findings|summary|analysis|investigation|postmortem|results)\b/.test(t)) return 'report';
  if (/\b(notes?|log|journal|memo)\b/.test(t)) return 'notes';
  if (/^\s*(- \[[ x]\]|\d+\.)\s/m.test(text) && /\b(step|phase|todo|milestone)/i.test(text)) return 'plan';
  return 'other';
}

/** A file name for downloads and the repository: lowercase words joined by dashes. */
export function slug(title: string): string {
  const value = title.normalize('NFKD').replace(/[̀-ͯ]/g, '').toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '').slice(0, 60).replace(/-+$/, '');
  return value || 'artifact';
}

export type DiffLine = { kind: 'same' | 'add' | 'remove'; text: string; before?: number; after?: number };
/**
 * A line diff (Myers, O((N+M)·D)), with a guard: past `limit` edit steps it
 * falls back to "everything removed, everything added", which is still correct.
 */
export function lineDiff(before: string, after: string, limit = 4000): DiffLine[] {
  const a = before === '' ? [] : before.split('\n'), b = after === '' ? [] : after.split('\n');
  // Trim the common head and tail: most versions change a few lines.
  let head = 0; while (head < a.length && head < b.length && a[head] === b[head]) head++;
  let tail = 0; while (tail < a.length - head && tail < b.length - head && a[a.length - 1 - tail] === b[b.length - 1 - tail]) tail++;
  const x = a.slice(head, a.length - tail), y = b.slice(head, b.length - tail);
  const middle = myers(x, y, limit) ?? [...x.map(text => ({ kind: 'remove' as const, text })), ...y.map(text => ({ kind: 'add' as const, text }))];
  const out: DiffLine[] = [];
  let i = 0, j = 0;
  const push = (kind: DiffLine['kind'], text: string) => {
    if (kind === 'same') out.push({ kind, text, before: ++i, after: ++j });
    else if (kind === 'remove') out.push({ kind, text, before: ++i });
    else out.push({ kind, text, after: ++j });
  };
  for (let k = 0; k < head; k++) push('same', a[k]);
  for (const line of middle) push(line.kind, line.text);
  for (let k = a.length - tail; k < a.length; k++) push('same', a[k]);
  return out;
}

function myers(a: string[], b: string[], limit: number): { kind: DiffLine['kind']; text: string }[] | undefined {
  const n = a.length, m = b.length, max = n + m;
  if (!n) return b.map(text => ({ kind: 'add', text }));
  if (!m) return a.map(text => ({ kind: 'remove', text }));
  const offset = max + 1, v = new Int32Array(2 * max + 3), trace: Int32Array[] = [];
  for (let d = 0; d <= max; d++) {
    if (d > limit) return undefined;
    trace.push(v.slice());
    for (let k = -d; k <= d; k += 2) {
      let xi = k === -d || (k !== d && v[offset + k - 1] < v[offset + k + 1]) ? v[offset + k + 1] : v[offset + k - 1] + 1;
      let yi = xi - k;
      while (xi < n && yi < m && a[xi] === b[yi]) { xi++; yi++; }
      v[offset + k] = xi;
      if (xi >= n && yi >= m) return backtrack(a, b, trace, offset, d);
    }
  }
  return undefined;
}
function backtrack(a: string[], b: string[], trace: Int32Array[], offset: number, depth: number) {
  const out: { kind: DiffLine['kind']; text: string }[] = [];
  let xi = a.length, yi = b.length;
  for (let d = depth; d > 0; d--) {
    const v = trace[d], k = xi - yi;
    const prevK = k === -d || (k !== d && v[offset + k - 1] < v[offset + k + 1]) ? k + 1 : k - 1;
    const prevX = v[offset + prevK], prevY = prevX - prevK;
    while (xi > prevX && yi > prevY) { out.push({ kind: 'same', text: a[--xi] }); yi--; }
    if (xi === prevX) out.push({ kind: 'add', text: b[--yi] }); else out.push({ kind: 'remove', text: a[--xi] });
  }
  while (xi > 0 && yi > 0) { out.push({ kind: 'same', text: a[--xi] }); yi--; }
  return out.reverse();
}

export const diffStats = (lines: DiffLine[]) => ({ added: lines.filter(l => l.kind === 'add').length, removed: lines.filter(l => l.kind === 'remove').length });

export type DiffHunk = { lines: DiffLine[]; hiddenBefore: number };
/** Changed lines with `context` lines around them; long unchanged runs fold into a count. */
export function hunks(lines: DiffLine[], context = 3): DiffHunk[] {
  const keep = new Array(lines.length).fill(false);
  lines.forEach((line, i) => { if (line.kind !== 'same') for (let k = Math.max(0, i - context); k <= Math.min(lines.length - 1, i + context); k++) keep[k] = true; });
  const out: DiffHunk[] = [];
  let hidden = 0, current: DiffHunk | undefined;
  lines.forEach((line, i) => {
    if (!keep[i]) { hidden++; current = undefined; return; }
    if (!current) { current = { lines: [], hiddenBefore: hidden }; out.push(current); hidden = 0; }
    current.lines.push(line);
  });
  if (hidden && out.length) out.push({ lines: [], hiddenBefore: hidden });
  return out;
}

/** "just now", "4 min ago", "3 h ago", "2 days ago", then the date. */
export function ago(unixMillis: bigint | number, now = Date.now()): string {
  const millis = Number(unixMillis);
  if (!millis) return '';
  const seconds = Math.max(0, Math.round((now - millis) / 1000));
  if (seconds < 45) return 'just now';
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `${minutes} min ago`;
  const hours = Math.round(minutes / 60);
  if (hours < 24) return `${hours} h ago`;
  const days = Math.round(hours / 24);
  if (days < 7) return `${days} day${days === 1 ? '' : 's'} ago`;
  return new Date(millis).toLocaleDateString(undefined, { month: 'short', day: 'numeric', year: new Date(millis).getFullYear() === new Date(now).getFullYear() ? undefined : 'numeric' });
}

/** Who made a version, in a few words: "you", "an agent", "the file on disk". */
export function versionOrigin(version: ArtifactVersion | undefined, agentName?: (sessionId: string) => string | undefined): string {
  if (!version) return '';
  const author = version.author?.kind, source = version.source?.kind;
  if (author === 'file' || (!author && source === 'file')) return 'file changed on disk';
  if (author === 'agent') return `published by ${agentName?.(version.author!.sessionId) || 'an agent'}`;
  if (source === 'message') return 'from a chat message';
  return 'edited by you';
}

/** The list line: "Updated from file · 2 min ago", "v3 · from a chat message · just now". */
export function updatedLine(artifact: Artifact, now = Date.now()): string {
  const latest = artifact.versions.at(-1);
  const when = ago(artifact.updatedAtUnixMillis, now);
  return [`v${latest?.number ?? 1}`, versionOrigin(latest), when].filter(Boolean).join(' · ');
}

/**
 * What "Send to agent" puts in a message box: the reference an agent can fetch
 * with the CLI, and a short outline so the person sees what they are sending.
 */
export function agentReference(artifact: Pick<Artifact, 'id' | 'title' | 'kind' | 'versions'> & Partial<Pick<Artifact, 'links'>>, content: string, budget = 1200, version = artifact.versions.at(-1)?.number ?? 1): string {
  const outline: string[] = [];
  let fenced = false, used = 0;
  for (const line of content.split('\n')) {
    if (/^\s*(```|~~~)/.test(line)) { fenced = !fenced; continue; }
    if (fenced) continue;
    const heading = /^\s{0,3}(#{1,3})\s+(.*)$/.exec(line);
    const item = heading ? `${'  '.repeat(heading[1].length - 1)}- ${clean(heading[2])}` : '';
    if (!item || used + item.length > budget) continue;
    outline.push(item); used += item.length + 1;
  }
  const lines = [`Artifact “${artifact.title}” (${kindLabel[asKind(artifact.kind)].toLowerCase()}, v${version}). Read it in full with \`lazybox artifact get ${artifact.id} --version ${version}\`.`];
  for (const input of artifact.links ?? []) {
    if (input.kind === 'artifact') lines.push(`Source ${input.role}: \`lazybox artifact get ${input.id} --version ${input.version}\`.`);
  }
  if (outline.length) lines.push('', 'Outline:', ...outline);
  else if (content.trim()) lines.push('', shorten(content.trim().split('\n').slice(0, 6).join('\n'), budget));
  lines.push('', '');
  return lines.join('\n');
}

/** Who made the latest version, for a list row: "by an agent", "from the file", "from chat", "by you". */
export function authorShort(version: ArtifactVersion | undefined): string {
  if (!version) return '';
  const author = version.author?.kind, source = version.source?.kind;
  if (author === 'file' || (!author && source === 'file')) return 'from the file';
  if (author === 'agent') return 'by an agent';
  if (source === 'message') return 'from chat';
  return 'by you';
}

/**
 * "Updated since you looked": a version the person did not make, newer than
 * the one they last saw (or one they never opened). `seen` is the
 * `updatedAtUnixMillis` they last saw, as a string.
 */
export function updatedSinceSeen(artifact: Pick<Artifact, 'updatedAtUnixMillis' | 'versions'>, seen: string | undefined): boolean {
  const latest = artifact.versions.at(-1);
  if (!latest || (latest.author?.kind || 'person') === 'person') return false;
  return seen === undefined || Number(latest.createdAtUnixMillis) > Number(seen);
}

/** A workspace path for "Save to repository": `member/docs/<slug>.md`. */
export const suggestedPath = (member: string, title: string) => `${member ? member + '/' : ''}docs/${slug(title)}.md`;

/** Files an artifact can follow: Markdown, which the artifact page renders. */
export const trackableFile = (path: string) => /\.(md|markdown|mdx)$/i.test(path);

// A code span (its backtick run, its text), or a Markdown link or image (its `!`, its target).
const spanOrLink = /(`+)([^`][^]*?)\1(?!`)|(!?)\[[^\]]*\]\(\s*<?([^)\s>]+)>?(?:\s+"[^"]*")?\s*\)/g;

/**
 * The Markdown files a message links, as `RichContent` makes them links: a
 * Markdown link to a file, or inline code that names one. Each once, in the
 * order written; nothing inside a fenced code block counts.
 */
export function linkedMarkdownFiles(text: string): string[] {
  const found: string[] = [];
  let fenced = false;
  for (const line of text.split('\n')) {
    if (/^\s{0,3}(```|~~~)/.test(line)) { fenced = !fenced; continue; }
    if (fenced) continue;
    for (const match of line.matchAll(spanOrLink)) {
      const link = match[1] ? fileReference(match[2], 'automatic') : match[3] ? undefined : fileReference(match[4]);
      if (link && trackableFile(link.path) && !found.includes(link.path)) found.push(link.path);
    }
  }
  return found;
}

/**
 * A message that mostly names a file ("Wrote `docs/PLAN.md`."): what is worth
 * tracking is the file, not the words around it. A longer reply is its own
 * content, whatever it links.
 */
export const mostlyNamesFile = (text: string, words = 30) =>
  !/^\s{0,3}(```|~~~)/m.test(text) && text.trim().split(/\s+/).length <= words && linkedMarkdownFiles(text).length > 0;

// ---- Finding artifacts ----------------------------------------------------
//
// An artifact belongs to its workspace, but people look for it from where
// they are: the conversation that made it, the palette, another workspace.

/** An artifact with the workspace that holds it. */
export type FoundArtifact = { artifact: Artifact; collectionId: string; workspace: string };
/** A conversation, by its session and its conversation id (either may be ''). */
export type ConversationRef = { sessionId: string; conversationId: string };

/** Whether a conversation made the artifact or any of its versions. */
export function fromConversation(artifact: Pick<Artifact, 'originSessionId' | 'originConversationId' | 'versions'>, conversation: ConversationRef): boolean {
  const { sessionId, conversationId } = conversation;
  if (!sessionId && !conversationId) return false;
  const same = (session = '', id = '') => (!!conversationId && id === conversationId) || (!!sessionId && session === sessionId);
  return same(artifact.originSessionId, artifact.originConversationId)
    || artifact.versions.some(v => same(v.source?.sessionId, v.source?.conversationId) || (!!sessionId && v.author?.sessionId === sessionId));
}

const changedAt = (found: FoundArtifact) => Number(found.artifact.updatedAtUnixMillis || 0n);

/** Artifacts by workspace for "All workspaces": the workspace changed last first, each newest first. */
export function groupByWorkspace(found: readonly FoundArtifact[]): { collectionId: string; workspace: string; items: FoundArtifact[] }[] {
  const groups = new Map<string, { collectionId: string; workspace: string; items: FoundArtifact[] }>();
  for (const item of [...found].sort((a, b) => changedAt(b) - changedAt(a))) {
    let group = groups.get(item.collectionId);
    if (!group) groups.set(item.collectionId, group = { collectionId: item.collectionId, workspace: item.workspace, items: [] });
    group.items.push(item);
  }
  return [...groups.values()];
}

/** One artifact as the palette lists it: "Plan · v3 · Beta release". */
export type ArtifactPaletteEntry = { key: string; title: string; detail: string; extra: string; collectionId: string; artifactId: string; here: boolean; icon: string };
export function artifactPaletteEntries(found: readonly FoundArtifact[], currentWorkspace: string): ArtifactPaletteEntry[] {
  return [...found].sort((a, b) => changedAt(b) - changedAt(a)).map(({ artifact, collectionId, workspace }) => {
    const kind = asKind(artifact.kind), version = artifact.versions.at(-1)?.number ?? 1;
    const here = collectionId === currentWorkspace;
    return {
      key: `artifact:${collectionId}:${artifact.id}`, title: artifact.title || 'Untitled artifact',
      detail: `${kindLabel[kind]} · v${version} · ${here ? 'This workspace' : workspace}`,
      // The workspace's name finds its artifacts ("beta plan"); a tracked file finds its artifact.
      extra: [workspace, kindLabel[kind], artifact.trackedPath].filter(Boolean).join(' '),
      collectionId, artifactId: artifact.id, here, icon: kindIcon[kind],
    };
  });
}

/** The chat header's chip: this conversation's artifacts, else the workspace's; nothing when there are none. */
export function artifactChip(artifacts: readonly Artifact[], conversation: ConversationRef): { count: number; mine: boolean; label: string; title: string } | undefined {
  if (!artifacts.length) return undefined;
  const mine = artifacts.filter(a => fromConversation(a, conversation)).length;
  const noun = (n: number) => `${n} artifact${n === 1 ? '' : 's'}`;
  const title = mine ? `${noun(mine)} from this conversation${artifacts.length > mine ? ` · ${artifacts.length} in this workspace` : ''}` : `${noun(artifacts.length)} in this workspace`;
  return { count: mine || artifacts.length, mine: mine > 0, label: noun(mine || artifacts.length), title };
}
