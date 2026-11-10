import type { ChatTarget, WorkspaceHost } from '@lazybox/platform';

/** Browser test host. Reconnect to existing views across an opener reload/close. */
export function browserWorkspaceHost(scope: string, token: string, chat?: ChatTarget): WorkspaceHost {
  const channel = new BroadcastChannel(`lazybox-views:${scope}`);
  const waiting = new Map<string, (value: { ok: boolean; error?: string }) => void>();
  const windows = new Map<string, Window>();
  channel.onmessage = event => {
    const value = event.data;
    if (!value || typeof value.requestId !== 'string') return;
    if (value.reply) { waiting.get(value.requestId)?.(value); return; }
    if (!chat || value.panelId !== chat.panelId) return;
    if (value.action === 'focus' && (value.sessionId !== chat.sessionId || value.collectionId !== chat.collectionId)) {
      channel.postMessage({ requestId: value.requestId, reply: true, ok: false, error: 'An existing chat window has another target. Bring it back before retargeting.' }); return;
    }
    if (value.action !== 'focus' && value.action !== 'dock') return;
    channel.postMessage({ requestId: value.requestId, reply: true, ok: true });
    if (value.action === 'dock') window.close(); else window.focus();
  };
  window.addEventListener('pagehide', () => channel.close(), { once: true });
  const request = (action: string, target: Partial<ChatTarget> & { panelId: string }): Promise<boolean> => new Promise((resolve, reject) => {
    const requestId = crypto.randomUUID();
    const timer = setTimeout(() => { waiting.delete(requestId); resolve(false); }, 250);
    waiting.set(requestId, value => { clearTimeout(timer); waiting.delete(requestId); if (value.ok) resolve(true); else reject(new Error(value.error)); });
    channel.postMessage({ requestId, action, ...target });
  });
  return {
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
      if (await request('focus', target)) return;
      const url = new URL(location.href);
      url.search = new URLSearchParams({ view: 'chat', panel: target.panelId, collection: target.collectionId, session: target.sessionId }).toString();
      url.hash = new URLSearchParams({ token }).toString();
      const child = window.open(url, `chat-${target.panelId}`);
      if (!child) throw new Error('The chat window was blocked by the browser');
      windows.set(target.panelId, child); child.focus();
    },
    async dockChat(panelId) {
      if (await request('dock', { panelId })) return;
      const child = windows.get(panelId);
      if (child?.closed) { windows.delete(panelId); return; }
      throw new Error('The detached browser view did not respond. Reopen it before bringing it back.');
    },
  };
}
