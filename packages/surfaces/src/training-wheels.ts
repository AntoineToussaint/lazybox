// Only the preference and explicit dismissal survive a window reload.
const preference = 'lazybox:training-wheels';
function read(key: string) {
  try { return globalThis.localStorage?.getItem(key) === 'true'; } catch { return false; }
}
function write(key: string, value: boolean) {
  try { globalThis.localStorage?.setItem(key, String(value)); } catch { /* window-only preference */ }
}
let enabled = read(preference);
let dismissed = read(preference + ':dismissed:maximize');
let uses = 0;
let learned = false;
const listeners = new Set<() => void>();
const notify = () => listeners.forEach(listener => listener());
// Keep opt-out and dismissal consistent when the person has several windows open.
if (typeof window !== 'undefined') window.addEventListener('storage', event => {
  if (event.key === preference || event.key === null) {
    enabled = read(preference);
    uses = 0;
    learned = false;
  }
  if (event.key === preference + ':dismissed:maximize' || event.key === null) {
    dismissed = read(preference + ':dismissed:maximize');
  }
  if (event.key === null || event.key.startsWith(preference)) notify();
});
export const coachState = () => !enabled ? 'off' : uses >= 3 && !learned && !dismissed ? 'tip' : 'waiting';
export function subscribeCoach(listener: () => void) {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
}
export function enableCoach(value: boolean) {
  enabled = value;
  uses = 0;
  learned = false;
  write(preference, value);
  notify();
}
export function dismissMaximizeTip() {
  dismissed = true;
  write(preference + ':dismissed:maximize', true);
  notify();
}
export function recordMaximize(source: 'pointer' | 'keys') {
  if (!enabled || dismissed || learned) return;
  if (source === 'keys') learned = true;
  else uses = Math.min(uses + 1, 3);
  notify();
}
