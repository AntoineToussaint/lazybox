import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { AppState } from 'react-native';
import { fetch as expoFetch } from 'expo/fetch';
import {
  ConnectError, DeviceRevoked, RunnerLink, openDeviceSession, pairDevice, parsePairingLink,
  type DeviceRecord, type DeviceSession, type RunnerClient,
} from '@lazybox/client-core';
import type { Collection, InfoResponse } from '@lazybox/protocol';
import { humanError } from '@lazybox/surfaces/models';
import { clearPairing, keyProvider, loadPairing, pairingStore, type Pairing } from './store';

/**
 * The phone's connection to its Runner (REMOTE.md), as the PWA's RemoteApp
 * has it: a pairing kept in the Keychain, a 15-minute device credential
 * renewed 90 s before it ends (and on any refused call, through RunnerLink),
 * the Runner polled every 3 s while the app is in front, and nothing at all
 * while it is not: iOS suspends a backgrounded app's sockets, so streams and
 * polls stop with it and the app reads again when it comes back.
 *
 * Every request goes through `expo/fetch`, whose responses stream: Connect's
 * server streaming (`WatchEvents`) needs that, and React Native's own fetch
 * buffers the whole body (MOBILE_CLOUD_HUB.md, spike S1).
 */
export type Phase =
  | { kind: 'loading' }
  | { kind: 'unpaired' }
  | { kind: 'revoked' }
  | { kind: 'signing-in'; pairing: Pairing }
  | { kind: 'offline'; pairing: Pairing; message: string }
  | { kind: 'ready'; pairing: Pairing; session: DeviceSession };

export type RunnerState = {
  phase: Phase;
  device?: DeviceRecord;
  api?: RunnerClient;
  info?: InfoResponse;
  collections: Collection[];
  /** The last poll's failure, or ''. */
  error: string;
  /** When the Runner last answered (Unix ms). */
  answeredAt: number;
  foreground: boolean;
  refresh(): Promise<void>;
  pair(link: string, name: string): Promise<DeviceRecord>;
  retry(): void;
  forget(): Promise<void>;
};

const RENEW_BEFORE_MS = 90_000, POLL_MS = 3_000, COLLECTIONS_MS = 20_000;
const fetcher = expoFetch as unknown as typeof globalThis.fetch;
const Context = createContext<RunnerState | undefined>(undefined);

export function useRunner(): RunnerState {
  const value = useContext(Context);
  if (!value) throw new Error('useRunner outside RunnerProvider');
  return value;
}

function useForeground(): boolean {
  const [foreground, setForeground] = useState(AppState.currentState !== 'background');
  useEffect(() => { const sub = AppState.addEventListener('change', state => setForeground(state === 'active')); return () => sub.remove(); }, []);
  return foreground;
}

