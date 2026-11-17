/** Navigation belongs to a particular native window, not to its execution. */
export type SurfaceTarget = { sessionId: string; commandId: string; commandsOpen: boolean; collectionId?: string; panelId?: string; artifactId?: string; webId?: string; webUrl?: string };
export type ChatTarget = { panelId: string; collectionId: string; sessionId: string };
/**
 * A chat window going back into the window it was detached from: the person
 * asked to put it back (that window shows it and comes forward), or closed it
 * (it is put back quietly, never lost).
 */
export type ChatReturn = { panelId: string; collectionId: string; reason: 'put-back' | 'closed' };
export type ArtifactTarget = { collectionId: string; artifactId: string };
/** A conversation that needs the person, as a native notification. `runner`: which of the window's Runners it is on, when it shows several. */
export type AttentionNotice = { key: string; title: string; body: string; collectionId: string; sessionId: string; runner?: string };
export type AttentionDestination = { collectionId: string; sessionId: string; runner?: string };
export type NotificationPermission = 'granted' | 'denied' | 'default' | 'unsupported';
/** What a host offers for attention: notifications, a dock badge, and the conversation a clicked notification asks for. */
export type AttentionHost = {
  notifyAttention?(notice: AttentionNotice): Promise<void>;
  setAttentionBadge?(count: number): Promise<void>;
  /** Calls `listener` for each clicked notification; returns the unsubscribe. */
  onAttentionOpen?(listener: (destination: AttentionDestination) => void): () => void;
  notificationPermission?(): Promise<NotificationPermission>;
  requestNotificationPermission?(): Promise<NotificationPermission>;
};
/** A region of the window in CSS pixels, from its top-left corner. */
export type WindowRect = { x: number; y: number; width: number; height: number };
/** What the desktop does for the `lazybox` CLI (POWER.md): bring the window forward, and take a PNG of it or of one region. */
export type PowerHost = {
  focusWindow?(): Promise<void>;
  captureWindow?(rect?: WindowRect): Promise<Uint8Array>;
};
/** The app itself: its version and, for an installed release, its updates (INSTALL.md). */
export type AppInfo = {
  version: string;
  /** "release" for an installed release; otherwise the app's identity (Dev, Working). */
  channel: string;
  /** This build checks a signed update feed. */
  updater: boolean;
  /** The bundled `lazybox` CLI, for an installed release. */
  cli: string;
  /**
   * Set when `make install` built this release from a clone: the commit and
   * the checkout. Such a build has no updater; it updates with `make update`.
   */
  source?: { commit: string; dirty: boolean; checkout: string } | null;
};
/** One section of the release notes, as CHANGELOG.md writes it (an id, a date, a title, items). */
export type UpdateRelease = { id: string; date: string; title: string; items: { title: string; body: string }[] };
export type AvailableUpdate = {
  version: string;
  current: string;
  /** Plain-text notes (latest.json `notes`), for a feed without structured releases. */
  notes: string;
  date: string;
  /** The changelog sections the feed carries, newest first (latest.json `lazybox.releases`). */
  releases?: UpdateRelease[];
};
export type UpdateProgress = { phase: 'download' | 'verify' | 'install' | 'runner' | 'restart'; downloaded: number; total: number };
/**
 * Why checking or installing failed, so the window can say it as a sentence:
 * `offline` (no network or GitHub unreachable), `feed` (no valid update
 * information published), `signature` (the download is not signed with this
 * app's key: nothing was replaced), `permission` (the app cannot replace
 * itself where it is), `install` (anything else while installing),
 * `unavailable` (this build does not update itself).
 */
export type UpdateErrorKind = 'offline' | 'feed' | 'signature' | 'permission' | 'install' | 'unavailable';
export type UpdateError = Error & { kind: UpdateErrorKind; detail?: string };
/** What Help → Send feedback attaches, when the person keeps it ticked. Redacted in the window, before anything is shown. */
export type FeedbackDiagnostics = {
  os: string;
  arch: string;
  /** This person's home directory, so paths under it read as `~`. */
  home: string;
  /** The Runner's log, when this build has one to read. */
  runnerLog?: { path: string; tail: string };
};
export type AppHost = {
  info(): Promise<AppInfo>;
  /** Asks the feed; resolves undefined when this is the newest version. Rejects with an UpdateError. */
  checkForUpdate?(): Promise<AvailableUpdate | undefined>;
  /**
   * Downloads the update found by the last check, verifies its signature,
   * installs it, moves the Runner to it and restarts the app. Agents keep
   * running across the Runner's restart. Rejects with an UpdateError.
   */
  installUpdate?(onProgress?: (progress: UpdateProgress) => void): Promise<void>;
  /** Puts the bundled CLI on PATH (`lazybox link`); returns where, and anything to do by hand. */
  linkCli?(): Promise<{ link: string; hints: string[] }>;
  /** Moves the Runner service to this app's release (`lazybox service install`); agents keep running. */
  moveRunner?(): Promise<void>;
  /** Versions, the OS and the Runner's log tail, for a feedback report. */
  feedbackDiagnostics?(): Promise<FeedbackDiagnostics>;
  /** Writes a feedback report (and its screenshot) to a folder of its own; resolves its path. Nothing is sent anywhere. */
  saveFeedback?(report: { markdown: string; screenshot?: Uint8Array }): Promise<string>;
  /** Shows a file in Finder. */
  revealPath?(path: string): Promise<void>;
};
/**
 * Where a native web view sits, in the window's CSS pixels, with the
 * viewport it was measured in (the host scales it by the window's zoom).
 */
