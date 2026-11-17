/**
 * Updates: the pure decisions behind Settings → Updates, the header's notice
 * and the install dialog. An update is offered, never forced: nothing is
 * downloaded or installed until the person chooses Install, a skipped version
 * is never offered again, and Remind me later means a day of quiet.
 * Rendering is in UpdateSettings.tsx; the shared check state in app-update.ts.
 */
import type { AppInfo, AvailableUpdate, UpdateErrorKind, UpdateProgress, UpdateRelease } from '@lazybox/platform';

/** Preference names (view-state.ts `readPreference` / `writePreference`), app-wide. */
export const UPDATE_PREFERENCES = {
  /** "off" turns the automatic check off; anything else (or nothing) leaves it on. */
  automatic: 'updates.check-automatically',
  /** The version the person chose to skip. */
  skipped: 'updates.skipped',
  /** "<version> <epoch ms>": that version is not offered again before then. */
  remind: 'updates.remind-later',
  /** Epoch ms of the last completed check, automatic or asked for. */
  lastChecked: 'updates.last-checked',
  /** The version being installed, so the next launch can say whether it took. */
  installing: 'updates.installing',
} as const;

export const CHECK_EVERY_MS = 6 * 60 * 60 * 1000;
export const REMIND_LATER_MS = 24 * 60 * 60 * 1000;

// ---- Versions --------------------------------------------------------------

