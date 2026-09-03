//! TerminalStack — multi-terminal right-pane surface per session.
//!
//! Each session can have several terminals open simultaneously: the
//! agent (Claude / Codex / Cursor), a shell, a log tail. This
//! component owns the per-terminal libghostty-vt parser state, feeds
//! it the bytes the daemon streams, and renders the resulting cell
//! grid via `lazybox_tui_term::GhosttyTerminal`.
//!
//! ## Why per-client emulation
//!
//! The daemon owns the PTY but the TUI owns the renderer. The daemon
//! broadcasts raw bytes (`Event::TerminalOutput`) so a remote TUI over
//! SSH gets exactly what a local one does — the wire format is "what
//! the agent printed", not "an already-rendered cell grid". Each
//! client runs its own libghostty-vt instance and computes its own
//! viewport. Resizing is per-client (the daemon has its own size,
//! used only to size the underlying PTY).
//!
//! ## Key routing
//!
//! When the TerminalStack is focused and a live terminal is active:
//! - `Ctrl-]` / `Ctrl-o` bubble up (exit terminal mode).
//! - `Tab` moves focus to the next sibling via `Outcome::FocusNext`.
//! - Everything else emits `Command::Write` to the active terminal.
//!
//! Without focus, all keys bubble up so the sidebar / overlays pick
//! them up first.

use crate::{PaneId, PaneOutcome};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use lazybox_core::SessionKey;
use lazybox_ipc::{
    Command, Event, TerminalId, TerminalInputIntent, TerminalKind, TerminalResyncRequest,
};
use lazybox_tui_term::GhosttyTerminal;
use libghostty_vt as vt;
use ratatui::Frame;
use ratatui::prelude::*;
use ratatui::widgets::*;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Default cell grid size for new terminals before the first
/// resize-from-render. Sized to match a typical agent default; the
/// renderer overrides as soon as it knows the actual viewport.
const DEFAULT_COLS: u16 = 120;
const DEFAULT_ROWS: u16 = 32;

/// Configured per-terminal client-VT scrollback depth, in lines. Must
/// mirror the tmux backend's `history-limit`: a deep-scrollback fetch
/// (`apply_scrollback`) replays tmux's full retained history into the
/// local VT, and a shallower cap here would silently clip everything past
/// the last N lines — the client would show only a fraction of the
/// history the daemon actually holds. Set once from
/// `terminal.scrollback_lines` via [`TerminalStack::apply_ui_defaults`];
/// the default covers any VT built before config lands (tests, the brief
/// window before `apply_ui_defaults`). Process-wide because the VTs that
/// read it are built in static contexts (`TerminalVt::new`, `reset`, the
/// `apply_scrollback` scratch parser) with no handle to per-stack config.
/// Shares [`lazybox_config::DEFAULT_SCROLLBACK_LINES`] so the client
/// fallback and the config default can't drift.
static CLIENT_SCROLLBACK_LINES: AtomicUsize =
    AtomicUsize::new(lazybox_config::DEFAULT_SCROLLBACK_LINES as usize);

/// The client VT's scrollback depth, in lines, for the current config.
///
/// This used to convert the line count into a *byte* budget with a
/// width-dependent estimate (`4096` bytes/line, sized for a ~275-col pane),
/// because libghostty's old `max_scrollback` capped page memory despite its
/// "number of lines" documentation, and was fixed at creation before the
/// real width was known. Upstream now takes a real line limit, so the
/// estimate — and the silent width sensitivity it carried — is gone: a pane
/// of any width retains the configured number of lines.
fn client_scrollback_lines() -> usize {
    CLIENT_SCROLLBACK_LINES.load(Ordering::Relaxed)
}

/// Memory backstop for the client VT, in bytes, paired with
/// [`client_scrollback_lines`].
///
/// Lines are the *policy*; this is only a ceiling so a pathologically wide
/// pane can't turn a line count into unbounded memory. It reuses the
/// per-line figure the old byte budget was built on, which keeps the memory
/// ceiling exactly where it already was while letting the line limit — not a
/// width guess — decide retention at any normal width.
///
/// It must be set explicitly: a fresh libghostty terminal carries a small
/// default byte limit that otherwise binds long before the line limit,
/// capping a 50_000-line request at a few hundred rows.
const CLIENT_SCROLLBACK_BYTES_PER_LINE: usize = 4096;

/// Hard per-slot ceiling on the VT byte backstop (2026-08-19 audit,
/// M2). The per-line figure above is load-bearing for wide panes
/// (#857: libghostty's per-row page cost grows with width), but the
/// product multiplies by EVERY session's slot in one process — a
/// 50k-line config made each VT a 195 MiB liability, 67 live
/// terminals a 13 GB theoretical ceiling. 64 MiB still holds the full
/// 50k lines up to ~300 cols equivalent; only extreme line-count ×
/// width combinations trade depth for a bounded process.
const CLIENT_SCROLLBACK_MAX_BYTES: usize = 64 * 1024 * 1024;

fn client_scrollback_bytes() -> Option<usize> {
    Some(
        client_scrollback_lines()
            .saturating_mul(CLIENT_SCROLLBACK_BYTES_PER_LINE)
            .min(CLIENT_SCROLLBACK_MAX_BYTES),
    )
}

/// Cap for the per-terminal recent-output buffer.
///
/// libghostty-vt holds the canonical cell grid for rendering, but
/// agent-state detection (Claude's "Are you sure?" prompts, error
/// markers, etc.) needs to pattern-match raw bytes — re-extracting
/// them from the cell grid loses the escape sequences. So we keep a
/// rolling window of the last ~4 KiB of bytes the daemon streamed in.
/// 4 KiB is enough to span any prompt the agents have shipped so far.
pub const RECENT_OUTPUT_CAP: usize = 4 * 1024;

/// Copy `area` out of a frame buffer into an owned buffer keyed to
/// that same area — the composed-frame cache behind the U1 render
/// gate. ~10k cell clones for a full-window tile: three orders of
/// magnitude cheaper than the FFI walk it lets later frames skip.
fn copy_frame_region(
    src: &ratatui::buffer::Buffer,
    area: ratatui::layout::Rect,
) -> ratatui::buffer::Buffer {
    let mut out = ratatui::buffer::Buffer::empty(area);
    let bounded = area.intersection(src.area);
    for y in bounded.top()..bounded.bottom() {
        for x in bounded.left()..bounded.right() {
            out[(x, y)] = src[(x, y)].clone();
        }
    }
    out
}

/// Blit the cached composed frame into the current frame buffer,
/// clipped to the intersection of the cache's area and the target
/// `area` (a frozen pane may have been resized since its freeze-frame;
/// the uncovered remainder keeps the frame's cleared cells).
fn blit_cached_frame(
    dst: &mut ratatui::buffer::Buffer,
    cached: &ratatui::buffer::Buffer,
    area: ratatui::layout::Rect,
) {
    let bounded = area.intersection(cached.area).intersection(dst.area);
    for y in bounded.top()..bounded.bottom() {
        for x in bounded.left()..bounded.right() {
            dst[(x, y)] = cached[(x, y)].clone();
        }
    }
}

/// DEC private modes carried across a deep-scrollback rebuild
/// (`apply_scrollback`). The capture replay is content-only, so any
/// mode the inner program enabled — Claude Code's mouse tracking, an
/// app's DECCKM / bracketed paste — would be lost with the parser
/// reset and never re-asserted (tmux believes the client still has
/// them). Read before the reset, re-fed as `CSI ? <n> h/l` after.
/// `GRAPHEME_CLUSTER` matters more than most: programs set it once at
/// startup and it changes how the parser lays out every subsequent
/// wide char, so losing it would skew emoji/CJK rendering for the rest
/// of the session.
const PRESERVED_DEC_MODES: &[vt::terminal::Mode] = &[
    vt::terminal::Mode::DECCKM,
    vt::terminal::Mode::CURSOR_VISIBLE,
    vt::terminal::Mode::X10_MOUSE,
    vt::terminal::Mode::NORMAL_MOUSE,
    vt::terminal::Mode::BUTTON_MOUSE,
    vt::terminal::Mode::ANY_MOUSE,
    vt::terminal::Mode::FOCUS_EVENT,
    vt::terminal::Mode::UTF8_MOUSE,
    vt::terminal::Mode::SGR_MOUSE,
    vt::terminal::Mode::ALT_SCROLL,
    vt::terminal::Mode::URXVT_MOUSE,
    vt::terminal::Mode::SGR_PIXELS_MOUSE,
    vt::terminal::Mode::BRACKETED_PASTE,
    vt::terminal::Mode::GRAPHEME_CLUSTER,
];

/// Cap on the raw bytes buffered for a terminal that isn't currently
/// on screen. Off-screen terminals defer the (expensive) VT parse and
/// just stash bytes here; the parser is fed lazily on the first render
/// after the terminal becomes visible. This bounds the *between-render*
/// backlog of a chatty hidden agent — when the next append would exceed
/// it, we feed the complete ordered batch into the parser and start a
/// fresh deferred batch. It is
/// deliberately independent of (and smaller than) the daemon's
/// `REPLAY_RING_BYTES`: a `Snapshot`'s recovery replay is stashed into
/// this buffer *uncapped* (it seeds a brand-new slot, is already
/// bounded by the daemon ring, and trimming it would drop scrollback);
/// the cap only trims *live* output appended while hidden.
const PENDING_FEED_CAP: usize = 64 * 1024;

/// Cap for the per-terminal composing buffer (the in-flight user
/// message that will commit on the next Enter). Practical agent
/// prompts fit in a few KB; this bound exists to keep a pathological
/// paste (a multi-MB blob dropped into the terminal) from sitting in
/// memory unbounded until the user finally hits Enter or abandons.
pub const COMPOSING_CAP: usize = 8 * 1024;

/// Visible prefix on the pinned "latest user message" recap row.
/// Whitespace + the box-drawing wedge reads as "input direction"
/// without being visually loud.
const RECAP_PREFIX: &str = "you ▸ ";

/// Client-side cap on the retained per-terminal prompt history. Keeps
/// the optimistic (pre-reconnect) history bounded to match the daemon's
/// own eviction; the authoritative capped list arrives on the next
/// snapshot. Deliberately loose — the daemon owns the hard budget.
const PROMPT_HISTORY_CAP: usize = 200;

/// Wall-clock milliseconds since the Unix epoch, for stamping a prompt
/// at submit time. Generated once client-side and persisted verbatim so
/// a single timestamp follows the entry through the daemon and back.
fn now_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

#[derive(Clone, Copy)]
struct ByteFingerprint {
    len: usize,
    newlines: usize,
    hash: u64,
}

fn byte_fingerprint(bytes: &[u8]) -> ByteFingerprint {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    ByteFingerprint {
        len: bytes.len(),
        newlines: bytes.iter().filter(|&&byte| byte == b'\n').count(),
        hash,
    }
}

fn debug_byte_fingerprint(bytes: &[u8]) -> Option<ByteFingerprint> {
    tracing::enabled!(tracing::Level::DEBUG).then(|| byte_fingerprint(bytes))
}

#[cfg(test)]
mod fingerprint_tests {
    use super::debug_byte_fingerprint;

    #[test]
    fn disabled_debug_logging_skips_fingerprint_work() {
        tracing::subscriber::with_default(tracing::subscriber::NoSubscriber::default(), || {
            assert!(debug_byte_fingerprint(b"large replay").is_none());
        });
    }
}

/// Build a one-entry `Typed` prompt history from an optional last
/// message, for tests that used to pass a single `Option<String>` recap.
#[cfg(test)]
fn typed_history(text: Option<&str>) -> Vec<lazybox_ipc::UserPrompt> {
    text.map(|t| lazybox_ipc::UserPrompt {
        text: t.to_string(),
        timestamp_ms: 0,
        source: lazybox_ipc::PromptSource::Typed,
    })
    .into_iter()
    .collect()
}

/// Collapse a possibly multi-line user message down to a single
/// line of plain text for the pinned recap row. Newlines and runs
/// of whitespace become single spaces so multi-line prompts
/// (Shift-Enter inside Claude) render as `fix bug in foo.rs and
/// retry` instead of `fix bug in foo.rs⏎and retry` with a visible
/// gap.
///
/// Dragged/pasted file and image paths injected ahead of the prose
/// are first collapsed to `[image]` / `[file]` so the recap doesn't
/// open with a 100-char absolute path. See [`collapse_injected_path`].
fn summarize_message(msg: &str) -> String {
    let collapsed = collapse_injected_path(msg);
    let mut out = String::with_capacity(collapsed.len());
    for word in collapsed.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

/// Image file extensions that collapse to `[image]`; everything else
/// path-shaped collapses to `[file]`.
const IMAGE_EXTS: [&str; 13] = [
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "svg", "tiff", "tif", "heic", "heif", "avif", "ico",
];

fn is_image_ext(ext: &str) -> bool {
    IMAGE_EXTS.iter().any(|e| ext.eq_ignore_ascii_case(e))
}

/// True if `s` opens with an unambiguous path prefix (`/`, `~/`,
/// `./`, `../`). Same shape the click-target detector requires in
/// [`find_path_at_byte`], kept in sync so the recap heuristic and the
/// clickable-path detection agree on what counts as a path.
fn looks_like_path(s: &str) -> bool {
    s.starts_with('/') || s.starts_with("~/") || s.starts_with("./") || s.starts_with("../")
}

/// Extension of the final path segment, ignoring a leading-dot
/// dotfile (`.bashrc` has no extension).
fn filename_ext(path: &str) -> Option<&str> {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let (stem, ext) = name.rsplit_once('.')?;
    (!stem.is_empty()).then_some(ext)
}

/// Walk from `start` to the first unescaped whitespace, treating
/// `\ ` as part of the token. Drag-and-drop shells escape spaces in
/// paths, so `CleanShot\ 2026.png` is one logical token even though
/// `split_whitespace` would tear it apart.
fn unescaped_ws_end(msg: &str, start: usize) -> usize {
    let mut escaped = false;
    for (rel, c) in msg[start..].char_indices() {
        if c.is_whitespace() && !escaped {
            return start + rel;
        }
        escaped = c == '\\' && !escaped;
    }
    msg.len()
}

/// Find the first plausible file extension at/after `start`: a `.`
/// followed by 1–5 alphanumerics that begin with a letter and end at
/// whitespace or end-of-string. Returns the byte index just past the
/// extension and the extension itself.
///
/// This anchors the path's end even when the path contains *raw*
/// (unescaped) spaces — e.g. macOS' `…/Application Support/CleanShot
/// 2026-06-02 at 11.35.48@2x.png` — which a token scan can't bound.
/// The all-digit guard skips dotted version-ish runs like the `.35`
/// and `.48` in that timestamp.
fn first_path_extension(msg: &str, start: usize) -> Option<(usize, &str)> {
    let hay = &msg[start..];
    let bytes = hay.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'.' {
            let s = i + 1;
            let mut j = s;
            while j < bytes.len() && bytes[j].is_ascii_alphanumeric() {
                j += 1;
            }
            let after_ok = j >= bytes.len() || bytes[j].is_ascii_whitespace();
            if (1..=5).contains(&(j - s)) && bytes[s].is_ascii_alphabetic() && after_ok {
                return Some((start + j, &hay[s..j]));
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    None
}

/// End byte and placeholder for an injected path beginning at `start`
/// (a word boundary), or `None` if the token there isn't path-shaped.
///
/// `leading` (nothing but whitespace precedes `start`) is the high-
/// confidence drag/paste position, so any path there collapses. A
/// path mid-prose only collapses when it carries an image extension —
/// a typed `/etc/hosts` reference is left intact, but an appended
/// screenshot path still folds to `[image]`.
fn detect_path_at(msg: &str, start: usize, leading: bool) -> Option<(usize, &'static str)> {
    let rest = &msg[start..];
    let first = rest.chars().next()?;

    if first == '"' || first == '\'' {
        let inner_start = start + first.len_utf8();
        let close = msg[inner_start..].find(first)?;
        let inner = &msg[inner_start..inner_start + close];
        if !looks_like_path(inner) {
            return None;
        }
        let placeholder = match filename_ext(inner) {
            Some(ext) if is_image_ext(ext) => "[image]",
            _ => "[file]",
        };
        return Some((inner_start + close + first.len_utf8(), placeholder));
    }

    if !looks_like_path(rest) {
        return None;
    }

    if let Some((end, ext)) = first_path_extension(msg, start) {
        if is_image_ext(ext) {
            return Some((end, "[image]"));
        }
        if leading {
            return Some((end, "[file]"));
        }
        return None;
    }

    leading.then(|| (unescaped_ws_end(msg, start), "[file]"))
}

/// Replace dragged/pasted file paths with `[image]` / `[file]`
/// placeholders, preserving the surrounding prose. Borrows the input
/// unchanged when no path is found. Runs on every recap render, so it
/// is a single left-to-right pass with a bounded extension probe per
/// path-prefixed token.
fn collapse_injected_path(msg: &str) -> std::borrow::Cow<'_, str> {
    let mut result: Option<String> = None;
    let mut cursor = 0;
    let mut i = 0;
    while i < msg.len() {
        let boundary = i == 0
            || msg[..i]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_whitespace());
        if boundary {
            let leading = msg[..i].chars().all(|c| c.is_whitespace());
            if let Some((end, placeholder)) = detect_path_at(msg, i, leading) {
                let out = result.get_or_insert_with(String::new);
                out.push_str(&msg[cursor..i]);
                out.push_str(placeholder);
                cursor = end;
                i = end;
                continue;
            }
        }
        i += msg[i..].chars().next().map_or(1, |c| c.len_utf8());
    }
    match result {
        Some(mut out) => {
            out.push_str(&msg[cursor..]);
            std::borrow::Cow::Owned(out)
        }
        None => std::borrow::Cow::Borrowed(msg),
    }
}

/// Which end of the terminal scrollback prevented a viewport move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollBoundary {
    /// The oldest available scrollback row.
    Top,
    /// The live terminal viewport.
    Bottom,
}

/// Outcome of a terminal viewport scroll.
///
/// Success is based on the observed before/after scrollbar offsets,
/// never merely on scrollback being present. Expected no-ops (an empty
/// scrollback, a zero delta, or an already-reached boundary) are typed
/// separately from [`Self::Stalled`], which means libghostty accepted a
/// request that should have moved but its offset stayed put.
#[must_use = "a terminal scroll outcome must be observed so a stalled viewport cannot fail silently"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollOutcome {
    /// No focused terminal (Tabs mode with no active tab, or an
    /// empty session).
    NoTerminal,
    /// `total <= len`: the terminal hasn't produced enough output to
    /// fill the active area + spill into scrollback yet.
    NoScrollback,
    /// The request deliberately asked for no movement (`By(0)`).
    Noop,
    /// The viewport was already at the requested edge.
    AtBoundary {
        boundary: ScrollBoundary,
        offset: u64,
        total: u64,
        len: u64,
    },
    /// The VT scrollbar could not be read before or after the request.
    StateUnavailable,
    /// Scrollback existed and the viewport was not at a boundary, but
    /// libghostty's offset did not change. This is the typed regression
    /// signal for the formerly silent Delta-scroll failure.
    Stalled {
        request: ScrollRequest,
        offset: u64,
        total: u64,
        len: u64,
    },
    /// Scroll succeeded. Carries both offsets so callers and tests can
    /// verify the reported transition against the viewport state.
    Moved {
        from: u64,
        offset: u64,
        total: u64,
        len: u64,
    },
}

/// A viewport scroll request — the entire vocabulary the scroll owner
/// (`TerminalVt::scroll`) accepts. Every scroll surface (wheel,
/// `Shift-PgUp/PgDn`, `Shift-Home/End`, the per-tile hover scroll)
/// speaks only these three verbs; nothing outside `TerminalVt::scroll`
/// calls libghostty's `scroll_viewport` directly. That single choke
/// point is what makes a silent no-op impossible (the #42/#371 promise):
/// a request either moves the viewport or comes back with a typed
/// [`ScrollOutcome`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollRequest {
    /// Move by a signed row delta — negative scrolls up into
    /// scrollback, positive scrolls down toward the live content.
    By(isize),
    /// Jump the viewport to the top of scrollback.
    Top,
    /// Jump the viewport to the live bottom.
    Bottom,
}

/// Classify the observed state transition after a request that passed
/// the empty-scrollback and boundary preflight checks.
fn classify_scroll_transition(
    request: ScrollRequest,
    before: vt::terminal::Scrollbar,
    after: vt::terminal::Scrollbar,
) -> ScrollOutcome {
    if after.offset == before.offset {
        ScrollOutcome::Stalled {
            request,
            offset: after.offset,
            total: after.total,
            len: after.len,
        }
    } else {
        ScrollOutcome::Moved {
            from: before.offset,
            offset: after.offset,
            total: after.total,
            len: after.len,
        }
    }
}

/// Whether a scroll outcome leaves the viewport at the live bottom —
/// the signal that ends a deep-scrollback visit and re-arms the fetch
/// (#393). An empty scrollback is trivially at the bottom.
fn at_live_bottom(outcome: ScrollOutcome) -> bool {
    match outcome {
        ScrollOutcome::NoScrollback => true,
        ScrollOutcome::AtBoundary {
            boundary: ScrollBoundary::Bottom,
            ..
        } => true,
        ScrollOutcome::Moved {
            offset, total, len, ..
        } => offset + len >= total,
        _ => false,
    }
}

/// What the user right-clicked on inside the terminal grid. Returned
/// by [`TerminalStack::target_at`] so the model can route each kind
/// to the right opener: URLs and issue references go to the browser,
/// file paths to the configured editor. Detection is passive — any
/// matching token rendered in the transcript is clickable, no markup
/// required.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClickTarget {
    /// `http(s)://…` — open in the system browser.
    Url(String),
    /// A filesystem path, exactly as it appeared on screen (possibly
    /// still `~`- or `.`-relative). The model resolves it against the
    /// focused session's worktree before opening in the editor.
    /// `line`/`col` come from an optional `:line[:col]` suffix.
    Path {
        path: String,
        line: Option<u32>,
        col: Option<u32>,
    },
    /// `#42` or `owner/repo#42` — open the GitHub issue/PR page.
    /// `repo` is `None` for a bare `#42`, which the model resolves
    /// against the focused workspace's repo.
    Issue { repo: Option<String>, number: u64 },
}

#[derive(Debug, Clone, Copy)]
struct TerminalHit {
    terminal_id: TerminalId,
    tile: Rect,
    body: Rect,
    /// The exact cell-grid rect the renderer drew for this terminal —
    /// the body with the recap rows and the scrollbar gutter already
    /// removed. Recorded so every screen↔grid coordinate mapping reads
    /// back the geometry the frame actually used instead of recomputing
    /// the recap/chrome inset and drifting by a row when they disagree
    /// (#1021).
    grid: Rect,
    /// The viewport scroll offset (`Scrollbar::offset`) of the frame this
    /// hit was recorded for — the screen row of the top visible grid row.
    /// The selection mapping composes a click against THIS offset, not the
    /// live VT offset, so output that advanced the viewport between the
    /// painted frame and the click can't shift the selection by the scroll
    /// delta (#1021). `None` when the scrollbar couldn't be read at render.
    offset: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RenderedClickTarget {
    pub(crate) terminal_id: TerminalId,
    pub(crate) tile: Rect,
    pub(crate) body: Rect,
    pub(crate) cell: Option<(u32, u32)>,
    pub(crate) target: Option<ClickTarget>,
}

pub struct TerminalStack {
    id: PaneId,
    terminals: HashMap<TerminalId, TerminalSlot>,
    /// Bumped whenever the visible-set inputs change (slot
    /// membership / kinds / active session) — invalidates
    /// `visible_cache` (#1237): `visible_terminals` used to rebuild
    /// and sort the whole slot map twice per output event.
    slots_rev: std::cell::Cell<u64>,
    /// `(slots_rev, ordered visible ids)` memo for
    /// [`Self::visible_terminals`].
    visible_cache: std::cell::RefCell<Option<(u64, Vec<TerminalId>)>>,
    /// Which session's terminals are currently visible. `None` =>
    /// render an empty-state message.
    active_session: Option<SessionKey>,
    /// Index into `visible_terminals()`. Clamped on every tab change /
    /// visible-set mutation so it can never point out of range.
    active_tab_idx: usize,
    /// Whether the body is collapsed to its header row. The app's
    /// `build_layout` reads this to give the pane a 1-row slot
    /// instead of its share of the right column. Default: collapsed
    /// when there are no terminals (we show the empty hint inline in
    /// the header rather than wasting the bottom 75% of the screen).
    collapsed: bool,
    /// Once the user explicitly toggles, stop auto-collapsing on
    /// emptiness. Same dance as `RightPane::activity_collapse_user_set`.
    collapse_user_set: bool,
    /// Tile/tab arrangement for the active session. Defaults to
    /// `Tabs` so the legacy single-runner-full-pane UX keeps working
    /// when no split has ever been requested. Mutating this triggers
    /// a `Command::SetSessionLayout` so the daemon persists.
    layout: lazybox_core::SessionLayout,
    /// Pending split operation: when the user hits `]]|` we
    /// emit `Command::Spawn` for a new shell, then once the
    /// `TerminalSpawned` event arrives we wrap the focused leaf in a
    /// fresh split with the new terminal. `Some((direction, armed_at))`
    /// means "the next active-session spawn within
    /// [`PENDING_SPLIT_WINDOW`] becomes the new sibling on this axis" —
    /// the timestamp keeps a marker whose spawn failed daemon-side from
    /// hijacking an unrelated spawn much later.
    pending_split: Option<(PendingSplit, std::time::Instant)>,
    /// tmux-style zoom (#1057): while set, the Splits grid renders only
    /// the focused tile across the whole pane, then restores the grid on
    /// the next `]]z`. A transient view state — deliberately NOT persisted
    /// (it's a momentary "read one closely" motion, not a layout), and
    /// cleared whenever the tree changes underfoot (split, close, focus
    /// move, session switch, daemon-synced layout) so it can't outlive the
    /// tile it maximized.
    zoomed: bool,
    /// The last layout applied via [`Self::set_layout`] for the active
    /// session — i.e. the persisted daemon-side state as this client
    /// last saw it. `set_layout` skips re-projections equal to it so
    /// local layout mutations survive the sync that runs on every
    /// event (see the method docs). Reset on session switch: equality
    /// against another session's layout means nothing.
    synced_layout: Option<lazybox_core::SessionLayout>,
    /// Resizes recorded during render and waiting to be drained by
    /// the App loop. Each entry is `(terminal_id, cols, rows)` — the
    /// App turns them into `Command::Resize` and ships them at the
    /// next loop tick. Drained on every `drain_pending_resizes`.
    pending_resizes: Vec<(TerminalId, u16, u16)>,
    /// Client-observed output gaps waiting for the model to request an
    /// authoritative daemon replay. `(terminal_id, required_seq)`.
    /// Terminal ids whose authoritative debt is ready for wire admission.
    /// The sequence watermark itself lives only in `TerminalSlot::sync`.
    pending_resync_requests: Vec<TerminalId>,
    /// Click targets for the tab strip, populated each render. Each
    /// entry is `(tab_idx, (start_col, end_col_exclusive), row)`.
    /// `handle_tab_click(col, row)` scans this on mouse-down to map
    /// a click on the `claude` / `shell` label to a tab switch.
    /// Cleared at the start of every render so removed terminals
    /// don't leave stale hit targets.
    tab_strip_hits: Vec<(usize, std::ops::Range<u16>, u16)>,
    /// Per-tile mouse targets, populated each render — one entry per
    /// visible terminal. The full tile drives click-to-focus while its
    /// body preserves the narrower hover-to-scroll target. Cleared at
    /// the start of every render so a closed tile leaves no stale target.
    tile_hits: Vec<TerminalHit>,
    /// Last-focused terminal per session. Recorded when we leave a
    /// session so returning restores the pane the user was last on
    /// instead of snapping back to the first. Keyed by terminal id
    /// (not tab index) so it survives the agent-first reordering of
    /// `visible_terminals`.
    last_focused: HashMap<SessionKey, TerminalId>,
    /// Terminals the user explicitly asked to close (`]]x` →
    /// `Command::Close`). The returning `TerminalExited` for one of
    /// these tears the pane down like any terminal; an agent that
    /// exited on its OWN (crash, killed binary — #356) is instead kept
    /// as a frozen "exited — restart?" pane. Drained on that event.
    closing: HashSet<TerminalId>,
    /// Grace window: an agent that exits `code 0` within this window of
    /// its spawn without ever engaging is treated as dead-on-arrival —
    /// its pane is kept (frozen + restart) rather than auto-closed. A
    /// clean exit past the window, or after the agent engaged, auto-
    /// closes. Sourced from `terminal.agent_dead_on_arrival_ms` via
    /// `apply_ui_defaults`; the const is the fallback for tests and any
    /// stack built before config lands. See #367.
    dead_on_arrival: std::time::Duration,
    /// User preference (`ui.terminal_new_layout`) for how an
    /// auto-spawned second-or-later terminal lands: `Split`
    /// (side-by-side tiles, the default) or `Tabs` (stacked behind the
    /// tab strip). Only consulted for the automatic layout on
    /// `TerminalSpawned`; explicit `]]|` / `]]-` splits ignore it.
    terminal_new_layout: lazybox_config::NewTerminalLayout,
    /// Terminal whose deep scrollback should be fetched from the daemon
    /// (`Command::FetchScrollback`). Armed by `scroll_terminal` /
    /// `scroll_to_top` when the user scrolls up into local scrollback —
    /// the local libghostty history of a live full-screen agent holds
    /// almost nothing (in-place redraws), while tmux has been retaining
    /// `history-limit` lines the whole time (#393). Drained by the key /
    /// wheel handlers into an outgoing command.
    pending_scrollback_fetch: Option<TerminalId>,
}

/// Records that a terminal's process has exited. Agent terminals keep
/// their slot when this is set (frozen last screen + a restart banner)
/// instead of the whole pane vanishing on a crash (#356).
#[derive(Debug, Clone)]
struct TerminalExit {
    /// Exit code the daemon reported, or `None` when it couldn't — e.g.
    /// death by signal (the classic outcome when a Homebrew self-upgrade
    /// swaps the agent binary out mid-run, #355).
    code: Option<i32>,
    /// The agent exited within seconds of spawn — a startup failure, not
    /// a real session end (#368). An immediate `code 0` otherwise reads
    /// as success; this makes the banner say "failed to start".
    dead_on_arrival: bool,
    /// The cleaned tail of the agent's last output, painted over an
    /// otherwise-blank frozen pane so a dead-on-arrival exit shows *why*
    /// instead of a black screen (#368).
    last_output: Option<String>,
}

/// How long an armed pending split waits for its shell's
/// `TerminalSpawned` before it stops claiming the next spawn. The
/// in-process round trip is sub-second; the window only has to beat
/// a daemon-side spawn that silently failed.
const PENDING_SPLIT_WINDOW: std::time::Duration = std::time::Duration::from_secs(10);

/// Fallback dead-on-arrival grace window (see
/// [`TerminalStack::dead_on_arrival`]) until config is applied.
const DEFAULT_DEAD_ON_ARRIVAL: std::time::Duration = std::time::Duration::from_secs(10);

/// Direction of a pending split. `Vertical` = `|` = side-by-side =
/// `HSplit`. `Horizontal` = `-` = stacked = `VSplit`. (Vim
/// vocabulary, which is what most users will type.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingSplit {
    Vertical,
    Horizontal,
}

/// Pure scanner: return the byte ranges of all OSC 52 clipboard-set
/// sequences inside `bytes`. Used by `forward_osc52` to know what
/// to write to stdout. Pure so the matching logic is unit-testable
/// without redirecting stdout.
///
/// OSC 52 format: `ESC ] 52 ; <selection> ; <base64-data> ST` where
/// `ST` is either BEL (0x07) or `ESC \` (0x1b 0x5c). The `<selection>`
/// char picks the clipboard target (`c` = clipboard, `p` = primary,
/// `s` = selection, etc.); we don't care which one, the host decides.
///
/// Returned ranges are non-overlapping and in input order. The second
/// element is the start index of a trailing OSC 52 that began but did
/// NOT terminate before end-of-bytes — either a complete header whose
/// base64 payload/terminator is still arriving, or a partial header
/// (`\x1b]5…`) split mid-sequence. The caller carries those bytes into
/// the next chunk so a clipboard copy split across PTY reads isn't
/// dropped.
pub(crate) fn osc52_scan(bytes: &[u8]) -> (Vec<std::ops::Range<usize>>, Option<usize>) {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 5 <= bytes.len() {
        if bytes[i] == 0x1b
            && bytes[i + 1] == b']'
            && bytes[i + 2] == b'5'
            && bytes[i + 3] == b'2'
            && bytes[i + 4] == b';'
        {
            let start = i;
            let mut j = i + 5;
            let end = loop {
                if j >= bytes.len() {
                    // Unterminated — report so the caller carries the
                    // tail into the next chunk instead of dropping it.
                    return (out, Some(start));
                }
                if bytes[j] == 0x07 {
                    break j + 1;
                }
                if bytes[j] == 0x1b && j + 1 < bytes.len() && bytes[j + 1] == b'\\' {
                    break j + 2;
                }
                j += 1;
            };
            out.push(start..end);
            i = end;
            continue;
        }
        i += 1;
    }
    // The 5-byte header itself can straddle the tail (chunk ends with
    // `\x1b`, `\x1b]`, `\x1b]5`, or `\x1b]52`). Carry a trailing proper
    // prefix of the header so the next chunk can complete it.
    const HEADER: &[u8] = b"\x1b]52;";
    let tail_start = bytes.len().saturating_sub(HEADER.len() - 1);
    for s in tail_start..bytes.len() {
        let tail = &bytes[s..];
        if tail.len() < HEADER.len() && HEADER.starts_with(tail) {
            return (out, Some(s));
        }
    }
    (out, None)
}

/// Convenience wrapper returning just the complete OSC 52 ranges.
#[cfg(test)]
pub(crate) fn osc52_ranges(bytes: &[u8]) -> Vec<std::ops::Range<usize>> {
    osc52_scan(bytes).0
}

/// Upper bound on the OSC 52 carry buffer. A clipboard payload larger
/// than this (after base64) is dropped rather than buffered without
/// limit — protects against a stream that opens `\x1b]52;` and never
/// terminates.
const OSC52_CARRY_CAP: usize = 4 * 1024 * 1024;

/// Forward any OSC 52 clipboard-set escape sequences in `bytes` to
/// the host terminal writer, so the inner program's "copy this"
/// requests reach the user's system clipboard.
///
/// We pass-through the WHOLE sequence verbatim (including the
/// terminators) so modern host terminals (Ghostty, iTerm2, Kitty,
/// Wezterm, tmux's allow-passthrough) honor it. Anything else in
/// `bytes` we leave alone — libghostty-vt handles rendering as
/// usual. Multiple OSC 52 sequences in one chunk are all forwarded.
///
/// `carry` buffers an OSC 52 that began in a previous chunk but hadn't
/// terminated yet — a large base64 clipboard payload routinely spans
/// PTY reads, and without carrying it the whole copy was silently
/// dropped. The carried head is prepended before scanning; only fully
/// terminated sequences are written to the host.
///
/// Best-effort: writer failures are ignored. The raw escape shares the
/// render writer's ordered lane, so it cannot splice into a ratatui frame or
/// race the crash-restore muzzle (#1170).
fn forward_osc52(carry: &mut Vec<u8>, bytes: &[u8]) {
    let combined: Vec<u8>;
    let scan: &[u8] = if carry.is_empty() {
        bytes
    } else {
        let mut v = std::mem::take(carry);
        v.extend_from_slice(bytes);
        combined = v;
        &combined
    };

    let (ranges, pending) = osc52_scan(scan);
    for range in ranges {
        crate::realm::model::render_writer::enqueue_raw(&scan[range]);
    }

    // Carry any unterminated trailing sequence into the next chunk,
    // bounded so a never-terminating stream can't grow it without limit.
    if let Some(start) = pending {
        let tail = &scan[start..];
        if tail.len() <= OSC52_CARRY_CAP {
            *carry = tail.to_vec();
        }
    }
}

/// Read-only walk of a `TileTree` along a path. Returns None if the
/// path tries to descend through a leaf.
fn subtree_at_path<'a>(
    root: &'a lazybox_core::TileTree,
    path: &[u8],
) -> Option<&'a lazybox_core::TileTree> {
    let mut node = root;
    for &step in path {
        node = match node {
            lazybox_core::TileTree::HSplit { left, right, .. }
            | lazybox_core::TileTree::VSplit {
                top: left,
                bottom: right,
                ..
            } => {
                if step == 0 {
                    left.as_ref()
                } else {
                    right.as_ref()
                }
            }
            lazybox_core::TileTree::Leaf { .. } => return None,
        };
    }
    Some(node)
}

/// First retry delay after a `TerminalResyncUnavailable` reply (#1254
/// finding 2). Without a tick-driven retry, a desynced pane whose agent
/// already finished had nothing left to re-drive its request — new
/// output was the only trigger — and froze forever.
const RESYNC_RETRY_INITIAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Backoff cap for the resync retry loop. Retries never stop
/// (advise-never-forbid: the pane must eventually converge); they just
/// slow to this cadence while the daemon keeps answering "unavailable".
const RESYNC_RETRY_CAP: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalStreamSync {
    Coherent,
    /// The one client-side owner of terminal recovery debt. `request` is the
    /// exact wire shape the daemon must satisfy; `request_pending` is only an
    /// admission latch, not a second sequence watermark.
    Desynced {
        request: TerminalResyncRequest,
        request_pending: bool,
    },
}

impl TerminalStreamSync {
    fn is_desynced(self) -> bool {
        matches!(self, Self::Desynced { .. })
    }
}

struct TerminalSlot {
    session_key: SessionKey,
    kind: TerminalKind,
    last_seq: u64,
    /// True after a sequence gap or an unavailable recovery snapshot.
    /// While set, live output is ignored so a torn byte stream cannot
    /// mutate the last coherent grid. Only an authoritative resync clears
    /// the debt.
    sync: TerminalStreamSync,
    /// When a `Desynced { request_pending: false }` slot should re-issue
    /// its resync request (#1254 finding 2). Armed by a
    /// `TerminalResyncUnavailable` reply; consumed by
    /// [`TerminalStack::tick_resync_retries`], which runs from the UI
    /// tick loop so recovery is driven by TIME, not by new output — a
    /// quiescent pane converges too. `None` while a request is in
    /// flight or the slot is coherent.
    resync_retry_at: Option<std::time::Instant>,
    /// Next retry delay: doubles per consecutive `Unavailable` reply
    /// from [`RESYNC_RETRY_INITIAL`] up to [`RESYNC_RETRY_CAP`]; reset
    /// on a successful resync.
    resync_retry_backoff: std::time::Duration,
    /// libghostty-vt parser. Each client owns its own — the daemon
    /// streams raw bytes; this is what turns them into a cell grid.
    /// `Box`ed so moving `TerminalSlot` doesn't move the inner FFI
    /// allocator pointers (they self-reference).
    vt: Box<TerminalVt>,
    /// Cap of recent raw bytes (post-feed). Pure debug aid; tests
    /// inspect it. Not used for rendering.
    recent: Vec<u8>,
    /// Buffer for an OSC 52 clipboard sequence that began in one
    /// `append_output` chunk but hasn't terminated yet — carried so a
    /// large copy split across PTY reads isn't dropped. See
    /// [`forward_osc52`].
    osc52_carry: Vec<u8>,
    /// Agent state cached from the daemon's `Event::AgentState`
    /// broadcasts. Drives the "needs input" / "working" badge in the
    /// tab strip. Default `Idle` so non-agent terminals (shells) carry
    /// a neutral state and never falsely show working.
    agent_state: lazybox_ipc::AgentState,
    /// Last (cols, rows) we rendered this terminal at. Used to detect
    /// pane resizes — when the rect changes between frames we push a
    /// `Command::Resize` so the backend PTY sees the new size and the
    /// shell process resizes its own view (otherwise output beyond
    /// the original spawn size never gets written and the user sees
    /// a frozen-looking pane).
    last_rendered_size: Option<(u16, u16)>,
    /// The last fully composed frame (cursor overlay included) and the
    /// `(content_rev, area)` it was painted at (2026-08-19 audit, U1).
    /// When the VT saw no mutation since that paint, the render path
    /// blits this instead of re-running the full per-cell FFI grid walk
    /// (~50k FFI calls for a full-window tile) — the dominant term in
    /// the observed 100–600 ms render stalls. Invalidated (rev slot set
    /// to `None`) whenever the parser is replaced wholesale (resync
    /// reset, deep-scrollback adoption), since a fresh parser restarts
    /// its revision counter. Also the freeze-frame a crashed agent pane
    /// renders from after its VT is dropped (M3).
    last_frame: Option<ratatui::buffer::Buffer>,
    last_frame_rev: Option<(u64, Rect)>,
    /// Characters the user has typed since the last submit. Only
    /// tracked on Agent terminals — the pinned recap is meaningless
    /// for shells. Cleared when the user hits Enter, Ctrl-C, Ctrl-U,
    /// or Esc (the same keys that wipe the prompt buffer in Claude
    /// Code / a shell prompt).
    composing: String,
    /// Bounded, oldest-first history of the prompts submitted to this
    /// terminal (issue #523). The last entry is rendered as a one-line
    /// recap above the agent's terminal grid so it's obvious "what you
    /// just asked the model" even after pages of tool output scroll the
    /// prompt off-screen; the whole list is browsable via `]]h`. Empty
    /// until the user has submitted at least one message here. Snippet-
    /// sourced entries carry a `Snippet` source so the history can tag
    /// them.
    prompt_history: Vec<lazybox_ipc::UserPrompt>,
    /// Launched in no-permission / bypass mode (autonomous session
    /// running unattended). Drives the "no-perms" badge in the tab
    /// strip so it's obvious which sessions skip approval prompts.
    no_permission: bool,
    /// Running on the repo's shared main checkout rather than an
    /// isolated worktree. Drives the "main" badge in the tab strip so
    /// it's obvious the session sits on the shared branch.
    on_main: bool,
    /// Model-tier label the session was launched with (`"Opus"`), when
    /// the user picked a tier via a `w S` / `a S` chord. Drives the
    /// tier badge in the tab strip. `None` for a default-model spawn.
    model_label: Option<String>,
    /// Whether this terminal was drawn in the last frame. Set by
    /// `render_one_terminal`, reset for every slot at the top of
    /// `render`. Output for a displayed terminal is fed to the VT
    /// parser immediately; output for a hidden one is stashed in
    /// `pending_feed` and replayed lazily on the next render. Without
    /// this gate, M chatty off-screen agents each pay full VT-parse
    /// cost on every chunk on the single UI thread.
    displayed: bool,
    /// Raw bytes received while this terminal was hidden, awaiting a
    /// deferred replay into `vt` on the first render after it becomes
    /// visible. Bounded by [`PENDING_FEED_CAP`].
    pending_feed: Vec<u8>,
    /// Set once this (agent) terminal's process has exited on its own
    /// rather than by an explicit user close. The slot is retained so
    /// the frozen last screen stays visible and a restart banner is
    /// offered — a crashing agent (#356) must not take the workspace
    /// down with it. `None` for a live terminal.
    exited: Option<TerminalExit>,
    /// The provider login process temporarily occupying this pane.
    /// Its successful replacement must inherit this slot's tile,
    /// history, and draft rather than landing as a new terminal.
    authenticating: bool,
    /// Stable daemon recovery identity; distinct from this ephemeral auth
    /// terminal id and used by retry/cancel commands.
    auth_recovery_id: Option<TerminalId>,
    /// When this slot's terminal was spawned. Used to tell a clean
    /// exit that ran for a while from a dead-on-arrival one that
    /// exited `code 0` almost immediately without doing anything (#367).
    spawned_at: std::time::Instant,
    /// Set once this (agent) terminal reached a non-`Idle` state
    /// (`Working` / `InputNeeded` / `Done`) — i.e. it actually engaged.
    /// A clean exit auto-closes only if the agent engaged or outran the
    /// dead-on-arrival window; one that never engaged and exited fast is
    /// treated as a failed launch and kept open with a restart (#367).
    did_work: bool,
    /// A `Command::FetchScrollback` is in flight (or already served)
    /// for the current scrollback visit. One fetch per visit: armed on
    /// the first upward scroll, cleared when the viewport returns to
    /// the bottom, so parking in scrollback never re-resets the grid
    /// while a fresh visit still re-captures up-to-date history (#393).
    deep_scrollback_requested: bool,
}

impl TerminalSlot {
    /// Append one char to the composing buffer when it fits within
    /// [`COMPOSING_CAP`]; silently drop it otherwise. The bound is the
    /// whole point — a runaway auto-typer (or a pathological paste)
    /// can't grow the buffer without limit.
    fn push_composing(&mut self, c: char) {
        if self.composing.len() + c.len_utf8() <= COMPOSING_CAP {
            self.composing.push(c);
        }
    }

    /// Commit the trimmed composing buffer as the latest submitted
    /// prompt text and reset it for the next prompt. An all-whitespace
    /// buffer is ignored, so mashing Enter on an empty prompt (e.g.
    /// dismissing an agent approval) doesn't blank out the recap. Returns
    /// the committed text when one was recorded; the caller stamps it
    /// with a source + timestamp and appends it via [`Self::push_prompt`]
    /// so both the local recap and the daemon persistence agree on the
    /// single entry (`Command::RecordUserMessage`).
    fn commit_composing(&mut self) -> Option<String> {
        let trimmed = self.composing.trim();
        let committed = (!trimmed.is_empty()).then(|| trimmed.to_string());
        self.composing.clear();
        committed
    }

    /// Append one submitted prompt to this slot's bounded history,
    /// evicting oldest entries past [`PROMPT_HISTORY_CAP`]. Drives the
    /// pinned recap (last entry) and the `]]h` history view.
    fn push_prompt(&mut self, prompt: lazybox_ipc::UserPrompt) {
        self.prompt_history.push(prompt);
        let overflow = self.prompt_history.len().saturating_sub(PROMPT_HISTORY_CAP);
        if overflow > 0 {
            self.prompt_history.drain(0..overflow);
        }
    }

    /// Replay any bytes buffered while this terminal was hidden into
    /// the VT parser, then clear the buffer. A no-op when nothing was
    /// buffered. The buffer is never truncated: `append_output` flushes
    /// complete batches into the existing parser at the cap, preserving
    /// byte-stream continuity while bounding deferred memory.
    fn flush_pending(&mut self) {
        if self.pending_feed.is_empty() {
            return;
        }
        self.vt.feed(&self.pending_feed);
        self.pending_feed.clear();
    }

    /// Feed the *exact bytes* that are about to be written to this
    /// terminal's PTY into the composing buffer + last-message state.
    /// Every submit path — raw keystrokes, snippet expansion,
    /// programmatic sends — funnels its payload through a
    /// `Command::Write`, so parsing that byte stream is the one place
    /// the recap can't drift from what the agent actually received.
    ///
    /// Interprets the stream the way the agent's own line editor sees
    /// it:
    ///   - printable text → append (respecting [`COMPOSING_CAP`])
    ///   - CR (`\r`) → commit the trimmed buffer as the latest message
    ///   - LF (`\n`) → soft newline (kept in the buffer, no submit) —
    ///     this is how a multi-line snippet body arrives
    ///   - `ESC \r` / `ESC \n` (Shift-Enter) → soft newline, no submit
    ///   - DEL / BS → erase one char
    ///   - Ctrl-C / Ctrl-U → clear the line
    ///   - lone ESC (no following byte) → clear the line (prompt reset)
    ///   - `ESC [ … ` (CSI) / `ESC O …` (SS3) sequences — arrows,
    ///     mouse reports, Delete — are skipped; they aren't literal
    ///     composed text. Any other `ESC`-prefixed meta sequence drops
    ///     just the `ESC` and keeps parsing, so a stray escape can
    ///     never silently wipe an in-flight prompt.
    ///
    /// **Contract:** each call must carry one *complete* logical write
    /// (a single keystroke's bytes, or a full one-shot command).
    /// Sequences are not buffered across calls, so an escape sequence
    /// or multi-byte codepoint split between two invocations would be
    /// mis-framed. Bracketed-paste *payloads* must not be routed here —
    /// they are captured separately via [`Self::append_paste`], so this
    /// never sees an `ESC[200~ … ESC[201~` body. Decoding is lossy
    /// UTF-8: the recap is display-only, so a stray invalid byte
    /// degrades to U+FFFD rather than dropping the write.
    ///
    /// Returns the message committed by a trailing submit (CR) in this
    /// write, if any — the caller forwards it to the daemon so the
    /// recap can be restored after a restart. A write that only edits
    /// the in-flight line returns `None`. When a single write carries
    /// multiple submits (rare: a scripted multi-command paste), the
    /// last committed message wins, matching what the recap shows.
    fn record_pty_bytes(&mut self, bytes: &[u8]) -> Option<String> {
        // ECMA-48: a CSI sequence runs until its final byte, which
        // lies in 0x40..=0x7e. Intermediate / parameter bytes are all
        // below that range, so the first byte in it terminates.
        const CSI_FINAL: std::ops::RangeInclusive<char> = '@'..='~';

        let text = String::from_utf8_lossy(bytes);
        let mut chars = text.chars().peekable();
        let mut committed = None;
        while let Some(c) = chars.next() {
            match c {
                '\x1b' => match chars.peek() {
                    // CSI: `ESC [ … final`. Consume up to and
                    // including the final byte. This also swallows
                    // bracketed-paste markers (`ESC[200~`/`201~`),
                    // the Delete key, and mouse reports.
                    Some('[') => {
                        chars.next();
                        while let Some(&n) = chars.peek() {
                            chars.next();
                            if CSI_FINAL.contains(&n) {
                                break;
                            }
                        }
                    }
                    // SS3: `ESC O <final>` (cursor keys in app mode).
                    Some('O') => {
                        chars.next();
                        chars.next();
                    }
                    // Shift-Enter arrives as `ESC \r` (see
                    // `key_to_bytes`): a newline in the prompt without
                    // a submit.
                    Some('\r' | '\n') => {
                        chars.next();
                        self.push_composing('\n');
                    }
                    // Lone Esc (the real Esc key is a single 0x1b) →
                    // reset the line.
                    None => self.composing.clear(),
                    // Unrecognised `ESC`-prefixed meta sequence: drop
                    // only the ESC and let the next char be parsed
                    // normally, rather than nuking the buffer.
                    Some(_) => {}
                },
                // CR is the submit. LF is only ever a soft newline
                // here (Enter maps to CR, never LF), so a multi-line
                // snippet body commits once, on its trailing CR, not
                // at each embedded newline.
                '\r' => {
                    if let Some(msg) = self.commit_composing() {
                        committed = Some(msg);
                    }
                }
                '\n' => self.push_composing('\n'),
                // DEL / Backspace.
                '\x7f' | '\x08' => {
                    self.composing.pop();
                }
                // Ctrl-C / Ctrl-U wipe the in-flight line.
                '\x03' | '\x15' => self.composing.clear(),
                // Other control chars (Tab, etc.) don't change the
                // literal text being composed.
                c if c.is_control() => {}
                c => self.push_composing(c),
            }
        }
        committed
    }

    /// Append `text` to the composing buffer, truncated to stay
    /// within [`COMPOSING_CAP`]. Used by the bracketed-paste path
    /// where the payload arrives as one chunk, so a single check at
    /// the boundary is enough to defend against pathological pastes.
    fn append_paste(&mut self, text: &str) {
        let remaining = COMPOSING_CAP.saturating_sub(self.composing.len());
        if remaining == 0 {
            return;
        }
        if text.len() <= remaining {
            self.composing.push_str(text);
            return;
        }
        // Find the largest UTF-8 char boundary ≤ `remaining` so we
        // don't split a multi-byte codepoint.
        let mut cut = remaining;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        self.composing.push_str(&text[..cut]);
    }
}

/// libghostty-vt state for one terminal.
///
/// **`!Send + !Sync`** because libghostty's allocator owns raw
/// pointers. Lives entirely on the main task — the TUI is single-
/// threaded by design (the daemon is what's multi-threaded).
struct TerminalVt {
    terminal: vt::Terminal<'static, 'static>,
    render_state: vt::RenderState<'static>,
    row_iter: vt::render::RowIterator<'static>,
    cell_iter: vt::render::CellIterator<'static>,
    cols: u16,
    rows: u16,
    /// Per-terminal scratch buffer backing `GhosttyTerminal`'s
    /// per-row FFI-error fallback (the last good content for a row
    /// whose cell iterator transiently fails). It is NOT a render
    /// fast path: libghostty's dirty flags can't be trusted to skip
    /// redraws (see `GhosttyTerminal` docs / #239), so the widget
    /// walks every cell every frame.
    shadow: Option<ratatui::buffer::Buffer>,
    /// Last viewport cell the cursor was drawn at while genuinely
    /// visible (DECTCEM on). Lets `GhosttyTerminal` tell a blink's
    /// hidden phase (cursor unmoved) from a genuine park-and-hide
    /// (cursor moved off-caret) so a hidden, parked cursor doesn't
    /// leave a stray block (#844).
    last_visible_cursor: Option<vt::render::CursorViewport>,
    /// Monotonic revision of the grid content, bumped by every
    /// mutation that can change what a render would show: `feed`,
    /// an actual viewport `scroll`, a real `ensure_size` change.
    /// Wholesale parser replacement (`reset`, the deep-scrollback
    /// adoption) swaps the whole struct, and the slot-level cached
    /// frame is invalidated at those sites. Unlike libghostty's
    /// dirty flags (unsound as a skip signal, #239), this is a
    /// client-side input log: if no mutation ran since the last
    /// paint, the grid literally cannot differ — skipping the full
    /// per-cell FFI walk is sound (2026-08-19 audit, U1).
    content_rev: u64,
    /// Deterministic fault injection for the resync retry contract.
    #[cfg(test)]
    fail_next_reset: bool,
    _not_send: std::marker::PhantomData<*mut ()>,
}

impl TerminalVt {
    fn new() -> Option<Box<Self>> {
        let terminal = vt::Terminal::new(vt::TerminalOptions {
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            max_scrollback_lines: client_scrollback_lines(),
            max_scrollback_bytes: client_scrollback_bytes(),
        })
        .ok()?;
        let render_state = vt::RenderState::new().ok()?;
        let row_iter = vt::render::RowIterator::new().ok()?;
        let cell_iter = vt::render::CellIterator::new().ok()?;
        Some(Box::new(Self {
            terminal,
            render_state,
            row_iter,
            cell_iter,
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            shadow: None,
            last_visible_cursor: None,
            content_rev: 0,
            #[cfg(test)]
            fail_next_reset: false,
            _not_send: std::marker::PhantomData,
        }))
    }

    fn feed(&mut self, bytes: &[u8]) {
        self.content_rev = self.content_rev.wrapping_add(1);
        self.terminal.vt_write(bytes);
    }

    /// THE one and only mutation of this terminal's viewport pin. Every
    /// scroll surface funnels here (`scroll_terminal`/`scroll_active` for
    /// the wheel + focused keyboard scroll, `scroll_to_top`/
    /// `scroll_to_bottom` for the jumps) and no other code in the crate
    /// calls `scroll_viewport` — a grep for it must return exactly the
    /// three lines below. A request either moves the viewport or the
    /// returned [`ScrollOutcome`] explains why it could not
    /// (`NoScrollback` when `total <= len`), so a scroll can never
    /// silently no-op.
    /// That single choke point is the #42/#371 encapsulation: one owner
    /// of scroll state, and it cannot fail quietly.
    fn scroll(&mut self, request: ScrollRequest) -> ScrollOutcome {
        if matches!(request, ScrollRequest::By(0)) {
            return ScrollOutcome::Noop;
        }
        let Ok(before) = self.terminal.scrollbar() else {
            tracing::error!(?request, "terminal scroll state unavailable before request");
            return ScrollOutcome::StateUnavailable;
        };

        if before.total <= before.len {
            return ScrollOutcome::NoScrollback;
        }

        let max_offset = before.total.saturating_sub(before.len);
        let boundary = match request {
            ScrollRequest::By(delta) if delta < 0 && before.offset == 0 => {
                Some(ScrollBoundary::Top)
            }
            ScrollRequest::By(delta) if delta > 0 && before.offset >= max_offset => {
                Some(ScrollBoundary::Bottom)
            }
            ScrollRequest::Top if before.offset == 0 => Some(ScrollBoundary::Top),
            ScrollRequest::Bottom if before.offset >= max_offset => Some(ScrollBoundary::Bottom),
            _ => None,
        };
        if let Some(boundary) = boundary {
            return ScrollOutcome::AtBoundary {
                boundary,
                offset: before.offset,
                total: before.total,
                len: before.len,
            };
        }

        self.content_rev = self.content_rev.wrapping_add(1);
        match request {
            ScrollRequest::By(delta) => self
                .terminal
                .scroll_viewport(vt::terminal::ScrollViewport::Delta(delta)),
            ScrollRequest::Top => self
                .terminal
                .scroll_viewport(vt::terminal::ScrollViewport::Top),
            ScrollRequest::Bottom => self
                .terminal
                .scroll_viewport(vt::terminal::ScrollViewport::Bottom),
        }
        let Ok(after) = self.terminal.scrollbar() else {
            tracing::error!(?request, "terminal scroll state unavailable after request");
            return ScrollOutcome::StateUnavailable;
        };
        let outcome = classify_scroll_transition(request, before, after);
        if let ScrollOutcome::Stalled {
            offset, total, len, ..
        } = outcome
        {
            tracing::error!(
                ?request,
                offset,
                total,
                len,
                "terminal viewport scroll stalled",
            );
        }
        outcome
    }

    fn ensure_size(&mut self, cols: u16, rows: u16) {
        if cols == self.cols && rows == self.rows {
            return;
        }
        self.content_rev = self.content_rev.wrapping_add(1);
        let _ = self.terminal.resize(cols, rows, 0, 0);
        self.cols = cols;
        self.rows = rows;
    }

    /// Discard all parser state and rebuild a fresh terminal at the
    /// current size. Used on resync after the event channel dropped
    /// `TerminalOutput`: feeding the daemon ring into the *existing*
    /// parser would double-render on top of a now-desynced grid, so we
    /// start clean and re-feed from scratch — exactly how `Snapshot`
    /// reconstructs a terminal on reconnect. Returns false (and leaves
    /// the old state in place) if libghostty-vt init fails, so a
    /// transient allocator hiccup degrades to a stale grid rather than
    /// a blank one.
    fn reset(&mut self) -> bool {
        #[cfg(test)]
        if std::mem::take(&mut self.fail_next_reset) {
            return false;
        }
        let (cols, rows) = (self.cols, self.rows);
        let Some(mut fresh) = TerminalVt::new() else {
            return false;
        };
        fresh.ensure_size(cols, rows);
        *self = *fresh;
        true
    }
}

impl TerminalStack {
    pub fn new(id: PaneId) -> Self {
        Self {
            id,
            terminals: HashMap::new(),
            slots_rev: std::cell::Cell::new(0),
            visible_cache: std::cell::RefCell::new(None),
            active_session: None,
            active_tab_idx: 0,
            collapsed: true,
            collapse_user_set: false,
            layout: lazybox_core::SessionLayout::default(),
            pending_split: None,
            zoomed: false,
            synced_layout: None,
            pending_resizes: Vec::new(),
            pending_resync_requests: Vec::new(),
            tab_strip_hits: Vec::new(),
            tile_hits: Vec::new(),
            last_focused: HashMap::new(),
            closing: HashSet::new(),
            dead_on_arrival: DEFAULT_DEAD_ON_ARRIVAL,
            terminal_new_layout: lazybox_config::NewTerminalLayout::default(),
            pending_scrollback_fetch: None,
        }
    }

    /// Fold resolved UI config into the stack: the dead-on-arrival
    /// grace window that gates auto-close of exited agent panes (#367),
    /// and the `terminal_new_layout` preference that decides whether an
    /// auto-spawned second terminal promotes the session into a
    /// side-by-side split or simply lands as a new tab (#361).
    pub fn apply_ui_defaults(&mut self, ui: &lazybox_config::UiDefaults) {
        self.dead_on_arrival = ui.agent_dead_on_arrival;
        self.terminal_new_layout = ui.terminal_new_layout;
        // Match the client VT's scrollback depth to the daemon's tmux
        // `history-limit` (both from `terminal.scrollback_lines`) so a
        // deep-scrollback fetch can surface the full retained history
        // rather than clipping it to a shallower local grid. Read by
        // `TerminalVt::new` when each terminal's parser is built.
        CLIENT_SCROLLBACK_LINES.store(ui.scrollback_lines as usize, Ordering::Relaxed);
    }

    /// The current new-terminal layout preference.
    pub fn terminal_new_layout(&self) -> lazybox_config::NewTerminalLayout {
        self.terminal_new_layout
    }

    /// Flip the new-terminal layout preference (`]]t`) and return the
    /// new value so the caller can persist it and flash a notice. Takes
    /// effect on the *next* spawn; terminals already open are untouched.
    pub fn toggle_terminal_new_layout(&mut self) -> lazybox_config::NewTerminalLayout {
        self.terminal_new_layout = match self.terminal_new_layout {
            lazybox_config::NewTerminalLayout::Split => lazybox_config::NewTerminalLayout::Tabs,
            lazybox_config::NewTerminalLayout::Tabs => lazybox_config::NewTerminalLayout::Split,
        };
        self.terminal_new_layout
    }

    /// Drain queued resize requests from the last frame. The App calls
    /// this after every render and ships each as a `Command::Resize`
    /// so the backend PTY's size tracks the visible rect — without
    /// this, the shell process inside the PTY stays at its initial
    /// spawn size and output past those rows never gets written,
    /// surfacing as "the terminal looks frozen."
    pub fn drain_pending_resizes(&mut self) -> Vec<(TerminalId, u16, u16)> {
        std::mem::take(&mut self.pending_resizes)
    }

    /// Drain sequence-gap recovery requests for the model's IPC client.
    pub fn drain_pending_resync_requests(&mut self) -> Vec<TerminalResyncRequest> {
        std::mem::take(&mut self.pending_resync_requests)
            .into_iter()
            .filter_map(|terminal_id| match self.terminals.get(&terminal_id)?.sync {
                TerminalStreamSync::Desynced {
                    request,
                    request_pending: true,
                } => Some(request),
                _ => None,
            })
            .collect()
    }

    /// Restore requests that could not enter the bounded IPC command lane.
    /// Their per-terminal latches remain set, so retaining the typed debt here
    /// is what makes a later daemon event retry instead of silently stranding
    /// the terminal in `desynced` forever.
    pub fn requeue_resync_requests(&mut self, requests: Vec<TerminalResyncRequest>) {
        for request in requests {
            let Some(slot) = self.terminals.get_mut(&request.terminal_id) else {
                continue;
            };
            match &mut slot.sync {
                TerminalStreamSync::Desynced {
                    request: debt,
                    request_pending,
                } => {
                    debt.required_seq = debt.required_seq.max(request.required_seq);
                    *request_pending = true;
                    slot.resync_retry_at = None;
                }
                TerminalStreamSync::Coherent => continue,
            }
            if !self.pending_resync_requests.contains(&request.terminal_id) {
                self.pending_resync_requests.push(request.terminal_id);
            }
        }
    }

    /// Re-drive recovery for quiescent desynced panes (#1254 finding 2).
    /// A `TerminalResyncUnavailable` reply releases the request latch,
    /// and without this only NEW output on that terminal would re-issue
    /// the request — a finished agent's desynced pane stayed frozen
    /// forever. Called from the UI tick loop; any slot in
    /// `Desynced { request_pending: false }` whose backoff deadline has
    /// passed re-arms its request. Retries never stop
    /// (advise-never-forbid: the pane must eventually converge); the
    /// backoff merely paces them, doubling per consecutive failure up
    /// to `RESYNC_RETRY_CAP`.
    pub fn tick_resync_retries(&mut self, now: std::time::Instant) {
        let due: Vec<TerminalId> = self
            .terminals
            .iter()
            .filter_map(|(id, slot)| match slot.sync {
                TerminalStreamSync::Desynced {
                    request_pending: false,
                    ..
                } if slot.exited.is_none() && slot.resync_retry_at.is_none_or(|at| now >= at) => {
                    Some(*id)
                }
                _ => None,
            })
            .collect();
        for id in due {
            let Some(slot) = self.terminals.get_mut(&id) else {
                continue;
            };
            if let TerminalStreamSync::Desynced {
                request_pending, ..
            } = &mut slot.sync
            {
                *request_pending = true;
            }
            // The next `Unavailable` reply re-arms the (doubled) backoff.
            slot.resync_retry_at = None;
            if !self.pending_resync_requests.contains(&id) {
                self.pending_resync_requests.push(id);
            }
        }
    }

    /// Ctrl-L's "give me the truth" hatch (#1254 finding 7): distrust
    /// every visible grid and ask the daemon for its authoritative
    /// replay. Clearing the host terminal fixes what the HOST forgot;
    /// only a ring replay can fix a client VT grid that parsed a torn
    /// or seam-garbled stream. The last coherent grid keeps rendering
    /// until each replay lands (`resync_terminal` resets + re-feeds),
    /// so the hatch never blanks a pane. Exited panes are skipped —
    /// they render a freeze-frame and have no daemon stream to ask.
    pub fn mark_visible_desynced(&mut self) {
        for id in self.visible_terminals() {
            let Some(slot) = self.terminals.get_mut(&id) else {
                continue;
            };
            if slot.exited.is_some() {
                continue;
            }
            let required_seq = match slot.sync {
                TerminalStreamSync::Coherent => slot.last_seq,
                TerminalStreamSync::Desynced { request, .. } => {
                    request.required_seq.max(slot.last_seq)
                }
            };
            slot.sync = TerminalStreamSync::Desynced {
                request: TerminalResyncRequest {
                    terminal_id: id,
                    required_seq,
                },
                request_pending: true,
            };
            slot.resync_retry_at = None;
            if !self.pending_resync_requests.contains(&id) {
                self.pending_resync_requests.push(id);
            }
        }
    }

    /// Apply a session's persisted layout. Called by the App when the
    /// active workspace + session change so the renderer matches the
    /// user's last arrangement.
    ///
    /// Called from `sync_panes` after every key dispatch and daemon
    /// event, so it must tolerate re-projections of a layout that
    /// hasn't actually changed daemon-side:
    /// - a re-projection must not reset the pending split — ANY daemon
    ///   event landing between `]]|` and the `TerminalSpawned` it
    ///   waits on used to silently cancel the split;
    /// - it must not stomp a LOCAL layout mutation (a `]]<arrow>`
    ///   focus move, a just-committed split) back to the stale
    ///   persisted state while the mutation's own `SetSessionLayout`
    ///   is still round-tripping to the daemon.
    /// Both fall out of skipping any layout equal to the one this
    /// session last synced; a genuinely new daemon-side layout
    /// (workspace switch, our echo, another client) still applies.
    pub fn set_layout(&mut self, layout: lazybox_core::SessionLayout) {
        if self.synced_layout.as_ref() == Some(&layout) {
            return;
        }
        self.synced_layout = Some(layout.clone());
        if self.layout == layout {
            return;
        }
        self.layout = layout;
        self.pending_split = None;
        self.zoomed = false;
    }

    pub fn layout(&self) -> &lazybox_core::SessionLayout {
        &self.layout
    }

    /// Toggle tmux-style zoom of the focused tile (#1057): maximize it
    /// across the whole pane and back. Only meaningful in a multi-tile
    /// Splits grid — returns `Some(true/false)` for the resulting state
    /// so the caller can flash a hint, or `None` when there is nothing to
    /// zoom (Tabs, or a single tile).
    pub fn toggle_zoom(&mut self) -> Option<bool> {
        let multi = matches!(
            &self.layout,
            lazybox_core::SessionLayout::Splits { tree, .. } if tree.leaves().len() >= 2
        );
        if !multi {
            self.zoomed = false;
            return None;
        }
        self.zoomed = !self.zoomed;
        Some(self.zoomed)
    }

    /// Whether the Splits grid is currently zoomed to its focused tile.
    pub fn is_zoomed(&self) -> bool {
        self.zoomed
    }

    /// Terminal id at the focused leaf (Splits mode), or the active
    /// tab's terminal id (Tabs mode). Returns None when nothing is
    /// renderable.
    pub fn focused_terminal_id(&self) -> Option<TerminalId> {
        match &self.layout {
            lazybox_core::SessionLayout::Tabs { .. } => self.active_terminal_id(),
            lazybox_core::SessionLayout::Splits { tree, focused } => {
                let leaves = tree.leaves();
                let path = focused.as_slice();
                let id = subtree_at_path(tree, path).and_then(|n| match n {
                    lazybox_core::TileTree::Leaf { terminal_id } => Some(*terminal_id),
                    _ => None,
                });
                id.map(TerminalId)
                    .or_else(|| leaves.first().map(|i| TerminalId(*i)))
            }
        }
    }

    pub fn terminal_agent_state(&self, terminal_id: TerminalId) -> Option<lazybox_ipc::AgentState> {
        self.terminals.get(&terminal_id).and_then(|slot| {
            matches!(slot.kind, TerminalKind::Agent(_)).then_some(slot.agent_state)
        })
    }

    /// Whether the pane should render only its header row.
    pub fn is_collapsed(&self) -> bool {
        self.collapsed
    }

    /// True when every agent slot keyed to `session_key` already
    /// shows `state` in its tab badge. The orchestrator uses it as a
    /// conservative redraw gate: a `false` means at least one agent
    /// tab in the session would paint differently from `state`, so an
    /// incoming `Event::AgentState` is worth a redraw.
    ///
    /// Checked ALONGSIDE the sidebar's equivalent before skipping a
    /// redraw: badges are per-terminal (the event arm applies state to
    /// the one terminal it names), so a freshly-spawned second agent in
    /// a workspace can need a badge flip even when the sidebar's
    /// session-level state is already correct. Vacuously true when the
    /// session has no agent slots (nothing to repaint).
    pub fn displays_agent_state(
        &self,
        session_key: &SessionKey,
        state: lazybox_ipc::AgentState,
    ) -> bool {
        self.terminals
            .values()
            .filter(|s| &s.session_key == session_key && matches!(s.kind, TerminalKind::Agent(_)))
            .all(|s| s.agent_state == state)
    }

    /// Toggle the collapse state. Marks the user-override flag so we
    /// stop auto-collapsing on emptiness.
    pub fn set_collapsed(&mut self, collapsed: bool) {
        self.collapsed = collapsed;
        self.collapse_user_set = true;
    }

    /// Re-apply the empty-aware default unless the user has already
    /// expressed a preference. Called from event handlers that change
    /// the visible terminal set (Snapshot, TerminalSpawned,
    /// TerminalExited, set_active_session).
    fn auto_collapse_on_emptiness(&mut self) {
        if self.collapse_user_set {
            return;
        }
        self.collapsed = self.visible_terminals().is_empty();
    }

    /// AppRoot calls this whenever the sidebar selection changes.
    /// Restores the tab the user last focused in the session we're
    /// entering (falling back to the first pane), and records where
    /// focus was in the session we're leaving so a later return lands
    /// on the same pane.
    pub fn set_active_session(&mut self, session: Option<SessionKey>) {
        if self.active_session == session {
            return;
        }
        // Capture focus before the swap — `focused_terminal_id` reads
        // the session we're leaving.
        let focused = self.focused_terminal_id();
        if let (Some(prev), Some(focused)) = (
            std::mem::replace(&mut self.active_session, session),
            focused,
        ) {
            self.last_focused.insert(prev, focused);
        }
        // AFTER the swap — an invalidate placed before it let the
        // focus capture above re-cache the OLD session's (possibly
        // empty) visible set against the new revision.
        self.invalidate_visible();
        self.active_tab_idx = self
            .active_session
            .as_ref()
            .and_then(|sk| self.last_focused.get(sk).copied())
            .and_then(|id| self.visible_terminals().iter().position(|t| *t == id))
            .unwrap_or(0);
        // Drop the user's explicit collapse override on session
        // change — each session gets its own auto-default.
        self.collapse_user_set = false;
        // The synced-layout memo is per-session; carrying it across a
        // switch could suppress applying the new session's layout.
        self.synced_layout = None;
        // Zoom is a transient view of the session we're leaving.
        self.zoomed = false;
        self.auto_collapse_on_emptiness();
    }

    pub fn active_session(&self) -> Option<&SessionKey> {
        self.active_session.as_ref()
    }

    /// TerminalIds visible in the current session, in stable order:
    /// agents first (far left), then shells / log tails, ties broken
    /// by u64 id so tab positions are deterministic.
    pub fn visible_terminals(&self) -> Vec<TerminalId> {
        // Memoized on `slots_rev` (#1237): this runs (at least) twice
        // per TerminalOutput event, and rebuilding + sorting the whole
        // global slot map per event was O(fleet) work on the UI thread.
        if let Some((rev, ids)) = self.visible_cache.borrow().as_ref()
            && *rev == self.slots_rev.get()
        {
            return ids.clone();
        }
        let ids = self.visible_terminals_uncached();
        *self.visible_cache.borrow_mut() = Some((self.slots_rev.get(), ids.clone()));
        ids
    }

    /// Test-only slot insertion that keeps the visible-set memo honest —
    /// production inserts route through event handlers that invalidate.
    #[cfg(test)]
    fn insert_slot_for_test(&mut self, id: TerminalId, slot: TerminalSlot) {
        self.invalidate_visible();
        self.terminals.insert(id, slot);
    }

    /// Mark the visible-set inputs changed — every slot-membership /
    /// kind / active-session mutation routes through this (#1237).
    fn invalidate_visible(&self) {
        self.slots_rev.set(self.slots_rev.get().wrapping_add(1));
    }

    fn visible_terminals_uncached(&self) -> Vec<TerminalId> {
        let Some(sk) = &self.active_session else {
            return vec![];
        };
        let mut ids: Vec<TerminalId> = self
            .terminals
            .iter()
            .filter(|(_, slot)| slot.session_key == *sk)
            .map(|(id, _)| *id)
            .collect();
        ids.sort_by_key(|id| {
            let agent = self
                .terminals
                .get(id)
                .is_some_and(|slot| matches!(slot.kind, TerminalKind::Agent(_)));
            (!agent, id.0)
        });
        ids
    }

    pub fn active_terminal_id(&self) -> Option<TerminalId> {
        self.visible_terminals().get(self.active_tab_idx).copied()
    }

    /// Whether the tracked terminal runs an agent (Claude / Codex /
    /// Cursor) as opposed to a plain shell or log tail. Snippet
    /// submission routes agents through the daemon's settle-gated
    /// inject path and shells through a direct write.
    pub fn terminal_is_agent(&self, id: TerminalId) -> bool {
        self.terminals
            .get(&id)
            .is_some_and(|slot| matches!(slot.kind, TerminalKind::Agent(_)))
    }

    pub(crate) fn terminal_agent_id(&self, id: TerminalId) -> Option<&str> {
        match &self.terminals.get(&id)?.kind {
            TerminalKind::Agent(agent_id) => Some(agent_id.as_str()),
            _ => None,
        }
    }

    pub(crate) fn terminal_is_on_main(&self, id: TerminalId) -> bool {
        self.terminals.get(&id).is_some_and(|slot| slot.on_main)
    }

    /// Whether the tracked terminal runs in no-permission / bypass mode
    /// (auto-accepts tool-use prompts, unattended). Drives the `⚠` tab
    /// glyph's on-focus footer hint (#989).
    pub(crate) fn terminal_no_permission(&self, id: TerminalId) -> bool {
        self.terminals
            .get(&id)
            .is_some_and(|slot| slot.no_permission)
    }

    pub(crate) fn prepare_agent_replacement(
        &mut self,
        id: TerminalId,
        client_request_id: &str,
        cmds: &mut Vec<Command>,
    ) -> bool {
        let Some(slot) = self.terminals.get(&id) else {
            return false;
        };
        if slot.exited.is_some() {
            self.drop_slot(id);
            return false;
        }
        self.closing.insert(id);
        cmds.push(Command::Close {
            terminal_id: id,
            client_request_id: Some(client_request_id.into()),
        });
        true
    }

    pub(crate) fn cancel_agent_replacement(&mut self, id: TerminalId) {
        self.closing.remove(&id);
    }

    /// The session a tracked terminal belongs to. Used by the spawn-
    /// follow pin to recover the workspace for a `TerminalFocusRequested`
    /// (which carries only the terminal id).
    pub fn session_key_for(&self, id: TerminalId) -> Option<&SessionKey> {
        self.terminals.get(&id).map(|slot| &slot.session_key)
    }

    /// Number of terminal slots currently tracked for `session_key`.
    /// Used as the baseline a non-singleton spawn (a shell) is measured
    /// against: the spawn is satisfied once the count rises above it.
    pub fn terminal_count_for(&self, session_key: &SessionKey) -> usize {
        self.terminals
            .values()
            .filter(|slot| &slot.session_key == session_key)
            .count()
    }

    /// Whether a spawn of `kind` into `session_key` has produced its
    /// terminal yet — the projection that drives the footer spawn
    /// spinner (#206). Singleton kinds (agents, editor) are satisfied
    /// once a runner of the same identity exists; non-singleton kinds
    /// (shells) once the session's terminal count exceeds the
    /// `baseline_count` captured when the spawn was sent. Recomputed
    /// from the live terminal set, so a dropped / raced / mismatched
    /// `TerminalSpawned` can't strand the spinner.
    pub fn spawn_satisfied(
        &self,
        session_key: &SessionKey,
        kind: &TerminalKind,
        baseline_count: usize,
    ) -> bool {
        if kind.singleton_key().is_some() {
            self.find_runner(session_key, kind).is_some()
        } else {
            self.terminal_count_for(session_key) > baseline_count
        }
    }

    /// Find an existing runner inside the given session whose kind
    /// has the same singleton-identity as `kind` (e.g. "this session
    /// already has a Claude → don't spawn a second one"). Returns
    /// `None` for non-singleton kinds (Shell) since those are
    /// always new spawns.
    pub fn find_runner(&self, session_key: &SessionKey, kind: &TerminalKind) -> Option<TerminalId> {
        let key = kind.singleton_key()?;
        self.terminals
            .iter()
            .find(|(_, slot)| {
                slot.session_key == *session_key && slot.kind.singleton_key() == Some(key.clone())
            })
            .map(|(id, _)| *id)
    }

    fn set_terminal_focus(&mut self, target: TerminalId) -> Option<bool> {
        let visible = self.visible_terminals();
        let idx = visible.iter().position(|id| *id == target)?;
        let layout_changed = match &mut self.layout {
            lazybox_core::SessionLayout::Tabs { active } => {
                let changed = *active != idx;
                *active = idx;
                changed
            }
            lazybox_core::SessionLayout::Splits { tree, focused } => {
                let path = tree.path_to(target.0)?;
                let changed = *focused != path;
                *focused = path;
                changed
            }
        };
        self.active_tab_idx = idx;
        // Expanding the section is part of "focus": collapsed
        // body would otherwise hide the terminal the user just asked
        // for.
        self.set_collapsed(false);
        Some(layout_changed)
    }

    /// Focus the given terminal in the active tab or split layout.
    /// Returns `false` when the target is not visible.
    pub fn focus_terminal(&mut self, target: TerminalId) -> bool {
        self.set_terminal_focus(target).is_some()
    }

    pub fn active_tab_idx(&self) -> usize {
        self.active_tab_idx
    }

    pub fn terminal_count(&self) -> usize {
        self.terminals.len()
    }

    /// Recent raw output for the active terminal. Used for tests +
    /// pattern-matching (e.g. detecting "Are you sure?" prompts).
    /// NOT the rendering source — that's libghostty-vt. Includes
    /// escape sequences as they came off the wire.
    pub fn active_content(&self) -> Option<&[u8]> {
        let id = self.active_terminal_id()?;
        self.terminals.get(&id).map(|s| s.recent.as_slice())
    }

    /// True when the focused terminal's inner program (libghostty
    /// has parsed CSI ?1000h / ?1002h / ?1003h / ?1006h SGR) wants
    /// raw mouse events forwarded. Claude Code, vim, less, etc. all
    /// turn this on while running. The orchestrator uses this signal
    /// for clicks; wheels always scroll lazybox's own history.
    pub fn focused_terminal_tracks_mouse(&self) -> bool {
        let Some(id) = self.focused_terminal_id() else {
            return false;
        };
        self.terminal_tracks_mouse(id)
    }

    /// True when `id`'s inner program has enabled terminal mouse tracking.
    pub fn terminal_tracks_mouse(&self, id: TerminalId) -> bool {
        self.terminals
            .get(&id)
            .and_then(|s| s.vt.terminal.is_mouse_tracking().ok())
            .unwrap_or(false)
    }

    /// Encode a mouse event for the focused terminal using its
    /// active mouse-tracking mode + format. Returns the bytes to
    /// `Write` to the PTY plus the terminal id. Returns `None` when
    /// the terminal isn't tracking mouse, encoding failed, or the
    /// event doesn't translate to anything (no-op for the protocol).
    /// `cell_col` / `cell_row` are 0-based cell coordinates **within
    /// the terminal's rect**, not the screen.
    pub fn encode_mouse_for_focused(
        &mut self,
        action: vt::mouse::Action,
        button: Option<vt::mouse::Button>,
        cell_col: u32,
        cell_row: u32,
    ) -> Option<(TerminalId, Vec<u8>)> {
        let id = self.focused_terminal_id()?;
        self.encode_mouse_for(id, action, button, cell_col, cell_row)
    }

    /// Same as [`Self::encode_mouse_for_focused`] but for an explicit
    /// terminal id.
    pub fn encode_mouse_for(
        &mut self,
        id: TerminalId,
        action: vt::mouse::Action,
        button: Option<vt::mouse::Button>,
        cell_col: u32,
        cell_row: u32,
    ) -> Option<(TerminalId, Vec<u8>)> {
        let slot = self.terminals.get_mut(&id)?;
        if !slot.vt.terminal.is_mouse_tracking().unwrap_or(false) {
            return None;
        }
        let mut encoder = vt::mouse::Encoder::new().ok()?;
        encoder.set_options_from_terminal(&slot.vt.terminal);
        // Cell-aligned reporting: width=cols, height=rows, cell=1×1
        // pixel. `Position::{x,y}` in pixels then equals the cell
        // index, which is what the protocol expects in non-pixel
        // formats (the encoder divides x/cell_width to get the cell).
        let cols = slot.vt.cols.max(1) as u32;
        let rows = slot.vt.rows.max(1) as u32;
        encoder.set_size(vt::mouse::EncoderSize {
            screen_width: cols,
            screen_height: rows,
            cell_width: 1,
            cell_height: 1,
            padding_top: 0,
            padding_bottom: 0,
            padding_left: 0,
            padding_right: 0,
        });
        let mut event = vt::mouse::Event::new().ok()?;
        event
            .set_action(action)
            .set_button(button)
            .set_position(vt::mouse::Position {
                x: cell_col as f32,
                y: cell_row as f32,
            });
        let mut buf: Vec<u8> = Vec::with_capacity(32);
        encoder.encode_to_vec(&event, &mut buf).ok()?;
        if buf.is_empty() {
            return None;
        }
        Some((id, buf))
    }

    /// Scroll the focused terminal's viewport by `delta` rows.
    /// Negative scrolls up into the scrollback; positive scrolls
    /// down toward the live content. Called from the app loop's
    /// mouse-wheel handler so trackpad gestures move the viewport
    /// instead of just being eaten. Uses `focused_terminal_id` so
    /// both Tabs and Splits modes route to the right tile.
    /// Human-readable summary of the focused terminal's scrollbar
    /// state — `screen=PRIMARY total=120 offset=10 len=32`. Used
    /// by the orchestrator's scroll diagnostic to surface in the
    /// footer notice why a scroll might look like a no-op.
    pub fn scrollbar_summary(&self) -> Option<String> {
        let id = self.focused_terminal_id()?;
        let slot = self.terminals.get(&id)?;
        let screen = slot.vt.terminal.active_screen().ok();
        let bar = slot.vt.terminal.scrollbar().ok()?;
        Some(format!(
            "screen={:?} total={} offset={} len={}",
            screen, bar.total, bar.offset, bar.len,
        ))
    }

    /// Jump the focused terminal's viewport to the top of scrollback.
    /// Returns the same typed transition as delta scrolling.
    pub fn scroll_to_top(&mut self) -> ScrollOutcome {
        let Some(id) = self.focused_terminal_id() else {
            return ScrollOutcome::NoTerminal;
        };
        let Some(slot) = self.terminals.get_mut(&id) else {
            return ScrollOutcome::NoTerminal;
        };
        let outcome = slot.vt.scroll(ScrollRequest::Top);
        // Same deep-scrollback arming as an upward `scroll_terminal`
        // (#393) — jumping straight to the top is the strongest
        // possible "show me the history" signal.
        if slot.exited.is_none() && !slot.deep_scrollback_requested {
            slot.deep_scrollback_requested = true;
            self.pending_scrollback_fetch = Some(id);
        }
        outcome
    }

    /// Jump the focused terminal's viewport to the live bottom.
    /// Returns the same typed transition as delta scrolling.
    pub fn scroll_to_bottom(&mut self) -> ScrollOutcome {
        let Some(id) = self.focused_terminal_id() else {
            return ScrollOutcome::NoTerminal;
        };
        let Some(slot) = self.terminals.get_mut(&id) else {
            return ScrollOutcome::NoTerminal;
        };
        let outcome = slot.vt.scroll(ScrollRequest::Bottom);
        // Back at the live bottom — this scrollback visit is over; see
        // `scroll_terminal`.
        slot.deep_scrollback_requested = false;
        outcome
    }

    /// Did the user click on a terminal-tab label? Returns the tab
    /// index when `(col, row)` lands inside one of the click
    /// targets cached during render. Called by the orchestrator's
    /// mouse-down handler; when `Some(idx)` is returned the caller
    /// flips `active_tab_idx` so the matching terminal comes to
    /// the front.
    pub fn tab_at(&self, col: u16, row: u16) -> Option<usize> {
        self.tab_strip_hits
            .iter()
            .find(|(_, range, hit_row)| *hit_row == row && range.contains(&col))
            .map(|(idx, _, _)| *idx)
    }

    /// Terminal whose tile the point `(col, row)` lands in, from the
    /// tile rects recorded during render. Drives hover-to-scroll: the
    /// wheel targets the pane under the cursor, not the focused one
    /// (#362). `None` when the point is over pane chrome (the tab
    /// strip, a divider) or outside every tile — including the 1-cell
    /// accent bar / split seams between tiles, where the wheel falls
    /// back to scrolling the focused tile.
    pub fn scroll_terminal_at(&self, col: u16, row: u16) -> Option<TerminalId> {
        self.tile_hits
            .iter()
            .find(|hit| Self::rect_contains(hit.body, col, row))
            .map(|hit| hit.terminal_id)
    }

    /// Terminal whose full tile contains `(col, row)`. Unlike
    /// [`Self::scroll_terminal_at`], this includes tile chrome
    /// such as the focus bar while still excluding split seams.
    pub fn tile_at(&self, col: u16, row: u16) -> Option<TerminalId> {
        self.tile_hits
            .iter()
            .find(|hit| Self::rect_contains(hit.tile, col, row))
            .map(|hit| hit.terminal_id)
    }

    pub(crate) fn rendered_target_at(&mut self, col: u16, row: u16) -> Option<RenderedClickTarget> {
        let hit = self
            .tile_hits
            .iter()
            .find(|hit| Self::rect_contains(hit.tile, col, row))
            .copied()?;
        // Map through the grid the renderer actually drew for this tile,
        // NOT a recap recomputed from the now-live slot state — a prompt
        // submitted between the painted frame and the click would otherwise
        // shift the recap and resolve the wrong row (#1021).
        let cell = Self::cell_in_grid(hit.grid, col, row);
        let target = cell.and_then(|(cell_col, cell_row)| {
            self.target_in_body(hit.terminal_id, cell_col, cell_row)
        });
        Some(RenderedClickTarget {
            terminal_id: hit.terminal_id,
            tile: hit.tile,
            body: hit.body,
            cell,
            target,
        })
    }

    fn rect_contains(rect: Rect, col: u16, row: u16) -> bool {
        col >= rect.x
            && col < rect.x.saturating_add(rect.width)
            && row >= rect.y
            && row < rect.y.saturating_add(rect.height)
    }

    /// Set the active tab by index. Called from the mouse handler
    /// after `tab_at` resolves a click to a tab. Bounds-checked: a
    /// click on a stale hit-target (rare race between render and
    /// click) does nothing rather than crashing.
    pub fn set_active_tab(&mut self, idx: usize) {
        if idx < self.visible_terminals().len() {
            self.active_tab_idx = idx;
            self.set_collapsed(false);
        }
    }

    /// The render-time hit `id` recorded this frame — its exact grid rect
    /// (chrome, recap rows, and scrollbar gutter already removed) and the
    /// viewport offset it was painted at (recorded in
    /// [`Self::render_one_terminal`]). This is the single source of truth
    /// every screen↔grid mapping reads, so none of them recompute the
    /// recap/chrome inset or re-read a live scroll offset and drift against
    /// what was actually painted — including split tiles, whose grid does
    /// not begin at the pane's top (#1021). `None` when `id` was not part
    /// of the last render.
    fn hit_for(&self, id: TerminalId) -> Option<TerminalHit> {
        self.tile_hits
            .iter()
            .find(|hit| hit.terminal_id == id)
            .copied()
    }

    /// The exact on-screen cell-grid rect the renderer drew for terminal
    /// `id` in the last frame, or `None` if it wasn't rendered. Callers
    /// that must confine per-terminal chrome to a single tile — e.g. the
    /// selection-highlight overlay — clip to this instead of the whole
    /// terminal pane, so in a split layout the effect can't bleed into a
    /// neighbouring tile (#1101).
    pub fn tile_grid_rect(&self, id: TerminalId) -> Option<tuirealm::ratatui::layout::Rect> {
        self.hit_for(id).map(|hit| hit.grid)
    }

    /// The viewport offset to compose a selection against for `id`: the
    /// offset of the frame the user clicked on (recorded in
    /// [`Self::hit_for`]), falling back to the live VT offset only before
    /// the first render, when there is no painted frame yet. `None` when
    /// the scrollbar is unavailable — the caller then declines the mapping
    /// rather than guessing.
    fn frame_offset_for(&self, id: TerminalId) -> Option<u64> {
        match self.hit_for(id) {
            Some(hit) => hit.offset,
            None => self
                .terminals
                .get(&id)?
                .vt
                .terminal
                .scrollbar()
                .ok()
                .map(|b| b.offset),
        }
    }

    /// Screen-absolute grid bounds of `id`'s cell grid: `(inner_x,
    /// inner_y, last_col, last_row)` in crossterm screen coordinates. Read
    /// straight from the rect the renderer drew ([`Self::hit_for`]) so the
    /// mapping can never diverge from the frame the user clicked on. `rect`
    /// is consulted ONLY on the pre-first-render fallback path (no painted
    /// frame to read yet); once anything has rendered it is ignored in
    /// favour of the recorded grid. `None` when `id` is unknown or its grid
    /// holds no cell.
    fn grid_bounds(
        &self,
        id: TerminalId,
        rect: tuirealm::ratatui::layout::Rect,
    ) -> Option<(u16, u16, u16, u16)> {
        if let Some(hit) = self.hit_for(id) {
            let grid = hit.grid;
            if grid.width == 0 || grid.height == 0 {
                return None;
            }
            let last_col = grid.x + grid.width - 1;
            let last_row = grid.y + grid.height - 1;
            return Some((grid.x, grid.y, last_col, last_row));
        }
        let slot = self.terminals.get(&id)?;
        let inner_x = rect.x.saturating_add(1);
        // Use the SAME body height the renderer feeds `recap_rows` — the
        // pane minus 3 top-chrome rows AND the 1 held-back bottom margin
        // (`render()` insets to `area.height - 4` before calling
        // `render_one_terminal`).
        let body_height = rect.height.saturating_sub(4);
        let recap = Self::recap_rows(slot, body_height);
        let inner_y = rect.y.saturating_add(3).saturating_add(recap);
        // The right border column and the bottom border row are chrome,
        // never grid cells (see `screen_to_cell`'s upper-bound guards).
        let last_col = rect.x.saturating_add(rect.width).saturating_sub(2);
        let last_row = rect.y.saturating_add(rect.height).saturating_sub(2);
        if last_col < inner_x || last_row < inner_y {
            return None;
        }
        Some((inner_x, inner_y, last_col, last_row))
    }

    /// Translate a crossterm `(col, row)` into **screen-absolute grid
    /// coordinates** `(grid_col, screen_row)` for terminal `id`, where
    /// `screen_row` counts from the top of the scrollback (the libghostty
    /// `Point::Screen` space). The point is clamped into `id`'s grid, so a
    /// position past the tile boundary (a drag that strayed into a
    /// neighbouring tile) resolves to `id`'s nearest edge cell rather than
    /// reading the adjacent grid — this is what scopes a split-tile
    /// selection to the tile it started in (#1101). An edge / chrome
    /// position likewise resolves to the nearest cell, which is what an
    /// edge-drag auto-scroll needs. Returns `None` when `id` is unknown or
    /// the scrollbar state is unavailable.
    ///
    /// Storing a drag-selection in this space (rather than on-screen
    /// crossterm cells) is what lets the anchor stay pinned to its
    /// content while the viewport auto-scrolls under the drag (#432).
    pub fn selection_point(
        &self,
        id: TerminalId,
        rect: tuirealm::ratatui::layout::Rect,
        col: u16,
        row: u16,
    ) -> Option<(u16, u32)> {
        let (inner_x, inner_y, last_col, last_row) = self.grid_bounds(id, rect)?;
        // The offset of the frame the user clicked on — NOT a live re-read,
        // which output arriving between that frame and the click may have
        // advanced (#1021).
        let offset = self.frame_offset_for(id)?;
        let vx = col.clamp(inner_x, last_col) - inner_x;
        let vy = u64::from(row.clamp(inner_y, last_row) - inner_y);
        // `offset` is the viewport top's row in the total scrollable area,
        // i.e. the screen row of viewport row 0. Adding the in-viewport
        // offset yields the content's absolute screen row.
        Some((vx, (offset + vy) as u32))
    }

    /// Extract the plain text of the selection spanning two
    /// screen-absolute grid points (see [`Self::selection_point`]) from
    /// terminal `id`. Pinning extraction to the terminal the drag started
    /// in — rather than whatever is focused at release — is what keeps a
    /// split-tile copy scoped to its own tile (#1101). Unlike a viewport
    /// read, this covers rows that have scrolled off-screen, so a passage
    /// longer than one screen copies in a single gesture (#432).
    ///
    /// Uses libghostty's own flowing-text selection + plain formatter, so
    /// the semantics (first row from the anchor, whole middle rows, last
    /// row to the focus; wide glyphs and trailing blanks handled) match
    /// what the terminal itself would copy. Empty on any error.
    pub fn extract_selection(&mut self, id: TerminalId, a: (u16, u32), b: (u16, u32)) -> String {
        let Some(slot) = self.terminals.get_mut(&id) else {
            return String::new();
        };
        // The grid must reflect every byte received, not just those that
        // arrived while on screen — copy can fire on a terminal that
        // gained focus but hasn't re-rendered yet.
        slot.flush_pending();
        // Row-major normalize so `start` is the earlier endpoint.
        let (a, b) = if (a.1, a.0) <= (b.1, b.0) {
            (a, b)
        } else {
            (b, a)
        };
        let terminal = &slot.vt.terminal;
        let point = |p: (u16, u32)| {
            vt::terminal::Point::Screen(vt::terminal::PointCoordinate { x: p.0, y: p.1 })
        };
        let (Ok(start), Ok(end)) = (terminal.grid_ref(point(a)), terminal.grid_ref(point(b)))
        else {
            return String::new();
        };
        let selection = vt::screen::Selection {
            start,
            end,
            rectangle: false,
        };
        let Ok(mut formatter) = vt::fmt::Formatter::new(
            terminal,
            vt::fmt::FormatterOptions {
                format: vt::fmt::Format::Plain,
                trim: true,
                unwrap: false,
                selection: Some(selection),
            },
        ) else {
            return String::new();
        };
        match formatter.format_alloc(None) {
            Ok(bytes) => String::from_utf8_lossy(&bytes)
                .trim_end_matches('\n')
                .to_string(),
            Err(_) => String::new(),
        }
    }

    /// Project a screen-absolute selection span back to the on-screen
    /// crossterm cells of terminal `id` currently visible in `rect`, for
    /// the reverse-video highlight. Because the endpoints map through
    /// `id`'s own grid, the highlight stays inside `id`'s tile even when
    /// the drag strayed into a neighbour (#1101). Endpoints scrolled
    /// outside the viewport clamp to the pane edges so the visible portion
    /// of a scrollback-spanning selection still highlights. `None` when
    /// `id` is unknown.
    pub fn selection_screen_span(
        &self,
        id: TerminalId,
        rect: tuirealm::ratatui::layout::Rect,
        a: (u16, u32),
        b: (u16, u32),
    ) -> Option<((u16, u16), (u16, u16))> {
        let (inner_x, inner_y, _last_col, _last_row) = self.grid_bounds(id, rect)?;
        // Project against the same recorded frame offset the anchor was
        // captured with, so highlight and extraction agree (#1021).
        let offset = self.frame_offset_for(id)? as i64;
        let max_x = i64::from(rect.x.saturating_add(rect.width.saturating_sub(1)));
        let max_y = i64::from(rect.y.saturating_add(rect.height.saturating_sub(1)));
        let project = |p: (u16, u32)| -> (u16, u16) {
            let sx = i64::from(inner_x) + i64::from(p.0);
            let sy = i64::from(inner_y) + (i64::from(p.1) - offset);
            (
                sx.clamp(i64::from(rect.x), max_x) as u16,
                sy.clamp(i64::from(rect.y), max_y) as u16,
            )
        };
        Some((project(a), project(b)))
    }

    /// Plain-text dump of a terminal's whole visible grid — every row
    /// top to bottom, trailing spaces trimmed, pure box-drawing border
    /// rows dropped, and blank rows dropped off both ends. Seeds an
    /// agent-to-agent handoff with the source agent's on-screen output
    /// (issue #431); the caller lets the user edit it before it's
    /// injected into the target session, so any remaining composer
    /// chrome is trimmed there. `None` when the terminal is unknown, its
    /// VT snapshot can't be read, or nothing but chrome/blanks is left.
    pub fn visible_text(&mut self, id: TerminalId) -> Option<String> {
        let slot = self.terminals.get_mut(&id)?;
        // The grid must reflect every byte received, not just those that
        // arrived while on screen — mirrors `target_at`.
        slot.flush_pending();
        let snapshot = slot.vt.render_state.update(&slot.vt.terminal).ok()?;
        let mut row_iter = slot.vt.row_iter.update(&snapshot).ok()?;
        let mut rows: Vec<String> = Vec::new();
        while let Some(row) = row_iter.next() {
            let mut line = String::new();
            if let Ok(mut cell_iter) = slot.vt.cell_iter.update(row) {
                while let Some(cell) = cell_iter.next() {
                    // Wide-glyph spacer cells carry no graphemes; emitting
                    // one as a space would split CJK text ("日本語" →
                    // "日 本 語"), so drop it.
                    if matches!(
                        cell.wide(),
                        Ok(vt::screen::CellWide::SpacerTail | vt::screen::CellWide::SpacerHead)
                    ) {
                        continue;
                    }
                    let graphemes = cell.graphemes().unwrap_or_default();
                    if graphemes.is_empty() {
                        line.push(' ');
                    } else {
                        for g in graphemes {
                            line.push(g);
                        }
                    }
                }
            }
            rows.push(line.trim_end().to_string());
        }
        // Drop pure box-drawing rows — the agent composer's borders and
        // separators (╭──╮, ├──┤, ────). A row with any real text stays,
        // so content framed by │ … │ survives (the user trims the rest).
        rows.retain(|r| !is_border_row(r));
        while rows.first().is_some_and(|l| l.is_empty()) {
            rows.remove(0);
        }
        while rows.last().is_some_and(|l| l.is_empty()) {
            rows.pop();
        }
        if rows.is_empty() {
            return None;
        }
        Some(rows.join("\n"))
    }

    /// The agent terminal for `session_key`, preferring a live pane but
    /// falling back to a kept exited one — so a handoff can capture the
    /// final output of an agent that already finished and exited (#431).
    /// Ties break on the lowest id for determinism. `None` when the
    /// session has no agent slot at all.
    pub fn agent_terminal_for(&self, session_key: &SessionKey) -> Option<TerminalId> {
        self.terminals
            .iter()
            .filter(|(_, s)| {
                s.session_key == *session_key && matches!(s.kind, TerminalKind::Agent(_))
            })
            .min_by_key(|(id, s)| (s.exited.is_some(), id.0))
            .map(|(id, _)| *id)
    }

    /// The terminal a focus-mode workspace pane displays for
    /// `session_key` (#1258): the session's agent terminal
    /// ([`Self::agent_terminal_for`]), falling back to its most recent
    /// terminal of any kind — live before exited, newest spawn (highest
    /// id) first, deterministic either way.
    pub fn display_terminal_for(&self, session_key: &SessionKey) -> Option<TerminalId> {
        self.agent_terminal_for(session_key).or_else(|| {
            self.terminals
                .iter()
                .filter(|(_, s)| s.session_key == *session_key)
                .min_by_key(|(id, s)| (s.exited.is_some(), std::cmp::Reverse(id.0)))
                .map(|(id, _)| *id)
        })
    }

    /// Sessions with a live agent terminal, most-recently-spawned agent
    /// first (spawn order is the recency signal the stack has — ids are
    /// monotonically allocated). The shortfall fallback for the
    /// focus-mode pane roster (#1258): when the starred roster runs
    /// out, panes fill from here.
    pub fn recent_agent_sessions(&self) -> Vec<SessionKey> {
        let mut newest: std::collections::HashMap<SessionKey, u64> =
            std::collections::HashMap::new();
        for (id, slot) in &self.terminals {
            if matches!(slot.kind, TerminalKind::Agent(_)) && slot.exited.is_none() {
                let entry = newest.entry(slot.session_key.clone()).or_insert(id.0);
                *entry = (*entry).max(id.0);
            }
        }
        let mut ordered: Vec<(u64, SessionKey)> =
            newest.into_iter().map(|(k, id)| (id, k)).collect();
        ordered.sort_by_key(|a| std::cmp::Reverse(a.0));
        ordered.into_iter().map(|(_, k)| k).collect()
    }

    /// The compact agent-state badge for a focus-pane header (#1258):
    /// the same glyph/color vocabulary the tab strip and tile headers
    /// use, `None` for shells and unknown terminals.
    pub fn pane_state_badge(
        &self,
        id: TerminalId,
        theme: &crate::theme::Theme,
    ) -> Option<(&'static str, Style)> {
        let slot = self.terminals.get(&id)?;
        if !matches!(slot.kind, TerminalKind::Agent(_)) {
            return None;
        }
        Self::agent_state_badge(slot.agent_state, slot.exited.is_some(), true, theme)
    }

    /// Reset the per-frame render bookkeeping without drawing the
    /// tab-strip chrome — the prologue of [`Self::render`], split out
    /// for the focus-mode multi-pane path (#1258), which renders one
    /// terminal per workspace pane via [`Self::render_terminal_by_id`]
    /// instead of the active session's tile tree. Must be called once
    /// per frame before the first `render_terminal_by_id` so terminals
    /// that fell off-screen revert to buffering and stale click targets
    /// are dropped.
    pub fn begin_focus_frame(&mut self) {
        for slot in self.terminals.values_mut() {
            slot.displayed = false;
        }
        self.tab_strip_hits.clear();
        self.tile_hits.clear();
    }

    /// Render one specific terminal into `area` — the focus-mode pane
    /// body path (#1258). Shares `render_one_terminal` with the normal
    /// render, so the per-terminal PTY-resize bookkeeping
    /// (`last_rendered_size` → `pending_resizes`) applies to every
    /// visible pane: entering, cycling, or leaving a layout changes the
    /// pane rects and fans the resizes out through `drain_cmds` exactly
    /// like a tile split does. Unknown ids and empty rects are no-ops.
    pub fn render_terminal_by_id(
        &mut self,
        id: TerminalId,
        area: Rect,
        frame: &mut Frame,
        focused: bool,
    ) {
        if area.width == 0 || area.height == 0 || !self.terminals.contains_key(&id) {
            return;
        }
        self.render_one_terminal(id, area, area, frame, focused);
    }

    /// If the cell at frame-space `(col, row)` lies inside a URL,
    /// file path, or `#N` / `owner/repo#N` issue reference, return the
    /// matching [`ClickTarget`]. Otherwise `None`.
    /// Drives right-click-to-open: the click coordinates arrive in
    /// the same frame-space the renderer used, so we translate the
    /// same way `grid_bounds` does (skip the pane border + tab strip
    /// + any recap rows via `recap_rows`). Soft-wrapped tokens are
    /// stitched back together: a long URL that spilled onto the next
    /// row(s) is one logical string here, so a click on any of its
    /// rows resolves the whole link (#596). The VT flags soft-wrap per
    /// row (`Row::is_wrapped`), so we only ever join genuine
    /// continuations — never two unrelated lines.
    pub fn target_at(
        &mut self,
        rect: tuirealm::ratatui::layout::Rect,
        col: u16,
        row: u16,
    ) -> Option<ClickTarget> {
        let id = self.focused_terminal_id()?;
        let body = Rect {
            x: rect.x.saturating_add(1),
            y: rect.y.saturating_add(3),
            width: rect.width.saturating_sub(2),
            height: rect.height.saturating_sub(4),
        };
        let (cell_col, cell_row) = {
            let slot = self.terminals.get(&id)?;
            Self::cell_in_body(slot, body, col, row)?
        };
        self.target_in_body(id, cell_col, cell_row)
    }

    fn target_in_body(
        &mut self,
        id: TerminalId,
        cell_col: u32,
        cell_row: u32,
    ) -> Option<ClickTarget> {
        let slot = self.terminals.get_mut(&id)?;
        let cell_col = cell_col as u16;
        let target_row = cell_row as usize;
        let hyperlink = hyperlink_uri_at(&slot.vt.terminal, cell_col, cell_row as u16);
        let snapshot = slot.vt.render_state.update(&slot.vt.terminal).ok()?;
        let mut row_iter = slot.vt.row_iter.update(&snapshot).ok()?;
        // Collect every visible row's text, its column→byte map, and
        // whether it soft-wraps into the row below. `cell_byte_starts`
        // maps the clicked cell back to a byte position in that row's
        // text; the wrap flag lets us stitch a wrapped token across rows.
        let mut rows: Vec<(String, Vec<usize>, bool)> = Vec::new();
        while let Some(r) = row_iter.next() {
            let wrapped = r
                .raw_row()
                .ok()
                .and_then(|raw| raw.is_wrapped().ok())
                .unwrap_or(false);
            let (text, starts) = row_text_and_starts(&mut slot.vt.cell_iter, r);
            rows.push((text, starts, wrapped));
        }
        if target_row >= rows.len() {
            return None;
        }
        // Widen to the whole soft-wrap group: walk back over predecessors
        // that wrap into us, forward over rows we (or our successors) wrap
        // into. Joining their text with no separator reconstructs the
        // original unbroken token.
        let mut start = target_row;
        while start > 0 && rows[start - 1].2 {
            start -= 1;
        }
        let mut end = target_row;
        while rows[end].2 && end + 1 < rows.len() {
            end += 1;
        }
        let mut joined = String::new();
        let mut target_offset = 0;
        for (i, (text, _, _)) in rows[start..=end].iter().enumerate() {
            if start + i == target_row {
                target_offset = joined.len();
            }
            joined.push_str(text);
        }
        let byte_pos = target_offset + *rows[target_row].1.get(cell_col as usize)?;
        detect_target(&joined, byte_pos, hyperlink.as_deref())
    }

    fn cell_in_body(slot: &TerminalSlot, body: Rect, col: u16, row: u16) -> Option<(u32, u32)> {
        let recap = Self::recap_rows(slot, body.height);
        let grid_y = body.y.saturating_add(recap);
        let grid_height = body.height.saturating_sub(recap);
        let grid_width = if body.width > 1 {
            body.width - 1
        } else {
            body.width
        };
        if col < body.x
            || col >= body.x.saturating_add(grid_width)
            || row < grid_y
            || row >= grid_y.saturating_add(grid_height)
        {
            return None;
        }
        Some(((col - body.x) as u32, (row - grid_y) as u32))
    }

    /// Map a screen `(col, row)` to 0-based grid-cell coordinates within a
    /// recorded grid rect — the exact area the renderer drew, with recap
    /// rows and the scrollbar gutter already carved off. Unlike
    /// [`Self::cell_in_body`] this recomputes nothing from live slot state,
    /// so it can't drift when the recap changes between the painted frame
    /// and the click (#1021). `None` when the point is outside the grid.
    fn cell_in_grid(grid: Rect, col: u16, row: u16) -> Option<(u32, u32)> {
        if col < grid.x
            || col >= grid.x.saturating_add(grid.width)
            || row < grid.y
            || row >= grid.y.saturating_add(grid.height)
        {
            return None;
        }
        Some(((col - grid.x) as u32, (row - grid.y) as u32))
    }

    /// Every `http(s)://…` URL visible in the focused terminal's grid,
    /// de-duplicated and ordered top-to-bottom by each URL's *most
    /// recent* on-screen row — so a URL echoed twice sorts by its latest
    /// appearance, and the `]]u` picker's newest-first list (which
    /// reverses this) stays accurate. Soft-wrapped rows are stitched
    /// before scanning so a URL that spilled across rows is captured
    /// whole — the same join `target_at` does for a click. Drives the
    /// `]]u` keyboard URL picker (#596), which sidesteps every
    /// mouse/emulator quirk that blocks right-click-to-open.
    ///
    /// `None` means specifically "no terminal is focused" — the caller's
    /// signal to say so. A focused terminal always yields `Some`, even
    /// when the grid holds no URL (empty `Vec`) or its VT snapshot can't
    /// be read (also empty — we scanned, there was nothing to open).
    pub fn focused_urls(&mut self) -> Option<Vec<String>> {
        self.urls_for(self.focused_terminal_id()?)
    }

    /// Like [`Self::focused_urls`] but scans an explicit terminal — the
    /// sidebar `]]u` scans the cursor workspace's terminal, which may not
    /// be the focused tile (#871).
    pub fn urls_for(&mut self, id: TerminalId) -> Option<Vec<String>> {
        let slot = self.terminals.get_mut(&id)?;
        // Reflect every byte received, not just what arrived on screen —
        // mirrors `visible_text` / `target_at`.
        slot.flush_pending();
        // A snapshot-read failure is not "no terminal" — the terminal is
        // right here, we just couldn't extract its grid. Report it as an
        // empty scan so the caller says "no URLs", not "no terminal".
        let Ok(snapshot) = slot.vt.render_state.update(&slot.vt.terminal) else {
            return Some(Vec::new());
        };
        let Ok(mut row_iter) = slot.vt.row_iter.update(&snapshot) else {
            return Some(Vec::new());
        };
        let mut rows: Vec<(String, bool)> = Vec::new();
        while let Some(r) = row_iter.next() {
            let wrapped = r
                .raw_row()
                .ok()
                .and_then(|raw| raw.is_wrapped().ok())
                .unwrap_or(false);
            let (text, _) = row_text_and_starts(&mut slot.vt.cell_iter, r);
            rows.push((text, wrapped));
        }
        let mut urls: Vec<String> = Vec::new();
        let mut i = 0;
        while i < rows.len() {
            // Fold this row and any it wraps into one logical line.
            let mut line = rows[i].0.clone();
            while rows[i].1 && i + 1 < rows.len() {
                i += 1;
                line.push_str(&rows[i].0);
            }
            i += 1;
            for url in scan_urls(&line) {
                // Keep the URL at its LATEST position: drop any earlier
                // sighting before re-appending, so recency ordering holds.
                if let Some(prev) = urls.iter().position(|u| u == url) {
                    urls.remove(prev);
                }
                urls.push(url.to_string());
            }
        }
        Some(urls)
    }

    /// Translate a screen `(col, row)` into 0-based grid-cell
    /// coordinates inside the focused terminal's grid, using the exact
    /// grid rect the renderer drew (`grid_bounds`) — the left
    /// border, the top chrome (tab strip + divider + blank), any recap
    /// rows, and the scrollbar gutter already removed. This is the
    /// coordinate space `encode_mouse_for_focused` expects. Returns
    /// `None` when the point falls outside that grid (border / tab strip
    /// / recap / gutter) so callers forwarding a click to a
    /// mouse-tracking inner program never feed it a cell the renderer
    /// never drew there.
    pub fn screen_to_cell(
        &self,
        rect: tuirealm::ratatui::layout::Rect,
        col: u16,
        row: u16,
    ) -> Option<(u32, u32)> {
        let id = self.focused_terminal_id()?;
        let (inner_x, inner_y, last_col, last_row) = self.grid_bounds(id, rect)?;
        // Reject points OUTSIDE the grid — the border columns, the top
        // chrome / recap rows, the bottom margin, and the scrollbar gutter
        // are never grid cells, so a click there must not be forwarded to
        // the inner program as a bogus near-edge cell.
        if col < inner_x || row < inner_y || col > last_col || row > last_row {
            return None;
        }
        Some(((col - inner_x) as u32, (row - inner_y) as u32))
    }

    pub fn scroll_active(&mut self, delta: isize) -> ScrollOutcome {
        let Some(id) = self.focused_terminal_id() else {
            return ScrollOutcome::NoTerminal;
        };
        self.scroll_terminal(id, delta)
    }

    /// Scroll a specific terminal's viewport by `delta` rows through the
    /// single scroll owner (`TerminalVt::scroll`). Used by the
    /// mouse-wheel handler to move the scrollback of the tile under the
    /// cursor (#362) rather than the focused one; `scroll_active` is the
    /// focused-terminal wrapper used by keyboard scroll. A zero delta is
    /// reported as [`ScrollOutcome::Noop`].
    pub fn scroll_terminal(&mut self, id: TerminalId, delta: isize) -> ScrollOutcome {
        let Some(slot) = self.terminals.get_mut(&id) else {
            return ScrollOutcome::NoTerminal;
        };
        let outcome = slot.vt.scroll(ScrollRequest::By(delta));
        // Deep-scrollback fetch, one per scrollback visit (#393): the
        // local grid was fed from the live byte stream, whose in-place
        // redraws leave almost no scrollback, while the daemon backend
        // (tmux) has retained the full pane history the whole time —
        // the same history a restart already seeds from. Arm on the
        // first upward scroll — even when the local grid has nothing to
        // move into yet (`NoScrollback`), which is exactly the case the
        // fetch exists for — but only for live terminals (an exited
        // pane's backend session is gone). The key / wheel handlers
        // drain the armed id into `Command::FetchScrollback` and the
        // reply rebuilds the grid via `apply_scrollback`.
        if delta < 0 {
            if slot.exited.is_none() && !slot.deep_scrollback_requested {
                slot.deep_scrollback_requested = true;
                self.pending_scrollback_fetch = Some(id);
            }
        } else if delta > 0 && at_live_bottom(outcome) {
            // Back at the live bottom — this scrollback visit is over.
            // The next visit re-fetches so its history is current.
            slot.deep_scrollback_requested = false;
        }
        outcome
    }

    /// Take the armed deep-scrollback fetch, if any. The caller ships
    /// it as a `Command::FetchScrollback`.
    pub fn take_scrollback_fetch(&mut self) -> Option<TerminalId> {
        self.pending_scrollback_fetch.take()
    }

    /// Drain the armed deep-scrollback fetch into an outgoing command.
    fn drain_scrollback_fetch(&mut self, cmds: &mut Vec<Command>) {
        if let Some(terminal_id) = self.take_scrollback_fetch() {
            cmds.push(Command::FetchScrollback { terminal_id });
        }
    }

    pub fn cycle_tab_forward(&mut self) {
        // Clamp first: if a terminal exited mid-session, `active_tab_idx`
        // may exceed the visible-terminals length. Without this, the
        // next forward cycle would wrap from a phantom index instead
        // of from the actual current tab. Symptom user reported:
        // closed all but the lone terminal, Tab-cycle skipped it.
        self.clamp_active_tab();
        let n = self.visible_terminals().len();
        if n == 0 {
            self.active_tab_idx = 0;
            return;
        }
        self.active_tab_idx = (self.active_tab_idx + 1) % n;
    }

    pub fn cycle_tab_backward(&mut self) {
        self.clamp_active_tab();
        let n = self.visible_terminals().len();
        if n == 0 {
            self.active_tab_idx = 0;
            return;
        }
        self.active_tab_idx = if self.active_tab_idx == 0 {
            n - 1
        } else {
            self.active_tab_idx - 1
        };
    }

    fn clamp_active_tab(&mut self) {
        let n = self.visible_terminals().len();
        if n == 0 {
            self.active_tab_idx = 0;
        } else if self.active_tab_idx >= n {
            self.active_tab_idx = n - 1;
        }
    }

    fn append_output(&mut self, id: TerminalId, bytes: &[u8], first_seq: u64, seq: u64) {
        // The focused terminal feeds eagerly even when it hasn't been
        // rendered yet (collapsed pane, or before the first frame): the
        // `&self` mouse/alt-screen readers query its live parser state
        // directly, so deferring its parse would make them stale. Hidden,
        // non-focused terminals — the chatty background agents this whole
        // path exists to bound — are never focused, so they still buffer.
        let eager = self.focused_terminal_id() == Some(id);
        let Some(slot) = self.terminals.get_mut(&id) else {
            return;
        };
        if seq <= slot.last_seq {
            return;
        }
        if let TerminalStreamSync::Desynced {
            request,
            request_pending,
        } = &mut slot.sync
        {
            request.required_seq = request.required_seq.max(seq);
            if !*request_pending {
                *request_pending = true;
                // Fresh output re-drove the request; the timed retry
                // stands down until the reply comes back unavailable.
                slot.resync_retry_at = None;
                self.pending_resync_requests.push(id);
            }
            return;
        }
        if first_seq != slot.last_seq.saturating_add(1) || first_seq > seq {
            let request = TerminalResyncRequest {
                terminal_id: id,
                required_seq: seq,
            };
            slot.sync = TerminalStreamSync::Desynced {
                request,
                request_pending: true,
            };
            self.pending_resync_requests.push(id);
            slot.osc52_carry.clear();
            tracing::warn!(
                terminal_id = ?id,
                last_seq = slot.last_seq,
                first_seq,
                seq,
                "terminal output sequence gap; preserving last coherent grid until resync"
            );
            return;
        }
        // OSC 52 passthrough — if the inner program (Claude,
        // tmux, vim) wrote `ESC ] 52 ; c ; <base64> BEL` to ask
        // the terminal to put text on the clipboard, forward the
        // sequence to the HOST terminal's stdout. Modern hosts
        // (Ghostty / iTerm2 / Kitty / Wezterm) honor it and the
        // user's system clipboard gets updated. Without this,
        // libghostty-vt consumes the sequence internally for its
        // own clipboard (which lazybox doesn't surface).
        forward_osc52(&mut slot.osc52_carry, bytes);
        // Only the displayed terminal pays the VT-parse cost per chunk;
        // hidden ones stash raw bytes and replay them lazily on the
        // first render after they become visible (`flush_pending`).
        // OSC 52 stays eager above so a background agent's clipboard
        // intent isn't deferred behind a tab switch.
        if slot.displayed || eager {
            slot.vt.feed(bytes);
        } else {
            // Never drop the prefix and reset+feed an arbitrary tail: it
            // may begin inside UTF-8/CSI state. Periodically parse a whole
            // ordered batch instead, keeping deferred memory bounded.
            if slot.pending_feed.len().saturating_add(bytes.len()) > PENDING_FEED_CAP {
                slot.flush_pending();
            }
            if bytes.len() > PENDING_FEED_CAP {
                slot.vt.feed(bytes);
            } else {
                slot.pending_feed.extend_from_slice(bytes);
            }
        }
        slot.recent.extend_from_slice(bytes);
        if slot.recent.len() > RECENT_OUTPUT_CAP {
            let excess = slot.recent.len() - RECENT_OUTPUT_CAP;
            slot.recent.drain(..excess);
        }
        slot.last_seq = seq;
    }

    /// Re-establish a terminal's grid from the daemon ring after the
    /// event channel dropped one or more `TerminalOutput` chunks for
    /// it. The parser is reset and re-fed from scratch (a partial
    /// stream would have left garbled escape state), so the
    /// reconstructed screen is correct without the dropped bytes.
    ///
    /// No OSC 52 passthrough here, unlike `append_output`: the replay
    /// is a re-render of bytes the inner program already emitted, not a
    /// fresh "copy this" intent, so re-forwarding clipboard sequences
    /// would spuriously rewrite the user's clipboard.
    fn resync_terminal(&mut self, id: TerminalId, replay: &[u8], seq: u64) {
        let Some(slot) = self.terminals.get_mut(&id) else {
            return;
        };
        if let Some(replay_fingerprint) = debug_byte_fingerprint(replay) {
            let composing_fingerprint = byte_fingerprint(slot.composing.as_bytes());
            tracing::debug!(
                terminal_id = ?id,
                seq,
                replay_len = replay_fingerprint.len,
                replay_newlines = replay_fingerprint.newlines,
                replay_hash = replay_fingerprint.hash,
                draft_len = composing_fingerprint.len,
                draft_newlines = composing_fingerprint.newlines,
                draft_hash = composing_fingerprint.hash,
                "terminal resync received at client reconstruction boundary"
            );
        }
        let required_seq = match slot.sync {
            TerminalStreamSync::Coherent => slot.last_seq,
            TerminalStreamSync::Desynced { request, .. } => request.required_seq,
        };
        if seq < slot.last_seq || seq < required_seq {
            // A reply can satisfy the debt that was on the wire while newer
            // quarantined output raises the local watermark. Re-request that
            // newer watermark immediately; merely releasing the latch leaves
            // the terminal frozen forever when no further output arrives.
            if let TerminalStreamSync::Desynced {
                request_pending, ..
            } = &mut slot.sync
            {
                *request_pending = true;
                if !self.pending_resync_requests.contains(&id) {
                    self.pending_resync_requests.push(id);
                }
            }
            return;
        }
        if !slot.sync.is_desynced() && seq == slot.last_seq {
            return;
        }
        if !slot.vt.reset() {
            // The replay was authoritative, but the local parser could
            // not adopt it. Keep the last coherent grid and immediately
            // request another replay; leaving the old request latch set
            // would deadlock recovery because no unavailable response is
            // coming to release it.
            let request = TerminalResyncRequest {
                terminal_id: id,
                required_seq: seq.max(required_seq),
            };
            slot.sync = TerminalStreamSync::Desynced {
                request,
                request_pending: true,
            };
            self.pending_resync_requests.push(id);
            return;
        }
        slot.vt.feed(replay);
        // The reset parser restarted its revision counter — a stale
        // cached frame keyed to the old counter must never blit.
        slot.last_frame_rev = None;
        // Drop any half-buffered clipboard sequence — the stream is being
        // rebuilt from the ring and we don't re-forward OSC 52 here.
        slot.osc52_carry.clear();
        // The ring replay is authoritative; any bytes buffered while
        // hidden are now stale and already covered by it.
        slot.pending_feed.clear();
        slot.recent.clear();
        let tail_start = replay.len().saturating_sub(RECENT_OUTPUT_CAP);
        slot.recent.extend_from_slice(&replay[tail_start..]);
        slot.last_seq = seq;
        slot.sync = TerminalStreamSync::Coherent;
        // Recovery converged — disarm the retry loop and reset its
        // backoff so the next episode starts fast again.
        slot.resync_retry_at = None;
        slot.resync_retry_backoff = RESYNC_RETRY_INITIAL;
        // The raw-stream rebuild just replaced any capture-fed deep
        // scrollback with the ring's shallow history, so the current
        // scrollback visit's fetch is spent. Release the latch: the
        // next upward scroll re-fetches instead of scrolling a grid
        // the resync silently emptied (#393).
        slot.deep_scrollback_requested = false;
    }

    /// Rebuild a terminal's grid from the daemon's deep-scrollback
    /// capture (`Event::TerminalScrollback`, the reply to
    /// `Command::FetchScrollback`). Same reset-and-refeed shape as
    /// [`Self::resync_terminal`], with two differences the payload
    /// forces:
    ///
    /// - The capture is content-only (tmux `capture-pane` never emits
    ///   DECSET), so terminal modes the inner program enabled — mouse
    ///   tracking, DECCKM, bracketed paste — would silently die with
    ///   the reset. They're read off the old parser and re-asserted
    ///   after the feed. The ring-fed resync path doesn't need this:
    ///   its replay carries the original escape stream.
    /// - The user is mid-scroll (that's what triggered the fetch), so
    ///   the viewport's distance from the bottom is restored instead of
    ///   snapping to the live tail. Distance-from-bottom is the anchor
    ///   because the rebuild grows the history above, not below.
    ///
    /// A capture also normalizes an unexpected alternate-screen client
    /// back onto the history-bearing primary screen. A desynced slot
    /// (mid gap-recovery) keeps its flags: the ring resync it already
    /// requested still arrives and re-feeds the authoritative stream.
    fn apply_scrollback(&mut self, id: TerminalId, replay: &[u8], seq: u64) {
        if replay.is_empty() {
            return;
        }
        let Some(slot) = self.terminals.get_mut(&id) else {
            return;
        };
        let t = &slot.vt.terminal;
        // Pre-flight the rebuild in a scratch parser at the same width
        // and only adopt it when it is actually DEEPER than the current
        // grid. A capture from a pane with no retained history — the
        // pane sat on the alternate screen under an older server config,
        // or was freshly spawned — is ~one screenful, and adopting it
        // would replace whatever scrollback the local grid had: the
        // scrollbar vanished on the very first scroll. The daemon skips
        // those fetches at the source; this guard makes shrinkage
        // impossible regardless of what arrives on the wire.
        let current_total = t.scrollbar().ok().map(|b| b.total).unwrap_or(0);
        let Some(mut scratch) = TerminalVt::new() else {
            return;
        };
        scratch.ensure_size(slot.vt.cols, slot.vt.rows);
        scratch.feed(replay);
        let rebuilt_total = scratch
            .terminal
            .scrollbar()
            .ok()
            .map(|b| b.total)
            .unwrap_or(0);
        if rebuilt_total <= current_total {
            return;
        }
        let preserved: Vec<(u16, bool)> = PRESERVED_DEC_MODES
            .iter()
            .filter_map(|m| t.mode(*m).ok().map(|on| (m.value(), on)))
            .collect();
        let dist_from_bottom = t
            .scrollbar()
            .ok()
            .map(|b| b.total.saturating_sub(b.offset + b.len))
            .unwrap_or(0);
        // Adopt the scratch parser we already built and fed instead of
        // resetting `slot.vt` and re-parsing the capture a second time — a
        // deep capture is multiple megabytes and the pre-flight already did
        // the full parse. Dropping the old parser here is the same grid
        // replacement `reset()` performed, minus the wasted second pass.
        slot.vt = scratch;
        // The fresh parser restarts its revision counter — a stale
        // cached frame keyed to the old counter must never blit.
        slot.last_frame_rev = None;
        let mut modes = Vec::with_capacity(preserved.len() * 8);
        for (value, on) in preserved {
            let flag = if on { 'h' } else { 'l' };
            modes.extend_from_slice(format!("\x1b[?{value}{flag}").as_bytes());
        }
        slot.vt.feed(&modes);
        if dist_from_bottom > 0 {
            // Through the single scroll owner; the restore is
            // best-effort (a capture shallower than the old offset
            // simply lands at the top).
            let _ = slot.vt.scroll(ScrollRequest::By(
                -(dist_from_bottom.min(isize::MAX as u64) as isize),
            ));
        }
        // Same bookkeeping as `resync_terminal`: the capture replaces
        // everything, including any bytes buffered while hidden, and
        // it re-renders already-emitted output so OSC 52 must not be
        // re-forwarded.
        slot.osc52_carry.clear();
        slot.pending_feed.clear();
        slot.recent.clear();
        let tail_start = replay.len().saturating_sub(RECENT_OUTPUT_CAP);
        slot.recent.extend_from_slice(&replay[tail_start..]);
        // The capture may lag chunks the client already applied (the
        // fetch raced live output) — never move the high-water mark
        // backwards or those chunks would be double-fed on re-delivery.
        slot.last_seq = slot.last_seq.max(seq);
    }

    #[allow(clippy::too_many_arguments)]
    fn make_slot(
        session_key: SessionKey,
        kind: TerminalKind,
        last_seq: u64,
        no_permission: bool,
        on_main: bool,
        model_label: Option<String>,
        prompt_history: Vec<lazybox_ipc::UserPrompt>,
        composing: String,
    ) -> TerminalSlot {
        let vt = TerminalVt::new().expect("libghostty-vt init");
        TerminalSlot {
            session_key,
            kind,
            last_seq,
            sync: TerminalStreamSync::Coherent,
            resync_retry_at: None,
            resync_retry_backoff: RESYNC_RETRY_INITIAL,
            vt,
            recent: Vec::new(),
            osc52_carry: Vec::new(),
            agent_state: lazybox_ipc::AgentState::Idle,
            last_rendered_size: None,
            last_frame: None,
            last_frame_rev: None,
            composing,
            prompt_history,
            no_permission,
            on_main,
            model_label,
            displayed: false,
            pending_feed: Vec::new(),
            exited: None,
            authenticating: false,
            auth_recovery_id: None,
            spawned_at: std::time::Instant::now(),
            did_work: false,
            deep_scrollback_requested: false,
        }
    }

    /// The last full user message committed to the given terminal
    /// (the bytes between two Enter presses), or `None` if no
    /// message has been committed — including for shells, where the
    /// composing buffer is intentionally left dormant. Drives the
    /// pinned "you ▸ …" recap line at the top of the agent view.
    pub fn last_user_message_of(&self, id: TerminalId) -> Option<&str> {
        self.terminals
            .get(&id)
            .and_then(|s| s.prompt_history.last())
            .map(|p| p.text.as_str())
    }

    /// The focused agent terminal's prompt history (issue #523),
    /// newest-first with each entry's source, for the `]]h` history
    /// picker. `None` when the focused terminal isn't an agent or has no
    /// history yet. Returns the terminal id so a re-send can target it.
    pub fn focused_prompt_history(&self) -> Option<(TerminalId, Vec<lazybox_ipc::UserPrompt>)> {
        self.prompt_history_for(self.focused_terminal_id()?)
    }

    /// Like [`Self::focused_prompt_history`] but for an explicit terminal —
    /// the sidebar `]]h` addresses the cursor workspace's agent, which may
    /// not be the focused tile (#871).
    pub fn prompt_history_for(
        &self,
        id: TerminalId,
    ) -> Option<(TerminalId, Vec<lazybox_ipc::UserPrompt>)> {
        let slot = self.terminals.get(&id)?;
        if !matches!(slot.kind, TerminalKind::Agent(_)) || slot.prompt_history.is_empty() {
            return None;
        }
        let mut history = slot.prompt_history.clone();
        history.reverse();
        Some((id, history))
    }

    /// The text a `]]r` recall should drop back into the focused agent
    /// composer: the in-flight draft if one survived, otherwise the last
    /// submitted message. Returns the focused agent terminal id with
    /// that text; `None` when the focused terminal isn't an agent or has
    /// nothing to recall. Both sources are restored from the daemon
    /// snapshot, so this works after a full restart (issue #373).
    pub fn recall_prompt(&mut self) -> Option<(TerminalId, String)> {
        self.recall_prompt_for(self.focused_terminal_id()?)
    }

    /// Like [`Self::recall_prompt`] but for an explicit terminal — the
    /// sidebar `]]r` recalls into the cursor workspace's agent, which may
    /// not be the focused tile (#871).
    pub fn recall_prompt_for(&mut self, id: TerminalId) -> Option<(TerminalId, String)> {
        let slot = self.terminals.get_mut(&id)?;
        if !matches!(slot.kind, TerminalKind::Agent(_)) {
            return None;
        }
        let text = if !slot.composing.is_empty() {
            slot.composing.clone()
        } else {
            slot.prompt_history.last()?.text.clone()
        };
        slot.composing = text.clone();
        Some((id, text))
    }

    /// In-flight characters the user has typed but not yet
    /// submitted to the given terminal. Exposed primarily for tests
    /// so they can verify buffer management (commit on Enter, clear
    /// on Ctrl-C, etc.) without having to drive a full render, but
    /// also usable by future surfaces that want a live "draft"
    /// indicator.
    pub fn composing_of(&self, id: TerminalId) -> Option<&str> {
        self.terminals.get(&id).map(|s| s.composing.as_str())
    }

    /// Record a bracketed-paste payload as part of the focused
    /// agent terminal's composing buffer. Pastes don't flow through
    /// `handle_key` (they arrive as a single `Event::Paste` and the
    /// realm forwards them straight to the PTY), so without this
    /// hook a long pasted prompt would commit on Enter as a blank
    /// recap. No-op for non-Agent terminals. Returns the focused
    /// terminal id and its updated draft so the caller can persist it
    /// via `Command::RecordComposingBuffer`; `None` when the paste
    /// didn't land on an agent.
    pub fn record_paste(&mut self, text: &str) -> Option<(TerminalId, String)> {
        let id = self.focused_terminal_id()?;
        self.record_compose_insert(id, text)
    }

    /// Append `text` to a specific terminal's composing buffer without
    /// submitting — the recap counterpart of a daemon-side compose insert
    /// (the `Shift-Enter` snippet insert, #791). Unlike [`Self::record_paste`]
    /// this targets `id` explicitly rather than the focused tile, so it
    /// tracks the terminal the snippet is actually delivered to (which may
    /// not be the focused split). Keeping the client's live composing buffer
    /// in step is what lets a later manual submit recap the full prompt and
    /// what makes the persisted draft (`RecordComposingBuffer`) survive the
    /// next keystroke instead of being clobbered by a body-less buffer.
    /// No-op (returns `None`) for non-Agent terminals — a shell has no
    /// composer recap. Returns the terminal id and its updated draft so the
    /// caller can persist it.
    pub fn record_compose_insert(
        &mut self,
        id: TerminalId,
        text: &str,
    ) -> Option<(TerminalId, String)> {
        let slot = self.terminals.get_mut(&id)?;
        if !matches!(slot.kind, TerminalKind::Agent(_)) {
            return None;
        }
        slot.append_paste(text);
        Some((id, slot.composing.clone()))
    }

    /// Mirror bytes written straight to a terminal's PTY — bypassing
    /// the per-keystroke `handle_key` path — into the recap + history
    /// state. Used by callers that synthesise a full command and submit
    /// it in one shot (snippet expansion writes the body + a trailing
    /// `\r`), which would otherwise leave the "you ▸ …" recap showing the
    /// previous message. `source` tags the entry (`Snippet{..}` for the
    /// `]]s` picker, `Typed` for free-text broadcast/handoff/resend).
    /// No-op for non-Agent terminals. When the write ends in a submit,
    /// appends the stamped `UserPrompt` to the slot history and returns
    /// it so the caller can persist it via `Command::RecordUserMessage`.
    pub fn record_pty_write(
        &mut self,
        id: TerminalId,
        bytes: &[u8],
        source: lazybox_ipc::PromptSource,
    ) -> Option<lazybox_ipc::UserPrompt> {
        let slot = self.terminals.get_mut(&id)?;
        if !matches!(slot.kind, TerminalKind::Agent(_)) {
            return None;
        }
        let text = slot.record_pty_bytes(bytes)?;
        let prompt = lazybox_ipc::UserPrompt {
            text,
            timestamp_ms: now_ms(),
            source,
        };
        slot.push_prompt(prompt.clone());
        Some(prompt)
    }

    /// Apply an agent prompt the daemon confirmed as delivered. The daemon
    /// supplies the authoritative entry so the recap cannot get ahead of
    /// terminal delivery.
    pub fn apply_delivered_prompt(&mut self, id: TerminalId, prompt: lazybox_ipc::UserPrompt) {
        let Some(slot) = self.terminals.get_mut(&id) else {
            return;
        };
        if !matches!(slot.kind, TerminalKind::Agent(_)) {
            return;
        }
        slot.composing.clear();
        slot.push_prompt(prompt);
    }

    fn tab_label(kind: &TerminalKind) -> String {
        match kind {
            TerminalKind::Agent(name) => name.clone(),
            TerminalKind::Shell => "shell".into(),
            TerminalKind::LogTail { path } => {
                // Short label: last path segment.
                path.rsplit('/').next().unwrap_or(path).to_string()
            }
        }
    }
}

/// Inherent methods. Lifted from the legacy `tui_kit::Pane` trait.
impl TerminalStack {
    /// Stable pane id.
    pub fn id(&self) -> PaneId {
        self.id
    }

    /// Border title.
    pub fn title(&self) -> &str {
        "Terminals"
    }

    /// Bindings shown in the hint bar. Terminal commands share one
    /// leader, whose popup carries the individual shortcuts.
    pub fn contextual_bindings(escape_char: char) -> Vec<crate::Binding> {
        use crate::Binding;
        use std::borrow::Cow;
        let leader = format!("{escape_char}{escape_char}");
        vec![Binding {
            keys: Cow::Owned(leader),
            label: Cow::Borrowed("menu"),
        }]
    }

    pub fn handle_key(&mut self, key: KeyEvent, cmds: &mut Vec<Command>) -> PaneOutcome {
        // Scrollback navigation. Mouse-wheel scroll is the primary
        // path (handled by the orchestrator's `handle_mouse`), but
        // a keyboard fallback matters: some host terminals don't
        // forward wheel events under mouse-capture, and reachable-
        // by-keyboard-only is a hard requirement for accessibility.
        //
        // Bindings mirror what iTerm2 / Ghostty / VS Code use:
        //   Shift-PageUp / Shift-PageDown — scroll by `STEP` rows
        //   Shift-Home / Shift-End       — jump to top / bottom
        const STEP: isize = 8;
        if key.modifiers.contains(KeyModifiers::SHIFT) {
            match key.code {
                KeyCode::PageUp => {
                    let _outcome = self.scroll_active(-STEP);
                    self.drain_scrollback_fetch(cmds);
                    return PaneOutcome::Consumed;
                }
                KeyCode::PageDown => {
                    let _outcome = self.scroll_active(STEP);
                    return PaneOutcome::Consumed;
                }
                KeyCode::Home => {
                    let _outcome = self.scroll_to_top();
                    self.drain_scrollback_fetch(cmds);
                    return PaneOutcome::Consumed;
                }
                KeyCode::End => {
                    let _outcome = self.scroll_to_bottom();
                    return PaneOutcome::Consumed;
                }
                _ => {}
            }
        }

        // An exited agent pane (#356) is frozen — its PTY is gone, so
        // typing can't reach a process. Intercept the restart affordance
        // (`r` / Enter) and swallow every other printable key instead of
        // pretending to feed a dead terminal. Scrollback (handled above)
        // still works so the last output stays inspectable, and `]]x` to
        // close rides the app-level leader, not this path.
        if let Some(id) = self
            .focused_terminal_id()
            .or_else(|| self.active_terminal_id())
            && self.terminals.get(&id).is_some_and(|s| s.exited.is_some())
        {
            if matches!(key.code, KeyCode::Char('r') | KeyCode::Enter) {
                self.restart_exited(id, cmds);
            }
            return PaneOutcome::Consumed;
        }

        // Escape sequence is owned by the app-level dispatcher (see
        // `dispatch_key` in app.rs). It uses a double-Esc latch on
        // `AppState` because that state needs to persist across calls.
        // Here we just route bytes to the focused terminal; everything
        // — q, Tab, Ctrl-C, single Esc — is the agent's.
        let id = self
            .focused_terminal_id()
            .or_else(|| self.active_terminal_id());
        let Some(id) = id else {
            // No terminal to route to — let the parent handle.
            return PaneOutcome::Pass;
        };
        if !self.terminals.contains_key(&id) {
            return PaneOutcome::Pass;
        }

        let Some(bytes) = key_to_bytes(&key) else {
            return PaneOutcome::Consumed;
        };
        // Mirror the exact bytes we're about to ship into our own
        // composing buffer so the pinned "you ▸ …" recap reflects the
        // latest submitted message. Parsing the byte stream (rather
        // than the `KeyEvent`) keeps the recap in lock-step with what
        // the agent receives. Scoped to Agent terminals — shells don't
        // have a single semantic "user prompt", so the recap would be
        // noisy (every cd, every grep) and surprising.
        let (committed, draft) = if let Some(slot) = self.terminals.get_mut(&id)
            && matches!(slot.kind, TerminalKind::Agent(_))
        {
            let before = slot.composing.clone();
            // A typed submit is always `Typed`; snippet-sourced sends go
            // through `deliver_prompt`, which stamps `Snippet` instead.
            let committed = slot.record_pty_bytes(&bytes).map(|text| {
                let prompt = lazybox_ipc::UserPrompt {
                    text,
                    timestamp_ms: now_ms(),
                    source: lazybox_ipc::PromptSource::Typed,
                };
                slot.push_prompt(prompt.clone());
                prompt
            });
            // Only the keys that actually edit the in-flight line
            // (text, backspace, Ctrl-U, a commit that clears it) yield a
            // draft to persist; arrows / mouse / Ctrl-C leave it be, so
            // this is `Some` on real edits only.
            let draft = (slot.composing != before).then(|| slot.composing.clone());
            (committed, draft)
        } else {
            (None, None)
        };
        let intent = if key.code == KeyCode::Enter && !key.modifiers.contains(KeyModifiers::SHIFT) {
            TerminalInputIntent::Submit
        } else {
            TerminalInputIntent::Compose
        };
        Self::push_write(cmds, id, bytes, intent);
        // Persist the submitted prompt daemon-side so the history survives
        // a restart — the replay ring only carries PTY output, not the
        // input we composed here.
        if let Some(prompt) = committed {
            cmds.push(Command::RecordUserMessage {
                terminal_id: id,
                prompt,
            });
        }
        // Persist the in-flight draft too (issue #373) so a restart can
        // recall a half-typed prompt via `]]r`. A commit clears the
        // buffer, so this ships an empty string that clears the stored
        // draft — the submitted message it carried lives on in the recap.
        if let Some(buffer) = draft {
            cmds.push(Command::RecordComposingBuffer {
                terminal_id: id,
                buffer,
            });
        }
        PaneOutcome::Consumed
    }

    /// Single emission point for PTY-bound bytes from this pane. Large
    /// payloads (a bracketed paste, a burst injection) are split via
    /// [`Command::write_chunked`] so each resulting `Command::Write`
    /// frames under the daemon's `MAX_COMMAND_FRAME_BYTES` socket
    /// ingress cap — one unchunked >256 KiB write used to kill a
    /// `--connect` session. Chunks are pushed in order onto the same
    /// `cmds` vec (and later the same ordered command channel), so the
    /// PTY receives the identical byte stream; splitting mid-UTF-8 or
    /// mid-escape is fine for a byte-oriented PTY.
    fn push_write(
        cmds: &mut Vec<Command>,
        terminal_id: TerminalId,
        bytes: Vec<u8>,
        intent: TerminalInputIntent,
    ) {
        cmds.extend(Command::write_chunked(terminal_id, bytes, intent));
    }

    pub fn on_event(&mut self, event: &Event) {
        match event {
            Event::Snapshot { terminals, .. } => {
                // The width the focused pane last rendered at, captured
                // before the rebuild. It's the fallback size for a focused
                // terminal that arrives brand new in this snapshot (a
                // lag-recovery snapshot introducing a not-yet-seen terminal
                // in the active session), so its eager flush below still
                // parses at the real pane width rather than the VT default.
                let prev_focused_size = self
                    .focused_terminal_id()
                    .and_then(|id| self.terminals.get(&id))
                    .and_then(|slot| slot.last_rendered_size);
                let mut previous = std::mem::take(&mut self.terminals);
                for snap in terminals {
                    if let Some(replay_fingerprint) = debug_byte_fingerprint(&snap.replay) {
                        let composing_fingerprint = snap
                            .composing_buffer
                            .as_deref()
                            .map(str::as_bytes)
                            .map(byte_fingerprint);
                        tracing::debug!(
                            terminal_id = ?snap.terminal_id,
                            last_seq = snap.last_seq,
                            replay_available = snap.replay_available,
                            replay_len = replay_fingerprint.len,
                            replay_newlines = replay_fingerprint.newlines,
                            replay_hash = replay_fingerprint.hash,
                            draft_present = composing_fingerprint.is_some(),
                            draft_len = composing_fingerprint.map_or(0, |fingerprint| fingerprint.len),
                            draft_newlines = composing_fingerprint
                                .map_or(0, |fingerprint| fingerprint.newlines),
                            draft_hash = composing_fingerprint
                                .map_or(0, |fingerprint| fingerprint.hash),
                            "terminal snapshot applied at client reconstruction boundary"
                        );
                    }
                    if !snap.replay_available
                        && let Some(mut slot) = previous.remove(&snap.terminal_id)
                    {
                        // A lag-recovery snapshot may fail for one backend.
                        // Preserve the user's last coherent screen, refresh
                        // identity/badges, and stop applying output until a
                        // later authoritative resync succeeds.
                        slot.session_key = snap.session_key.clone();
                        slot.kind = snap.kind.clone();
                        slot.no_permission = snap.no_permission;
                        slot.on_main = snap.on_main;
                        slot.model_label = snap.model_label.clone();
                        slot.prompt_history = snap.prompt_history.clone();
                        slot.composing = snap.composing_buffer.clone().unwrap_or_default();
                        slot.authenticating = snap.authenticating;
                        if let Some(state) = snap.agent_state {
                            slot.agent_state = state;
                        }
                        let request = TerminalResyncRequest {
                            terminal_id: snap.terminal_id,
                            required_seq: slot.last_seq.max(snap.last_seq),
                        };
                        slot.sync = TerminalStreamSync::Desynced {
                            request,
                            request_pending: true,
                        };
                        self.pending_resync_requests.push(snap.terminal_id);
                        self.invalidate_visible();
                        self.terminals.insert(snap.terminal_id, slot);
                        continue;
                    }
                    let mut slot = Self::make_slot(
                        snap.session_key.clone(),
                        snap.kind.clone(),
                        snap.last_seq,
                        snap.no_permission,
                        snap.on_main,
                        snap.model_label.clone(),
                        snap.prompt_history.clone(),
                        snap.composing_buffer.clone().unwrap_or_default(),
                    );
                    slot.agent_state = snap.agent_state.unwrap_or(lazybox_ipc::AgentState::Idle);
                    slot.authenticating = snap.authenticating;
                    if !snap.replay_available {
                        // Total snapshot budgeting and transient backend
                        // failures omit whole replays. Ask for this one
                        // terminal immediately so a quiet pane cannot stay
                        // blank forever waiting for live output.
                        let request = TerminalResyncRequest {
                            terminal_id: snap.terminal_id,
                            required_seq: snap.last_seq,
                        };
                        slot.sync = TerminalStreamSync::Desynced {
                            request,
                            request_pending: true,
                        };
                        self.pending_resync_requests.push(snap.terminal_id);
                    }
                    // Defer the daemon-ring replay instead of parsing
                    // it here: a reconnect / broadcast-lag snapshot
                    // carries EVERY terminal's full ring (potentially
                    // MiBs each), and feeding them all through
                    // libghostty synchronously in one dispatch stalled
                    // the single UI thread for the whole batch. The
                    // slot is brand-new (empty grid), so the stash is
                    // clean replace-not-append semantics: the replay
                    // becomes the first bytes the fresh parser sees on
                    // the terminal's first render (`flush_pending`) —
                    // no reset needed, unlike the hidden-overflow path
                    // — and live output arriving before then appends
                    // behind it in stream order. `PENDING_FEED_CAP`
                    // deliberately does not apply to this one-shot
                    // stash (it bounds the *between-render* backlog of
                    // a chatty hidden agent); the replay is already
                    // bounded by the daemon's ring, and capping it here
                    // would silently drop scrollback the old eager path
                    // preserved. A later live overflow while hidden
                    // still trims to the cap tail via `append_output`.
                    if snap.replay_available {
                        slot.pending_feed = snap.replay.clone();
                    }
                    self.invalidate_visible();
                    self.terminals.insert(snap.terminal_id, slot);
                }
                self.clamp_active_tab();
                self.auto_collapse_on_emptiness();
                // Eagerly parse only the terminal actually in the
                // foreground: the `&self` readers (mouse-tracking probe,
                // alt-screen check) consult its live parser state between
                // now and the next render, and it's what the user is
                // looking at. Everything else parses lazily on first
                // display, at which point `render` sizes the grid before it
                // flushes — so only this eager path can parse at the wrong
                // width.
                //
                // Size the VT to the width the pane will render at before
                // flushing, so a cursor-relative redraw in the replay (an
                // agent status block redrawn with `ESC[<n>A`) lands on the
                // right rows instead of scrolling stale copies into
                // reconstructed scrollback (#1405). The width is the
                // terminal's own last-rendered size, or the pane's if it
                // arrived brand new this snapshot. With neither known (a
                // cold client that has never rendered — where `handle_mouse`
                // early-returns on a zero `last_area`, so the probes can't be
                // consulted anyway) we defer to the render.
                if let Some(id) = self.focused_terminal_id() {
                    let width = previous
                        .get(&id)
                        .and_then(|prev| prev.last_rendered_size)
                        .or(prev_focused_size);
                    if let Some((cols, rows)) = width
                        && let Some(slot) = self.terminals.get_mut(&id)
                    {
                        slot.vt.ensure_size(cols, rows);
                        slot.flush_pending();
                    }
                }
            }
            Event::TerminalSpawned {
                terminal_id,
                session_key,
                kind,
                no_permission,
                on_main,
                model_label,
            } => {
                let slot = Self::make_slot(
                    session_key.clone(),
                    kind.clone(),
                    0,
                    *no_permission,
                    *on_main,
                    model_label.clone(),
                    Vec::new(),
                    String::new(),
                );
                self.invalidate_visible();
                self.terminals.insert(*terminal_id, slot);
                // A fresh terminal arrived for the active session —
                // expand so the user actually sees it. We bypass the
                // user-override here on purpose: spawning is itself an
                // explicit user action, and silently leaving the
                // section collapsed would make the user wonder if
                // anything happened.
                if Some(session_key) == self.active_session.as_ref() {
                    self.collapsed = false;
                    self.collapse_user_set = true;
                }
                // Stage 2 of a `]]|` split: wrap the focused leaf
                // in a fresh split with this new terminal as the
                // sibling. Without this, the new shell shows up as a
                // tab but never enters the tile tree. Consumed only by
                // a spawn on the ACTIVE session (a spawn elsewhere must
                // not eat the marker while the split's own shell is
                // still coming) and only while fresh — if the split's
                // spawn failed daemon-side, the marker must not lie in
                // wait and hijack an unrelated spawn minutes later.
                let pending = if Some(session_key) == self.active_session.as_ref() {
                    match self.pending_split.take() {
                        Some((direction, at)) if at.elapsed() <= PENDING_SPLIT_WINDOW => {
                            Some(direction)
                        }
                        // Stale marker (or none): drop it and let the
                        // spawn take the normal auto-layout path below.
                        _ => None,
                    }
                } else {
                    None
                };
                if let Some(direction) = pending {
                    self.commit_pending_split(*terminal_id, direction);
                } else if Some(session_key) == self.active_session.as_ref() {
                    let session_count = self
                        .terminals
                        .iter()
                        .filter(|(_, slot)| Some(&slot.session_key) == self.active_session.as_ref())
                        .count();
                    if session_count >= 2 && self.auto_split_on_spawn() {
                        // Two-or-more terminals on the same session: the
                        // user wants to see both. Auto-promote (Tabs) or
                        // extend (Splits) into a vertical split so the new
                        // arrival lands beside the existing tiles AND
                        // becomes the focused leaf — without this the spawn
                        // would either hide behind the active tab or, in an
                        // existing split, never enter the tile tree at all.
                        self.commit_pending_split(*terminal_id, PendingSplit::Vertical);
                    } else if let Some(idx) = self
                        .visible_terminals()
                        .iter()
                        .position(|id| id == terminal_id)
                    {
                        // Stay in Tabs and make the fresh spawn the active
                        // tab so the user lands on it. Reached for the first
                        // terminal (cheaper render, no wasted dividers) and,
                        // under `ui.terminal_new_layout: tabs`, for every
                        // later spawn too — a new tab instead of a split.
                        self.active_tab_idx = idx;
                    }
                }
            }
            Event::TerminalReplaced {
                old_terminal_id,
                terminal_id,
                session_key,
                kind,
                no_permission,
                on_main,
                model_label,
                authenticating,
            } => {
                self.replace_terminal(
                    *old_terminal_id,
                    *terminal_id,
                    session_key.clone(),
                    kind.clone(),
                    *no_permission,
                    *on_main,
                    model_label.clone(),
                    *authenticating,
                );
            }
            Event::TerminalOutput {
                terminal_id,
                bytes,
                first_seq,
                seq,
            } => {
                self.append_output(*terminal_id, bytes, *first_seq, *seq);
            }
            Event::AgentAuthOutput {
                terminal_id,
                bytes,
                first_seq,
                seq,
            } => {
                self.append_output(*terminal_id, bytes, *first_seq, *seq);
            }
            Event::AgentAuthReplay {
                terminal_id,
                replay,
                seq,
            } => {
                self.resync_terminal(*terminal_id, replay, *seq);
            }
            Event::TerminalResync {
                terminal_id,
                replay,
                seq,
            } => {
                self.resync_terminal(*terminal_id, replay, *seq);
            }
            Event::TerminalResyncUnavailable { terminal_id } => {
                if let Some(slot) = self.terminals.get_mut(terminal_id) {
                    match &mut slot.sync {
                        TerminalStreamSync::Desynced {
                            request_pending, ..
                        } => *request_pending = false,
                        TerminalStreamSync::Coherent => {
                            slot.sync = TerminalStreamSync::Desynced {
                                request: TerminalResyncRequest {
                                    terminal_id: *terminal_id,
                                    required_seq: slot.last_seq,
                                },
                                request_pending: false,
                            };
                        }
                    }
                    // Arm the tick-driven retry (#1254 finding 2): with
                    // the request latch released, NOTHING else re-drives
                    // recovery when the terminal never produces another
                    // byte — a finished agent's desynced pane froze
                    // forever. Bounded backoff, retried until the daemon
                    // finally serves an authoritative replay.
                    slot.resync_retry_at =
                        Some(std::time::Instant::now() + slot.resync_retry_backoff);
                    slot.resync_retry_backoff =
                        (slot.resync_retry_backoff * 2).min(RESYNC_RETRY_CAP);
                }
            }
            Event::TerminalScrollback {
                terminal_id,
                replay,
                seq,
            } => {
                self.apply_scrollback(*terminal_id, replay, *seq);
            }
            Event::TerminalFocusRequested { terminal_id } => {
                // Daemon-driven focus from the singleton guard.
                // Make the matching tab active + bring the pane up.
                self.focus_terminal(*terminal_id);
            }
            Event::AgentState {
                terminal_id, state, ..
            } => {
                // The tab badge is per-terminal. The daemon caches and
                // broadcasts agent state per terminal, so a workspace
                // running two agents (a `]]|` split, or claude +
                // codex) must have each badge track its OWN terminal.
                // Applying by `session_key` instead clobbered every
                // sibling badge with the last terminal's state — a
                // busy agent read Idle the moment a quiet sibling
                // emitted, and an idle one read Working off a busy
                // sibling. Update only the slot the event names.
                if let Some(slot) = self.terminals.get_mut(terminal_id)
                    && matches!(slot.kind, TerminalKind::Agent(_))
                {
                    slot.agent_state = *state;
                    // A `Working` / `InputNeeded` / `Done` reading means
                    // the agent engaged — the signal that separates a
                    // genuine clean exit from a dead-on-arrival one (#367).
                    // `Exited` is NOT engagement: it's the death itself,
                    // and the PTY-exit teardown broadcasts it right before
                    // `TerminalExited` (#357/#369). Counting it as work
                    // would flip `did_work` true a beat before the exit is
                    // triaged, so a dead-on-arrival launch would read as
                    // "clean" and auto-close instead of keeping its
                    // failed-to-start pane (#367/#368).
                    if !matches!(
                        state,
                        lazybox_ipc::AgentState::Idle | lazybox_ipc::AgentState::Exited { .. }
                    ) {
                        slot.did_work = true;
                    }
                }
            }
            Event::TerminalModelChanged {
                terminal_id,
                model_label,
                ..
            } => {
                // The tab badge is per-terminal; the daemon's live model
                // reading (Codex's `<model> <effort>` footer) supersedes
                // the spawn-time tier label on the same slot field.
                if let Some(slot) = self.terminals.get_mut(terminal_id) {
                    slot.model_label = Some(model_label.clone());
                }
            }
            Event::TerminalExited {
                terminal_id,
                exit_code,
                last_output,
            } => {
                let user_closed = self.closing.remove(terminal_id);
                let is_agent = self
                    .terminals
                    .get(terminal_id)
                    .is_some_and(|s| matches!(s.kind, TerminalKind::Agent(_)));
                // A shell going away — or any terminal the user closed
                // with `]]x` — takes its pane with it, like every other
                // terminal emulator. An AGENT that exited on its own is
                // triaged: a clean, expected exit auto-closes as it used
                // to (#367), but an abnormal one (non-zero code, killed
                // by signal, or dead-on-arrival) must NOT silently
                // vanish — its slot is kept frozen on the last screen so
                // the workspace survives and a restart is offered
                // (#356/#357).
                if is_agent && !user_closed && !self.agent_exit_is_clean(terminal_id, *exit_code) {
                    let window = self.dead_on_arrival;
                    if let Some(slot) = self.terminals.get_mut(terminal_id) {
                        // Decide dead-on-arrival here, at exit, from the
                        // same engagement/grace signals `agent_exit_is_clean`
                        // reads — never at render time, where `elapsed`
                        // keeps growing and would eventually flip a frozen
                        // pane. A kept `code 0` exit that never engaged and
                        // died inside the window failed to launch (#367);
                        // that drives the "failed to start" wording and
                        // painting the captured tail (#368). A non-zero
                        // code or signal death is kept too, but as a plain
                        // crash — not dead-on-arrival.
                        let dead_on_arrival = *exit_code == Some(0)
                            && !slot.did_work
                            && slot.spawned_at.elapsed() < window;
                        slot.exited = Some(TerminalExit {
                            code: *exit_code,
                            dead_on_arrival,
                            last_output: last_output.clone(),
                        });
                        // Reclaim the VT (2026-08-19 audit, M3): a
                        // frozen pane renders its freeze-frame
                        // (`last_frame`) from here on, and the dead
                        // parser would otherwise pin up to the full
                        // scrollback ceiling until the user closes the
                        // pane — with crashes routine at fleet scale,
                        // that accumulated. Only when a painted frame
                        // exists to freeze on: a never-displayed slot
                        // keeps its VT (bounded, and `pending_feed`
                        // still renders on first show).
                        if slot.last_frame.is_some()
                            && let Some(fresh) = TerminalVt::new()
                        {
                            slot.vt = fresh;
                            slot.last_frame_rev = None;
                        }
                    }
                } else {
                    self.drop_slot(*terminal_id);
                }
            }
            Event::AgentAuthProgress {
                recovery_terminal_id,
                terminal_id,
                phase: _,
            } => {
                if let Some(slot) = self.terminals.get_mut(terminal_id) {
                    slot.authenticating = true;
                    slot.auth_recovery_id = Some(*recovery_terminal_id);
                    slot.exited = None;
                }
                self.focus_terminal(*terminal_id);
            }
            Event::AgentAuthFinished {
                recovery_terminal_id,
                terminal_id,
                success: false,
                ..
            } => {
                if let Some(slot) = self.terminals.get_mut(terminal_id) {
                    slot.authenticating = true;
                    slot.auth_recovery_id = Some(*recovery_terminal_id);
                    slot.exited = Some(TerminalExit {
                        code: Some(1),
                        dead_on_arrival: false,
                        last_output: None,
                    });
                }
            }
            Event::TerminalsRebadged { from, to } => {
                // The daemon moved every terminal keyed to `from` onto
                // `to` (issue→PR collapse or manual adopt). Re-point our
                // slots so they follow — and crucially so the
                // `WorkspaceRemoved(from)` that trails a collapse no
                // longer matches them and drops the live session.
                for (terminal_id, slot) in &mut self.terminals {
                    if &slot.session_key == from {
                        if let Some(composing_fingerprint) =
                            debug_byte_fingerprint(slot.composing.as_bytes())
                        {
                            tracing::debug!(
                                terminal_id = ?terminal_id,
                                from = %from,
                                to = %to,
                                draft_len = composing_fingerprint.len,
                                draft_newlines = composing_fingerprint.newlines,
                                draft_hash = composing_fingerprint.hash,
                                "terminal draft crossing rebadge boundary"
                            );
                        }
                        slot.session_key = to.clone();
                    }
                }
                if let Some(id) = self.last_focused.remove(from) {
                    self.last_focused.insert(to.clone(), id);
                }
                if self.active_session.as_ref() == Some(from) {
                    self.invalidate_visible();
                    self.active_session = Some(to.clone());
                }
            }
            Event::WorkspaceRemoved(workspace_key) => {
                // Drop every terminal that belonged to the removed
                // workspace. Wire-side the slot's session_key carries
                // the workspace's key string, so a literal compare
                // is enough.
                let key_str = workspace_key.as_str();
                self.terminals
                    .retain(|_, slot| slot.session_key.as_str() != key_str);
                // Forget remembered focus for the gone workspace so a
                // re-created one with the same key starts fresh instead
                // of restoring a pane from a previous incarnation.
                self.last_focused.retain(|sk, _| sk.as_str() != key_str);
                self.clamp_active_tab();
                self.auto_collapse_on_emptiness();
            }
            _ => {}
        }
    }

    pub fn render(&mut self, area: Rect, frame: &mut Frame, focused: bool) {
        // Modern minimal: title row + thin divider, no surrounding box.
        let theme = crate::theme::current();

        // Re-derive on-screen status from scratch each frame. Every slot
        // starts hidden; `render_one_terminal` flips the ones it actually
        // draws back on (and flushes their buffered bytes). A terminal
        // that fell off-screen this frame — collapsed pane, switched
        // session/tab — therefore reverts to buffering its output.
        for slot in self.terminals.values_mut() {
            slot.displayed = false;
        }

        let visible = self.visible_terminals();
        let title_area = Rect::new(
            area.x + 1,
            area.y,
            area.width.saturating_sub(2),
            1.min(area.height),
        );
        // Clear last frame's tab click targets — terminals may have
        // come or gone, indices shifted, area resized. We'll
        // repopulate as the tab spans go in.
        self.tab_strip_hits.clear();
        // Cleared for the same reason as the tab hits — each render
        // re-records every visible tile's rect from scratch so the
        // wheel handler hit-tests against the current layout.
        self.tile_hits.clear();
        // Title row: a mode label plus an icon+label per active terminal
        // (e.g. `Terminals    claude   _ shell`). Active is bold-accent;
        // inactive is dim grey. Two-tab common case looks like a tab
        // strip; single-terminal shows just one entry.
        //
        // When the pane has focus the label becomes an explicit
        // "▶ typing to" pointer (#1110): co-located with the tab strip,
        // it makes the focused terminal unmistakable so a user can't
        // mistake navigation mode for "typing to the agent". Unfocused
        // it reads as the quiet "Terminals" heading. The two forms are
        // padded to the same width so the tab strip doesn't jump.
        let title_prefix = if focused {
            "▶ typing to  "
        } else {
            "Terminals    "
        };
        let mut title_spans: Vec<Span<'static>> =
            vec![Span::styled(title_prefix, theme.title(focused))];
        // Cursor in cells — used to compute the column range each
        // tab label occupies for click-hit-testing.
        let mut cursor: u16 = title_area.x + title_prefix.chars().count() as u16;
        for (i, id) in visible.iter().enumerate() {
            let (icon, label, agent_state, no_permission, on_main, model_label) = self
                .terminals
                .get(id)
                .map(|s| {
                    let icon: &'static str = match &s.kind {
                        TerminalKind::Shell => crate::components::icons::SHELL,
                        TerminalKind::Agent(agent_id) => {
                            crate::components::icons::agent_icon(agent_id)
                        }
                        // Log-tail terminals reuse the shell glyph for now.
                        _ => crate::components::icons::SHELL,
                    };
                    (
                        icon,
                        Self::tab_label(&s.kind),
                        Some(s.agent_state),
                        s.no_permission,
                        s.on_main,
                        s.model_label.clone(),
                    )
                })
                .unwrap_or((
                    crate::components::icons::SHELL,
                    "?".into(),
                    None,
                    false,
                    false,
                    None,
                ));
            let is_active = i == self.active_tab_idx;
            let style = if is_active && focused {
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else if is_active {
                Style::default()
                    .fg(theme.text_strong)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.chrome)
            };
            if i > 0 {
                title_spans.push(Span::raw("  "));
                cursor = cursor.saturating_add(2);
            }
            let tab_text = format!("{icon} {label}");
            let tab_w = tab_text.chars().count() as u16;
            // Record the click range BEFORE pushing the span so
            // `cursor` is the tab's start column. End is exclusive.
            self.tab_strip_hits
                .push((i, cursor..cursor.saturating_add(tab_w), title_area.y));
            cursor = cursor.saturating_add(tab_w);
            title_spans.push(Span::styled(tab_text, style));
            // State hint next to the tab, mirroring the sidebar's
            // single state slot. Bold yellow "! needs input" when the
            // agent is waiting on the user (stays prominent regardless
            // of which tab is active so a Claude prompt is noticed even
            // while typing in another shell); a dim accent "· working"
            // while it streams. Idle/untracked shows nothing.
            // An exited agent pane (#356) overrides the live state hint —
            // a stale "working" on a crashed tab would be actively
            // misleading.
            let exited = self.terminals.get(id).is_some_and(|s| s.exited.is_some());
            if let Some((label, hint_style)) = Self::agent_state_badge(
                agent_state.unwrap_or(lazybox_ipc::AgentState::Idle),
                exited,
                false,
                theme,
            ) {
                let hint = format!(" {label}");
                let hint_w = hint.chars().count() as u16;
                title_spans.push(Span::styled(hint, hint_style));
                cursor = cursor.saturating_add(hint_w);
            }
            // No-permission / bypass mode: this session auto-accepts
            // tool-use prompts and runs unattended. A compact `⚠` glyph
            // flags it at a glance without crowding the tab strip; the
            // full meaning surfaces as a footer hint on focus (#989).
            if no_permission {
                let noperm_text = " ⚠";
                title_spans.push(Span::styled(
                    noperm_text,
                    Style::default().fg(theme.warn).add_modifier(Modifier::DIM),
                ));
                cursor = cursor.saturating_add(noperm_text.chars().count() as u16);
            }
            // On-main: this session runs on the repo's shared main
            // checkout, not an isolated worktree — flag it so it's
            // obvious edits here touch the shared branch directly.
            if on_main {
                let main_text = " ⎇ main";
                title_spans.push(Span::styled(
                    main_text,
                    Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
                ));
                cursor = cursor.saturating_add(main_text.chars().count() as u16);
            }
            // Model tier: the session was launched at a non-default
            // model via a `w S` / `a S` chord — show which one so the
            // user remembers what's running behind this tab.
            if let Some(tier) = &model_label {
                let tier_text = format!(" ◆ {tier}");
                let width = tier_text.chars().count() as u16;
                title_spans.push(Span::styled(
                    tier_text,
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ));
                cursor = cursor.saturating_add(width);
            }
        }
        frame.render_widget(Paragraph::new(Line::from(title_spans)), title_area);

        if area.height >= 2 {
            let div_area = Rect::new(area.x + 1, area.y + 1, area.width.saturating_sub(2), 1);
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "─".repeat(div_area.width as usize),
                    // Accent while the pane has focus — same at-a-glance
                    // focus cue the sidebar / activity dividers use (#286).
                    theme.pane_divider(focused),
                ))),
                div_area,
            );
        }

        // Top chrome eats 3 rows (title + divider + blank). The bottom
        // row is held back as a blank margin so the inner program's last
        // line — e.g. Claude Code's "? for shortcuts" — never renders
        // flush against the footer hint bar one row below. Without the
        // gap that line abuts lazybox's hints and reads as one of them,
        // appearing or vanishing purely with scroll position (#20).
        let inner = Rect {
            x: area.x + 1,
            y: area.y + 3,
            width: area.width.saturating_sub(2),
            height: area.height.saturating_sub(4),
        };

        if visible.is_empty() {
            let line = Line::from(Span::styled(
                "(no terminals — press s for shell, a c for claude)",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ));
            // Pre-truncate so a narrow pane doesn't clip mid-word
            // without an `…` hint. ratatui's Paragraph clips
            // silently; the user-visible bug was the right pane
            // showing "c for" with no indication that "claude)"
            // was cut.
            frame.render_widget(
                Paragraph::new(crate::components::table::truncate_line(
                    line,
                    inner.width as usize,
                )),
                inner,
            );
            return;
        }

        // Branch on layout. Tabs = legacy single-pane render. Splits
        // = walk the tile tree, render each leaf at its rect with
        // dividers between.
        let body = Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: inner.height,
        };

        match self.layout.clone() {
            lazybox_core::SessionLayout::Tabs { .. } => {
                // Render the active tab full-pane (existing behavior).
                if let Some(id) = self.active_terminal_id() {
                    self.render_one_terminal(id, body, body, frame, focused);
                }
            }
            lazybox_core::SessionLayout::Splits {
                tree,
                focused: focus_path,
            } => {
                if self.zoomed {
                    // tmux-style zoom: the focused tile fills the pane; the
                    // rest of the grid is hidden until `]]z` restores it.
                    if let Some(id) = self.focused_terminal_id() {
                        self.render_tile_leaf(id, body, frame, focused, true);
                    }
                } else {
                    // Recursive tile renderer. Dividers are drawn on the
                    // boundary between adjacent leaves; the focused leaf
                    // gets a brighter border so the user can tell where
                    // typing lands.
                    let theme_chrome = theme.chrome;
                    self.render_tile_tree(
                        &tree,
                        body,
                        frame,
                        focused,
                        &focus_path,
                        &[],
                        theme_chrome,
                    );
                }
            }
        }
    }
}

impl TerminalStack {
    /// Whether an auto-spawned second-or-later terminal should promote
    /// the session into a side-by-side split (vs land as a new tab).
    /// `Split` (the default) always splits; `Tabs` only splits a
    /// session the user has *already* split by hand — a Tabs-mode
    /// session stays tabbed so the existing tile keeps its full size.
    fn auto_split_on_spawn(&self) -> bool {
        match self.terminal_new_layout {
            lazybox_config::NewTerminalLayout::Split => true,
            lazybox_config::NewTerminalLayout::Tabs => {
                matches!(self.layout, lazybox_core::SessionLayout::Splits { .. })
            }
        }
    }

    /// Commit a pending split: take the currently-focused leaf in
    /// the layout (or fabricate one if we're still in Tabs mode) and
    /// wrap it in a fresh `HSplit`/`VSplit` whose other side is the
    /// new terminal. After the mutation the user keeps focus on the
    /// new leaf so they can immediately type into the freshly-
    /// spawned shell.
    fn commit_pending_split(&mut self, new_id: TerminalId, direction: PendingSplit) {
        // Promote Tabs → Splits if needed. The Tabs mode's "focused"
        // leaf is the active terminal id.
        let mut tree = match self.layout.clone() {
            lazybox_core::SessionLayout::Splits { tree, .. } => tree,
            lazybox_core::SessionLayout::Tabs { .. } => {
                let Some(current_id) = self.active_terminal_id() else {
                    // No terminal at all yet — the new spawn is just
                    // the first tab. Stay in Tabs mode.
                    return;
                };
                lazybox_core::TileTree::Leaf {
                    terminal_id: current_id.0,
                }
            }
        };
        let focused_path = match &self.layout {
            lazybox_core::SessionLayout::Splits { focused, .. } => focused.clone(),
            lazybox_core::SessionLayout::Tabs { .. } => Vec::new(),
        };

        // Read the existing leaf at the focused path, build the new
        // split with [old, new] (so the new tile lands to the right
        // / below — matches tmux defaults), put it back at the path.
        let Some(existing) = subtree_at_path(&tree, &focused_path).cloned() else {
            return;
        };
        let new_leaf = lazybox_core::TileTree::Leaf {
            terminal_id: new_id.0,
        };
        let new_split = match direction {
            PendingSplit::Vertical => lazybox_core::TileTree::HSplit {
                left: Box::new(existing),
                right: Box::new(new_leaf),
                ratio: 50,
            },
            PendingSplit::Horizontal => lazybox_core::TileTree::VSplit {
                top: Box::new(existing),
                bottom: Box::new(new_leaf),
                ratio: 50,
            },
        };
        tree.replace_at(&focused_path, new_split);

        // New focus = the new leaf, which is the second child of the
        // split we just inserted at `focused_path`.
        let mut new_focus = focused_path;
        new_focus.push(1);
        self.layout = lazybox_core::SessionLayout::Splits {
            tree,
            focused: new_focus,
        };
    }

    /// Stage 1 of a split (`]]|` / `]]-`): arm the pending-split flag
    /// and emit a shell-spawn command. The new terminal id arrives on
    /// `Event::TerminalSpawned`; that's where we mutate the layout.
    /// No-op without an active session — the split has nowhere to land.
    pub fn split_tile(&mut self, direction: PendingSplit, cmds: &mut Vec<Command>) {
        let Some(session_key) = self.active_session.clone() else {
            return;
        };
        // A new split reveals the grid — a maximized tile would hide the
        // very tile being spawned.
        self.zoomed = false;
        self.pending_split = Some((direction, std::time::Instant::now()));
        cmds.push(Command::Spawn {
            model_alias: None,
            access: lazybox_ipc::AgentRunAccess::Default,
            session_key,
            session_id: None,
            client_request_id: None,
            kind: TerminalKind::Shell,
            cwd: None,
            initial_prompt: None,
            initial_snippet: None,
            // A tile-split shell lands in the workspace's default
            // (isolated) worktree, not the shared main checkout.
            on_main: false,
            force_new: false,
        });
    }

    /// Move focus across the tile tree (`]]<arrow>`), or cycle through
    /// tabs in Tabs mode. Persists the new layout via `SetSessionLayout`.
    pub fn move_tile_focus(&mut self, dir: lazybox_core::TileDirection, cmds: &mut Vec<Command>) {
        // Moving focus un-zooms so the grid is visible to navigate — the
        // tmux `select-pane` motion. Re-zoom the new tile with `]]z`.
        self.zoomed = false;
        // The tab strip shows the ACTIVE session's terminals, so the
        // cycle length is the visible set — the raw `terminals` map
        // also holds other sessions' slots and would overshoot.
        let visible_count = self.visible_terminals().len();
        match &mut self.layout {
            lazybox_core::SessionLayout::Tabs { active } => {
                // In tabs mode ←/→ cycle the tab strip; ↑/↓ are no-ops
                // since there's only one row of "tabs" stacked vertically.
                let n = visible_count;
                if n == 0 {
                    return;
                }
                match dir {
                    lazybox_core::TileDirection::Left => {
                        *active = if *active == 0 { n - 1 } else { *active - 1 };
                    }
                    lazybox_core::TileDirection::Right => {
                        *active = (*active + 1) % n;
                    }
                    _ => {}
                }
                self.active_tab_idx = *active;
            }
            lazybox_core::SessionLayout::Splits { tree, focused } => {
                if let Some(new_path) = tree.neighbor(focused, dir) {
                    *focused = new_path;
                }
            }
        }
        self.persist_layout(cmds);
    }

    /// Focus the tile containing `target` and persist the updated split
    /// layout. Returns `false` when the terminal is not visible.
    pub fn focus_tile(&mut self, target: TerminalId, cmds: &mut Vec<Command>) -> bool {
        let Some(layout_changed) = self.set_terminal_focus(target) else {
            return false;
        };
        if layout_changed {
            self.persist_layout(cmds);
        }
        true
    }

    /// Remove a terminal slot from the map and the tile tree,
    /// collapsing splits and re-clamping the tab strip. Shared by the
    /// exit teardown, the restart path (#356), and the
    /// spawn-supersedes-crashed-pane path.
    /// Whether an exited agent terminal exited cleanly enough to
    /// auto-close (as pre-#356 behavior did) rather than linger with a
    /// restart affordance. Clean = `code 0` AND the agent actually came
    /// to rest: it either engaged (reached a non-`Idle` state) or ran
    /// past the dead-on-arrival window. A non-zero code, death by signal
    /// (`None`), or a fast never-engaged exit is abnormal — kept open.
    /// Exit code alone is insufficient (an agent can exit `0` while
    /// failing to launch, #357), hence the runtime/engagement gate.
    fn agent_exit_is_clean(&self, terminal_id: &TerminalId, exit_code: Option<i32>) -> bool {
        if exit_code != Some(0) {
            return false;
        }
        self.terminals
            .get(terminal_id)
            .is_some_and(|slot| slot.did_work || slot.spawned_at.elapsed() >= self.dead_on_arrival)
    }

    fn drop_slot(&mut self, terminal_id: TerminalId) {
        // Removing a tile from the active grid reshuffles focus (the
        // collapse below re-points `focused` at the removed tile's
        // sibling) and can drop the tree back to Tabs — so a live zoom can
        // no longer trust which tile it maximizes. Drop back to the grid
        // rather than silently retargeting the zoom onto a sibling (#1057
        // review). A terminal in another session isn't part of this tree,
        // so its exit leaves the zoom intact.
        if self.zoomed
            && let lazybox_core::SessionLayout::Splits { tree, .. } = &self.layout
            && tree.path_to(terminal_id.0).is_some()
        {
            self.zoomed = false;
        }
        self.invalidate_visible();
        self.terminals.remove(&terminal_id);
        // Prune the tile tree so the removal surfaces visually: a
        // single-leaf split collapses to a Leaf root; an n-way split
        // loses just the dead branch. Tabs mode doesn't carry tile
        // state — no work to do there.
        if let lazybox_core::SessionLayout::Splits { tree, focused } = &mut self.layout {
            if let Some(path) = tree.path_to(terminal_id.0) {
                match tree.remove_at(&path) {
                    Ok(new_focus) => {
                        *focused = new_focus;
                    }
                    Err(_) => {
                        // path was empty (the removed leaf was the only
                        // tile) → drop back to the tabs default so a
                        // future spawn opens a fresh layout instead of
                        // leaving an orphan tree.
                        self.layout = lazybox_core::SessionLayout::Tabs { active: 0 };
                    }
                }
            }
            // If the post-collapse tree is just a Leaf, drop back to
            // Tabs — keeping a Splits-with-single-leaf payload renders
            // fine but means the next spawn promotes us right back into
            // Splits, which is confusing UX.
            if let lazybox_core::SessionLayout::Splits { tree, .. } = &self.layout
                && matches!(tree, lazybox_core::TileTree::Leaf { .. })
            {
                self.layout = lazybox_core::SessionLayout::Tabs { active: 0 };
            }
        }
        self.clamp_active_tab();
        self.auto_collapse_on_emptiness();
    }

    #[allow(clippy::too_many_arguments)]
    fn replace_terminal(
        &mut self,
        old_terminal_id: TerminalId,
        terminal_id: TerminalId,
        session_key: SessionKey,
        kind: TerminalKind,
        no_permission: bool,
        on_main: bool,
        model_label: Option<String>,
        authenticating: bool,
    ) {
        if !self.terminals.contains_key(&old_terminal_id)
            && let Some(slot) = self.terminals.get_mut(&terminal_id)
        {
            slot.session_key = session_key.clone();
            slot.kind = kind;
            slot.no_permission = no_permission;
            slot.on_main = on_main;
            if model_label.is_some() {
                slot.model_label = model_label;
            }
            slot.authenticating = authenticating;
            slot.auth_recovery_id = authenticating.then_some(old_terminal_id);
            // The slot's identity just changed under it — a cached frame
            // painted for the OLD identity (badges, kind) must never
            // blit again. Repainting from the live VT is always safe.
            slot.last_frame = None;
            slot.last_frame_rev = None;
            if !authenticating {
                // Non-auth slot reuse (#1254 finding 7): a duplicate or
                // late `TerminalReplaced` re-announcing an id this
                // client never matched to its old terminal. Nothing
                // guarantees the reused slot's grid or sequence
                // watermark describe the PTY now behind the id — a
                // watermark above the new stream's next seq silently
                // drops ALL of its output (`append_output`'s
                // `seq <= last_seq` dedupe), a permanently frozen pane.
                // Start from a clean grid, quarantined until the
                // daemon's authoritative replay describes the stream.
                // If the VT reset fails (allocator hiccup) the replay
                // path resets again before feeding, so recovery still
                // converges.
                //
                // An `authenticating` handoff is the one reuse that is
                // KNOWN current: the auth PTY's bytes stream on the
                // connection-private lane and legitimately precede this
                // broadcast (see `replacement_event_is_idempotent_…`),
                // and the provider-owned auth PTY has no replay ring to
                // serve a resync from — wiping it would blank the very
                // login screen the user must read.
                let _ = slot.vt.reset();
                slot.pending_feed.clear();
                slot.recent.clear();
                slot.osc52_carry.clear();
                slot.last_seq = 0;
                slot.deep_scrollback_requested = false;
                slot.resync_retry_at = None;
                slot.resync_retry_backoff = RESYNC_RETRY_INITIAL;
                slot.sync = TerminalStreamSync::Desynced {
                    request: TerminalResyncRequest {
                        terminal_id,
                        required_seq: 0,
                    },
                    request_pending: true,
                };
                if !self.pending_resync_requests.contains(&terminal_id) {
                    self.pending_resync_requests.push(terminal_id);
                }
            }
            self.invalidate_visible();
            if self.active_session.as_ref() == Some(&session_key) {
                self.focus_terminal(terminal_id);
            }
            self.collapsed = false;
            return;
        }
        let old_focus = self.focused_terminal_id() == Some(old_terminal_id);
        let old_recovery_id = self
            .terminals
            .get(&old_terminal_id)
            .and_then(|slot| slot.auth_recovery_id)
            .unwrap_or(old_terminal_id);
        self.invalidate_visible();
        let old = self.terminals.remove(&old_terminal_id);
        let (prompt_history, composing, inherited_model_label) = old.map_or_else(
            || (Vec::new(), String::new(), None),
            |slot| (slot.prompt_history, slot.composing, slot.model_label),
        );
        let mut slot = Self::make_slot(
            session_key.clone(),
            kind,
            0,
            no_permission,
            on_main,
            model_label.or(inherited_model_label),
            prompt_history,
            composing,
        );
        slot.authenticating = authenticating;
        slot.auth_recovery_id = authenticating.then_some(old_recovery_id);
        self.invalidate_visible();
        self.terminals.insert(terminal_id, slot);

        if let lazybox_core::SessionLayout::Splits { tree, focused } = &mut self.layout
            && let Some(path) = tree.path_to(old_terminal_id.0)
        {
            let _ = tree.replace_at(
                &path,
                lazybox_core::TileTree::Leaf {
                    terminal_id: terminal_id.0,
                },
            );
            if old_focus {
                *focused = path;
            }
        }
        for focused in self.last_focused.values_mut() {
            if *focused == old_terminal_id {
                *focused = terminal_id;
            }
        }
        self.closing.remove(&old_terminal_id);
        self.clamp_active_tab();
        if old_focus || self.active_session.as_ref() == Some(&session_key) {
            self.focus_terminal(terminal_id);
        }
        self.collapsed = false;
    }

    /// Resume the agent behind an exited pane while leaving the frozen pane
    /// in place until the daemon publishes its exact replacement.
    fn restart_exited(&mut self, terminal_id: TerminalId, cmds: &mut Vec<Command>) {
        let Some(slot) = self.terminals.get(&terminal_id) else {
            return;
        };
        if slot.exited.is_none() {
            return;
        }
        if let Some(recovery_terminal_id) = slot.auth_recovery_id {
            cmds.push(Command::ReauthenticateAgent {
                terminal_id: recovery_terminal_id,
                switch_account: true,
            });
        } else {
            cmds.push(Command::ResumeAgent { terminal_id });
        }
    }

    /// Close the focused terminal (`]]x`). In Splits, collapses the
    /// focused leaf's parent split into the surviving sibling; in Tabs,
    /// closes the active tab's terminal (the event flow prunes the slot
    /// and re-clamps the strip). Either way the terminal's PTY is
    /// killed daemon-side via `Command::Close`.
    pub fn close_focused_tile(&mut self, cmds: &mut Vec<Command>) {
        // Closing a tile collapses the tree — drop any zoom so the
        // surviving grid is what renders.
        self.zoomed = false;
        let lazybox_core::SessionLayout::Splits { tree, focused } = &mut self.layout else {
            if let Some(id) = self.active_terminal_id() {
                if self.terminals.get(&id).is_some_and(|s| s.exited.is_some()) {
                    if self
                        .terminals
                        .get(&id)
                        .is_some_and(|slot| slot.auth_recovery_id.is_some())
                    {
                        self.closing.insert(id);
                        cmds.push(Command::Close {
                            terminal_id: id,
                            client_request_id: None,
                        });
                    } else {
                        self.drop_slot(id);
                    }
                } else {
                    // Tag as a user close so the returning
                    // `TerminalExited` tears the pane down instead of
                    // keeping it as an exited agent pane (#356).
                    self.closing.insert(id);
                    cmds.push(Command::Close {
                        terminal_id: id,
                        client_request_id: None,
                    });
                }
            }
            return;
        };
        // Capture the terminal that's about to disappear before we
        // mutate the tree — we'll close its PTY too.
        let target_id = subtree_at_path(tree, focused).and_then(|n| match n {
            lazybox_core::TileTree::Leaf { terminal_id } => Some(*terminal_id),
            _ => None,
        });
        if tree.remove_at(focused).is_ok() {
            // After collapse, descend into a leaf so focus lands on
            // a real tile (not a now-stale split path).
            let leaves = tree.leaves();
            if let Some(first) = leaves.first()
                && let Some(p) = tree.path_to(*first)
            {
                *focused = p;
            } else {
                *focused = Vec::new();
            }
            // If the close left us with a single leaf, downgrade to
            // Tabs so the rest of the UI (tab strip, focus models)
            // doesn't see a degenerate splits tree.
            if leaves.len() <= 1 {
                self.layout = lazybox_core::SessionLayout::Tabs { active: 0 };
                self.active_tab_idx = 0;
            }
            if let Some(id) = target_id {
                let tid = TerminalId(id);
                if self.terminals.get(&tid).is_some_and(|s| s.exited.is_some()) {
                    if self
                        .terminals
                        .get(&tid)
                        .is_some_and(|slot| slot.auth_recovery_id.is_some())
                    {
                        self.closing.insert(tid);
                        cmds.push(Command::Close {
                            terminal_id: tid,
                            client_request_id: None,
                        });
                    } else {
                        self.invalidate_visible();
                        self.terminals.remove(&tid);
                    }
                } else {
                    self.closing.insert(tid);
                    cmds.push(Command::Close {
                        terminal_id: tid,
                        client_request_id: None,
                    });
                }
            }
            self.persist_layout(cmds);
        }
    }

    /// Push a `Command::SetSessionLayout` for the currently-active
    /// session if we know which one we're on. The daemon writes the
    /// new layout to the workspace record + rebroadcasts.
    fn persist_layout(&self, cmds: &mut Vec<Command>) {
        let Some(session_key) = &self.active_session else {
            return;
        };
        let Ok(layout_json) = serde_json::to_string(&self.layout) else {
            return;
        };
        // Find the session id we belong to. With one session per
        // workspace today, the active_session string IS the workspace
        // key; the user picks the first session by default. A future
        // multi-session sidebar would override this with an explicit
        // session id from selected_session_id. For now, leave the id
        // empty and the daemon's handler tolerates it (no-op).
        cmds.push(Command::SetSessionLayout {
            session_key: session_key.clone(),
            session_id_raw: String::new(),
            layout_json,
        });
    }

    /// Render the `you ▸ <recap>` pin into `area`. Dim styling so
    /// the line reads as chrome / recap, not as fresh agent output
    /// the user has to parse. Truncates with `…` when the message
    /// overflows the row width — same affordance the empty-state
    /// hint uses elsewhere in this pane.
    fn render_user_message_recap(frame: &mut Frame, area: Rect, msg: &str, age: &str) {
        let theme = crate::theme::current();
        let summary = summarize_message(msg);
        // A relative age ("5m ago") on the right lets the user judge whether
        // the conversation is stale at a glance, without opening `]]h` (#523).
        // It's supplementary, so it yields to the message: reserve space only
        // when the row is wide enough for both, and drop it otherwise.
        let age_w = age.chars().count() as u16;
        let age_reserve = if age.is_empty() || area.width < age_w + 12 {
            0
        } else {
            age_w + 2 // one-column gap before the age, one of breathing room
        };
        let summary_area = Rect {
            width: area.width.saturating_sub(age_reserve),
            ..area
        };
        let line = ratatui::text::Line::from(vec![
            Span::styled(
                RECAP_PREFIX,
                Style::default()
                    .fg(theme.text_dim)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(summary, Style::default().fg(theme.text_dim)),
        ]);
        let line = crate::components::table::truncate_line(line, summary_area.width as usize);
        frame.render_widget(Paragraph::new(line), summary_area);
        if age_reserve > 0 {
            let age_area = Rect {
                x: area.x + area.width - age_w,
                width: age_w,
                ..area
            };
            frame.render_widget(
                Paragraph::new(ratatui::text::Line::from(Span::styled(
                    age.to_string(),
                    Style::default().fg(theme.text_dim),
                ))),
                age_area,
            );
        }
    }

    /// Rows carved off the top of a terminal's body for the pinned
    /// `you ▸ <recap>` line plus a blank spacer below it: 2 for an
    /// agent terminal with a remembered last user message, 0 for
    /// everything else. `body_height` is the height of the grid area
    /// (the rect handed to [`Self::render_one_terminal`], already inside the
    /// tab strip + divider) — the recap is refused below 3 rows so a
    /// tiny split keeps every cell for the agent grid. This is the one
    /// source of truth for the offset: the render path and the
    /// selection/click coordinate mappers all read it so they map the
    /// same rows.
    fn recap_rows(slot: &TerminalSlot, body_height: u16) -> u16 {
        let show_recap = matches!(slot.kind, TerminalKind::Agent(_))
            && !slot.prompt_history.is_empty()
            && body_height >= 3;
        if show_recap { 2 } else { 0 }
    }

    /// Render a single terminal slot full-rect. Used by both the
    /// tabs path and the splits path's leaf case.
    fn render_one_terminal(
        &mut self,
        id: TerminalId,
        rect: Rect,
        tile: Rect,
        frame: &mut Frame,
        focused: bool,
    ) {
        let _ = focused; // ghostty-vt doesn't render focus chrome itself
        if let Some(slot) = self.terminals.get_mut(&id) {
            // Coming on screen: mark displayed so subsequent chunks feed
            // eagerly (this slot stays current as long as it's visible).
            // The buffered bytes themselves are NOT fed yet — that waits
            // until the VT has been sized to this frame's grid, below.
            slot.displayed = true;
            // Carve off the recap rows (see `recap_rows`): the recap
            // sits on row 0, row 1 stays blank so the agent output
            // doesn't visually run into it, and the body starts at row
            // 2.
            let recap = Self::recap_rows(slot, rect.height);
            let body = if recap > 0 {
                Rect {
                    x: rect.x,
                    y: rect.y + recap,
                    width: rect.width,
                    height: rect.height - recap,
                }
            } else {
                rect
            };
            if recap > 0
                && let Some(last) = slot.prompt_history.last()
            {
                let header_rect = Rect {
                    x: rect.x,
                    y: rect.y,
                    width: rect.width,
                    height: 1,
                };
                let age = crate::realm::model::relative_age(last.timestamp_ms, now_ms());
                Self::render_user_message_recap(frame, header_rect, &last.text, &age);
            }
            // Reserve the rightmost column of the body as a scrollbar
            // gutter. Held back unconditionally so the PTY width stays
            // stable — sizing it only when scrollback exists would make
            // the column flip in and out as output scrolled into
            // history, forcing a resize storm. The gutter stays blank
            // until there's something to scroll (the indicator
            // auto-hides), so an idle terminal just loses one column.
            let (grid, gutter) = if body.width > 1 {
                (
                    Rect {
                        width: body.width - 1,
                        ..body
                    },
                    Some(Rect {
                        x: body.x + body.width - 1,
                        y: body.y,
                        width: 1,
                        height: body.height,
                    }),
                )
            } else {
                (body, None)
            };
            self.tile_hits.push(TerminalHit {
                terminal_id: id,
                tile,
                body: rect,
                grid,
                offset: None,
            });
            slot.vt.ensure_size(grid.width, grid.height);
            // Only now drain whatever arrived while this slot was hidden.
            // Feeding before the resize would parse those bytes at the
            // VT's default width and then reflow them to the real one —
            // and reflow is not a faithful substitute for having wrapped
            // at the right width to begin with. A reattaching client
            // (whose entire replay arrives buffered) would land on a
            // different grid than a live one fed the same bytes. See
            // `fresh_and_reattach_reach_identical_scroll_state`.
            slot.flush_pending();
            // Record the viewport offset of the frame we're painting AFTER
            // the flush — the widget below renders from this same post-flush
            // state, so this is the offset the selection mapping must reuse
            // (see `TerminalHit::offset`).
            let frame_offset = slot.vt.terminal.scrollbar().ok().map(|b| b.offset);
            if let Some(hit) = self.tile_hits.last_mut() {
                hit.offset = frame_offset;
            }
            // Backend PTY also needs to know the new size — otherwise
            // the shell process keeps writing at its spawn dimensions
            // and the bottom rows go blank as soon as the user scrolls
            // past them. Queue a resize for the App to ship.
            let new_size = (grid.width, grid.height);
            if grid.width > 0 && grid.height > 0 && slot.last_rendered_size != Some(new_size) {
                slot.last_rendered_size = Some(new_size);
                self.pending_resizes.push((id, grid.width, grid.height));
            }
            // Content-revision gate (2026-08-19 audit, U1). The full
            // widget render is a per-cell FFI walk — ~5 round-trips ×
            // every viewport cell, ~50k calls for a full-window tile,
            // every frame. When the VT saw NO mutation since the last
            // paint at this exact area (revision + rect match), the
            // grid cannot differ, so blit the cached composed frame
            // (cursor overlay included) instead. Unlike libghostty's
            // dirty flags (#239) this is sound: the revision is a log
            // of the VT's *inputs*, not its self-reported dirtiness.
            // A frozen crashed pane (M3) always blits its freeze-frame
            // — its VT was dropped to reclaim scrollback memory.
            let rev_key = (slot.vt.content_rev, grid);
            let frozen = slot.exited.is_some() && slot.last_frame.is_some();
            let unchanged = slot.last_frame_rev == Some(rev_key) && slot.last_frame.is_some();
            if frozen || unchanged {
                if let Some(cached) = &slot.last_frame {
                    blit_cached_frame(frame.buffer_mut(), cached, grid);
                }
            } else if let Ok(snapshot) = slot.vt.render_state.update(&slot.vt.terminal) {
                let widget = GhosttyTerminal::new(
                    &snapshot,
                    &mut slot.vt.row_iter,
                    &mut slot.vt.cell_iter,
                    &mut slot.vt.shadow,
                    &mut slot.vt.last_visible_cursor,
                );
                frame.render_widget(widget, grid);
                slot.last_frame = Some(copy_frame_region(frame.buffer_mut(), grid));
                slot.last_frame_rev = Some(rev_key);
            }
            if let Some(gutter) = gutter
                && let Ok(bar) = slot.vt.terminal.scrollbar()
            {
                crate::components::scrollbar::render_vertical(
                    frame,
                    gutter,
                    bar.total as usize,
                    bar.len as usize,
                    bar.offset as usize,
                );
            }
            // An exited agent pane overlays a restart banner on its last
            // row, leaving the frozen screen visible above it (#356).
            if let Some(exit) = &slot.exited {
                Self::render_exit_banner(frame, grid, exit);
            }
        }
    }

    /// Paint the "agent exited — restart?" banner across the bottom row
    /// of an exited pane's grid (#356). The frozen last screen stays
    /// visible above it; this row is a filled bar so it reads as an
    /// alert over whatever output the crash left behind.
    ///
    /// See also `blit_cached_frame` / `copy_frame_region` (U1): the
    /// frozen screen itself now comes from the cached composed frame,
    /// not a live VT.
    ///
    /// A dead-on-arrival exit (#368) — the agent gone within seconds of
    /// spawn — reads as "failed to start" rather than a plain "exited",
    /// so an immediate `code 0` isn't mistaken for success, and the
    /// captured tail of its output is painted just above the banner so
    /// the pane shows *why* instead of a blank black screen.
    fn render_exit_banner(frame: &mut Frame, grid: Rect, exit: &TerminalExit) {
        if grid.width == 0 || grid.height == 0 {
            return;
        }
        let theme = crate::theme::current();
        let status = match exit.code {
            Some(code) => format!("code {code}"),
            None => "killed".to_string(),
        };
        let verb = if exit.dead_on_arrival {
            "failed to start"
        } else {
            "exited"
        };
        let text = format!("⚠ agent {verb} ({status}) — r restart · ]]x close");
        let width = grid.width as usize;
        // Pad (or truncate) to the full row so the fill spans it.
        let display: String = if text.chars().count() > width {
            text.chars().take(width).collect()
        } else {
            let pad = width - text.chars().count();
            format!("{text}{}", " ".repeat(pad))
        };
        // A dead-on-arrival pane is blank (the agent produced no lasting
        // screen), so paint the captured output tail in the rows directly
        // above the banner — the error/last lines the agent printed
        // before dying, bottom-aligned so they read as one block with the
        // banner. Skipped for a normal exit, whose frozen screen already
        // shows its real final state.
        if exit.dead_on_arrival
            && let Some(last) = &exit.last_output
        {
            let avail = grid.height.saturating_sub(1);
            // Most-recent `avail` lines, back into chronological order.
            let mut tail: Vec<&str> = last.lines().rev().take(avail as usize).collect();
            tail.reverse();
            let base_y = grid.y + avail - tail.len() as u16;
            for (i, line) in tail.iter().enumerate() {
                let shown: String = line.chars().take(width).collect();
                let row = Rect {
                    x: grid.x,
                    y: base_y + i as u16,
                    width: grid.width,
                    height: 1,
                };
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        shown,
                        Style::default().fg(theme.text_dim),
                    ))),
                    row,
                );
            }
        }
        let row = Rect {
            x: grid.x,
            y: grid.y + grid.height - 1,
            width: grid.width,
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                display,
                Style::default()
                    .bg(theme.fill)
                    .fg(theme.error)
                    .add_modifier(Modifier::BOLD),
            ))),
            row,
        );
    }

    /// Render one tile: a one-row status header carved off the top, then
    /// the PTY grid in the remainder. Shared by the grid renderer and the
    /// zoomed view so both show the same header. `zoomed` adds a `⛶`
    /// marker so a maximized tile is distinguishable from a lone terminal.
    ///
    /// The header row is CARVED off the tile's rect (the PTY is sized to
    /// the remainder), never painted over content — overdrawing hid the
    /// tile's top grid row and the agent recap. A one-row tile keeps its
    /// content instead.
    fn render_tile_leaf(
        &mut self,
        terminal_id: TerminalId,
        rect: Rect,
        frame: &mut Frame,
        is_focused_leaf: bool,
        zoomed: bool,
    ) {
        let body = if rect.height >= 2 && rect.width > 0 {
            let bar = Rect {
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: 1,
            };
            let header = self.tile_header_line(terminal_id, is_focused_leaf, zoomed, bar.width);
            frame.render_widget(Paragraph::new(header), bar);
            Rect {
                x: rect.x,
                y: rect.y + 1,
                width: rect.width,
                height: rect.height - 1,
            }
        } else {
            rect
        };
        self.render_one_terminal(terminal_id, body, rect, frame, is_focused_leaf);
    }

    /// The one-row tile header for the Splits grid (#1057). Reads as a
    /// divider rule that also carries the runner, its agent-state chip,
    /// and the model badge — so a grid of agents tells you at a glance
    /// which is which and which one needs you.
    ///
    /// Focus colouring mirrors the old bare rule: accent on the focused
    /// tile, chrome on the rest (#286) — the contrast is what makes "where
    /// does my typing land" legible. On top of that, a *background* tile
    /// whose agent is asking for input paints its whole bar warn+bold, so
    /// it stands out in the grid even while you type in another tile.
    fn tile_header_line(
        &self,
        id: TerminalId,
        is_focused_leaf: bool,
        zoomed: bool,
        width: u16,
    ) -> Line<'static> {
        let theme = crate::theme::current();
        let width = width as usize;
        let Some(slot) = self.terminals.get(&id) else {
            return Line::from(Span::styled(
                "─".repeat(width),
                Style::default().fg(theme.chrome),
            ));
        };

        let exited = slot.exited.is_some();
        // Both states park the agent waiting on the user (a permission
        // prompt, or a provider rate-limit block — #847), so both pull
        // attention.
        let needs_you = !exited
            && matches!(
                slot.agent_state,
                lazybox_ipc::AgentState::InputNeeded
                    | lazybox_ipc::AgentState::LimitReached
                    | lazybox_ipc::AgentState::CreditExhausted
            );
        // Attention: a background tile whose agent needs you paints its
        // whole bar warn so it's noticeable without watching every tile.
        let attention = needs_you && !is_focused_leaf;
        let base = if attention {
            theme.warn
        } else if is_focused_leaf {
            theme.accent
        } else {
            theme.chrome
        };
        let base_style = if attention || is_focused_leaf {
            Style::default().fg(base).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(base)
        };

        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut used = 0usize;

        let icon = match &slot.kind {
            TerminalKind::Agent(agent_id) => crate::components::icons::agent_icon(agent_id),
            _ => crate::components::icons::SHELL,
        };
        let lead = format!("─ {icon} {} ", Self::tab_label(&slot.kind));
        used += lead.chars().count();
        spans.push(Span::styled(lead, base_style));

        if let Some((chip, style)) = Self::agent_state_badge(slot.agent_state, exited, true, theme)
        {
            let chip = format!("{chip} ");
            used += chip.chars().count();
            spans.push(Span::styled(chip, style));
        }

        if let Some(tier) = &slot.model_label {
            let badge = format!("◆ {tier} ");
            used += badge.chars().count();
            spans.push(Span::styled(
                badge,
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ));
        }

        if slot.on_main {
            let main = "⎇ main ".to_string();
            used += main.chars().count();
            spans.push(Span::styled(
                main,
                Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
            ));
        }

        if zoomed {
            let zoom = "⛶ zoom ".to_string();
            used += zoom.chars().count();
            spans.push(Span::styled(zoom, base_style));
        }

        // Fill the remainder with the rule so the header keeps reading as
        // a divider between tiles.
        if used < width {
            spans.push(Span::styled(
                "─".repeat(width - used),
                Style::default().fg(base),
            ));
        }

        Line::from(spans)
    }

    /// The agent-state badge (`label`, `style`) shared by the tab strip
    /// and the tile-grid headers: which agent needs you at a glance.
    /// `exited` overrides the live state with the process-ended pill.
    /// `compact` selects the tile grid's terse `● asking` glyph over the
    /// tab strip's wordier `! needs input`; every other label and colour
    /// is identical across both surfaces, so they can't drift.
    ///
    /// The `match` is deliberately exhaustive — no `_` wildcard — so a new
    /// [`lazybox_ipc::AgentState`] variant is a compile error here rather
    /// than silently rendering blank on both surfaces (the failure mode
    /// the previous per-surface `_ =>` arms had). The label carries no
    /// surrounding whitespace; each caller adds its own spacing.
    fn agent_state_badge(
        state: lazybox_ipc::AgentState,
        exited: bool,
        compact: bool,
        theme: &crate::theme::Theme,
    ) -> Option<(&'static str, Style)> {
        use lazybox_ipc::AgentState;
        if exited {
            return Some((
                "✗ exited",
                Style::default()
                    .fg(theme.error)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        match state {
            AgentState::InputNeeded => Some((
                if compact {
                    "● asking"
                } else {
                    "! needs input"
                },
                Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
            )),
            AgentState::Working => Some(("· working", Style::default().fg(theme.accent))),
            AgentState::Done => Some((
                "✓ done",
                Style::default()
                    .fg(theme.success)
                    .add_modifier(Modifier::BOLD),
            )),
            // A provider usage / rate-limit block (#847) — the agent is
            // parked waiting on the user just like `InputNeeded`, so it
            // gets the same attention treatment, with the `⏳` glyph the
            // sidebar pill already uses for it.
            AgentState::LimitReached => Some((
                "⏳ limited",
                Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
            )),
            AgentState::CreditExhausted => Some((
                "¢ no credit",
                Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
            )),
            // The calm sibling of `LimitReached`: auto-wait pressed Wait and
            // the agent is parked until reset — handled, nothing for you to
            // do — so it gets a quiet 💤 in the dim text color, NOT the
            // alerting bold `warn` the two blocks above use.
            AgentState::AwaitingReset => Some(("💤 waiting", Style::default().fg(theme.text_dim))),
            // Idle has nothing to act on; `Exited` is surfaced by the
            // `exited` flag above (the process-ended pill lives on the
            // slot, not the live state).
            AgentState::Idle | AgentState::Exited { .. } => None,
        }
    }

    /// Recursive walk of the tile tree. Each Leaf gets its own rect
    /// rendered via the existing per-terminal pipeline; each Split
    /// divides its rect according to `ratio` and recurses, drawing a
    /// thin divider line between the two children.
    #[allow(clippy::too_many_arguments)]
    fn render_tile_tree(
        &mut self,
        node: &lazybox_core::TileTree,
        rect: Rect,
        frame: &mut Frame,
        pane_focused: bool,
        focus_path: &[u8],
        current_path: &[u8],
        chrome: Color,
    ) {
        match node {
            lazybox_core::TileTree::Leaf { terminal_id } => {
                let is_focused_leaf = pane_focused && current_path == focus_path;
                self.render_tile_leaf(
                    TerminalId(*terminal_id),
                    rect,
                    frame,
                    is_focused_leaf,
                    false,
                );
            }
            lazybox_core::TileTree::HSplit { left, right, ratio } => {
                let split_at = (rect.width as u32 * (*ratio).min(100) as u32 / 100) as u16;
                let left_w = split_at.min(rect.width.saturating_sub(1));
                let right_x = rect.x + left_w + 1;
                let right_w = rect.width.saturating_sub(left_w + 1);
                let left_rect = Rect {
                    x: rect.x,
                    y: rect.y,
                    width: left_w,
                    height: rect.height,
                };
                let right_rect = Rect {
                    x: right_x,
                    y: rect.y,
                    width: right_w,
                    height: rect.height,
                };
                let mut p_left = current_path.to_vec();
                p_left.push(0);
                let mut p_right = current_path.to_vec();
                p_right.push(1);
                self.render_tile_tree(
                    left,
                    left_rect,
                    frame,
                    pane_focused,
                    focus_path,
                    &p_left,
                    chrome,
                );
                self.render_tile_tree(
                    right,
                    right_rect,
                    frame,
                    pane_focused,
                    focus_path,
                    &p_right,
                    chrome,
                );
                // Vertical divider between the two halves.
                if rect.height > 0 {
                    let div = Rect {
                        x: rect.x + left_w,
                        y: rect.y,
                        width: 1,
                        height: rect.height,
                    };
                    let lines: Vec<Line> = (0..rect.height)
                        .map(|_| Line::from(Span::styled("│", Style::default().fg(chrome))))
                        .collect();
                    frame.render_widget(Paragraph::new(lines), div);
                }
            }
            lazybox_core::TileTree::VSplit { top, bottom, ratio } => {
                let split_at = (rect.height as u32 * (*ratio).min(100) as u32 / 100) as u16;
                let top_h = split_at.min(rect.height.saturating_sub(1));
                let bottom_y = rect.y + top_h + 1;
                let bottom_h = rect.height.saturating_sub(top_h + 1);
                let top_rect = Rect {
                    x: rect.x,
                    y: rect.y,
                    width: rect.width,
                    height: top_h,
                };
                let bottom_rect = Rect {
                    x: rect.x,
                    y: bottom_y,
                    width: rect.width,
                    height: bottom_h,
                };
                let mut p_top = current_path.to_vec();
                p_top.push(0);
                let mut p_bot = current_path.to_vec();
                p_bot.push(1);
                self.render_tile_tree(
                    top,
                    top_rect,
                    frame,
                    pane_focused,
                    focus_path,
                    &p_top,
                    chrome,
                );
                self.render_tile_tree(
                    bottom,
                    bottom_rect,
                    frame,
                    pane_focused,
                    focus_path,
                    &p_bot,
                    chrome,
                );
                // Horizontal divider.
                if rect.width > 0 {
                    let div = Rect {
                        x: rect.x,
                        y: rect.y + top_h,
                        width: rect.width,
                        height: 1,
                    };
                    frame.render_widget(
                        Paragraph::new(Line::from(Span::styled(
                            "─".repeat(div.width as usize),
                            Style::default().fg(chrome),
                        ))),
                        div,
                    );
                }
            }
        }
    }
}

// ── ANSI strip helper ──────────────────────────────────────────────────

/// Strip ANSI escape sequences from a byte buffer, returning plain
/// text bytes. Handles CSI (`ESC [ ... <final>`), OSC (`ESC ] ... BEL`
/// or `ESC \`), and generic ESC-char sequences. Leaves printable bytes
/// (including UTF-8 multi-byte) alone.
///
/// This is a pragmatic MVP parser — it's not a full VT spec compliance
/// layer. Real terminal rendering via libghostty-vt replaces this
/// entirely in task #78.
pub fn strip_ansi(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == 0x1b {
            // ESC introducer.
            if i + 1 >= input.len() {
                break;
            }
            match input[i + 1] {
                b'[' => {
                    // CSI: skip through the final byte (0x40..=0x7E).
                    i += 2;
                    while i < input.len() {
                        let c = input[i];
                        if (0x40..=0x7e).contains(&c) {
                            i += 1;
                            break;
                        }
                        i += 1;
                    }
                }
                b']' => {
                    // OSC: terminates on BEL (0x07) or ST (ESC \).
                    i += 2;
                    while i < input.len() {
                        if input[i] == 0x07 {
                            i += 1;
                            break;
                        }
                        if input[i] == 0x1b && i + 1 < input.len() && input[i + 1] == b'\\' {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                }
                _ => {
                    // Two-byte ESC sequences (ESC c, ESC (B, etc.).
                    i += 2;
                }
            }
            continue;
        }
        // Drop the single BEL byte — it's not printable.
        if input[i] == 0x07 {
            i += 1;
            continue;
        }
        out.push(input[i]);
        i += 1;
    }
    out
}

// ── Key → PTY bytes ────────────────────────────────────────────────────

/// Encode a key event as the bytes we'd write to a PTY. Returns None
/// for keys we don't know how to encode yet. Public so the app-level
/// escape-latch can flush buffered keystrokes through the same
/// encoding path the live key dispatch uses.
pub fn key_to_bytes(key: &KeyEvent) -> Option<Vec<u8>> {
    use KeyCode::*;
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        Char(c) => {
            // Build ESC-prefixed (Meta) + optionally control-folded byte.
            // Previously Alt was dropped entirely, so `Alt-b` (word-back
            // in readline) reached the agent as a bare `b`.
            let mut bytes = Vec::with_capacity(if alt { 2 } else { 1 });
            if alt {
                bytes.push(0x1b);
            }
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                // Ctrl-<letter>: low control byte.
                bytes.push((c as u8) & 0x1f);
            } else {
                let mut buf = [0u8; 4];
                bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
            Some(bytes)
        }
        Enter => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                // Shift-Enter → ESC + CR. Claude Code accepts this as
                // "newline in prompt without submit".
                Some(vec![0x1b, b'\r'])
            } else {
                Some(vec![b'\r'])
            }
        }
        Backspace => Some(if alt { vec![0x1b, 0x7f] } else { vec![0x7f] }),
        Esc => Some(vec![0x1b]),
        Tab => Some(vec![b'\t']),
        BackTab => Some(b"\x1b[Z".to_vec()),
        // Cursor keys carry their xterm modifier encoding so Ctrl/Shift/
        // Alt + arrow (word-motion, selection) reach the inner program
        // instead of arriving unmodified. Bare keys keep the short form.
        Up => Some(cursor_seq(b'A', key.modifiers)),
        Down => Some(cursor_seq(b'B', key.modifiers)),
        Right => Some(cursor_seq(b'C', key.modifiers)),
        Left => Some(cursor_seq(b'D', key.modifiers)),
        Home => Some(cursor_seq(b'H', key.modifiers)),
        End => Some(cursor_seq(b'F', key.modifiers)),
        Insert => Some(b"\x1b[2~".to_vec()),
        Delete => Some(b"\x1b[3~".to_vec()),
        // Unmodified PageUp/PageDown reach the inner program (less, vim);
        // the Shift-modified variants are intercepted earlier for local
        // scrollback and never get here.
        PageUp => Some(b"\x1b[5~".to_vec()),
        PageDown => Some(b"\x1b[6~".to_vec()),
        F(n) => function_key_seq(n),
        _ => None,
    }
}

/// xterm modifier parameter: `1 + bitmask(shift=1, alt=2, ctrl=4)`.
/// Returns 1 (no modifier) when none are held.
fn xterm_modifier_param(mods: KeyModifiers) -> u8 {
    let mut m = 0u8;
    if mods.contains(KeyModifiers::SHIFT) {
        m |= 1;
    }
    if mods.contains(KeyModifiers::ALT) {
        m |= 2;
    }
    if mods.contains(KeyModifiers::CONTROL) {
        m |= 4;
    }
    m + 1
}

/// Encode a cursor / Home / End key (`final_byte` is `A`/`B`/`C`/`D`/
/// `H`/`F`). Unmodified → `ESC [ <final>`; modified → the xterm
/// `ESC [ 1 ; <mod> <final>` form.
fn cursor_seq(final_byte: u8, mods: KeyModifiers) -> Vec<u8> {
    let m = xterm_modifier_param(mods);
    if m == 1 {
        vec![0x1b, b'[', final_byte]
    } else {
        format!("\x1b[1;{m}{}", final_byte as char).into_bytes()
    }
}

/// Encode F1–F12 to the conventional xterm sequences. F1–F4 use the
/// `ESC O <P..S>` SS3 form; F5+ use `ESC [ <n> ~`.
fn function_key_seq(n: u8) -> Option<Vec<u8>> {
    let seq: &[u8] = match n {
        1 => b"\x1bOP",
        2 => b"\x1bOQ",
        3 => b"\x1bOR",
        4 => b"\x1bOS",
        5 => b"\x1b[15~",
        6 => b"\x1b[17~",
        7 => b"\x1b[18~",
        8 => b"\x1b[19~",
        9 => b"\x1b[20~",
        10 => b"\x1b[21~",
        11 => b"\x1b[23~",
        12 => b"\x1b[24~",
        _ => return None,
    };
    Some(seq.to_vec())
}

/// True when a row is nothing but box-drawing characters and
/// whitespace — a composer/panel border or separator (`╭──╮`, `├──┤`,
/// `────`), never agent prose or code. Used to strip TUI chrome from a
/// handoff scrape (#431). A row with any non-box glyph (so any framed
/// content like `│ text │`) is kept. An empty row is not a border.
fn is_border_row(row: &str) -> bool {
    let trimmed = row.trim();
    !trimmed.is_empty()
        && trimmed
            .chars()
            .all(|c| c.is_whitespace() || ('\u{2500}'..='\u{257F}').contains(&c))
}

/// Build a row's plain text plus a per-column byte-offset map from a
/// render-state row, applying the wide-glyph spacer handling both the
/// click resolver and the URL scan depend on. `starts[col]` is the byte
/// offset in the returned string where the glyph at screen column `col`
/// begins; a blank spacer cell of a wide glyph carries no text but still
/// occupies a column, so it maps back to the wide base (the last
/// recorded start). Covers the post-glyph `SpacerTail` and the soft-wrap
/// `SpacerHead`.
fn row_text_and_starts(
    cell_iter: &mut vt::render::CellIterator<'static>,
    row: &vt::render::RowIteration<'static, '_>,
) -> (String, Vec<usize>) {
    let mut text = String::new();
    let mut starts: Vec<usize> = Vec::new();
    if let Ok(mut cell_iter) = cell_iter.update(row) {
        while let Some(cell) = cell_iter.next() {
            if matches!(
                cell.wide(),
                Ok(vt::screen::CellWide::SpacerTail | vt::screen::CellWide::SpacerHead)
            ) {
                starts.push(starts.last().copied().unwrap_or(0));
                continue;
            }
            starts.push(text.len());
            let graphemes = cell.graphemes().unwrap_or_default();
            if graphemes.is_empty() {
                text.push(' ');
            } else {
                for g in graphemes {
                    text.push(g);
                }
            }
        }
    }
    (text, starts)
}

/// The byte spans of every `http(s)://…` token in `text`, in order. A
/// URL terminates at the first whitespace; trailing punctuation that's
/// almost never part of the URL (`.,;:!?` plus the closing brackets and
/// quotes) is trimmed so `see https://example.com.` yields
/// `https://example.com`. Trailing box-drawing glyphs (`│`, `╮`, …) are
/// trimmed too, so a URL butting against an agent's composer border with
/// no separating space (`https://x│`) doesn't carry the frame into the
/// opened link. Empty matches (nothing left after trimming) are skipped.
fn url_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut search_start = 0;
    while search_start < text.len() {
        let rest = &text[search_start..];
        let http = rest.find("http://");
        let https = rest.find("https://");
        let off = match (http, https) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (None, None) => break,
        };
        let url_start = search_start + off;
        let after_scheme = &text[url_start..];
        let raw_end_off = after_scheme
            .char_indices()
            .find(|(_, c)| c.is_whitespace())
            .map(|(i, _)| i)
            .unwrap_or(after_scheme.len());
        let mut url_end = url_start + raw_end_off;
        // Trim trailing punctuation that's almost never part of a
        // URL. Stop once we hit something URL-valid.
        loop {
            let slice = &text[url_start..url_end];
            let Some(last) = slice.chars().next_back() else {
                break;
            };
            if matches!(
                last,
                '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '\'' | '"'
            ) || ('\u{2500}'..='\u{257F}').contains(&last)
            {
                url_end -= last.len_utf8();
            } else {
                break;
            }
        }
        if url_end > url_start {
            spans.push((url_start, url_end));
        }
        // Advance past this match (or the leading whitespace if the whole
        // token trimmed away) to look for the next.
        search_start = url_end.max(url_start + 1);
    }
    spans
}

/// Scan `row_text` for an `http(s)://…` token whose byte range contains
/// `byte_pos`. Returns the trimmed URL as a borrowed slice when found.
pub(crate) fn find_url_at_byte(row_text: &str, byte_pos: usize) -> Option<&str> {
    url_spans(row_text)
        .into_iter()
        .find(|&(start, end)| byte_pos >= start && byte_pos < end)
        .map(|(start, end)| &row_text[start..end])
}

/// Every `http(s)://…` URL in `text`, in order, each trimmed the same
/// way [`find_url_at_byte`] trims a single hit. Shared with the `]]u`
/// URL picker so the picker and right-click agree on URL boundaries.
fn scan_urls(text: &str) -> Vec<&str> {
    url_spans(text)
        .into_iter()
        .map(|(start, end)| &text[start..end])
        .collect()
}

/// Read the OSC 8 hyperlink URI attached to the viewport cell at
/// `(col, row)`, if any. `libghostty-vt` parses OSC 8 hyperlinks but
/// the render-state cell iterator used to build `row_text` doesn't
/// expose them, so we resolve the cell through the grid-ref API (fast
/// for viewport coordinates) and read its URI directly. Returns
/// `None` for a cell with no hyperlink or on any FFI error.
fn hyperlink_uri_at(
    terminal: &vt::Terminal<'static, 'static>,
    col: u16,
    row: u16,
) -> Option<String> {
    let point = vt::terminal::Point::Viewport(vt::terminal::PointCoordinate {
        x: col,
        y: row as u32,
    });
    let grid_ref = terminal.grid_ref(point).ok()?;
    let mut buf = vec![0u8; 256];
    loop {
        match grid_ref.hyperlink_uri(&mut buf) {
            Ok(0) => return None,
            Ok(n) => return String::from_utf8(buf[..n].to_vec()).ok(),
            Err(vt::error::Error::OutOfSpace { required }) if required > buf.len() => {
                buf.resize(required, 0);
            }
            Err(_) => return None,
        }
    }
}

/// Classify the click at `byte_pos` in `row_text`. An OSC 8
/// hyperlink attached to the clicked cell (`hyperlink`) is
/// authoritative — the program told us exactly where this text
/// points, so it wins over any heuristic scan of the visible glyphs,
/// which are often a title rather than the literal URL. Otherwise we
/// fall back to scanning the row text, trying detectors in
/// specificity order: a URL (begins with a scheme) wins over an
/// issue reference, which wins over a bare file path. Returns `None`
/// when the click landed on whitespace or an unrecognized token.
pub(crate) fn detect_target(
    row_text: &str,
    byte_pos: usize,
    hyperlink: Option<&str>,
) -> Option<ClickTarget> {
    if let Some(uri) = hyperlink {
        return Some(ClickTarget::Url(uri.to_string()));
    }
    if let Some(url) = find_url_at_byte(row_text, byte_pos) {
        return Some(ClickTarget::Url(url.to_string()));
    }
    if let Some(issue) = find_issue_ref_at_byte(row_text, byte_pos) {
        return Some(issue);
    }
    find_path_at_byte(row_text, byte_pos)
}

/// Return the byte span of the whitespace-delimited token containing
/// `pos`. `None` when `pos` is out of range, not on a char boundary,
/// or sits on whitespace (no token under the cursor).
fn token_at_byte(s: &str, pos: usize) -> Option<(usize, usize)> {
    if pos >= s.len() || !s.is_char_boundary(pos) {
        return None;
    }
    let here = s[pos..].chars().next()?;
    if here.is_whitespace() {
        return None;
    }
    let mut start = pos;
    while start > 0 {
        let prev = s[..start].chars().next_back()?;
        if prev.is_whitespace() {
            break;
        }
        start -= prev.len_utf8();
    }
    let mut end = pos;
    while end < s.len() {
        let next = s[end..].chars().next()?;
        if next.is_whitespace() {
            break;
        }
        end += next.len_utf8();
    }
    Some((start, end))
}

/// Strip a single layer of wrapping brackets / quotes and trailing
/// sentence punctuation so `(./foo.rs),` becomes `./foo.rs`. A
/// trailing `:` is preserved — it's significant for `path:line`
/// suffixes. Leading openers are only stripped when the matching
/// closer is present so we don't eat a real leading char.
fn trim_token(tok: &str) -> &str {
    let mut t = tok;
    loop {
        let trimmed = t
            .strip_suffix(['.', ',', ';', '!', '?', ')', ']', '}', '>', '"', '\''])
            .unwrap_or(t);
        let trimmed = trimmed
            .strip_prefix(['(', '[', '{', '<', '"', '\''])
            .unwrap_or(trimmed);
        if trimmed == t {
            return t;
        }
        t = trimmed;
    }
}

/// Detect a `#42` or `owner/repo#42` issue reference under `byte_pos`.
fn find_issue_ref_at_byte(row_text: &str, byte_pos: usize) -> Option<ClickTarget> {
    let (start, end) = token_at_byte(row_text, byte_pos)?;
    let tok = trim_token(&row_text[start..end]);

    // Same-repo: `#42`.
    if let Some(rest) = tok.strip_prefix('#') {
        if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
            return Some(ClickTarget::Issue {
                repo: None,
                number: rest.parse().ok()?,
            });
        }
        return None;
    }

    // Cross-repo: `owner/repo#42`. Mirrors the validation in
    // `lazybox_core::issue_links`: the repo part must contain a `/` and
    // only owner/repo-legal characters.
    let hash = tok.find('#')?;
    let repo = &tok[..hash];
    let rest = &tok[hash + 1..];
    if repo.contains('/')
        && repo
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.'))
        && !rest.is_empty()
        && rest.chars().all(|c| c.is_ascii_digit())
    {
        return Some(ClickTarget::Issue {
            repo: Some(repo.to_string()),
            number: rest.parse().ok()?,
        });
    }
    None
}

/// Detect a file path under `byte_pos`. To avoid treating every bare
/// word as a file, a candidate must start with an unambiguous path
/// prefix (`/`, `~`, `./`, `../`). A trailing `:line` or `:line:col`
/// suffix is split off and parsed.
fn find_path_at_byte(row_text: &str, byte_pos: usize) -> Option<ClickTarget> {
    let (start, end) = token_at_byte(row_text, byte_pos)?;
    let tok = trim_token(&row_text[start..end]);
    if !(tok == "~" || looks_like_path(tok)) {
        return None;
    }
    let (path, line, col) = split_line_col(tok);
    Some(ClickTarget::Path {
        path: path.to_string(),
        line,
        col,
    })
}

/// Split a trailing `:line` or `:line:col` suffix off a path. Both
/// suffix segments must be all-ASCII-digit runs; anything else leaves
/// the path untouched (so a colon inside a filename isn't mistaken
/// for a line number).
fn split_line_col(s: &str) -> (&str, Option<u32>, Option<u32>) {
    let Some((rest, last)) = s.rsplit_once(':') else {
        return (s, None, None);
    };
    if last.is_empty() || !last.chars().all(|c| c.is_ascii_digit()) {
        return (s, None, None);
    }
    if let Some((rest2, mid)) = rest.rsplit_once(':')
        && !mid.is_empty()
        && mid.chars().all(|c| c.is_ascii_digit())
    {
        // path:line:col
        return (rest2, mid.parse().ok(), last.parse().ok());
    }
    // path:line
    (rest, last.parse().ok(), None)
}

#[cfg(test)]
mod scroll_outcome_tests {
    use super::*;

    fn bar(offset: u64) -> vt::terminal::Scrollbar {
        vt::terminal::Scrollbar {
            total: 100,
            offset,
            len: 20,
        }
    }

    #[test]
    fn unchanged_mid_buffer_transition_is_stalled_not_moved() {
        assert_eq!(
            classify_scroll_transition(ScrollRequest::By(-3), bar(40), bar(40)),
            ScrollOutcome::Stalled {
                request: ScrollRequest::By(-3),
                offset: 40,
                total: 100,
                len: 20,
            },
        );
    }

    #[test]
    fn changed_transition_carries_both_observed_offsets() {
        assert_eq!(
            classify_scroll_transition(ScrollRequest::By(-3), bar(40), bar(37)),
            ScrollOutcome::Moved {
                from: 40,
                offset: 37,
                total: 100,
                len: 20,
            },
        );
    }
}

#[cfg(test)]
mod find_url_at_byte_tests {
    use super::find_url_at_byte;

    #[test]
    fn returns_url_when_click_inside_https() {
        let row = "see https://example.com here";
        // Click on the 'h' of https (column 4).
        assert_eq!(find_url_at_byte(row, 4), Some("https://example.com"));
        // Click on the 'm' of .com (column 22).
        assert_eq!(find_url_at_byte(row, 22), Some("https://example.com"));
    }

    #[test]
    fn returns_url_when_click_inside_http() {
        let row = "go http://example.com/foo done";
        assert_eq!(find_url_at_byte(row, 3), Some("http://example.com/foo"));
    }

    #[test]
    fn returns_none_when_click_outside_url() {
        let row = "see https://example.com here";
        // Click on space before URL.
        assert_eq!(find_url_at_byte(row, 3), None);
        // Click on space after URL.
        assert_eq!(find_url_at_byte(row, 23), None);
        // Click on 'h' of "here".
        assert_eq!(find_url_at_byte(row, 24), None);
    }

    #[test]
    fn trims_trailing_punctuation() {
        // Sentence-ending period must NOT be part of the URL.
        let row = "visit https://example.com.";
        assert_eq!(find_url_at_byte(row, 6), Some("https://example.com"));
        // Click on the trailing period itself returns None — the
        // period isn't part of the URL anymore.
        assert_eq!(find_url_at_byte(row, 25), None);
    }

    #[test]
    fn trims_closing_bracket() {
        let row = "see (https://example.com)";
        assert_eq!(find_url_at_byte(row, 5), Some("https://example.com"));
    }

    #[test]
    fn returns_none_when_no_url() {
        let row = "plain text with no link here";
        assert_eq!(find_url_at_byte(row, 0), None);
        assert_eq!(find_url_at_byte(row, 10), None);
    }

    #[test]
    fn picks_correct_url_when_multiple_on_row() {
        let row = "first https://a.example.com then http://b.example.com end";
        // Inside first URL.
        assert_eq!(find_url_at_byte(row, 10), Some("https://a.example.com"));
        // Inside second URL.
        assert_eq!(find_url_at_byte(row, 40), Some("http://b.example.com"));
        // Between them.
        assert_eq!(find_url_at_byte(row, 28), None);
    }

    #[test]
    fn url_at_end_of_row() {
        let row = "tail https://example.com";
        // Last char of URL.
        let last = row.len() - 1;
        assert_eq!(find_url_at_byte(row, last), Some("https://example.com"));
    }
}

#[cfg(test)]
mod detect_target_tests {
    use super::{ClickTarget, detect_target, split_line_col};

    #[test]
    fn url_wins_over_everything() {
        let row = "open https://github.com/o/r/issues/9 now";
        assert_eq!(
            detect_target(row, 6, None),
            Some(ClickTarget::Url("https://github.com/o/r/issues/9".into()))
        );
    }

    #[test]
    fn osc8_hyperlink_wins_over_visible_text() {
        // The visible glyphs are a title, not a URL — only the OSC 8
        // URI knows where the link points, and it must win.
        let row = "view the docs";
        assert_eq!(
            detect_target(row, 0, Some("https://example.com/docs")),
            Some(ClickTarget::Url("https://example.com/docs".into()))
        );
    }

    #[test]
    fn osc8_hyperlink_overrides_a_different_visible_url() {
        let row = "https://decoy.example";
        assert_eq!(
            detect_target(row, 0, Some("https://real.example/page")),
            Some(ClickTarget::Url("https://real.example/page".into()))
        );
    }

    #[test]
    fn detects_absolute_path() {
        let row = "see /etc/hosts for config";
        assert_eq!(
            detect_target(row, 5, None),
            Some(ClickTarget::Path {
                path: "/etc/hosts".into(),
                line: None,
                col: None,
            })
        );
    }

    #[test]
    fn detects_home_relative_path() {
        let row = "edit ~/.config/lazybox.yaml please";
        assert_eq!(
            detect_target(row, 6, None),
            Some(ClickTarget::Path {
                path: "~/.config/lazybox.yaml".into(),
                line: None,
                col: None,
            })
        );
    }

    #[test]
    fn detects_dot_relative_path() {
        let row = "open ./src/main.rs here";
        assert_eq!(
            detect_target(row, 5, None),
            Some(ClickTarget::Path {
                path: "./src/main.rs".into(),
                line: None,
                col: None,
            })
        );
    }

    #[test]
    fn detects_path_with_line() {
        let row = "at ./src/main.rs:42 boom";
        assert_eq!(
            detect_target(row, 4, None),
            Some(ClickTarget::Path {
                path: "./src/main.rs".into(),
                line: Some(42),
                col: None,
            })
        );
    }

    #[test]
    fn detects_path_with_line_and_col() {
        let row = "panic at /abs/file.rs:12:3 here";
        assert_eq!(
            detect_target(row, 10, None),
            Some(ClickTarget::Path {
                path: "/abs/file.rs".into(),
                line: Some(12),
                col: Some(3),
            })
        );
    }

    #[test]
    fn trims_wrapping_punctuation_on_path() {
        let row = "see (./README.md).";
        assert_eq!(
            detect_target(row, 6, None),
            Some(ClickTarget::Path {
                path: "./README.md".into(),
                line: None,
                col: None,
            })
        );
    }

    #[test]
    fn bare_word_is_not_a_path() {
        let row = "the quick brown fox";
        assert_eq!(detect_target(row, 5, None), None);
    }

    #[test]
    fn relative_without_dot_prefix_is_not_a_path() {
        // `src/main.rs` (no leading ./) is too ambiguous — could be
        // prose — so we require an explicit prefix.
        let row = "src/main.rs changed";
        assert_eq!(detect_target(row, 2, None), None);
    }

    #[test]
    fn detects_same_repo_issue() {
        let row = "fixed in #42 today";
        assert_eq!(
            detect_target(row, 10, None),
            Some(ClickTarget::Issue {
                repo: None,
                number: 42,
            })
        );
    }

    #[test]
    fn detects_cross_repo_issue() {
        let row = "see acme/widgets#7 upstream";
        assert_eq!(
            detect_target(row, 4, None),
            Some(ClickTarget::Issue {
                repo: Some("acme/widgets".into()),
                number: 7,
            })
        );
    }

    #[test]
    fn issue_with_trailing_punctuation() {
        let row = "closes #99.";
        assert_eq!(
            detect_target(row, 7, None),
            Some(ClickTarget::Issue {
                repo: None,
                number: 99,
            })
        );
    }

    #[test]
    fn hash_without_digits_is_not_an_issue() {
        let row = "a #section heading";
        assert_eq!(detect_target(row, 2, None), None);
    }

    #[test]
    fn whitespace_click_returns_none() {
        let row = "see /etc/hosts here";
        // Column 3 is the space before the path.
        assert_eq!(detect_target(row, 3, None), None);
    }

    #[test]
    fn split_line_col_variants() {
        assert_eq!(split_line_col("/a/b.rs"), ("/a/b.rs", None, None));
        assert_eq!(split_line_col("/a/b.rs:7"), ("/a/b.rs", Some(7), None));
        assert_eq!(split_line_col("/a/b.rs:7:3"), ("/a/b.rs", Some(7), Some(3)));
        // A non-numeric trailing segment is part of the path.
        assert_eq!(split_line_col("/a/b:c"), ("/a/b:c", None, None));
    }
}

#[cfg(test)]
mod ctrl_w_tests {
    //! Tile management moved under the `]]` leader (#286); `Ctrl-w`
    //! is no longer a lazybox prefix and must reach the inner program
    //! unmediated (readline word-erase, vim/emacs window commands).
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use lazybox_ipc::Command;

    fn shell_stack() -> TerminalStack {
        let sk = SessionKey::new("session");
        let mut stack = TerminalStack::new(PaneId::new(0));
        let slot = TerminalStack::make_slot(
            sk.clone(),
            TerminalKind::Shell,
            0,
            false,
            false,
            None,
            Vec::new(),
            String::new(),
        );
        stack.insert_slot_for_test(TerminalId(1), slot);
        stack.set_active_session(Some(sk));
        stack
    }

    fn ctrl_w() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL)
    }

    fn write_bytes(cmds: &[Command]) -> Vec<u8> {
        cmds.iter()
            .flat_map(|c| match c {
                Command::Write { bytes, .. } => bytes.clone(),
                _ => Vec::new(),
            })
            .collect()
    }

    #[test]
    fn ctrl_w_forwards_straight_to_the_pty() {
        let mut stack = shell_stack();
        let mut cmds = Vec::new();
        stack.handle_key(ctrl_w(), &mut cmds);
        assert_eq!(
            write_bytes(&cmds),
            vec![0x17],
            "Ctrl-w is the inner program's key, not a lazybox prefix"
        );
    }
}

#[cfg(test)]
mod write_chunking_tests {
    //! One unchunked >256 KiB `Command::Write` (a huge bracketed
    //! paste) used to exceed the daemon's `MAX_COMMAND_FRAME_BYTES`
    //! socket ingress cap and permanently kill a `--connect` session.
    //! Every PTY-bound write from this pane now flows through
    //! `TerminalStack::push_write`, which splits at
    //! `MAX_WRITE_CHUNK_BYTES` boundaries.
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use lazybox_ipc::{Command, MAX_COMMAND_FRAME_BYTES, MAX_WRITE_CHUNK_BYTES};

    #[test]
    fn oversized_write_splits_into_ordered_chunks_under_the_command_cap() {
        // A paste comfortably above the daemon's command-frame cap,
        // with a non-uniform pattern so a dropped, truncated, or
        // reordered chunk cannot concatenate back to the original.
        let original: Vec<u8> = (0..(MAX_COMMAND_FRAME_BYTES as usize + 100 * 1024))
            .map(|i| (i % 251) as u8)
            .collect();

        let mut cmds = Vec::new();
        TerminalStack::push_write(
            &mut cmds,
            TerminalId(4),
            original.clone(),
            TerminalInputIntent::Compose,
        );

        assert!(
            cmds.len() > 1,
            "a paste above the cap must split into multiple writes"
        );
        let mut reassembled = Vec::new();
        for cmd in &cmds {
            match cmd {
                Command::Write {
                    terminal_id, bytes, ..
                } => {
                    assert_eq!(*terminal_id, TerminalId(4));
                    assert!(
                        bytes.len() <= MAX_WRITE_CHUNK_BYTES,
                        "each chunk must frame under the daemon's command cap \
                         ({} > {MAX_WRITE_CHUNK_BYTES})",
                        bytes.len(),
                    );
                    reassembled.extend_from_slice(bytes);
                }
                other => panic!("push_write must only emit Write commands: {other:?}"),
            }
        }
        assert_eq!(
            reassembled, original,
            "chunks must concatenate byte-identically, in order"
        );
    }

    /// The everyday path is untouched: a single keystroke through
    /// `handle_key` still yields exactly one small `Command::Write`.
    #[test]
    fn ordinary_keystroke_stays_a_single_write() {
        let sk = SessionKey::new("session");
        let mut stack = TerminalStack::new(PaneId::new(0));
        let slot = TerminalStack::make_slot(
            sk.clone(),
            TerminalKind::Shell,
            0,
            false,
            false,
            None,
            Vec::new(),
            String::new(),
        );
        stack.insert_slot_for_test(TerminalId(1), slot);
        stack.set_active_session(Some(sk));

        let mut cmds = Vec::new();
        stack.handle_key(
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
            &mut cmds,
        );
        let writes: Vec<_> = cmds
            .iter()
            .filter(|c| matches!(c, Command::Write { .. }))
            .collect();
        assert_eq!(writes.len(), 1, "one keystroke, one write: {cmds:?}");
        assert!(matches!(
            writes[0],
            Command::Write { bytes, .. } if bytes == &vec![b'a']
        ));
    }
}

#[cfg(test)]
mod key_encoding_tests {
    use super::key_to_bytes;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn k(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn ctrl_w_folds_to_control_byte() {
        // The literal-Ctrl-W escape hatch relies on this encoding.
        assert_eq!(
            key_to_bytes(&k(KeyCode::Char('w'), KeyModifiers::CONTROL)),
            Some(vec![0x17]),
        );
    }

    #[test]
    fn alt_char_is_esc_prefixed() {
        // Alt-b (readline word-back) was dropped to a bare `b`.
        assert_eq!(
            key_to_bytes(&k(KeyCode::Char('b'), KeyModifiers::ALT)),
            Some(vec![0x1b, b'b']),
        );
    }

    #[test]
    fn modified_arrows_carry_xterm_modifier() {
        // Bare arrow keeps the short form.
        assert_eq!(
            key_to_bytes(&k(KeyCode::Right, KeyModifiers::NONE)),
            Some(b"\x1b[C".to_vec())
        );
        // Ctrl-Right → word-right: ESC[1;5C (mod = 1 + ctrl(4)).
        assert_eq!(
            key_to_bytes(&k(KeyCode::Right, KeyModifiers::CONTROL)),
            Some(b"\x1b[1;5C".to_vec()),
        );
        // Shift-Up → selection: ESC[1;2A (mod = 1 + shift(1)).
        assert_eq!(
            key_to_bytes(&k(KeyCode::Up, KeyModifiers::SHIFT)),
            Some(b"\x1b[1;2A".to_vec()),
        );
    }

    #[test]
    fn function_keys_and_nav_are_encoded() {
        assert_eq!(
            key_to_bytes(&k(KeyCode::F(1), KeyModifiers::NONE)),
            Some(b"\x1bOP".to_vec())
        );
        assert_eq!(
            key_to_bytes(&k(KeyCode::F(5), KeyModifiers::NONE)),
            Some(b"\x1b[15~".to_vec())
        );
        assert_eq!(
            key_to_bytes(&k(KeyCode::F(12), KeyModifiers::NONE)),
            Some(b"\x1b[24~".to_vec())
        );
        assert_eq!(
            key_to_bytes(&k(KeyCode::PageUp, KeyModifiers::NONE)),
            Some(b"\x1b[5~".to_vec())
        );
        assert_eq!(
            key_to_bytes(&k(KeyCode::Insert, KeyModifiers::NONE)),
            Some(b"\x1b[2~".to_vec())
        );
    }
}

#[cfg(test)]
mod osc52_tests {
    use super::{osc52_ranges, osc52_scan};

    #[test]
    fn empty_input_returns_no_ranges() {
        assert!(osc52_ranges(b"").is_empty());
    }

    #[test]
    fn no_osc_sequence_returns_no_ranges() {
        assert!(osc52_ranges(b"plain output, no clipboard requests").is_empty());
    }

    #[test]
    fn finds_bel_terminated_sequence() {
        // Standard `ESC ] 52 ; c ; aGVsbG8= BEL` — copy "hello" to
        // clipboard. The whole sequence must be in the returned range.
        let bytes = b"prefix\x1b]52;c;aGVsbG8=\x07suffix";
        let ranges = osc52_ranges(bytes);
        assert_eq!(ranges.len(), 1);
        let r = &ranges[0];
        assert_eq!(&bytes[r.clone()], b"\x1b]52;c;aGVsbG8=\x07");
    }

    #[test]
    fn finds_string_terminator_sequence() {
        // ST form: `ESC \` instead of BEL.
        let bytes = b"\x1b]52;c;aGVsbG8=\x1b\\";
        let ranges = osc52_ranges(bytes);
        assert_eq!(ranges.len(), 1);
        assert_eq!(&bytes[ranges[0].clone()], bytes);
    }

    #[test]
    fn finds_multiple_sequences_in_one_chunk() {
        // Two back-to-back copies — both ranges must come back,
        // in order, non-overlapping.
        let bytes = b"\x1b]52;c;Zm9v\x07middle\x1b]52;p;YmFy\x07tail";
        let ranges = osc52_ranges(bytes);
        assert_eq!(ranges.len(), 2);
        assert_eq!(&bytes[ranges[0].clone()], b"\x1b]52;c;Zm9v\x07");
        assert_eq!(&bytes[ranges[1].clone()], b"\x1b]52;p;YmFy\x07");
    }

    #[test]
    fn unterminated_sequence_emits_no_complete_range_but_is_pending() {
        // Sequence starts but no BEL/ST in the chunk. No complete range
        // is emitted (we never write a half-sequence to the host), but
        // the start is reported so the caller carries it forward instead
        // of dropping the copy.
        let bytes = b"\x1b]52;c;aGVsbG8=";
        let (ranges, pending) = osc52_scan(bytes);
        assert!(ranges.is_empty());
        assert_eq!(pending, Some(0));
    }

    #[test]
    fn split_payload_completes_when_carried_into_next_chunk() {
        // A large base64 payload split across two PTY reads. The first
        // chunk is pending from its OSC 52 start; concatenating the
        // carried tail with the next chunk yields one complete range
        // spanning the whole sequence — the contract `forward_osc52`
        // relies on.
        let chunk1 = b"out\x1b]52;c;aGVsbG8g".to_vec();
        let chunk2 = b"d29ybGQ=\x07more".to_vec();
        let (ranges1, pending1) = osc52_scan(&chunk1);
        assert!(ranges1.is_empty());
        let start = pending1.expect("first chunk leaves a pending OSC 52");

        let mut combined = chunk1[start..].to_vec();
        combined.extend_from_slice(&chunk2);
        let (ranges2, pending2) = osc52_scan(&combined);
        assert_eq!(ranges2.len(), 1);
        assert_eq!(pending2, None);
        assert_eq!(
            &combined[ranges2[0].clone()],
            b"\x1b]52;c;aGVsbG8gd29ybGQ=\x07"
        );
    }

    #[test]
    fn partial_header_at_tail_is_carried() {
        // The 5-byte `\x1b]52;` header itself split across chunks. The
        // trailing `\x1b]5` must be reported as pending so the next
        // chunk can complete it.
        let (ranges, pending) = osc52_scan(b"output\x1b]5");
        assert!(ranges.is_empty());
        assert_eq!(pending, Some(6));
    }

    #[test]
    fn ignores_non_52_osc_sequences() {
        // `OSC 11` (set background color) must NOT match — we only
        // forward clipboard requests, not arbitrary OSC traffic.
        let bytes = b"\x1b]11;#000000\x07";
        assert!(osc52_ranges(bytes).is_empty());
    }

    #[test]
    fn ignores_osc_521_or_other_prefixes_starting_with_52() {
        // `OSC 521` (hypothetical 3-digit numeric) must not match
        // because we require the `;` to follow `52`. Guards
        // against treating `521;…` as `52;1;…`.
        let bytes = b"\x1b]521;data\x07";
        assert!(osc52_ranges(bytes).is_empty());
    }
}

#[cfg(test)]
mod selection_offset_tests {
    use super::*;
    use ratatui::layout::Rect;

    /// Build a single-terminal stack focused on a freshly-fed grid.
    /// Each entry in `lines` lands on its own grid row (row 0, row 1,
    /// …) so a selection that maps to the wrong row copies a
    /// recognisably different string.
    fn stack_with(
        kind: TerminalKind,
        last_user_message: Option<&str>,
        lines: &[&str],
    ) -> TerminalStack {
        let sk = SessionKey::new("session");
        let mut stack = TerminalStack::new(PaneId::new(0));
        let mut slot = TerminalStack::make_slot(
            sk.clone(),
            kind,
            0,
            false,
            false,
            None,
            typed_history(last_user_message),
            String::new(),
        );
        let mut payload = String::new();
        for line in lines {
            payload.push_str(line);
            payload.push_str("\r\n");
        }
        slot.vt.feed(payload.as_bytes());
        stack.insert_slot_for_test(TerminalId(1), slot);
        stack.set_active_session(Some(sk));
        stack
    }

    #[test]
    fn visible_text_dumps_the_whole_grid_trimming_blank_edges() {
        let mut stack = stack_with(
            TerminalKind::Agent("claude".into()),
            Some("do the thing"),
            &["plan:", "  1. scope", "  2. build"],
        );
        let text = stack
            .visible_text(TerminalId(1))
            .expect("a fed grid yields text");
        assert_eq!(text, "plan:\n  1. scope\n  2. build");
    }

    #[test]
    fn visible_text_is_none_for_an_unknown_terminal() {
        let mut stack = stack_with(TerminalKind::Agent("claude".into()), None, &["x"]);
        assert!(stack.visible_text(TerminalId(999)).is_none());
    }

    #[test]
    fn visible_text_drops_pure_border_rows() {
        let mut stack = stack_with(
            TerminalKind::Agent("claude".into()),
            None,
            &["────────", "actual content", "╰──────╯"],
        );
        let text = stack.visible_text(TerminalId(1)).expect("content survives");
        assert_eq!(text, "actual content", "box-drawing borders are stripped");
    }

    #[test]
    fn agent_terminal_for_finds_a_kept_exited_pane() {
        // A finished agent whose process exited but whose pane is kept
        // (showing its final output) is still a valid handoff source.
        let mut stack = stack_with(TerminalKind::Agent("claude".into()), None, &["done"]);
        stack.terminals.get_mut(&TerminalId(1)).unwrap().exited = Some(TerminalExit {
            code: Some(0),
            dead_on_arrival: false,
            last_output: None,
        });
        let sk = SessionKey::new("session");
        assert_eq!(stack.agent_terminal_for(&sk), Some(TerminalId(1)));
    }

    #[test]
    fn agent_terminal_for_prefers_a_live_pane_over_an_exited_one() {
        let sk = SessionKey::new("session");
        let mut stack = stack_with(TerminalKind::Agent("claude".into()), None, &["a"]);
        // Exit the low-id pane; add a live higher-id agent in the same
        // session. The live one must win despite the higher id.
        stack.terminals.get_mut(&TerminalId(1)).unwrap().exited = Some(TerminalExit {
            code: Some(0),
            dead_on_arrival: false,
            last_output: None,
        });
        let live = TerminalStack::make_slot(
            sk.clone(),
            TerminalKind::Agent("codex".into()),
            0,
            false,
            false,
            None,
            Vec::new(),
            String::new(),
        );
        stack.insert_slot_for_test(TerminalId(5), live);
        assert_eq!(stack.agent_terminal_for(&sk), Some(TerminalId(5)));
    }

    #[test]
    fn conversion_metadata_and_live_replacement_are_terminal_scoped() {
        let mut stack = stack_with(TerminalKind::Agent("codex".into()), None, &["working tree"]);
        assert_eq!(stack.terminal_agent_id(TerminalId(1)), Some("codex"));
        assert!(!stack.terminal_is_on_main(TerminalId(1)));
        stack.terminals.get_mut(&TerminalId(1)).unwrap().on_main = true;
        assert!(stack.terminal_is_on_main(TerminalId(1)));

        let mut commands = Vec::new();
        assert!(stack.prepare_agent_replacement(TerminalId(1), "conversion-1", &mut commands));
        assert!(matches!(
            commands.as_slice(),
            [Command::Close {
                terminal_id: TerminalId(1),
                client_request_id: Some(request_id),
            }]
            if request_id == "conversion-1"
        ));
        stack.on_event(&Event::TerminalExited {
            terminal_id: TerminalId(1),
            exit_code: None,
            last_output: None,
        });
        assert!(!stack.terminals.contains_key(&TerminalId(1)));
    }

    #[test]
    fn replacing_an_exited_agent_drops_it_without_closing_again() {
        let mut stack = stack_with(TerminalKind::Agent("claude".into()), None, &["done"]);
        stack.terminals.get_mut(&TerminalId(1)).unwrap().exited = Some(TerminalExit {
            code: Some(1),
            dead_on_arrival: false,
            last_output: None,
        });
        let mut commands = Vec::new();

        assert!(!stack.prepare_agent_replacement(TerminalId(1), "conversion-1", &mut commands));
        assert!(commands.is_empty());
        assert!(!stack.terminals.contains_key(&TerminalId(1)));
    }

    /// Map two crossterm points through `selection_point` and copy the
    /// span via `extract_selection` — the whole mouse-up copy chain.
    fn copy_between(
        stack: &mut TerminalStack,
        rect: Rect,
        start: (u16, u16),
        end: (u16, u16),
    ) -> String {
        let id = stack.focused_terminal_id().expect("focused");
        let a = stack
            .selection_point(id, rect, start.0, start.1)
            .expect("anchor");
        let b = stack
            .selection_point(id, rect, end.0, end.1)
            .expect("focus");
        stack.extract_selection(id, a, b)
    }

    /// #523: the pinned `you ▸` recap above the agent grid shows the last
    /// prompt's relative age on the right, so staleness reads at a glance
    /// without opening `]]h`.
    #[test]
    fn recap_row_shows_relative_age_on_the_right() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        const W: u16 = 60;
        let area = Rect::new(0, 0, W, 1);
        let mut term = Terminal::new(TestBackend::new(W, 1)).unwrap();
        term.draw(|f| {
            TerminalStack::render_user_message_recap(f, area, "review the diff", "5m ago")
        })
        .unwrap();
        let buf = term.backend().buffer();
        let row: String = (0..W).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(row.contains("you ▸"), "prefix present: {row:?}");
        assert!(row.contains("review the diff"), "message present: {row:?}");
        assert!(
            row.trim_end().ends_with("5m ago"),
            "age is right-aligned: {row:?}"
        );
    }

    /// The age is supplementary: on a row too narrow for both, the message
    /// wins and the age is dropped rather than clipping the prompt.
    #[test]
    fn recap_row_drops_age_when_too_narrow() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        const W: u16 = 14;
        let area = Rect::new(0, 0, W, 1);
        let mut term = Terminal::new(TestBackend::new(W, 1)).unwrap();
        term.draw(|f| TerminalStack::render_user_message_recap(f, area, "hello there", "5m ago"))
            .unwrap();
        let buf = term.backend().buffer();
        let row: String = (0..W).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(
            !row.contains("5m ago"),
            "age dropped on a narrow row: {row:?}"
        );
        assert!(row.contains("you ▸"), "prefix still present: {row:?}");
    }

    #[test]
    fn agent_recap_maps_selection_to_highlighted_row() {
        let mut stack = stack_with(
            TerminalKind::Agent("claude".into()),
            Some("do the thing"),
            &["line0", "line1", "line2", "line3"],
        );
        // Border + tab strip + divider (3) plus the recap's 2 rows put
        // the grid top at screen row 5, so a click there is grid row 0.
        // Before the recap rows were accounted for this returned
        // "line2" — the row two below the highlight.
        let text = copy_between(&mut stack, Rect::new(0, 0, 80, 30), (1, 5), (10, 5));
        assert_eq!(text, "line0");
    }

    /// U1 regression (2026-08-19 audit): with no VT mutation between
    /// frames, the render path must repaint from the cached composed
    /// frame — pixel-identical to a fresh walk — and a subsequent feed
    /// must invalidate the cache so new content shows.
    #[test]
    fn unchanged_terminal_repaints_identically_from_the_cached_frame() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        const W: u16 = 60;
        const H: u16 = 8;
        let area = Rect::new(0, 0, W, H);
        let mut stack = stack_with(TerminalKind::Shell, None, &[]);
        stack
            .terminals
            .get_mut(&TerminalId(1))
            .unwrap()
            .vt
            .feed(b"hello gate\r\n");

        let mut term = Terminal::new(TestBackend::new(W, H)).unwrap();
        term.draw(|f| stack.render(area, f, true)).unwrap();
        let first = term.backend().buffer().clone();
        assert!(
            stack.terminals[&TerminalId(1)].last_frame_rev.is_some(),
            "a fresh paint must populate the frame cache"
        );

        // No mutation: the second draw takes the blit path and must be
        // pixel-identical.
        term.draw(|f| stack.render(area, f, true)).unwrap();
        assert_eq!(*term.backend().buffer(), first, "cached blit == fresh walk");

        // A feed bumps the revision; the next draw must show it.
        stack
            .terminals
            .get_mut(&TerminalId(1))
            .unwrap()
            .vt
            .feed(b"fresh bytes\r\n");
        term.draw(|f| stack.render(area, f, true)).unwrap();
        let after = term.backend().buffer();
        let text: String = (0..H)
            .map(|y| {
                (0..W)
                    .map(|x| after[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("fresh bytes"),
            "a mutation must invalidate the cached frame: {text}"
        );
    }

    /// M3 regression (2026-08-19 audit): an abnormally exited agent
    /// keeps its last painted screen visible while its VT — up to the
    /// full scrollback ceiling of memory — is dropped and replaced by
    /// an empty parser.
    #[test]
    fn crashed_agent_pane_freezes_its_screen_and_drops_the_vt() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        const W: u16 = 60;
        const H: u16 = 8;
        let area = Rect::new(0, 0, W, H);
        let mut stack = stack_with(TerminalKind::Agent("claude".into()), None, &[]);
        stack
            .terminals
            .get_mut(&TerminalId(1))
            .unwrap()
            .vt
            .feed(b"crash evidence\r\n");
        let mut term = Terminal::new(TestBackend::new(W, H)).unwrap();
        term.draw(|f| stack.render(area, f, true)).unwrap();

        stack.on_event(&Event::TerminalExited {
            terminal_id: TerminalId(1),
            exit_code: Some(2),
            last_output: None,
        });
        let slot = &stack.terminals[&TerminalId(1)];
        assert!(slot.exited.is_some(), "abnormal exit keeps the frozen slot");
        assert_eq!(
            slot.vt.content_rev, 0,
            "the dead parser was replaced by a fresh empty one"
        );

        term.draw(|f| stack.render(area, f, true)).unwrap();
        let buf = term.backend().buffer();
        let text: String = (0..H)
            .map(|y| {
                (0..W)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("crash evidence"),
            "the freeze-frame must keep showing the last screen: {text}"
        );
        assert!(
            text.contains("agent exited"),
            "the restart banner overlays the frozen screen: {text}"
        );
    }

    /// Every on-screen grid row — top and bottom boundary included —
    /// copies the exact line the renderer painted there, with recap rows
    /// present AND after scrolling into scrollback. Guards the #1021
    /// off-by-one: the selection row mapping reads the exact grid the
    /// frame drew, so it can't drift from `bar.offset` / recap / chrome.
    #[test]
    fn selection_row_matches_rendered_grid_with_recap_and_scroll() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        const W: u16 = 80;
        const H: u16 = 12;
        let area = Rect::new(0, 0, W, H);

        // Agent + remembered message → 2 recap rows carved off the top.
        let mut stack = stack_with(
            TerminalKind::Agent("claude".into()),
            Some("do the thing"),
            &[],
        );
        // Enough distinctly-numbered lines to overflow the grid, leaving
        // scrollback to scroll into.
        let mut payload = String::new();
        for i in 0..40 {
            payload.push_str(&format!("row{i:02}\r\n"));
        }
        stack
            .terminals
            .get_mut(&TerminalId(1))
            .unwrap()
            .vt
            .feed(payload.as_bytes());

        // Render to a real backend, then assert each on-screen grid row
        // copies exactly the text drawn there.
        fn assert_rows_match(stack: &mut TerminalStack, area: Rect) {
            let backend = TestBackend::new(area.width, area.height);
            let mut term = Terminal::new(backend).unwrap();
            term.draw(|f| stack.render(area, f, true)).unwrap();
            let buf = term.backend().buffer().clone();

            let (inner_x, inner_y, last_col, last_row) = stack
                .grid_bounds(TerminalId(1), area)
                .expect("focused grid");
            // The top grid row must map to the viewport-top content row —
            // the crisp no-off-by-one check.
            let bar = stack.terminals[&TerminalId(1)]
                .vt
                .terminal
                .scrollbar()
                .unwrap();
            assert_eq!(
                stack.selection_point(TerminalId(1), area, inner_x, inner_y),
                Some((0, bar.offset as u32)),
                "grid-top screen row must map to the viewport-top content row",
            );
            for screen_row in inner_y..=last_row {
                let onscreen: String = (inner_x..=last_col)
                    .map(|x| buf[(x, screen_row)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string();
                if onscreen.is_empty() {
                    continue;
                }
                let text = copy_between(stack, area, (inner_x, screen_row), (last_col, screen_row));
                assert_eq!(text, onscreen, "click at screen row {screen_row}");
            }
        }

        // At the live bottom.
        assert_rows_match(&mut stack, area);

        // …and scrolled up into scrollback (non-zero `bar.offset`).
        assert!(
            matches!(stack.scroll_active(-3), ScrollOutcome::Moved { .. }),
            "scroll must move",
        );
        let offset = stack.terminals[&TerminalId(1)]
            .vt
            .terminal
            .scrollbar()
            .unwrap()
            .offset;
        assert!(offset > 0, "must be scrolled into scrollback");
        assert_rows_match(&mut stack, area);
    }

    /// A click composes its content row against the offset of the frame the
    /// user SAW, not a live re-read. If output advances the viewport after
    /// that frame is painted but before the click is handled (no re-render
    /// between), the mapping must still use the painted frame's offset —
    /// else the selection jumps by the scroll delta (#1021).
    #[test]
    fn selection_uses_rendered_frame_offset_not_a_live_reread() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        const W: u16 = 80;
        const H: u16 = 12;
        let area = Rect::new(0, 0, W, H);

        let mut stack = stack_with(TerminalKind::Shell, None, &[]);
        let mut payload = String::new();
        for i in 0..40 {
            payload.push_str(&format!("row{i:02}\r\n"));
        }
        stack
            .terminals
            .get_mut(&TerminalId(1))
            .unwrap()
            .vt
            .feed(payload.as_bytes());

        // Paint a frame — records this frame's viewport offset.
        let mut term = Terminal::new(TestBackend::new(W, H)).unwrap();
        term.draw(|f| stack.render(area, f, true)).unwrap();

        let (inner_x, inner_y, _, _) = stack.grid_bounds(TerminalId(1), area).expect("grid");
        let rendered = stack
            .selection_point(TerminalId(1), area, inner_x, inner_y)
            .expect("grid-top maps");

        // More output arrives and advances the viewport — but WITHOUT another
        // render, so the frame still on screen shows the old top row.
        stack
            .terminals
            .get_mut(&TerminalId(1))
            .unwrap()
            .vt
            .feed(b"more0\r\nmore1\r\nmore2\r\n");
        let live = stack.terminals[&TerminalId(1)]
            .vt
            .terminal
            .scrollbar()
            .unwrap()
            .offset;
        assert!(
            live as u32 > rendered.1,
            "precondition: the live viewport advanced past the painted frame",
        );

        // The click at the same on-screen cell must still map to the painted
        // frame's content row, not the advanced live offset.
        assert_eq!(
            stack.selection_point(TerminalId(1), area, inner_x, inner_y),
            Some(rendered),
            "selection must reuse the painted frame's offset, not a live re-read",
        );
    }

    /// A right-click resolves its target through the grid the renderer
    /// drew, not a recap recomputed from live state. A prompt submitted
    /// between the painted frame and the click changes the agent's recap;
    /// the click must still land on the row it was painted at (#1021).
    #[test]
    fn rendered_target_uses_recorded_grid_across_a_recap_change() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        const W: u16 = 80;
        const H: u16 = 20;
        let area = Rect::new(0, 0, W, H);

        // Agent with NO remembered message → recap 0 at render time.
        let mut stack = stack_with(TerminalKind::Agent("claude".into()), None, &[]);
        let mut payload = String::new();
        for i in 0..8 {
            payload.push_str(&format!("plain{i}\r\n"));
        }
        payload.push_str("see https://example.com/target here\r\n");
        stack
            .terminals
            .get_mut(&TerminalId(1))
            .unwrap()
            .vt
            .feed(payload.as_bytes());

        // Paint, then locate the URL's screen cell from the buffer.
        let mut term = Terminal::new(TestBackend::new(W, H)).unwrap();
        term.draw(|f| stack.render(area, f, true)).unwrap();
        let buf = term.backend().buffer().clone();
        let (url_col, url_row) = (0..H)
            .find_map(|y| {
                let line: String = (0..W).map(|x| buf[(x, y)].symbol()).collect();
                line.find("https").map(|c| (c as u16, y))
            })
            .expect("url is on screen");

        let want = Some(ClickTarget::Url("https://example.com/target".into()));
        assert_eq!(
            stack
                .rendered_target_at(url_col, url_row)
                .and_then(|t| t.target),
            want,
            "baseline: the click resolves the URL",
        );

        // A prompt is now remembered — the LIVE recap would become 2 rows —
        // but no frame has been repainted since.
        stack
            .terminals
            .get_mut(&TerminalId(1))
            .unwrap()
            .push_prompt(lazybox_ipc::UserPrompt {
                text: "hi".into(),
                timestamp_ms: 0,
                source: lazybox_ipc::PromptSource::Typed,
            });

        // The same on-screen cell must still resolve the URL: the mapping
        // reads the recorded grid (recap 0 when painted), not the now-changed
        // live recap that would shift the row by 2.
        assert_eq!(
            stack
                .rendered_target_at(url_col, url_row)
                .and_then(|t| t.target),
            want,
            "recap drift between frame and click must not move the target row",
        );
    }

    /// Selection in a split's BOTTOM tile maps against that tile's grid —
    /// not the pane top. The bottom tile's grid begins well below the top
    /// chrome, so the old pane-rect recompute grabbed rows from the wrong
    /// tile entirely (#1021).
    #[test]
    fn selection_in_bottom_split_tile_maps_to_that_tile() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        const W: u16 = 80;
        const H: u16 = 24;
        let area = Rect::new(0, 0, W, H);

        let sk = SessionKey::new("split");
        let mut stack = TerminalStack::new(PaneId::new(0));
        let feed_tagged = |sk: &SessionKey, tag: &str| {
            let mut slot = TerminalStack::make_slot(
                sk.clone(),
                TerminalKind::Shell,
                0,
                false,
                false,
                None,
                Vec::new(),
                String::new(),
            );
            let mut payload = String::new();
            for i in 0..30 {
                payload.push_str(&format!("{tag}{i:02}\r\n"));
            }
            slot.vt.feed(payload.as_bytes());
            slot
        };
        stack
            .terminals
            .insert(TerminalId(1), feed_tagged(&sk, "TOP"));
        stack
            .terminals
            .insert(TerminalId(2), feed_tagged(&sk, "BOT"));
        stack.set_active_session(Some(sk));
        // Stacked split, focus on the BOTTOM leaf. `focused` is a
        // child-index path: index 1 = the second (bottom) child.
        stack.layout = lazybox_core::SessionLayout::Splits {
            tree: lazybox_core::TileTree::VSplit {
                top: Box::new(lazybox_core::TileTree::Leaf { terminal_id: 1 }),
                bottom: Box::new(lazybox_core::TileTree::Leaf { terminal_id: 2 }),
                ratio: 50,
            },
            focused: vec![1],
        };
        assert_eq!(
            stack.focused_terminal_id(),
            Some(TerminalId(2)),
            "precondition: the bottom tile is focused",
        );

        let mut term = Terminal::new(TestBackend::new(W, H)).unwrap();
        term.draw(|f| stack.render(area, f, true)).unwrap();
        let buf = term.backend().buffer().clone();

        let (inner_x, inner_y, last_col, last_row) = stack
            .grid_bounds(TerminalId(2), area)
            .expect("focused grid");
        // The recorded grid is the bottom tile's — its top sits far below the
        // pane's own top chrome (which would put a naive mapping at row 3).
        assert!(
            inner_y > area.y + 4,
            "bottom tile grid must start below the pane top chrome, got {inner_y}",
        );

        // Every visible row of the focused (bottom) tile copies exactly what
        // the renderer painted there — and it is BOT content, not TOP.
        for screen_row in inner_y..=last_row {
            let onscreen: String = (inner_x..=last_col)
                .map(|x| buf[(x, screen_row)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string();
            if onscreen.is_empty() {
                continue;
            }
            assert!(
                onscreen.starts_with("BOT"),
                "focused tile must show bottom content, got {onscreen:?}",
            );
            let text = copy_between(
                &mut stack,
                area,
                (inner_x, screen_row),
                (last_col, screen_row),
            );
            assert_eq!(
                text, onscreen,
                "click at screen row {screen_row} in bottom tile"
            );
        }
    }

    #[test]
    fn shell_maps_selection_to_highlighted_row() {
        let mut stack = stack_with(TerminalKind::Shell, None, &["line0", "line1", "line2"]);
        // No recap: grid top stays at screen row 3.
        let text = copy_between(&mut stack, Rect::new(0, 0, 80, 30), (1, 3), (10, 3));
        assert_eq!(text, "line0");
    }

    #[test]
    fn multi_row_selection_copies_flowing_span() {
        let mut stack = stack_with(TerminalKind::Shell, None, &["line0", "line1", "line2"]);
        // Grid row 0 (screen row 3) through grid row 2: a flowing
        // selection copies each visual row on its own line.
        let text = copy_between(&mut stack, Rect::new(0, 0, 80, 30), (1, 3), (5, 5));
        assert_eq!(text, "line0\nline1\nline2");
    }

    #[test]
    fn selection_spans_scrollback_beyond_the_viewport() {
        // Feed far more lines than the ~32-row default viewport so most
        // of the passage lives in scrollback, not the visible screen —
        // the exact case host-native selection could never reach (#432).
        let lines: Vec<String> = (0..100).map(|i| format!("row{i:03}")).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let mut stack = stack_with(TerminalKind::Shell, None, &refs);
        // Screen-absolute rows: `row000` is screen row 0, so rows 10..=19
        // are deep in scrollback, well above the live viewport.
        let text = stack.extract_selection(TerminalId(1), (0, 10), (6, 19));
        let want: String = (10..=19)
            .map(|i| format!("row{i:03}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(text, want);
    }

    #[test]
    fn selection_screen_span_round_trips_visible_cells() {
        // For a selection entirely inside the viewport, projecting the
        // screen-absolute endpoints back to crossterm cells returns the
        // very cells `selection_point` mapped them from.
        let stack = stack_with(TerminalKind::Shell, None, &["line0", "line1", "line2"]);
        let rect = Rect::new(0, 0, 80, 30);
        let a = stack
            .selection_point(TerminalId(1), rect, 3, 3)
            .expect("anchor");
        let b = stack
            .selection_point(TerminalId(1), rect, 5, 5)
            .expect("focus");
        let (pa, pb) = stack
            .selection_screen_span(TerminalId(1), rect, a, b)
            .expect("span");
        assert_eq!(pa, (3, 3));
        assert_eq!(pb, (5, 5));
    }

    #[test]
    fn selection_screen_span_clamps_offscreen_endpoints_to_the_pane() {
        // With scrollback present and the viewport at the live bottom,
        // an anchor at the oldest screen row sits far above the viewport;
        // it clamps to the top of the pane so the visible portion of a
        // scrollback-spanning selection still highlights (#432).
        let lines: Vec<String> = (0..100).map(|i| format!("row{i:03}")).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let stack = stack_with(TerminalKind::Shell, None, &refs);
        let rect = Rect::new(0, 10, 80, 20);
        let visible = stack
            .selection_point(TerminalId(1), rect, 3, 13)
            .expect("focus");
        let ((_ax, ay), _b) = stack
            .selection_screen_span(TerminalId(1), rect, (0, 0), visible)
            .expect("span");
        assert_eq!(
            ay, rect.y,
            "an anchor above the viewport clamps to the pane top"
        );
    }

    #[test]
    fn selection_point_tracks_scroll_offset() {
        // Scrollback present: the same on-screen cell resolves to an
        // older (smaller) screen-absolute row after scrolling up, which
        // is what pins a drag anchor to its content across auto-scroll.
        let lines: Vec<String> = (0..100).map(|i| format!("row{i:03}")).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let mut stack = stack_with(TerminalKind::Shell, None, &refs);
        let rect = Rect::new(0, 0, 80, 30);
        let bottom = stack
            .selection_point(TerminalId(1), rect, 1, 3)
            .expect("at live bottom");
        let _ = stack.scroll_active(-5);
        let scrolled = stack
            .selection_point(TerminalId(1), rect, 1, 3)
            .expect("after scrolling up");
        assert_eq!(
            bottom.0, scrolled.0,
            "column is unaffected by a vertical scroll",
        );
        assert_eq!(
            bottom.1 - scrolled.1,
            5,
            "scrolling up 5 rows moves the same cell 5 rows earlier in screen space",
        );
    }

    #[test]
    fn osc8_hyperlink_is_a_click_target_when_mouse_tracking_off() {
        // An OSC 8 hyperlink whose visible text ("docs") isn't a URL.
        // Right-click-to-open must resolve it via the hyperlink URI,
        // not the text scan — and it must do so while the inner
        // program isn't tracking the mouse (the idle state where the
        // event would otherwise never reach the PTY either, #22).
        let link = "\x1b]8;;https://example.com/page\x1b\\docs\x1b]8;;\x1b\\";
        let mut stack = stack_with(TerminalKind::Shell, None, &[link]);

        assert!(
            !stack.focused_terminal_tracks_mouse(),
            "precondition: a freshly-fed shell isn't mouse-tracking",
        );

        // Shell has no recap: grid row 0 renders at screen row 3, and
        // the body starts one column in past the left border.
        let target = stack.target_at(Rect::new(0, 0, 80, 30), 1, 3);
        assert_eq!(
            target,
            Some(ClickTarget::Url("https://example.com/page".into())),
        );
    }

    #[test]
    fn wide_glyphs_copy_without_spurious_spaces() {
        // Each CJK glyph occupies two cells (base + blank spacer tail).
        // The spacer must be skipped on copy, or this comes back as
        // "日 本 語  s p a c e d".
        let mut stack = stack_with(TerminalKind::Shell, None, &["日本語"]);
        let text = copy_between(&mut stack, Rect::new(0, 0, 80, 30), (1, 3), (40, 3));
        assert_eq!(text, "日本語");
    }

    #[test]
    fn right_click_resolves_url_after_wide_text() {
        // A wide-glyph prefix shifts every following screen column by
        // one spacer cell. If target_at's column→byte map doesn't
        // account for the spacer tails, the click lands on the wrong
        // byte and the URL is mis-parsed (or missed). The URL starts at
        // screen column 1(border) + 6 cells (3 wide glyphs) = column 7.
        let mut stack = stack_with(TerminalKind::Shell, None, &["日本語 https://example.com/x"]);
        let target = stack.target_at(Rect::new(0, 0, 80, 30), 1 + 7, 3);
        assert_eq!(
            target,
            Some(ClickTarget::Url("https://example.com/x".into())),
        );
    }

    /// A long URL that soft-wraps across rows is one logical link:
    /// right-clicking ANY of its rows resolves the whole URL, not the
    /// fragment on that row (#596). Before the fix `target_at` was
    /// single-row and returned `None` off the first row.
    #[test]
    fn right_click_resolves_a_soft_wrapped_url() {
        let sk = SessionKey::new("session");
        let mut stack = TerminalStack::new(PaneId::new(0));
        let mut slot = TerminalStack::make_slot(
            sk.clone(),
            TerminalKind::Shell,
            0,
            false,
            false,
            None,
            Vec::new(),
            String::new(),
        );
        // A narrow grid forces the 54-char URL to soft-wrap over 3 rows.
        slot.vt.ensure_size(20, 10);
        slot.vt
            .feed(b"https://example.com/a/very/long/path/that/wraps/around\r\n");
        stack.insert_slot_for_test(TerminalId(1), slot);
        stack.set_active_session(Some(sk));

        let rect = Rect::new(0, 0, 80, 30);
        let want =
            ClickTarget::Url("https://example.com/a/very/long/path/that/wraps/around".into());
        // Grid row 0 renders at screen row 3 (shell, no recap); the
        // wrapped continuations follow at 4 and 5. Every one resolves the
        // whole URL.
        for screen_row in 3..=5 {
            assert_eq!(
                stack.target_at(rect, 1, screen_row),
                Some(want.clone()),
                "click on wrapped row {screen_row} resolves the full URL",
            );
        }
    }

    #[test]
    fn focused_urls_collects_scans_and_dedups_including_wrapped() {
        let sk = SessionKey::new("session");
        let mut stack = TerminalStack::new(PaneId::new(0));
        let mut slot = TerminalStack::make_slot(
            sk.clone(),
            TerminalKind::Shell,
            0,
            false,
            false,
            None,
            Vec::new(),
            String::new(),
        );
        slot.vt.ensure_size(20, 12);
        // A plain URL, the SAME URL again (deduped), a soft-wrapped one
        // (stitched whole), and a non-URL line.
        slot.vt.feed(b"https://a.example.com\r\n");
        slot.vt.feed(b"https://a.example.com\r\n");
        slot.vt
            .feed(b"https://b.example.com/a/long/wrapping/path\r\n");
        slot.vt.feed(b"no link on this line\r\n");
        stack.insert_slot_for_test(TerminalId(1), slot);
        stack.set_active_session(Some(sk));

        assert_eq!(
            stack.focused_urls(),
            Some(vec![
                "https://a.example.com".to_string(),
                "https://b.example.com/a/long/wrapping/path".to_string(),
            ]),
        );
    }

    #[test]
    fn scan_urls_finds_every_url_trimming_punctuation() {
        assert_eq!(
            scan_urls("go to https://a.example.com, then https://b.example.com."),
            vec!["https://a.example.com", "https://b.example.com"],
        );
        assert!(scan_urls("no urls at all here").is_empty());
    }

    /// A URL butting against an agent composer's box-drawing frame with
    /// no separating space must not carry the border glyph into the
    /// opened link (right-click and `]]u` share this scanner).
    #[test]
    fn scan_urls_trims_a_trailing_box_drawing_border() {
        assert_eq!(
            scan_urls("│ https://example.com/path│"),
            vec!["https://example.com/path"],
        );
        // A leading border is already excluded — the scan starts at the
        // scheme — so both sides come out clean.
        assert_eq!(
            scan_urls("│https://example.com╮"),
            vec!["https://example.com"],
        );
    }

    /// `focused_urls` returns `None` ONLY when no terminal is focused —
    /// the caller's cue to say "no terminal focused" rather than
    /// "no URLs". A focused-but-URL-less terminal yields `Some(empty)`.
    #[test]
    fn focused_urls_is_none_without_a_focused_terminal() {
        let mut empty = TerminalStack::new(PaneId::new(0));
        assert_eq!(empty.focused_urls(), None);

        let mut stack = stack_with(TerminalKind::Shell, None, &["no links on this line"]);
        assert_eq!(
            stack.focused_urls(),
            Some(Vec::new()),
            "a focused terminal with no URLs scans to an empty Vec, not None",
        );
    }

    /// A URL echoed twice is kept once, at its MOST RECENT (lowest on
    /// screen) row — so the `]]u` picker's newest-first list is accurate.
    #[test]
    fn focused_urls_dedups_keeping_the_latest_position() {
        let mut stack = stack_with(
            TerminalKind::Shell,
            None,
            &[
                "https://a.example.com",
                "https://b.example.com",
                "https://a.example.com",
            ],
        );
        // `a` first appears on row 0 but is re-echoed on row 2, below
        // `b` on row 1 — so recency order is [b, a], not [a, b].
        assert_eq!(
            stack.focused_urls(),
            Some(vec![
                "https://b.example.com".to_string(),
                "https://a.example.com".to_string(),
            ]),
        );
    }

    #[test]
    fn agent_without_remembered_message_has_no_recap_offset() {
        let mut stack = stack_with(
            TerminalKind::Agent("claude".into()),
            None,
            &["line0", "line1"],
        );
        let text = copy_between(&mut stack, Rect::new(0, 0, 80, 30), (1, 3), (10, 3));
        assert_eq!(text, "line0");
    }

    #[test]
    fn recap_rows_refused_when_body_too_short() {
        let sk = SessionKey::new("session");
        let mut slot = TerminalStack::make_slot(
            sk,
            TerminalKind::Agent("claude".into()),
            0,
            false,
            false,
            None,
            Vec::new(),
            String::new(),
        );
        slot.push_prompt(lazybox_ipc::UserPrompt {
            text: "hi".into(),
            timestamp_ms: 0,
            source: lazybox_ipc::PromptSource::Typed,
        });
        assert_eq!(TerminalStack::recap_rows(&slot, 2), 0);
        assert_eq!(TerminalStack::recap_rows(&slot, 3), 2);
    }

    #[test]
    fn screen_to_cell_maps_grid_and_rejects_chrome_and_borders() {
        let stack = stack_with(TerminalKind::Shell, None, &["line0"]);
        let rect = Rect::new(0, 0, 80, 30);
        // Grid origin: left border (+1 col), 3 top-chrome rows (shell has
        // no recap). And a parity spot-check against target_at's geometry.
        assert_eq!(stack.screen_to_cell(rect, 1, 3), Some((0, 0)));
        assert_eq!(stack.screen_to_cell(rect, 6, 8), Some((5, 5)));
        // Chrome above / left of the grid → None.
        assert_eq!(stack.screen_to_cell(rect, 0, 3), None, "left border col");
        assert_eq!(stack.screen_to_cell(rect, 1, 2), None, "tab-strip row");
        // Right border column and bottom row → None (off-grid, must not
        // forward a bogus near-edge cell to the inner program).
        assert_eq!(stack.screen_to_cell(rect, 79, 5), None, "right border col");
        assert_eq!(stack.screen_to_cell(rect, 5, 29), None, "bottom row");
    }

    #[test]
    fn recap_geometry_matches_renderer_at_height_6() {
        // At pane height 6 the renderer's body is `area.height - 4 = 2`
        // rows, so `recap_rows` is REFUSED (needs ≥ 3) and the grid top
        // sits at screen row 3. The readers must agree: computing recap on
        // `height - 3 = 3` (the old basis) wrongly assumed 2 recap rows and
        // pushed the grid top to row 5 — off-screen for this pane.
        let stack = stack_with(
            TerminalKind::Agent("claude".into()),
            Some("do the thing"),
            &["line0"],
        );
        let rect = Rect::new(0, 0, 80, 6);
        assert_eq!(stack.screen_to_cell(rect, 1, 3), Some((0, 0)));
    }
}

#[cfg(test)]
mod resync_tests {
    //! After the bounded event channel drops `TerminalOutput`, the
    //! daemon emits one `TerminalResync` carrying the full ring. The
    //! consumer must rebuild a *correct* grid from it — re-feeding onto
    //! the desynced parser (the naive drop) would garble the screen.
    use super::*;
    use ratatui::layout::Rect;

    const ROW0: (u16, u16) = (1, 3);
    const ROW1: (u16, u16) = (1, 4);

    fn shell_stack(id: TerminalId, sk: &SessionKey) -> TerminalStack {
        let mut stack = TerminalStack::new(PaneId::new(0));
        stack.on_event(&Event::TerminalSpawned {
            model_label: None,
            terminal_id: id,
            session_key: sk.clone(),
            kind: TerminalKind::Shell,
            no_permission: false,
            on_main: false,
        });
        stack.set_active_session(Some(sk.clone()));
        stack
    }

    fn row(stack: &mut TerminalStack, at: (u16, u16)) -> String {
        let rect = Rect::new(0, 0, 80, 30);
        let id = stack.focused_terminal_id().expect("focused");
        let a = stack.selection_point(id, rect, at.0, at.1).expect("anchor");
        let b = stack.selection_point(id, rect, 20, at.1).expect("focus");
        stack.extract_selection(id, a, b)
    }

    #[test]
    fn resync_rebuilds_correct_grid_after_dropped_output() {
        let sk = SessionKey::new("s");
        // The full byte stream the daemon ring holds at resync time.
        let full = b"line0\r\nline1\r\nline2\r\n";

        // Reference: a client that received the whole stream cleanly.
        let mut clean = shell_stack(TerminalId(1), &sk);
        clean.on_event(&Event::TerminalOutput {
            terminal_id: TerminalId(1),
            bytes: full.to_vec(),
            first_seq: 1,
            seq: 9,
        });
        let want0 = row(&mut clean, ROW0);
        let want1 = row(&mut clean, ROW1);
        assert_eq!(want0, "line0");
        assert_eq!(want1, "line1");

        // Desynced: this client only saw "line0" plus an unterminated
        // CSI — the rest was dropped on a full channel. Its grid is
        // missing line1/line2 and the parser is mid-escape.
        let mut desynced = shell_stack(TerminalId(1), &sk);
        desynced.on_event(&Event::TerminalOutput {
            terminal_id: TerminalId(1),
            bytes: b"line0\r\n\x1b[".to_vec(),
            first_seq: 1,
            seq: 3,
        });
        assert_ne!(row(&mut desynced, ROW1), want1, "precondition: desynced");

        // Resync from the ring restores the exact correct grid.
        desynced.on_event(&Event::TerminalResync {
            terminal_id: TerminalId(1),
            replay: full.to_vec(),
            seq: 9,
        });
        assert_eq!(row(&mut desynced, ROW0), want0);
        assert_eq!(row(&mut desynced, ROW1), want1);

        // …and the consumer adopts the ring's seq so the resumed live
        // stream (all seq > 9) applies on top exactly once.
        assert_eq!(desynced.terminals[&TerminalId(1)].last_seq, 9);
    }

    #[test]
    fn resync_for_unknown_terminal_is_a_noop() {
        let sk = SessionKey::new("s");
        let mut stack = shell_stack(TerminalId(1), &sk);
        // Different id — must not panic or create a phantom slot.
        stack.on_event(&Event::TerminalResync {
            terminal_id: TerminalId(99),
            replay: b"whatever".to_vec(),
            seq: 5,
        });
        assert!(!stack.terminals.contains_key(&TerminalId(99)));
    }

    #[test]
    fn client_sequence_gap_quarantines_output_until_authoritative_resync() {
        let sk = SessionKey::new("s");
        let id = TerminalId(1);
        let mut stack = shell_stack(id, &sk);
        stack.on_event(&Event::TerminalOutput {
            terminal_id: id,
            bytes: b"stable".to_vec(),
            first_seq: 1,
            seq: 1,
        });

        // Chunk 2 vanished somewhere below the daemon's normal recovery
        // machinery. Defense in depth: neither chunk 3 nor later output
        // may mutate the coherent prefix.
        stack.on_event(&Event::TerminalOutput {
            terminal_id: id,
            bytes: b"-torn".to_vec(),
            first_seq: 3,
            seq: 3,
        });
        assert_eq!(
            stack.drain_pending_resync_requests(),
            vec![TerminalResyncRequest {
                terminal_id: id,
                required_seq: 3,
            }]
        );

        // The first request is now in flight. Output 4 raises the recovery
        // debt but does not enqueue a duplicate request while that reply is
        // outstanding.
        stack.on_event(&Event::TerminalOutput {
            terminal_id: id,
            bytes: b"-still-torn".to_vec(),
            first_seq: 4,
            seq: 4,
        });
        let slot = &stack.terminals[&id];
        assert!(slot.sync.is_desynced());
        assert_eq!(slot.last_seq, 1);
        assert_eq!(slot.recent, b"stable");
        assert!(stack.drain_pending_resync_requests().is_empty());

        // The in-flight reply only covers its original debt. It cannot clear
        // the newer watermark; receiving it must immediately queue one retry
        // through sequence 4 even if the terminal goes quiet now.
        stack.on_event(&Event::TerminalResync {
            terminal_id: id,
            replay: b"still-stale".to_vec(),
            seq: 3,
        });
        assert_eq!(stack.terminals[&id].last_seq, 1);
        assert_eq!(stack.terminals[&id].recent, b"stable");
        assert_eq!(
            stack.drain_pending_resync_requests(),
            vec![TerminalResyncRequest {
                terminal_id: id,
                required_seq: 4,
            }]
        );

        stack.on_event(&Event::TerminalResync {
            terminal_id: id,
            replay: b"recovered".to_vec(),
            seq: 4,
        });
        let slot = &stack.terminals[&id];
        assert!(!slot.sync.is_desynced());
        assert_eq!(slot.last_seq, 4);
        assert_eq!(slot.recent, b"recovered");

        stack.on_event(&Event::TerminalOutput {
            terminal_id: id,
            bytes: b"!".to_vec(),
            first_seq: 5,
            seq: 5,
        });
        assert_eq!(stack.terminals[&id].recent, b"recovered!");
    }

    /// #1254 finding 2: after the daemon answers `TerminalResyncUnavailable`,
    /// a pane whose agent has FINISHED produces no further output — and
    /// output used to be the only thing that re-drove the request, so the
    /// pane froze forever. The tick loop must re-issue the request on a
    /// bounded backoff until an authoritative replay converges the grid.
    #[test]
    fn quiescent_terminal_recovers_after_resync_unavailable() {
        let sk = SessionKey::new("s");
        let id = TerminalId(1);
        let mut stack = shell_stack(id, &sk);
        stack.on_event(&Event::TerminalOutput {
            terminal_id: id,
            bytes: b"ok".to_vec(),
            first_seq: 1,
            seq: 1,
        });
        // A gap desyncs the pane and sends one request.
        stack.on_event(&Event::TerminalOutput {
            terminal_id: id,
            bytes: b"torn".to_vec(),
            first_seq: 3,
            seq: 3,
        });
        let request = TerminalResyncRequest {
            terminal_id: id,
            required_seq: 3,
        };
        assert_eq!(stack.drain_pending_resync_requests(), vec![request]);

        // The daemon can't serve it right now — and no further output
        // will EVER arrive on this terminal.
        stack.on_event(&Event::TerminalResyncUnavailable { terminal_id: id });
        let now = std::time::Instant::now();
        // Inside the backoff window nothing is re-issued (no hammering)...
        stack.tick_resync_retries(now);
        assert!(
            stack.drain_pending_resync_requests().is_empty(),
            "the backoff window paces the retry"
        );
        // ...but ticks alone — zero new output — re-drive the request.
        stack.tick_resync_retries(now + std::time::Duration::from_secs(31));
        assert_eq!(
            stack.drain_pending_resync_requests(),
            vec![request],
            "the tick loop must re-drive a quiescent desynced pane"
        );

        // A second unavailable reply keeps the loop alive: retries slow
        // down (backoff doubles, capped) but never stop.
        stack.on_event(&Event::TerminalResyncUnavailable { terminal_id: id });
        stack.tick_resync_retries(now + std::time::Duration::from_secs(120));
        assert_eq!(
            stack.drain_pending_resync_requests(),
            vec![request],
            "retries continue for as long as recovery keeps failing"
        );

        // The authoritative replay finally lands and converges the pane.
        stack.on_event(&Event::TerminalResync {
            terminal_id: id,
            replay: b"ok-then-torn".to_vec(),
            seq: 3,
        });
        let slot = &stack.terminals[&id];
        assert!(!slot.sync.is_desynced(), "the pane converged");
        assert!(slot.resync_retry_at.is_none(), "the retry loop disarmed");
        assert_eq!(
            slot.resync_retry_backoff, RESYNC_RETRY_INITIAL,
            "the next episode starts from the fast backoff again"
        );
    }

    /// #1254 finding 7 (second half): `replace_terminal`'s slot-reuse
    /// branch used to mutate slot identity while keeping the old grid,
    /// frame cache, and sequence watermark — a permanently stale pane.
    /// The reused slot must start from a clean grid, quarantined until
    /// the daemon's authoritative replay describes the new stream.
    #[test]
    fn replace_terminal_slot_reuse_starts_from_a_clean_grid() {
        let sk = SessionKey::new("s");
        let id = TerminalId(2);
        let mut stack = shell_stack(id, &sk);
        stack.on_event(&Event::TerminalOutput {
            terminal_id: id,
            bytes: b"stale\r\n".to_vec(),
            first_seq: 1,
            seq: 5,
        });
        assert_eq!(
            row(&mut stack, ROW0),
            "stale",
            "precondition: prior content"
        );
        {
            let slot = stack.terminals.get_mut(&id).expect("slot");
            slot.last_frame = Some(ratatui::buffer::Buffer::empty(Rect::new(0, 0, 10, 2)));
            slot.last_frame_rev = Some((slot.vt.content_rev, Rect::new(0, 0, 10, 2)));
        }

        // The daemon replaces a terminal this client never saw the old
        // id of — the slot-reuse branch.
        stack.on_event(&Event::TerminalReplaced {
            old_terminal_id: TerminalId(99),
            terminal_id: id,
            session_key: sk.clone(),
            kind: TerminalKind::Shell,
            no_permission: false,
            on_main: false,
            model_label: None,
            authenticating: false,
        });

        let slot = &stack.terminals[&id];
        assert!(
            slot.last_frame.is_none() && slot.last_frame_rev.is_none(),
            "a stale cached frame must never blit for the new stream"
        );
        assert_eq!(
            slot.last_seq, 0,
            "the watermark restarts with the new stream"
        );
        assert!(
            slot.sync.is_desynced(),
            "the clean grid is quarantined until the authoritative replay"
        );
        assert_eq!(
            stack.drain_pending_resync_requests(),
            vec![TerminalResyncRequest {
                terminal_id: id,
                required_seq: 0,
            }],
            "the replacement immediately requests the daemon's truth"
        );
        assert_eq!(row(&mut stack, ROW0), "", "the old grid's content is gone");
    }

    #[test]
    fn unavailable_recovery_snapshot_preserves_existing_grid_and_sequence() {
        let sk = SessionKey::new("s");
        let id = TerminalId(1);
        let mut stack = shell_stack(id, &sk);
        stack.on_event(&Event::TerminalOutput {
            terminal_id: id,
            bytes: b"known-good".to_vec(),
            first_seq: 1,
            seq: 1,
        });

        stack.on_event(&Event::Snapshot {
            workspaces: Vec::new(),
            projects: Vec::new(),
            terminals: vec![lazybox_ipc::TerminalSnapshot {
                terminal_id: id,
                session_key: sk,
                kind: TerminalKind::Shell,
                replay: Vec::new(),
                last_seq: 0,
                replay_available: false,
                no_permission: false,
                on_main: false,
                model_label: None,
                prompt_history: Vec::new(),
                composing_buffer: None,
                agent_state: None,
                authenticating: false,
            }],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });
        let slot = &stack.terminals[&id];
        assert!(slot.sync.is_desynced());
        assert_eq!(slot.last_seq, 1, "failed snapshot cannot lower coverage");
        assert_eq!(slot.recent, b"known-good", "screen state is preserved");
        assert_eq!(
            stack.drain_pending_resync_requests(),
            vec![TerminalResyncRequest {
                terminal_id: id,
                required_seq: 1,
            }]
        );

        stack.on_event(&Event::TerminalOutput {
            terminal_id: id,
            bytes: b"ignored".to_vec(),
            first_seq: 2,
            seq: 2,
        });
        assert_eq!(stack.terminals[&id].recent, b"known-good");
        assert!(
            stack.drain_pending_resync_requests().is_empty(),
            "the reconnect repair already owns this recovery episode"
        );
    }

    #[test]
    fn local_vt_reset_failure_releases_latch_and_retries_immediately() {
        let sk = SessionKey::new("s");
        let id = TerminalId(1);
        let mut stack = shell_stack(id, &sk);
        stack.on_event(&Event::TerminalOutput {
            terminal_id: id,
            bytes: b"stable".to_vec(),
            first_seq: 1,
            seq: 1,
        });
        stack
            .terminals
            .get_mut(&id)
            .expect("slot")
            .vt
            .fail_next_reset = true;

        stack.on_event(&Event::TerminalResync {
            terminal_id: id,
            replay: b"authoritative".to_vec(),
            seq: 4,
        });

        let slot = &stack.terminals[&id];
        assert!(slot.sync.is_desynced());
        assert_eq!(slot.last_seq, 1, "failed local reset cannot claim coverage");
        assert_eq!(slot.recent, b"stable", "last coherent grid is preserved");
        assert_eq!(
            stack.drain_pending_resync_requests(),
            vec![TerminalResyncRequest {
                terminal_id: id,
                required_seq: 4,
            }]
        );
    }

    #[test]
    fn stale_resync_cannot_roll_back_a_coherent_terminal() {
        let sk = SessionKey::new("s");
        let id = TerminalId(1);
        let mut stack = shell_stack(id, &sk);
        stack.on_event(&Event::TerminalOutput {
            terminal_id: id,
            bytes: b"new".to_vec(),
            first_seq: 1,
            seq: 1,
        });
        stack.on_event(&Event::TerminalResync {
            terminal_id: id,
            replay: b"old".to_vec(),
            seq: 1,
        });
        assert_eq!(stack.terminals[&id].recent, b"new");
    }
}

#[cfg(test)]
mod deep_scrollback_tests {
    //! Live sessions share the restart path's tmux-history scrollback
    //! (#393): the first upward scroll arms a `Command::FetchScrollback`
    //! and the `Event::TerminalScrollback` reply rebuilds the grid with
    //! the backend's full retained history — while preserving the
    //! terminal modes and the user's viewport position, which the
    //! content-only capture cannot carry itself.
    use super::*;

    /// Serializes the two tests that pin a specific process-global
    /// `CLIENT_SCROLLBACK_LINES` (`apply_ui_defaults` stores it) and then
    /// build a VT that reads it. Under `cargo test`'s parallelism
    /// `wide_pane`'s `8_000` could otherwise land between
    /// `raised_scrollback`'s `30_000` store and its VT spawn, clipping the
    /// 15k capture to ~8k — the flake mis-read as a macOS libghostty bug
    /// (#1108). No other `apply_ui_defaults` caller stores below the feed
    /// size, so a lock shared by just these two is sufficient.
    static SCROLLBACK_GLOBAL_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn agent_stack(id: TerminalId, sk: &SessionKey) -> TerminalStack {
        let mut stack = TerminalStack::new(PaneId::new(0));
        stack.on_event(&Event::TerminalSpawned {
            model_label: None,
            terminal_id: id,
            session_key: sk.clone(),
            kind: TerminalKind::Agent("claude".into()),
            no_permission: false,
            on_main: false,
        });
        stack.set_active_session(Some(sk.clone()));
        stack
    }

    /// Feed one coalesced output event covering chunks
    /// `first_seq..=seq` (a single chunk when they're equal). Chunks
    /// must be contiguous with the slot's `last_seq` or the gap
    /// machinery freezes the grid awaiting a resync.
    fn feed(stack: &mut TerminalStack, id: TerminalId, bytes: &[u8], first_seq: u64, seq: u64) {
        stack.on_event(&Event::TerminalOutput {
            terminal_id: id,
            bytes: bytes.to_vec(),
            first_seq,
            seq,
        });
    }

    /// A capture-shaped payload: `lines` CRLF-joined history lines and
    /// a final line without a trailing newline (`normalize_capture`
    /// parks the cursor at the end of the last line).
    fn deep_history(lines: usize) -> Vec<u8> {
        let mut payload = String::new();
        for i in 0..lines {
            payload.push_str(&format!("history line {i}\r\n"));
        }
        payload.push_str("live bottom");
        payload.into_bytes()
    }

    fn scrollbar(stack: &TerminalStack, id: TerminalId) -> vt::terminal::Scrollbar {
        stack.terminals[&id].vt.terminal.scrollbar().unwrap()
    }

    fn mode(stack: &TerminalStack, id: TerminalId, mode: vt::terminal::Mode) -> bool {
        stack.terminals[&id].vt.terminal.mode(mode).unwrap()
    }

    /// The trigger fires even when the local grid has NO scrollback yet
    /// — that's exactly the live-agent case the fetch exists for — and
    /// fires once per visit, not once per wheel notch.
    #[test]
    fn scroll_up_arms_one_fetch_per_visit() {
        let sk = SessionKey::new("s");
        let mut stack = agent_stack(TerminalId(1), &sk);
        feed(&mut stack, TerminalId(1), b"a couple\r\nof lines", 1, 1);

        let _ = stack.scroll_active(-3);
        assert_eq!(stack.take_scrollback_fetch(), Some(TerminalId(1)));
        let _ = stack.scroll_active(-3);
        assert_eq!(
            stack.take_scrollback_fetch(),
            None,
            "still in the same scrollback visit — no second fetch"
        );

        // Scrolling back down to the live bottom ends the visit; the
        // next upward scroll re-fetches so its history is current.
        let _ = stack.scroll_active(3);
        let _ = stack.scroll_active(-1);
        assert_eq!(stack.take_scrollback_fetch(), Some(TerminalId(1)));
    }

    /// `Shift-PageUp` through the real key handler ships the command.
    #[test]
    fn shift_pageup_ships_the_fetch_command() {
        let sk = SessionKey::new("s");
        let mut stack = agent_stack(TerminalId(7), &sk);
        feed(&mut stack, TerminalId(7), b"some output", 1, 1);

        let mut cmds = Vec::new();
        stack.handle_key(
            KeyEvent::new(KeyCode::PageUp, KeyModifiers::SHIFT),
            &mut cmds,
        );
        assert!(
            matches!(
                cmds.as_slice(),
                [Command::FetchScrollback {
                    terminal_id: TerminalId(7)
                }]
            ),
            "expected a single FetchScrollback, got {cmds:?}"
        );
    }

    /// A deep-scrollback capture larger than the old hardcoded 10k client
    /// VT cap must be retained in full once `terminal.scrollback_lines` is
    /// raised (#857) — otherwise the client silently clips everything past
    /// the last 10k lines and the deeper tmux history the daemon fetched
    /// never becomes scrollable. The VT reads the depth at creation, so
    /// `apply_ui_defaults` runs before the terminal is spawned.
    ///
    /// Holds [`SCROLLBACK_GLOBAL_LOCK`] across the whole test: the depth is
    /// a process-global, so a concurrent `wide_pane` store of `8_000`
    /// between the `30_000` store here and the spawn below would clip the
    /// capture to ~8k (the flake filed as #1108).
    #[test]
    fn raised_scrollback_lines_lets_client_retain_beyond_old_cap() {
        let _serial = SCROLLBACK_GLOBAL_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let sk = SessionKey::new("s");
        let mut stack = TerminalStack::new(PaneId::new(0));
        stack.apply_ui_defaults(&lazybox_config::UiDefaults {
            scrollback_lines: 30_000,
            ..Default::default()
        });
        stack.on_event(&Event::TerminalSpawned {
            model_label: None,
            terminal_id: TerminalId(1),
            session_key: sk.clone(),
            kind: TerminalKind::Agent("claude".into()),
            no_permission: false,
            on_main: false,
        });
        stack.set_active_session(Some(sk.clone()));

        stack.on_event(&Event::TerminalScrollback {
            terminal_id: TerminalId(1),
            replay: deep_history(15_000),
            seq: 9,
        });

        let after = scrollbar(&stack, TerminalId(1));
        assert!(
            after.total > 12_000,
            "client VT must retain the deep capture in full, not clip it \
             to the old 10k cap: {after:?}"
        );
    }

    /// libghostty caps scrollback by BYTES and a row's cost grows with the
    /// terminal's width, so a too-small per-line byte budget clips a WIDE
    /// pane to a fraction of the configured line depth (#857). A pane far
    /// wider than the default must still hold ~all of the configured lines.
    #[test]
    fn wide_pane_retains_the_configured_line_depth() {
        let _serial = SCROLLBACK_GLOBAL_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let sk = SessionKey::new("s");
        let mut stack = TerminalStack::new(PaneId::new(0));
        stack.apply_ui_defaults(&lazybox_config::UiDefaults {
            scrollback_lines: 8_000,
            ..Default::default()
        });
        stack.on_event(&Event::TerminalSpawned {
            model_label: None,
            terminal_id: TerminalId(1),
            session_key: sk.clone(),
            kind: TerminalKind::Agent("claude".into()),
            no_permission: false,
            on_main: false,
        });
        stack.set_active_session(Some(sk.clone()));
        // Widen the pane well past the default 120 cols; the deep-scrollback
        // rebuild sizes its parser to the slot's current width.
        stack
            .terminals
            .get_mut(&TerminalId(1))
            .expect("slot")
            .vt
            .ensure_size(220, 24);

        stack.on_event(&Event::TerminalScrollback {
            terminal_id: TerminalId(1),
            replay: deep_history(8_000),
            seq: 9,
        });

        let after = scrollbar(&stack, TerminalId(1));
        assert!(
            after.total >= 7_900,
            "a wide pane must still hold ~all configured lines, not a \
             width-clipped fraction: {after:?}"
        );
    }

    #[test]
    fn apply_scrollback_deepens_history_and_adopts_seq() {
        let sk = SessionKey::new("s");
        let mut stack = agent_stack(TerminalId(1), &sk);
        feed(&mut stack, TerminalId(1), b"shallow", 1, 3);
        let before = scrollbar(&stack, TerminalId(1));
        assert!(before.total <= before.len, "precondition: no scrollback");

        stack.on_event(&Event::TerminalScrollback {
            terminal_id: TerminalId(1),
            replay: deep_history(200),
            seq: 9,
        });
        let after = scrollbar(&stack, TerminalId(1));
        assert!(
            after.total > after.len,
            "capture must open real scrollback: {after:?}"
        );
        assert_eq!(stack.terminals[&TerminalId(1)].last_seq, 9);
    }

    /// The user-reported shape of the #393 follow-up regression: "the
    /// scroll bar disappears as soon as I start scrolling". The pane
    /// sat on the alternate screen (Claude ≥2.1 under a pre-fix server
    /// config), so tmux retained zero history and the capture was ~one
    /// screenful — and adopting it REPLACED the client's deeper local
    /// grid, wiping the scrollback and the scrollbar with it. A rebuild
    /// that is not strictly deeper than the current grid must be a
    /// no-op.
    #[test]
    fn shallow_capture_never_shrinks_the_grid() {
        let sk = SessionKey::new("s");
        let mut stack = agent_stack(TerminalId(1), &sk);
        let mut lines = String::new();
        for i in 0..120 {
            lines.push_str(&format!("local line {i}\r\n"));
        }
        feed(&mut stack, TerminalId(1), lines.as_bytes(), 1, 5);
        let before = scrollbar(&stack, TerminalId(1));
        assert!(before.total > before.len, "precondition: deep local grid");

        stack.on_event(&Event::TerminalScrollback {
            terminal_id: TerminalId(1),
            replay: deep_history(3),
            seq: 9,
        });

        let after = scrollbar(&stack, TerminalId(1));
        assert_eq!(
            after.total, before.total,
            "a one-screen capture must never replace a deeper grid \
             (this is the disappearing-scrollbar regression)"
        );
        assert!(after.total > after.len, "scrollback (and its bar) survive");
        assert_eq!(
            stack.terminals[&TerminalId(1)].last_seq,
            5,
            "a skipped rebuild adopts nothing"
        );
    }

    /// The capture is content-only, so the modes the inner program had
    /// enabled must survive the rebuild — losing mouse tracking here
    /// would silently break click forwarding for the rest of the
    /// session (tmux never re-asserts a mode it believes is still set).
    #[test]
    fn apply_scrollback_preserves_dec_modes() {
        let sk = SessionKey::new("s");
        let mut stack = agent_stack(TerminalId(1), &sk);
        feed(
            &mut stack,
            TerminalId(1),
            b"\x1b[?1002h\x1b[?1006h\x1b[?2004h\x1b[?25lagent screen",
            1,
            1,
        );
        assert!(stack.focused_terminal_tracks_mouse(), "precondition");

        stack.on_event(&Event::TerminalScrollback {
            terminal_id: TerminalId(1),
            replay: deep_history(100),
            seq: 2,
        });
        assert!(stack.focused_terminal_tracks_mouse());
        assert!(mode(
            &stack,
            TerminalId(1),
            vt::terminal::Mode::BUTTON_MOUSE
        ));
        assert!(mode(&stack, TerminalId(1), vt::terminal::Mode::SGR_MOUSE));
        assert!(mode(
            &stack,
            TerminalId(1),
            vt::terminal::Mode::BRACKETED_PASTE
        ));
        assert!(!mode(
            &stack,
            TerminalId(1),
            vt::terminal::Mode::CURSOR_VISIBLE
        ));
        // A mode that was never set stays off.
        assert!(!mode(&stack, TerminalId(1), vt::terminal::Mode::ANY_MOUSE));
    }

    /// Grapheme clustering (DEC 2027) is set once at program startup
    /// and shapes every subsequent wide-char cell layout — of all the
    /// preserved modes it's the one whose loss would silently skew
    /// rendering for the rest of the session.
    #[test]
    fn apply_scrollback_preserves_grapheme_clustering() {
        let sk = SessionKey::new("s");
        let mut stack = agent_stack(TerminalId(1), &sk);
        feed(&mut stack, TerminalId(1), b"\x1b[?2027hagent screen", 1, 1);
        assert!(
            mode(&stack, TerminalId(1), vt::terminal::Mode::GRAPHEME_CLUSTER),
            "precondition: the inner program enabled mode 2027"
        );

        stack.on_event(&Event::TerminalScrollback {
            terminal_id: TerminalId(1),
            replay: deep_history(100),
            seq: 2,
        });
        assert!(mode(
            &stack,
            TerminalId(1),
            vt::terminal::Mode::GRAPHEME_CLUSTER
        ));
    }

    /// A gap resync rebuilds the grid from the RING — replacing any
    /// capture-fed deep scrollback with the raw stream's shallow one —
    /// so it must also release the per-visit latch: without that, the
    /// user's next scroll-up moved a silently emptied grid and no
    /// re-fetch fired until they bounced off the live bottom.
    #[test]
    fn gap_resync_rearms_the_fetch() {
        let sk = SessionKey::new("s");
        let mut stack = agent_stack(TerminalId(1), &sk);
        feed(&mut stack, TerminalId(1), b"some output", 1, 1);

        // First visit: scroll up, fetch armed and taken.
        let _ = stack.scroll_active(-3);
        assert_eq!(stack.take_scrollback_fetch(), Some(TerminalId(1)));
        let _ = stack.scroll_active(-3);
        assert_eq!(stack.take_scrollback_fetch(), None, "latch held mid-visit");

        // A dropped-chunk resync replaces the grid from the ring.
        stack.on_event(&Event::TerminalResync {
            terminal_id: TerminalId(1),
            replay: b"ring replay".to_vec(),
            seq: 5,
        });

        // The rebuilt grid has no deep history — the next upward scroll
        // must fetch again instead of riding the spent latch.
        let _ = stack.scroll_active(-3);
        assert_eq!(stack.take_scrollback_fetch(), Some(TerminalId(1)));
    }

    /// The fetch fires mid-scroll, so the rebuild must keep the
    /// viewport where the user parked it (anchored to the bottom, since
    /// the new history grows above) instead of snapping to the live
    /// tail.
    #[test]
    fn apply_scrollback_keeps_viewport_distance_from_bottom() {
        let sk = SessionKey::new("s");
        let mut stack = agent_stack(TerminalId(1), &sk);
        feed(&mut stack, TerminalId(1), &deep_history(60), 1, 1);
        let _ = stack.scroll_active(-5);
        let before = scrollbar(&stack, TerminalId(1));
        let dist = before.total - before.offset - before.len;
        assert_eq!(dist, 5, "precondition: parked 5 rows above the bottom");

        stack.on_event(&Event::TerminalScrollback {
            terminal_id: TerminalId(1),
            replay: deep_history(300),
            seq: 2,
        });
        let after = scrollbar(&stack, TerminalId(1));
        assert_eq!(
            after.total - after.offset - after.len,
            5,
            "viewport must stay anchored to the bottom: {after:?}"
        );
    }

    /// Deep history is the scrolling source of truth, so adopting it
    /// also brings an unexpected alternate-screen client back to the
    /// history-bearing primary screen.
    #[test]
    fn apply_scrollback_normalizes_alt_screen_to_history() {
        let sk = SessionKey::new("s");
        let mut stack = agent_stack(TerminalId(1), &sk);
        feed(&mut stack, TerminalId(1), b"\x1b[?1049hvim screen", 1, 1);

        stack.on_event(&Event::TerminalScrollback {
            terminal_id: TerminalId(1),
            replay: deep_history(100),
            seq: 9,
        });
        assert!(!mode(
            &stack,
            TerminalId(1),
            vt::terminal::Mode::ALT_SCREEN_SAVE
        ));
        assert_eq!(
            stack.terminals[&TerminalId(1)].last_seq,
            9,
            "the adopted history advances the seq high-water mark"
        );
        let bar = scrollbar(&stack, TerminalId(1));
        assert!(bar.total > bar.len, "the capture provides local scrollback");
    }

    /// A capture that raced live output (its seq lags what the client
    /// already applied) still rebuilds the grid but never rewinds the
    /// high-water mark — that would double-feed the newer chunks.
    #[test]
    fn apply_scrollback_never_rewinds_last_seq() {
        let sk = SessionKey::new("s");
        let mut stack = agent_stack(TerminalId(1), &sk);
        feed(&mut stack, TerminalId(1), b"newer output", 1, 9);

        stack.on_event(&Event::TerminalScrollback {
            terminal_id: TerminalId(1),
            replay: deep_history(100),
            seq: 5,
        });
        assert_eq!(stack.terminals[&TerminalId(1)].last_seq, 9);
        let after = scrollbar(&stack, TerminalId(1));
        assert!(after.total > after.len, "grid still rebuilt: {after:?}");
    }
}

#[cfg(test)]
mod hidden_feed_tests {
    //! Off-screen terminals must not pay the VT-parse cost per chunk.
    //! Output that arrives while a terminal isn't displayed is buffered
    //! raw and replayed into the parser lazily on the first render after
    //! it becomes visible — the resulting grid must match a terminal
    //! that received the same bytes while on screen.
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

    const W: u16 = 80;
    const H: u16 = 24;
    const ROW0: (u16, u16) = (1, 3);

    fn render(stack: &mut TerminalStack) {
        let _ = screen_rows(stack);
    }

    /// Render the pane to a test backend and return the trimmed screen
    /// rows. Doubles as the "force a render" path the lazy flush hooks
    /// into.
    fn screen_rows(stack: &mut TerminalStack) -> Vec<String> {
        let backend = TestBackend::new(W, H);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| stack.render(Rect::new(0, 0, W, H), f, true))
            .unwrap();
        let buf = term.backend().buffer().clone();
        (0..H)
            .map(|y| {
                (0..W)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn spawn(stack: &mut TerminalStack, id: TerminalId, sk: &SessionKey) {
        stack.on_event(&Event::TerminalSpawned {
            model_label: None,
            terminal_id: id,
            session_key: sk.clone(),
            kind: TerminalKind::Shell,
            no_permission: false,
            on_main: false,
        });
    }

    fn feed(stack: &mut TerminalStack, id: TerminalId, bytes: &[u8], seq: u64) {
        stack.on_event(&Event::TerminalOutput {
            terminal_id: id,
            bytes: bytes.to_vec(),
            first_seq: seq,
            seq,
        });
    }

    fn row0(stack: &mut TerminalStack) -> String {
        let rect = Rect::new(0, 0, W, H);
        let id = stack.focused_terminal_id().expect("focused");
        let a = stack
            .selection_point(id, rect, ROW0.0, ROW0.1)
            .expect("anchor");
        let b = stack.selection_point(id, rect, 20, ROW0.1).expect("focus");
        stack.extract_selection(id, a, b)
    }

    /// Two sessions, one terminal each; A is active. After a render, A's
    /// terminal feeds eagerly while B's buffers — and B's deferred replay
    /// reconstructs the exact same grid A would show.
    #[test]
    fn hidden_terminal_buffers_then_replays_to_match_visible() {
        let sk_a = SessionKey::new("a");
        let sk_b = SessionKey::new("b");
        let mut stack = TerminalStack::new(PaneId::new(0));
        spawn(&mut stack, TerminalId(1), &sk_a);
        spawn(&mut stack, TerminalId(2), &sk_b);
        stack.set_active_session(Some(sk_a.clone()));
        render(&mut stack);

        // A is on screen → fed immediately, nothing buffered.
        feed(&mut stack, TerminalId(1), b"hello\r\n", 1);
        assert!(stack.terminals[&TerminalId(1)].displayed);
        assert!(stack.terminals[&TerminalId(1)].pending_feed.is_empty());

        // B is off screen → byte-for-byte buffered, parser untouched.
        feed(&mut stack, TerminalId(2), b"hello\r\n", 1);
        assert!(!stack.terminals[&TerminalId(2)].displayed);
        assert_eq!(stack.terminals[&TerminalId(2)].pending_feed, b"hello\r\n");

        // Bring B on screen. The render flushes the buffer (asserted
        // before extract_text, which would itself flush).
        stack.set_active_session(Some(sk_b.clone()));
        render(&mut stack);
        assert!(stack.terminals[&TerminalId(2)].pending_feed.is_empty());

        // Same grid as the eagerly-fed sibling.
        stack.set_active_session(Some(sk_a));
        assert_eq!(row0(&mut stack), "hello");
        stack.set_active_session(Some(sk_b));
        assert_eq!(row0(&mut stack), "hello");
    }

    /// A hidden buffer crossing `PENDING_FEED_CAP` flushes its complete
    /// ordered prefix into the existing parser instead of dropping bytes
    /// and resetting from an arbitrary tail.
    #[test]
    fn overflowing_hidden_buffer_preserves_stream_in_bounded_batches() {
        let sk_a = SessionKey::new("a");
        let sk_b = SessionKey::new("b");
        let mut stack = TerminalStack::new(PaneId::new(0));
        spawn(&mut stack, TerminalId(1), &sk_a);
        spawn(&mut stack, TerminalId(2), &sk_b);
        stack.set_active_session(Some(sk_a));
        render(&mut stack);

        // Overrun the cap with filler, then a final line that must end up
        // on screen at the live bottom.
        let filler = vec![b'x'; PENDING_FEED_CAP];
        feed(&mut stack, TerminalId(2), &filler, 1);
        feed(&mut stack, TerminalId(2), b"\r\nlast line", 2);
        let slot = &stack.terminals[&TerminalId(2)];
        assert!(slot.pending_feed.len() <= PENDING_FEED_CAP);
        assert_eq!(slot.pending_feed, b"\r\nlast line");

        stack.set_active_session(Some(sk_b));
        let rows = screen_rows(&mut stack);
        let slot = &stack.terminals[&TerminalId(2)];
        assert!(slot.pending_feed.is_empty());
        // Both the flushed prefix and later batch were fed in order.
        assert!(
            rows.iter().any(|r| r.contains("last line")),
            "expected the post-truncation tail on screen, got {rows:?}"
        );
    }

    /// A reconnect `Snapshot` must not pay the VT parse for every
    /// terminal synchronously on the UI thread: only the foreground
    /// terminal is fed eagerly; hidden terminals stash their replay in
    /// `pending_feed` and reconstruct the exact grid on first display.
    /// The foreground terminal is fed eagerly only once its render width
    /// is known (it rendered before the reconnect) — flushing a fresh
    /// slot would parse the replay at the VT default and reflow it, the
    /// scrollback-corrupting path #1405 guards against.
    #[test]
    fn snapshot_defers_hidden_terminal_replays() {
        let sk_a = SessionKey::new("a");
        let sk_b = SessionKey::new("b");
        let mut stack = TerminalStack::new(PaneId::new(0));
        stack.set_active_session(Some(sk_a.clone()));
        // The foreground terminal already rendered once — the reconnect
        // precondition that lets its replay flush eagerly at the real width.
        spawn(&mut stack, TerminalId(1), &sk_a);
        render(&mut stack);

        let snap = |id: u64, sk: &SessionKey, replay: &[u8]| lazybox_ipc::TerminalSnapshot {
            terminal_id: TerminalId(id),
            session_key: sk.clone(),
            kind: TerminalKind::Shell,
            replay: replay.to_vec(),
            last_seq: 1,
            replay_available: true,
            no_permission: false,
            on_main: false,
            model_label: None,
            prompt_history: Vec::new(),
            composing_buffer: None,
            agent_state: None,
            authenticating: false,
        };
        stack.on_event(&Event::Snapshot {
            workspaces: vec![],
            terminals: vec![
                snap(1, &sk_a, b"visible\r\n"),
                snap(2, &sk_b, b"hidden\r\n"),
            ],
            projects: vec![],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });

        // Foreground terminal: parsed eagerly (the mouse/alt-screen
        // readers consult its parser before any render).
        assert!(
            stack.terminals[&TerminalId(1)].pending_feed.is_empty(),
            "the focused terminal's replay must be fed eagerly"
        );
        // Background terminal: replay stashed, parser untouched.
        assert_eq!(
            stack.terminals[&TerminalId(2)].pending_feed,
            b"hidden\r\n",
            "a hidden terminal's replay must be deferred, not parsed in the dispatch"
        );

        // Live output arriving before the first display appends behind
        // the stashed replay in stream order.
        feed(&mut stack, TerminalId(2), b"more\r\n", 2);
        assert_eq!(
            stack.terminals[&TerminalId(2)].pending_feed,
            b"hidden\r\nmore\r\n"
        );

        // First display of the hidden terminal reconstructs the grid
        // from replay + live tail.
        stack.set_active_session(Some(sk_b));
        let rows = screen_rows(&mut stack);
        assert!(stack.terminals[&TerminalId(2)].pending_feed.is_empty());
        assert!(
            rows.iter().any(|r| r.contains("hidden")) && rows.iter().any(|r| r.contains("more")),
            "deferred replay must render the full stream, got {rows:?}"
        );

        // And the eagerly-fed foreground grid was correct all along.
        stack.set_active_session(Some(sk_a));
        assert_eq!(row0(&mut stack), "visible");
    }

    /// The focused terminal's replay is flushed eagerly so the `&self`
    /// mouse-tracking / alt-screen probes — which the mouse handler
    /// consults on input dispatched before the first render — reflect the
    /// reattached stream's terminal modes immediately. A deferred flush
    /// would leave those probes reading the VT default (mouse tracking
    /// off), and a click into a reattached mouse-tracking agent would be
    /// swallowed as a lazybox selection instead of forwarded.
    #[test]
    fn snapshot_eager_flush_exposes_focused_terminal_modes_before_render() {
        let sk = SessionKey::new("a");
        let mut stack = TerminalStack::new(PaneId::new(0));
        stack.set_active_session(Some(sk.clone()));
        // Rendered once before the reconnect, so the pane width is known.
        spawn(&mut stack, TerminalId(1), &sk);
        render(&mut stack);

        stack.on_event(&Event::Snapshot {
            workspaces: vec![],
            terminals: vec![lazybox_ipc::TerminalSnapshot {
                terminal_id: TerminalId(1),
                session_key: sk.clone(),
                kind: TerminalKind::Agent("claude".into()),
                // SGR mouse tracking on (`?1002h`/`?1006h`), as vim/Claude do.
                replay: b"\x1b[?1002h\x1b[?1006hediting\r\n".to_vec(),
                last_seq: 1,
                replay_available: true,
                no_permission: false,
                on_main: false,
                model_label: None,
                prompt_history: Vec::new(),
                composing_buffer: None,
                agent_state: None,
                authenticating: false,
            }],
            projects: vec![],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });

        assert!(
            stack.terminals[&TerminalId(1)].pending_feed.is_empty(),
            "the focused terminal's replay must flush eagerly, not defer"
        );
        assert!(
            stack.terminal_tracks_mouse(TerminalId(1)),
            "mouse tracking from the reconnect replay must be visible to the \
             &self probe before the first render"
        );
    }

    #[test]
    fn reattached_agent_keeps_composer_text_on_the_prompt_row_after_refocus() {
        let agent_key = SessionKey::new("agent");
        let shell_key = SessionKey::new("shell");
        let mut stack = TerminalStack::new(PaneId::new(0));
        stack.set_active_session(Some(agent_key.clone()));
        let snapshot = |id: u64, session_key: &SessionKey, kind: TerminalKind, replay: &[u8]| {
            lazybox_ipc::TerminalSnapshot {
                terminal_id: TerminalId(id),
                session_key: session_key.clone(),
                kind,
                replay: replay.to_vec(),
                last_seq: 1,
                replay_available: true,
                no_permission: false,
                on_main: false,
                model_label: None,
                prompt_history: Vec::new(),
                composing_buffer: None,
                agent_state: None,
                authenticating: false,
            }
        };
        stack.on_event(&Event::Snapshot {
            workspaces: vec![],
            terminals: vec![
                snapshot(
                    1,
                    &agent_key,
                    TerminalKind::Agent("claude".into()),
                    "❯ add an issue:".as_bytes(),
                ),
                snapshot(2, &shell_key, TerminalKind::Shell, b"shell prompt"),
            ],
            projects: vec![],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });

        let rows = screen_rows(&mut stack);
        assert!(
            rows.iter().any(|row| row.contains("❯ add an issue:")),
            "reattach must keep prompt and text on one row: {rows:?}",
        );

        stack.set_active_session(Some(shell_key));
        render(&mut stack);
        stack.set_active_session(Some(agent_key));
        let rows = screen_rows(&mut stack);
        assert!(
            rows.iter().any(|row| row.contains("❯ add an issue:")),
            "refocus must not insert a row before composer text: {rows:?}",
        );
    }

    /// A resync replaces any bytes buffered while hidden — the ring is
    /// authoritative, so the stale buffer is dropped.
    #[test]
    fn resync_clears_pending_buffer() {
        let sk_a = SessionKey::new("a");
        let sk_b = SessionKey::new("b");
        let mut stack = TerminalStack::new(PaneId::new(0));
        spawn(&mut stack, TerminalId(1), &sk_a);
        spawn(&mut stack, TerminalId(2), &sk_b);
        stack.set_active_session(Some(sk_a));
        render(&mut stack);

        feed(&mut stack, TerminalId(2), b"stale\r\n", 1);
        assert!(!stack.terminals[&TerminalId(2)].pending_feed.is_empty());

        stack.on_event(&Event::TerminalResync {
            terminal_id: TerminalId(2),
            replay: b"fresh\r\n".to_vec(),
            seq: 9,
        });
        assert!(stack.terminals[&TerminalId(2)].pending_feed.is_empty());

        stack.set_active_session(Some(sk_b));
        assert_eq!(row0(&mut stack), "fresh");
    }

    /// #909 regression: a long soft-wrapped token and a sticky status row
    /// share a scroll region, then an authoritative replay replaces the live
    /// parser after a sequence gap. The replay must be reset-and-refeed, not
    /// appended to the shifted grid; otherwise the wrapped tail and status
    /// row accumulate once per redraw/resync.
    #[test]
    fn soft_wrap_scroll_region_redraw_and_resync_do_not_duplicate_rows() {
        let sk = SessionKey::new("agent");
        let id = TerminalId(1);
        let mut stack = TerminalStack::new(PaneId::new(0));
        spawn(&mut stack, id, &sk);
        stack.set_active_session(Some(sk));
        render(&mut stack);

        let long_branch = format!("branch {}WRAP-TAIL-909", "x".repeat(90));
        let replay = format!(
            "\x1b[2J\x1b[H\x1b[2;18r\x1b[4;1H{long_branch}\
             \x1b[16;1H\x1b[2KSTATUS-909 running in background\
             \x1b[16;1H\x1b[2KSTATUS-909 running in background\
             \x1b[2;1H\x1bM\
             \x1b[17;1H\x1b[2K\x1b[16;1H\x1b[2KSTATUS-909 running in background"
        );
        feed(&mut stack, id, replay.as_bytes(), 1);

        let assert_once = |rows: &[String]| {
            assert_eq!(
                rows.iter()
                    .filter(|row| row.contains("WRAP-TAIL-909"))
                    .count(),
                1,
                "soft-wrapped tail duplicated: {rows:?}",
            );
            assert_eq!(
                rows.iter().filter(|row| row.contains("STATUS-909")).count(),
                1,
                "sticky status row duplicated: {rows:?}",
            );
        };

        assert_once(&screen_rows(&mut stack));

        // Skip seq 2. The client preserves the coherent grid until the
        // authoritative ring replay arrives, then rebuilds from scratch.
        feed(&mut stack, id, b"dropped successor", 3);
        assert!(stack.terminals[&id].sync.is_desynced());
        stack.on_event(&Event::TerminalResync {
            terminal_id: id,
            replay: replay.into_bytes(),
            seq: 3,
        });
        assert!(!stack.terminals[&id].sync.is_desynced());

        assert_once(&screen_rows(&mut stack));
        // A paint-only redraw must remain idempotent as well.
        assert_once(&screen_rows(&mut stack));
    }
}

/// Regression coverage for #20: the footer hint bar (and the blank
/// margin lazybox holds back above it) must look the same no matter
/// where the focused terminal is scrolled. The bug was the inner
/// program's last line — Claude Code prints "? for shortcuts" — landing
/// flush against the hint bar at the live bottom and being read as one
/// of lazybox's hints, then vanishing once the user scrolled up.
#[cfg(test)]
mod footer_scroll_independence {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

    const W: u16 = 80;
    const H: u16 = 24;

    /// Render the terminal pane + footer the way `Model::view` does and
    /// return the screen rows as plain strings (symbols only).
    fn render_rows(stack: &mut TerminalStack) -> Vec<String> {
        let backend = TestBackend::new(W, H);
        let mut term = Terminal::new(backend).unwrap();
        let binds = TerminalStack::contextual_bindings(']');
        term.draw(|f| {
            // Footer owns the last row; the panes fill everything above.
            let pane = Rect::new(0, 0, W, H - 1);
            let footer = Rect::new(0, H - 1, W, 1);
            stack.render(pane, f, true);
            crate::realm::components::footer::render(
                f,
                footer,
                None,
                &binds,
                &[],
                &[],
                "?",
                None,
                None,
            );
        })
        .unwrap();
        let buf = term.backend().buffer().clone();
        (0..H)
            .map(|y| {
                (0..W)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn agent_stack_with_scrollback() -> TerminalStack {
        let sk = SessionKey::new("s");
        let mut stack = TerminalStack::new(PaneId::new(0));
        let mut slot = TerminalStack::make_slot(
            sk.clone(),
            TerminalKind::Agent("claude".into()),
            0,
            false,
            false,
            None,
            Vec::new(),
            String::new(),
        );
        slot.vt.ensure_size(W - 3, H - 4);
        let mut payload = String::new();
        for i in 0..40 {
            payload.push_str(&format!("output line {i}\r\n"));
        }
        // Mirror Claude Code's persistent bottom chrome.
        payload.push_str("? for shortcuts");
        slot.vt.feed(payload.as_bytes());
        stack.insert_slot_for_test(TerminalId(1), slot);
        stack.set_active_session(Some(sk));
        stack
    }

    #[test]
    fn hint_bar_and_its_margin_are_identical_at_top_and_bottom() {
        let mut stack = agent_stack_with_scrollback();

        // At the live bottom the inner program's "? for shortcuts" is on
        // screen; the margin row + footer below it must still be clean.
        let at_bottom = render_rows(&mut stack);
        // Scroll well up into scrollback, then back down to the live
        // bottom — same two rows both times.
        let _outcome = stack.scroll_active(-12);
        let at_top = render_rows(&mut stack);

        let margin = (H - 2) as usize; // blank row lazybox holds back
        let footer_row = (H - 1) as usize; // the hint bar itself

        assert_eq!(
            at_top[margin], at_bottom[margin],
            "margin row above the hint bar drifted with scroll: {:?} vs {:?}",
            at_top[margin], at_bottom[margin]
        );
        assert_eq!(
            at_top[footer_row], at_bottom[footer_row],
            "hint bar drifted with scroll: {:?} vs {:?}",
            at_top[footer_row], at_bottom[footer_row]
        );

        // The margin keeps inner-program output (the stray "?") from ever
        // touching the hint bar, at either scroll position.
        assert_eq!(at_bottom[margin], "", "agent output bled into the margin");
        assert_eq!(at_top[margin], "");
        assert!(
            !at_bottom[footer_row].contains('?'),
            "footer should never carry a `?` hint: {:?}",
            at_bottom[footer_row]
        );
    }

    /// #321: `Shift-PageUp` on the focused terminal must move the
    /// viewport into scrollback and `Shift-End` must bring it back.
    /// This drives the real key handler (`handle_key`), not
    /// `scroll_active` directly, so a regression in the key routing —
    /// the modifier match, the `PageUp`/`Home`/`End` arms — fails here.
    ///
    /// Entry point is the pane's `handle_key` rather than
    /// `Model::dispatch_key`: the latter ends every keystroke in
    /// `sync_panes`, which in a bare model (no selected sidebar
    /// workspace) resets the active session and would mask the scroll.
    /// That's a harness gap, not a product bug — typing into a live
    /// terminal proves `sync_panes` preserves the session in real use —
    /// but it does mean the top-level keyboard route isn't covered
    /// end-to-end here; `handle_key` is the closest faithful seam.
    #[test]
    fn shift_pageup_moves_viewport_and_shift_end_returns() {
        let mut stack = agent_stack_with_scrollback();

        fn offset(stack: &TerminalStack) -> u64 {
            stack
                .scrollbar_summary()
                .expect("summary")
                .split_whitespace()
                .find_map(|kv| kv.strip_prefix("offset="))
                .expect("offset field")
                .parse()
                .expect("numeric offset")
        }

        let bottom = offset(&stack);
        assert!(bottom > 0, "the scrollback-filled agent must have history");

        let mut cmds = Vec::new();
        let outcome = stack.handle_key(
            KeyEvent::new(KeyCode::PageUp, KeyModifiers::SHIFT),
            &mut cmds,
        );
        assert!(
            matches!(outcome, PaneOutcome::Consumed),
            "scroll is consumed"
        );
        // The viewport move itself is in-process; the only allowed
        // side effect is the deep-scrollback fetch (#393) — never a
        // PTY write.
        assert!(
            !cmds.iter().any(|c| matches!(c, Command::Write { .. })),
            "keyboard scroll must not write to the PTY: {cmds:?}"
        );
        let scrolled = offset(&stack);
        assert!(
            scrolled < bottom,
            "Shift-PageUp must move the viewport up into scrollback \
             (bottom={bottom} scrolled={scrolled})",
        );

        // Shift-End jumps back to the live bottom.
        stack.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::SHIFT), &mut cmds);
        assert_eq!(
            offset(&stack),
            bottom,
            "Shift-End returns to the live bottom"
        );
    }

    /// #360: every keyboard scroll binding must move the viewport on a
    /// FRESH agent slot — one built straight from `make_slot`, never
    /// reattached or replayed. The chronic regression only ever bit
    /// brand-new sessions (reattach replays history and looked fine),
    /// so the fresh path is pinned across all four bindings:
    /// Shift-PageUp / Shift-PageDown scroll by a step, Shift-Home /
    /// Shift-End jump to the extremes.
    #[test]
    fn fresh_agent_keyboard_scroll_bindings_move_the_viewport() {
        let mut stack = agent_stack_with_scrollback();

        fn offset(stack: &TerminalStack) -> u64 {
            stack
                .scrollbar_summary()
                .expect("summary")
                .split_whitespace()
                .find_map(|kv| kv.strip_prefix("offset="))
                .expect("offset field")
                .parse()
                .expect("numeric offset")
        }

        let bottom = offset(&stack);
        assert!(bottom > 0, "the fresh agent must have scrollback to move");

        let mut cmds = Vec::new();
        macro_rules! press {
            ($code:expr) => {
                stack.handle_key(KeyEvent::new($code, KeyModifiers::SHIFT), &mut cmds)
            };
        }

        // Shift-Home pins the viewport to the very top of scrollback.
        press!(KeyCode::Home);
        let top = offset(&stack);
        assert_eq!(top, 0, "Shift-Home jumps to the top of scrollback");

        // Shift-PageDown walks back down toward the live bottom.
        press!(KeyCode::PageDown);
        let after_pgdn = offset(&stack);
        assert!(
            after_pgdn > top,
            "Shift-PageDown must move the viewport down (top={top} after={after_pgdn})",
        );

        // Shift-PageUp walks back up into scrollback.
        press!(KeyCode::PageUp);
        assert!(
            offset(&stack) < after_pgdn,
            "Shift-PageUp must move the viewport up into scrollback",
        );

        // Shift-End returns to the live bottom.
        press!(KeyCode::End);
        assert_eq!(
            offset(&stack),
            bottom,
            "Shift-End returns to the live bottom"
        );
        // The viewport moves are in-process; the only allowed side
        // effect is the deep-scrollback fetch (#393) — never a PTY
        // write.
        assert!(
            !cmds.iter().any(|c| matches!(c, Command::Write { .. })),
            "keyboard scroll must not write to the PTY: {cmds:?}"
        );
    }

    #[test]
    fn tab_strip_shows_chosen_model_tier_badge() {
        // A session launched via a tier chord carries its model label
        // through to the tab strip so the running model is visible.
        let sk = SessionKey::new("s");
        let mut stack = TerminalStack::new(PaneId::new(0));
        let slot = TerminalStack::make_slot(
            sk.clone(),
            TerminalKind::Agent("claude".into()),
            0,
            false,
            false,
            Some("Opus".into()),
            Vec::new(),
            String::new(),
        );
        stack.insert_slot_for_test(TerminalId(1), slot);
        stack.set_active_session(Some(sk));

        let rows = render_rows(&mut stack);
        assert!(
            rows.iter().any(|r| r.contains("◆ Opus")),
            "tab strip should show the tier badge; got {rows:?}",
        );
    }

    #[test]
    fn model_changed_supersedes_the_spawn_tier_in_the_tab() {
        // #779: a spawn tier can go stale once the user switches model
        // inside the agent. The daemon's live reading arrives as
        // `TerminalModelChanged` and must replace the tab's tier label.
        let sk = SessionKey::new("s");
        let mut stack = TerminalStack::new(PaneId::new(0));
        let slot = TerminalStack::make_slot(
            sk.clone(),
            TerminalKind::Agent("codex".into()),
            0,
            false,
            false,
            Some("Opus".into()),
            Vec::new(),
            String::new(),
        );
        stack.insert_slot_for_test(TerminalId(1), slot);
        stack.set_active_session(Some(sk.clone()));

        stack.on_event(&Event::TerminalModelChanged {
            session_key: sk,
            terminal_id: TerminalId(1),
            model_label: "gpt-5.5 · xhigh".into(),
        });

        let rows = render_rows(&mut stack);
        assert!(
            rows.iter().any(|r| r.contains("◆ gpt-5.5 · xhigh")),
            "the live model must replace the spawn tier; got {rows:?}",
        );
        assert!(
            !rows.iter().any(|r| r.contains("Opus")),
            "the stale spawn tier must be gone; got {rows:?}",
        );
    }
}

#[cfg(test)]
mod summarize_message_tests {
    use super::summarize_message;

    #[test]
    fn collapses_whitespace_in_plain_prose() {
        assert_eq!(
            summarize_message("fix bug in foo.rs\nand   retry"),
            "fix bug in foo.rs and retry"
        );
    }

    #[test]
    fn collapses_leading_image_path_with_raw_spaces() {
        // The motivating case: a CleanShot path with unescaped spaces,
        // followed by the user's actual prompt.
        let msg = "/tmp/Application Support/CleanShot/media/media_qjLWRXdkJW/CleanShot 2026-06-02 at 11.35.48@2x.png create an issue: foo";
        assert_eq!(summarize_message(msg), "[image] create an issue: foo");
    }

    #[test]
    fn collapses_escaped_space_image_path() {
        let msg = "/tmp/CleanShot\\ 2026-06-02\\ at\\ 11.35.48@2x.png describe this";
        assert_eq!(summarize_message(msg), "[image] describe this");
    }

    #[test]
    fn message_that_is_only_an_image_path() {
        assert_eq!(summarize_message("/tmp/shot.png"), "[image]");
        assert_eq!(summarize_message("~/Pictures/a.JPEG"), "[image]");
    }

    #[test]
    fn message_that_is_only_a_file_path() {
        assert_eq!(summarize_message("/tmp/report.pdf"), "[file]");
        // No extension, leading path → still a file.
        assert_eq!(summarize_message("~/notes/scratch"), "[file]");
    }

    #[test]
    fn collapses_quoted_path() {
        assert_eq!(
            summarize_message("\"/tmp/my shot.png\" look here"),
            "[image] look here"
        );
    }

    #[test]
    fn non_image_path_mid_prose_is_left_intact() {
        // A typed reference, not an injected drag — keep it readable.
        assert_eq!(
            summarize_message("check /etc/hosts then restart"),
            "check /etc/hosts then restart"
        );
    }

    #[test]
    fn appended_image_path_mid_prose_still_collapses() {
        assert_eq!(
            summarize_message("look at this ./screenshot.png please"),
            "look at this [image] please"
        );
    }

    #[test]
    fn dotted_version_run_does_not_anchor_early() {
        // Ensure the all-digit `.35`/`.48` segments don't terminate the
        // path before the real `.png` extension.
        let msg = "/a/b 11.35.48.png go";
        assert_eq!(summarize_message(msg), "[image] go");
    }

    #[test]
    fn plain_prose_with_no_path_is_unchanged() {
        assert_eq!(
            summarize_message("just normal text here"),
            "just normal text here"
        );
    }
}

#[cfg(test)]
mod agent_badge_tests {
    //! `displays_agent_state` drives the orchestrator's redraw-skip
    //! for `AgentState` pings. It must report "stale" whenever ANY
    //! agent slot in the session would change badge — a second agent
    //! spawned after the session went Working starts Idle and needs
    //! its flip painted.
    use super::*;

    fn spawn_agent(stack: &mut TerminalStack, id: u64, sk: &SessionKey, agent: &str) {
        stack.on_event(&Event::TerminalSpawned {
            model_label: None,
            terminal_id: TerminalId(id),
            session_key: sk.clone(),
            kind: TerminalKind::Agent(agent.into()),
            no_permission: false,
            on_main: false,
        });
    }

    #[test]
    fn predicate_reports_stale_when_a_second_agent_badge_lags() {
        use lazybox_ipc::AgentState;
        let sk = SessionKey::new("github:o/r#1");
        let mut stack = TerminalStack::new(PaneId::new(0));

        spawn_agent(&mut stack, 1, &sk, "claude");
        stack.on_event(&Event::AgentState {
            terminal_id: TerminalId(1),
            session_key: sk.clone(),
            state: AgentState::Working,
        });
        assert!(
            stack.displays_agent_state(&sk, AgentState::Working),
            "single up-to-date badge → displayed",
        );

        // Second agent spawns Idle: its badge would flip on the next
        // Working ping, so the predicate must report "not displayed".
        spawn_agent(&mut stack, 2, &sk, "codex");
        assert!(
            !stack.displays_agent_state(&sk, AgentState::Working),
            "a lagging second badge means the event still changes pixels",
        );

        // Applying the event catches the badge up again.
        stack.on_event(&Event::AgentState {
            terminal_id: TerminalId(2),
            session_key: sk.clone(),
            state: AgentState::Working,
        });
        assert!(stack.displays_agent_state(&sk, AgentState::Working));

        // Vacuously true for a session with no agent slots — nothing
        // to repaint.
        let other = SessionKey::new("github:o/r#2");
        assert!(stack.displays_agent_state(&other, AgentState::Working));
    }

    /// A state event for one agent must not clobber a sibling agent's
    /// badge. Two agents in one session diverge (Working / Done); an
    /// event for one leaves the other's badge untouched. The pre-fix
    /// session-wide apply overwrote every sibling, so a busy agent read
    /// the last sibling's Idle/Done (false negative) and an idle one
    /// read a sibling's Working (false positive).
    #[test]
    fn agent_state_event_updates_only_its_own_terminal() {
        use lazybox_ipc::AgentState;
        let sk = SessionKey::new("github:o/r#1");
        let mut stack = TerminalStack::new(PaneId::new(0));

        spawn_agent(&mut stack, 1, &sk, "claude");
        spawn_agent(&mut stack, 2, &sk, "codex");

        let badge = |stack: &TerminalStack, id: u64| {
            stack.terminals.get(&TerminalId(id)).map(|s| s.agent_state)
        };

        stack.on_event(&Event::AgentState {
            terminal_id: TerminalId(1),
            session_key: sk.clone(),
            state: AgentState::Working,
        });
        stack.on_event(&Event::AgentState {
            terminal_id: TerminalId(2),
            session_key: sk.clone(),
            state: AgentState::Done,
        });
        assert_eq!(badge(&stack, 1), Some(AgentState::Working));
        assert_eq!(badge(&stack, 2), Some(AgentState::Done));

        // Terminal 1 finishes — terminal 2's badge is left alone.
        stack.on_event(&Event::AgentState {
            terminal_id: TerminalId(1),
            session_key: sk.clone(),
            state: AgentState::Idle,
        });
        assert_eq!(badge(&stack, 1), Some(AgentState::Idle));
        assert_eq!(
            badge(&stack, 2),
            Some(AgentState::Done),
            "a sibling's state event must not overwrite this badge",
        );
    }
}

#[cfg(test)]
mod rebadge_tests {
    //! Issue→PR collapse rebadges terminals onto the PR workspace. The
    //! terminal stack must follow that move BEFORE the trailing
    //! `WorkspaceRemoved(issue)` arrives — otherwise the moved slots
    //! still carry the issue key, the removal handler drops them, and
    //! the live session vanishes from view (#34).
    use super::*;

    fn spawned_stack(id: TerminalId, sk: &SessionKey) -> TerminalStack {
        let mut stack = TerminalStack::new(PaneId::new(0));
        stack.on_event(&Event::TerminalSpawned {
            model_label: None,
            terminal_id: id,
            session_key: sk.clone(),
            kind: TerminalKind::Agent("claude".into()),
            no_permission: false,
            on_main: false,
        });
        stack.set_active_session(Some(sk.clone()));
        stack
    }

    fn active_rows(stack: &TerminalStack, id: TerminalId) -> Vec<String> {
        let slot = stack.terminals.get(&id).expect("terminal slot");
        let terminal = &slot.vt.terminal;
        (0..slot.vt.rows)
            .map(|y| {
                let mut text = String::new();
                for x in 0..slot.vt.cols {
                    let cell = terminal
                        .grid_ref(vt::terminal::Point::Active(vt::terminal::PointCoordinate {
                            x,
                            y: y.into(),
                        }))
                        .expect("active cell");
                    let mut graphemes = ['\0'; 16];
                    let len = cell.graphemes(&mut graphemes).expect("graphemes");
                    text.extend(&graphemes[..len]);
                }
                text.trim_end().to_string()
            })
            .collect()
    }

    #[test]
    fn rebadge_then_remove_keeps_the_moved_terminal() {
        let issue = SessionKey::new("github:o/r#1");
        let pr = SessionKey::new("github:o/r#2");
        let mut stack = spawned_stack(TerminalId(1), &issue);

        stack.on_event(&Event::TerminalsRebadged {
            from: issue.clone(),
            to: pr.clone(),
        });

        // Slot now belongs to the PR session, and the active session
        // followed.
        assert_eq!(stack.active_session(), Some(&pr));
        assert_eq!(
            stack.terminals.get(&TerminalId(1)).map(|s| &s.session_key),
            Some(&pr),
        );

        // The trailing removal of the (now-gone) issue workspace must
        // NOT drop the moved terminal.
        stack.on_event(&Event::WorkspaceRemoved(lazybox_core::WorkspaceKey::new(
            issue.as_str().to_string(),
        )));
        assert!(
            stack.terminals.contains_key(&TerminalId(1)),
            "rebadged terminal survived the issue-workspace removal",
        );
    }

    #[test]
    fn rebadge_moves_every_issue_terminal_and_spares_other_workspaces() {
        // A workspace can run several terminals (agent + shell, splits/
        // tabs); the rebadge must carry all of them, and only them.
        let issue = SessionKey::new("github:o/r#1");
        let pr = SessionKey::new("github:o/r#2");
        let other = SessionKey::new("github:o/r#3");
        let mut stack = spawned_stack(TerminalId(1), &issue);
        stack.on_event(&Event::TerminalSpawned {
            model_label: None,
            terminal_id: TerminalId(2),
            session_key: issue.clone(),
            kind: TerminalKind::Shell,
            no_permission: false,
            on_main: false,
        });
        stack.on_event(&Event::TerminalSpawned {
            model_label: None,
            terminal_id: TerminalId(3),
            session_key: other.clone(),
            kind: TerminalKind::Shell,
            no_permission: false,
            on_main: false,
        });

        stack.on_event(&Event::TerminalsRebadged {
            from: issue.clone(),
            to: pr.clone(),
        });

        for id in [TerminalId(1), TerminalId(2)] {
            assert_eq!(
                stack.terminals.get(&id).map(|s| &s.session_key),
                Some(&pr),
                "every issue terminal must follow the move",
            );
        }
        assert_eq!(
            stack.terminals.get(&TerminalId(3)).map(|s| &s.session_key),
            Some(&other),
            "an unrelated workspace's terminal must not be rebadged",
        );

        stack.on_event(&Event::WorkspaceRemoved(lazybox_core::WorkspaceKey::new(
            issue.as_str().to_string(),
        )));
        for id in [TerminalId(1), TerminalId(2), TerminalId(3)] {
            assert!(
                stack.terminals.contains_key(&id),
                "no live terminal may be dropped by the trailing removal",
            );
        }
    }

    #[test]
    fn without_rebadge_removal_still_drops_the_issue_terminal() {
        // Guards the rebadge's necessity: a removal that ISN'T preceded
        // by a rebadge drops the terminal, exactly as before the fix.
        let issue = SessionKey::new("github:o/r#1");
        let mut stack = spawned_stack(TerminalId(1), &issue);

        stack.on_event(&Event::WorkspaceRemoved(lazybox_core::WorkspaceKey::new(
            issue.as_str().to_string(),
        )));
        assert!(!stack.terminals.contains_key(&TerminalId(1)));
    }

    #[test]
    fn rebadge_matrix_preserves_draft_and_visible_composer_exactly() {
        for (agent, draft) in [
            ("claude", "single-line draft"),
            ("claude", "first line\n  second line"),
            ("codex", "single-line draft"),
            ("codex", "first line\n  second line"),
        ] {
            let id = TerminalId(1);
            let mut current = SessionKey::new(format!("test:{agent}-issue"));
            let mut stack = TerminalStack::new(PaneId::new(0));
            stack.on_event(&Event::TerminalSpawned {
                terminal_id: id,
                session_key: current.clone(),
                kind: TerminalKind::Agent(agent.into()),
                no_permission: false,
                on_main: false,
                model_label: None,
            });
            stack.set_active_session(Some(current.clone()));
            let rendered = format!(
                "\x1b[2J\x1b[H{agent}\r\n\r\n> {}",
                draft.replace('\n', "\r\n")
            );
            stack.on_event(&Event::TerminalOutput {
                terminal_id: id,
                bytes: rendered.into_bytes(),
                first_seq: 1,
                seq: 1,
            });
            stack
                .terminals
                .get_mut(&id)
                .expect("terminal slot")
                .record_pty_bytes(draft.as_bytes());
            let expected_rows = active_rows(&stack, id);

            for cycle in 0..4 {
                let next = SessionKey::new(format!("test:{agent}-pr-{cycle}"));
                stack.on_event(&Event::TerminalsRebadged {
                    from: current,
                    to: next.clone(),
                });
                assert_eq!(
                    stack.composing_of(id),
                    Some(draft),
                    "{agent} cycle {cycle}: draft bytes changed"
                );
                assert_eq!(
                    active_rows(&stack, id),
                    expected_rows,
                    "{agent} cycle {cycle}: visible composer changed"
                );
                current = next;
            }
        }
    }
}

#[cfg(test)]
mod hover_scroll_tests {
    //! #362: the mouse wheel scrolls the tile UNDER THE CURSOR, not the
    //! focused one. After a render records each tile's rect, a wheel
    //! event's coordinates must resolve to the tile they landed in so
    //! its scrollback moves — even while a different tile holds focus.
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    const W: u16 = 80;
    const H: u16 = 24;

    fn shell_with_scrollback(sk: &SessionKey, tag: &str) -> TerminalSlot {
        let mut slot = TerminalStack::make_slot(
            sk.clone(),
            TerminalKind::Shell,
            0,
            false,
            false,
            None,
            Vec::new(),
            String::new(),
        );
        slot.vt.ensure_size(W / 2, H - 4);
        let mut payload = String::new();
        for i in 0..60 {
            payload.push_str(&format!("{tag} line {i}\r\n"));
        }
        slot.vt.feed(payload.as_bytes());
        slot
    }

    /// Two shells side by side, each with its own scrollback. Focus is
    /// on the RIGHT tile; the layout matches what a `]]|` split builds.
    fn split_stack() -> TerminalStack {
        let sk = SessionKey::new("s");
        let mut stack = TerminalStack::new(PaneId::new(0));
        stack
            .terminals
            .insert(TerminalId(1), shell_with_scrollback(&sk, "left"));
        stack
            .terminals
            .insert(TerminalId(2), shell_with_scrollback(&sk, "right"));
        stack.set_active_session(Some(sk));
        stack.layout = lazybox_core::SessionLayout::Splits {
            tree: lazybox_core::TileTree::HSplit {
                left: Box::new(lazybox_core::TileTree::Leaf { terminal_id: 1 }),
                right: Box::new(lazybox_core::TileTree::Leaf { terminal_id: 2 }),
                ratio: 50,
            },
            focused: vec![1],
        };
        stack
    }

    fn render(stack: &mut TerminalStack) {
        let mut term = Terminal::new(TestBackend::new(W, H)).unwrap();
        term.draw(|f| stack.render(Rect::new(0, 0, W, H), f, true))
            .unwrap();
    }

    fn offset(stack: &TerminalStack, id: TerminalId) -> u64 {
        stack.terminals[&id]
            .vt
            .terminal
            .scrollbar()
            .expect("scrollbar")
            .offset
    }

    /// A wheel event at coordinates inside the LEFT tile scrolls the
    /// left terminal, even though the RIGHT tile is focused — and focus
    /// never moves.
    #[test]
    fn wheel_in_left_tile_scrolls_left_while_right_is_focused() {
        let mut stack = split_stack();
        render(&mut stack);
        assert_eq!(
            stack.focused_terminal_id(),
            Some(TerminalId(2)),
            "the right tile is focused",
        );

        // A point well inside the left half of the body (past the pane
        // border + top chrome) hit-tests to the left tile.
        let (col, row) = (5, 6);
        assert_eq!(
            stack.scroll_terminal_at(col, row),
            Some(TerminalId(1)),
            "coordinates in the left tile resolve to the left terminal",
        );

        let left_before = offset(&stack, TerminalId(1));
        let right_before = offset(&stack, TerminalId(2));

        // Route the wheel exactly as the handler does: resolve the tile
        // under the cursor, then scroll it.
        let target = stack.scroll_terminal_at(col, row).unwrap();
        let _outcome = stack.scroll_terminal(target, -3);

        assert_eq!(
            offset(&stack, TerminalId(1)),
            left_before - 3,
            "the hovered (left) terminal scrolled up into its scrollback",
        );
        assert_eq!(
            offset(&stack, TerminalId(2)),
            right_before,
            "the focused (right) terminal must not move",
        );
        assert_eq!(
            stack.focused_terminal_id(),
            Some(TerminalId(2)),
            "hover-to-scroll must not change focus",
        );
    }

    /// The symmetric case: hovering the right tile scrolls the right
    /// terminal while the left tile is focused.
    #[test]
    fn wheel_in_right_tile_scrolls_right_while_left_is_focused() {
        let mut stack = split_stack();
        stack.layout = lazybox_core::SessionLayout::Splits {
            tree: lazybox_core::TileTree::HSplit {
                left: Box::new(lazybox_core::TileTree::Leaf { terminal_id: 1 }),
                right: Box::new(lazybox_core::TileTree::Leaf { terminal_id: 2 }),
                ratio: 50,
            },
            focused: vec![0],
        };
        render(&mut stack);
        assert_eq!(stack.focused_terminal_id(), Some(TerminalId(1)));

        let (col, row) = (W - 6, 6);
        assert_eq!(stack.scroll_terminal_at(col, row), Some(TerminalId(2)));

        let left_before = offset(&stack, TerminalId(1));
        let right_before = offset(&stack, TerminalId(2));
        let target = stack.scroll_terminal_at(col, row).unwrap();
        let _outcome = stack.scroll_terminal(target, -3);

        assert_eq!(offset(&stack, TerminalId(2)), right_before - 3);
        assert_eq!(offset(&stack, TerminalId(1)), left_before);
    }
}

#[cfg(test)]
mod set_layout_tests {
    //! `set_layout` runs inside `sync_panes`, i.e. after EVERY key
    //! dispatch and daemon event. It must be a no-op when the layout
    //! is unchanged — otherwise any daemon event landing between
    //! `]]|` and the `TerminalSpawned` it waits on resets the
    //! latch and silently cancels the pending split.
    use super::*;

    fn stack_with_terminal(sk: &SessionKey) -> TerminalStack {
        let mut stack = TerminalStack::new(PaneId::new(0));
        stack.on_event(&Event::TerminalSpawned {
            model_label: None,
            terminal_id: TerminalId(1),
            session_key: sk.clone(),
            kind: TerminalKind::Shell,
            no_permission: false,
            on_main: false,
        });
        stack.set_active_session(Some(sk.clone()));
        stack
    }

    #[test]
    fn unchanged_layout_keeps_the_pending_split() {
        let sk = SessionKey::new("github:o/r#1");
        let mut stack = stack_with_terminal(&sk);
        stack.pending_split = Some((PendingSplit::Vertical, std::time::Instant::now()));

        // Same layout re-projected (what every daemon event does via
        // sync_panes) — the armed split must survive.
        stack.set_layout(stack.layout().clone());
        assert_eq!(
            stack.pending_split.map(|(d, _)| d),
            Some(PendingSplit::Vertical),
            "an unchanged layout must not cancel the pending split",
        );

        // The deferred spawn then completes the split as intended.
        stack.on_event(&Event::TerminalSpawned {
            model_label: None,
            terminal_id: TerminalId(2),
            session_key: sk,
            kind: TerminalKind::Shell,
            no_permission: false,
            on_main: false,
        });
        assert!(stack.pending_split.is_none(), "spawn consumed the split");
        assert!(
            matches!(stack.layout(), lazybox_core::SessionLayout::Splits { .. }),
            "the new terminal landed in a split layout",
        );
    }

    #[test]
    fn changed_layout_still_resets_the_latch() {
        let sk = SessionKey::new("github:o/r#1");
        let mut stack = stack_with_terminal(&sk);
        stack.pending_split = Some((PendingSplit::Horizontal, std::time::Instant::now()));

        // A genuine workspace switch swaps the layout — stale split
        // intent must not leak into the next workspace.
        stack.set_layout(lazybox_core::SessionLayout::Splits {
            tree: lazybox_core::TileTree::Leaf { terminal_id: 9 },
            focused: vec![],
        });
        assert!(
            stack.pending_split.is_none(),
            "a layout change must clear the stale pending split",
        );
    }

    /// A LOCAL layout mutation (`]]<arrow>` focus move, a committed
    /// split) must survive re-projections of the stale persisted
    /// layout — `sync_panes` re-applies it on every daemon event while
    /// the mutation's own `SetSessionLayout` is still round-tripping.
    /// A genuinely new daemon-side layout must still apply.
    #[test]
    fn local_layout_mutation_survives_stale_resync() {
        let sk = SessionKey::new("github:o/r#1");
        let mut stack = stack_with_terminal(&sk);
        stack.on_event(&Event::TerminalSpawned {
            model_label: None,
            terminal_id: TerminalId(2),
            session_key: sk,
            kind: TerminalKind::Shell,
            no_permission: false,
            on_main: false,
        });
        let two_leaves = |focused: Vec<u8>| lazybox_core::SessionLayout::Splits {
            tree: lazybox_core::TileTree::HSplit {
                left: Box::new(lazybox_core::TileTree::Leaf { terminal_id: 1 }),
                right: Box::new(lazybox_core::TileTree::Leaf { terminal_id: 2 }),
                ratio: 50,
            },
            focused,
        };

        // Daemon projects the persisted layout (focus on the left).
        stack.set_layout(two_leaves(vec![0]));
        // Local mutation: move tile focus right.
        let mut cmds = Vec::new();
        stack.move_tile_focus(lazybox_core::TileDirection::Right, &mut cmds);
        assert_eq!(stack.focused_terminal_id(), Some(TerminalId(2)));

        // The same stale persisted layout re-projected (any daemon
        // event does this) must NOT stomp the local move.
        stack.set_layout(two_leaves(vec![0]));
        assert_eq!(
            stack.focused_terminal_id(),
            Some(TerminalId(2)),
            "a stale re-projection must not revert the local focus move",
        );

        // A genuinely different daemon-side layout still applies.
        stack.set_layout(lazybox_core::SessionLayout::Tabs { active: 0 });
        assert!(matches!(
            stack.layout(),
            lazybox_core::SessionLayout::Tabs { .. }
        ));
    }
}

#[cfg(test)]
mod pending_split_tests {
    //! The `]]|` pending-split marker must only claim the spawn it
    //! belongs to: an active-session spawn arriving within
    //! [`PENDING_SPLIT_WINDOW`]. A spawn on another session must not
    //! consume it, and a marker whose shell spawn failed daemon-side
    //! must expire instead of hijacking an unrelated spawn later.
    use super::*;

    fn stack_with_terminal(sk: &SessionKey) -> TerminalStack {
        let mut stack = TerminalStack::new(PaneId::new(0));
        stack.on_event(&Event::TerminalSpawned {
            model_label: None,
            terminal_id: TerminalId(1),
            session_key: sk.clone(),
            kind: TerminalKind::Shell,
            no_permission: false,
            on_main: false,
        });
        stack.set_active_session(Some(sk.clone()));
        stack
    }

    fn spawn(stack: &mut TerminalStack, id: u64, sk: &SessionKey) {
        stack.on_event(&Event::TerminalSpawned {
            model_label: None,
            terminal_id: TerminalId(id),
            session_key: sk.clone(),
            kind: TerminalKind::Shell,
            no_permission: false,
            on_main: false,
        });
    }

    #[test]
    fn other_session_spawn_does_not_consume_the_marker() {
        let sk = SessionKey::new("github:o/r#1");
        let other = SessionKey::new("github:o/r#2");
        let mut stack = stack_with_terminal(&sk);
        stack.pending_split = Some((PendingSplit::Horizontal, std::time::Instant::now()));

        // A background spawn on another workspace lands first.
        spawn(&mut stack, 7, &other);
        assert!(
            stack.pending_split.is_some(),
            "a foreign-session spawn must not eat the pending split",
        );

        // The split's own shell then arrives and commits the armed
        // direction (VSplit = stacked = the `-` chord).
        spawn(&mut stack, 2, &sk);
        assert!(stack.pending_split.is_none());
        assert!(
            matches!(
                stack.layout(),
                lazybox_core::SessionLayout::Splits {
                    tree: lazybox_core::TileTree::VSplit { .. },
                    ..
                }
            ),
            "the armed horizontal direction must win, got {:?}",
            stack.layout(),
        );
    }

    #[test]
    fn expired_marker_falls_back_to_the_auto_layout_path() {
        let sk = SessionKey::new("github:o/r#1");
        let mut stack = stack_with_terminal(&sk);
        let stale = std::time::Instant::now()
            .checked_sub(PENDING_SPLIT_WINDOW + std::time::Duration::from_secs(1))
            .expect("clock predates the window");
        stack.pending_split = Some((PendingSplit::Horizontal, stale));

        spawn(&mut stack, 2, &sk);
        assert!(stack.pending_split.is_none(), "stale marker is dropped");
        assert!(
            matches!(
                stack.layout(),
                lazybox_core::SessionLayout::Splits {
                    tree: lazybox_core::TileTree::HSplit { .. },
                    ..
                }
            ),
            "an expired split direction must not steer the spawn; the \
             auto path splits vertically, got {:?}",
            stack.layout(),
        );
    }
}

#[cfg(test)]
mod spawn_focus_tests {
    //! Regression coverage for #58: spawning a terminal into the active
    //! session must make the new terminal the focused one, no matter how
    //! many terminals (and which layout) the session already has.
    use super::*;

    fn spawn(stack: &mut TerminalStack, id: u64, sk: &SessionKey, kind: TerminalKind) {
        stack.on_event(&Event::TerminalSpawned {
            model_label: None,
            terminal_id: TerminalId(id),
            session_key: sk.clone(),
            kind,
            no_permission: false,
            on_main: false,
        });
    }

    #[test]
    fn first_spawn_becomes_the_active_tab() {
        let sk = SessionKey::new("github:o/r#1");
        let mut stack = TerminalStack::new(PaneId::new(0));
        stack.set_active_session(Some(sk.clone()));

        spawn(&mut stack, 1, &sk, TerminalKind::Shell);

        assert_eq!(stack.focused_terminal_id(), Some(TerminalId(1)));
        assert!(matches!(
            stack.layout(),
            lazybox_core::SessionLayout::Tabs { .. }
        ));
    }

    #[test]
    fn second_spawn_splits_and_focuses_the_new_terminal() {
        let sk = SessionKey::new("github:o/r#1");
        let mut stack = TerminalStack::new(PaneId::new(0));
        stack.set_active_session(Some(sk.clone()));

        spawn(&mut stack, 1, &sk, TerminalKind::Agent("claude".into()));
        spawn(&mut stack, 2, &sk, TerminalKind::Shell);

        assert!(matches!(
            stack.layout(),
            lazybox_core::SessionLayout::Splits { .. }
        ));
        assert_eq!(stack.focused_terminal_id(), Some(TerminalId(2)));
    }

    #[test]
    fn third_spawn_into_existing_split_joins_the_tree_and_takes_focus() {
        // The original bug: with the session already in a Splits layout,
        // the spawn handler ignored the new terminal — it never entered
        // the tile tree and focus stayed on the previous leaf.
        let sk = SessionKey::new("github:o/r#1");
        let mut stack = TerminalStack::new(PaneId::new(0));
        stack.set_active_session(Some(sk.clone()));

        spawn(&mut stack, 1, &sk, TerminalKind::Agent("claude".into()));
        spawn(&mut stack, 2, &sk, TerminalKind::Shell);
        spawn(&mut stack, 3, &sk, TerminalKind::Shell);

        // All three are part of the visible set...
        let leaves = match stack.layout() {
            lazybox_core::SessionLayout::Splits { tree, .. } => tree.leaves(),
            other => panic!("expected Splits layout, got {other:?}"),
        };
        assert!(
            leaves.contains(&3),
            "the third spawn must enter the tile tree: {leaves:?}",
        );
        // ...and the freshly-spawned shell is the one in focus.
        assert_eq!(stack.focused_terminal_id(), Some(TerminalId(3)));
    }

    #[test]
    fn spawn_into_background_session_does_not_steal_focus() {
        let active = SessionKey::new("github:o/r#1");
        let background = SessionKey::new("github:o/r#2");
        let mut stack = TerminalStack::new(PaneId::new(0));
        stack.set_active_session(Some(active.clone()));

        spawn(&mut stack, 1, &active, TerminalKind::Shell);
        spawn(&mut stack, 2, &background, TerminalKind::Shell);

        // The background spawn must not pull focus off the active session.
        assert_eq!(stack.focused_terminal_id(), Some(TerminalId(1)));
    }
}

#[cfg(test)]
mod new_terminal_layout_tests {
    //! Issue #361: `ui.terminal_new_layout: tabs` makes an
    //! auto-spawned second terminal land as a new tab instead of
    //! promoting the session into a side-by-side split. The default
    //! (`split`) is unchanged.
    use super::*;

    fn spawn(stack: &mut TerminalStack, id: u64, sk: &SessionKey, kind: TerminalKind) {
        stack.on_event(&Event::TerminalSpawned {
            model_label: None,
            terminal_id: TerminalId(id),
            session_key: sk.clone(),
            kind,
            no_permission: false,
            on_main: false,
        });
    }

    fn with_pref(pref: lazybox_config::NewTerminalLayout) -> TerminalStack {
        let mut stack = TerminalStack::new(PaneId::new(0));
        let ui = lazybox_config::UiDefaults {
            terminal_new_layout: pref,
            ..Default::default()
        };
        stack.apply_ui_defaults(&ui);
        stack
    }

    #[test]
    fn tabs_pref_keeps_a_second_spawn_as_a_tab() {
        let sk = SessionKey::new("github:o/r#1");
        let mut stack = with_pref(lazybox_config::NewTerminalLayout::Tabs);
        stack.set_active_session(Some(sk.clone()));

        spawn(&mut stack, 1, &sk, TerminalKind::Agent("claude".into()));
        spawn(&mut stack, 2, &sk, TerminalKind::Shell);

        assert!(
            matches!(stack.layout(), lazybox_core::SessionLayout::Tabs { .. }),
            "tabs preference must not promote to a split: {:?}",
            stack.layout(),
        );
        // Both terminals are visible tabs and the fresh spawn is active.
        assert_eq!(stack.visible_terminals().len(), 2);
        assert_eq!(stack.focused_terminal_id(), Some(TerminalId(2)));
    }

    #[test]
    fn split_pref_is_the_unchanged_default() {
        let sk = SessionKey::new("github:o/r#1");
        let mut stack = with_pref(lazybox_config::NewTerminalLayout::Split);
        stack.set_active_session(Some(sk.clone()));

        spawn(&mut stack, 1, &sk, TerminalKind::Agent("claude".into()));
        spawn(&mut stack, 2, &sk, TerminalKind::Shell);

        assert!(
            matches!(stack.layout(), lazybox_core::SessionLayout::Splits { .. }),
            "split preference must promote to a split: {:?}",
            stack.layout(),
        );
        assert_eq!(stack.focused_terminal_id(), Some(TerminalId(2)));
    }

    #[test]
    fn tabs_pref_still_extends_a_hand_made_split() {
        // A session the user split by hand (`]]|`) stays in Splits; a
        // later ordinary spawn extends the tree rather than snapping
        // back to tabs.
        let sk = SessionKey::new("github:o/r#1");
        let mut stack = with_pref(lazybox_config::NewTerminalLayout::Tabs);
        stack.set_active_session(Some(sk.clone()));

        spawn(&mut stack, 1, &sk, TerminalKind::Agent("claude".into()));
        let mut cmds = Vec::new();
        stack.split_tile(PendingSplit::Vertical, &mut cmds);
        spawn(&mut stack, 2, &sk, TerminalKind::Shell);
        assert!(matches!(
            stack.layout(),
            lazybox_core::SessionLayout::Splits { .. }
        ));

        spawn(&mut stack, 3, &sk, TerminalKind::Shell);
        let leaves = match stack.layout() {
            lazybox_core::SessionLayout::Splits { tree, .. } => tree.leaves(),
            other => panic!("expected Splits layout, got {other:?}"),
        };
        assert!(
            leaves.contains(&3),
            "a spawn into an existing split still joins the tree: {leaves:?}",
        );
    }

    #[test]
    fn toggle_flips_and_takes_effect_on_the_next_spawn() {
        let sk = SessionKey::new("github:o/r#1");
        let mut stack = with_pref(lazybox_config::NewTerminalLayout::Split);
        stack.set_active_session(Some(sk.clone()));

        // Flip Split → Tabs before the second spawn.
        assert_eq!(
            stack.toggle_terminal_new_layout(),
            lazybox_config::NewTerminalLayout::Tabs
        );
        assert_eq!(
            stack.terminal_new_layout(),
            lazybox_config::NewTerminalLayout::Tabs
        );

        spawn(&mut stack, 1, &sk, TerminalKind::Agent("claude".into()));
        spawn(&mut stack, 2, &sk, TerminalKind::Shell);
        assert!(
            matches!(stack.layout(), lazybox_core::SessionLayout::Tabs { .. }),
            "post-toggle the second spawn stays a tab: {:?}",
            stack.layout(),
        );

        // Flip back Tabs → Split.
        assert_eq!(
            stack.toggle_terminal_new_layout(),
            lazybox_config::NewTerminalLayout::Split
        );
    }
}

#[cfg(test)]
mod terminal_availability_tests {
    //! Issue #114: the splash / tour / footer advertise a set of
    //! "always available" globals (`?`, `q q`, `Shift-T`, …). In a
    //! focused terminal the PTY eats every key, so those globals don't
    //! fire — the advertised set must match what `TerminalStack`
    //! actually dispatches. These tests pin that contract: the catalog
    //! flag `available_in_terminal` is the single source of truth, and
    //! the pane's real behavior agrees with it.
    use super::*;
    use lazybox_tui_core::action;

    fn stack_with_agent() -> TerminalStack {
        let sk = SessionKey::new("github:o/r#1");
        let mut stack = TerminalStack::new(PaneId::new(0));
        stack.on_event(&Event::TerminalSpawned {
            model_label: None,
            terminal_id: TerminalId(1),
            session_key: sk.clone(),
            kind: TerminalKind::Agent("claude".into()),
            no_permission: false,
            on_main: false,
        });
        stack.set_active_session(Some(sk));
        stack
    }

    #[test]
    fn universal_globals_forward_to_the_pty_instead_of_firing() {
        // Every "universal" shortcut, pressed in a live terminal, must
        // reach the PTY as a `Write` — proof the global action did NOT
        // intercept it. This is the behavior the catalog encodes via
        // `available_in_terminal == false`, asserted directly against
        // the pane that owns the keys.
        let probes = [
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), // quit chord
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE), // help
            KeyEvent::new(KeyCode::Char('T'), KeyModifiers::SHIFT), // tour
            KeyEvent::new(KeyCode::Char(','), KeyModifiers::NONE), // settings
        ];
        for key in probes {
            let mut stack = stack_with_agent();
            let mut cmds = Vec::new();
            let outcome = stack.handle_key(key, &mut cmds);
            assert!(matches!(outcome, PaneOutcome::Consumed));
            assert!(
                cmds.iter()
                    .any(|c| matches!(c, Command::Write { terminal_id, .. } if *terminal_id == TerminalId(1))),
                "{key:?} must forward to the PTY, not fire a global action",
            );
        }
    }

    #[test]
    fn terminal_bindings_collapse_into_the_configured_leader() {
        let bindings = TerminalStack::contextual_bindings(']');

        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].keys, "]]");
        assert_eq!(bindings[0].label, "menu");
        assert_eq!(TerminalStack::contextual_bindings('~')[0].keys, "~~");
        for def in action::universal_shortcuts() {
            assert!(
                bindings.iter().all(|b| b.keys != def.default_keys),
                "{:?} ({}) is advertised in the terminal hint bar but \
                 the PTY would eat it",
                def.kind,
                def.default_keys,
            );
        }
    }

    fn type_chars(stack: &mut TerminalStack, text: &str, cmds: &mut Vec<Command>) {
        for ch in text.chars() {
            stack.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), cmds);
        }
    }

    fn last_recorded_draft(cmds: &[Command]) -> Option<&str> {
        cmds.iter().rev().find_map(|c| match c {
            Command::RecordComposingBuffer { buffer, .. } => Some(buffer.as_str()),
            _ => None,
        })
    }

    /// Issue #373: each keystroke that edits the in-flight line ships a
    /// `RecordComposingBuffer` so a restart can recover a half-typed
    /// prompt — the daemon ring only carries output, not this input.
    #[test]
    fn typing_persists_the_in_flight_draft() {
        let mut stack = stack_with_agent();
        let mut cmds = Vec::new();
        type_chars(&mut stack, "fix", &mut cmds);
        assert_eq!(last_recorded_draft(&cmds), Some("fix"));
        assert_eq!(stack.composing_of(TerminalId(1)), Some("fix"));
    }

    /// Submitting commits the message AND clears the persisted draft
    /// (an empty `RecordComposingBuffer`), so the recovered line is the
    /// live one, never a stale already-sent prompt.
    #[test]
    fn submitting_records_the_message_and_clears_the_draft() {
        let mut stack = stack_with_agent();
        let mut cmds = Vec::new();
        type_chars(&mut stack, "ship it", &mut cmds);
        cmds.clear();
        stack.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &mut cmds);
        assert!(
            cmds.iter().any(|c| matches!(
                c,
                Command::RecordUserMessage { prompt, .. } if prompt.text == "ship it"
            )),
            "the submit records the committed message",
        );
        assert_eq!(
            last_recorded_draft(&cmds),
            Some(""),
            "the submit clears the stored draft",
        );
        assert_eq!(stack.composing_of(TerminalId(1)), Some(""));
    }

    /// A snapshot (client reconnect or fresh-daemon restart) restores
    /// the in-flight draft byte-for-byte, including user-entered leading
    /// and trailing newlines, while also restoring the submitted history.
    #[test]
    fn snapshot_recall_preserves_exact_draft_and_last_message() {
        let sk = SessionKey::new("github:o/r#1");
        let draft = "\n\t  half typed\n";
        let mut stack = TerminalStack::new(PaneId::new(0));
        stack.set_active_session(Some(sk.clone()));
        stack.on_event(&Event::Snapshot {
            workspaces: vec![],
            projects: vec![],
            terminals: vec![lazybox_ipc::TerminalSnapshot {
                terminal_id: TerminalId(1),
                session_key: sk.clone(),
                kind: TerminalKind::Agent("claude".into()),
                replay: Vec::new(),
                last_seq: 0,
                replay_available: true,
                no_permission: false,
                on_main: false,
                model_label: None,
                prompt_history: typed_history(Some("last submitted")),
                composing_buffer: Some(draft.into()),
                agent_state: None,
                authenticating: false,
            }],
            recent_snippets: Vec::new(),
            dismissed_updates: Vec::new(),
        });
        assert_eq!(stack.composing_of(TerminalId(1)), Some(draft));
        assert_eq!(
            stack.recall_prompt(),
            Some((TerminalId(1), draft.to_string()))
        );
        assert_eq!(stack.composing_of(TerminalId(1)), Some(draft));
        assert_eq!(
            stack.last_user_message_of(TerminalId(1)),
            Some("last submitted"),
        );
    }

    /// Recall prefers the in-flight draft; with none it falls back to
    /// the last submitted message; with neither there's nothing to
    /// recall.
    #[test]
    fn recall_prefers_draft_then_last_message() {
        let mut stack = stack_with_agent();
        assert_eq!(stack.recall_prompt(), None, "nothing typed or sent yet");

        stack
            .terminals
            .get_mut(&TerminalId(1))
            .unwrap()
            .push_prompt(lazybox_ipc::UserPrompt {
                text: "previous prompt".into(),
                timestamp_ms: 0,
                source: lazybox_ipc::PromptSource::Typed,
            });
        assert_eq!(
            stack.recall_prompt(),
            Some((TerminalId(1), "previous prompt".into())),
            "falls back to the last submitted message",
        );
        assert_eq!(
            stack.composing_of(TerminalId(1)),
            Some("previous prompt"),
            "recalled text becomes the mirrored in-flight draft",
        );
        stack
            .terminals
            .get_mut(&TerminalId(1))
            .unwrap()
            .composing
            .clear();

        let mut cmds = Vec::new();
        type_chars(&mut stack, "new draft", &mut cmds);
        assert_eq!(
            stack.recall_prompt(),
            Some((TerminalId(1), "new draft".into())),
            "an in-flight draft wins over the last message",
        );
    }

    /// Recall is agent-only: a shell has no meaningful "last prompt".
    #[test]
    fn recall_is_none_for_a_shell() {
        let sk = SessionKey::new("github:o/r#1");
        let mut stack = TerminalStack::new(PaneId::new(0));
        stack.on_event(&Event::TerminalSpawned {
            model_label: None,
            terminal_id: TerminalId(1),
            session_key: sk.clone(),
            kind: TerminalKind::Shell,
            no_permission: false,
            on_main: false,
        });
        stack.set_active_session(Some(sk));
        assert_eq!(stack.recall_prompt(), None);
    }
}

#[cfg(test)]
mod spawn_projection_tests {
    //! #206: the footer spawn spinner is a projection of the live
    //! terminal set, cleared by `spawn_satisfied` rather than by a
    //! single must-arrive `TerminalSpawned` event.
    use super::*;

    fn spawn(stack: &mut TerminalStack, id: u64, sk: &SessionKey, kind: TerminalKind) {
        stack.on_event(&Event::TerminalSpawned {
            model_label: None,
            terminal_id: TerminalId(id),
            session_key: sk.clone(),
            kind,
            no_permission: false,
            on_main: false,
        });
    }

    #[test]
    fn agent_spawn_satisfied_once_runner_exists() {
        let sk = SessionKey::new("github:o/r#1");
        let kind = TerminalKind::Agent("claude".into());
        let mut stack = TerminalStack::new(PaneId::new(0));
        // No terminal yet → spinner stays lit.
        assert!(!stack.spawn_satisfied(&sk, &kind, 0));
        // The agent terminal lands → satisfied via the singleton runner,
        // independent of any explicit clear event.
        spawn(&mut stack, 1, &sk, kind.clone());
        assert!(stack.spawn_satisfied(&sk, &kind, 0));
    }

    #[test]
    fn agent_spawn_ignores_a_terminal_in_another_session() {
        let target = SessionKey::new("github:o/r#1");
        let other = SessionKey::new("github:o/r#2");
        let kind = TerminalKind::Agent("claude".into());
        let mut stack = TerminalStack::new(PaneId::new(0));
        spawn(&mut stack, 1, &other, kind.clone());
        // A concurrent spawn into a DIFFERENT workspace must not satisfy
        // ours — the old "any TerminalSpawned clears the spinner" bug.
        assert!(!stack.spawn_satisfied(&target, &kind, 0));
    }

    #[test]
    fn shell_spawn_satisfied_when_count_rises_above_baseline() {
        let sk = SessionKey::new("github:o/r#1");
        let kind = TerminalKind::Shell;
        let mut stack = TerminalStack::new(PaneId::new(0));
        // Session already has one shell when the new spawn is sent.
        spawn(&mut stack, 1, &sk, TerminalKind::Shell);
        let baseline = stack.terminal_count_for(&sk);
        assert_eq!(baseline, 1);
        // Not yet satisfied — the count hasn't risen above the baseline.
        assert!(!stack.spawn_satisfied(&sk, &kind, baseline));
        // The second shell lands → count exceeds baseline → satisfied.
        spawn(&mut stack, 2, &sk, TerminalKind::Shell);
        assert!(stack.spawn_satisfied(&sk, &kind, baseline));
    }
}

#[cfg(test)]
mod agent_crash_tests {
    //! A spawned agent that exits *abnormally* on its own (crash,
    //! killed binary — #356 — or dead-on-arrival — #367) must NOT take
    //! its workspace down with it: the pane stays, frozen on its last
    //! screen, and offers a restart. A *clean* agent exit (code 0 after
    //! it engaged or matured past the grace window), a shell exit, or an
    //! explicit user close (`]]x`) tears the pane down (#367).
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn active_stack(id: u64, sk: &SessionKey, kind: TerminalKind) -> TerminalStack {
        let mut stack = TerminalStack::new(PaneId::new(0));
        stack.set_active_session(Some(sk.clone()));
        stack.on_event(&Event::TerminalSpawned {
            model_label: None,
            terminal_id: TerminalId(id),
            session_key: sk.clone(),
            kind,
            no_permission: false,
            on_main: false,
        });
        stack
    }

    #[test]
    fn agent_crash_keeps_pane_and_records_exit() {
        let sk = SessionKey::new("github:o/r#1");
        let mut stack = active_stack(1, &sk, TerminalKind::Agent("codex".into()));

        stack.on_event(&Event::TerminalExited {
            terminal_id: TerminalId(1),
            exit_code: Some(1),
            last_output: None,
        });

        let slot = stack
            .terminals
            .get(&TerminalId(1))
            .expect("crashed agent pane must survive");
        assert!(
            matches!(slot.exited, Some(TerminalExit { code: Some(1), .. })),
            "the exit code is recorded so the banner can show it",
        );
    }

    #[test]
    fn agent_crash_without_exit_code_still_keeps_pane() {
        let sk = SessionKey::new("github:o/r#1");
        let mut stack = active_stack(1, &sk, TerminalKind::Agent("codex".into()));

        // Death by signal (the Homebrew-swap case) reports no code.
        stack.on_event(&Event::TerminalExited {
            terminal_id: TerminalId(1),
            exit_code: None,
            last_output: None,
        });

        assert!(matches!(
            stack
                .terminals
                .get(&TerminalId(1))
                .map(|s| s.exited.clone()),
            Some(Some(TerminalExit { code: None, .. })),
        ));
    }

    #[test]
    fn shell_exit_removes_pane() {
        let sk = SessionKey::new("github:o/r#1");
        let mut stack = active_stack(1, &sk, TerminalKind::Shell);

        stack.on_event(&Event::TerminalExited {
            terminal_id: TerminalId(1),
            exit_code: Some(0),
            last_output: None,
        });

        assert!(
            stack.terminals.get(&TerminalId(1)).is_none(),
            "a shell exiting closes its pane like any terminal",
        );
    }

    #[test]
    fn user_close_removes_agent_pane() {
        let sk = SessionKey::new("github:o/r#1");
        let mut stack = active_stack(1, &sk, TerminalKind::Agent("codex".into()));

        // `]]x` → close_focused_tile tags the id, then the daemon's kill
        // echoes TerminalExited. That must tear the pane down, not leave
        // an "exited" banner on a terminal the user deliberately closed.
        let mut cmds = Vec::new();
        stack.close_focused_tile(&mut cmds);
        assert!(
            matches!(
                cmds.as_slice(),
                [Command::Close {
                    terminal_id: TerminalId(1),
                    ..
                }]
            ),
            "close pushes a daemon-side kill",
        );
        stack.on_event(&Event::TerminalExited {
            terminal_id: TerminalId(1),
            exit_code: Some(0),
            last_output: None,
        });

        assert!(stack.terminals.get(&TerminalId(1)).is_none());
    }

    #[test]
    fn restart_key_respawns_same_agent() {
        let sk = SessionKey::new("github:o/r#1");
        let mut stack = active_stack(1, &sk, TerminalKind::Agent("codex".into()));
        stack.on_event(&Event::TerminalExited {
            terminal_id: TerminalId(1),
            exit_code: Some(1),
            last_output: None,
        });

        let mut cmds = Vec::new();
        let outcome = stack.handle_key(
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
            &mut cmds,
        );

        assert!(matches!(outcome, PaneOutcome::Consumed));
        assert!(matches!(
            cmds.as_slice(),
            [Command::ResumeAgent {
                terminal_id: TerminalId(1),
            }]
        ));
    }

    #[test]
    fn failed_auth_restart_retries_the_stable_recovery_identity() {
        let sk = SessionKey::new("github:o/r#1");
        let mut stack = active_stack(1, &sk, TerminalKind::Agent("codex".into()));
        stack.on_event(&Event::TerminalReplaced {
            old_terminal_id: TerminalId(1),
            terminal_id: TerminalId(2),
            session_key: sk,
            kind: TerminalKind::Agent("codex".into()),
            no_permission: false,
            on_main: false,
            model_label: None,
            authenticating: true,
        });
        stack.on_event(&Event::AgentAuthFinished {
            recovery_terminal_id: TerminalId(1),
            terminal_id: TerminalId(2),
            display_name: "Codex".into(),
            success: false,
            error: Some("login failed".into()),
        });

        let mut cmds = Vec::new();
        let outcome = stack.handle_key(
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
            &mut cmds,
        );

        assert!(matches!(outcome, PaneOutcome::Consumed));
        assert!(matches!(
            cmds.as_slice(),
            [Command::ReauthenticateAgent {
                terminal_id: TerminalId(1),
                switch_account: true,
            }]
        ));
    }

    #[test]
    fn closing_a_failed_auth_pane_notifies_the_daemon() {
        let sk = SessionKey::new("github:o/r#1");
        let mut stack = active_stack(1, &sk, TerminalKind::Agent("claude".into()));
        stack.on_event(&Event::TerminalReplaced {
            old_terminal_id: TerminalId(1),
            terminal_id: TerminalId(2),
            session_key: sk,
            kind: TerminalKind::Agent("claude".into()),
            no_permission: false,
            on_main: false,
            model_label: None,
            authenticating: true,
        });
        stack.on_event(&Event::AgentAuthFinished {
            recovery_terminal_id: TerminalId(1),
            terminal_id: TerminalId(2),
            display_name: "Claude Code".into(),
            success: false,
            error: Some("login failed".into()),
        });

        let mut cmds = Vec::new();
        stack.close_focused_tile(&mut cmds);

        assert!(matches!(
            cmds.as_slice(),
            [Command::Close {
                terminal_id: TerminalId(2),
                ..
            }]
        ));
        assert!(stack.terminals.contains_key(&TerminalId(2)));
    }

    #[test]
    fn keys_do_not_reach_a_dead_pty() {
        let sk = SessionKey::new("github:o/r#1");
        let mut stack = active_stack(1, &sk, TerminalKind::Agent("codex".into()));
        stack.on_event(&Event::TerminalExited {
            terminal_id: TerminalId(1),
            exit_code: Some(1),
            last_output: None,
        });

        // A printable key that isn't the restart affordance is swallowed
        // rather than written into the gone PTY.
        let mut cmds = Vec::new();
        let outcome = stack.handle_key(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
            &mut cmds,
        );
        assert!(matches!(outcome, PaneOutcome::Consumed));
        assert!(cmds.is_empty(), "no Write reaches a dead terminal");
    }

    #[test]
    fn exact_replacement_supersedes_only_the_named_exited_pane() {
        let sk = SessionKey::new("github:o/r#1");
        let mut stack = active_stack(1, &sk, TerminalKind::Agent("codex".into()));
        {
            let slot = stack.terminals.get_mut(&TerminalId(1)).unwrap();
            slot.prompt_history.push(lazybox_ipc::UserPrompt {
                text: "preserve this prompt".into(),
                timestamp_ms: 1,
                source: lazybox_ipc::PromptSource::Typed,
            });
            slot.composing = "preserve this draft".into();
        }
        stack.on_event(&Event::TerminalSpawned {
            model_label: None,
            terminal_id: TerminalId(3),
            session_key: sk.clone(),
            kind: TerminalKind::Shell,
            no_permission: false,
            on_main: false,
        });
        stack.set_layout(lazybox_core::SessionLayout::Splits {
            tree: lazybox_core::TileTree::HSplit {
                left: Box::new(lazybox_core::TileTree::Leaf { terminal_id: 1 }),
                right: Box::new(lazybox_core::TileTree::Leaf { terminal_id: 3 }),
                ratio: 50,
            },
            focused: vec![0],
        });
        stack.on_event(&Event::TerminalExited {
            terminal_id: TerminalId(1),
            exit_code: Some(1),
            last_output: None,
        });

        stack.on_event(&Event::TerminalReplaced {
            old_terminal_id: TerminalId(1),
            model_label: None,
            terminal_id: TerminalId(2),
            session_key: sk.clone(),
            kind: TerminalKind::Agent("codex".into()),
            no_permission: false,
            on_main: false,
            authenticating: false,
        });

        assert!(stack.terminals.get(&TerminalId(1)).is_none());
        let resumed = stack.terminals.get(&TerminalId(2)).unwrap();
        assert_eq!(resumed.prompt_history[0].text, "preserve this prompt");
        assert_eq!(resumed.composing, "preserve this draft");
        let lazybox_core::SessionLayout::Splits { tree, focused } = &stack.layout else {
            panic!("resumed terminal must keep the split layout");
        };
        assert_eq!(tree.leaves(), vec![2, 3]);
        assert_eq!(focused, &[0]);
        assert_eq!(
            stack.visible_terminals(),
            vec![TerminalId(2), TerminalId(3)]
        );
    }

    #[test]
    fn replacement_event_is_idempotent_after_reconnect_snapshot() {
        let sk = SessionKey::new("github:o/r#1");
        let mut stack = active_stack(2, &sk, TerminalKind::Agent("codex".into()));
        stack.on_event(&Event::AgentAuthOutput {
            terminal_id: TerminalId(2),
            bytes: b"provider login screen".to_vec(),
            first_seq: 1,
            seq: 1,
        });

        stack.on_event(&Event::TerminalReplaced {
            old_terminal_id: TerminalId(1),
            terminal_id: TerminalId(2),
            session_key: sk,
            kind: TerminalKind::Agent("codex".into()),
            no_permission: true,
            on_main: true,
            model_label: Some("large".into()),
            authenticating: true,
        });

        let auth = stack.terminals.get(&TerminalId(2)).expect("auth terminal");
        assert_eq!(auth.recent, b"provider login screen");
        assert_eq!(auth.last_seq, 1);
        assert!(auth.no_permission);
        assert!(auth.on_main);
        assert_eq!(auth.model_label.as_deref(), Some("large"));
        assert_eq!(auth.auth_recovery_id, Some(TerminalId(1)));
    }

    #[test]
    fn resumed_agent_replaces_the_auth_terminal_in_place() {
        let sk = SessionKey::new("github:o/r#1");
        let mut stack = active_stack(1, &sk, TerminalKind::Agent("claude".into()));
        stack.terminals.get_mut(&TerminalId(1)).unwrap().composing = "draft".into();
        stack.on_event(&Event::AgentAuthProgress {
            recovery_terminal_id: TerminalId(1),
            terminal_id: TerminalId(1),
            phase: lazybox_ipc::AgentAuthPhase::LoginInteractive,
        });

        stack.on_event(&Event::TerminalReplaced {
            old_terminal_id: TerminalId(1),
            model_label: Some("Opus".into()),
            terminal_id: TerminalId(2),
            session_key: sk,
            kind: TerminalKind::Agent("claude".into()),
            no_permission: true,
            on_main: true,
            authenticating: false,
        });

        assert!(stack.terminals.get(&TerminalId(1)).is_none());
        let resumed = stack.terminals.get(&TerminalId(2)).unwrap();
        assert_eq!(resumed.composing, "draft");
        assert!(!resumed.authenticating);
        assert!(resumed.no_permission);
        assert!(resumed.on_main);
        assert_eq!(resumed.model_label.as_deref(), Some("Opus"));
        assert_eq!(stack.focused_terminal_id(), Some(TerminalId(2)));
    }

    #[test]
    fn authentication_output_starts_a_fresh_sequence_on_the_replacement_terminal() {
        let sk = SessionKey::new("github:o/r#1");
        let mut stack = active_stack(1, &sk, TerminalKind::Agent("codex".into()));
        stack.on_event(&Event::TerminalOutput {
            terminal_id: TerminalId(1),
            bytes: b"blocked agent output".to_vec(),
            first_seq: 10,
            seq: 10,
        });
        stack.on_event(&Event::TerminalReplaced {
            old_terminal_id: TerminalId(1),
            terminal_id: TerminalId(2),
            session_key: sk,
            kind: TerminalKind::Agent("codex".into()),
            no_permission: false,
            on_main: false,
            model_label: None,
            authenticating: true,
        });
        stack.on_event(&Event::AgentAuthOutput {
            terminal_id: TerminalId(2),
            bytes: b"provider login".to_vec(),
            first_seq: 1,
            seq: 1,
        });

        let auth = stack.terminals.get(&TerminalId(2)).expect("auth terminal");
        assert_eq!(auth.last_seq, 1);
        assert_eq!(auth.recent, b"provider login");
        assert!(!auth.sync.is_desynced());
    }

    #[test]
    fn exited_pane_renders_a_restart_banner() {
        let sk = SessionKey::new("github:o/r#1");
        let mut stack = active_stack(1, &sk, TerminalKind::Agent("codex".into()));
        stack.on_event(&Event::TerminalExited {
            terminal_id: TerminalId(1),
            exit_code: Some(137),
            last_output: None,
        });

        let backend = TestBackend::new(80, 24);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| stack.render(Rect::new(0, 0, 80, 24), f, true))
            .unwrap();
        let buf = term.backend().buffer().clone();
        let screen: String = (0..24)
            .map(|y| (0..80).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            screen.contains("agent exited (code 137)"),
            "banner shows the exit code:\n{screen}",
        );
        assert!(
            screen.contains("restart"),
            "banner offers a restart:\n{screen}",
        );
    }

    #[test]
    fn dead_on_arrival_pane_reads_as_failed_and_shows_the_reason() {
        // A codex that dies on arrival with code 0 (#368): a freshly
        // spawned agent that never engaged and exits `code 0` inside the
        // grace window is dead-on-arrival (#367), so the banner must say
        // "failed to start" — not "exited", which reads as success — and
        // the captured error must fill the otherwise-blank pane.
        let sk = SessionKey::new("github:o/r#1");
        let mut stack = active_stack(1, &sk, TerminalKind::Agent("codex".into()));
        stack.on_event(&Event::TerminalExited {
            terminal_id: TerminalId(1),
            exit_code: Some(0),
            last_output: Some("Error: not logged in\nrun `codex login`".into()),
        });

        let backend = TestBackend::new(80, 24);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| stack.render(Rect::new(0, 0, 80, 24), f, true))
            .unwrap();
        let buf = term.backend().buffer().clone();
        let screen: String = (0..24)
            .map(|y| (0..80).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            screen.contains("agent failed to start (code 0)"),
            "an immediate code-0 exit is surfaced as a failure:\n{screen}",
        );
        assert!(
            screen.contains("not logged in") && screen.contains("codex login"),
            "the captured output is painted over the blank pane:\n{screen}",
        );
    }

    #[test]
    fn dead_on_arrival_survives_the_preceding_exited_state_event() {
        // Regression: the real teardown broadcasts `AgentState::Exited`
        // right before `TerminalExited` (#357/#369). `Exited` must NOT
        // count as engagement, or `did_work` flips true and the
        // dead-on-arrival launch reads as a clean exit and auto-closes —
        // silently reaping the pane the user needs to see and restart.
        let sk = SessionKey::new("github:o/r#1");
        let mut stack = active_stack(1, &sk, TerminalKind::Agent("codex".into()));
        stack.on_event(&Event::AgentState {
            terminal_id: TerminalId(1),
            session_key: sk.clone(),
            state: lazybox_ipc::AgentState::Exited { code: Some(0) },
        });
        stack.on_event(&Event::TerminalExited {
            terminal_id: TerminalId(1),
            exit_code: Some(0),
            last_output: Some("Error: not logged in".into()),
        });

        let slot = stack
            .terminals
            .get(&TerminalId(1))
            .expect("a dead-on-arrival pane must survive, not auto-close");
        assert!(
            matches!(
                &slot.exited,
                Some(TerminalExit {
                    dead_on_arrival: true,
                    ..
                })
            ),
            "it must still classify as dead-on-arrival despite the Exited pill",
        );
    }

    #[test]
    fn a_different_agent_spawn_leaves_the_exited_pane() {
        let sk = SessionKey::new("github:o/r#1");
        let mut stack = active_stack(1, &sk, TerminalKind::Agent("codex".into()));
        stack.on_event(&Event::TerminalExited {
            terminal_id: TerminalId(1),
            exit_code: Some(1),
            last_output: None,
        });

        // Spawning a *different* agent in the same workspace must not
        // reap the crashed codex pane — its banner is still relevant.
        stack.on_event(&Event::TerminalSpawned {
            model_label: None,
            terminal_id: TerminalId(2),
            session_key: sk.clone(),
            kind: TerminalKind::Agent("claude".into()),
            no_permission: false,
            on_main: false,
        });

        assert!(stack.terminals.get(&TerminalId(1)).is_some());
        assert!(stack.terminals.get(&TerminalId(2)).is_some());
    }

    /// Backdate a slot's spawn so it reads as having run past the
    /// dead-on-arrival grace window.
    fn age_slot(stack: &mut TerminalStack, id: TerminalId, by: std::time::Duration) {
        let slot = stack.terminals.get_mut(&id).expect("slot");
        slot.spawned_at = slot.spawned_at.checked_sub(by).expect("backdate");
    }

    #[test]
    fn clean_exit_after_grace_auto_closes() {
        // The regression #367 targets: an agent that ran for a while and
        // exited cleanly (code 0) auto-closes as it did pre-#356 — no
        // lingering `[exited]` tile to `]]x` by hand.
        let sk = SessionKey::new("github:o/r#1");
        let mut stack = active_stack(1, &sk, TerminalKind::Agent("claude".into()));
        age_slot(
            &mut stack,
            TerminalId(1),
            std::time::Duration::from_secs(30),
        );

        stack.on_event(&Event::TerminalExited {
            terminal_id: TerminalId(1),
            exit_code: Some(0),
            last_output: None,
        });

        assert!(
            stack.terminals.get(&TerminalId(1)).is_none(),
            "a clean, matured agent exit auto-closes its pane",
        );
    }

    #[test]
    fn clean_exit_after_engaging_auto_closes_even_when_fast() {
        // An agent that reached a working/done state has "come to rest";
        // a subsequent clean exit auto-closes even inside the grace
        // window — engagement, not just elapsed time, satisfies "clean".
        let sk = SessionKey::new("github:o/r#1");
        let mut stack = active_stack(1, &sk, TerminalKind::Agent("claude".into()));
        stack.on_event(&Event::AgentState {
            terminal_id: TerminalId(1),
            session_key: sk.clone(),
            state: lazybox_ipc::AgentState::Working,
        });

        stack.on_event(&Event::TerminalExited {
            terminal_id: TerminalId(1),
            exit_code: Some(0),
            last_output: None,
        });

        assert!(
            stack.terminals.get(&TerminalId(1)).is_none(),
            "an engaged agent's clean exit auto-closes even before the grace window",
        );
    }

    #[test]
    fn dead_on_arrival_clean_exit_keeps_pane() {
        // Exit code alone isn't enough (#357): an agent that exits code 0
        // almost immediately without ever engaging failed to launch —
        // keep the pane frozen with a restart, don't silently reap it.
        let sk = SessionKey::new("github:o/r#1");
        let mut stack = active_stack(1, &sk, TerminalKind::Agent("codex".into()));

        stack.on_event(&Event::TerminalExited {
            terminal_id: TerminalId(1),
            exit_code: Some(0),
            last_output: None,
        });

        assert!(
            matches!(
                stack
                    .terminals
                    .get(&TerminalId(1))
                    .map(|s| s.exited.clone()),
                Some(Some(TerminalExit { code: Some(0), .. })),
            ),
            "a dead-on-arrival code-0 exit keeps the pane for a restart",
        );
    }

    #[test]
    fn config_threshold_gates_dead_on_arrival() {
        // The grace window is configurable: shrink it so an exit that
        // would be dead-on-arrival under the default counts as matured
        // and auto-closes.
        let sk = SessionKey::new("github:o/r#1");
        let mut stack = active_stack(1, &sk, TerminalKind::Agent("codex".into()));
        let ui = lazybox_config::UiDefaults {
            agent_dead_on_arrival: std::time::Duration::ZERO,
            ..Default::default()
        };
        stack.apply_ui_defaults(&ui);

        stack.on_event(&Event::TerminalExited {
            terminal_id: TerminalId(1),
            exit_code: Some(0),
            last_output: None,
        });

        assert!(
            stack.terminals.get(&TerminalId(1)).is_none(),
            "a zero grace window makes even an instant clean exit auto-close",
        );
    }
}

/// tmux-style zoom (`]]z`) and the per-tile status headers of the
/// multi-agent grid (#1057).
#[cfg(test)]
mod zoom_and_tile_header_tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    const W: u16 = 80;
    const H: u16 = 24;

    /// A single session with two agent tiles side-by-side; focus on the
    /// left (terminal 1).
    fn two_tile_grid() -> (TerminalStack, SessionKey) {
        let sk = SessionKey::new("github:o/r#1");
        let mut stack = TerminalStack::new(PaneId::new(0));
        for id in [1u64, 2] {
            stack.on_event(&Event::TerminalSpawned {
                model_label: None,
                terminal_id: TerminalId(id),
                session_key: sk.clone(),
                kind: TerminalKind::Agent("claude".into()),
                no_permission: false,
                on_main: false,
            });
        }
        stack.set_active_session(Some(sk.clone()));
        stack.set_layout(lazybox_core::SessionLayout::Splits {
            tree: lazybox_core::TileTree::HSplit {
                left: Box::new(lazybox_core::TileTree::Leaf { terminal_id: 1 }),
                right: Box::new(lazybox_core::TileTree::Leaf { terminal_id: 2 }),
                ratio: 50,
            },
            focused: vec![0],
        });
        (stack, sk)
    }

    fn feed(stack: &mut TerminalStack, id: u64, text: &str) {
        stack.on_event(&Event::TerminalOutput {
            terminal_id: TerminalId(id),
            bytes: format!("{text}\r\n").into_bytes(),
            first_seq: 1,
            seq: 1,
        });
    }

    fn rows(stack: &mut TerminalStack) -> Vec<String> {
        let backend = TestBackend::new(W, H);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| stack.render(Rect::new(0, 0, W, H), f, true))
            .unwrap();
        let buf = term.backend().buffer().clone();
        (0..H)
            .map(|y| {
                (0..W)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn zoom_toggles_only_in_a_multi_tile_grid() {
        // Tabs (single terminal): nothing to zoom.
        let sk = SessionKey::new("github:o/r#9");
        let mut tabs = TerminalStack::new(PaneId::new(0));
        tabs.on_event(&Event::TerminalSpawned {
            model_label: None,
            terminal_id: TerminalId(1),
            session_key: sk.clone(),
            kind: TerminalKind::Agent("claude".into()),
            no_permission: false,
            on_main: false,
        });
        tabs.set_active_session(Some(sk));
        assert_eq!(tabs.toggle_zoom(), None);
        assert!(!tabs.is_zoomed());

        // Splits grid: toggle in and back out.
        let (mut stack, _sk) = two_tile_grid();
        assert_eq!(stack.toggle_zoom(), Some(true));
        assert!(stack.is_zoomed());
        assert_eq!(stack.toggle_zoom(), Some(false));
        assert!(!stack.is_zoomed());
    }

    #[test]
    fn zoom_clears_when_the_tree_changes_underfoot() {
        // Moving tile focus un-zooms so the grid is visible to navigate.
        let (mut stack, _sk) = two_tile_grid();
        stack.toggle_zoom();
        let mut cmds = Vec::new();
        stack.move_tile_focus(lazybox_core::TileDirection::Right, &mut cmds);
        assert!(!stack.is_zoomed(), "moving focus restores the grid");

        // Closing a tile collapses the tree — drop zoom.
        let (mut stack, _sk) = two_tile_grid();
        stack.toggle_zoom();
        let mut cmds = Vec::new();
        stack.close_focused_tile(&mut cmds);
        assert!(!stack.is_zoomed(), "closing a tile restores the grid");

        // A new split reveals the freshly-spawned tile.
        let (mut stack, _sk) = two_tile_grid();
        stack.toggle_zoom();
        let mut cmds = Vec::new();
        stack.split_tile(PendingSplit::Vertical, &mut cmds);
        assert!(!stack.is_zoomed(), "a new split restores the grid");

        // Switching workspaces drops the zoom of the one we left.
        let (mut stack, _sk) = two_tile_grid();
        stack.toggle_zoom();
        stack.set_active_session(Some(SessionKey::new("github:o/r#2")));
        assert!(!stack.is_zoomed(), "a session switch clears zoom");
    }

    #[test]
    fn zoomed_grid_renders_only_the_focused_tile() {
        let (mut stack, _sk) = two_tile_grid();
        feed(&mut stack, 1, "LEFTAGENT");
        feed(&mut stack, 2, "RIGHTAGENT");

        let grid = rows(&mut stack);
        assert!(
            grid.iter().any(|r| r.contains("LEFTAGENT"))
                && grid.iter().any(|r| r.contains("RIGHTAGENT")),
            "the grid shows both tiles: {grid:?}",
        );

        // Focus is on the left tile (terminal 1); zoom hides the right.
        stack.toggle_zoom();
        let zoomed = rows(&mut stack);
        assert!(
            zoomed.iter().any(|r| r.contains("LEFTAGENT")),
            "the focused tile stays visible when zoomed: {zoomed:?}",
        );
        assert!(
            !zoomed.iter().any(|r| r.contains("RIGHTAGENT")),
            "the background tile is hidden when zoomed: {zoomed:?}",
        );
        assert!(
            zoomed.iter().any(|r| r.contains("zoom")),
            "the zoomed tile is marked: {zoomed:?}",
        );
    }

    #[test]
    fn tile_header_carries_runner_state_and_model() {
        let (mut stack, _sk) = two_tile_grid();
        {
            let slot = stack.terminals.get_mut(&TerminalId(2)).unwrap();
            slot.agent_state = lazybox_ipc::AgentState::InputNeeded;
            slot.model_label = Some("Opus".into());
        }
        let header = stack.tile_header_line(TerminalId(2), false, false, 60);
        let text: String = header.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("claude"), "runner name in header: {text}");
        assert!(
            text.contains("asking"),
            "agent-state chip in header: {text}"
        );
        assert!(text.contains("Opus"), "model badge in header: {text}");
    }

    #[test]
    fn background_asking_tile_header_is_highlighted() {
        let (mut stack, _sk) = two_tile_grid();
        stack.terminals.get_mut(&TerminalId(2)).unwrap().agent_state =
            lazybox_ipc::AgentState::InputNeeded;
        let theme = crate::theme::current();

        // A background asking tile paints its whole bar warn+bold so it
        // stands out while you type in another tile.
        let bg = stack.tile_header_line(TerminalId(2), false, false, 40);
        assert!(
            bg.spans
                .iter()
                .any(|s| s.style.fg == Some(theme.warn)
                    && s.style.add_modifier.contains(Modifier::BOLD)),
            "background asking tile is warn+bold",
        );

        // The same agent while focused uses the accent focus colour — you
        // are already looking at it, so no attention pull.
        let fg = stack.tile_header_line(TerminalId(2), true, false, 40);
        assert!(
            fg.spans.iter().any(|s| s.style.fg == Some(theme.accent)),
            "the focused tile uses the accent rule, not the attention warn",
        );
    }

    /// Three tiles: tile 1 on the left, tiles 2 & 3 stacked on the right;
    /// focus on tile 1. Big enough that removing one tile leaves a real
    /// Splits grid (not a downgrade to Tabs).
    fn three_tile_grid() -> TerminalStack {
        let sk = SessionKey::new("github:o/r#3");
        let mut stack = TerminalStack::new(PaneId::new(0));
        for id in [1u64, 2, 3] {
            stack.on_event(&Event::TerminalSpawned {
                model_label: None,
                terminal_id: TerminalId(id),
                session_key: sk.clone(),
                kind: TerminalKind::Agent("claude".into()),
                no_permission: false,
                on_main: false,
            });
        }
        stack.set_active_session(Some(sk));
        stack.set_layout(lazybox_core::SessionLayout::Splits {
            tree: lazybox_core::TileTree::HSplit {
                left: Box::new(lazybox_core::TileTree::Leaf { terminal_id: 1 }),
                right: Box::new(lazybox_core::TileTree::HSplit {
                    left: Box::new(lazybox_core::TileTree::Leaf { terminal_id: 2 }),
                    right: Box::new(lazybox_core::TileTree::Leaf { terminal_id: 3 }),
                    ratio: 50,
                }),
                ratio: 50,
            },
            focused: vec![0],
        });
        stack
    }

    #[test]
    fn zoom_exits_when_a_grid_tile_is_removed_but_survives_another_sessions_exit() {
        // Removing the zoomed (focused) tile → zoom drops back to the grid.
        let mut stack = three_tile_grid();
        assert_eq!(stack.toggle_zoom(), Some(true));
        assert_eq!(stack.focused_terminal_id(), Some(TerminalId(1)));
        stack.drop_slot(TerminalId(1));
        assert!(!stack.is_zoomed(), "removing the zoomed tile exits zoom");

        // Removing a BACKGROUND tile also exits zoom: `drop_slot` re-points
        // focus at the removed tile's sibling, so holding the zoom would
        // silently show a different agent than the user chose. Dropping
        // back to the grid is the honest outcome.
        let mut stack = three_tile_grid();
        stack.toggle_zoom();
        stack.drop_slot(TerminalId(2));
        assert!(
            !stack.is_zoomed(),
            "removing a background grid tile also exits zoom",
        );

        // A terminal in a DIFFERENT session isn't part of this grid — its
        // exit leaves the active session's zoom untouched.
        let mut stack = three_tile_grid();
        stack.toggle_zoom();
        let other = SessionKey::new("github:o/r#99");
        stack.on_event(&Event::TerminalSpawned {
            model_label: None,
            terminal_id: TerminalId(42),
            session_key: other,
            kind: TerminalKind::Shell,
            no_permission: false,
            on_main: false,
        });
        assert!(
            stack.is_zoomed(),
            "spawning another session must not clear zoom"
        );
        stack.drop_slot(TerminalId(42));
        assert!(
            stack.is_zoomed(),
            "an unrelated session's terminal exit keeps the zoom",
        );
    }

    #[test]
    fn agent_state_badge_is_one_exhaustive_mapping_for_both_surfaces() {
        use lazybox_ipc::AgentState;
        let t = crate::theme::current();
        let label = |s, exited, compact| {
            TerminalStack::agent_state_badge(s, exited, compact, t).map(|(l, _)| l)
        };

        // `exited` overrides the live state on both surfaces.
        assert_eq!(label(AgentState::Working, true, true), Some("✗ exited"));
        assert_eq!(label(AgentState::Working, true, false), Some("✗ exited"));

        // The asking state is the ONLY label that differs by surface: the
        // tile grid's terse `● asking` vs the tab strip's `! needs input`.
        assert_eq!(
            label(AgentState::InputNeeded, false, true),
            Some("● asking")
        );
        assert_eq!(
            label(AgentState::InputNeeded, false, false),
            Some("! needs input")
        );

        for compact in [true, false] {
            // Working / Done / blocked states are identical across both
            // surfaces (only the asking label differs).
            assert_eq!(
                label(AgentState::Working, false, compact),
                Some("· working")
            );
            assert_eq!(label(AgentState::Done, false, compact), Some("✓ done"));
            // A rate-limited agent needs you too — it shows the `⏳` pill
            // on both surfaces, not a blank slot.
            assert_eq!(
                label(AgentState::LimitReached, false, compact),
                Some("⏳ limited")
            );
            assert_eq!(
                label(AgentState::CreditExhausted, false, compact),
                Some("¢ no credit")
            );
            // Silent states render nothing on BOTH surfaces — no
            // per-surface drift.
            assert_eq!(label(AgentState::Idle, false, compact), None);
            assert_eq!(
                label(AgentState::Exited { code: Some(0) }, false, compact),
                None
            );
        }
    }

    #[test]
    fn background_rate_limited_tile_is_highlighted_like_an_asking_one() {
        let (mut stack, _sk) = two_tile_grid();
        stack.terminals.get_mut(&TerminalId(2)).unwrap().agent_state =
            lazybox_ipc::AgentState::LimitReached;
        let theme = crate::theme::current();

        // A background rate-limited tile pulls attention just like an
        // asking one: whole bar warn+bold, with the `⏳ limited` chip.
        let bg = stack.tile_header_line(TerminalId(2), false, false, 40);
        assert!(
            bg.spans
                .iter()
                .any(|s| s.style.fg == Some(theme.warn)
                    && s.style.add_modifier.contains(Modifier::BOLD)),
            "background rate-limited tile is warn+bold",
        );
        let text: String = bg.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("limited"), "shows the limited chip: {text}");
    }
}
