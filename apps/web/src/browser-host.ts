import type { AddComputerStep, AttentionDestination, AttentionHost, ChatReturn, ChatTarget, NotificationPermission, RunnerDirectoryHost, RunnerListing, SshHost, WorkspaceHost } from '@lazybox/platform';

/**
 * Browser test host. Reconnect to existing views across an opener reload/close.
 * A chat's window (`chat`) answers its owner on a broadcast channel: focus,
 * dock and ping; it asks to go back with `putBack`, and says when it goes away.
 * The owner hears both through `onChatReturn`.
 */
export function browserWorkspaceHost(scope: string, token: string, chat?: ChatTarget, notifies = false): WorkspaceHost & { putBack?(): Promise<void> } {
  const attention = notifies ? browserAttention() : {};
  const channel = new BroadcastChannel(`lazybox-views:${scope}`);
  const waiting = new Map<string, (value: { ok: boolean; error?: string }) => void>();
  const windows = new Map<string, Window>();
  channel.onmessage = event => {
    const value = event.data;
    if (!value || typeof value.requestId !== 'string') return;
    if (value.reply) { waiting.get(value.requestId)?.(value); return; }
    if (value.action === 'return' || value.action === 'leaving') { if (!chat) returned(value); return; }
    if (!chat || value.panelId !== chat.panelId) return;
    if (value.action === 'ping') { channel.postMessage({ requestId: value.requestId, reply: true, ok: true }); return; }
    if (value.action === 'focus' && (value.sessionId !== chat.sessionId || value.collectionId !== chat.collectionId)) {
      channel.postMessage({ requestId: value.requestId, reply: true, ok: false, error: 'An existing chat window has another target. Bring it back before retargeting.' }); return;
    }
    if (value.action !== 'focus' && value.action !== 'dock') return;
    channel.postMessage({ requestId: value.requestId, reply: true, ok: true });
    if (value.action === 'dock') window.close(); else window.focus();
  };
  // A chat window going away (closed, or reloading): its owner checks a moment later whether it is still there.
  let putting = false;
  if (chat) window.addEventListener('pagehide', () => { if (!putting) channel.postMessage({ requestId: crypto.randomUUID(), action: 'leaving', panelId: chat.panelId, collectionId: chat.collectionId }); }, { once: true });
  window.addEventListener('pagehide', () => channel.close(), { once: true });
  const reopened = new Map<string, number>();
  const returnListeners = new Set<(request: ChatReturn) => void>();
  const emit = (request: ChatReturn) => { for (const listener of returnListeners) listener(request); };
  function returned(value: { action: string; panelId?: unknown; collectionId?: unknown }) {
    if (typeof value.panelId !== 'string') return;
    const panelId = value.panelId, collectionId = typeof value.collectionId === 'string' ? value.collectionId : '';
    if (value.action === 'return') { windows.delete(panelId); window.focus(); emit({ panelId, collectionId, reason: 'put-back' }); return; }
    // Gone, or only reloading: a reloaded window answers again once it is up. One
    // opened again for this panel meanwhile is a new window, not the one that left.
    const left = Date.now();
    setTimeout(() => void request('ping', { panelId }, 1500).then(open => { if (!open && !((reopened.get(panelId) ?? 0) >= left)) emit({ panelId, collectionId, reason: 'closed' }); }), 300);
  }
  // A chat window this page opened, closed by the person.
  const watch = setInterval(() => { for (const [panelId, child] of windows) if (child.closed) { windows.delete(panelId); emit({ panelId, collectionId: '', reason: 'closed' }); } }, 500);
  window.addEventListener('pagehide', () => clearInterval(watch), { once: true });
  const request = (action: string, target: Partial<ChatTarget> & { panelId: string }, wait = 250): Promise<boolean> => new Promise((resolve, reject) => {
    const requestId = crypto.randomUUID();
    const timer = setTimeout(() => { waiting.delete(requestId); resolve(false); }, wait);
    waiting.set(requestId, value => { clearTimeout(timer); waiting.delete(requestId); if (value.ok) resolve(true); else reject(new Error(value.error)); });
    channel.postMessage({ requestId, action, ...target });
  });
  return {
    ...attention,
    ...(notifies ? { runners: browserRunners() } : {}),
    async openWorkspace(collectionId) {
      const url=new URL(location.href);url.search=new URLSearchParams({view:'workspace',collection:collectionId,workspaceWindow:'1'}).toString();url.hash=new URLSearchParams({token}).toString();
      const opened=window.open(url,`workspace-${collectionId}`);if(!opened)throw new Error('Workspace window was blocked');opened.focus();
    },
    async openSignIn(value) {
      const url=new URL(value);
      if (url.protocol!=='https:' || url.username || url.password) throw new Error('Invalid sign-in URL');
      const opened=window.open(url.href,'_blank','noopener,noreferrer');
      // Browsers may return null for a successful noopener navigation.
      void opened;
    },
    async openChat(target) {
      // A window this panel had before is not this one: its closing says nothing about the new one.
      reopened.set(target.panelId, Date.now()); windows.delete(target.panelId);
      if (await request('focus', target)) return;
      const url = new URL(location.href);
      url.search = new URLSearchParams({ view: 'chat', panel: target.panelId, collection: target.collectionId, session: target.sessionId }).toString();
      url.hash = new URLSearchParams({ token }).toString();
      const child = window.open(url, `chat-${target.panelId}`);
      if (!child) throw new Error('The chat window was blocked by the browser');
      windows.set(target.panelId, child); child.focus();
    },
    async openArtifact(target) {
      const url = new URL(location.href);
      url.search = new URLSearchParams({ view: 'artifact', collection: target.collectionId, artifact: target.artifactId }).toString();
      url.hash = new URLSearchParams({ token }).toString();
      // Named per artifact: opening it again focuses the window it already has.
      const opened = window.open(url, `artifact-${target.artifactId}`);
      if (!opened) throw new Error('The artifact window was blocked by the browser');
      opened.focus();
    },
    async openWebWindow(target) {
      const url = new URL(location.href);
      url.search = new URLSearchParams({ view: 'web', web: target.webId, url: target.url, collection: target.collectionId }).toString();
      url.hash = new URLSearchParams({ token }).toString();
      const opened = window.open(url, `web-${target.webId}`);
      if (!opened) throw new Error('The web window was blocked by the browser');
      opened.focus();
    },
    async dockChat(panelId) {
      windows.delete(panelId);
      // A window that does not answer is gone already (closed, or not reopened after a reload): the chat comes back all the same.
      await request('dock', { panelId });
      window.focus();
    },
    onChatReturn(listener) { returnListeners.add(listener); return () => { returnListeners.delete(listener); }; },
    chatWindowOpen: panelId => request('ping', { panelId }, 600),
    ...(chat ? {
      async putBack() {
        putting = true;
        channel.postMessage({ requestId: crypto.randomUUID(), action: 'return', panelId: chat.panelId, collectionId: chat.collectionId });
        window.close();
      },
    } : {}),
  };
}

