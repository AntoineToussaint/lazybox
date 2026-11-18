import { useCallback, useEffect, useRef, useState } from 'react';
import type { RunnerAuthority } from '@lazybox/client-core';
import type { RunnerDirectoryHost, RunnerListing } from '@lazybox/platform';

/** What a window's host reads to find its Runner (the desktop's endpoint file, through the native host). */
export type LocatedRunner = { url: string; token: string };

// How long a requested restart is followed at the faster pace.
const FOLLOW_MS = 300_000;

/**
 * Follow a Runner locator for one window. The locator is reread on an interval
 * (every second while a requested restart is under way) and on demand through
 * `renew`, which the window's Runner link calls when a request is refused or
 * cannot reach the Runner. One read is in flight at a time, so an older read
 * never replaces a newer one. A read naming the same Runner keeps the current
 * value, so nothing rerenders; a restarted Runner (new bearer, possibly a new
 * port) becomes the new value and the window keeps its state.
 */
export function useRunnerLocator<T extends LocatedRunner>(read: () => Promise<T>, { intervalMs = 3000, fastMs = 1000 } = {}) {
  const [value, setValue] = useState<T>();
  const [error, setError] = useState('');
  const source = useRef(read); source.current = read;
  const state = useRef<{ reading?: Promise<T | undefined>; following: number; seen: string }>({ following: 0, seen: '' });
  const reread = useCallback((): Promise<T | undefined> => {
    const current = state.current;
    if (current.reading) return current.reading;
    current.reading = source.current().then(next => {
      const seen = next.url + ' ' + next.token;
      // The restarted Runner is adopted: back to the ordinary pace.
      if (current.seen && current.seen !== seen) current.following = 0;
      current.seen = seen;
      setValue(previous => previous && previous.url === next.url && previous.token === next.token ? previous : next);
      setError('');
      return next;
    }, failure => { setError(String(failure)); return undefined; }).finally(() => { current.reading = undefined; });
    return current.reading;
  }, []);
  useEffect(() => {
    let ticks = 0;
    void reread();
    const step = Math.max(50, Math.min(intervalMs, fastMs));
    const timer = setInterval(() => {
      ticks++;
      if (Date.now() < state.current.following || ticks * step % intervalMs < step) void reread();
    }, step);
    return () => clearInterval(timer);
  }, [reread, intervalMs, fastMs]);
  const renew = useCallback(async (): Promise<RunnerAuthority | undefined> => {
    const next = await reread();
    return next && { baseUrl: next.url, token: next.token };
  }, [reread]);
  const follow = useCallback(() => { state.current.following = Date.now() + FOLLOW_MS; }, []);
  return { value, error, renew, follow };
}

/** What a listing says that matters to the window: a new value only when one of these changes, so a poll that finds the same Runners rerenders nothing. */
export const listingKey = (listings: readonly RunnerListing[]) => JSON.stringify(listings.map(({ id, name, product, channel, source, locator, current, state, problem, url, token, remote }) => [id, name, product, channel, source, locator, current, state, problem, url, token, remote && [remote.host, remote.route, remote.machine, remote.os, remote.fingerprint, remote.state, remote.code, remote.problem, remote.rttMs]]));

/**
 * The Runners a window's host can find (RunnerDirectoryHost.list), reread on
 * an interval and on demand. A listing that cannot be read keeps the last one
 * (the window keeps showing what it knew) and says why. `reread` resolves the
 * fresh listing, so a Runner whose bearer was refused renews from it.
 */
export function useRunnerListing(host: RunnerDirectoryHost | undefined, intervalMs = 4000) {
  const [listings, setListings] = useState<RunnerListing[]>([]);
  const [error, setError] = useState('');
  const [ready, setReady] = useState(!host);
  const reading = useRef<Promise<RunnerListing[]> | undefined>(undefined);
  const latest = useRef(''), source = useRef(host); source.current = host;
  const reread = useCallback((): Promise<RunnerListing[]> => {
    const list = source.current?.list;
    if (!list) return Promise.resolve([]);
    reading.current ??= list().then(next => {
      const valid = Array.isArray(next) ? next.filter(value => value && typeof value.id === 'string' && typeof value.name === 'string') : [];
      const key = listingKey(valid);
      if (key !== latest.current) { latest.current = key; setListings(valid); }
      setError('');
      return valid;
    }, failure => { setError(String(failure)); throw failure; }).finally(() => { reading.current = undefined; setReady(true); });
    return reading.current;
  }, []);
  useEffect(() => {
    if (!host) { setReady(true); return; }
    void reread().catch(() => {});
    const timer = setInterval(() => void reread().catch(() => {}), intervalMs);
    return () => clearInterval(timer);
  }, [host, reread, intervalMs]);
  return { listings, error, ready, reread };
}
