import type { PushPreferences, PushSettings } from '@lazybox/protocol';
import type { RunnerClient } from '@lazybox/client-core';

/**
 * Web Push on the phone (REMOTE.md). The Runner keeps one VAPID key and each
 * paired phone's subscription; this side asks the person, subscribes the
 * browser with the Runner's key, and hands the subscription to the Runner.
 * The service worker (apps/web/public/sw.js) shows what arrives.
 */

/** Whether this browser can receive a push, and if not, what the person can do about it. */
export type PushSupport =
  | { kind: 'supported' }
  /** iPhone or iPad Safari in a tab: push needs the app on the Home Screen (iOS 16.4+). */
  | { kind: 'install' }
  /** iOS before 16.4 has no web push at all. */
  | { kind: 'old-ios'; version: string }
  /** No service worker or push in this browser (an in-app browser, a private window). */
  | { kind: 'unsupported' };

export type PushEnvironment = { userAgent: string; maxTouchPoints: number; standalone: boolean; serviceWorker: boolean; pushManager: boolean; notification: boolean };

export function isIOS(env: Pick<PushEnvironment, 'userAgent' | 'maxTouchPoints'>): boolean {
  // iPadOS reports itself as a Mac; touch tells them apart.
  return /iPhone|iPad|iPod/.test(env.userAgent) || (/Macintosh/.test(env.userAgent) && env.maxTouchPoints > 1);
}

/** Pure: what this browser can do with push. */
export function pushSupport(env: PushEnvironment): PushSupport {
  if (isIOS(env)) {
    const match = /OS (\d+)[_.](\d+)/.exec(env.userAgent) || /Version\/(\d+)\.(\d+)/.exec(env.userAgent);
    if (match) {
      const major = Number(match[1]), minor = Number(match[2]);
      if (major < 16 || (major === 16 && minor < 4)) return { kind: 'old-ios', version: `${major}.${minor}` };
    }
    if (!env.standalone) return { kind: 'install' };
  }
  return env.serviceWorker && env.pushManager && env.notification ? { kind: 'supported' } : { kind: 'unsupported' };
}

export function currentEnvironment(): PushEnvironment {
  const nav = navigator as Navigator & { standalone?: boolean };
  return {
    userAgent: nav.userAgent, maxTouchPoints: nav.maxTouchPoints || 0,
    standalone: nav.standalone === true || (typeof matchMedia === 'function' && matchMedia('(display-mode: standalone)').matches),
    serviceWorker: 'serviceWorker' in nav, pushManager: typeof window !== 'undefined' && 'PushManager' in window, notification: typeof Notification !== 'undefined',
  };
}

export function base64urlToBytes(value: string): Uint8Array<ArrayBuffer> {
  const padded = value.replace(/-/g, '+').replace(/_/g, '/') + '='.repeat((4 - value.length % 4) % 4);
  const binary = atob(padded);
  const out = new Uint8Array(new ArrayBuffer(binary.length));
  for (let i = 0; i < binary.length; i++) out[i] = binary.charCodeAt(i);
  return out;
}
function sameBytes(a: ArrayBuffer | null | undefined, b: Uint8Array): boolean {
  if (!a) return false;
  const left = new Uint8Array(a);
  return left.length === b.length && left.every((value, i) => value === b[i]);
}

/** The kinds a phone can be told about: the same three as Settings → Notifications on the computer, with its defaults. */
export const defaultPushPreferences = { needsInput: true, failed: true, review: false, quietStart: '', quietEnd: '', timeZone: '' };
export type PhonePushPreferences = typeof defaultPushPreferences;
export function preferencesOf(settings?: PushSettings): PhonePushPreferences {
  const p = settings?.preferences;
  return p ? { needsInput: p.needsInput, failed: p.failed, review: p.review, quietStart: p.quietStart, quietEnd: p.quietEnd, timeZone: p.timeZone } : { ...defaultPushPreferences };
}
export function wirePreferences(p: PhonePushPreferences): Omit<PushPreferences, '$typeName' | '$unknown'> {
  return { needsInput: p.needsInput, failed: p.failed, review: p.review, quietStart: p.quietStart, quietEnd: p.quietEnd, timeZone: p.timeZone || localTimeZone() };
}
export function localTimeZone(): string {
  try { return Intl.DateTimeFormat().resolvedOptions().timeZone || ''; } catch { return ''; }
}

/** Pure: "Quiet 22:00–07:00" while those hours hold, else "". Both ends in the phone's own clock. */
export function quietNow(p: Pick<PhonePushPreferences, 'quietStart' | 'quietEnd'>, now: Date): boolean {
  const minutes = (value: string) => { const m = /^(\d{2}):(\d{2})$/.exec(value); return m ? Number(m[1]) * 60 + Number(m[2]) : -1; };
  const start = minutes(p.quietStart), end = minutes(p.quietEnd), at = now.getHours() * 60 + now.getMinutes();
  if (start < 0 || end < 0 || start === end) return false;
  return start < end ? at >= start && at < end : at >= start || at < end;
}