/**
 * Web notifications for the browser client. A click focuses this tab and
 * opens the conversation; the badge is the installed-app badge where the
 * browser has one, and the tab title everywhere.
 */
function browserAttention(): AttentionHost {
  const listeners = new Set<(destination: AttentionDestination) => void>();
  const title = document.title;
  const permission = (): NotificationPermission => typeof Notification === 'undefined' ? 'unsupported' : Notification.permission;
  return {
    async notifyAttention(notice) {
      if (permission() !== 'granted') return;
      const shown = new Notification(notice.title, { body: notice.body, tag: `lazybox:${notice.key}` });
      shown.onclick = () => {
        window.focus(); shown.close();
        if (notice.sessionId) for (const listener of listeners) listener({ collectionId: notice.collectionId, sessionId: notice.sessionId, ...(notice.runner ? { runner: notice.runner } : {}) });
      };
    },
    async setAttentionBadge(count) {
      document.title = count ? `(${count}) ${title}` : title;
      const badge = navigator as Navigator & { setAppBadge?(count: number): Promise<void>; clearAppBadge?(): Promise<void> };
      try { if (count) await badge.setAppBadge?.(count); else await badge.clearAppBadge?.(); } catch { /* Not an installed app. */ }
    },
    onAttentionOpen(listener) { listeners.add(listener); return () => { listeners.delete(listener); }; },
    async notificationPermission() { return permission(); },
    async requestNotificationPermission() { return permission() === 'unsupported' ? 'unsupported' : await Notification.requestPermission(); },
  };
}

