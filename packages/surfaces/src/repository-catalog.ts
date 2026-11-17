import { useEffect, useMemo, useSyncExternalStore } from "react";
import type { RunnerClient } from "@lazybox/client-core";
import type { RepositoryCatalog } from "@lazybox/protocol";

type Snapshot = {
  catalog?: RepositoryCatalog;
  loading: boolean;
  refreshing: boolean;
  error: string;
};
/** One observable catalog per connection. UI lifetimes never discard Runner discovery. */
class CatalogReader {
  private value: Snapshot = { loading: true, refreshing: false, error: "" };
  private listeners = new Set<() => void>();
  private pending?: Promise<void>;
  private lastRefresh = 0;
  constructor(private api: RunnerClient) {}
  getSnapshot = () => this.value;
  subscribe = (listener: () => void) => {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  };
  private publish(value: Partial<Snapshot>) {
    this.value = { ...this.value, ...value };
    this.listeners.forEach((listener) => listener());
  }
  load = (refresh = false): Promise<void> => {
    if (this.pending) return this.pending;
    this.publish({
      loading: !this.value.catalog,
      refreshing: refresh,
      error: "",
    });
    this.pending = (async () => {
      try {
        const catalog = await this.api.getRepositoryCatalog(
          { refresh },
          { timeoutMs: 100000 },
        );
        if (refresh) this.lastRefresh = Date.now();
        this.publish({ catalog });
      } catch (error) {
        this.publish({ error: String(error) });
      } finally {
        this.publish({ loading: false, refreshing: false });
        this.pending = undefined;
      }
    })();
    return this.pending;
  };
  open = async () => {
    await this.load();
    await this.refreshIfStale();
  };
  refreshIfStale = async () => {
    const c = this.value.catalog,
      source = c?.sources.find((s) => s.id === "github");
    if (
      c?.refreshSeconds &&
      source?.state === "connected" &&
      Date.now() - Number(source.updatedAt) * 1000 > c.refreshSeconds * 1000 &&
      Date.now() - this.lastRefresh > c.refreshSeconds * 1000
    )
      await this.load(true);
  };
}
const readers = new WeakMap<RunnerClient, CatalogReader>();
export function repositoryCatalog(api: RunnerClient) {
  let value = readers.get(api);
  if (!value) {
    value = new CatalogReader(api);
    readers.set(api, value);
  }
  return value;
}
export function useRepositoryCatalog(api: RunnerClient) {
  const reader = useMemo(() => repositoryCatalog(api), [api]);
  const state = useSyncExternalStore(reader.subscribe, reader.getSnapshot);
  useEffect(() => {
    void reader.open();
    const timer = setInterval(() => {
      void reader.open();
    }, 60000);
    return () => clearInterval(timer);
  }, [reader]);
  return { ...state, reload: reader.load };
}
