import type { ViewStateStore } from '@lazybox/platform';
import { readLaunchChoices, readPreference, writePreference } from './view-state';

export const CHECK_IN_FIRST = 'check-in-first-v1';
const key = 'setup.behavior-profile';
export const BEHAVIOR_PROFILES = [
  { id: '', label: 'Default behavior', example: 'I’ll look at the test failure and trace it to the code.' },
  { id: 'balanced-v2', label: 'Balanced', example: 'I’ll reproduce the failure and fix it if the cause is clear. I’ll check with you if the scope changes.' },
  { id: 'hands-on-v2', label: 'Hands-on', example: 'I’ll reproduce the failure, make a reversible fix, and run the relevant tests.' },
  { id: CHECK_IN_FIRST, label: 'Check in first', example: 'I’ll inspect the failure and show you the proposed fix before editing.' },
] as const;

export function behaviorForProvider(value: string, provider: string) {
  return value === CHECK_IN_FIRST && provider !== 'claude' && provider !== 'codex' ? '' : value;
}

export function readDefaultBehavior(store?: ViewStateStore) {
  try { return readPreference(store, key) ?? readLaunchChoices(store).behaviorProfile ?? ''; }
  catch { return ''; }
}

export function saveDefaultBehavior(store: ViewStateStore | undefined, behavior: string) {
  writePreference(store, key, behavior);
}

export function BehaviorOptions({ provider }: { provider?: string }) {
  return <>{BEHAVIOR_PROFILES.filter(profile => !provider || behaviorForProvider(profile.id, provider) === profile.id)
    .map(profile => <option key={profile.id} value={profile.id}>{profile.label}</option>)}</>;
}