export type WebViewFrame = { x: number; y: number; width: number; height: number; viewportWidth: number; viewportHeight: number };
/** What a native web view reports about its page. Every event names the view (`id`). */
export type WebViewEvent =
  | { id: string; kind: 'loading'; url: string }
  | { id: string; kind: 'loaded'; url: string; title?: string }
  | { id: string; kind: 'title'; title: string }
  /** The page's icon, as a data: image URL. */
  | { id: string; kind: 'icon'; icon: string }
  /** The page tried to open a link Lazybox does not show (mailto:, a custom app scheme…). */
  | { id: string; kind: 'blocked'; url: string }
  /** The page started a download: the viewer never saves files. */
  | { id: string; kind: 'download'; url: string };
/**
 * Native web views over a panel (the desktop app): a real browser engine,
 * so sites that refuse to be framed still show. Each view runs on its own
 * website data, with no access to the app's commands. `id` is unique per window.
 */
export type NativeWebViews = {
  /** Moves the view to `frame` and shows it. With a `url` (its panel's first show)
   * it is created on that page, or taken there if it exists on another; an empty
   * `url` only moves it. */
  show(id: string, url: string, frame: WebViewFrame, zoom: number): Promise<void>;
  hide(id: string): Promise<void>;
  navigate(id: string, url: string): Promise<void>;
  action(id: string, action: 'reload' | 'stop' | 'focus' | 'close'): Promise<void>;
  zoom(id: string, zoom: number): Promise<void>;
  /** Every view's events in this window; returns the unsubscribe. */
  subscribe(listener: (event: WebViewEvent) => void): () => void;
};
/** A web page in a window of its own (a detached Web panel). */
export type WebWindowTarget = { webId: string; url: string; collectionId: string };
/** The UI zoom a native host applies to every window and keeps across launches. */
export type ZoomHost = {
  get(): Promise<number>;
  set(level: number): Promise<number>;
  /** Calls `listener` whenever the zoom changes (a menu shortcut, another window); returns the unsubscribe. */
  subscribe(listener: (level: number) => void): () => void;
};
/**
 * A Runner a window can show besides its own (MULTI_RUNNER.md): found through
 * a locator (`~/.config/lazybox/runners/*.json`, what `lazybox runners`
 * lists) or a file the person added in Settings → Runners. Only a running
 * Runner carries its loopback origin and owner bearer, read from its private
 * endpoint file by the host; nothing here is ever written anywhere.
 */
export type RunnerListing = {
  /** Stable while the locator exists, safe in keys ([A-Za-z0-9_.-]). */
  id: string;
  /** The locator's name: "dev", "working", "lazybox", or the file's. */
  name: string;
  product?: string;
  /** "dev", "working" or "release"; empty for a Runner the person added. */
  channel?: string;
  /** Registered by a launcher, or added by the person (who can forget it). */
  source: 'registered' | 'added';
  /** The file it was found through (a locator or an endpoint file); never a credential. */
  locator?: string;
  /** The window's own Runner. */
  current: boolean;
  state: 'running' | 'stopped' | 'invalid';
  /** Why it is stopped or cannot be used, as a sentence. */
  problem?: string;
  /** Loopback origin and owner bearer, for a running Runner only. */
  url?: string;
  token?: string;
};
export type RunnerDirectoryHost = {
  /** Every Runner the host can find now, the window's own included. */
  list(): Promise<RunnerListing[]>;
  /** Adds a Runner by its locator or endpoint file; resolves its listing. */
  add?(path: string): Promise<RunnerListing>;
  /** Forgets a Runner the person added (registered ones are the launchers'). */
  forget?(id: string): Promise<void>;
};

