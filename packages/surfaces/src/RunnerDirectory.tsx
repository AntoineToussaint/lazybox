import { createContext, useCallback, useContext, useEffect, useLayoutEffect, useMemo, useRef, useState, useSyncExternalStore, type ReactNode } from 'react';
import type { Collection } from '@lazybox/protocol';
import type { AppHost, RunnerDirectoryHost, RunnerListing, ViewStateStore } from '@lazybox/platform';
import { RunnerStateProvider, useRunner, useRunnerState, type RunnerConnection, type RunnerState } from './RunnerContext';
import { useRunnerListing } from './runner-locator';
import { useWorkspaceCatalog } from './useWorkspaceCatalog';
import { AttentionScope, markSeenIn, readSeenText, subscribeAttention, useAttentionHost, useAttentionRoot, useInbox } from './useAttention';
import { conversationKey, parseSeen, seenMark, type AttentionState } from './attention';
import { mergeConversations } from './conversation-model';
import { healthLabel, launchRunner, mergeInboxes, orderRunners, OWN_RUNNER, parseRunnerPreferences, RUNNER_PREFERENCES, runnerHealth, runnerLabel, splitKey, unnoticedRunners, windowChannel, withRunnerShown, type RunnerHealth, type RunnerInbox, type RunnerPreferences, type RunnerRow, type RunnerSummary } from './runner-directory';
import { PREFERENCE_EVENT, readPreference, writePreference } from './view-state';
import { showToast } from './toasts';
import { Icon } from './Icons';
import type { ViewTarget } from './workspace-model';
import './runner-directory.css';

/**
 * Several Runners in one window (MULTI_RUNNER.md step 2). The window's own
 * Runner is its RunnerProvider; every other Runner its host lists gets a
 * connection of its own here (its own link, bearer, renewal and info), never
 * mixed with another's. `RunnerScope` makes everything under it talk to one
 * Runner, so a sidebar section, an inbox row, a peek or the whole page of
 * another Runner calls exactly that Runner. The page shows one Runner at a
 * time (the active one); the sidebar and the inbox show them all.
 */

/** One Runner of the window as every list sees it. */
export type RunnerView = RunnerSummary & {
  /** Its connection; absent while it is not running. */
  state?: RunnerState;
  /** Where its seen marks and notification choices are kept. */
  root: string;
  collections: Collection[];
  collectionsLoaded: boolean;
  /** The Runner's own version ("0.1.21", "dev"); empty until it answers. */
  version: string;
};
type Directory = {
  runners: RunnerView[];
  /** Not hidden by the person. */
  shown: RunnerView[];
  /** More than one Runner is shown: lists group and badge by Runner. */
  multi: boolean;
  /** The Runner the page shows. */
  active: string;
  /** Show another Runner's page, opening `target` on it. */
  activate(id: string, target?: ViewTarget): void;
  /** What the active Runner's page opens on after a switch. */
  target?: ViewTarget;
  preferences: RunnerPreferences;
  setPreferences(next: RunnerPreferences): void;
  host?: RunnerDirectoryHost;
  listError: string;
  /** The first listing has been read (or there is nothing to list). */
  ready: boolean;
  /** The window is deciding which Runner its page opens on (the primary, if it runs). */
  launching: boolean;
  reread(): Promise<void>;
};
const DirectoryContext = createContext<Directory | null>(null);
/** Which Runner the components under it talk to. */
type Identity = { id: string; own: boolean; label: string; multi: boolean };
const IdentityContext = createContext<Identity>({ id: OWN_RUNNER, own: true, label: '', multi: false });

/** The window's Runners; undefined in a window that shows one (an agent window, the phone). */
export function useRunnerDirectory(): Directory | undefined { return useContext(DirectoryContext) || undefined; }
/** The Runner this part of the page belongs to. */
export function useRunnerIdentity(): Identity { return useContext(IdentityContext); }

type Extras = { collections: Collection[]; collectionsLoaded: boolean; version: string };

