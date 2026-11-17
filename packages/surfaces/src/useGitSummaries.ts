import { useEffect, useSyncExternalStore } from 'react';
import { useRunner } from './RunnerContext';
import { pollFor } from './git-summary-poll';

export { nudgeGitSummaries } from './git-summary-poll';

/** The sidebar's git summaries (git-summary-poll.ts). A change of `nudge` asks the Runner again shortly. */
export function useGitSummaries(nudge = '') {
  const { api } = useRunner();
  const poll = pollFor(api);
  const snapshot = useSyncExternalStore(poll.subscribe, () => poll.snapshot);
  useEffect(() => { if (nudge) poll.nudge(); }, [poll, nudge]);
  return snapshot;
}
