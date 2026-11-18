import { useSyncExternalStore } from 'react';
import type { ViewStateStore } from '@lazybox/platform';
import type { ActivityDetail } from './tool-lines';
import { PREFERENCE_EVENT, readPreference, writePreference } from './view-state';

export const DISPLAY_PRESETS = [
  { id: 'focused', label: 'Focused', description: 'Keep the current work and anything that needs you in view.', activity: 'quiet', workspaceDetails: false, timestamps: false },
  { id: 'standard', label: 'Standard', description: 'See workspace and agent status with grouped tool activity.', activity: 'smart', workspaceDetails: true, timestamps: false },
  { id: 'power', label: 'Power user', description: 'Show each tool call, workspace details, and reply times.', activity: 'verbose', workspaceDetails: true, timestamps: true },
] as const satisfies readonly { id: string; label: string; description: string; activity: ActivityDetail; workspaceDetails: boolean; timestamps: boolean }[];

export type DisplayPreset = typeof DISPLAY_PRESETS[number]['id'];
const key = 'display-detail';
type DisplayDetail = { preset: DisplayPreset; workspaceDetails?: boolean; timestamps?: boolean };
const fallback: DisplayDetail = { preset: 'standard' };

function read(store?: ViewStateStore): DisplayDetail {
  try {
    const value = JSON.parse(readPreference(store, key) || '{}');
    if (!DISPLAY_PRESETS.some(preset => preset.id === value?.preset)) return fallback;
    return { preset: value.preset, workspaceDetails: typeof value.workspaceDetails === 'boolean' ? value.workspaceDetails : undefined,
      timestamps: typeof value.timestamps === 'boolean' ? value.timestamps : undefined };
  } catch { return fallback; }
}

function subscribe(callback: () => void) {
  window.addEventListener(PREFERENCE_EVENT, callback);
  window.addEventListener('storage', callback);
  return () => { window.removeEventListener(PREFERENCE_EVENT, callback); window.removeEventListener('storage', callback); };
}

export function useDisplayDetail(store?: ViewStateStore) {
  const encoded = useSyncExternalStore(subscribe, () => JSON.stringify(read(store)), () => JSON.stringify(fallback));
  const state: DisplayDetail = JSON.parse(encoded);
  const preset = DISPLAY_PRESETS.find(choice => choice.id === state.preset) || DISPLAY_PRESETS[1];
  return {
    preset: preset.id,
    workspaceDetails: state.workspaceDetails ?? preset.workspaceDetails,
    timestamps: state.timestamps ?? preset.timestamps,
    save: (next: DisplayDetail) => writePreference(store, key, JSON.stringify(next)),
    customize: (change: Pick<DisplayDetail, 'workspaceDetails' | 'timestamps'>) => writePreference(store, key, JSON.stringify({ ...read(store), ...change })),
  };
}
