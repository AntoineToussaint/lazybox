import { useCallback, useEffect, useState } from 'react';

/** Where the phone is: the Inbox, a conversation, one of its commands, or settings. */
export type Route = { session?: string; command?: string; screen?: 'settings' };

export function parseRoute(search: string): Route {
  const params = new URLSearchParams(search);
  const session = params.get('session') || '', command = params.get('command') || '';
  if (params.get('screen') === 'settings') return { screen: 'settings' };
  return { ...(session ? { session } : {}), ...(session && command ? { command } : {}) };
}

export function routeSearch(base: string, route: Route): string {
  const params = new URLSearchParams(base);
  for (const key of ['session', 'command', 'screen']) params.delete(key);
  if (route.screen) params.set('screen', route.screen);
  if (route.session) params.set('session', route.session);
  if (route.session && route.command) params.set('command', route.command);
  const text = params.toString();
  return text ? `?${text}` : '';
}

const depth = (route: Route) => route.command ? 2 : route.session || route.screen ? 1 : 0;

/**
 * The route lives in the URL, so the phone's back gesture goes back, and a
 * notification's link (`/?view=remote&session=…`) opens its conversation.
 * Going up a level goes back in history when that is where the person came
 * from, so Back never leaves a trail of duplicate entries.
 */
export function useRoute(): [Route, (next: Route) => void] {
  const [route, setRoute] = useState(() => parseRoute(location.search));
  useEffect(() => { const pop = () => setRoute(parseRoute(location.search)); window.addEventListener('popstate', pop); return () => window.removeEventListener('popstate', pop); }, []);
  const go = useCallback((next: Route) => {
    const current = parseRoute(location.search);
    const url = new URL(location.href);
    url.search = routeSearch(url.search, next);
    const up = depth(next) < depth(current) && history.state?.lazyboxDepth === depth(current);
    if (up && depth(next) === depth(current) - 1) history.back();
    else if (depth(next) > 0 && depth(next) >= depth(current)) history.pushState({ lazyboxDepth: depth(next) }, '', url);
    else history.replaceState({ lazyboxDepth: depth(next) }, '', url);
    setRoute(next); window.scrollTo(0, 0);
  }, []);
  return [route, go];
}
