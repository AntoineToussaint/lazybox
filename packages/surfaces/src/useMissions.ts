import { useCallback, useEffect, useMemo, useRef, useSyncExternalStore } from 'react';
import { ConnectError, type RunnerClient } from '@lazybox/client-core';
import type { Mission } from '@lazybox/protocol';
import { useRunner } from './RunnerContext';

type Snapshot = { missions: Mission[]; loaded: boolean; error: string };
const empty: Snapshot = { missions: [], loaded: false, error: '' };
const POLL_MS = 4000;

/**
 * Every mission of one Runner, polled while anything shows them (the
 * Missions panel, the inbox). One store per Runner client, so the panel and
 * the inbox read the same answer and a change made in one shows in the other
 * at once. Statuses are the Runner's: they are derived there, from the log and
 * live observations, and change without any event (an agent starts running),
 * which is why this polls.
 */
class MissionStore {
  private snapshot: Snapshot = empty;
  readonly details = new Map<string, Mission>();
  private listeners = new Set<() => void>();
  private timer?: ReturnType<typeof setInterval>;
  private reading = false;
  constructor(readonly api: RunnerClient) {}
  get = () => this.snapshot;
  subscribe = (listener: () => void) => {
    this.listeners.add(listener);
    if (this.listeners.size === 1) this.start();
    return () => { this.listeners.delete(listener); if (!this.listeners.size) this.stop(); };
  };
  private emit() { for (const listener of this.listeners) listener(); }
  private set(next: Partial<Snapshot>) { this.snapshot = { ...this.snapshot, ...next }; this.emit(); }
  private start() {
    void this.refresh();
    this.timer = setInterval(() => { if (document.visibilityState === 'visible') void this.refresh(); }, POLL_MS);
    window.addEventListener('focus', this.refreshNow);
  }
  private stop() { clearInterval(this.timer); window.removeEventListener('focus', this.refreshNow); }
  private refreshNow = () => { void this.refresh(); };
  async refresh() {
    if (this.reading) return; this.reading = true;
    const before = new Set(this.snapshot.missions);
    try {
      const list = await this.api.listMissions({}, { timeoutMs: 8000 });
      // A mutation can finish while the list request is in flight. Keep its
      // result until a list started after the mutation can supersede it.
      const missions = new Map(list.missions.map(mission => [mission.id, mission]));
      for (const mission of this.snapshot.missions) {
        const listed = missions.get(mission.id);
        if (!before.has(mission) && (!listed || listed.seq <= mission.seq)) missions.set(mission.id, mission);
      }
      this.set({ missions: [...missions.values()], loaded: true, error: '' });
    }
    catch (error) { this.set({ loaded: true, error: ConnectError.from(error).rawMessage || String(error) }); }
    finally { this.reading = false; }
  }
  /** A mission the Runner just returned (a change, a detailed read) replaces the listed one. */
  put(mission: Mission) {
    const previous = this.snapshot.missions.find(m => m.id === mission.id);
    if (previous && previous.seq > mission.seq) return;
    if (mission.events.length) this.details.set(mission.id, mission);
    const listed: Mission = { ...mission, events: [] };
    this.set({ missions: previous ? this.snapshot.missions.map(m => m.id === mission.id ? listed : m) : [listed, ...this.snapshot.missions] });
  }
}

const stores = new WeakMap<RunnerClient, MissionStore>();
function storeFor(api: RunnerClient) {
  let store = stores.get(api);
  if (!store) { store = new MissionStore(api); stores.set(api, store); }
  return store;
}

/** One typed change, as the client takes it. */
export type ChangeInput = NonNullable<Parameters<RunnerClient['updateMission']>[0]['change']>;

/** The Runner's missions, live, and the changes a person makes to them. */
export function useMissions() {
  const { api } = useRunner();
  const store = storeFor(api);
  const snapshot = useSyncExternalStore(store.subscribe, store.get);
  const pendingCreate = useRef<{ api: RunnerClient; title: string; brief: string; key: string } | undefined>(undefined);
  const create = useCallback(async (title: string, brief: string) => {
    let attempt = pendingCreate.current;
    if (!attempt || attempt.api !== api || attempt.title !== title || attempt.brief !== brief) {
      attempt = { api, title, brief, key: crypto.randomUUID() };
      pendingCreate.current = attempt;
    }
    const mission = await api.createMission({ requestKey: attempt.key, title, brief }, { timeoutMs: 10000 });
    if (pendingCreate.current === attempt) pendingCreate.current = undefined;
    store.put(mission); return mission;
  }, [api, store]);
  // An acknowledgement can be lost after the Runner writes the event. Keep
  // that operation id until success; a later intentional repeat gets a new id.
  const pendingChanges = useMemo(() => new Map<string, string>(), [api]);
  const update = useCallback(async (missionId: string, change: ChangeInput, operationId?: string) => {
    const signature = JSON.stringify([missionId, change], (_, value) => typeof value === 'bigint' ? value.toString() : value);
    const key = operationId || pendingChanges.get(signature) || crypto.randomUUID();
    pendingChanges.set(signature, key);
    const mission = await api.updateMission({ missionId, operationId: key, change }, { timeoutMs: 30000 });
    if (pendingChanges.get(signature) === key) pendingChanges.delete(signature);
    store.put(mission); return mission;
  }, [api, store, pendingChanges]);
  return { ...snapshot, refresh: () => store.refresh(), create, update, put: (mission: Mission) => store.put(mission) };
}

/** One mission in detail (with its history), refreshed while shown; asks the Runner to refresh pull requests now and then. */
export function useMissionDetail(missionId: string, put: (mission: Mission) => void) {
  const { api } = useRunner();
  const store = storeFor(api);
  const snapshot = useSyncExternalStore(store.subscribe, store.get);
  useEffect(() => {
    if (!missionId) return;
    let alive = true, ticks = 0;
    const read = async () => {
      try { const mission = await api.getMission({ missionId, refreshEvidence: ticks++ % 6 === 0 }, { timeoutMs: 8000 }); if (alive) put(mission); }
      catch { /* the list keeps the last answer */ }
    };
    void read();
    const timer = setInterval(() => { if (document.visibilityState === 'visible') void read(); }, 5000);
    return () => { alive = false; clearInterval(timer); };
  }, [api, missionId]);
  const listed = snapshot.missions.find(m => m.id === missionId);
  const detail = store.details.get(missionId);
  // The list is fresher for the board; the detail adds the history.
  return listed ? { ...listed, events: detail?.events || [], messages: detail?.messages || [] } as Mission : detail;
}
