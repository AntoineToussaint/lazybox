import { useCallback, useEffect, useRef, useState } from 'react';
import type { UserActions } from '@lazybox/protocol';
import { useRunner } from './RunnerContext';

/** How often the actions file is looked at: the Runner rereads it every second. */
export const ACTIONS_POLL_MS = 2000;

/**
 * The actions file as the Runner last loaded it, kept current: polled while
 * mounted and at once when the window regains focus. A new revision replaces
 * the value; an unchanged one keeps it, so menus do not re-render for nothing.
 */
export function useUserActionsFile(): { file?: UserActions; error: string; refresh(): void; replace(next: UserActions): void } {
  const { api } = useRunner();
  const [file, setFile] = useState<UserActions>();
  const [error, setError] = useState('');
  const revision = useRef(-1n);
  const [tick, setTick] = useState(0);
  const refresh = useCallback(() => setTick(value => value + 1), []);
  const replace = useCallback((next: UserActions) => { revision.current = next.revision; setFile(next); setError(''); }, []);
  useEffect(() => {
    const control = new AbortController();
    let busy = false;
    const load = async () => {
      if (busy) return; busy = true;
      try {
        const next = await api.getUserActions({}, { signal: control.signal, timeoutMs: 5000 });
        if (control.signal.aborted) return;
        setError('');
        if (next.revision !== revision.current) { revision.current = next.revision; setFile(next); }
      } catch (e) { if (!control.signal.aborted) setError(String(e)); }
      finally { busy = false; }
    };
    void load();
    const timer = setInterval(() => { if (document.visibilityState !== 'hidden') void load(); }, ACTIONS_POLL_MS);
    const focus = () => void load();
    window.addEventListener('focus', focus);
    return () => { control.abort(); clearInterval(timer); window.removeEventListener('focus', focus); };
  }, [api, tick]);
  return { file, error, refresh, replace };
}
