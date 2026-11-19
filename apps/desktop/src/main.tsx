import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { createRoot } from 'react-dom/client';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { getCurrentWebview } from '@tauri-apps/api/webview';
import { fetch as nativeFetch } from '@tauri-apps/plugin-http';
import { AgentWindow, ArtifactWindow, PREFERENCE_EVENT, WebWindow, WindowControls, WindowErrorBoundary, Workspace, configureContextMenus, configureExternalTerminal, configureSystemSounds, configureTitleBar, configureVoice, installLinkRouting, openInViewer, runMenuAction, useNativeActionMenu, useRunnerLocator, type NativeMenuItem } from '@lazybox/surfaces';
import type { AddComputerStep, RunnerDirectoryHost, RunnerListing, SshHost } from '@lazybox/platform';
import type { AppHost, AppInfo, ChatReturn, AttentionDestination, AttentionHost, AvailableUpdate, FeedbackDiagnostics, NativeWebViews, RecognitionEvent, RecognizerLanguage, RecognizerStatus, SpeechVoice, SurfaceHost, SurfaceTarget, TitleBarInset, ViewStateStore, UpdateError, UpdateErrorKind, UpdateProgress, VoiceHost, WebViewEvent, WorkspaceHost, ZoomHost } from '@lazybox/platform';
import { FirstRun } from './FirstRun';

type Bootstrap = { url: string; token: string; workingDirectory: string; target: SurfaceTarget; viewKey: string; viewScope: string; viewRoot: string; applicationName: string; fixedWorkspaceId: string; kind: 'workspace' | 'chat' | 'console' | 'artifact' | 'web'; notice: string; viewState: Record<string,string> };
const publishMenu = (items: NativeMenuItem[]) => invoke<void>('set_action_menu', { items });
const fetchRunner: typeof globalThis.fetch = (input, init) => nativeFetch(input, { ...init, maxRedirections: 0 });

/**
 * Notifications are shown by the native host: the webview holds no
 * notification permission, only these commands. A click on a notification
 * focuses the main window, which then asks the host for its conversation.
 */
const desktopAttention: AttentionHost = {
  notifyAttention: notice => invoke<void>('notify_attention', { notice: { title: notice.title, body: notice.body, destination: { collectionId: notice.collectionId, sessionId: notice.sessionId, runner: notice.runner || '' } } }),
  setAttentionBadge: count => invoke<void>('set_attention_badge', { count }),
  notificationPermission: async () => 'granted',
  onAttentionOpen(listener) {
    let disposed = false, stop: (() => void) | undefined;
    const take = async () => {
      const destination = await invoke<AttentionDestination | null>('take_attention_destination');
      if (destination && !disposed) listener(destination);
    };
    void take().catch(() => {});
    void listen('attention-open', () => void take().catch(() => {})).then(unlisten => { if (disposed) unlisten(); else stop = unlisten; });
    return () => { disposed = true; stop?.(); };
  },
};

/**
 * The Web panel's native views: child webviews of this window, placed over
 * their panels (apps/desktop/src-tauri/src/viewer.rs). Views a previous load
 * of this page left behind are closed before the first is shown.
 */
const reset = invoke<void>('web_view_reset').catch(error => console.error('Could not reset web views', error));
const desktopWebViews: NativeWebViews = {
  show: async (id, url, frame, zoom) => { await reset; await invoke<void>('web_view_show', { id, url, frame, zoom }); },
  hide: async id => { await reset; await invoke<void>('web_view_hide', { id }); },
  navigate: (id, url) => invoke<void>('web_view_navigate', { id, url }),
  action: (id, action) => invoke<void>('web_view_action', { id, action }),
  zoom: (id, zoom) => invoke<void>('web_view_zoom', { id, zoom }),
  subscribe(listener) {
    let disposed = false, stop: (() => void) | undefined;
    void getCurrentWebview().listen<WebViewEvent>('web-view', event => listener(event.payload)).then(unlisten => { if (disposed) unlisten(); else stop = unlisten; });
    return () => { disposed = true; stop?.(); };
  },
};
const openLink = (url: string) => invoke<void>('open_link', { url });
/** The UI zoom is the host's: one level for every window, kept across launches (View → Zoom). */
const desktopZoom: ZoomHost = {
  get: () => invoke<number>('ui_zoom'),
  set: level => invoke<number>('set_ui_zoom', { level }),
  subscribe(listener) {
    let disposed = false, stop: (() => void) | undefined;
    void listen<number>('ui-zoom', event => listener(event.payload)).then(unlisten => { if (disposed) unlisten(); else stop = unlisten; });
    return () => { disposed = true; stop?.(); };
  },
};

