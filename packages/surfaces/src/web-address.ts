/**
 * The Web viewer's addresses and history, without React or a host: what the
 * address bar accepts, where a search goes, and how a page the viewer reports
 * it loaded becomes history. Shared by the Web panel and the workspace Preview.
 */

export type SearchEngine = { id: string; name: string; template: string };
/** `%s` is replaced by the encoded search terms. */
export const searchEngines: readonly SearchEngine[] = [
  { id: 'google', name: 'Google', template: 'https://www.google.com/search?q=%s' },
  { id: 'duckduckgo', name: 'DuckDuckGo', template: 'https://duckduckgo.com/?q=%s' },
  { id: 'bing', name: 'Bing', template: 'https://www.bing.com/search?q=%s' },
  { id: 'kagi', name: 'Kagi', template: 'https://kagi.com/search?q=%s' },
  { id: 'startpage', name: 'Startpage', template: 'https://www.startpage.com/do/search?q=%s' },
];
export const defaultSearchEngine = searchEngines[0];
export const searchEngine = (id: string | null | undefined) => searchEngines.find(engine => engine.id === id) || defaultSearchEngine;

const scheme = /^[a-z][a-z0-9+.-]*:/i;
const loopbackHost = (host: string) => host === 'localhost' || host.endsWith('.localhost') || /^127(\.\d{1,3}){3}$/.test(host) || host === '[::1]';

/** Only web pages: never javascript:, file:, data: or an app's own scheme. A
 * URL carrying a user name or password is refused: it is how a link dresses
 * one site up as another (`https://bank.com@evil.test`). */
export function webPage(text: string): string {
  try {
    const url = new URL(text.trim());
    return (url.protocol === 'http:' || url.protocol === 'https:') && !url.username && !url.password && !!url.hostname ? url.href : '';
  } catch { return ''; }
}

export type Resolved = { url: string; search?: string } | { error: string };
/**
 * What the address bar was given: a web address (scheme optional), or terms
 * to search for. `localhost:3000` and `127.0.0.1:8080/x` are addresses on
 * plain http; a bare word or a sentence is a search; a non-web scheme is
 * refused rather than searched, so `javascript:` never goes anywhere.
 */
