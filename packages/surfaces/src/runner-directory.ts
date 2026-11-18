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

/**
 * What the person chose about the listed Runners, app-wide: those hidden from
 * the window, the other local Runners shown in it (`shown`: they are opt-in),
 * the one it opens on, and the Runners it has already pointed out once
 * (`noticed`, the hint "Lazybox Working is also running on this Mac").
 */
export type RunnerPreferences = { hidden: string[]; shown: string[]; primary: string; noticed: string[] };
export const RUNNER_PREFERENCES = 'runners.v1';
const ID = /^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$/;
export const validRunnerId = (value: unknown): value is string => typeof value === 'string' && ID.test(value) && value !== OWN_RUNNER;
const ids = (value: unknown) => Array.isArray(value) ? [...new Set(value.filter(validRunnerId) as string[])].slice(0, 64) : [];
export const emptyRunnerPreferences = (): RunnerPreferences => ({ hidden: [], shown: [], primary: '', noticed: [] });

export function parseRunnerPreferences(text: string | null | undefined): RunnerPreferences {
  try {
    const value = text && text.length < 64 * 1024 ? JSON.parse(text) : null;
    if (value && typeof value === 'object' && !Array.isArray(value)) {
      const primary = validRunnerId(value.primary) ? value.primary : '';
      // A primary Runner was chosen to be shown: before `shown` existed, that was the one explicit "show".
      const shown = ids(value.shown);
      if (primary && !shown.includes(primary)) shown.push(primary);
      return { hidden: ids(value.hidden), shown, primary, noticed: ids(value.noticed) };
    }
  } catch { /* a corrupt choice falls back to the defaults */ }
  return emptyRunnerPreferences();
}

/**
 * Another Runner on this computer that a launcher registered (Dev, Working,
 * the installed release): a window shows only its own Runner unless the person
 * opts in, so Dev's window never looks as if it shared Working's state. A
 * Runner the person added by file, or a computer they added, is shown.
 */
export const optInRunner = (listing: Pick<RunnerListing, 'source'> & Partial<Pick<RunnerListing, 'remote'>>) => listing.source === 'registered' && !listing.remote;
/** Whether the window leaves a listed Runner out: hidden by the person, or a local Runner they have not chosen to show. */
export function runnerHidden(listing: Pick<RunnerListing, 'id' | 'source'> & Partial<Pick<RunnerListing, 'remote'>>, preferences: RunnerPreferences): boolean {
  if (preferences.hidden.includes(listing.id)) return true;
  // The primary Runner was chosen too: the window opens on it.
  return optInRunner(listing) && !preferences.shown.includes(listing.id) && preferences.primary !== listing.id;
}
/** The preferences with a Runner shown or hidden in this window. */
export function withRunnerShown(preferences: RunnerPreferences, id: string, shown: boolean): RunnerPreferences {
  const without = (list: string[]) => list.filter(value => value !== id);
  return shown
    ? { ...preferences, hidden: without(preferences.hidden), shown: [...without(preferences.shown), id] }
    : { ...preferences, hidden: [...without(preferences.hidden), id], shown: without(preferences.shown), primary: preferences.primary === id ? '' : preferences.primary };
}
/** The preferences without a Runner the person forgot or removed. */
export const withoutRunner = (preferences: RunnerPreferences, id: string): RunnerPreferences => ({
  hidden: preferences.hidden.filter(value => value !== id), shown: preferences.shown.filter(value => value !== id),
  primary: preferences.primary === id ? '' : preferences.primary, noticed: preferences.noticed,
});
/**
 * The other local Runners running now that the window leaves out by default
 * and has not pointed out yet: the hint names the first, once, and every one
 * of them is then noticed.
 */
export function unnoticedRunners(listings: readonly RunnerListing[], preferences: RunnerPreferences): RunnerListing[] {
  return listings.filter(listing => !listing.current && listing.state === 'running' && optInRunner(listing) && validRunnerId(listing.id)
    && !preferences.hidden.includes(listing.id) && !preferences.shown.includes(listing.id) && !preferences.noticed.includes(listing.id));
}
/**
 * The window's own channel, by its brand: "Dev" or "Working" when it is not
 * the release app. From its own Runner's listing, else the app's identity
 * (AppInfo.channel: "release", or the bundle id of Dev's or Working's app).
 */