export function RunnerDirectoryProvider({ host, viewState, ownName = '', children }: { host?: RunnerDirectoryHost; viewState?: ViewStateStore; ownName?: string; children: ReactNode }) {
  const own = useRunner();
  const ownRoot = useAttentionRoot();
  const { listings, error: listError, ready, reread: rereadListing } = useRunnerListing(host);
  const [states, setStates] = useState<Record<string, RunnerState | undefined>>({});
  const [extras, setExtras] = useState<Record<string, Extras>>({});
  const [preferences, setPreferencesState] = useState(() => parseRunnerPreferences(readPreference(viewState, RUNNER_PREFERENCES)));
  useEffect(() => {
    const reread = () => setPreferencesState(previous => { const next = parseRunnerPreferences(readPreference(viewState, RUNNER_PREFERENCES)); return JSON.stringify(next) === JSON.stringify(previous) ? previous : next; });
    window.addEventListener(PREFERENCE_EVENT, reread);
    return () => window.removeEventListener(PREFERENCE_EVENT, reread);
  }, [viewState]);
  const setPreferences = useCallback((next: RunnerPreferences) => {
    setPreferencesState(next);
    try { writePreference(viewState, RUNNER_PREFERENCES, JSON.stringify(next)); } catch { /* holds for this window */ }
  }, [viewState]);
  const report = useCallback((id: string, state: RunnerState | undefined) => setStates(previous => previous[id] === state ? previous : { ...previous, [id]: state }), []);
  const reportExtras = useCallback((id: string, value: Extras) => setExtras(previous => { const before = previous[id]; return before && before.collections === value.collections && before.collectionsLoaded === value.collectionsLoaded && before.version === value.version ? previous : { ...previous, [id]: value }; }), []);

  const summaries = useMemo(() => orderRunners(listings, preferences, (id, listing) => runnerHealth(listing, id === OWN_RUNNER ? own : states[id]), ownName), [listings, preferences, own, states, ownName]);
  const shownCount = summaries.filter(row => !row.hidden).length;
  const multi = shownCount > 1;
  const runners = useMemo<RunnerView[]>(() => summaries.map(row => ({
    ...row, state: row.id === OWN_RUNNER ? own : states[row.id], root: row.id === OWN_RUNNER ? ownRoot : `${ownRoot}:runner:${row.id}`,
    collections: extras[row.id]?.collections || [], collectionsLoaded: !!extras[row.id]?.collectionsLoaded, version: extras[row.id]?.version || '',
  })), [summaries, own, states, ownRoot, extras]);

  // The page opens on the primary Runner when it is running; switching later is for this window's life.
  const [active, setActive] = useState(OWN_RUNNER);
  const [target, setTarget] = useState<ViewTarget>();
  // Until the first listing says whether the primary Runner runs, the page waits (briefly) rather than opening on one Runner and jumping to another.
  const [launching, setLaunching] = useState(() => !!host && !!preferences.primary);
  useEffect(() => {
    if (!launching || !ready) return;
    const id = launchRunner(summaries);
    if (id === OWN_RUNNER) { setLaunching(false); return; }
    if (states[id]) { setActive(id); setLaunching(false); }
  }, [launching, ready, summaries, states]);
  useEffect(() => { if (!launching) return; const timer = setTimeout(() => setLaunching(false), 2500); return () => clearTimeout(timer); }, [launching]);
  // A Runner hidden, no longer listed or stopped hands the page back to the window's own.
  // (One that only stops answering keeps its page, which says it is reconnecting.)
  useEffect(() => {
    if (active === OWN_RUNNER || !ready) return;
    const row = runners.find(value => value.id === active);
    if (row && !row.hidden && row.state) return;
    setTarget(undefined); setActive(OWN_RUNNER);
    if (row && !row.hidden) showToast({ id: `runner:${row.id}`, tone: 'info', title: `${row.label} stopped`, body: `Showing ${runners.find(value => value.own)?.label || 'this Runner'}. ${row.label}'s agents show again once it is back.` });
  }, [active, runners, ready]);
  const latest = useRef({ runners, active }); latest.current = { runners, active };
  const activate = useCallback((id: string, next?: ViewTarget) => {
    const { runners, active } = latest.current;
    if (id === active) return;
    const row = runners.find(value => value.id === id);
    if (!row?.state) { showToast({ tone: 'info', title: `${row?.label || 'That Runner'} is not running`, body: row?.listing?.problem || 'Start it, and its workspaces show here again.' }); return; }
    setTarget(next); setActive(id);
  }, []);
  const reread = useCallback(async () => { await rereadListing().catch(() => {}); }, [rereadListing]);

  const fetch = own.connection.fetch;
  // Only the Runners the window shows are connected: a hidden one (another local Runner by default) is not called at all.
  const connected = listings.filter(listing => !listing.current && listing.url && listing.token && summaries.some(row => row.id === listing.id && !row.hidden));
  useRunnerHint(listings, preferences, setPreferences, ready && !!host);
  const value = useMemo<Directory>(() => ({ runners, shown: runners.filter(row => !row.hidden), multi, active, activate, target, preferences, setPreferences, host, listError, ready, launching, reread }),
    [runners, multi, active, activate, target, preferences, setPreferences, host, listError, ready, launching, reread]);
  return <DirectoryContext.Provider value={value}>
    {connected.map(listing => <OtherRunner key={listing.id} listing={listing} fetch={fetch} reread={rereadListing} onState={report}/>)}
    {multi && runners.filter(row => row.state && !row.hidden).map(row => <RunnerStateProvider key={row.id} state={row.state!}><RunnerExtras id={row.id} onExtras={reportExtras}/></RunnerStateProvider>)}
    {children}
  </DirectoryContext.Provider>;
}

