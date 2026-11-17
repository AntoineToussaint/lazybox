import { useEffect, useSyncExternalStore } from 'react';
import { ConnectError, type RunnerClient } from '@lazybox/client-core';
import type { Extensions } from '@lazybox/protocol';
import { useRunner } from './RunnerContext';

/**
 * The person's extensions as the Runner last scanned them (ListExtensions),
 * shared by every panel and Settings in the window. Polled every two seconds
 * while anything shows them and the window is visible, so a saved file or a
 * new folder shows within a moment; at once when the window regains focus.
 */
type Store = { value?: Extensions; error: string; listeners: Set<() => void>; timer?: ReturnType<typeof setTimeout>; loading?: Promise<void> };
const stores = new WeakMap<RunnerClient, Store>();
const POLL_MS = 2000;

function storeFor(api: RunnerClient): Store {
  let store = stores.get(api);
  if (!store) { store = { error: '', listeners: new Set() }; stores.set(api, store); }
  return store;
}

function publish(store: Store, value: Extensions | undefined, error: string) {
  if (value) store.value = value;
  store.error = error;
  for (const listener of store.listeners) listener();
}

async function load(api: RunnerClient, store: Store) {
  if (store.loading) return store.loading;
  store.loading = (async () => {
    try { publish(store, await api.listExtensions({}, { timeoutMs: 5000 }), ''); }
    catch (error) { publish(store, undefined, ConnectError.from(error).rawMessage || String(error)); }
    finally { store.loading = undefined; }
  })();
  return store.loading;
}

function schedule(api: RunnerClient, store: Store) {
  clearTimeout(store.timer);
  if (!store.listeners.size) return;
  store.timer = setTimeout(async () => {
    if (document.visibilityState !== 'hidden') await load(api, store);
    schedule(api, store);
  }, POLL_MS);
}

export type ExtensionsStore = {
  value?: Extensions;
  error: string;
  refresh(): Promise<void>;
  /** Runs a change and adopts the list it returns; resolves to the error text, or ''. */
  change(run: (api: RunnerClient) => Promise<Extensions>): Promise<string>;
};

export function useExtensions(): ExtensionsStore {
  const { api } = useRunner();
  const store = storeFor(api);
  const snapshot = useSyncExternalStore(listener => {
    store.listeners.add(listener);
    if (store.listeners.size === 1) { void load(api, store); schedule(api, store); }
    return () => { store.listeners.delete(listener); if (!store.listeners.size) clearTimeout(store.timer); };
  }, () => store.value);
  const error = useSyncExternalStore(listener => { store.listeners.add(listener); return () => store.listeners.delete(listener); }, () => store.error);
  useEffect(() => {
    const focus = () => void load(api, store);
    window.addEventListener('focus', focus);
    return () => window.removeEventListener('focus', focus);
  }, [api, store]);
  return {
    value: snapshot, error,
    refresh: () => load(api, store),
    async change(run) {
      try { publish(store, await run(api), ''); return ''; }
      catch (error) { const message = ConnectError.from(error).rawMessage || String(error); return message; }
    },
  };
}
