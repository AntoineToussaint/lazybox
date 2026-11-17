import type { RunnerListing } from '@lazybox/platform';
import type { InfoResponse } from '@lazybox/protocol';
import { attentionOrder, buildInbox, type Attention, type AttentionState, type Inbox, type Seen } from './attention';
import type { ViewTarget } from './workspace-model';

/**
 * Several Runners in one window (MULTI_RUNNER.md), the pure half:
 * RunnerDirectory.tsx connects to them and renders them. A window always has
 * its own Runner (the one it was opened on, `OWN_RUNNER`); the host may list
 * others (Dev, Working, a standalone Runner, another checkout). Each is
 * addressed by its listing id everywhere a target, a row or a notification
 * names it, so nothing of one Runner is ever sent to another.
 */

/** The id of the window's own Runner, whatever its listing calls it: it never changes, so nothing remounts when the listing arrives. */
export const OWN_RUNNER = 'window';

/** A target in some Runner of the window. Without `runner`, the Runner the page shows. */
export type RunnerTarget = ViewTarget & { runner?: string };

/** What the person chose about the listed Runners: those hidden from the window, and the one it opens on. App-wide. */
export type RunnerPreferences = { hidden: string[]; primary: string };
export const RUNNER_PREFERENCES = 'runners.v1';
const ID = /^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$/;
export const validRunnerId = (value: unknown): value is string => typeof value === 'string' && ID.test(value) && value !== OWN_RUNNER;

export function parseRunnerPreferences(text: string | null | undefined): RunnerPreferences {
  try {
    const value = text && text.length < 64 * 1024 ? JSON.parse(text) : null;
    if (value && typeof value === 'object' && !Array.isArray(value)) {
      const hidden = Array.isArray(value.hidden) ? [...new Set(value.hidden.filter(validRunnerId) as string[])].slice(0, 64) : [];
      return { hidden, primary: validRunnerId(value.primary) ? value.primary : '' };
    }
  } catch { /* a corrupt choice falls back to the defaults */ }
  return { hidden: [], primary: '' };
}

/**
 * How a Runner of the window is doing, as its sidebar header says it:
 * `online` answers; `connecting` has not answered yet; `offline` was listed as
 * running but does not answer (it is restarting, or went away); `stopped` is
 * registered but not running; `invalid` has a locator Lazybox cannot use.
 */
export type RunnerHealth = 'online' | 'connecting' | 'offline' | 'stopped' | 'invalid';
export function runnerHealth(listing: Pick<RunnerListing, 'state'> | undefined, connection: { info?: InfoResponse; connecting: boolean; error: string } | undefined): RunnerHealth {
  if (listing?.state === 'stopped') return 'stopped';
  if (listing?.state === 'invalid') return 'invalid';
  if (!connection || (connection.connecting && !connection.info)) return 'connecting';
  if (connection.error) return 'offline';
  return connection.info ? 'online' : 'connecting';
}
export const healthLabel: Record<RunnerHealth, string> = { online: 'Connected', connecting: 'Connecting…', offline: 'Not reachable', stopped: 'Not running', invalid: 'Cannot be used' };

const CHANNELS: Record<string, string> = { dev: 'Dev', working: 'Working', lazybox: 'Release', release: 'Release' };
/** The name a Runner goes by in the window: its locator's ("Dev", "Working"), the product's, or the file's. */
export function runnerLabel(listing: Pick<RunnerListing, 'name' | 'product'>): string {
  const name = listing.name.trim();
  if (CHANNELS[name.toLowerCase()]) return CHANNELS[name.toLowerCase()];
  if (name) return name.charAt(0).toUpperCase() + name.slice(1);
  return listing.product?.trim() || 'Runner';
}
/** Dev, Working or Release for the launchers' Runners; empty for one the person added. */
export function runnerChannel(listing: Pick<RunnerListing, 'name' | 'channel'>): string {
  const channel = (listing.channel || listing.name).toLowerCase();
  return CHANNELS[channel] || '';
}

/** One Runner of the window, as the sidebar, the inbox and ⌘N see it. */
export type RunnerSummary = {
  id: string;
  label: string;
  channel: string;
  own: boolean;
  health: RunnerHealth;
  hidden: boolean;
  primary: boolean;
  listing?: RunnerListing;
};

/**
 * The Runners in the window's order: the primary first, then the window's own,
 * then those running, then the rest, each by name. The window's own Runner is always listed and never
 * hidden: it is the one this window was opened on. A listing naming the
 * window's own Runner (`current`) is folded into it.
 */
