import type { ViewStateStore } from '@lazybox/platform';
import type { RunnerClient } from '@lazybox/client-core';
import { readPreference, writePreference } from './view-state';

/**
 * Sent messages, for ↑/↓ recall in the message box: this conversation's first,
 * newest first, then every other conversation's. Stored per window root, so
 * every window of the app shares one history, like a shell's.
 */
export type PromptEntry = { text: string; conversation: string };
const key = 'prompt-history:v1', limit = 200, textLimit = 16 << 10;

export function parsePromptHistory(raw: string | null): PromptEntry[] {
  try {
    const value = JSON.parse(raw || '[]');
    return Array.isArray(value) ? value.filter(e => e && typeof e.text === 'string' && typeof e.conversation === 'string').slice(0, limit) : [];
  } catch { return []; }
}

/** Pure: the history with `text` as its newest entry; repeats move to the top. Blank and oversized text is not kept. */
export function recordPrompt(history: readonly PromptEntry[], text: string, conversation: string): PromptEntry[] {
  if (!text.trim() || text.length > textLimit) return [...history];
  return [{ text, conversation }, ...history.filter(e => !(e.text === text && e.conversation === conversation))].slice(0, limit);
}

/**
 * Pure: the recall order, newest first and without repeats. `sent` is this
 * conversation's own messages from its transcript (oldest first), so recall
 * works for a conversation sent from another window or device.
 */
export function recallOrder(history: readonly PromptEntry[], conversation: string, sent: readonly string[] = []): string[] {
  // What this window sent is newest; the transcript adds what was sent elsewhere.
  const own = history.filter(e => e.conversation === conversation).map(e => e.text).concat([...sent].reverse());
  const others = history.filter(e => e.conversation !== conversation).map(e => e.text);
  const seen = new Set<string>(), order: string[] = [];
  for (const text of [...own, ...others]) if (text.trim() && !seen.has(text)) { seen.add(text); order.push(text); }
  return order;
}

/** The caret is on the first (or last) line of the text, with nothing selected. */
export function caretOnFirstLine(text: string, start: number, end: number): boolean {
  return start === end && !text.slice(0, start).includes('\n');
}
export function caretOnLastLine(text: string, start: number, end: number): boolean {
  return start === end && !text.slice(end).includes('\n');
}

/**
 * This window's history, then the Runner's prompt log: every prompt sent to
 * any conversation of this Runner, from any window or device (newest first).
 * The Runner's part is read in the background and kept for a while, so a key
 * press never waits on the network; the first recall after a while may lack
 * what other devices sent since.
 */
export function readPromptHistory(store?: ViewStateStore, api?: RunnerClient): PromptEntry[] {
  let local: PromptEntry[] = [];
  try { local = parsePromptHistory(readPreference(store, key)); } catch { /* history is a convenience */ }
  if (!api) return local;
  const cached = runnerHistory.get(api);
  if (!cached || Date.now() - cached.at > RUNNER_HISTORY_MS) refreshRunnerHistory(api);
  return mergeHistory(local, cached?.entries || []);
}
const RUNNER_HISTORY_MS = 15_000;
const runnerHistory = new WeakMap<RunnerClient, { at: number; entries: PromptEntry[]; loading?: boolean }>();
function refreshRunnerHistory(api: RunnerClient) {
  const cached = runnerHistory.get(api);
  if (cached?.loading) return;
  runnerHistory.set(api, { at: cached?.at || 0, entries: cached?.entries || [], loading: true });
  api.listPromptHistory({ limit }, { timeoutMs: 5000 })
    .then(history => runnerHistory.set(api, { at: Date.now(), entries: history.prompts.map(p => ({ text: p.text, conversation: p.conversationId })) }))
    .catch(() => runnerHistory.set(api, { at: Date.now(), entries: cached?.entries || [] }));
}
/** Pure: this window's entries first, then the Runner's that it does not already have. */
export function mergeHistory(local: readonly PromptEntry[], runner: readonly PromptEntry[]): PromptEntry[] {
  const seen = new Set(local.map(e => e.conversation + '\u0000' + e.text));
  return [...local, ...runner.filter(e => !seen.has(e.conversation + '\u0000' + e.text))].slice(0, limit * 2);
}
export function savePrompt(store: ViewStateStore | undefined, text: string, conversation: string): void {
  try { writePreference(store, key, JSON.stringify(recordPrompt(readPromptHistory(store), text, conversation))); }
  catch { /* History is a convenience; a failed save never blocks sending. */ }
}
