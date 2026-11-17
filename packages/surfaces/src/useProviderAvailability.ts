import { useCallback, useEffect, useRef, useState } from 'react';
import type { ProviderAvailability } from '@lazybox/protocol';
import { useRunner } from './RunnerContext';

/**
 * The Runner's provider availability report (GetProviderAvailability): which
 * agents are installed and which subscriptions can take work. Read when
 * mounted and whenever the Runner's profiles change; `refresh` probes the
 * programs again. A Runner without the RPC (an older one) reports nothing,
 * and the caller falls back to its own choices.
 */
export function useProviderAvailability(enabled = true) {
  const { api, info } = useRunner();
  const [report, setReport] = useState<ProviderAvailability>();
  const [error, setError] = useState('');
  const [checking, setChecking] = useState(false);
  const latest = useRef(0);
  const load = useCallback(async (refresh: boolean) => {
    const call = ++latest.current;
    setChecking(true);
    try {
      const next = await api.getProviderAvailability({ refresh });
      if (call === latest.current) { setReport(next); setError(''); }
    } catch (e) {
      if (call === latest.current) setError(String(e));
    } finally {
      if (call === latest.current) setChecking(false);
    }
  }, [api]);
  // Profiles and their states, as Info reports them: a sign-in, a new
  // subscription or a limit reached elsewhere changes what can be used.
  const profiles = (info?.profiles || []).map(p => `${p.id}:${p.availability?.state || ''}:${p.availability?.resetAt || ''}`).join('|');
  useEffect(() => { if (enabled && info) void load(false); }, [enabled, !!info, profiles, load]);
  return { report, error, checking, refresh: () => load(true) };
}
