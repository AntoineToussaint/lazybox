import { useEffect, useRef } from 'react';
import type { ViewTarget } from './workspace-model';

/**
 * Requests that cross panels or windows:
 * - a detached window (a chat on its own) asks the main window to show a
 *   conversation or workspace. Windows of one Runner share localStorage, and a
 *   write raises a `storage` event in every other window;
 * - the palette asks the Missions panel to show a mission, now or once mounted.
 */
const navigateKey = (root: string) => `${root}:navigate-request`;
const fresh = 10_000;

/** Where another window asks this one to go: a conversation or workspace, or an artifact of a workspace. */
export type NavigationRequest = ViewTarget & { artifactId?: string };

export function requestNavigation(root: string, target: NavigationRequest) {
  try { localStorage.setItem(navigateKey(root), JSON.stringify({ ...target, at: Date.now(), nonce: crypto.randomUUID() })); } catch { /* storage unavailable: nothing to deliver it */ }
}
/** The main window: show what another window of this Runner asked for. */
export function useNavigationRequests(root: string, enabled: boolean, show: (target: NavigationRequest) => void) {
  const ref = useRef(show); ref.current = show;
  useEffect(() => {
    if (!enabled || !root) return;
    const listener = (event: StorageEvent) => {
      if (event.key !== navigateKey(root) || !event.newValue) return;
      try {
        const value = JSON.parse(event.newValue);
        if (typeof value?.collectionId !== 'string' || typeof value.sessionId !== 'string' || !(Date.now() - value.at < fresh)) return;
        ref.current({ collectionId: value.collectionId, sessionId: value.sessionId, commandId: typeof value.commandId === 'string' ? value.commandId : '', ...(typeof value.artifactId === 'string' ? { artifactId: value.artifactId } : {}) });
      } catch { /* a malformed request is ignored */ }
    };
    window.addEventListener('storage', listener);
    return () => window.removeEventListener('storage', listener);
  }, [root, enabled]);
}

const PAGE = 'lazybox-missions-page';
/** Ask the workspace to show the missions board as a page of its own. */
export function showMissionsPage() { window.dispatchEvent(new CustomEvent(PAGE)); }
/** The workspace: open (or bring forward) the Missions page when asked. */
export function useMissionsPageHost(show: () => void) {
  const ref = useRef(show); ref.current = show;
  useEffect(() => {
    const listener = () => ref.current();
    window.addEventListener(PAGE, listener);
    return () => window.removeEventListener(PAGE, listener);
  }, []);
}

const SELECT = 'lazybox-select-mission';
let pendingMission = '';
/** Ask the Missions panel to show a mission; kept until a panel takes it. */
export function selectMission(missionId: string) {
  pendingMission = missionId;
  window.dispatchEvent(new CustomEvent(SELECT, { detail: missionId }));
}
/** The Missions panel: the mission asked for, now or once mounted. */
export function useMissionSelection(select: (missionId: string) => void) {
  const ref = useRef(select); ref.current = select;
  useEffect(() => {
    if (pendingMission) { ref.current(pendingMission); pendingMission = ''; }
    const listener = (event: Event) => { pendingMission = ''; ref.current((event as CustomEvent<string>).detail); };
    window.addEventListener(SELECT, listener);
    return () => window.removeEventListener(SELECT, listener);
  }, []);
}