/** The service worker that shows pushes and opens their conversation. */
export async function registerWorker(): Promise<ServiceWorkerRegistration | undefined> {
  if (!('serviceWorker' in navigator)) return undefined;
  try { return await navigator.serviceWorker.register('/sw.js', { scope: '/' }); }
  catch { return undefined; }
}

export class PushError extends Error {
  constructor(message: string, readonly reason: 'denied' | 'unsupported' | 'failed') { super(message); }
}

/**
 * Turns notifications on: the browser's permission (asked first, inside the
 * tap, as iOS requires), a subscription with the Runner's key, and the
 * Runner told. A subscription made with another Runner's key is replaced.
 */
export async function enablePush(api: RunnerClient, preferences: PhonePushPreferences): Promise<PushSettings> {
  if (typeof Notification === 'undefined') throw new PushError('This browser cannot show notifications.', 'unsupported');
  const permission = Notification.permission === 'granted' ? 'granted' : await Notification.requestPermission();
  if (permission !== 'granted') throw new PushError('Notifications are blocked for Lazybox on this phone.', 'denied');
  const registration = await registerWorker();
  if (!registration) throw new PushError('This browser cannot run the notification service.', 'unsupported');
  await navigator.serviceWorker.ready;
  const settings = await api.getPushSettings({}, { timeoutMs: 15000 });
  if (!settings.applicationServerKey) throw new PushError('Your computer did not give a notification key. Update Lazybox there.', 'failed');
  const key = base64urlToBytes(settings.applicationServerKey);
  let subscription = await registration.pushManager.getSubscription();
  if (subscription && !sameBytes(subscription.options?.applicationServerKey, key)) { await subscription.unsubscribe().catch(() => false); subscription = null; }
  try { subscription ??= await registration.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: key }); }
  catch (error) { throw new PushError(`The phone's push service refused: ${error instanceof Error ? error.message : String(error)}`, 'failed'); }
  return sendSubscription(api, subscription, preferences);
}

async function sendSubscription(api: RunnerClient, subscription: PushSubscription, preferences: PhonePushPreferences): Promise<PushSettings> {
  const json = subscription.toJSON();
  return api.setPushSettings({ subscription: { endpoint: json.endpoint || subscription.endpoint, p256dh: json.keys?.p256dh || '', auth: json.keys?.auth || '' }, preferences: wirePreferences(preferences) }, { timeoutMs: 15000 });
}

/** Turns notifications off on this phone and on the Runner. */
export async function disablePush(api: RunnerClient): Promise<PushSettings> {
  const registration = 'serviceWorker' in navigator ? await navigator.serviceWorker.getRegistration('/').catch(() => undefined) : undefined;
  const subscription = await registration?.pushManager.getSubscription().catch(() => null);
  await subscription?.unsubscribe().catch(() => false);
  return api.setPushSettings({ unsubscribe: true }, { timeoutMs: 15000 });
}

/**
 * On every start: a browser may rotate its subscription, and the Runner drops
 * one its push service says has ended. When the person allowed notifications
 * and the Runner should be sending them, hand it the current subscription.
 */
export async function resyncPush(api: RunnerClient, settings: PushSettings): Promise<PushSettings | undefined> {
  if (typeof Notification === 'undefined' || Notification.permission !== 'granted' || !settings.applicationServerKey) return undefined;
  const registration = await navigator.serviceWorker?.getRegistration('/').catch(() => undefined);
  if (!registration) return undefined;
  let subscription = await registration.pushManager.getSubscription().catch(() => null);
  const key = base64urlToBytes(settings.applicationServerKey);
  if (subscription && !sameBytes(subscription.options?.applicationServerKey, key)) { await subscription.unsubscribe().catch(() => false); subscription = null; }
  // Off on purpose (the person turned it off, or never on): nothing to repair.
  if (!subscription && !settings.subscribed) return undefined;
  subscription ??= await registration.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: key }).catch(() => null);
  return subscription ? sendSubscription(api, subscription, preferencesOf(settings)) : undefined;
}

/** Closes the banners of conversations that no longer need the person (answered on the computer, say). */
export async function closeStaleNotifications(stillWaiting: (tag: string) => boolean): Promise<void> {
  const registration = await navigator.serviceWorker?.getRegistration('/').catch(() => undefined);
  if (!registration?.getNotifications) return;
  for (const notification of await registration.getNotifications().catch(() => [] as Notification[])) {
    if (notification.tag && !stillWaiting(notification.tag)) notification.close();
  }
}
