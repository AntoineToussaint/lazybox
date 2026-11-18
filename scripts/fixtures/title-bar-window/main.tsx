// The desktop app's windows without the native shell, for the title bar test
// (scripts/test-title-bar.mjs): each window kind as apps/desktop draws it, with
// the macOS host's title bar simulated. The host's answers are what
// src-tauri/src/title_bar.rs gives on macOS 26 (a 48-point header in a
// workspace window, 40 elsewhere), and what the page asks of it (move the
// window, the double-click) is recorded in `window.titleBarCalls`. The test
// enters and leaves full screen and zooms through `window.fixtureTitleBar`.
// The window buttons are drawn where the host puts them, for the screenshots.
// `?overlay=0` draws the window as a browser does: no title bar to simulate.
import React from 'react';
import { createRoot } from 'react-dom/client';
import { AgentWindow, ArtifactWindow, WebWindow, WindowControls, WindowErrorBoundary, Workspace, configureTitleBar } from '@lazybox/surfaces';
import type { TitleBarHost } from '@lazybox/platform';
import { FirstRun } from '../../../apps/desktop/src/FirstRun';

const params = new URLSearchParams(location.search);
const kind = params.get('kind') || 'workspace';
const overlay = params.get('overlay') !== '0';
const connection = { baseUrl: params.get('runner') || '', token: params.get('token') || '' };
const viewState = { key: `fixture:${kind}`, scope: 'fixture', root: 'fixture', getItem: (key: string) => localStorage.getItem(key), setItem: (key: string, value: string) => localStorage.setItem(key, value) };

type Call = { call: 'drag' | 'double-click'; popout: boolean };
declare global { interface Window { titleBarCalls: Call[]; fixtureTitleBar: { fullScreen(value: boolean): void; popoutFullScreen(value: boolean): void; zoom(level: number): void } } }
window.titleBarCalls = [];

// What title_bar.rs measures and places on macOS 26: buttons 14 points
// square, 23 apart, as far from the leading edge as from the top.
const BUTTONS = { size: 14, spacing: 23 };
const WORKSPACE = { inset: 89, height: 48 }, TOOLBAR = { inset: 85, height: 40 };
const header = kind === 'workspace' ? WORKSPACE : TOOLBAR;

if (overlay) {
  const fullScreen = new Set<(value: boolean) => void>(), popoutFullScreen = new Set<(value: boolean) => void>(), zoom = new Set<(level: number) => void>();
  const subscribe = <T,>(set: Set<(value: T) => void>) => (listener: (value: T) => void) => { set.add(listener); return () => { set.delete(listener); }; };
  let level = 1;
  const host: TitleBarHost = {
    window: header, popout: TOOLBAR, fullScreen: false,
    onFullScreen: subscribe(fullScreen), onPopoutFullScreen: subscribe(popoutFullScreen),
    startDragging: async popout => { window.titleBarCalls.push({ call: 'drag', popout }); },
    doubleClick: async popout => { window.titleBarCalls.push({ call: 'double-click', popout }); },
    zoom: { get: async () => level, set: async next => level = next, subscribe: subscribe(zoom) },
  };
  configureTitleBar(host);
  window.fixtureTitleBar = {
    fullScreen: value => { for (const listener of fullScreen) listener(value); lights.style.display = value ? 'none' : 'flex'; },
    popoutFullScreen: value => { for (const listener of popoutFullScreen) listener(value); },
    zoom: next => { level = next; for (const listener of zoom) listener(next); },
  };
  // The system's buttons, drawn over the page where the host places them (screen points, so not zoomed with the page).
  const margin = (header.height - BUTTONS.size) / 2;
  const lights = document.createElement('div');
  lights.className = 'fixture-traffic-lights';
  lights.setAttribute('aria-hidden', 'true');
  lights.style.cssText = `position:fixed;z-index:2147483647;pointer-events:none;left:${margin}px;top:${margin}px;display:flex;gap:${BUTTONS.spacing - BUTTONS.size}px`;
  for (const color of ['#ff5f57', '#febc2e', '#28c840']) {
    const light = document.createElement('span');
    light.style.cssText = `width:${BUTTONS.size}px;height:${BUTTONS.size}px;border-radius:50%;background:${color};box-shadow:inset 0 0 0 .5px rgba(0,0,0,.25)`;
    lights.append(light);
  }
  document.body.append(lights);
}

const noop = () => {};
function Window() {
  if (kind === 'setup') return <FirstRun/>;
  if (kind === 'workspace') return <Workspace connection={connection} viewState={viewState} host={{ openChat: async () => {}, dockChat: async () => {} }}/>;
  if (kind === 'web') return <div className="desktop-root"><WebWindow id="web" url="about:blank" viewState={viewState} applicationName="Lazybox Dev" onDock={async () => {}}/></div>;
  return <div className="desktop-root">
    <WindowControls applicationName="Lazybox Dev" viewState={viewState} onPutBack={kind === 'chat' ? noop : undefined} onWorkspace={noop} onReopenClosed={noop} onReconnect={noop}/>
    {kind === 'artifact'
      ? <ArtifactWindow connection={connection} collectionId="alpha" artifactId={params.get('artifact') || ''}/>
      : <AgentWindow connection={connection} agent="claude" mode="agent" viewState={viewState} collectionId="alpha" allowStart={false} showCommands={false} embedded initialSessionId={params.get('session') || undefined}/>}
  </div>;
}
createRoot(document.getElementById('root')!).render(<React.StrictMode><WindowErrorBoundary><Window/></WindowErrorBoundary></React.StrictMode>);