/**
 * Other Runners for the browser client (MULTI_RUNNER.md): whatever serves this
 * page may list them at `/__lazybox/runners`, each behind a path of this same
 * origin, so the page never calls another origin and nothing crosses it. A
 * server that does not (Vite on its own) answers 404 once and the page shows
 * its one Runner.
 */
function browserRunners(): RunnerDirectoryHost {
  let unsupported = false;
  return {
    async list() {
      if (unsupported) return [];
      const response = await fetch('/__lazybox/runners', { cache: 'no-store', headers: { Accept: 'application/json' } });
      if (response.status === 404 || !(response.headers.get('Content-Type') || '').includes('application/json')) { unsupported = true; return []; }
      if (!response.ok) throw new Error(`The Runner list answered ${response.status}`);
      const value = await response.json() as { runners?: RunnerListing[] };
      // Same origin only: a listed Runner is a path of this page's origin.
      return (Array.isArray(value.runners) ? value.runners : []).flatMap(runner => {
        if (!runner.url) return [runner];
        const url = new URL(runner.url, location.origin);
        return url.origin === location.origin ? [{ ...runner, url: url.href.replace(/\/+$/, '') }] : [];
      });
    },
    // Runners on other computers, when this page's server runs `lazybox
    // remote` for it (the fixture server does; the desktop app uses its host).
    async sshHosts() {
      const response = await fetch('/__lazybox/remote/hosts', { cache: 'no-store', headers: { Accept: 'application/json' } });
      if (!response.ok) throw new Error('This page cannot add computers: use the desktop app, or `lazybox remote add <host>` in a terminal.');
      const value = await response.json() as { hosts?: SshHost[] };
      return Array.isArray(value.hosts) ? value.hosts : [];
    },
    async addComputer(host, onStep, options) {
      const response = await fetch('/__lazybox/remote/add', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ host, name: options?.name || '' }) });
      if (!response.ok || !response.body) throw new Error('This page cannot add computers: use the desktop app, or `lazybox remote add <host>` in a terminal.');
      const reader = response.body.getReader(), decoder = new TextDecoder();
      let buffer = '', failure = '', done = false;
      const take = (line: string) => {
        if (!line.trim()) return;
        const value = JSON.parse(line) as AddComputerStep & { error?: string; done?: boolean };
        if (value.step) onStep(value);
        else if (value.error) failure = value.error;
        else if (value.done) done = true;
      };
      for (;;) {
        const { value, done: end } = await reader.read();
        if (end) break;
        buffer += decoder.decode(value, { stream: true });
        const lines = buffer.split('\n'); buffer = lines.pop() || '';
        lines.forEach(take);
      }
      take(buffer);
      if (failure) throw new Error(failure);
      if (!done) throw new Error('Adding the computer stopped before it finished.');
    },
    async removeComputer(name) {
      const response = await fetch('/__lazybox/remote/remove', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ name }) });
      if (!response.ok) throw new Error(await response.text() || 'Could not remove it');
    },
  };
}

/** A web window's page going back to the main window (Move back to the workspace). */
export function dockWebPage(scope: string, url: string) {
  const channel = new BroadcastChannel(`lazybox-web:${scope}`);
  channel.postMessage({ url }); channel.close();
}
export function onDockedWebPage(scope: string, open: (url: string) => boolean) {
  const channel = new BroadcastChannel(`lazybox-web:${scope}`);
  channel.onmessage = event => { if (typeof event.data?.url === 'string' && open(event.data.url)) window.focus(); };
  window.addEventListener('pagehide', () => channel.close(), { once: true });
}