/**
 * One quiet hint, the first time another local Runner is found running: the
 * window shows only its own Runner, so it says once that Working (or Dev)
 * runs too and offers to show it here. Each Runner is pointed out once, ever
 * (`noticed`); dismissing the hint is enough.
 */
function useRunnerHint(listings: readonly RunnerListing[], preferences: RunnerPreferences, setPreferences: (next: RunnerPreferences) => void, ready: boolean) {
  const latest = useRef(preferences); latest.current = preferences;
  const unnoticed = useMemo(() => ready ? unnoticedRunners(listings, preferences) : [], [ready, listings, preferences]);
  useEffect(() => {
    if (!unnoticed.length) return;
    const first = unnoticed[0], label = runnerLabel(first);
    // Working's app is called plain "Lazybox": the hint names it by its channel.
    const name = label === 'Release' ? 'The installed Lazybox' : `Lazybox ${label}`;
    setPreferences({ ...latest.current, noticed: [...latest.current.noticed, ...unnoticed.map(listing => listing.id)].slice(-64) });
    showToast({ id: 'runner-hint', tone: 'info', title: `${name} is also running on this Mac`, body: 'Show it here? (Settings → Runners)', timeout: 20_000,
      button: { label: 'Show it here', run: () => setPreferences(withRunnerShown(latest.current, first.id, true)) } });
  }, [unnoticed, setPreferences]);
}

/** The window's channel by its brand ("Dev", "Working"); empty for the release app. */
export function useWindowChannel(app?: AppHost): string {
  const directory = useRunnerDirectory();
  const own = directory?.runners.find(row => row.own)?.channel || '';
  const [appChannel, setAppChannel] = useState('');
  useEffect(() => { if (own || !app) return; let live = true; void app.info().then(info => { if (live) setAppChannel(info.channel); }, () => {}); return () => { live = false; }; }, [app, own]);
  return windowChannel(own, appChannel);
}

/** Another Runner's connection: its own link, bearer and renewal, from its listing. */
function OtherRunner({ listing, fetch, reread, onState }: { listing: RunnerListing; fetch?: typeof globalThis.fetch; reread(): Promise<RunnerListing[]>; onState(id: string, state: RunnerState | undefined): void }) {
  const id = listing.id;
  // The last authority it served on: a Runner going away keeps its section (offline), not a blank.
  const [authority, setAuthority] = useState({ baseUrl: listing.url!, token: listing.token! });
  useEffect(() => { if (listing.url && listing.token) setAuthority(previous => previous.baseUrl === listing.url && previous.token === listing.token ? previous : { baseUrl: listing.url!, token: listing.token! }); }, [listing.url, listing.token]);
  const renew = useCallback(async () => {
    const next = (await reread().catch(() => [])).find(value => value.id === id);
    return next?.url && next.token ? { baseUrl: next.url, token: next.token } : undefined;
  }, [reread, id]);
  const connection = useMemo<RunnerConnection>(() => ({ ...authority, fetch, renew }), [authority, fetch, renew]);
  const state = useRunnerState(connection);
  useLayoutEffect(() => { onState(id, state); }, [id, state, onState]);
  useEffect(() => () => onState(id, undefined), [id, onState]);
  return null;
}

