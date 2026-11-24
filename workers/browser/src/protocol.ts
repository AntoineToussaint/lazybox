/**
 * The Runner ⇄ browser service protocol (BROWSER.md). One JSON object per
 * line on stdin/stdout, the same framing as the Claude worker:
 *
 * - Runner → service request:  { id, method, params }  → reply { id, result } or { id, error: { message } }
 * - service → Runner request:  { id, method, params }  → Runner replies { id, result } or { id, error }
 * - service → Runner event:    { method, params }      (no id, no reply)
 *
 * The Runner owns policy (who may browse, which sites are allowed, the action
 * log the person sees). The service owns Chromium: one persistent browser
 * context per profile (a workspace), one page per browser (a conversation),
 * and the hard network rules it is told in `open`. It never decides that a new
 * site is allowed: it asks (`site`), and it refuses blocked addresses itself.
 */

/** Hard network rules for one browser, from the Runner. Enforced on every request the page makes. */
export type Policy = {
  /** Loopback ports a page may reach (the workspace's running previews). Every other loopback or private address is refused. */
  loopbackPorts: number[];
  /** host:port pairs never reachable, whatever else allows them (the Runner, its device gateway). */
  blockedHosts: string[];
};

export type Box = { x: number; y: number; width: number; height: number };

/**
 * What the page shows now. `origin` is the site the agent may act on without
 * asking: one the person allowed, or went to themselves (a page that moved
 * itself while they held the browser is neither). The Runner grants it to the
 * agent when the person gives the browser back. Empty when there is none.
 */
export type PageState = { url: string; title: string; loading: boolean; canGoBack: boolean; canGoForward: boolean; viewportWidth: number; viewportHeight: number; origin: string };

// ───────────── Runner → service ─────────────

export type OpenParams = {
  /** The browser's id: the conversation id. */
  browser: string;
  /** The profile directory: one persistent context per profile, shared by the browsers that name it. Absolute, created owner-only. */
  profile: string;
  policy: Policy;
  viewport?: { width: number; height: number };
};
/** open → PageState. Idempotent: an open browser keeps its page and takes the new policy. */

export type ActParams = {
  browser: string;
  /** Who acts: the agent (asks for new sites through `site`) or the person during Take over (never asks; still blocked from blocked addresses). */
  actor: 'agent' | 'person';
  action: 'navigate' | 'snapshot' | 'click' | 'type' | 'select' | 'press' | 'scroll' | 'screenshot' | 'back' | 'forward' | 'reload' | 'wait_for';
  url?: string;
  /** An element ref from the latest snapshot (e12). */
  ref?: string;
  text?: string;
  submit?: boolean;
  values?: string[];
  key?: string;
  direction?: 'up' | 'down' | 'left' | 'right';
  /** Scroll: CSS pixels; absent scrolls most of a viewport. */
  amount?: number;
  fullPage?: boolean;
  /** wait_for: text to appear, text to disappear, or a ref to become visible. */
  textGone?: string;
  timeoutMs?: number;
  /** Cap on snapshot text returned (default 8000 characters). */
  maxChars?: number;
  /** navigate/back/forward/click/type/select/press also return a snapshot of the page after (capped). */
  withSnapshot?: boolean;
};
export type ActResult = {
  /** Compact text for the agent: what happened, the page, and the snapshot when asked. */
  text: string;
  /** One line for the action log: Clicked button "Sign in". */
  summary: string;
  state: PageState;
  /** Where the action happened (the target's box), in viewport CSS pixels. */
  box?: Box;
  /** A screenshot (JPEG), base64. */
  image?: { mediaType: 'image/jpeg' | 'image/png'; data: string };
};

/** input: the person's mouse and keyboard during Take over. → PageState */
export type InputParams = {
  browser: string;
  type: 'mouse_move' | 'mouse_down' | 'mouse_up' | 'wheel' | 'key_down' | 'key_up' | 'text';
  x?: number; y?: number; button?: 'left' | 'middle' | 'right'; clickCount?: number;
  deltaX?: number; deltaY?: number; key?: string; text?: string;
  /** Alt=1, Control=2, Meta=4, Shift=8. */
  modifiers?: number;
};

/** screencast: frames flow (as `frame` events) only while on. → {} */
export type ScreencastParams = { browser: string; on: boolean };
/** close: closes the browser's page; the profile's context closes with its last page. → {} */
export type CloseParams = { browser: string };
/** close_profile: closes every browser of a profile and its context, so the Runner can delete it. → {} */
export type CloseProfileParams = { profile: string };

// ───────────── service → Runner requests ─────────────

/**
 * site: a main-frame navigation the agent's page is about to make to an
 * origin (scheme://host:port) not yet visited in this browser's current
 * visit. The navigation waits for the answer. → { allow: boolean, reason?: string }
 * Not asked for the person's own navigation (actor person) or for blocked
 * addresses (refused without asking).
 */
export type SiteParams = { browser: string; url: string; origin: string };
/** download: the page started a download. → { allow: boolean, path?: string } — when allowed, the service saves it to `path`. */
export type DownloadParams = { browser: string; url: string; fileName: string };
/**
 * sign_in: the agent is about to submit a form holding a filled password field
 * (or a field whose autocomplete says it holds one), by any means: a click, a
 * key, a script the action set off. The submission waits for the answer. → { allow: boolean }
 */
export type SignInParams = { browser: string; url: string; origin: string };

// ───────────── service → Runner events ─────────────

/** frame: one screencast frame. */
export type FrameEvent = { browser: string; data: string /* JPEG base64 */; width: number; height: number };
/** state: the page changed (navigation, title, loading, history). */
export type StateEvent = { browser: string; state: PageState };
/** log: something the Runner did not ask for happened: a blocked request, a popup folded into the page, a download refused. */
export type LogEvent = { browser: string; kind: 'blocked' | 'navigate' | 'download' | 'popup'; summary: string; url?: string };
/** closed: the page or its context went away (crash, close). */
export type ClosedEvent = { browser: string; reason: string };