/**
 * The app itself (src-tauri/src/release.rs, updates.rs, feedback.rs): its
 * version, and for an installed release its updates, the CLI on PATH and the
 * Runner service. Installing an update moves the Runner service to it and
 * restarts the app; agents keep running. Update failures come back as
 * `{ kind, message }` and are rethrown as an UpdateError.
 */
const updateError = (error: unknown): UpdateError => {
  const value = error as { kind?: UpdateErrorKind; message?: string; detail?: string } | string;
  const failure = new Error(typeof value === 'string' ? value : value?.message || String(error)) as UpdateError;
  failure.kind = typeof value === 'object' && value?.kind ? value.kind : 'install';
  if (typeof value === 'object' && value?.detail) failure.detail = value.detail;
  return failure;
};
const base64 = (bytes: Uint8Array) => { let text = ''; for (let i = 0; i < bytes.length; i += 0x8000) text += String.fromCharCode(...bytes.subarray(i, i + 0x8000)); return btoa(text); };
const desktopApp: AppHost = {
  info: () => invoke<AppInfo>('app_info'),
  checkForUpdate: async () => {
    try { return (await invoke<AvailableUpdate | null>('check_for_update')) ?? undefined; } catch (error) { throw updateError(error); }
  },
  installUpdate: async onProgress => {
    const stop = await listen<UpdateProgress>('update-progress', event => onProgress?.(event.payload));
    try { await invoke<void>('install_update'); } catch (error) { throw updateError(error); } finally { stop(); }
  },
  linkCli: async () => {
    const report = await invoke<{ link: string; hints?: string[] }>('link_cli');
    return { link: report.link, hints: report.hints || [] };
  },
  // `lazybox service install`: the Runner restarts on this release, agents keep running.
  moveRunner: async () => { await invoke('setup_install', { linkCli: false }); },
  feedbackDiagnostics: () => invoke<FeedbackDiagnostics>('feedback_diagnostics'),
  saveFeedback: ({ markdown, screenshot }) => invoke<string>('save_feedback', { markdown, screenshot: screenshot ? base64(screenshot) : null }),
  revealPath: path => invoke<void>('plugin:opener|reveal_item_in_dir', { paths: [path] }),
};

/**
 * Voice (VOICE.md): macOS speech recognition on-device and AVSpeechSynthesizer,
 * in the host (src-tauri/src/voice.rs). The page never opens the microphone:
 * the host does, for one recognition at a time, and streams its text here.
 */