type Parsed = { core: [number, number, number]; pre: (number | string)[] };
/** A release version: x.y.z with an optional -prerelease (build metadata ignored). */
export function parseVersion(value: string): Parsed | undefined {
  const m = /^v?(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?(?:\+[0-9A-Za-z.-]+)?$/.exec(value.trim());
  if (!m) return undefined;
  return { core: [Number(m[1]), Number(m[2]), Number(m[3])], pre: m[4] ? m[4].split('.').map(p => /^\d+$/.test(p) ? Number(p) : p) : [] };
}

/**
 * Semantic version order: negative when `a` is older. A prerelease is older
 * than its release (0.2.0-rc.1 < 0.2.0). Something that is not a version
 * sorts before every version, so it is never offered as newer.
 */
export function compareVersions(a: string, b: string): number {
  const x = parseVersion(a), y = parseVersion(b);
  if (!x || !y) return x ? 1 : y ? -1 : 0;
  for (let i = 0; i < 3; i++) if (x.core[i] !== y.core[i]) return x.core[i] - y.core[i];
  if (!x.pre.length || !y.pre.length) return y.pre.length - x.pre.length;
  for (let i = 0; i < Math.max(x.pre.length, y.pre.length); i++) {
    const p = x.pre[i], q = y.pre[i];
    if (p === undefined) return -1;
    if (q === undefined) return 1;
    if (p === q) continue;
    if (typeof p === 'number' && typeof q === 'number') return p - q;
    if (typeof p === 'number') return -1;
    if (typeof q === 'number') return 1;
    return p < q ? -1 : 1;
  }
  return 0;
}
export const isNewer = (candidate: string, current: string) => compareVersions(candidate, current) > 0;

// ---- Preferences -----------------------------------------------------------

export type UpdatePreferences = {
  automatic: boolean;
  skipped: string;
  remind?: { version: string; until: number };
  lastChecked: number;
};
export function readUpdatePreferences(read: (name: string) => string | null | undefined): UpdatePreferences {
  const remind = (read(UPDATE_PREFERENCES.remind) || '').split(' ');
  const until = Number(remind[1]);
  const last = Number(read(UPDATE_PREFERENCES.lastChecked) || 0);
  return {
    automatic: read(UPDATE_PREFERENCES.automatic) !== 'off',
    skipped: read(UPDATE_PREFERENCES.skipped) || '',
    remind: remind[0] && Number.isFinite(until) && until > 0 ? { version: remind[0], until } : undefined,
    lastChecked: Number.isFinite(last) && last > 0 ? last : 0,
  };
}
export const remindValue = (version: string, now: number) => `${version} ${now + REMIND_LATER_MS}`;

/**
 * What to do with an update the feed offers:
 * - `offer`: say so (the header's notice, Settings → Updates);
 * - `skipped`: the person skipped this version; a later one is offered again;
 * - `snoozed`: Remind me later, still within its day (for this version only);
 * - `none`: nothing newer.
 */
export type OfferState = 'none' | 'offer' | 'skipped' | 'snoozed';
export function offerState(available: AvailableUpdate | undefined, prefs: UpdatePreferences, now: number): OfferState {
  if (!available) return 'none';
  if (prefs.skipped && prefs.skipped === available.version) return 'skipped';
  if (prefs.remind && prefs.remind.version === available.version && prefs.remind.until > now) return 'snoozed';
  return 'offer';
}

/** Whether the window should ask the feed now on its own: only when the person left automatic checks on. */
export function automaticCheckDue(prefs: UpdatePreferences, now: number): boolean {
  return prefs.automatic && (!prefs.lastChecked || now - prefs.lastChecked >= CHECK_EVERY_MS || prefs.lastChecked > now);
}

// ---- When ----

/** "5 minutes ago", "yesterday at 14:02": when the feed was last asked. */
export function lastCheckedText(at: number, now: number): string {
  if (!at) return 'Never checked';
  const minutes = Math.round((now - at) / 60_000);
  if (minutes < 1) return 'Checked just now';
  if (minutes < 60) return `Checked ${minutes} minute${minutes === 1 ? '' : 's'} ago`;
  const date = new Date(at), today = new Date(now);
  const time = date.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' });
  if (date.toDateString() === today.toDateString()) return `Checked today at ${time}`;
  const yesterday = new Date(now - 86_400_000);
  if (date.toDateString() === yesterday.toDateString()) return `Checked yesterday at ${time}`;
  return `Checked ${date.toLocaleDateString(undefined, { month: 'short', day: 'numeric' })} at ${time}`;
}

// ---- What's new ------------------------------------------------------------

/**
 * The release notes between the installed version and the update, newest
 * first: the feed's sections down to (not including) the installed
 * version's. `complete` is false when the installed version is not among
 * them (it is older than the notes the feed carries).
 */
export function releasesSince(update: AvailableUpdate, installed: string): { releases: UpdateRelease[]; complete: boolean } {
  const all = update.releases || [];
  const index = all.findIndex(release => release.id === installed);
  if (index >= 0) return { releases: all.slice(0, index), complete: true };
  if (all.length) return { releases: all, complete: false };
  // A feed with plain notes only: one section made of its lines ("• What: body").
  const lines = update.notes.split('\n').map(line => line.trim()).filter(Boolean);
  if (!lines.length) return { releases: [], complete: false };
  const [title, ...rest] = lines[0].startsWith('•') ? [`Lazybox ${update.version}`, ...lines] : lines;
  const items = rest.map(line => {
    const text = line.replace(/^[•\-*]\s*/, '');
    const split = text.indexOf(': ');
    return split > 0 ? { title: text.slice(0, split), body: text.slice(split + 2) } : { title: text, body: '' };
  });
  return { releases: [{ id: update.version, date: update.date.slice(0, 10), title, items }], complete: false };
}

// ---- Running agents ---------------------------------------------------------

/**
 * What installing does to running agents, said precisely. The update
 * replaces the app on disk, then `lazybox service install` stops the old
 * Runner (it detaches) and starts the new one, which re-attaches every
 * session host (SESSION_HOSTS.md). The agents' processes never stop.
 */
export function agentsNotice(sessions: readonly { closed?: boolean; historical?: boolean; busy?: boolean }[]): { count: number; working: number; title: string; body: string } | undefined {
  const live = sessions.filter(s => !s.closed && !s.historical);
  if (!live.length) return undefined;
  const working = live.filter(s => s.busy).length;
  const agents = live.length === 1 ? '1 agent is running' : `${live.length} agents are running`;
  const busy = working === 0 ? '' : working === live.length ? (live.length === 1 ? ', in the middle of a turn' : ', all in the middle of a turn') : `, ${working} in the middle of a turn`;
  return {
    count: live.length,
    working,
    title: `${agents}${busy}. ${live.length === 1 ? 'It keeps' : 'They keep'} running.`,
    body: 'Installing replaces the app and restarts its Runner; the new Runner picks every agent, command and terminal back up within seconds. Nothing they write meanwhile is lost, and an approval that is waiting stays waiting. Lazybox\'s windows close for a moment and reopen where they were.',
  };
}

// ---- Errors and progress ---------------------------------------------------

/** A failed check or install as a sentence a person can act on. */
export function updateErrorText(kind: UpdateErrorKind | undefined, phase: 'check' | 'install', detail = ''): string {
  switch (kind) {
    case 'offline': return phase === 'check'
      ? 'Lazybox could not reach GitHub to check for updates. Check your connection; it tries again later.'
      : 'The download stopped: Lazybox could not reach GitHub. Nothing was changed. Try again when you are back online.';
    case 'feed': return 'No update information is published where Lazybox looks (the v2-updates release on GitHub). Nothing to do; it tries again later.';
    case 'signature': return 'The download is not signed with Lazybox\'s key, so it was not installed. Your current version is untouched. Try again later; if it keeps happening, tell us from Help → Send feedback.';
    case 'permission': return 'Lazybox cannot replace itself where it is installed. Move Lazybox to Applications, or install the new version from its disk image.';
    case 'unavailable': return 'This build of Lazybox does not update itself.';
    default: return phase === 'check'
      ? `Could not check for updates${detail ? `: ${detail}` : ''}.`
      : `The update could not be installed${detail ? `: ${detail}` : ''}. Your current version keeps working.`;
  }
}

export type InstallProgress = UpdateProgress | { phase: 'starting'; downloaded: 0; total: 0 };
/** Progress as a sentence: what the person is waiting for. */
export function progressText(p: InstallProgress | undefined, version = ''): string {
  if (!p) return '';
  switch (p.phase) {
    case 'download': return p.total > 0 ? `Downloading… ${Math.min(100, Math.round((p.downloaded / p.total) * 100))}%` : 'Downloading…';
    case 'verify': return 'Checking the signature…';
    case 'install': return 'Installing…';
    case 'runner': return `Moving the Runner to ${version || 'the new version'}…`;
    case 'restart': return 'Restarting Lazybox…';
    default: return 'Starting the download…';
  }
}
/** 0–1 for a progress bar; undefined while the length is unknown. */
export function progressFraction(p: InstallProgress | undefined): number | undefined {
  if (!p) return undefined;
  if (p.phase === 'download') return p.total > 0 ? Math.min(1, p.downloaded / p.total) * 0.8 : undefined;
  return { starting: 0, verify: 0.82, install: 0.88, runner: 0.94, restart: 1 }[p.phase];
}

// ---- After the restart -----------------------------------------------------

/**
 * The launch after an install: did it take? `updated` when the app now runs
 * the version that was being installed (What's new opens), `failed` when it
 * still runs an older one (say so, once), `none` otherwise.
 */
export function postUpdateDecision(installing: string | null | undefined, current: string): 'none' | 'updated' | 'failed' {
  if (!installing || !parseVersion(installing)) return 'none';
  const order = compareVersions(current, installing);
  return order >= 0 ? 'updated' : 'failed';
}

// ---- App, Runner and CLI agree ---------------------------------------------

/**
 * The app, its Runner and its CLI are one release. A Runner on another
 * version (the app was updated and the service not yet moved, or someone
 * reinstalled one without the other) is explained, with the fix.
 */
export function versionAgreement({ release, app, runner }: { release: boolean; app: string; runner: string }): { agree: boolean; problem: string; canMove: boolean } {
  if (!release) return { agree: true, problem: '', canMove: false };
  if (!runner) return { agree: false, problem: 'The Runner is not answering, so its version is unknown.', canMove: false };
  if (runner === app) return { agree: true, problem: '', canMove: false };
  const older = compareVersions(runner, app) < 0;
  return {
    agree: false,
    problem: older
      ? `The Runner is still ${runner} and the app is ${app}: the app was updated but its Runner has not moved yet. Lazybox moves it the next time the app starts; Move the Runner now does it without waiting. Agents keep running.`
      : `The Runner is ${runner}, newer than this app (${app}): another copy of Lazybox installed it. Move the Runner puts it back on this app's version (agents keep running), or update this app.`,
    canMove: true,
  };
}

/**
 * How this copy of Lazybox gets new versions, for Settings → Updates and
 * About. A build from source (`make install`) has no updater and never
 * checks a feed: it updates with `make update` in the checkout it was built
 * from. Dev and Working move through stage and promote. Only a downloaded
 * release (`updater`) updates itself.
 */
export type UpdatePath = { kind: 'updater' | 'source' | 'development' | 'browser'; text: string; commit?: string };
export function updatePath(info: Pick<AppInfo, 'channel' | 'updater' | 'source'> | undefined): UpdatePath {
  if (!info) return { kind: 'browser', text: 'Updates come with the Lazybox app for Mac; this window is not the app.' };
  if (info.source) {
    const commit = info.source.commit ? info.source.commit.slice(0, 12) + (info.source.dirty ? ', with uncommitted changes' : '') : undefined;
    const where = info.source.checkout ? ` in ${info.source.checkout}` : '';
    return { kind: 'source', commit, text: `Built from source; update with make update${where}. Lazybox does not check for updates by itself.` };
  }
  if (info.updater) return { kind: 'updater', text: 'Lazybox tells you when a new version is out and installs it only when you say so.' };
  return { kind: 'development', text: `A development build (${info.channel}): it updates through make stage and make promote, not by itself.` };
}
