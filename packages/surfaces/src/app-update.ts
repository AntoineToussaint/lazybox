import { useEffect, useLayoutEffect, useSyncExternalStore } from 'react';
import type { AppHost, AvailableUpdate, UpdateErrorKind, ViewStateStore } from '@lazybox/platform';
import { PREFERENCE_EVENT, readPreference, writePreference } from './view-state';
import { automaticCheckDue, offerState, updatePath, readUpdatePreferences, remindValue, updateErrorText, UPDATE_PREFERENCES, type InstallProgress, type OfferState, type UpdatePreferences } from './update-model';

/**
 * One update state for the whole window, shared by the header's notice,
 * Settings → Updates and the install dialog. The window asks the feed on its
 * own only while "Check automatically" is on (every six hours, counted across
 * launches); nothing is downloaded or installed until the person chooses
 * Install. The decisions are in update-model.ts.
 */
export type UpdateState = {
  checking: boolean;
  /** A check answered in this launch (so "up to date" is known, not assumed). */
  checked?: boolean;
  available?: AvailableUpdate;
  /** The last failure, and whether it was a check or an install. */
  error?: { kind?: UpdateErrorKind; text: string; phase: 'check' | 'install' };
  installing?: InstallProgress;
};
export type AppUpdate = UpdateState & { prefs: UpdatePreferences; offer: OfferState };

let state: UpdateState = { checking: false };
let store: ViewStateStore | undefined;
let prefsVersion = 0;
const listeners = new Set<() => void>();
const notify = () => listeners.forEach(l => l());
const set = (next: Partial<UpdateState>) => { state = { ...state, ...next }; notify(); };
const subscribe = (l: () => void) => { listeners.add(l); return () => { listeners.delete(l); }; };

// A preference is a convenience: an unavailable store never breaks the window.
const read = (name: string) => { try { return readPreference(store, name); } catch { return null; } };
const write = (name: string, value: string) => { try { writePreference(store, name, value); } catch { /* kept for this session only */ } prefsVersion++; notify(); };
export const updatePreferences = () => readUpdatePreferences(read);

const failure = (error: unknown, phase: 'check' | 'install'): UpdateState['error'] => {
  const kind = (error as { kind?: UpdateErrorKind })?.kind;
  const detail = error instanceof Error ? error.message : typeof error === 'object' && error && 'message' in error ? String((error as { message: unknown }).message) : String(error);
  return { kind, phase, text: updateErrorText(kind, phase, kind ? '' : detail) };
};

export async function checkForUpdate(app: AppHost | undefined) {
  if (!app?.checkForUpdate || state.checking || state.installing) return;
  set({ checking: true });
  try {
    const available = await app.checkForUpdate();
    write(UPDATE_PREFERENCES.lastChecked, String(Date.now()));
    set({ checking: false, checked: true, available, error: undefined });
  } catch (error) {
    // A failed check is still a check: the next automatic one waits its six hours.
    write(UPDATE_PREFERENCES.lastChecked, String(Date.now()));
    set({ checking: false, error: failure(error, 'check') });
  }
}

export async function installUpdate(app: AppHost | undefined) {
  const version = state.available?.version;
  if (!app?.installUpdate || state.installing || !version) return;
  set({ installing: { phase: 'starting', downloaded: 0, total: 0 }, error: undefined });
  // The next launch reads it to tell whether the update took (Onboarding).
  write(UPDATE_PREFERENCES.installing, version);
  try {
    await app.installUpdate(progress => set({ installing: progress }));
    // The app restarts; nothing after this runs on success.
  } catch (error) {
    write(UPDATE_PREFERENCES.installing, '');
    set({ installing: undefined, error: failure(error, 'install') });
  }
}

/** Never offer this version again; a later one is offered as usual. */
export const skipVersion = (version: string) => write(UPDATE_PREFERENCES.skipped, version);
export const stopSkipping = () => write(UPDATE_PREFERENCES.skipped, '');
/** Quiet for a day, for this version. */
export const remindLater = (version: string) => write(UPDATE_PREFERENCES.remind, remindValue(version, Date.now()));
export const setAutomaticChecks = (on: boolean) => write(UPDATE_PREFERENCES.automatic, on ? 'on' : 'off');
/** The version an interrupted install was going to, read once after a restart. */
export const takeInstalling = () => { const value = read(UPDATE_PREFERENCES.installing) || ''; if (value) write(UPDATE_PREFERENCES.installing, ''); return value; };

let timer: ReturnType<typeof setInterval> | undefined;
let timerApp: AppHost | undefined;
// Once when the window opens (a launch is rare, and what it knew may be stale), then every six hours.
const tick = () => {
  if (!timerApp) return;
  const prefs = updatePreferences();
  if (prefs.automatic && (!state.checked && !state.error || automaticCheckDue(prefs, Date.now()))) void checkForUpdate(timerApp);
};

let snapshot: AppUpdate | undefined, snapshotOf: [UpdateState, number, number] | undefined;
function current(): AppUpdate {
  // A snapshot per state and preference change (and per minute, for Remind me later's end).
  const minute = Math.floor(Date.now() / 60_000);
  if (!snapshot || !snapshotOf || snapshotOf[0] !== state || snapshotOf[1] !== prefsVersion || snapshotOf[2] !== minute) {
    const prefs = updatePreferences();
    snapshot = { ...state, prefs, offer: offerState(state.available, prefs, Date.now()) };
    snapshotOf = [state, prefsVersion, minute];
  }
  return snapshot;
}

/**
 * The update state, with the person's choices. The first caller with an app
 * starts the automatic check, when "Check automatically" is on: once as the
 * window opens, then six hours after the last check (counted across launches).
 */
export function useAppUpdate(app: AppHost | undefined): AppUpdate {
  const value = useSyncExternalStore(subscribe, current, current);
  useEffect(() => {
    // Another window changed a preference (Skip, Remind me later, Check automatically).
    const changed = (event: Event) => { const name = (event as CustomEvent<{ name?: string; key?: string }>).detail; const key = name?.name || name?.key || ''; if (!key || key.includes('updates.')) { prefsVersion++; notify(); } };
    window.addEventListener(PREFERENCE_EVENT, changed);
    const minute = setInterval(notify, 60_000);
    return () => { window.removeEventListener(PREFERENCE_EVENT, changed); clearInterval(minute); };
  }, []);
  useEffect(() => {
    if (!app?.checkForUpdate || timer || timerApp) return;
    timerApp = app;
    // Only a build that updates itself checks: a build from source (make
    // update) or a development build never asks the feed on its own.
    void app.info().then(info => {
      if (timerApp !== app || timer || updatePath(info).kind !== 'updater') return;
      tick();
      timer = setInterval(tick, 60_000);
    }, () => {});
  }, [app]);
  return value;
}

/**
 * Where the person's update choices are kept: the main window's preferences
 * (app-wide, shared with its other windows). Mounted once, by Onboarding.
 */
export function useUpdatePreferencesStore(viewState: ViewStateStore | undefined) {
  useLayoutEffect(() => {
    if (!viewState || store === viewState) return;
    store = viewState; prefsVersion++; notify();
  }, [viewState]);
}

/** Test seam: a fresh store. */
export function resetUpdateState() { state = { checking: false }; store = undefined; prefsVersion++; if (timer) clearInterval(timer); timer = undefined; timerApp = undefined; notify(); }
