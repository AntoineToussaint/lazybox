// Lazybox on a phone (REMOTE.md): the service worker the device gateway serves.
// It shows the notifications the Runner pushes, opens the conversation one
// names, and keeps the app's shell so that, with the computer unreachable,
// Lazybox still opens and says so instead of the browser's error page.
//
// A push carries only { title, kind, url, tag }: the conversation's title,
// what happened, where to open it, and which banner it replaces. Never code
// or messages. The Runner encrypts it for this browser alone (RFC 8291).

const SHELL = 'lazybox-shell-v1';
const MAX_ASSETS = 80;
const headings = {
  approval: 'Approval needed',
  question: 'Question for you',
  failed: 'Something failed',
  review: 'Ready for review',
  pr_failed: 'Pull request needs you',
  pr_ready: 'Pull request update',
};

/** Only a link into the phone app on this origin; anything else opens the Inbox. */
function safeUrl(value) {
  try {
    const url = new URL(String(value || ''), self.location.origin);
    if (url.origin === self.location.origin && url.searchParams.get('view') === 'remote') return url.pathname + url.search;
  } catch { /* fall through */ }
  return '/?view=remote';
}

/** Pure: the notification a push becomes. */
function notificationFor(data) {
  const kind = typeof data.kind === 'string' ? data.kind : '';
  const name = typeof data.title === 'string' ? data.title.slice(0, 120) : '';
  const url = safeUrl(data.url);
  const tag = typeof data.tag === 'string' && data.tag ? data.tag.slice(0, 128) : url;
  if (kind === 'test') return { title: name || 'Notifications work', options: { body: 'Lazybox will tell this phone when an agent needs you.', tag: 'test', data: { url }, icon: '/icon-192.png', badge: '/icon-192.png' } };
  if (kind === 'summary') return { title: name || 'Agents need you', options: { body: 'Open the Inbox to see them all.', tag: 'summary', renotify: true, data: { url }, icon: '/icon-192.png', badge: '/icon-192.png' } };
  return {
    title: headings[kind] || 'Lazybox',
    options: { body: name || 'Open Lazybox to see what changed.', tag, renotify: kind !== 'review', requireInteraction: kind === 'approval' || kind === 'question', data: { url }, icon: '/icon-192.png', badge: '/icon-192.png' },
  };
}

self.addEventListener('install', () => self.skipWaiting());
self.addEventListener('activate', event => event.waitUntil(self.clients.claim()));

self.addEventListener('push', event => {
  let data = {};
  try { data = event.data ? event.data.json() : {}; } catch { data = {}; }
  const { title, options } = notificationFor(data && typeof data === 'object' ? data : {});
  // Every push shows a notification: iOS and Chrome both require it.
  event.waitUntil(self.registration.showNotification(title, options));
});

/** Focus an open Lazybox and take it to the conversation, or open one there. */
async function openTarget(url) {
  const target = new URL(safeUrl(url), self.location.origin);
  const windows = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
  const client = windows.find(c => new URL(c.url).origin === self.location.origin && new URL(c.url).searchParams.get('view') === 'remote');
  if (client) {
    await client.focus().catch(() => undefined);
    client.postMessage({ type: 'lazybox-open', url: target.href });
    return client;
  }
  return self.clients.openWindow(target.href);
}

self.addEventListener('notificationclick', event => {
  event.notification.close();
  event.waitUntil(openTarget(event.notification.data && event.notification.data.url));
});

const offlinePage = () => new Response(`<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Lazybox</title><body style="font:16px -apple-system,system-ui,sans-serif;background:#121317;color:#e8e9ed;margin:0;padding:15vh 24px;text-align:center"><h1 style="font-size:22px">Can’t reach your computer</h1><p style="color:#9a9ca5;line-height:1.5">Lazybox keeps working there. Check that the computer is awake and that Tailscale is connected on both devices, then pull down to try again.</p></body>`, { status: 503, headers: { 'Content-Type': 'text/html; charset=utf-8', 'Cache-Control': 'no-store' } });

async function trim(cache) {
  const keys = await cache.keys();
  for (const key of keys.slice(0, Math.max(0, keys.length - MAX_ASSETS))) await cache.delete(key);
}

self.addEventListener('fetch', event => {
  const request = event.request;
  if (request.method !== 'GET') return;
  const url = new URL(request.url);
  // The Runner's API and sign-in are never cached.
  if (url.origin !== self.location.origin || url.pathname.startsWith('/lazybox.') || url.pathname.startsWith('/remote/')) return;
  if (request.mode === 'navigate') {
    // Network first: a new release is used at once; the saved shell only when the computer is away.
    event.respondWith(fetch(request).then(response => {
      if (response.ok) { const copy = response.clone(); event.waitUntil(caches.open(SHELL).then(cache => cache.put('/?view=remote', copy))); }
      return response;
    }).catch(() => caches.open(SHELL).then(cache => cache.match('/?view=remote')).then(hit => hit || offlinePage())));
    return;
  }
  // Built assets are named by their content, so a saved one is always right.
  if (url.pathname.startsWith('/assets/')) {
    event.respondWith(caches.open(SHELL).then(cache => cache.match(request).then(hit => hit || fetch(request).then(response => {
      if (response.ok) event.waitUntil(cache.put(request, response.clone()).then(() => trim(cache)));
      return response;
    }))));
  }
});

// For tests: the same functions the events use.
self.lazyboxWorker = { notificationFor, openTarget, safeUrl };
