import { useEffect, useRef } from 'react';
import type { CommandStatus } from '@lazybox/protocol';
import type { RunnerClient } from '@lazybox/client-core';
import type { ViewStateStore } from '@lazybox/platform';
import { isRuntimeRun, isWorkspaceRun, RunTracker } from './command-runs';
import { groupOf, shownPanels, type Panel, type WorkspacePage } from './workspace-model';

// Which run the command output ("console") shows when runs start by themselves.
// The person stays in control: a new run is offered (the "newer" chip, the
// picker), and takes the console over only when nobody is reading it.

/** A run still going this long after it was first listed may take the console over; one that ends sooner is only offered. */
export const LONG_RUN_MS = 1500;
/** Choosing or using the console (picking a run, finding, scrolling, selecting, A−/A+) keeps it on its run this long. */
export const RECENT_USE_MS = 30_000;

export type TakeOverInput = {
  /** The window's "Follow new runs" choice. */
  follow: boolean;
  /** The run the console shows now; absent when it shows none (or one no longer listed). */
  shown?: { done: boolean };
  /** Milliseconds since the person last chose or used the console. */
  sinceUse: number;
  /**
   * The new run: whether it ended, how long it has run since it was first
   * listed, and whether it came from the person's own terminal (`lazybox log`),
   * which they started themselves and so wait for, however quick.
   */
  run: { done: boolean; runningFor: number; yours?: boolean };
};

/** Should a run that just started take over the console the person can see? */
export function shouldTakeOver({ follow, shown, sinceUse, run }: TakeOverInput): boolean {
  if (!follow) return false;
  if (!run.yours && (run.done || run.runningFor < LONG_RUN_MS)) return false;
  if (shown && !shown.done) return false;
  return sinceUse >= RECENT_USE_MS;
}

type Listed = Pick<CommandStatus, 'id' | 'done' | 'origin' | 'sessionId' | 'collectionId'>;
export type RunNews = {
  /** Runs listed for the first time, oldest first (the first listing is the baseline). */
  fresh: string[];
  /** Runs that may take the console over now, oldest first, each once: still running LONG_RUN_MS after they were first listed, or from the person's own terminal. */
  candidates: { id: string; runningFor: number; yours: boolean }[];
};

/**
 * RunWatch reads the listings of a session's runs. Actions and setup scripts
 * have their own panel and are never reported. A run that ends before
 * LONG_RUN_MS is never a candidate: it is listed and counted, nothing more.
 */
export class RunWatch {
  private tracker = new RunTracker();
  private pending = new Map<string, number>();
  /** `listedAt`: when the listing was asked for; `now`: when it arrived. */
  next(commands: readonly Listed[], listedAt: number, now = listedAt): RunNews {
    const runs = commands.filter(command => !isRuntimeRun(command));
    const fresh = this.tracker.next(runs.map(command => command.id));
    const byId = new Map(runs.map(command => [command.id, command]));
    const candidates: RunNews['candidates'] = [];
    for (const id of fresh) {
      const command = byId.get(id)!;
      if (isWorkspaceRun(command)) candidates.push({ id, runningFor: 0, yours: true });
      else if (!command.done) this.pending.set(id, listedAt);
    }
    for (const [id, since] of this.pending) {
      const command = byId.get(id);
      if (!command || command.done) { this.pending.delete(id); continue; }
      if (now - since >= LONG_RUN_MS) { this.pending.delete(id); candidates.push({ id, runningFor: now - since, yours: false }); }
    }
    return { fresh, candidates };
  }
}

/** The runs newer than the one shown (a listing is oldest first), oldest first; actions and setup scripts are not counted. */
export function newerRuns<T extends Listed>(commands: readonly T[], shown: string): T[] {
  const index = commands.findIndex(command => command.id === shown);
  return index < 0 ? [] : commands.slice(index + 1).filter(command => !isRuntimeRun(command));
}

