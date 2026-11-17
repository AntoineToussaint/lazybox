import { providerData } from '@lazybox/client-core';
import type { AgentEvent, BrowserAction, BrowserBox, BrowserState } from '@lazybox/protocol';

/**
 * The agent browser (BROWSER.md) as the window shows it: the badge a
 * conversation wears while its agent browses, the action log, the requests
 * its browsing raises, the tool calls in the transcript, and the geometry
 * between the page's CSS pixels and the canvas it is drawn on. Pure.
 */

export type BrowserTone = 'live' | 'person' | 'waiting' | 'stopped' | 'failed' | 'closed';
export type BrowserBadge = { label: string; tone: BrowserTone; detail?: string };

/** Whether a conversation's browser is in use now: opening, open, or held by the person. */
export const browsing = (state?: Pick<BrowserState, 'status'>) => !!state && (state.status === 'open' || state.status === 'starting');

/** The badge for a browser's state; undefined when it has nothing to say (no browser, or closed). */
export function browserBadge(state: Pick<BrowserState, 'status' | 'controller' | 'waiting' | 'agentName' | 'problem'> | undefined, fallbackAgent = 'The agent'): BrowserBadge | undefined {
  if (!state) return undefined;
  const agent = state.agentName || fallbackAgent;
  switch (state.status) {
    case 'starting': return { label: `${agent} is opening the browser`, tone: 'live' };
    case 'open':
      if (state.controller === 'person') return { label: 'You’re browsing', tone: 'person', detail: `${agent} is paused until you give the browser back` };
      if (state.waiting) return { label: 'Waiting for you', tone: 'waiting', detail: `${agent}’s browsing waits for your answer in the conversation` };
      return { label: `${agent} is browsing`, tone: 'live' };
    case 'stopped': return { label: 'Browser stopped', tone: 'stopped', detail: state.problem || `You stopped ${agent}’s browser` };
    case 'failed': return { label: 'Browser failed', tone: 'failed', detail: state.problem || 'The browser stopped unexpectedly' };
  }
  return undefined;
}

/** A page's address as the header shows it: its host (with port), whether it is secure, and the rest. */
export function pageOrigin(url: string): { host: string; secure: boolean; local: boolean; path: string } | undefined {
  try {
    const u = new URL(url);
    if (u.protocol !== 'http:' && u.protocol !== 'https:') return undefined;
    const local = u.hostname === 'localhost' || u.hostname === '127.0.0.1' || u.hostname === '[::1]';
    return { host: u.host, secure: u.protocol === 'https:', local, path: `${u.pathname === '/' ? '' : u.pathname}${u.search}` };
  } catch { return undefined; }
}

// ───────────── The action log ─────────────

export const ACTION_LIMIT = 200;
/** Applies one action (new, or the same seq updated: running → done) to a log ordered by seq. */
export function applyAction(log: readonly BrowserAction[], action: BrowserAction): BrowserAction[] {
  const at = log.findIndex(a => a.seq === action.seq);
  if (at >= 0) { const next = log.slice(); next[at] = action; return next; }
  const next = [...log, action];
  if (next.length > 1 && next[next.length - 2].seq > action.seq) next.sort((a, b) => Number(a.seq - b.seq));
  return next.length > ACTION_LIMIT ? next.slice(next.length - ACTION_LIMIT) : next;
}
/** The log after a stream's first event: its history replaces what was known, later actions update it. */
export function mergeHistory(history: readonly BrowserAction[]): BrowserAction[] {
  let log: BrowserAction[] = [];
  for (const action of history) log = applyAction(log, action);
  return log;
}

export const hasBox = (box?: BrowserBox): box is BrowserBox => !!box && box.width > 0 && box.height > 0;

/** The action whose place the overlay shows: the latest with a box, while it is recent. */
export function pointedAction(log: readonly BrowserAction[]): BrowserAction | undefined {
  for (let i = log.length - 1; i >= 0; i--) if (hasBox(log[i].box)) return log[i];
  return undefined;
}

