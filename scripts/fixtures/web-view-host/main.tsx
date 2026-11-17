// The workspace as the desktop app shows it, with native web views, for
// browser tests: a host whose views are not drawn but recorded. Each Web panel
// asks the host to show its page at a frame; the test compares that frame with
// where the panel's page area is, under every layout, and checks that the view
// is hidden whenever something of the app's own is over it.
import React from 'react';
import { createRoot } from 'react-dom/client';
import type { NativeWebViews, WebViewEvent, WebViewFrame } from '@lazybox/platform';
import { WindowErrorBoundary, Workspace, installLinkRouting } from '@lazybox/surfaces';

type View = { shown: boolean; url: string; frame?: WebViewFrame; shows: number };
const views: Record<string, View> = {};
const listeners = new Set<(event: WebViewEvent) => void>();
const emit = (event: WebViewEvent) => setTimeout(() => { for (const listener of listeners) listener(event); });
const loaded = (id: string, url: string) => emit({ id, kind: 'loaded', url, title: new URL(url).host });
const webViews: NativeWebViews = {
  async show(id, url, frame) {
    const view = views[id] ??= { shown: false, url: '', shows: 0 };
    Object.assign(view, { shown: true, frame, shows: view.shows + 1 });
    if (url && url !== view.url) { view.url = url; loaded(id, url); }
  },
  async hide(id) { if (views[id]) views[id].shown = false; },
  async navigate(id, url) { if (views[id]) { views[id].url = url; loaded(id, url); } },
  async action(id, action) { if (action === 'close') delete views[id]; },
  async zoom() {},
  subscribe(listener) { listeners.add(listener); return () => { listeners.delete(listener); }; },
};
(window as unknown as { nativeWebViews: typeof views }).nativeWebViews = views;

installLinkRouting();
const token = new URLSearchParams(location.hash.slice(1)).get('token') || '';
const initial = new URLSearchParams(location.search);
const viewState = { key: 'fixture:main', scope: 'fixture', root: 'fixture', getItem: (key: string) => localStorage.getItem(key), setItem: (key: string, value: string) => localStorage.setItem(key, value) };
const host = { webViews, openChat: async () => {}, dockChat: async () => {} };
createRoot(document.getElementById('root')!).render(<React.StrictMode>
  <WindowErrorBoundary development><Workspace connection={{ baseUrl: location.origin, token }} viewState={viewState} host={host} initialTarget={{ collectionId: initial.get('collection') || '', sessionId: initial.get('session') || '', commandId: '' }}/></WindowErrorBoundary>
</React.StrictMode>);
