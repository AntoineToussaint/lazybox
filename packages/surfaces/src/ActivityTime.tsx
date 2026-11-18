import { useState, useSyncExternalStore } from 'react';
import './activity-time.css';

const exactFormat = new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'medium' });
const dateFormat = new Intl.DateTimeFormat(undefined, { dateStyle: 'long' });
const relativeFormat = new Intl.RelativeTimeFormat(undefined, { numeric: 'always', style: 'short' });
const interval = 30_000;
const snapshot = () => Math.floor(Date.now() / interval) * interval;
const listeners = new Set<() => void>();
let timer: ReturnType<typeof setInterval> | undefined;
function subscribe(listener: () => void) {
  listeners.add(listener);
  timer ??= setInterval(() => listeners.forEach(notify => notify()), interval);
  return () => {
    listeners.delete(listener);
    if (!listeners.size) { clearInterval(timer); timer = undefined; }
  };
}

export const useActivityClock = () => useSyncExternalStore(subscribe, snapshot, snapshot);
export const validActivityTime = (at?: number): at is number => typeof at === 'number' && at > 0 && Number.isFinite(new Date(at).getTime());
export const localDay = (at: number) => dateFormat.format(at);
export const exactTime = (at: number) => exactFormat.format(at);

export function relativeTime(at: number, now: number) {
  const minutes = Math.max(0, Math.floor((now - at) / 60_000));
  if (!minutes) return 'Just now';
  if (minutes < 60) return relativeFormat.format(-minutes, 'minute');
  if (minutes < 1440) return relativeFormat.format(-Math.floor(minutes / 60), 'hour');
  return relativeFormat.format(-Math.floor(minutes / 1440), 'day');
}

export function ActivityTime({ at, className = '' }: { at: number; className?: string }) {
  const now = useActivityClock();
  const [exact, setExact] = useState(false);
  const full = exactTime(at);
  return <button className={`activity-time ${className}`} type="button" title={full} aria-label={`Show exact time: ${full}`} aria-pressed={exact} onClick={() => setExact(value => !value)}>
    <time dateTime={new Date(at).toISOString()}>{exact ? full : relativeTime(at, now)}</time>
  </button>;
}
