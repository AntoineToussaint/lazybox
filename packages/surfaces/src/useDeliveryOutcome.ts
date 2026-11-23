import { useEffect, useState } from 'react';
import { Code, ConnectError } from '@lazybox/client-core';
import type { DeliveryOutcome } from '@lazybox/protocol';
import { useRunner } from './RunnerContext';

export type DeliveryLookup = { sessionId: string } | { missionId: string; taskId: string };

const POLL_MS = 10_000;

/**
 * The delivery outcome of a conversation or a Mission task, from the one
 * projection the Runner serves for both: the card, the inbox and the
 * conversation say the same thing. undefined while loading, null when nothing
 * names what to deliver. It refreshes when refresh changes (a turn ending)
 * and every 10 s while visible.
 */
export function useDeliveryOutcome(lookup: DeliveryLookup | undefined, refresh?: unknown): DeliveryOutcome | null | undefined {
  const { api } = useRunner();
  const [outcome, setOutcome] = useState<DeliveryOutcome | null | undefined>(undefined);
  const key = !lookup ? '' : 'sessionId' in lookup ? `s:${lookup.sessionId}` : `t:${lookup.missionId}/${lookup.taskId}`;
  useEffect(() => {
    if (!api || !lookup) { setOutcome(null); return; }
    let live = true;
    const load = async () => {
      if (document.visibilityState === 'hidden') return;
      try {
        const o = await api.getDeliveryOutcome('sessionId' in lookup ? { sessionId: lookup.sessionId } : { missionId: lookup.missionId, taskId: lookup.taskId }, { timeoutMs: 8000 });
        if (live) setOutcome(o);
      } catch (e) {
        const code = ConnectError.from(e).code;
        // No contract, or a Runner that serves no outcomes: nothing to show.
        if (live && (code === Code.FailedPrecondition || code === Code.Unimplemented || code === Code.NotFound)) setOutcome(null);
      }
    };
    void load();
    const timer = setInterval(() => void load(), POLL_MS);
    return () => { live = false; clearInterval(timer); };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [api, key, refresh]);
  return outcome;
}
