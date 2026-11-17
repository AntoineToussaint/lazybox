import { useEffect, useRef, useState } from 'react';
import type { UiRequest } from '@lazybox/protocol';
import { useRunner } from './RunnerContext';

/** What the window does for each CLI request (POWER.md): a sentence of what it did, or a PNG. */
export type UiRequestHandlers = {
  open(request: UiRequest): Promise<string>;
  shot(request: UiRequest): Promise<{ png: Uint8Array; detail: string }>;
  action(request: UiRequest): Promise<string>;
  /** `lazybox do`: an action of the app's registry. */
  do(request: UiRequest): Promise<string>;
};

/** How long a browser tab waits before it follows the CLI's requests (see below). */
export const WEB_SETTLE_MS = 3000;

const message = (error: unknown) => (error instanceof Error ? error.message : String(error)).replace(/^\[[a-z_]+\]\s*/, '');

/**
 * Follows the requests the `lazybox` CLI addresses to this window — `lazybox
 * open`, `lazybox shot`, `lazybox actions run`, `lazybox do` — and answers each one. Only
 * the main workspace window follows them; the Runner prefers a desktop window
 * over a browser tab. The stream reconnects with backoff, so a restarted
 * Runner finds the window again. Returns whether the window is registered.
 */
export function useUiRequests(enabled: boolean, surface: 'desktop' | 'web', handlers: UiRequestHandlers): boolean {
  const { api } = useRunner();
  const current = useRef(handlers); current.current = handlers;
  const [registered, setRegistered] = useState(false);
  useEffect(() => {
    if (!enabled) return;
    const control = new AbortController();
    const answer = async (request: UiRequest) => {
      let error = '', detail = '', png: Uint8Array = new Uint8Array();
      try {
        if (request.kind === 'open') detail = await current.current.open(request);
        else if (request.kind === 'shot') ({ png, detail } = await current.current.shot(request));
        else if (request.kind === 'action') detail = await current.current.action(request);
        else if (request.kind === 'do') detail = await current.current.do(request);
        else error = `This window does not know “${request.kind}” requests; update Lazybox`;
      } catch (e) { error = message(e); }
      try { await api.completeUiRequest({ id: request.id, error, detail, png }, { signal: control.signal, timeoutMs: 30_000 }); }
      catch { /* the CLI reports a request nobody answered */ }
    };
    void (async () => {
      let delay = 500;
      // A browser allows six connections to the Runner and this stream holds
      // one: a tab joins only once its own first loads are done, and (re)joins
      // only while it is visible. The desktop's native fetch has no such limit.
      if (surface === 'web') await new Promise(resolve => setTimeout(resolve, WEB_SETTLE_MS));
      while (!control.signal.aborted) {
        if (surface === 'web' && document.visibilityState === 'hidden') {
          await new Promise<void>(resolve => { const shown = () => { if (document.visibilityState !== 'hidden' || control.signal.aborted) { document.removeEventListener('visibilitychange', shown); resolve(); } }; document.addEventListener('visibilitychange', shown); control.signal.addEventListener('abort', shown); });
          continue;
        }
        try {
          for await (const request of api.watchUiRequests({ surface }, { signal: control.signal })) {
            delay = 500;
            if (!request.id) { setRegistered(true); continue; }
            void answer(request);
          }
        } catch { /* reconnect below */ }
        setRegistered(false);
        if (control.signal.aborted) return;
        await new Promise(resolve => setTimeout(resolve, delay));
        delay = Math.min(delay * 2, 10_000);
      }
    })();
    return () => { control.abort(); setRegistered(false); };
  }, [api, enabled, surface]);
  return registered;
}

/** The panel of this kind on the page, as a region of the window in CSS pixels. */
export function panelRect(kind: string): { x: number; y: number; width: number; height: number } | undefined {
  const node = document.querySelector<HTMLElement>(`.page-layout .workspace-panel.panel-${CSS.escape(kind)}`);
  if (!node) return undefined;
  const r = node.getBoundingClientRect();
  return { x: Math.round(r.left), y: Math.round(r.top), width: Math.round(r.width), height: Math.round(r.height) };
}
