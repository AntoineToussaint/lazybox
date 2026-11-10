import { createContext, useCallback, useContext, useEffect, useMemo, useState, type ReactNode } from 'react';
import { createRunnerClient, type RunnerClient } from '@lazybox/client-core';
import type { InfoResponse } from '@lazybox/protocol';

export type RunnerConnection = { baseUrl: string; token: string; fetch?: typeof globalThis.fetch };
type RunnerState = { connection: RunnerConnection; api: RunnerClient; info?: InfoResponse; connecting: boolean; error: string; refresh(): void; revision: number };
const Context = createContext<RunnerState | null>(null);

/** One resolved authority per window. Panel lifetimes never own execution. */
export function RunnerProvider({ connection, children }: { connection: RunnerConnection; children: ReactNode }) {
  const api = useMemo(() => createRunnerClient(connection.baseUrl, connection.token, connection.fetch), [connection.baseUrl, connection.token, connection.fetch]);
  const [info, setInfo] = useState<InfoResponse>();
  const [connecting, setConnecting] = useState(true);
  const [error, setError] = useState('');
  const [revision, setRevision] = useState(0);
  const refresh = useCallback(() => setRevision(value => value + 1), []);
  useEffect(() => { setConnecting(true); setInfo(undefined); setError(''); }, [api]);
  useEffect(() => {
    const control = new AbortController();
    if (!connection.token) { setError('A resolved Runner connection is required.'); setConnecting(false); return () => control.abort(); }
    let updating = false;
    const update = async () => {
      if (updating) return; updating = true;
      try { const next = await api.info({}, { signal: control.signal, timeoutMs:5000 }); if (!control.signal.aborted) { setInfo(next); setError(''); } }
      catch (error) { if (!control.signal.aborted) setError(String(error)); }
      finally { updating = false; if (!control.signal.aborted) setConnecting(false); }
    };
    void update(); const timer = setInterval(() => void update(), 3000);
    return () => { control.abort(); clearInterval(timer); };
  }, [api, connection.token, revision]);
  return <Context.Provider value={{ connection, api, info, connecting, error, refresh, revision }}>{children}</Context.Provider>;
}

export function useRunner(): RunnerState {
  const state = useContext(Context);
  if (!state) throw new Error('Runner panels require RunnerProvider');
  return state;
}
