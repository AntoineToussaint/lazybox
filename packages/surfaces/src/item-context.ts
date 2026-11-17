/**
 * Starting work from an issue (PROJECTS_AND_INTEGRATIONS.md), pure: whether a
 * paste names an issue, the workspace's name and branch from it, the first
 * message, which mode a start from an issue may use, and the quoted item a
 * first message carries, recognised so the conversation shows it as a chip.
 * The Runner writes that quoted block (workitems.Quote); this only reads it.
 */

/** The fields of an item this module reads (the protocol's Item). */
export type ItemLike = {
  ref?: { provider: string; key: string };
  kind: string;
  title: string;
  url?: string;
  branchHint?: string;
  state?: { category: string; label: string };
};

const GITHUB_URL = /^https?:\/\/(?:www\.)?github\.com\/([A-Za-z0-9_.-]+)\/([A-Za-z0-9_.-]+)\/(issues|pull)\/(\d+)(?:[/?#][^\s]*)?$/i;
const GITHUB_KEY = /^([A-Za-z0-9_.-]+)\/([A-Za-z0-9_.-]+)#(\d+)$/;

/**
 * The item a pasted text names: a GitHub issue or pull request link, or
 * owner/name#123, alone on the clipboard (surrounding words are the person's
 * message, not a reference). Undefined otherwise.
 */
export function itemReference(text: string): string | undefined {
  const value = text.trim();
  if (!value || value.length > 2048 || /\s/.test(value)) return undefined;
  const url = GITHUB_URL.exec(value);
  if (url && Number(url[4]) > 0) return `${url[1]}/${url[2]}#${Number(url[4])}`;
  const key = GITHUB_KEY.exec(value);
  if (key && Number(key[3]) > 0) return `${key[1]}/${key[2]}#${Number(key[3])}`;
  return undefined;
}

/** "acme/api#812" → "acme/api"; "" for a key that names no repository. */
export const itemRepository = (key: string) => GITHUB_KEY.exec(key)?.slice(1, 3).join('/') || '';
/** "acme/api#812" → "812". */
export const itemNumber = (key: string) => key.includes('#') ? key.slice(key.lastIndexOf('#') + 1) : key;

/** The provider in words, for a chip. */
export const providerName = (provider: string) => provider === 'github' || !provider ? 'GitHub' : provider[0].toUpperCase() + provider.slice(1);

/** An item's kind in words: "issue", "pull request". */
export const itemNoun = (kind: string) => kind === 'pull_request' ? 'pull request' : kind ? kind.replace(/_/g, ' ') : 'item';

const FOLD: Record<string, string> = { à: 'a', á: 'a', â: 'a', ä: 'a', ã: 'a', å: 'a', ç: 'c', è: 'e', é: 'e', ê: 'e', ë: 'e', ì: 'i', í: 'i', î: 'i', ï: 'i', ñ: 'n', ò: 'o', ó: 'o', ô: 'o', ö: 'o', õ: 'o', ù: 'u', ú: 'u', û: 'u', ü: 'u', ý: 'y', ÿ: 'y', ß: 'ss' };

/**
 * The branch of a workspace started from an item: the provider's suggestion,
 * else "<number>-<slug of the title>" ("812-fix-refund-rounding"), with "-2",
 * "-3"… when a workspace already holds it. The Runner's default is the same
 * (workitems.Branch), so the CLI and the sheet agree.
 */
export function itemBranch(item: ItemLike, taken: ReadonlySet<string> = new Set()): string {
  let base = (item.branchHint || '').trim();
  if (!base) {
    const number = itemNumber(item.ref?.key || '').toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '');
    let slug = [...item.title.toLowerCase()].map(c => FOLD[c] ?? c).join('').replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '');
    if (slug.length > 40) {
      slug = slug.slice(0, 40);
      const cut = slug.lastIndexOf('-');
      if (cut > 20) slug = slug.slice(0, cut);
      slug = slug.replace(/^-+|-+$/g, '');
    }
    base = [number, slug].filter(Boolean).join('-') || 'work-item';
  }
  let branch = base;
  for (let n = 2; taken.has(branch); n++) branch = `${base}-${n}`;
  return branch;
}

/** The workspace's name: the item's title, cut at a word to fit the sidebar's 128 bytes. */
export function itemTaskName(item: ItemLike): string {
  const title = item.title.replace(/\s+/g, ' ').trim();
  if (!title) return item.ref?.key || 'Work item';
  if (title.length <= 120) return title;
  const cut = title.slice(0, 118);
  return cut.slice(0, Math.max(cut.lastIndexOf(' '), 60)).trim() + '…';
}