export function RunnerProvider({ children }: { children: ReactNode }) {
  const [phase, setPhase] = useState<Phase>({ kind: 'loading' });
  const foreground = useForeground();
  const signIn = useCallback(async (pairing: Pairing) => {
    setPhase(current => current.kind === 'ready' ? current : { kind: 'signing-in', pairing });
    try { setPhase({ kind: 'ready', pairing, session: await openDeviceSession({ baseUrl: pairing.baseUrl, stored: pairing.stored, fetch: fetcher }) }); }
    catch (error) {
      if (error instanceof DeviceRevoked) { await clearPairing(); setPhase({ kind: 'revoked' }); return; }
      setPhase({ kind: 'offline', pairing, message: error instanceof Error ? error.message : String(error) });
    }
  }, []);
  useEffect(() => { void loadPairing().then(pairing => pairing ? signIn(pairing) : setPhase({ kind: 'unpaired' })); }, [signIn]);

  const ready = phase.kind === 'ready' ? phase : undefined;
  const pairing = ready?.pairing;
  // One link per pairing: clients made from it adopt each renewed credential, open streams included.
  const renewing = useRef<Promise<DeviceSession | undefined> | undefined>(undefined);
  const latest = useRef(phase); latest.current = phase;
  const renew = useCallback(async () => {
    const current = latest.current;
    if (current.kind !== 'ready') return undefined;
    renewing.current ??= openDeviceSession({ baseUrl: current.pairing.baseUrl, stored: current.pairing.stored, fetch: fetcher })
      .then(session => { setPhase({ kind: 'ready', pairing: current.pairing, session }); return session; },
        async error => { if (error instanceof DeviceRevoked) { await clearPairing(); setPhase({ kind: 'revoked' }); } return undefined; })
      .finally(() => { renewing.current = undefined; });
    const next = await renewing.current;
    return next && { baseUrl: current.pairing.baseUrl, token: next.token };
  }, []);
  // A renewed token is adopted by the same link (below), never a new link: open streams keep their client.
  const link = useMemo(() => pairing && ready ? new RunnerLink({ baseUrl: pairing.baseUrl, token: ready.session.token }, { fetch: fetcher, renew }) : undefined, [pairing, renew]);
  useEffect(() => { if (link && ready) link.adopt({ baseUrl: ready.pairing.baseUrl, token: ready.session.token }); }, [link, ready?.session.token]);
  const api = useMemo(() => link?.client(), [link]);

  // Renew ahead of expiry, and at once on return to the front when it is close.
  const expiresAt = ready?.session.expiresAt ?? 0;
  useEffect(() => {
    if (!expiresAt || !foreground) return;
    const timer = setTimeout(() => void renew(), Math.max(0, expiresAt - Date.now() - RENEW_BEFORE_MS));
    return () => clearTimeout(timer);
  }, [expiresAt, foreground, renew]);

  const [info, setInfo] = useState<InfoResponse>();
  const [collections, setCollections] = useState<Collection[]>([]);
  const [error, setError] = useState('');
  const [answeredAt, setAnsweredAt] = useState(0);
  const poll = useCallback(async (signal?: AbortSignal) => {
    if (!api) return;
    try {
      const next = await api.info({}, { signal, timeoutMs: 8000 });
      if (signal?.aborted) return;
      setInfo(next); setError(''); setAnsweredAt(Date.now());
    } catch (failure) {
      if (signal?.aborted) return;
      setError(humanError(ConnectError.from(failure)));
    }
  }, [api]);
  const loadCollections = useCallback(async (signal?: AbortSignal) => {
    if (!api) return;
    try { const result = await api.listCollections({}, { signal, timeoutMs: 10000 }); if (!signal?.aborted) setCollections(result.collections); } catch { /* the next poll says it */ }
  }, [api]);
  useEffect(() => {
    if (!api || !foreground) return;
    const control = new AbortController();
    void poll(control.signal); void loadCollections(control.signal);
    const timer = setInterval(() => void poll(control.signal), POLL_MS);
    const slow = setInterval(() => void loadCollections(control.signal), COLLECTIONS_MS);
    return () => { control.abort(); clearInterval(timer); clearInterval(slow); };
  }, [api, foreground, poll, loadCollections]);
  useEffect(() => { if (!ready) { setInfo(undefined); setCollections([]); setError(''); } }, [!!ready]);

  const refresh = useCallback(async () => { await Promise.all([poll(), loadCollections()]); }, [poll, loadCollections]);
  const pair = useCallback(async (text: string, name: string) => {
    const parsed = parsePairingLink(text);
    if (!parsed) throw new Error('That is not a Lazybox pairing link. On your computer, open Settings → Devices → Pair a phone, and scan or copy its code.');
    const stored = await pairDevice({ baseUrl: parsed.baseUrl, offer: parsed.offer, name, store: pairingStore(parsed.baseUrl), fetch: fetcher, keyProvider });
    void signIn({ baseUrl: parsed.baseUrl, stored });
    return stored.device;
  }, [signIn]);
  const retry = useCallback(() => { const current = latest.current; if (current.kind === 'offline') void signIn(current.pairing); }, [signIn]);
  const forget = useCallback(async () => { await clearPairing(); setPhase({ kind: 'unpaired' }); }, []);

  const device = phase.kind === 'ready' ? phase.session.device : phase.kind === 'signing-in' || phase.kind === 'offline' ? phase.pairing.stored.device : undefined;
  const value: RunnerState = { phase, device, api, info, collections, error, answeredAt, foreground, refresh, pair, retry, forget };
  return <Context.Provider value={value}>{children}</Context.Provider>;
}