export function orderRunners(listings: readonly RunnerListing[], preferences: RunnerPreferences, health: (id: string, listing?: RunnerListing) => RunnerHealth, ownName = ''): RunnerSummary[] {
  const own = listings.find(listing => listing.current);
  const primary = preferences.primary && listings.some(listing => listing.id === preferences.primary && !listing.current) ? preferences.primary : OWN_RUNNER;
  const rows: RunnerSummary[] = [{
    id: OWN_RUNNER, label: own ? runnerLabel(own) : ownName || 'This Runner', channel: own ? runnerChannel(own) : '', own: true,
    health: health(OWN_RUNNER, own), hidden: false, primary: primary === OWN_RUNNER, listing: own,
  }];
  const seen = new Set<string>();
  for (const listing of listings) {
    if (listing.current || !validRunnerId(listing.id) || seen.has(listing.id)) continue;
    seen.add(listing.id);
    rows.push({ id: listing.id, label: runnerLabel(listing), channel: runnerChannel(listing), own: false, health: health(listing.id, listing), hidden: preferences.hidden.includes(listing.id), primary: primary === listing.id, listing });
  }
  // Two Runners with one name (two checkouts' Dev) are told apart by where they were found.
  const counts = new Map<string, number>();
  for (const row of rows) counts.set(row.label, (counts.get(row.label) || 0) + 1);
  for (const row of rows) if ((counts.get(row.label) || 0) > 1 && row.listing?.locator) row.label = `${row.label} · ${shortPath(row.listing.locator)}`;
  const running = (row: RunnerSummary) => row.own || row.listing?.state === 'running';
  return rows.sort((a, b) => Number(b.primary) - Number(a.primary) || Number(b.own) - Number(a.own) || Number(running(b)) - Number(running(a)) || a.label.localeCompare(b.label));
}
/** The last two segments of a path: enough to tell two checkouts apart. */
export function shortPath(path: string): string {
  const parts = path.split('/').filter(Boolean);
  return parts.length > 2 ? '…/' + parts.slice(-2).join('/') : path;
}

/** The Runner a window opens on: the primary when it is running, else its own. */
export function launchRunner(rows: readonly RunnerSummary[]): string {
  const primary = rows.find(row => row.primary && !row.hidden);
  return primary && !primary.own && primary.listing?.state === 'running' ? primary.id : OWN_RUNNER;
}

/**
 * A row of the merged inbox: which Runner it belongs to. Rows of the Runner the
 * page shows keep their own key (conversationKey), so marks, focus and the
 * existing lists behave as with one Runner; rows of another Runner are keyed
 * under it, so two Runners' conversations never share a key.
 */
export type RunnerRow = Attention & { runner: string };
const SEPARATOR = '␟';
export const scopedKey = (runner: string, key: string) => `${runner}${SEPARATOR}${key}`;
/** The Runner and its own key a merged row's key names. `here` is the Runner the page shows. */
export function splitKey(key: string, here: string): { runner: string; key: string } {
  const at = key.indexOf(SEPARATOR);
  return at < 0 ? { runner: here, key } : { runner: key.slice(0, at), key: key.slice(at + 1) };
}

export type RunnerInbox = Omit<Inbox, 'rows' | 'groups'> & { rows: RunnerRow[]; groups: { state: AttentionState; rows: RunnerRow[] }[]; byRunner: Record<string, Record<AttentionState, number>> };
/**
 * Pure: every Runner's conversations as one inbox, most urgent first across
 * Runners. Each Runner's rows come from its own sessions and its own seen
 * marks. `here` is the Runner the page shows (its keys stay as they are).
 */
export function mergeInboxes(sources: readonly { runner: string; sessions: Parameters<typeof buildInbox>[0]; seen: Seen }[], here: string): RunnerInbox {
  const rows: RunnerRow[] = [];
  const byRunner: Record<string, Record<AttentionState, number>> = {};
  for (const source of sources) {
    const inbox = buildInbox(source.sessions, source.seen);
    byRunner[source.runner] = inbox.counts;
    for (const row of inbox.rows) rows.push({ ...row, runner: source.runner, key: source.runner === here ? row.key : scopedKey(source.runner, row.key) });
  }
  rows.sort((a, b) => attentionOrder.indexOf(a.state) - attentionOrder.indexOf(b.state) || b.at - a.at || (a.key < b.key ? -1 : 1));
  const counts = Object.fromEntries(attentionOrder.map(state => [state, 0])) as Record<AttentionState, number>;
  for (const row of rows) counts[row.state]++;
  return { rows, groups: attentionOrder.map(state => ({ state, rows: rows.filter(row => row.state === state) })).filter(group => group.rows.length), counts, unread: rows.filter(row => row.unread).length, byRunner };
}

/** A conversation of a merged row, as a target the window can show (switching Runner when it is another's). */
export function rowTarget(row: Pick<RunnerRow, 'runner' | 'session'>, commandId = ''): RunnerTarget {
  return { collectionId: row.session.collectionId, sessionId: row.session.id, commandId, runner: row.runner };
}
/** Whether a target names a Runner other than the one the page shows. */
export const elsewhere = (target: RunnerTarget, here: string) => !!target.runner && target.runner !== here;
/** A target as the page stores it: layouts never carry a Runner (the page belongs to one). */
export const localTarget = ({ collectionId, sessionId, commandId }: RunnerTarget): ViewTarget => ({ collectionId, sessionId, commandId });

/** What a Runner's sidebar header counts: conversations that wait for the person, and those to review. */
export function runnerCounts(counts: Record<AttentionState, number> | undefined): { waiting: number; review: number } {
  return { waiting: (counts?.['needs-input'] || 0) + (counts?.failed || 0), review: counts?.review || 0 };
}
