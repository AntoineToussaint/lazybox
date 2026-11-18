import { useEffect, useRef } from 'react';
import { runnerLink, type RunnerClient } from '@lazybox/client-core';
import { appendToDraft } from './command-runs';

/** Text another panel puts in a conversation's message box for the person to review and send. */
export type ComposeRequest = { sessionId: string; text: string; api?: RunnerClient };
const EVENT = 'lazybox-compose';
const SCOPED_EVENT = 'lazybox-scoped-compose';
const pending = new Map<string, string>();
const scoped = new WeakMap<object, Map<string, string>>();
const owner = (api: RunnerClient) => runnerLink(api) || api;

function requestsFor(api?: RunnerClient): Map<string, string> {
  if (!api) return pending;
  const key = owner(api);
  let requests = scoped.get(key);
  if (!requests) { requests = new Map(); scoped.set(key, requests); }
  return requests;
}

/** Delivers to a mounted composer or holds the text until one is ready. True when delivered now. */
export function composeInConversation(request: ComposeRequest): boolean {
  const requests = requestsFor(request.api);
  const text = appendToDraft(requests.get(request.sessionId) || '', request.text);
  requests.delete(request.sessionId);
  const event = new CustomEvent<ComposeRequest>(request.api ? SCOPED_EVENT : EVENT, { detail: { ...request, text }, cancelable: true });
  if (!window.dispatchEvent(event)) return true;
  requests.set(request.sessionId, text);
  return false;
}

/** A composer takes requests for its conversation once ready. */
export function useComposeRequests(sessionId: string, ready: boolean, accept: (text: string) => void, api?: RunnerClient) {
  const ref = useRef(accept);
  ref.current = accept;
  useEffect(() => {
    if (!sessionId || !ready) return;
    const take = (requests: Map<string, string>) => {
      const queued = requests.get(sessionId);
      if (queued !== undefined) { requests.delete(sessionId); ref.current(queued); }
    };
    take(pending);
    if (api) take(requestsFor(api));
    const listener = (event: Event) => {
      const request = (event as CustomEvent<ComposeRequest>).detail;
      if (request.sessionId !== sessionId || event.defaultPrevented) return;
      if (request.api && (!api || owner(api) !== owner(request.api))) return;
      event.preventDefault(); ref.current(request.text);
    };
    window.addEventListener(EVENT, listener);
    window.addEventListener(SCOPED_EVENT, listener);
    return () => {
      window.removeEventListener(EVENT, listener);
      window.removeEventListener(SCOPED_EVENT, listener);
    };
  }, [sessionId, ready, api]);
}