/** What a Runner's sidebar section shows beyond its info: its workspaces and version. Only while the window shows several. */
function RunnerExtras({ id, onExtras }: { id: string; onExtras(id: string, value: Extras): void }) {
  const { api, info } = useRunner();
  const { collections, catalogLoaded } = useWorkspaceCatalog(api);
  const [version, setVersion] = useState('');
  const instance = info?.instanceId || '';
  useEffect(() => {
    const control = new AbortController();
    void api.version({}, { signal: control.signal, timeoutMs: 5000 }).then(value => setVersion(value.version), () => {});
    return () => control.abort();
  }, [api, instance]);
  useEffect(() => { onExtras(id, { collections, collectionsLoaded: catalogLoaded, version }); }, [id, collections, catalogLoaded, version, onExtras]);
  return null;
}

/**
 * Everything under it talks to `runner`: its connection, its seen marks.
 * Notifications stay the window's (the merged inbox raises them). Renders
 * `fallback` while that Runner has no connection.
 */
export function RunnerScope({ runner, children, fallback = null, here = false }: { runner: string; children: ReactNode; fallback?: ReactNode; /** Already this Runner's part of the page: nothing to re-provide. */ here?: boolean }) {
  const directory = useContext(DirectoryContext);
  const host = useAttentionHost();
  const row = directory?.runners.find(value => value.id === runner);
  const identity = useMemo<Identity>(() => ({ id: runner, own: runner === OWN_RUNNER, label: row?.label || '', multi: !!directory?.multi }), [runner, row?.label, directory?.multi]);
  if (!directory || here) return <>{children}</>;
  if (!row?.state) return <>{fallback}</>;
  return <RunnerStateProvider state={row.state}><IdentityContext.Provider value={identity}><AttentionScope root={row.root} host={host}>{children}</AttentionScope></IdentityContext.Provider></RunnerStateProvider>;
}

/** Renders `render` once per shown, connected Runner, each in its own scope. */
export function EachRunner({ render, include }: { render(row: RunnerView): ReactNode; include?(row: RunnerView): boolean }) {
  const directory = useContext(DirectoryContext);
  if (!directory?.multi) return null;
  return <>{directory.shown.filter(row => row.state && (!include || include(row))).map(row => <RunnerScope key={row.id} runner={row.id}>{render(row)}</RunnerScope>)}</>;
}

/** Seen marks of several Runners at once, as one snapshot. */
function useSeenTexts(roots: readonly string[]): (string | null)[] {
  const joined = useSyncExternalStore(subscribeAttention, () => JSON.stringify(roots.map(readSeenText)));
  return useMemo(() => JSON.parse(joined) as (string | null)[], [joined]);
}

/**
 * The inbox of every Runner the window shows, most urgent first across them,
 * each row naming its Runner. With one Runner it is that Runner's inbox. A
 * Runner that does not answer contributes nothing (its section says so):
 * nothing it last said is offered as if it could still be acted on.
 */