type NativeVoiceEvent = RecognitionEvent & { session: number };
const sleep = (ms: number) => new Promise(resolve => setTimeout(resolve, ms));
let speaking = 0;
const desktopVoice: VoiceHost = {
  recognizer: {
    id: 'system',
    label: 'macOS speech recognition',
    status: language => invoke<RecognizerStatus>('voice_status', { language }),
    requestPermission: language => invoke<RecognizerStatus>('voice_request_permission', { language }),
    languages: () => invoke<RecognizerLanguage[]>('voice_languages'),
    openPrivacySettings: pane => invoke<void>('voice_open_settings', { pane }),
    async start(options, listener) {
      // Events can arrive before the session id does: hold them until then.
      let session = 0, done = false;
      const early: NativeVoiceEvent[] = [];
      let stop: (() => void) | undefined;
      const deliver = (event: NativeVoiceEvent) => {
        if (event.session !== session || done) return;
        const { session: _, ...rest } = event;
        if (event.kind === 'end') { done = true; stop?.(); }
        listener(rest as RecognitionEvent);
      };
      stop = await getCurrentWebview().listen<NativeVoiceEvent>('voice-event', event => { if (session) deliver(event.payload); else early.push(event.payload); });
      try { session = await invoke<number>('voice_start', { options }); }
      catch (error) { stop(); throw error instanceof Error ? error : new Error(String(error)); }
      for (const event of early) deliver(event);
      return { stop: () => invoke<void>('voice_stop', { session }), cancel: () => invoke<void>('voice_cancel', { session }) };
    },
  },
  synthesizer: {
    voices: () => invoke<SpeechVoice[]>('speech_voices'),
    async speak(text, { voice, rate }) {
      const mine = ++speaking;
      await invoke<void>('speech_speak', { options: { text, voice, rate } });
      // The synthesizer reports no end here: watch it until it stops (or another reading takes over).
      let started = false;
      for (let waited = 0; mine === speaking; waited += 250) {
        await sleep(250);
        const now = await invoke<boolean>('speech_speaking');
        if (now) started = true;
        else if (started || waited > 2000) return;
      }
    },
    stop: async () => { speaking++; await invoke<void>('speech_stop'); },
  },
};
configureVoice(desktopVoice);

/**
 * The other Runners on this machine (src-tauri/src/runners.rs): the launchers'
 * locators and the files the person added. The host checks each endpoint file
 * as it checks this window's (loopback, only this user can read it) and lets
 * the page call exactly that Runner's RPC path. Only the main window lists them.
 */
const desktopRunners: RunnerDirectoryHost = {
  list: () => invoke<RunnerListing[]>('runner_list'),
  add: path => invoke<RunnerListing>('runner_add', { path }),
  forget: id => invoke<void>('runner_forget', { id }),
  // Runners on other computers (src-tauri/src/links.rs): the bundled CLI's
  // `lazybox remote …`; the app keeps one link agent per computer added.
  sshHosts: () => invoke<SshHost[]>('remote_hosts'),
  async addComputer(host, onStep, options) {
    const id = Math.random().toString(36).slice(2);
    const unlisten = await listen<{ id: string; event: AddComputerStep }>('remote-add-progress', event => { if (event.payload.id === id) onStep(event.payload.event); });
    try { await invoke('remote_add', { id, host, name: options?.name || null }); }
    finally { unlisten(); }
  },
  removeComputer: async name => { await invoke('remote_remove', { name }); },
};

