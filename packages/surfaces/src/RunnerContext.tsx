import { createContext, useCallback, useContext, useEffect, useLayoutEffect, useMemo, useRef, useState, useSyncExternalStore, type ReactNode } from 'react';
import { Code, ConnectError, RunnerLink, type RunnerAuthority, type RunnerClient } from '@lazybox/client-core';
import type { InfoResponse } from '@lazybox/protocol';

/**
 * The window's Runner. `renew` rereads where the Runner serves now (the
 * desktop's endpoint file): a restarted Runner has a new bearer and may have a
 * new port, and every request of the window follows it without a reload.
 */
export type RunnerConnection = { baseUrl: string; token: string; fetch?: typeof globalThis.fetch; renew?(): Promise<RunnerAuthority | undefined> };
/** A restart the person asked for, from request until a new Runner process serves. */
export type RunnerRestart = { phase: 'idle' } | { phase: 'restarting' } | { phase: 'restarted' } | { phase: 'failed'; message: string };
export type RunnerState = { connection: RunnerConnection; api: RunnerClient; info?: InfoResponse; connecting: boolean; error: string; authFailed: boolean; refresh(): void; revision: number; restart: RunnerRestart; restartRunner(): Promise<void>; dismissRestart(): void };
const Context = createContext<RunnerState | null>(null);

// The launcher waits up to three minutes for a Runner to serve; Dev may also
// rebuild it first. Past this, say so rather than spin forever.
export const RESTART_TIMEOUT_MS = 240_000;
// Outcomes that mean the Runner refused: it is not restarting.
const refused = [Code.FailedPrecondition, Code.PermissionDenied, Code.Unauthenticated, Code.Unimplemented, Code.Internal];

/** One resolved authority per window. Panel lifetimes never own execution. */
export function RunnerProvider({ connection, expectRestart, children }: { connection: RunnerConnection; expectRestart?: () => Promise<void>; children: ReactNode }) {
  const state = useRunnerState(connection, expectRestart);
  return <Context.Provider value={state}>{children}</Context.Provider>;
}

/**
 * Everything under it talks to the Runner `state` belongs to: a section of the
 * sidebar, a row of the merged inbox or the whole page of another Runner of
 * the window (RunnerDirectory.tsx). Every `useRunner()` call below reads it.
 */
export function RunnerStateProvider({ state, children }: { state: RunnerState; children: ReactNode }) {
  return <Context.Provider value={state}>{children}</Context.Provider>;
}

/**
 * One Runner's connection: its link (bearer, renewal, restart following), its
 * info polled every 3 s, and restarts the person asked for. A window holds one
 * for its own Runner (RunnerProvider) and one per other Runner it shows.
 */
