import { useCallback, useEffect, useMemo, useState, useSyncExternalStore } from 'react';
import type { Artifact } from '@lazybox/protocol';
import type { RunnerClient } from '@lazybox/client-core';
import { useRunner } from './RunnerContext';
import { errorText, onArtifactsChanged } from './artifact-client';
import type { FoundArtifact } from './artifact-model';

/** How often a shown list rereads the Runner: a tracked file or an agent may add a version at any time. */
export const ARTIFACT_POLL_MS = 4000;
/** How often the artifacts of workspaces not on screen are reread (the palette, "All workspaces"). */
export const OTHER_ARTIFACTS_POLL_MS = 15000;

/** `artifacts` are the live ones, newest first; `retired` the retired ones, kept apart so no view lists them by mistake. */
type Snapshot = { artifacts: Artifact[]; retired: Artifact[]; loaded: boolean; error: string };
type Entry = {
  snapshot: Snapshot;
  /** Each subscriber's poll interval: the list is reread at the shortest. */
  subscribers: Map<symbol, { interval: number; notify(): void }>;
  timer?: ReturnType<typeof setInterval>;
  every: number;
  busy: boolean;
  again: boolean;
  control?: AbortController;
  load(): void;
};

const EMPTY: Snapshot = { artifacts: [], retired: [], loaded: true, error: '' };
/** What makes a list worth rendering again: a new reply that says the same keeps the old array. */
const signature = (list: readonly Artifact[]) => list.map(a => `${a.id}:${a.updatedAtUnixMillis}:${a.versions.length}:${a.title}:${a.kind}:${a.trackedPath}:${a.trackingProblem}:${retiredStamp(a)}`).join('|');
/** What changes when an artifact is retired, re-pointed or restored. */
export const retiredStamp = (a: Artifact) => a.retired ? `${a.retired.atUnixMillis}>${a.retired.supersededBy}` : '';

/**
 * One list per workspace per Runner client, shared by everything that shows
 * it (the Artifacts panel, the chat's cards, the header chip, the palette):
 * one request per poll however many views are mounted.
 */
const stores = new WeakMap<RunnerClient, Map<string, Entry>>();
/** The lists someone watches now, for the window-wide reloads. */
const live = new Set<{ key: string; entry: Entry }>();
let watching = false;
function watchWindow() {
  if (watching || typeof window === 'undefined') return;
  watching = true;
  onArtifactsChanged(collectionId => { for (const value of live) if (value.key === collectionId) value.entry.load(); });
  document.addEventListener('visibilitychange', () => { if (document.visibilityState === 'visible') for (const value of live) value.entry.load(); });
}

function entryFor(api: RunnerClient, collectionId: string): Entry {
  let byWorkspace = stores.get(api);
  if (!byWorkspace) stores.set(api, byWorkspace = new Map());
  let entry = byWorkspace.get(collectionId);
  if (entry) return entry;
  const created: Entry = {
    snapshot: { artifacts: [], retired: [], loaded: false, error: '' }, subscribers: new Map(), every: 0, busy: false, again: false,
    load() {
      if (!created.subscribers.size || document.visibilityState === 'hidden') return;
      if (created.busy) { created.again = true; return; }
      created.busy = true;
      const control = created.control = new AbortController();
      const publish = (next: Snapshot) => { created.snapshot = next; for (const s of created.subscribers.values()) s.notify(); };
      // Retired ones too, in the same call: the panel offers them ("Show retired"), and a page names what superseded it.
      api.listArtifacts({ collectionId, includeRetired: true }, { signal: control.signal, timeoutMs: 10000 }).then(result => {
        if (control.signal.aborted) return;
        const previous = created.snapshot;
        const same = previous.loaded && !previous.error && signature([...previous.artifacts, ...previous.retired]) === signature(result.artifacts);
        if (!same) publish({ artifacts: result.artifacts.filter(a => !a.retired), retired: result.artifacts.filter(a => a.retired), loaded: true, error: '' });
      }, error => { if (!control.signal.aborted) publish({ ...created.snapshot, loaded: true, error: errorText(error) }); })
        .finally(() => { created.busy = false; if (created.again && !control.signal.aborted) { created.again = false; created.load(); } });
    },
  };
  byWorkspace.set(collectionId, created);
  return created;
}