export function useMergedInbox(): RunnerInbox & { multi: boolean; here: string; offline: RunnerView[]; mark(entries: { key: string; at: number }[]): void } {
  const directory = useContext(DirectoryContext);
  const { id: here } = useContext(IdentityContext);
  const single = useInbox();
  const hereRoot = useAttentionRoot();
  const multi = !!directory?.multi;
  const sources = useMemo(() => multi ? directory!.shown.filter(row => row.state?.info && (row.id === here || row.health === 'online')) : [], [multi, directory, here]);
  const roots = useMemo(() => sources.map(row => row.id === here ? hereRoot : row.root), [sources, here, hereRoot]);
  const texts = useSeenTexts(roots);
  const merged = useMemo<RunnerInbox>(() => {
    if (!multi) {
      const rows: RunnerRow[] = single.rows.map(row => ({ ...row, runner: here }));
      return { rows, groups: attentionGroups(rows), counts: single.counts, unread: single.unread, byRunner: { [here]: single.counts } };
    }
    const now = Date.now();
    return mergeInboxes(sources.map((row, index) => ({ runner: row.id, sessions: mergeConversations([], row.state!.info!.sessions), seen: parseSeen(texts[index], now) })), here);
  }, [multi, single.rows, single.counts, single.unread, sources, texts, here]);
  const mark = useCallback((entries: { key: string; at: number }[]) => {
    const byRunner = new Map<string, { key: string; at: number }[]>();
    for (const entry of entries) { const { runner, key } = splitKey(entry.key, here); byRunner.set(runner, [...(byRunner.get(runner) || []), { key, at: entry.at }]); }
    for (const [runner, list] of byRunner) {
      if (runner === here) single.mark(list);
      else { const row = directory?.runners.find(value => value.id === runner); if (row) markSeenIn(row.root, list); }
    }
  }, [here, single.mark, directory]);
  // Only a Runner that was answering and stopped: one that is not running is said in Workspaces, not here every day.
  const offline = useMemo(() => multi ? directory!.shown.filter(row => row.id !== here && row.health === 'offline') : [], [multi, directory, here]);
  return { ...merged, multi, here, offline, mark };
}
function attentionGroups(rows: RunnerRow[]) {
  const order: AttentionState[] = ['needs-input', 'failed', 'review', 'working', 'idle'];
  return order.map(state => ({ state, rows: rows.filter(row => row.state === state) })).filter(group => group.rows.length);
}
/** A merged row marked seen up to what it shows. */
export const seenEntry = (row: RunnerRow) => ({ key: row.key, at: seenMark(row) });
/** The conversation key a merged row's actions use in its own Runner. */
export const ownKey = (row: RunnerRow) => conversationKey(row.session);

/**
 * Per-Runner conversation actions for a merged list: one actions object per
 * Runner, made in that Runner's scope (its pins, deletes and dialogs go to
 * it). `get(runner)` is read when the person acts, so it is always current.
 */
export function useRunnerActionHosts<T>(make: () => T, dialogs: (value: T) => ReactNode) {
  const store = useRef(new Map<string, T>());
  const directory = useContext(DirectoryContext);
  const { id: here } = useContext(IdentityContext);
  // The latest `make` and `dialogs`, read on each render of a host; the hosts' component identity stays.
  const latest = useRef({ make, dialogs }); latest.current = { make, dialogs };
  const Host = useMemo(() => function ActionHost({ id }: { id: string }) {
    const value = latest.current.make();
    store.current.set(id, value);
    useEffect(() => () => { store.current.delete(id); }, [id]);
    return <>{latest.current.dialogs(value)}</>;
  }, []);
  // The caller's own Runner acts with its own actions; the others get a host each.
  const hosts = directory?.multi ? <>{directory.shown.filter(row => row.state && row.id !== here).map(row => <RunnerScope key={row.id} runner={row.id}><Host id={row.id}/></RunnerScope>)}</> : null;
  return { get: (runner: string) => store.current.get(runner), hosts };
}

const healthIcon: Record<RunnerHealth, string> = { online: '', connecting: '', offline: 'alert', stopped: 'stop', invalid: 'alert' };
/** A Runner's name on a row of a merged list. */
export function RunnerBadge({ runner, className = '' }: { runner: string; className?: string }) {
  const directory = useContext(DirectoryContext);
  const row = directory?.runners.find(value => value.id === runner);
  if (!directory?.multi || !row) return null;
  return <span className={`runner-badge ${className}`.trim()} data-runner={row.id} data-channel={row.channel.toLowerCase() || undefined} title={`On ${row.label}${row.channel && row.channel !== row.label ? ` (${row.channel})` : ''}`}>{row.label}</span>;
}
/** A Runner's health as a dot and words. */
export function RunnerHealthDot({ health }: { health: RunnerHealth }) {
  return <span className="runner-health" data-health={health} title={healthLabel[health]}>{healthIcon[health] ? <Icon name={healthIcon[health]} size={10}/> : <i/>}<span className="sr-only">{healthLabel[health]}</span></span>;
}