export function useRunnerState(connection: RunnerConnection, expectRestart?: () => Promise<void>): RunnerState {
  // One link per window: clients made before a restart (caches, open streams)
  // send the current bearer too, so nothing keeps calling with a stale one.
  const renew = useRef(connection.renew); renew.current = connection.renew;
  const link = useMemo(() => new RunnerLink({ baseUrl: connection.baseUrl, token: connection.token }, { fetch: connection.fetch, renew: () => renew.current ? renew.current() : Promise.resolve(undefined) }), [connection.fetch]);
  // Before any child's effect sends a request with the authority it replaces.
  useLayoutEffect(() => { link.adopt({ baseUrl: connection.baseUrl, token: connection.token }); }, [link, connection.baseUrl, connection.token]);
  const version = useSyncExternalStore(link.subscribe, () => link.version);
  const authFailure = useSyncExternalStore(link.subscribe, () => link.authFailure);
  // A new client per authority re-runs the effects that load once, so whatever
  // failed while the Runner was away is fetched again from its successor.
  const api = useMemo(() => link.client(), [link, version]);
  const [info, setInfo] = useState<InfoResponse>();
  const [connecting, setConnecting] = useState(true);
  const [error, setError] = useState('');
  const [revision, setRevision] = useState(0);
  const [restart, setRestart] = useState<RunnerRestart>({ phase: 'idle' });
  // The process being replaced and when to give up. A new connection (the
  // desktop adopting the restarted Runner's port and bearer) keeps it.
  const pending = useRef<{ previous: string; deadline: number } | null>(null);
  const latest = useRef<InfoResponse | undefined>(undefined);
  const refresh = useCallback(() => setRevision(value => value + 1), []);
  // What the window knew (its Runner's info) is kept across a restart: it is
  // replaced by the successor's answer, never cleared while waiting for it.
  const restarting = restart.phase === 'restarting';
  useEffect(() => {
    const control = new AbortController();
    let updating = false;
    const update = async () => {
      if (updating) return; updating = true;
      try {
        const next = await api.info({}, { signal: control.signal, timeoutMs:5000 });
        if (!control.signal.aborted) {
          latest.current = next; setInfo(next); setError('');
          const waiting = pending.current;
          if (waiting && next.instanceId && next.instanceId !== waiting.previous) { pending.current = null; setRestart({ phase: 'restarted' }); }
        }
      }
      catch (error) { if (!control.signal.aborted) setError(String(error)); }
      finally {
        updating = false;
        if (!control.signal.aborted) {
          setConnecting(false);
          if (pending.current && Date.now() > pending.current.deadline) {
            pending.current = null;
            setRestart({ phase: 'failed', message: 'The Runner did not come back within 4 minutes. Check the terminal that launched it. Agents keep running and reconnect once a Runner starts.' });
          }
        }
      }
    };
    // Poll faster while a restart is under way, so the window reconnects promptly.
    void update(); const timer = setInterval(() => void update(), restarting ? 1000 : 3000);
    return () => { control.abort(); clearInterval(timer); };
  }, [api, revision, restarting]);
  const restartRunner = useCallback(async () => {
    if (pending.current) return;
    const waiting = { previous: latest.current?.instanceId || '', deadline: Date.now() + RESTART_TIMEOUT_MS };
    pending.current = waiting;
    setRestart({ phase: 'restarting' });
    const fail = (message: string) => { if (pending.current === waiting) pending.current = null; setRestart({ phase: 'failed', message }); };
    try { await expectRestart?.(); }
    catch (error) { fail(`This window cannot follow a restarted Runner: ${String(error)}`); return; }
    try {
      const response = await api.restartRunner({}, { timeoutMs: 10000 });
      if (response.instanceId) waiting.previous = response.instanceId;
    } catch (error) {
      const failure = ConnectError.from(error);
      // Anything else means the answer was lost, not refused: the Runner may
      // already be stopping. Keep waiting for a new process to serve.
      if (refused.includes(failure.code)) { fail(failure.rawMessage || String(error)); return; }
    }
  }, [api, expectRestart]);
  const dismissRestart = useCallback(() => setRestart(value => value.phase === 'restarting' ? value : { phase: 'idle' }), []);
  useEffect(() => {
    if (restart.phase !== 'restarted') return;
    const timer = setTimeout(() => setRestart(value => value.phase === 'restarted' ? { phase: 'idle' } : value), 8000);
    return () => clearTimeout(timer);
  }, [restart.phase]);
  // A refused bearer anywhere in the window is a connection fault, not a panel's.
  const shown = error || (authFailure ? `The Runner refused this window's credentials (${authFailure}). Reconnecting…` : '');
  const authFailed = !!authFailure || error.includes('[unauthenticated]');
  return useMemo(() => ({ connection, api, info, connecting, error: shown, authFailed, refresh, revision, restart, restartRunner, dismissRestart }), [connection, api, info, connecting, shown, authFailed, refresh, revision, restart, restartRunner, dismissRestart]);
}

export function useRunner(): RunnerState {
  const state = useContext(Context);
  if (!state) throw new Error('Runner panels require RunnerProvider');
  return state;
}
