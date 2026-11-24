/**
 * Whether sending a review's findings closes its reviewer: on unless the
 * person turned it off (the review tray's checkbox). The Runner closes only a
 * reviewer whose review is over and whose findings are all sent or
 * dismissed (SetReviewDisposition close_reviewers, agent/types.go).
 * Every send reads it: the tray's and the review card's in the author's chat.
 */
import { useSyncExternalStore } from 'react';
import { PREFERENCE_EVENT, readPreference, writePreference } from './view-state';

const key = 'review-close-reviewers:v1';

export function closeReviewersOnSend(): boolean {
  try { return readPreference(undefined, key) !== 'off'; } catch { return true; }
}

export function setCloseReviewersOnSend(on: boolean): void {
  try { writePreference(undefined, key, on ? 'on' : 'off'); } catch { /* this send still follows the box */ }
}

const subscribe = (change: () => void) => {
  window.addEventListener(PREFERENCE_EVENT, change);
  window.addEventListener('storage', change);
  return () => { window.removeEventListener(PREFERENCE_EVENT, change); window.removeEventListener('storage', change); };
};
export function useCloseReviewersOnSend(): [boolean, (on: boolean) => void] {
  return [useSyncExternalStore(subscribe, closeReviewersOnSend), setCloseReviewersOnSend];
}
