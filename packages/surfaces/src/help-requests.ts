import { useEffect, useRef } from 'react';

/**
 * "Ask Lazybox" from anywhere (Help menu, ⌘K, ]] h): the window shows the
 * Ask Lazybox panel, and the panel switches to Help and focuses its message
 * box. A panel shown for the request mounts after it was made, so the
 * request waits briefly for one to take it.
 */
const EVENT = 'lazybox-ask';
let pending = 0;
let question: string | undefined;

export function requestAsk(suggestion?: string) {
  question = suggestion;
  pending = Date.now();
  window.dispatchEvent(new Event(EVENT));
}

/** Calls onAsk for each request while mounted, and for one made just before mounting. */
export function useAskRequests(onAsk: (question?: string) => void) {
  const latest = useRef(onAsk); latest.current = onAsk;
  useEffect(() => {
    const take = () => { if (!pending) return; pending = 0; latest.current(question); question = undefined; };
    if (pending && Date.now() - pending < 3000) queueMicrotask(take); else pending = 0;
    window.addEventListener(EVENT, take);
    return () => window.removeEventListener(EVENT, take);
  }, []);
}