export type WorkspaceHost = AttentionHost & PowerHost & {
  /** The other Runners this window can show (MULTI_RUNNER.md); absent where there are none to find. */
  runners?: RunnerDirectoryHost;
  /** The UI zoom, where the host owns one (the desktop app). */
  zoom?: ZoomHost;
  /** The desktop app: version, updates (absent in a browser). */
  app?: AppHost;
  /** Opens a link in the person's browser (http, https and mailto only). */
  openLink?(url: string): Promise<void>;
  /** Native web views for the Web panel and Preview; without them both use an iframe. */
  webViews?: NativeWebViews;
  /** Opens a web page in its own window, with the viewer's toolbar. */
  openWebWindow?(target: WebWindowTarget): Promise<void>;
 reconnect?(): Promise<void>; /** Before Restart Runner: follow the restarted Runner, even on a new port, without reloading. */ expectRunnerRestart?(): Promise<void>;  openWorkspace?(collectionId: string): Promise<void>; chooseRepositoryFolder?(): Promise<string | null>; openSignIn?(url: string): Promise<void>; copySignInCode?(code: string): Promise<void>; openChat(target: ChatTarget): Promise<void>; /** An artifact page in its own window; reuses that artifact's window when open. */ openArtifact?(target: ArtifactTarget): Promise<void>;
  /** Closes a chat's own window and brings this window forward, where the chat now is. */
  dockChat(panelId: string): Promise<void>;
  /** Chats of this window whose own window asks to come back, or closed; returns the unsubscribe. */
  onChatReturn?(listener: (request: ChatReturn) => void): () => void;
  /** Whether a chat's own window is still open: a saved layout's chat whose window is gone comes back. */
  chatWindowOpen?(panelId: string): Promise<boolean>;
};
export type ViewStateStore = {
  key: string;
  /** Resolved authority scope shared by panels moved between native windows. */
  scope?: string;
  /** Runner authority every window shares; app-wide preferences are saved under it. */
  root?: string;
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
  /** Tells this host's other windows that an app-wide preference changed (they reread it). */
  share?(key: string, value: string): void;
};
export type SurfaceHost = {
  saveTarget(target: SurfaceTarget): Promise<void>;
  /** A chat's own window: put it back in the window it came from, and close this one. */
  putBack?(): Promise<void>;
  reopenAgent(): Promise<void>;
  reopenClosed(): Promise<void>;
  reconnect(): Promise<void>;
};

// ---- Voice (VOICE.md) -----------------------------------------------------

/** An authorization the system holds for the app: the microphone, or speech recognition. */
export type VoicePermission = 'granted' | 'denied' | 'restricted' | 'not-determined' | 'unsupported';
/** Whether a recognizer can dictate now, and whether audio stays on this device. */
export type RecognizerStatus = {
  available: boolean;
  /** True only when recognition runs on this device: the audio never leaves it. */
  onDevice: boolean;
  microphone: VoicePermission;
  speech: VoicePermission;
  /** Why it is not available, as a sentence; empty when it is. */
  reason: string;
  /** The system's language (BCP 47), what "Automatic" uses; empty when unknown. */
  language: string;
};
export type RecognizerLanguage = { code: string; name: string; onDevice?: boolean };
/** What a recognition reports. `segment` numbers stretches of speech; `final` settles one. */
export type RecognitionEvent =
  | { kind: 'result'; segment: number; text: string; final: boolean }
  /** The microphone's level, 0–1. */
  | { kind: 'level'; level: number }
  /** Speech heard or not (for engines that report no level). */
  | { kind: 'activity'; speaking: boolean }
  | { kind: 'error'; code: string; message: string }
  /** Over: the microphone is closed. */
  | { kind: 'end' };
export type RecognitionSession = {
  /** Stop listening; the recognizer settles what it heard, then ends. */
  stop(): Promise<void>;
  /** Stop listening and drop what was heard. */
  cancel(): Promise<void>;
};
export type RecognitionOptions = { language: string; punctuation: boolean; allowServer: boolean };
/** A speech recognizer: the desktop's (macOS Speech, on-device) or the browser's (Web Speech). */
export type SpeechRecognizerHost = {
  id: 'system' | 'browser';
  label: string;
  status(language: string): Promise<RecognizerStatus>;
  /** Asks for the microphone and speech recognition, where the system asks once. */
  requestPermission?(language: string): Promise<RecognizerStatus>;
  languages(): Promise<RecognizerLanguage[]>;
  /** Opens the system's privacy settings for one permission. */
  openPrivacySettings?(pane: 'microphone' | 'speech' | 'keyboard'): Promise<void>;
  start(options: RecognitionOptions, listener: (event: RecognitionEvent) => void): Promise<RecognitionSession>;
};
export type SpeechVoice = { id: string; name: string; language: string };
/** Read-aloud: the desktop's AVSpeechSynthesizer or the browser's speechSynthesis. */
export type SpeechSynthesizerHost = {
  voices(): Promise<SpeechVoice[]>;
  /** Resolves when the text was read or reading stopped. */
  speak(text: string, options: { voice: string; rate: number }): Promise<void>;
  stop(): Promise<void>;
};
export type VoiceHost = { recognizer?: SpeechRecognizerHost; synthesizer?: SpeechSynthesizerHost };
