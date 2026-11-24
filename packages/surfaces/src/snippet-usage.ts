import type { ViewStateStore } from '@lazybox/platform';
import { readPreference, writePreference } from './view-state';

/** How often and how lately each snippet was used (sent or inserted), by key. */
export type SnippetUsage = Readonly<Record<string, { count: number; last: number }>>;
const usageKey = 'snippet-usage:v1', legacyRecentKey = 'recent-snippets:v2', limit = 500;

/** Pure: reads saved usage; a pre-count "recent" list becomes one use each, in its order. */
export function parseSnippetUsage(raw: string | null, legacyRecent: string | null = null): SnippetUsage {
  try {
    const value = JSON.parse(raw || 'null');
    if (value && typeof value === 'object' && !Array.isArray(value)) {
      const usage: Record<string, { count: number; last: number }> = {};
      for (const [key, row] of Object.entries(value as Record<string, any>)) if (Number.isFinite(row?.count) && Number.isFinite(row?.last)) usage[key] = { count: Math.max(0, Math.floor(row.count)), last: row.last };
      return usage;
    }
  } catch { /* fall back to the legacy list */ }
  try {
    const recent = JSON.parse(legacyRecent || '[]');
    if (!Array.isArray(recent)) return {};
    return Object.fromEntries(recent.filter((k): k is string => typeof k === 'string').map((k, i) => [k, { count: 1, last: recent.length - i }]));
  } catch { return {}; }
}

/** Pure: usage after one more use of `key` at `at`; bounded to the most recently used. */
export function recordSnippetUse(usage: SnippetUsage, key: string, at: number): SnippetUsage {
  const next = { ...usage, [key]: { count: (usage[key]?.count || 0) + 1, last: Math.max(at, (usage[key]?.last || 0) + 1) } };
  const keys = Object.keys(next);
  if (keys.length <= limit) return next;
  return Object.fromEntries(keys.sort((a, b) => next[b].last - next[a].last).slice(0, limit).map(k => [k, next[k]]));
}

export type SnippetOrder = 'suggested' | 'frequent';
/**
 * Pure: the picker's order without a search. Suggested: the last-used snippet
 * first, then favorites, then by recency, then by category and key. Frequent:
 * by use count, ties by recency.
 */
export function sortSnippets<T extends { key: string; category: string }>(rows: readonly T[], usage: SnippetUsage, favorites: readonly string[], order: SnippetOrder): T[] {
  const last = Object.entries(usage).reduce<[string, number]>((best, [key, row]) => row.last > best[1] ? [key, row.last] : best, ['', -Infinity])[0];
  const byName = (a: T, b: T) => a.category.localeCompare(b.category) || a.key.localeCompare(b.key);
  const recency = (a: T, b: T) => (usage[b.key]?.last || 0) - (usage[a.key]?.last || 0);
  return [...rows].sort((a, b) => {
    if (order === 'frequent') return (usage[b.key]?.count || 0) - (usage[a.key]?.count || 0) || recency(a, b) || byName(a, b);
    if (a.key === last || b.key === last) return a.key === last ? -1 : 1;
    return Number(favorites.includes(b.key)) - Number(favorites.includes(a.key)) || recency(a, b) || byName(a, b);
  });
}

export function readSnippetUsage(store?: ViewStateStore): SnippetUsage {
  try { return parseSnippetUsage(readPreference(store, usageKey), readPreference(store, legacyRecentKey)); } catch { return {}; }
}
/** Records one use; returns the new usage. A failed save keeps the picker working. */
export function saveSnippetUse(store: ViewStateStore | undefined, key: string, at = Date.now()): SnippetUsage {
  const next = recordSnippetUse(readSnippetUsage(store), key, at);
  try { writePreference(store, usageKey, JSON.stringify(next)); } catch { /* usage is a convenience */ }
  return next;
}

type SearchableSnippet = { key: string; category: string; description: string; body: string };

const words = (text: string) => text.toLowerCase().match(/[\p{L}\p{N}]+/gu) ?? [];

/** Rank a filtered view by relevance, keeping its browse order for tied matches. */
export function searchSnippets<T extends SearchableSnippet>(rows: readonly T[], query: string): T[] {
  const text = query.trim().toLowerCase();
  if (!text) return [...rows];
  if (text.startsWith('body:')) {
    const phrase = text.slice(5).trim().replace(/\s+/g, ' ');
    return rows.filter(row => row.body.toLowerCase().replace(/\s+/g, ' ').includes(phrase));
  }
  const terms = words(text);
  const joined = terms.join('');
  const matches = (term: string, field: string[]) => field.some(word => word.startsWith(term));
  return rows.flatMap(row => {
    const key = row.key.toLowerCase();
    if (key === text) return [{ row, rank: 0 }];
    const keyWords = words(key), compactKey = keyWords.join('');
    const description = words(row.description), category = words(row.category);
    if (joined && compactKey === joined) return [{ row, rank: 1 }];
    if (key.startsWith(text) || (joined && compactKey.startsWith(joined))) return [{ row, rank: 2 }];
    if (terms.length && (terms.every(term => matches(term, keyWords)) ||
        (joined.length >= 3 && compactKey.includes(joined)))) return [{ row, rank: 3 }];
    if (terms.length && terms.every(term => matches(term, [...keyWords, ...description]))) return [{ row, rank: 4 }];
    if (terms.length && terms.every(term => matches(term, [...keyWords, ...description, ...category]))) return [{ row, rank: 5 }];
    return [];
  }).sort((a, b) => a.rank - b.rank).map(match => match.row);
}

/** Catalog actions whose instructions belong in the structured code-review brief. */
export function isReviewSnippet(key: string) {
  return key === 'review' || key === 'deepreview';
}

const commandUsageKey = 'slash-command-usage:v1';
export function readCommandUsage(store?: ViewStateStore): SnippetUsage {
  try { return parseSnippetUsage(readPreference(store, commandUsageKey)); } catch { return {}; }
}
export function saveCommandUse(store: ViewStateStore | undefined, key: string): void {
  const next = recordSnippetUse(readCommandUsage(store), key, Date.now());
  try { writePreference(store, commandUsageKey, JSON.stringify(next)); } catch { /* suggestions also work without storage */ }
}