/**
 * The first message of a task started from an item: the person's words (or a
 * plain request to work on it), then the item's quoted context, exactly as
 * the Runner wrote it. The CLI composes the same (`lazybox new --from`).
 */
export function itemMessage(prompt: string, item: ItemLike, context: string): string {
  const key = item.ref?.key || '';
  // An item that could not be read is named by its key alone.
  const words = prompt.trim() || (item.title ? `Work on ${itemNoun(item.kind)} ${key}: ${item.title.replace(/\.+$/, '')}.` : `Work on ${key}.`);
  return context ? `${words}\n\n${context}` : words;
}

/**
 * The mode a start from an item uses. Item text is untrusted input to the
 * agent, so it never starts with full access unless the person picked YOLO in
 * the sheet: the Runner default, when that is YOLO, becomes Ask.
 */
export function itemSafeMode<M extends string>(mode: M | 'default' | 'ask', runnerPermission: string): { mode: M | 'default' | 'ask'; guarded: boolean } {
  if (mode === 'default' && runnerPermission === 'yolo') return { mode: 'ask', guarded: true };
  return { mode, guarded: false };
}

/** The quoted item a message carries, as the Runner frames it. */
export type QuotedItem = { noun: string; key: string; url: string; quoted: string; text: string };
export type MessageParts = { before: string; item: QuotedItem; after: string };

const OPEN = '[Lazybox] External content: ';
const CLOSE = '[Lazybox] End of external content from ';

/**
 * Splits a message into the person's words and the quoted item, so the
 * conversation shows the item as a chip (expandable to exactly what was
 * sent). Undefined when the message carries none, or when the framing is not
 * whole: then the message shows as it is.
 */
export function splitItemContext(text: string): MessageParts | undefined {
  const lines = text.split('\n');
  const start = lines.findIndex(line => line.startsWith(OPEN));
  if (start < 0) return undefined;
  const head = /^(.+?) (\S+#\S+|[A-Z][A-Z0-9]*-\d+|\S+), (https:\/\/\S+)$/.exec(lines[start].slice(OPEN.length));
  if (!head) return undefined;
  const [, noun, key, url] = head;
  const end = lines.findIndex((line, i) => i > start && line === CLOSE + key);
  if (end < 0) return undefined;
  // Everything between the frame is quoted, line by line, after the explanation.
  const body = lines.slice(start + 1, end);
  const firstQuoted = body.findIndex(line => line === '>' || line.startsWith('> '));
  if (firstQuoted < 0 || body.slice(firstQuoted).some(line => line !== '>' && !line.startsWith('> '))) return undefined;
  const quoted = body.slice(firstQuoted).map(line => line === '>' ? '' : line.slice(2)).join('\n');
  return {
    before: lines.slice(0, start).join('\n').replace(/\s+$/, ''),
    item: { noun, key, url, quoted, text: lines.slice(start, end + 1).join('\n') },
    after: lines.slice(end + 1).join('\n').replace(/^\s+/, ''),
  };
}

/** The title the quoted item opens with ("**Fix refund rounding** (acme/api#812, open)"). */
export function quotedTitle(quoted: string): string {
  return /^\*\*(.+)\*\* \(/.exec(quoted.split('\n')[0] || '')?.[1] || '';
}

/** A dot's tone for an item's state category. */
export function stateTone(category: string | undefined): 'open' | 'progress' | 'done' | 'closed' | 'unknown' {
  switch (category) {
    case 'open': return 'open';
    case 'started': case 'review': return 'progress';
    case 'done': case 'merged': return 'done';
    case 'closed': return 'closed';
    default: return 'unknown';
  }
}

/** "3m ago", "5h ago", "2d ago", or the date, from a time in milliseconds. */
export function updatedAgo(at: number, now = Date.now()): string {
  if (!at) return '';
  const minutes = Math.max(0, Math.floor((now - at) / 60000));
  if (minutes < 1) return 'just now';
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.round(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.round(hours / 24);
  if (days < 30) return `${days}d ago`;
  return new Date(at).toISOString().slice(0, 10);
}

/** The picker's scopes, in the order Tab cycles them. */
export const ITEM_SCOPES = [
  { id: 'mine', label: 'Assigned to you' },
  { id: 'recent', label: 'Involving you' },
  { id: 'all', label: 'All of GitHub' },
] as const;
export type ItemScope = (typeof ITEM_SCOPES)[number]['id'];
export const nextScope = (scope: ItemScope, step = 1): ItemScope => ITEM_SCOPES[(ITEM_SCOPES.findIndex(s => s.id === scope) + step + ITEM_SCOPES.length) % ITEM_SCOPES.length].id;
