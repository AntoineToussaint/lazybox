import { useCallback, useEffect, useState } from 'react';
import type { ViewStateStore } from '@lazybox/platform';
import { readPreference, writePreference } from './view-state';
import { isDiffMode, type DiffMode } from './diff-layout';

/** Below this width a side-by-side diff is two unreadable slivers: it shows unified. */
export const SPLIT_MIN_WIDTH = 700;

/** Unified or side by side, remembered for every window of this Runner. */
export function useDiffMode(store?: ViewStateStore): [DiffMode, (mode: DiffMode) => void] {
  const [mode, setMode] = useState<DiffMode>(() => { try { const saved = readPreference(store, 'changes.diff-mode'); return isDiffMode(saved) ? saved : 'unified'; } catch { return 'unified'; } });
  const choose = useCallback((next: DiffMode) => { setMode(next); try { writePreference(store, 'changes.diff-mode', next); } catch { /* a convenience */ } }, [store]);
  return [mode, choose];
}

/** Whether the element (from a callback ref) is narrower than `width`. */
export function useNarrow(node: HTMLElement | null, width = SPLIT_MIN_WIDTH): boolean {
  const [narrow, setNarrow] = useState(false);
  useEffect(() => {
    if (!node || typeof ResizeObserver === 'undefined') return;
    const observer = new ResizeObserver(([entry]) => setNarrow(entry.contentRect.width > 0 && entry.contentRect.width < width));
    observer.observe(node);
    return () => observer.disconnect();
  }, [node, width]);
  return narrow;
}

/** Moves to the next (1) or previous (-1) hunk of the diff in `root`: the
 * focused hunk's neighbour, else the first one past the top of the view.
 * Returns false at either end. */
export function moveToHunk(root: HTMLElement, step: 1 | -1): boolean {
  const hunks = [...root.querySelectorAll<HTMLElement>('.review-diff [data-diff-hunk]')];
  if (!hunks.length) return false;
  let current = hunks.indexOf(document.activeElement as HTMLElement);
  if (current < 0) {
    const scroller = root.querySelector<HTMLElement>('.diff-view') || root;
    const top = scroller.getBoundingClientRect().top + 8;
    const below = hunks.findIndex(hunk => hunk.getBoundingClientRect().top > top);
    // Nothing focused yet: n goes to the first hunk below the top, p to the one above it.
    current = below < 0 ? hunks.length : below;
    if (step > 0) current--;
  }
  const next = hunks[current + step];
  if (!next) return false;
  next.focus({ preventScroll: true });
  next.scrollIntoView({ block: 'start' });
  return true;
}

/** Keys typed in a text field, a composer, or with a modifier are not the diff's; nor are keys in the review tray or the commit / pull request area beside it. */
export const diffKeyTarget = (event: { target: EventTarget | null; ctrlKey: boolean; metaKey: boolean; altKey: boolean; defaultPrevented: boolean }) => {
  const element = event.target as HTMLElement | null;
  return !event.defaultPrevented && !event.ctrlKey && !event.metaKey && !event.altKey && !!element && !element.closest('input:not([type=checkbox]),textarea,select,[contenteditable=""],[contenteditable=true],.monaco-editor,.review-tray,.ship');
};

/** One file of the Changes list: committed on the branch, or uncommitted. */
export const fileKey = (member: string, path: string, committed: boolean) => `${committed ? 'c' : 'u'}:${member}/${path}`;
const LIMIT = 2000;

/**
 * Files the person marked viewed, per workspace, as on GitHub: a file whose
 * diff changed since is no longer viewed. Each mark keeps the diff's content
 * key; a file marked from the list before its diff was read keeps none, and
 * takes the diff's key the first time it is shown.
 */
export function useViewedFiles(workspaceId: string, store?: ViewStateStore) {
  const name = `changes.viewed:${workspaceId}`;
  const read = () => { try { const value = JSON.parse(readPreference(store, name) || '{}'); return value && typeof value === 'object' && !Array.isArray(value) ? value as Record<string, string> : {}; } catch { return {}; } };
  const [marks, setMarks] = useState<Record<string, string>>(read);
  useEffect(() => setMarks(read()), [workspaceId, store]);
  const save = (next: Record<string, string>) => {
    const entries = Object.entries(next);
    const kept = entries.length > LIMIT ? Object.fromEntries(entries.slice(-LIMIT)) : next;
    setMarks(kept);
    try { writePreference(store, name, JSON.stringify(kept)); } catch { /* a convenience */ }
  };
  return {
    isViewed: (key: string) => Object.hasOwn(marks, key),
    count: (keys: string[]) => keys.filter(key => Object.hasOwn(marks, key)).length,
    set(key: string, viewed: boolean, content = '') {
      if (viewed === Object.hasOwn(marks, key) && (!viewed || marks[key] === content)) return;
      const next = { ...marks };
      if (viewed) next[key] = content; else delete next[key];
      save(next);
    },
    /** The file's diff as read now: a mark made on another diff is dropped. */
    reconcile(key: string, content: string) {
      if (!Object.hasOwn(marks, key) || marks[key] === content) return;
      const next = { ...marks };
      if (marks[key] === '') next[key] = content; else delete next[key];
      save(next);
    },
  };
}