export function resolveAddress(text: string, engine: SearchEngine = defaultSearchEngine): Resolved {
  const trimmed = text.trim();
  if (!trimmed) return { error: 'Enter a web address or something to search for.' };
  if (/^https?:\/\//i.test(trimmed)) { const url = webPage(trimmed); return url ? { url } : { error: 'That is not a web address Lazybox can open.' }; }
  const hostLike = /^(localhost|\[[0-9a-f:]+\]|\d{1,3}(\.\d{1,3}){3}|([a-z0-9-]+\.)+[a-z][a-z0-9-]*)(:\d{1,5})?([/?#].*)?$/i;
  // `localhost:3000` parses as the scheme "localhost": an address, not a scheme.
  if (scheme.test(trimmed) && !hostLike.test(trimmed)) {
    const name = trimmed.slice(0, trimmed.indexOf(':')).toLowerCase();
    return { error: `Lazybox shows web pages (http and https), not ${name}: links.` };
  }
  if (!/\s/.test(trimmed) && hostLike.test(trimmed)) {
    const host = trimmed.replace(/[:/?#].*$/, '').toLowerCase();
    const url = webPage(`${loopbackHost(host) ? 'http' : 'https'}://${trimmed}`);
    if (url) return { url };
  }
  return { url: engine.template.replace('%s', encodeURIComponent(trimmed)), search: trimmed };
}

/** Where a web page is, as the address bar's origin chip shows it. */
export function originOf(text: string): { host: string; secure: boolean; local: boolean } | undefined {
  try {
    const url = new URL(text);
    if (url.protocol !== 'http:' && url.protocol !== 'https:') return undefined;
    const local = loopbackHost(url.hostname);
    return { host: url.host, secure: url.protocol === 'https:' || local, local };
  } catch { return undefined; }
}
export const isLoopback = (text: string) => !!originOf(text)?.local;

export type WebEntry = { url: string; title?: string };
export type WebHistory = { entries: WebEntry[]; index: number; /** The entry a requested navigation will land on. */ expect?: number };
export const emptyHistory = (): WebHistory => ({ entries: [], index: -1 });
const MAX_ENTRIES = 50;
export const current = (history: WebHistory): WebEntry | undefined => history.entries[history.index];

/** Opening an address the person asked for: a new entry after the current one. */
export function visit(history: WebHistory, url: string): WebHistory {
  const kept = history.entries.slice(0, history.index + 1);
  const entries = [...kept, { url }].slice(-MAX_ENTRIES);
  return { entries, index: entries.length - 1, expect: entries.length - 1 };
}
/** Back or forward: the entry `by` steps away, which the viewer will load. */
export function step(history: WebHistory, by: number): WebHistory {
  const index = Math.max(0, Math.min(history.entries.length - 1, history.index + by));
  return index === history.index ? history : { ...history, index, expect: index };
}
/**
 * The viewer finished loading `url`. A load Lazybox asked for lands on its
 * entry (taking the address it was redirected to); a load the page started
 * itself (a link clicked inside it) is a new entry, as in any browser; the
 * same address again (a reload) changes nothing.
 */
export function arrive(history: WebHistory, url: string): WebHistory {
  if (history.expect !== undefined && history.entries[history.expect]) {
    const entries = history.entries.map((entry, i) => i === history.expect ? { ...entry, url } : entry);
    return { entries, index: history.expect };
  }
  if (current(history)?.url === url) return history.expect === undefined ? history : { ...history, expect: undefined };
  const entries = [...history.entries.slice(0, history.index + 1), { url }].slice(-MAX_ENTRIES);
  return { entries, index: entries.length - 1 };
}
export function titled(history: WebHistory, title: string): WebHistory {
  const entry = current(history);
  if (!entry || entry.title === title) return history;
  return { ...history, entries: history.entries.map((value, i) => i === history.index ? { ...value, title: title.slice(0, 300) } : value) };
}
/** Saved history, read back defensively: anything that is not a web page is dropped. */
export function parseHistory(text: string | null): WebHistory {
  try {
    const value = JSON.parse(text || 'null');
    if (!value || !Array.isArray(value.entries)) return emptyHistory();
    const entries: WebEntry[] = value.entries.slice(-MAX_ENTRIES).flatMap((entry: unknown) => {
      if (!entry || typeof entry !== 'object') return [];
      const { url, title } = entry as Record<string, unknown>;
      const page = typeof url === 'string' ? webPage(url) : '';
      return page ? [{ url: page, ...(typeof title === 'string' && title ? { title: title.slice(0, 300) } : {}) }] : [];
    });
    const index = Number.isInteger(value.index) ? Math.max(0, Math.min(entries.length - 1, value.index)) : entries.length - 1;
    return { entries, index: entries.length ? index : -1 };
  } catch { return emptyHistory(); }
}

export const zoomLevels = [0.5, 0.67, 0.75, 0.8, 0.9, 1, 1.1, 1.25, 1.5, 1.75, 2] as const;
export function nextZoom(zoom: number, by: -1 | 1): number {
  const index = zoomLevels.findIndex(level => level >= zoom - 0.001);
  const at = index === -1 ? zoomLevels.length - 1 : index;
  const next = by > 0 ? (zoomLevels[at] > zoom + 0.001 ? at : at + 1) : at - 1;
  return zoomLevels[Math.max(0, Math.min(zoomLevels.length - 1, next))];
}
export const validZoom = (value: unknown): number => typeof value === 'number' && value >= 0.5 && value <= 2 ? value : 1;

/** A short name for a page, for the panel's tab: its title, else its host. */
export function pageLabel(entry: WebEntry | undefined): string {
  if (!entry) return '';
  if (entry.title?.trim()) return entry.title.trim();
  return originOf(entry.url)?.host || entry.url;
}

export type LinkTarget = 'browser' | 'viewer';
/** What a hover over a web link says it does, so ⌥-click is discoverable where a Web panel can open it. */
export function linkHint(url: string, target: LinkTarget, viewer: boolean, alt = 'Alt'): string {
  if (!viewer || !webPage(url)) return url;
  return `${url}\n${alt}-click to open ${target === 'viewer' ? 'in your browser' : 'in Lazybox'} · right-click for more`;
}