const ACTION_ICONS: Record<string, string> = {
  open: 'globe', navigate: 'globe', snapshot: 'eye', click: 'target', type: 'keyboard', select: 'checklist', press: 'keyboard',
  scroll: 'chevron', screenshot: 'image', back: 'back', forward: 'chevronRight', wait: 'pending', close: 'close',
  blocked: 'shield', popup: 'float', download: 'download', take_over: 'activity', give_back: 'activity', stop: 'stop', site: 'lock',
};
export const actionIcon = (kind: string) => ACTION_ICONS[kind] || 'activity';

/** "now", "12s", "4m", "2h", "3d": how long ago, for a compact log. */
export function ago(at: number, now: number): string {
  const s = Math.max(0, Math.round((now - at) / 1000));
  if (s < 5) return 'now';
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h`;
  return `${Math.floor(s / 86400)}d`;
}

// ───────────── Geometry ─────────────

export type Rect = { x: number; y: number; width: number; height: number };
/** Where content of this size is drawn inside a box of that size: as large as fits, centered (letterboxed). */
export function fitRect(boxWidth: number, boxHeight: number, contentWidth: number, contentHeight: number): Rect {
  if (boxWidth <= 0 || boxHeight <= 0 || contentWidth <= 0 || contentHeight <= 0) return { x: 0, y: 0, width: 0, height: 0 };
  const scale = Math.min(boxWidth / contentWidth, boxHeight / contentHeight);
  const width = contentWidth * scale, height = contentHeight * scale;
  return { x: (boxWidth - width) / 2, y: (boxHeight - height) / 2, width, height };
}
/** A point on the drawn page (relative to its box) in the page's CSS pixels; undefined outside the page. */
export function toPage(x: number, y: number, drawn: Rect, viewport: { width: number; height: number }): { x: number; y: number } | undefined {
  if (drawn.width <= 0 || drawn.height <= 0) return undefined;
  const px = (x - drawn.x) / drawn.width, py = (y - drawn.y) / drawn.height;
  if (px < 0 || py < 0 || px > 1 || py > 1) return undefined;
  return { x: Math.round(px * viewport.width * 10) / 10, y: Math.round(py * viewport.height * 10) / 10 };
}
/** A box in page CSS pixels, as drawn (relative to the drawing's box). */
export function toDrawn(box: BrowserBox, drawn: Rect, viewport: { width: number; height: number }): Rect {
  const sx = viewport.width > 0 ? drawn.width / viewport.width : 0, sy = viewport.height > 0 ? drawn.height / viewport.height : 0;
  return { x: drawn.x + box.x * sx, y: drawn.y + box.y * sy, width: box.width * sx, height: box.height * sy };
}

/** A key event's modifiers as the Runner takes them: Alt=1, Control=2, Meta=4, Shift=8. */
export function modifiersOf(event: { altKey: boolean; ctrlKey: boolean; metaKey: boolean; shiftKey: boolean }): number {
  return (event.altKey ? 1 : 0) | (event.ctrlKey ? 2 : 0) | (event.metaKey ? 4 : 0) | (event.shiftKey ? 8 : 0);
}
export const mouseButton = (button: number): 'left' | 'middle' | 'right' => button === 1 ? 'middle' : button === 2 ? 'right' : 'left';

// ───────────── Requests ─────────────

export type BrowserRequestKind = 'site' | 'download' | 'sign_in';
export type BrowserRequestView = { id: string; kind: BrowserRequestKind; url: string; origin: string; host: string; reason: string; fileName: string; permission: string; agentName: string };
const text = (value: unknown) => typeof value === 'string' ? value : '';
/** A pending "browser_request" entry, its provider data read. */
export function browserRequestOf(request: AgentEvent): BrowserRequestView {
  const data = providerData(request.providerDataJson);
  const kind = (['site', 'download', 'sign_in'] as const).find(k => k === data.kind) || 'site';
  const url = text(data.url), origin = text(data.origin);
  const host = pageOrigin(origin || url)?.host || origin || url;
  return { id: text(data.id) || request.requestId, kind, url, origin, host, reason: text(data.reason), fileName: text(data.fileName), permission: text(data.permission), agentName: text(data.agentName) };
}
/** The request in one line, for the inbox and the status: "Open example.com". */
export function browserRequestSummary(request: AgentEvent): string {
  const view = browserRequestOf(request);
  if (view.kind === 'download') return `Download ${view.fileName || 'a file'}${view.host ? ` from ${view.host}` : ''}`;
  if (view.kind === 'sign_in') return `Sign in on ${view.host || 'a site'}`;
  return `Open ${view.host || 'a site'}`;
}
/** The card's heading. */
export function browserRequestHeading(view: BrowserRequestView, agent: string): string {
  if (view.kind === 'download') return `${agent} wants to download ${view.fileName || 'a file'}`;
  if (view.kind === 'sign_in') return `${agent} wants to sign in on ${view.host || 'this site'}`;
  return `${agent} wants to open ${view.host || 'a site'}`;
}
export type BrowserDecision = 'once' | 'conversation' | 'workspace' | 'deny';
export type DecisionRow = { decision: BrowserDecision; label: string; detail: string; key: string; tone: 'allow' | 'always' | 'deny' };
/** The choices a request offers, with their keys. */
export function decisionRows(view: BrowserRequestView, agent: string): DecisionRow[] {
  const deny: DecisionRow = { decision: 'deny', label: 'Deny…', detail: `Tell ${agent} why`, key: 'n', tone: 'deny' };
  if (view.kind === 'download') return [{ decision: 'once', label: 'Allow this download', detail: 'Saved where the agent can read it', key: 'y', tone: 'allow' }, deny];
  if (view.kind === 'sign_in') return [{ decision: 'once', label: 'Allow this sign-in', detail: 'Submits the form once; the password stays in the page', key: 'y', tone: 'allow' }, deny];
  const host = view.host || 'this site';
  return [
    { decision: 'once', label: 'Allow once', detail: `This visit to ${host}`, key: 'y', tone: 'allow' },
    { decision: 'conversation', label: 'Allow for this conversation', detail: `${host}, until this conversation ends`, key: 'c', tone: 'always' },
    { decision: 'workspace', label: 'Always allow in this workspace', detail: `Every conversation here; remove it in Settings → Browser`, key: 'a', tone: 'always' },
    deny,
  ];
}

// ───────────── Tool calls in the transcript ─────────────

/** The browser verb of a tool call's name ("mcp__lazybox_browser__browser_click", "browser_click" → "click"), or undefined. */
export function browserVerb(name: string): string | undefined {
  const match = /^(?:mcp__lazybox_browser__)?browser_([a-z_]+)$/.exec(name);
  return match?.[1];
}
const clip = (value: string, limit: number) => value.length > limit ? value.slice(0, limit - 1) + '…' : value;
const hostOf = (url: string) => { const o = pageOrigin(url); return o ? o.host + (o.path && o.path.length < 40 ? o.path : '') : url; };
/** What an element is called in a line: the agent's own description, else its ref. */
const element = (input: Record<string, any>) => typeof input.element === 'string' && input.element.trim() ? `“${clip(input.element.trim(), 60)}”` : typeof input.ref === 'string' ? input.ref : 'an element';
/** A browser call as a person would say it: "Opened example.com", "Clicked “Sign in”". */
export function describeBrowserCall(verb: string, input: Record<string, any>, running: boolean): { verb: string; target: string } {
  const t = (past: string, present: string) => running ? present : past;
  switch (verb) {
    case 'open': return { verb: t('Opened', 'Opening'), target: typeof input.url === 'string' && input.url ? hostOf(input.url) : 'the browser' };
    case 'navigate': return { verb: t('Went to', 'Going to'), target: typeof input.url === 'string' ? hostOf(input.url) : '' };
    case 'snapshot': return { verb: t('Read the page', 'Reading the page'), target: '' };
    case 'click': return { verb: t('Clicked', 'Clicking'), target: element(input) };
    case 'type': {
      const typed = typeof input.text === 'string' ? `“${clip(input.text, 24)}”` : 'text';
      return { verb: t('Typed', 'Typing'), target: `${typed} into ${element(input)}${input.submit ? ' and submitted' : ''}` };
    }
    case 'select': return { verb: t('Chose', 'Choosing'), target: `${Array.isArray(input.values) ? input.values.map(String).map(v => `“${clip(v, 24)}”`).join(', ') : 'an option'} in ${element(input)}` };
    case 'press': return { verb: t('Pressed', 'Pressing'), target: typeof input.key === 'string' ? input.key : 'a key' };
    case 'scroll': return { verb: t('Scrolled', 'Scrolling'), target: [typeof input.direction === 'string' ? input.direction : 'down', typeof input.ref === 'string' ? `to ${input.ref}` : ''].filter(Boolean).join(' ') };
    case 'screenshot': return { verb: t('Took a screenshot', 'Taking a screenshot'), target: input.fullPage ? 'of the whole page' : '' };
    case 'back': return { verb: t('Went back', 'Going back'), target: '' };
    case 'forward': return { verb: t('Went forward', 'Going forward'), target: '' };
    case 'wait_for': {
      const what = typeof input.text === 'string' ? `“${clip(input.text, 40)}”` : typeof input.textGone === 'string' ? `“${clip(input.textGone, 40)}” to go` : typeof input.ref === 'string' ? input.ref : 'the page';
      return { verb: t('Waited for', 'Waiting for'), target: what };
    }
    case 'close': return { verb: t('Closed the browser', 'Closing the browser'), target: '' };
  }
  return { verb: t('Used the browser', 'Using the browser'), target: verb.replace(/_/g, ' ') };
}

// ───────────── Settings ─────────────

/** A site for the allow-list, normalized ("https://Docs.Example.com/x" → "docs.example.com"), or an error in words. */
export function normalizeSite(input: string): { site: string } | { error: string } {
  let value = input.trim().toLowerCase();
  if (!value) return { error: 'Type a site, like docs.example.com or *.example.com.' };
  if (/^[a-z][a-z0-9+.-]*:\/\//.test(value)) {
    try { const url = new URL(value); if (url.protocol !== 'http:' && url.protocol !== 'https:') return { error: 'Only web sites (http and https) can be allowed.' }; value = url.host; }
    catch { return { error: 'That is not a site address.' }; }
  }
  value = value.replace(/\/.*$/, '').replace(/\.$/, '');
  const wildcard = value.startsWith('*.');
  const host = wildcard ? value.slice(2) : value;
  const [name, port] = host.split(':');
  if (port !== undefined && !/^\d{1,5}$/.test(port)) return { error: 'A port is a number, like example.com:8080.' };
  const label = /^(?!-)[a-z0-9-]{1,63}(?<!-)$/;
  if (!name || !name.split('.').every(part => label.test(part))) return { error: `“${input.trim()}” is not a site name. Use a host like docs.example.com, or *.example.com for all its subdomains.` };
  if (wildcard && !name.includes('.')) return { error: 'A wildcard needs a domain with a dot, like *.example.com.' };
  return { site: value };
}

/** Bytes in words: 0 B, 12 KB, 3.4 MB, 1.2 GB. */
export function sizeText(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let value = bytes, unit = 0;
  while (value >= 1024 && unit < units.length - 1) { value /= 1024; unit++; }
  return `${unit === 0 ? value : value < 10 ? value.toFixed(1).replace(/\.0$/, '') : Math.round(value)} ${units[unit]}`;
}
