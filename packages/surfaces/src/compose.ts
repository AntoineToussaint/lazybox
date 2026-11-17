import { useEffect, useRef } from 'react';

/** Text another panel puts in a conversation's message box for the person to
 * review and send. It is never sent on their behalf. */
export type ComposeRequest = { sessionId: string; text: string };
const EVENT = 'lazybox-compose';
const pending = new Map<string, string>();

/** Delivers to a mounted composer of that conversation, or keeps the text until
 * one mounts (the conversation is being opened). True when delivered now. */
export function composeInConversation(request: ComposeRequest): boolean {
  const event = new CustomEvent<ComposeRequest>(EVENT, { detail: request, cancelable: true });
  if (!window.dispatchEvent(event)) { pending.delete(request.sessionId); return true; }
  pending.set(request.sessionId, request.text);
  return false;
}

/** A composer takes requests for its conversation once ready (its session is
 * loaded and can take a message); until then they stay queued. */
export function useComposeRequests(sessionId: string, ready: boolean, accept: (text: string) => void) {
  const ref = useRef(accept);
  ref.current = accept;
  useEffect(() => {
    if (!sessionId || !ready) return;
    const queued = pending.get(sessionId);
    if (queued !== undefined) { pending.delete(sessionId); ref.current(queued); }
    const listener = (event: Event) => {
      const request = (event as CustomEvent<ComposeRequest>).detail;
      if (request.sessionId !== sessionId || event.defaultPrevented) return;
      event.preventDefault(); ref.current(request.text);
    };
    window.addEventListener(EVENT, listener);
    return () => window.removeEventListener(EVENT, listener);
  }, [sessionId, ready]);
}