export function windowChannel(ownChannel: string, appChannel = ''): string {
  if (ownChannel) return ownChannel === 'Release' ? '' : ownChannel;
  const app = appChannel.toLowerCase();
  if (!app || app === 'release') return '';
  const last = app.split('.').pop() || '';
  return CHANNELS[last] && CHANNELS[last] !== 'Release' ? CHANNELS[last] : '';
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
/** The name a Runner goes by in the window: its locator's ("Dev", "Working"), the product's, or the file's; a computer's name as it is ("build-box"). */
export function runnerLabel(listing: Pick<RunnerListing, 'name' | 'product'> & Partial<Pick<RunnerListing, 'remote'>>): string {
  const name = listing.name.trim();
  if (listing.remote && name) return name;
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
    rows.push({ id: listing.id, label: runnerLabel(listing), channel: runnerChannel(listing), own: false, health: health(listing.id, listing), hidden: runnerHidden(listing, preferences), primary: primary === listing.id, listing });
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

/**
 * Runners on other computers (REMOTE_RUNNERS.md): a listing with `remote` is
 * reached through a link agent on this computer. These say how its link is,
 * in words, and group the window's Runners by the computer they run on.
 */
export const isRemote = (listing: Pick<RunnerListing, 'remote'> | undefined) => !!listing?.remote;

/** How a remote Runner's link is, as its header says it; empty when it is connected. */
export function linkStateText(listing: Pick<RunnerListing, 'remote' | 'state'> | undefined): string {
  const remote = listing?.remote;
  if (!remote) return '';
  if (listing?.state !== 'running') return 'Connecting…';
  switch (remote.state) {
    case 'connected': return '';
    case 'reconnecting': return 'Reconnecting…';
    case 'refused': return remote.code === 'unknown-key' ? 'Access revoked' : remote.code === 'wrong-runner' ? 'Not its Runner' : 'Refused';
    case 'stopped': return 'Not connected';
    default: return 'Connecting…';
  }
}

/** A Runner's state in words: the link's for a remote Runner, else its health's. */
export function runnerStateText(row: Pick<RunnerSummary, 'health' | 'listing'>): string {
  return linkStateText(row.listing) || healthLabel[row.health];
}

/** A round trip is on the header only when it is slow; always in its tooltip. */
export const SLOW_RTT_MS = 300;
export const rttText = (ms: number | undefined) => ms && ms > 0 ? `${Math.round(ms)} ms` : '';

/** The route a remote Runner is reached by, as a short label. */
export const routeLabel = (route: string | undefined) => ({ ssh: 'SSH', tailnet: 'Tailnet', relay: 'Relay', socket: 'Container' } as Record<string, string>)[route || ''] || (route || '').toUpperCase();

/** One computer of the window: this one, or another reached over a link. */
export type Machine<T> = { id: string; label: string; remote: boolean; host?: string; route?: string; rttMs?: number; state: string; problem?: string; rows: T[] };
export const THIS_MACHINE = 'this-machine';

/**
 * The window's Runners by computer: this one first ("This Mac": Dev,
 * Working, the release), then each other computer by name, keeping the
 * Runners' order within each.
 */
export function groupByMachine<T extends Pick<RunnerSummary, 'listing'>>(rows: readonly T[], thisMachine = 'This Mac'): Machine<T>[] {
  const local: Machine<T> = { id: THIS_MACHINE, label: thisMachine, remote: false, state: '', rows: [] };
  const others = new Map<string, Machine<T>>();
  for (const row of rows) {
    const remote = row.listing?.remote;
    if (!remote) { local.rows.push(row); continue; }
    const label = remote.machine || remote.host;
    const id = `machine:${label.toLowerCase()}`;
    let machine = others.get(id);
    if (!machine) {
      machine = { id, label, remote: true, host: remote.host, route: remote.route, rttMs: remote.rttMs, state: linkStateText(row.listing), problem: remote.state === 'refused' ? remote.problem : undefined, rows: [] };
      others.set(id, machine);
    }
    machine.rows.push(row);
  }
  const out = [...others.values()].sort((a, b) => a.label.localeCompare(b.label));
  return local.rows.length ? [local, ...out] : out;
}

/** This computer's name in the window: "This Mac" on a Mac. */
export const thisMachineLabel = (platform = typeof navigator === 'undefined' ? '' : navigator.platform || navigator.userAgent) => /Mac/i.test(platform) ? 'This Mac' : 'This computer';