function subscribe(api: RunnerClient, collectionId: string, interval: number, notify: () => void) {
  watchWindow();
  const entry = entryFor(api, collectionId), id = Symbol(collectionId), handle = { key: collectionId, entry };
  const first = !entry.subscribers.size;
  entry.subscribers.set(id, { interval, notify });
  if (first) live.add(handle);
  const retime = () => {
    const every = Math.min(...[...entry.subscribers.values()].map(s => s.interval));
    if (every === entry.every && entry.timer) return;
    clearInterval(entry.timer); entry.every = every;
    entry.timer = setInterval(() => entry.load(), every);
  };
  retime();
  // A list already shown elsewhere is current; one nobody watched may be stale.
  if (first || !entry.snapshot.loaded) entry.load();
  return () => {
    entry.subscribers.delete(id);
    if (entry.subscribers.size) { retime(); return; }
    clearInterval(entry.timer); entry.timer = undefined; entry.every = 0;
    entry.control?.abort(); entry.busy = false; entry.again = false;
    for (const value of live) if (value.entry === entry) live.delete(value);
  };
}

/**
 * A workspace's artifacts (no content), and apart its retired ones, kept current while mounted: reread
 * every few seconds while the window is visible, and at once after a write
 * from this window.
 */
export function useArtifacts(collectionId: string, interval = ARTIFACT_POLL_MS) {
  const { api } = useRunner();
  const listen = useCallback((notify: () => void) => collectionId ? subscribe(api, collectionId, interval, notify) : () => {}, [api, collectionId, interval]);
  const snapshot = useSyncExternalStore(listen, () => collectionId ? entryFor(api, collectionId).snapshot : EMPTY);
  const reload = useCallback(() => { if (collectionId) entryFor(api, collectionId).load(); }, [api, collectionId]);
  return { ...snapshot, reload };
}

/**
 * The artifacts of several workspaces, newest first, each with its workspace:
 * the palette's "go to any artifact" and the panel's "All workspaces".
 */
export function useArtifactsOf(collections: readonly { id: string; name: string }[], interval = OTHER_ARTIFACTS_POLL_MS): { items: FoundArtifact[]; loaded: boolean } {
  const { api } = useRunner();
  const ids = collections.filter(c => c.id).map(c => `${c.id}\u0000${c.name}`).join('\u0001');
  const shown = useMemo(() => ids ? ids.split('\u0001').map(entry => { const [id, name] = entry.split('\u0000'); return { id, name }; }) : [], [ids]);
  const read = useCallback(() => {
    const snapshots = shown.map(c => ({ ...c, snapshot: entryFor(api, c.id).snapshot }));
    const items = snapshots.flatMap(c => c.snapshot.artifacts.map(artifact => ({ artifact, collectionId: c.id, workspace: c.name || c.id })))
      .sort((a, b) => Number(b.artifact.updatedAtUnixMillis || 0n) - Number(a.artifact.updatedAtUnixMillis || 0n));
    return { items, loaded: snapshots.every(c => c.snapshot.loaded) };
  }, [api, shown]);
  const [state, setState] = useState(read);
  useEffect(() => {
    setState(read());
    let frame = 0;
    // Several lists answer at once: one render for them.
    const notify = () => { if (!frame) frame = requestAnimationFrame(() => { frame = 0; setState(read()); }); };
    const stops = shown.map(c => subscribe(api, c.id, interval, notify));
    return () => { cancelAnimationFrame(frame); stops.forEach(stop => stop()); };
  }, [api, shown, interval, read]);
  return state;
}