// What this window's console shows and when the person last used it, per
// conversation. The console reports both; the window reads them when a run
// starts. A window is one page, so this lives as long as the page does.
const consoles = new Map<string, { shown: string; usedAt: number }>();
const entry = (sessionId: string) => { let value = consoles.get(sessionId); if (!value) { value = { shown: '', usedAt: 0 }; consoles.set(sessionId, value); } return value; };
export function noteConsoleShows(sessionId: string, commandId: string) { if (sessionId) entry(sessionId).shown = commandId; }
export function noteConsoleUse(sessionId: string, at = Date.now()) { if (sessionId) entry(sessionId).usedAt = at; }
export function consoleView(sessionId: string): { shown: string; usedAt: number } { return { ...(consoles.get(sessionId) || { shown: '', usedAt: 0 }) }; }

/** "Follow new runs", remembered for the window's workspace like the rest of its console view (on unless turned off). */
const followKey = (store: ViewStateStore) => `${store.scope || store.key}:console-follow-runs`;
export function readFollowRuns(store?: ViewStateStore): boolean {
  if (!store) return true;
  try { return store.getItem(followKey(store)) !== 'off'; } catch { return true; }
}
export function writeFollowRuns(store: ViewStateStore | undefined, on: boolean) {
  if (store) store.setItem(followKey(store), on ? 'on' : 'off');
}

/**
 * Where the page's console is: none; put away by the person; behind another
 * tab of its group; its group folded to a tab (not by the person, as the
 * starting layouts do); or what its group shows.
 */
export type ConsolePlace = 'none' | 'away' | 'behind' | 'folded' | 'front';
export function followConsole(page: WorkspacePage): Panel | undefined {
  return shownPanels(page).find(panel => panel.kind === 'console' && panel.binding.mode === 'follow' && !panel.detached);
}
export function consolePlace(page: WorkspacePage, putAway: boolean): ConsolePlace {
  const output = followConsole(page);
  if (!output) return 'none';
  if (putAway) return 'away';
  const group = groupOf(page, output.id);
  if (!group || group.active !== output.id) return 'behind';
  return group.minimized ? 'folded' : 'front';
}

type Follow = { commandId: string };
/**
 * What a listing changes. A console nobody can see (put away, behind another
 * tab, gone) points at the newest run, so opening it shows that one, and is
 * never brought forward. A folded console stays folded.
 * A console in view moves only when shouldTakeOver says so.
 */
export function followRuns(news: RunNews, input: { follow: boolean; place: ConsolePlace; shown?: { done: boolean }; sinceUse: number }): Follow | undefined {
  if (!input.follow) return undefined;
  const candidate = news.candidates.at(-1);
  if (input.place === 'front') return candidate && shouldTakeOver({ follow: true, shown: input.shown, sinceUse: input.sinceUse, run: { done: false, ...candidate } }) ? { commandId: candidate.id } : undefined;
  const newest = news.fresh.at(-1);
  return newest ? { commandId: newest } : undefined;
}

/**
 * useRunFollowing lists the session's runs at the console's own interval and
 * hands `onFollow` what followRuns decides. `context` is read at each listing.
 */
export function useRunFollowing(api: RunnerClient, sessionId: string, context: () => { follow: boolean; place: ConsolePlace; shown: string }, onFollow: (follow: Follow) => void) {
  const latest = useRef({ context, onFollow }); latest.current = { context, onFollow };
  useEffect(() => {
    if (!sessionId) return;
    const watch = new RunWatch(), control = new AbortController(); let updating = false;
    const update = async () => {
      if (updating) return; updating = true;
      try {
        const listedAt = Date.now();
        const { commands } = await api.listCommands({ sessionId }, { signal: control.signal });
        if (control.signal.aborted) return;
        const now = Date.now(), news = watch.next(commands, listedAt, now);
        if (!news.fresh.length && !news.candidates.length) return;
        const { follow, place, shown } = latest.current.context();
        const view = consoleView(sessionId), showing = shown || view.shown;
        const status = commands.find(command => command.id === showing);
        const decided = followRuns(news, { follow, place, shown: status && { done: status.done }, sinceUse: now - view.usedAt });
        if (decided) latest.current.onFollow(decided);
      }
      catch { /* The console panel reports listing errors when it is open. */ }
      finally { updating = false; }
    };
    void update(); const timer = setInterval(() => void update(), 1500);
    return () => { control.abort(); clearInterval(timer); };
  }, [api, sessionId]);
}