type Renew = () => Promise<{ baseUrl: string; token: string } | undefined>;
function DesktopWindow({ bootstrap, followRestart, renew }: { bootstrap: Bootstrap; followRestart(): void; renew: Renew }) {
  const [error, setError] = useState(bootstrap.notice);
  const host = useMemo<SurfaceHost>(() => ({
    saveTarget: target => invoke<void>('save_surface', { target }),
    reopenAgent: () => invoke<void>('reopen_agent'),
    reopenClosed: () => invoke<void>('reopen_closed'),
    reconnect: () => invoke<void>('reconnect_view'),
    // A chat's window only: the host closes it and tells the window it came from (host.rs `return_chat`).
    putBack: () => invoke<void>('return_chat'),
  }), []);
  const retained = useRef(new Map(Object.entries(bootstrap.viewState)));
  const pendingWrites = useRef(Promise.resolve());
  const viewState = useMemo<ViewStateStore>(() => ({ key: bootstrap.viewKey, scope: bootstrap.viewScope, root: bootstrap.viewRoot,
    getItem: key => retained.current.get(key) ?? localStorage.getItem(key),
    setItem: (key, value) => {
      if (value.length > 256 * 1024) throw new Error('Saved view is too large');
      retained.current.set(key, value);
      pendingWrites.current = pendingWrites.current.then(() => invoke<void>('save_view_state', { key, value })).catch(error => setError(`Could not save this window: ${String(error)}`));
    },
    // Other windows reread an app-wide preference (appearance, shortcuts) once it is saved.
    share: (key, value) => { pendingWrites.current = pendingWrites.current.then(() => invoke<void>('share_preference', { key, value })).catch(() => {}); },
  }), [bootstrap.viewKey, bootstrap.viewScope, bootstrap.viewRoot]);
  const saveTarget = useCallback((target: SurfaceTarget) => host.saveTarget(target), [host]);
  // The main window alone notifies and owns the dock badge; every other window shows the inbox only.
  const notifies = bootstrap.kind === 'workspace' && !bootstrap.fixedWorkspaceId;
  const workspaceHost = useMemo<WorkspaceHost>(() => ({
    ...(notifies ? desktopAttention : {}),
    ...(notifies ? { runners: desktopRunners } : {}),
    zoom: desktopZoom,
    reconnect: () => invoke<void>('reconnect_view'),
    // Arm the host to adopt the restarted Runner's locator (possibly a new port)
    // and reread it promptly; the window keeps its state, nothing reloads.
    expectRunnerRestart: async () => { await invoke<void>('expect_runner_restart'); followRestart(); },
    openWorkspace: collectionId => invoke<void>('open_workspace', { collectionId }),
    chooseRepositoryFolder: () => invoke<string | null>('choose_repository_folder'),
    openSignIn: url => invoke<void>('open_sign_in', { url }),
    copySignInCode: code => invoke<void>('copy_sign_in_code', { code }),
    openChat: target => invoke<void>('open_chat', target),
    dockChat: panelId => invoke<void>('dock_chat', { panelId }),
    // A chat window put back or closed: the host tells the window that owns it (host.rs `chat_owner`).
    onChatReturn(listener) {
      let disposed = false, stop: (() => void) | undefined;
      void getCurrentWebview().listen<ChatReturn>('chat-return', event => listener(event.payload)).then(unlisten => { if (disposed) unlisten(); else stop = unlisten; });
      return () => { disposed = true; stop?.(); };
    },
    chatWindowOpen: panelId => invoke<boolean>('chat_window_open', { panelId }),
    openArtifact: target => invoke<void>('open_artifact', target),
    openLink,
    app: desktopApp,
    webViews: desktopWebViews,
    openWebWindow: target => invoke<void>('open_web_window', target),
    // `lazybox open` and `lazybox shot` (POWER.md): only the main window
    // follows the CLI's requests, so only it offers these.
    ...(notifies ? {
      focusWindow: () => invoke<void>('focus_window'),
      // The page's own snapshot (no screen-recording permission), optionally
      // one panel's rectangle in CSS pixels; the host scales it to the zoomed view.
      captureWindow: async (rect?: { x: number; y: number; width: number; height: number }) =>
        new Uint8Array(await invoke<ArrayBuffer>('capture_window', { region: rect ? { ...rect, viewportWidth: window.innerWidth } : null })),
    } : {}),
  }), [followRestart, notifies]);
  const saveSelection = useCallback((target: { collectionId: string; sessionId: string; commandId: string }) => host.saveTarget({ ...target, commandsOpen: false }), [host]);
  const openViewer = useCallback((sessionId: string, commandId: string) => invoke<void>('open_console', { sessionId, commandId }), []);
  // A preference another window saved: keep its value and let this window's views reread it.
  useEffect(() => {
    let disposed = false; let stop: (() => void) | undefined;
    void listen<{ key: string; value: string }>('preference-changed', event => {
      retained.current.set(event.payload.key, event.payload.value);
      window.dispatchEvent(new CustomEvent(PREFERENCE_EVENT, { detail: { key: event.payload.key } }));
    }).then(unlisten => { if (disposed) unlisten(); else stop = unlisten; });
    return () => { disposed = true; stop?.(); };
  }, []);
  useEffect(() => {
    let disposed = false; let stop: (() => void) | undefined;
    void listen<string>('desktop-error', event => setError(event.payload)).then(unlisten => { if (disposed) unlisten(); else stop = unlisten; });
    return () => { disposed = true; stop?.(); };
  }, []);
  // The menu bar's action items run here when this window is in front; the main window keeps the menu current.
  useEffect(() => {
    let disposed = false; let stop: (() => void) | undefined;
    void listen<string>('menu-action', event => { runMenuAction(event.payload); }).then(unlisten => { if (disposed) unlisten(); else stop = unlisten; });
    return () => { disposed = true; stop?.(); };
  }, []);
  useNativeActionMenu(viewState, notifies ? publishMenu : undefined);
  const focusMain = useCallback(() => void host.reopenAgent().catch(() => {}), [host]);
  // A web window moved back to the workspace: its page opens in a Web panel here.
  useEffect(() => {
    if (bootstrap.kind !== 'workspace') return;
    let disposed = false; let stop: (() => void) | undefined;
    void getCurrentWebview().listen<string>('web-dock', event => { if (!openInViewer(event.payload, true)) void openLink(event.payload).catch(e => setError(String(e))); }).then(unlisten => { if (disposed) unlisten(); else stop = unlisten; });
    return () => { disposed = true; stop?.(); };
  }, [bootstrap.kind]);
  const action = async (callback: () => Promise<void>) => { try { await callback(); setError(''); } catch (error) { setError(String(error)); } };
  return <div className="desktop-root">
    {bootstrap.kind === 'web' ? <WebWindow id={bootstrap.target.webId || 'web'} url={bootstrap.target.webUrl || ''} viewState={viewState} webViews={desktopWebViews} applicationName={bootstrap.applicationName} onDock={url => invoke<void>('dock_web', { url })}/> : <>
    {bootstrap.kind !== 'workspace' && <WindowControls applicationName={bootstrap.applicationName} viewState={viewState} onPutBack={bootstrap.kind === 'chat' ? () => void action(host.putBack!) : undefined} onWorkspace={() => void action(host.reopenAgent)} onReopenClosed={() => void action(host.reopenClosed)} onReconnect={() => void action(host.reconnect)}/>}
    {error && <div className="error" role="alert">{error}<button onClick={() => setError('')}>Dismiss</button></div>}
    {bootstrap.kind === 'artifact' ? <ArtifactWindow connection={{ baseUrl: bootstrap.url, token: bootstrap.token, fetch: fetchRunner, renew }} collectionId={bootstrap.target.collectionId || ''} artifactId={bootstrap.target.artifactId || ''}/> : bootstrap.kind === 'workspace' ? <Workspace
      fixedWorkspaceId={bootstrap.fixedWorkspaceId || undefined}
      connection={{ baseUrl: bootstrap.url, token: bootstrap.token, fetch: fetchRunner, renew }}
      viewState={viewState} host={workspaceHost} onTargetChange={saveSelection} onOpenViewer={openViewer}
      initialTarget={{ collectionId: bootstrap.target.collectionId || '', sessionId: bootstrap.target.sessionId, commandId: bootstrap.target.commandId }}
    /> : <AgentWindow
      connection={{ baseUrl: bootstrap.url, token: bootstrap.token, fetch: fetchRunner, renew }}
      agent="claude" mode={bootstrap.kind === 'console' ? 'console' : 'agent'} viewState={viewState}
      collectionId={bootstrap.target.collectionId} allowStart={false}
      showCommands={false} embedded={bootstrap.kind === 'chat'}
      workingDirectory={bootstrap.workingDirectory}
      initialSessionId={bootstrap.target.sessionId || undefined}
      initialCommandId={bootstrap.target.commandId || undefined}
      initialCommandsOpen={bootstrap.target.commandsOpen}
      onViewChange={bootstrap.kind === 'console' ? saveTarget : undefined}
      onOpenViewer={openViewer}
      onFocusMain={focusMain}
    />}
    </>}
  </div>;
}
// The host rereads this window's Runner locator. A restarted Runner (a Codefly
// hot reload, or Restart Runner from the app) gets a new bearer and may get a
// new port; the window follows it without reloading, whether or not the
// restart was requested here.
const readBootstrap = () => invoke<Bootstrap>('runner_bootstrap');
function DesktopApp() {
  const { value: bootstrap, error, renew, follow } = useRunnerLocator(readBootstrap);
  const [reconnectError, setReconnectError] = useState('');
  const shown = reconnectError || error;
  if (bootstrap) return <>{shown && <div className="error" role="alert">{shown}<button onClick={()=>void invoke('reconnect_view').then(()=>setReconnectError(''),e=>setReconnectError(String(e)))}>Reconnect this window</button></div>}<DesktopWindow bootstrap={bootstrap} followRestart={follow} renew={renew}/></>;
  return <p role={error ? 'alert' : 'status'}>{error ? `Unable to restore this window: ${error}` : 'Restoring your windows…'}</p>;
}
// Web links open in the person's browser (or, with ⌥ or the Settings choice,
// in a Web panel), never inside the app's own page: chat messages, artifacts,
// PR and check links, Markdown anywhere. See web-links.ts.
installLinkRouting(openLink);
window.open = ((url?: string | URL) => { const href = String(url || ''); if (/^(https?:|mailto:)/i.test(href)) void invoke('open_link', { url: href }).catch(error => console.error('Could not open link', error)); return null; }) as typeof window.open;
// Context menus reveal workspace files in Finder; a development build opens
// the inspector from them (Alt+right-click also shows WebKit's own menu).
configureContextMenus({
  openLocalMedia: path => invoke<boolean>('open_local_media', { path }),
  revealInFinder: path => invoke<void>('plugin:opener|reveal_item_in_dir', { paths: [path] }),
  ...(import.meta.env.DEV ? { inspect: () => invoke<void>('plugin:webview|internal_toggle_devtools') } : {}),
});
// "Open in Terminal" opens the person's own terminal app at a workspace's
// folder, and notification sounds can be macOS system sounds: both played by
// the host (src-tauri/src/terminal.rs, sounds.rs), never by the page.
configureExternalTerminal({
  installed: () => invoke<string[]>('terminal_apps'),
  open: request => invoke('open_terminal', { request }),
});
configureSystemSounds({ play: (name, level) => invoke<void>('play_system_sound', { name, level }) });
// An installed release's first run opens the setup window instead (release.rs).
const firstRun = getCurrentWebview().label === 'setup';
/** An event the host sends this window's page alone; returns the unsubscribe. */
function listenHere<T>(event: string, listener: (payload: T) => void) {
  let disposed = false, stop: (() => void) | undefined;
  // The setup window holds no capability, so it cannot listen: it keeps the room in full screen.
  getCurrentWebview().listen<T>(event, value => listener(value.payload)).then(unlisten => { if (disposed) unlisten(); else stop = unlisten; }, () => {});
  return () => { disposed = true; stop?.(); };
}
/**
 * No separate title bar on macOS (src-tauri/src/title_bar.rs): the window's
 * header is it, with room for the window buttons, before the page first draws.
 * Elsewhere the host answers null and headers stay as they are.
 */
async function configureWindowTitleBar() {
  // The zoom too, so the room is right on the first draw (the buttons are not zoomed).
  const [titleBar, zoom] = await Promise.all([
    invoke<{ window: TitleBarInset; popout: TitleBarInset; fullScreen: boolean } | null>('title_bar').catch(() => null),
    firstRun ? Promise.resolve(1) : desktopZoom.get().catch(() => 1),
  ]);
  if (!titleBar) return;
  configureTitleBar({ ...titleBar,
    onFullScreen: listener => listenHere('title-bar-full-screen', listener),
    onPopoutFullScreen: listener => listenHere('title-bar-popout-full-screen', listener),
    startDragging: popout => invoke<void>('title_bar_drag', { popout }),
    doubleClick: popout => invoke<void>('title_bar_double_click', { popout }),
    ...(firstRun ? {} : { zoom: { ...desktopZoom, get: async () => zoom } }) });
}
void configureWindowTitleBar().finally(() => createRoot(document.getElementById('root')!).render(<React.StrictMode><WindowErrorBoundary development={import.meta.env.DEV}>{firstRun ? <FirstRun/> : <DesktopApp/>}</WindowErrorBoundary></React.StrictMode>));
