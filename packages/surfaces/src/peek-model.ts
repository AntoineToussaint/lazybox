/**
 * Peek, the pure half: what can be peeked, how an element says what it is,
 * and moving through a list of them. Peek.tsx renders the overlay.
 *
 * Any row marks itself peekable with `data-peek` (see `peekProps`); Alt-click
 * on it, Space while it has focus (like Quick Look), or "Peek" in its context
 * menu opens the live item over the current page. Arrow keys then step through
 * the peekable rows around it, in the order they are shown.
 */
export type PeekItem =
  /** `runner`: which of the window's Runners it is on, when the window shows several. */
  | { kind: 'conversation'; sessionId: string; workspaceId?: string; title?: string; runner?: string }
  | { kind: 'command'; commandId: string; title?: string }
  | { kind: 'process'; processId: string; title?: string }
  | { kind: 'artifact'; workspaceId: string; artifactId: string; title?: string }
  | { kind: 'mission-task'; taskId: string; title?: string }
  | { kind: 'file'; workspaceId: string; path: string; line?: number }
  /** Shown by the conversation's own sub-agent panel, floating. */
  | { kind: 'subagent'; agentId: string; title?: string };
export type PeekKind = PeekItem['kind'];

const required: Record<PeekKind, string[]> = {
  conversation: ['sessionId'], command: ['commandId'], process: ['processId'], artifact: ['workspaceId', 'artifactId'],
  'mission-task': ['taskId'], file: ['workspaceId', 'path'], subagent: ['agentId'],
};
const optional = ['workspaceId', 'title', 'runner'];
const LIMIT = 4096;

/** The attribute a peekable element carries. */
export const peekProps = (item: PeekItem) => ({ 'data-peek': JSON.stringify(item) });

/** An element's `data-peek`, checked: anything malformed is not peekable. */
export function parsePeek(text: string | null | undefined): PeekItem | undefined {
  if (!text || text.length > LIMIT) return;
  let value: unknown;
  try { value = JSON.parse(text); } catch { return; }
  if (!value || typeof value !== 'object' || Array.isArray(value)) return;
  const record = value as Record<string, unknown>;
  const kind = record.kind as PeekKind;
  if (!Object.hasOwn(required, kind)) return;
  const out: Record<string, unknown> = { kind };
  for (const key of required[kind]) {
    if (typeof record[key] !== 'string' || !record[key]) return;
    out[key] = record[key];
  }
  for (const key of optional) if (typeof record[key] === 'string' && record[key]) out[key] = record[key];
  if (kind === 'file' && Number.isSafeInteger(record.line) && (record.line as number) > 0) out.line = record.line;
  return out as PeekItem;
}

/** One identity per item, whatever its title says. */
export function peekKey(item: PeekItem): string {
  switch (item.kind) {
    case 'conversation': return item.runner ? `conversation:${item.runner}:${item.sessionId}` : `conversation:${item.sessionId}`;
    case 'command': return `command:${item.commandId}`;
    case 'process': return `process:${item.processId}`;
    case 'artifact': return `artifact:${item.workspaceId}:${item.artifactId}`;
    case 'mission-task': return `mission-task:${item.taskId}`;
    case 'file': return `file:${item.workspaceId}:${item.path}`;
    case 'subagent': return `subagent:${item.agentId}`;
  }
}

/** Items in the order shown, each once (a row and its nested card can both say the same thing). */
export function peekList(items: readonly (PeekItem | undefined)[]): PeekItem[] {
  const seen = new Set<string>(), out: PeekItem[] = [];
  for (const item of items) {
    if (!item || item.kind === 'subagent') continue;
    const key = peekKey(item);
    if (!seen.has(key)) { seen.add(key); out.push(item); }
  }
  return out;
}

/** The item `step` places away, or undefined at either end (no wrapping, like Quick Look). */
export function stepPeek(list: readonly PeekItem[], current: PeekItem, step: number): PeekItem | undefined {
  const index = list.findIndex(item => peekKey(item) === peekKey(current));
  if (index < 0) return;
  return list[index + step];
}

export const peekNouns: Record<PeekKind, string> = {
  conversation: 'Conversation', command: 'Command', process: 'Process', artifact: 'Artifact', 'mission-task': 'Mission task', file: 'File', subagent: 'Sub-agent',
};
export const peekIcons: Record<PeekKind, string> = {
  conversation: 'chat', command: 'output', process: 'processes', artifact: 'artifact', 'mission-task': 'checklist', file: 'file', subagent: 'subagent',
};

/** The title an item has before its live data arrives. */
export function peekTitle(item: PeekItem): string {
  if (item.kind === 'file') return item.path.split('/').at(-1) || item.path;
  return item.title || peekNouns[item.kind];
}

/** Whether a key event belongs to a field the person is typing in. */
export function typingTarget(target: { tagName?: string; isContentEditable?: boolean; type?: string } | null | undefined): boolean {
  if (!target) return false;
  if (target.isContentEditable) return true;
  const tag = (target.tagName || '').toLowerCase();
  if (tag === 'textarea' || tag === 'select') return true;
  return tag === 'input' && !['checkbox', 'radio', 'button', 'submit', 'reset', 'range', 'color', 'file'].includes((target.type || 'text').toLowerCase());
}
