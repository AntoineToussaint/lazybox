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
use lazybox_ipc::{Command, Event, TerminalId, TerminalKind};
use lazybox_tui_term::GhosttyTerminal;
use libghostty_vt as vt;
use ratatui::Frame;
use ratatui::prelude::*;
use ratatui::widgets::*;
use std::collections::HashMap;
use std::collections::HashSet;

/// Default cell grid size for new terminals before the first
/// resize-from-render. Sized to match a typical agent default; the
/// renderer overrides as soon as it knows the actual viewport.
const DEFAULT_COLS: u16 = 120;
const DEFAULT_ROWS: u16 = 32;

/// Cap for the per-terminal recent-output buffer.
///
/// libghostty-vt holds the canonical cell grid for rendering, but
/// agent-state detection (Claude's "Are you sure?" prompts, error
/// markers, etc.) needs to pattern-match raw bytes — re-extracting
/// them from the cell grid loses the escape sequences. So we keep a
/// rolling window of the last ~4 KiB of bytes the daemon streamed in.
/// 4 KiB is enough to span any prompt the agents have shipped so far.
pub const RECENT_OUTPUT_CAP: usize = 4 * 1024;

/// Cap on the raw bytes buffered for a terminal that isn't currently
/// on screen. Off-screen terminals defer the (expensive) VT parse and
/// just stash bytes here; the parser is fed lazily on the first render
/// after the terminal becomes visible. This bounds the *between-render*
/// backlog of a chatty hidden agent — when it overflows we keep the
/// most-recent tail and reset+refeed the parser on display. It is
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

/// A viewport scroll request — the entire vocabulary the scroll owner
/// accepts. Every scroll surface (wheel, `Shift-PgUp/PgDn`,
/// `Shift-Home/End`, per-tile wheel) speaks only these three verbs;
/// nothing outside `TerminalVt::scroll` pokes a raw offset or calls
/// `scroll_viewport` directly. That single choke point is what makes a
/// silent no-op impossible (the #42/#371 promise): a request either
/// moves the viewport or comes back with a typed [`ScrollOutcome`].
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

/// Outcome of a scroll attempt on a terminal. Used by the
/// orchestrator's mouse-wheel handler to surface why a scroll might
/// have looked like nothing happened — without this, "no scrollback
/// content yet" was indistinguishable from a broken Delta path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollOutcome {
    /// No focused terminal (Tabs mode with no active tab, or an
    /// empty session).
    NoTerminal,
    /// `total <= len`: the terminal hasn't produced enough output to
    /// fill the active area + spill into scrollback yet. `alternate`
    /// flags the special case where the inner program is on the
    /// alternate screen (claude/vim/less) — by design those have no
    /// scrollback, so it's the *program's* responsibility to paginate.
    NoScrollback { alternate: bool },
    /// Scroll succeeded. Carries the post-state for the footer notice.
    Moved { offset: u64, total: u64, len: u64 },
}

/// How a mouse-wheel tick over the focused terminal should be handled.
/// The wheel always means "scroll", but *who* scrolls depends on which
/// screen the inner program is on and whether it asked for mouse
/// reporting — so the orchestrator resolves the whole decision in one
/// focused-terminal lookup rather than probing several booleans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WheelRoute {
    /// Primary screen: the pane history is lazybox's, so scroll the
    /// local libghostty scrollback in-process (no daemon round trip).
    /// Covers plain shells AND primary-screen apps that track the mouse
    /// only for clicks (Claude Code) — they own no pager, so the wheel
    /// is always lazybox's, from the first frame of a fresh spawn
    /// (#321, #360).
    LocalScrollback,
    /// Alt-screen app that enabled mouse reporting (vim `mouse=a`,
    /// htop, less `--mouse`): it owns the only scrollable buffer, so
    /// forward the wheel as an SGR mouse report and let the app scroll.
    ForwardSgr,
    /// Alt-screen app that did NOT enable mouse reporting (less, man,
    /// the git pager, vim without `mouse`): it owns the visible buffer
    /// but there is no lazybox scrollback to move into and no mouse
    /// protocol to speak, so synthesize arrow-key presses — xterm's
    /// `alternateScroll`, which every terminal-in-terminal implements.
    /// `app_cursor` selects the SS3 (`ESC O A`) vs CSI (`ESC [ A`) form
    /// from the terminal's DECCKM (application-cursor-keys) mode.
    AlternateScrollArrows { app_cursor: bool },
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

/// True when `(col, row)` lies inside `rect` (half-open on the far
/// edges, matching how ratatui addresses cells).
fn rect_contains_point(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x
        && col < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

/// The leaf `terminal_id` whose rect contains `(col, row)`, walking the
/// tile tree with the EXACT geometry [`TerminalStack::render`] lays out
/// with (`render_tile_tree`): an `HSplit` gives its first child
/// `width * ratio / 100` columns (capped one short of the full width),
/// a one-column divider, then the remainder; a `VSplit` splits rows the
/// same way. Kept a pure function of `(tree, rect)` so it can be unit
/// tested against the renderer's split math without a frame.
fn tile_at(tree: &lazybox_core::TileTree, rect: Rect, col: u16, row: u16) -> Option<u64> {
    match tree {
        lazybox_core::TileTree::Leaf { terminal_id } => {
            rect_contains_point(rect, col, row).then_some(*terminal_id)
        }
        lazybox_core::TileTree::HSplit { left, right, ratio } => {
            let split_at = (rect.width as u32 * (*ratio).min(100) as u32 / 100) as u16;
            let left_w = split_at.min(rect.width.saturating_sub(1));
            let left_rect = Rect {
                width: left_w,
                ..rect
            };
            let right_rect = Rect {
                x: rect.x + left_w + 1,
                width: rect.width.saturating_sub(left_w + 1),
                ..rect
            };
            tile_at(left, left_rect, col, row).or_else(|| tile_at(right, right_rect, col, row))
        }
        lazybox_core::TileTree::VSplit { top, bottom, ratio } => {
            let split_at = (rect.height as u32 * (*ratio).min(100) as u32 / 100) as u16;
            let top_h = split_at.min(rect.height.saturating_sub(1));
            let top_rect = Rect {
                height: top_h,
                ..rect
            };
            let bottom_rect = Rect {
                y: rect.y + top_h + 1,
                height: rect.height.saturating_sub(top_h + 1),
                ..rect
            };
            tile_at(top, top_rect, col, row).or_else(|| tile_at(bottom, bottom_rect, col, row))
        }
    }
}

/// The rect `render_one_terminal` receives for the leaf carrying
/// `terminal_id`, walking the tree with the SAME split geometry as
/// [`tile_at`] and applying the same one-row focus rule
/// `render_tile_tree` carves off each leaf (`rect.height >= 2 &&
/// rect.width > 0`). Returns the leaf's post-rule body so a forwarded
/// mouse event's cell coordinates match the grid the renderer drew.
fn leaf_rect_of(tree: &lazybox_core::TileTree, rect: Rect, terminal_id: u64) -> Option<Rect> {
    match tree {
        lazybox_core::TileTree::Leaf { terminal_id: leaf } => (*leaf == terminal_id).then(|| {
            if rect.height >= 2 && rect.width > 0 {
                Rect {
                    y: rect.y + 1,
                    height: rect.height - 1,
                    ..rect
                }
            } else {
                rect
            }
        }),
        lazybox_core::TileTree::HSplit { left, right, ratio } => {
            let split_at = (rect.width as u32 * (*ratio).min(100) as u32 / 100) as u16;
            let left_w = split_at.min(rect.width.saturating_sub(1));
            let left_rect = Rect {
                width: left_w,
                ..rect
            };
            let right_rect = Rect {
                x: rect.x + left_w + 1,
                width: rect.width.saturating_sub(left_w + 1),
                ..rect
            };
            leaf_rect_of(left, left_rect, terminal_id)
                .or_else(|| leaf_rect_of(right, right_rect, terminal_id))
        }
        lazybox_core::TileTree::VSplit { top, bottom, ratio } => {
            let split_at = (rect.height as u32 * (*ratio).min(100) as u32 / 100) as u16;
            let top_h = split_at.min(rect.height.saturating_sub(1));
            let top_rect = Rect {
                height: top_h,
                ..rect
            };
            let bottom_rect = Rect {
                y: rect.y + top_h + 1,
                height: rect.height.saturating_sub(top_h + 1),
                ..rect
            };
            leaf_rect_of(top, top_rect, terminal_id)
                .or_else(|| leaf_rect_of(bottom, bottom_rect, terminal_id))
        }
    }
}

pub struct TerminalStack {
    id: PaneId,
    terminals: HashMap<TerminalId, TerminalSlot>,
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
    /// Click targets for the tab strip, populated each render. Each
    /// entry is `(tab_idx, (start_col, end_col_exclusive), row)`.
    /// `handle_tab_click(col, row)` scans this on mouse-down to map
    /// a click on the `claude` / `shell` label to a tab switch.
    /// Cleared at the start of every render so removed terminals
    /// don't leave stale hit targets.
    tab_strip_hits: Vec<(usize, std::ops::Range<u16>, u16)>,
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
}

/// Records that a terminal's process has exited. Agent terminals keep
/// their slot when this is set (frozen last screen + a restart banner)
/// instead of the whole pane vanishing on a crash (#356).
#[derive(Debug, Clone, Copy)]
struct TerminalExit {
    /// Exit code the daemon reported, or `None` when it couldn't — e.g.
    /// death by signal (the classic outcome when a Homebrew self-upgrade
    /// swaps the agent binary out mid-run, #355).
    code: Option<i32>,
}

/// How long an armed pending split waits for its shell's
/// `TerminalSpawned` before it stops claiming the next spawn. The
/// in-process round trip is sub-second; the window only has to beat
/// a daemon-side spawn that silently failed.
const PENDING_SPLIT_WINDOW: std::time::Duration = std::time::Duration::from_secs(10);

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
/// the host terminal's stdout, so the inner program's "copy this"
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
/// Best-effort: stdout write failures are ignored. Writing to the
/// host while ratatui is mid-frame is safe in practice — terminals
/// pop OSC out of the stream and don't paint it.
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
    if !ranges.is_empty() {
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        for range in ranges {
            let _ = out.write_all(&scan[range]);
        }
        let _ = out.flush();
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

struct TerminalSlot {
    session_key: SessionKey,
    kind: TerminalKind,
    last_seq: u64,
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
    /// Characters the user has typed since the last submit. Only
    /// tracked on Agent terminals — the pinned recap is meaningless
    /// for shells. Cleared when the user hits Enter, Ctrl-C, Ctrl-U,
    /// or Esc (the same keys that wipe the prompt buffer in Claude
    /// Code / a shell prompt).
    composing: String,
    /// Most recently submitted user message. Rendered as a one-line
    /// recap above the agent's terminal grid so it's obvious "what
    /// you just asked the model" even after pages of tool output
    /// scroll the prompt off-screen. `None` until the user has
    /// submitted at least one message in this terminal.
    last_user_message: Option<String>,
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
    /// Set when `pending_feed` overflowed [`PENDING_FEED_CAP`] and the
    /// oldest bytes were dropped. The deferred replay then resets the
    /// parser and re-feeds only the retained tail (resync semantics)
    /// instead of continuing the pre-hidden grid, which the dropped
    /// prefix would have desynced.
    pending_truncated: bool,
    /// Set once this (agent) terminal's process has exited on its own
    /// rather than by an explicit user close. The slot is retained so
    /// the frozen last screen stays visible and a restart banner is
    /// offered — a crashing agent (#356) must not take the workspace
    /// down with it. `None` for a live terminal.
    exited: Option<TerminalExit>,
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

    /// Commit the trimmed composing buffer as the latest user message
    /// and reset it for the next prompt. An all-whitespace buffer is
    /// ignored, so mashing Enter on an empty prompt (e.g. dismissing
    /// an agent approval) doesn't blank out the recap. Returns the
    /// committed message when one was recorded, so the caller can ship
    /// it to the daemon for persistence (`Command::RecordUserMessage`).
    fn commit_composing(&mut self) -> Option<String> {
        let trimmed = self.composing.trim();
        let committed = (!trimmed.is_empty()).then(|| trimmed.to_string());
        if let Some(msg) = &committed {
            self.last_user_message = Some(msg.clone());
        }
        self.composing.clear();
        committed
    }

    /// Replay any bytes buffered while this terminal was hidden into
    /// the VT parser, then clear the buffer. A no-op when nothing was
    /// buffered. If the buffer overflowed while hidden we reset the
    /// parser first and feed only the retained tail — feeding a
    /// truncated stream onto the stale pre-hidden grid would render
    /// garbage, so this mirrors the resync path's reset+refeed. On a
    /// reset failure the bytes are left in place to retry next frame
    /// rather than being dropped.
    fn flush_pending(&mut self) {
        if self.pending_feed.is_empty() {
            return;
        }
        if self.pending_truncated {
            if !self.vt.reset() {
                return;
            }
            self.pending_truncated = false;
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
    _not_send: std::marker::PhantomData<*mut ()>,
}

impl TerminalVt {
    fn new() -> Option<Box<Self>> {
        let terminal = vt::Terminal::new(vt::TerminalOptions {
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            max_scrollback: 10_000,
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
            _not_send: std::marker::PhantomData,
        }))
    }

    fn feed(&mut self, bytes: &[u8]) {
        self.terminal.vt_write(bytes);
    }

    /// THE one and only mutation of this terminal's viewport pin.
    /// Every scroll surface funnels here (`scroll_active`,
    /// `scroll_to_top`/`scroll_to_bottom`, the per-tile wheel path) and
    /// no other code in the crate calls `scroll_viewport` — a grep for
    /// it must return exactly this line. A request either moves the
    /// viewport or the returned [`ScrollOutcome`] explains why it could
    /// not (`NoScrollback` when `total <= len`, `alternate` when the
    /// inner program owns the buffer), so a scroll can never silently
    /// no-op. That single choke point is the #42/#371 encapsulation:
    /// there is one owner of scroll state and it cannot fail quietly.
    fn scroll(&mut self, request: ScrollRequest) -> ScrollOutcome {
        let alternate = matches!(
            self.terminal.active_screen().ok(),
            Some(vt::screen::Screen::Alternate)
        );
        match request {
            // A zero delta is a state query, not a move — fall through
            // to report the current scrollbar without touching the pin.
            ScrollRequest::By(0) => {}
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
        match self.terminal.scrollbar().ok() {
            Some(s) if s.total <= s.len => ScrollOutcome::NoScrollback { alternate },
            Some(s) => ScrollOutcome::Moved {
                offset: s.offset,
                total: s.total,
                len: s.len,
            },
            None => ScrollOutcome::NoTerminal,
        }
    }

    fn ensure_size(&mut self, cols: u16, rows: u16) {
        if cols == self.cols && rows == self.rows {
            return;
        }
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
            active_session: None,
            active_tab_idx: 0,
            collapsed: true,
            collapse_user_set: false,
            layout: lazybox_core::SessionLayout::default(),
            pending_split: None,
            synced_layout: None,
            pending_resizes: Vec::new(),
            tab_strip_hits: Vec::new(),
            last_focused: HashMap::new(),
            closing: HashSet::new(),
        }
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
    }

    pub fn layout(&self) -> &lazybox_core::SessionLayout {
        &self.layout
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
        self.auto_collapse_on_emptiness();
    }

    pub fn active_session(&self) -> Option<&SessionKey> {
        self.active_session.as_ref()
    }

    /// TerminalIds visible in the current session, in stable order:
    /// agents first (far left), then shells / log tails, ties broken
    /// by u64 id so tab positions are deterministic.
    pub fn visible_terminals(&self) -> Vec<TerminalId> {
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

    /// Switch the active tab to the given terminal (must belong to
    /// the active session, otherwise no-op). Used by the singleton
    /// toggle-or-focus path: the user pressed `c`, we already have
    /// a Claude in this session, just bring it forward.
    pub fn focus_terminal(&mut self, target: TerminalId) -> bool {
        let visible = self.visible_terminals();
        if let Some(idx) = visible.iter().position(|id| *id == target) {
            self.active_tab_idx = idx;
            // Expanding the section is part of "focus": collapsed
            // body would otherwise hide the tab the user just asked
            // for.
            self.set_collapsed(false);
            true
        } else {
            false
        }
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
    /// turn this on while running. The orchestrator's mouse handler
    /// uses this signal to choose between "scroll the scrollback"
    /// and "encode + send to PTY".
    pub fn focused_terminal_tracks_mouse(&self) -> bool {
        let Some(id) = self.focused_terminal_id() else {
            return false;
        };
        self.terminals
            .get(&id)
            .and_then(|s| s.vt.terminal.is_mouse_tracking().ok())
            .unwrap_or(false)
    }

    /// Decide how a mouse-wheel tick over the focused terminal should
    /// be handled — see [`WheelRoute`]. Resolved in a single
    /// focused-terminal lookup: the primary/alt-screen split and the
    /// mouse-tracking probe all read the same VT, so the whole routing
    /// decision is one place instead of the handler probing several
    /// booleans that could disagree if the focus moved between them.
    pub fn wheel_route(&self) -> WheelRoute {
        self.wheel_route_for(self.focused_terminal_id())
    }

    /// Route a wheel tick that landed at frame-space `(col, row)` in the
    /// terminal pane `rect` off the tile UNDER THE CURSOR (#362), so the
    /// routing decision, the scroll target, and any forwarded report all
    /// name the same terminal. Falls back to the focused terminal when
    /// the point is over pane chrome. In Tabs mode this resolves to the
    /// active terminal, leaving the single-pane case unchanged.
    pub fn wheel_route_at(&self, rect: Rect, col: u16, row: u16) -> WheelRoute {
        self.wheel_route_for(self.wheel_target(rect, col, row))
    }

    /// Shared routing decision for a specific terminal. `wheel_route`
    /// (focused) and `wheel_route_at` (tile under cursor) both delegate
    /// here so the primary/alt-screen split and the mouse-tracking probe
    /// are computed in exactly one place.
    fn wheel_route_for(&self, id: Option<TerminalId>) -> WheelRoute {
        let Some(id) = id else {
            return WheelRoute::LocalScrollback;
        };
        let Some(slot) = self.terminals.get(&id) else {
            return WheelRoute::LocalScrollback;
        };
        let t = &slot.vt.terminal;
        let on_alt_screen = t.mode(vt::terminal::Mode::ALT_SCREEN).unwrap_or(false)
            || t.mode(vt::terminal::Mode::ALT_SCREEN_SAVE).unwrap_or(false)
            || t.mode(vt::terminal::Mode::ALT_SCREEN_LEGACY)
                .unwrap_or(false);
        let tracks_mouse = t.is_mouse_tracking().unwrap_or(false);
        // Primary screen → the pane history is lazybox's, so scroll it
        // in-process, ALWAYS — even for a Claude Code that tracks the
        // mouse for clicks, and even before the client has accumulated
        // any scrollback (#321, #360). Gating this on
        // `total > len` (as an earlier fix did) made brand-new agents
        // silently forward the wheel to an app that ignores it: a
        // primary-screen agent owns no pager, so forwarding scrolls
        // nothing, while the local viewport starts moving the instant
        // real history exists. Any `total == len` frame is simply an
        // empty scrollback, not a reason to hand the wheel away.
        if !on_alt_screen {
            return WheelRoute::LocalScrollback;
        }
        // Alt-screen: the app owns the buffer. If it speaks mouse,
        // forward SGR; otherwise fall back to xterm alternateScroll.
        if tracks_mouse {
            WheelRoute::ForwardSgr
        } else {
            let app_cursor = t.mode(vt::terminal::Mode::DECCKM).unwrap_or(false);
            WheelRoute::AlternateScrollArrows { app_cursor }
        }
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

    /// Encode a mouse event for a SPECIFIC terminal (the tile under the
    /// cursor, resolved by [`Self::cell_at`]). Same contract as
    /// [`Self::encode_mouse_for_focused`] but addressed by id, so a wheel
    /// forwarded on a split targets the terminal the pointer is over
    /// rather than whichever holds focus.
    fn encode_mouse_for(
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

    /// Jump the focused terminal's viewport to the top of its
    /// scrollback. (Delta and Top/Bottom are equally reliable
    /// against libghostty-vt — `scroll_repro.rs` pins both; a
    /// scroll that looks like a no-op means there is no scrollback
    /// to move into, which `scroll_active` reports as
    /// `ScrollOutcome::NoScrollback`.) Returns the scrollbar state
    /// for the diagnostic notice.
    pub fn scroll_to_top(&mut self) -> Option<String> {
        self.scroll_focused(ScrollRequest::Top);
        self.scrollbar_summary()
    }

    pub fn scroll_to_bottom(&mut self) -> Option<String> {
        self.scroll_focused(ScrollRequest::Bottom);
        self.scrollbar_summary()
    }

    /// Route a scroll request to a specific terminal through the single
    /// owner (`TerminalVt::scroll`). The one internal chokepoint every
    /// public scroll entry point delegates to — keeping the "who owns
    /// scroll state" answer to exactly one place.
    fn scroll_terminal(&mut self, id: Option<TerminalId>, request: ScrollRequest) -> ScrollOutcome {
        let Some(id) = id else {
            return ScrollOutcome::NoTerminal;
        };
        match self.terminals.get_mut(&id) {
            Some(slot) => slot.vt.scroll(request),
            None => ScrollOutcome::NoTerminal,
        }
    }

    /// Scroll the focused terminal — the keyboard scrollback path
    /// (`Shift-PgUp/PgDn/Home/End`), which has no pointer to target a
    /// tile with, so it acts on wherever typing currently lands.
    fn scroll_focused(&mut self, request: ScrollRequest) -> ScrollOutcome {
        let id = self.focused_terminal_id();
        self.scroll_terminal(id, request)
    }

    /// The terminal whose tile contains frame-space `(col, row)` inside
    /// the terminal pane `rect`. Tabs mode → the active terminal (the
    /// whole body is one tile). Splits mode → the leaf under the point,
    /// resolved with the SAME geometry [`Self::render`] lays out with.
    /// `None` when the point falls outside every leaf (pane chrome).
    /// This is what lets the wheel target the tile under the cursor
    /// rather than the focused one (#362).
    pub fn terminal_at(&self, rect: Rect, col: u16, row: u16) -> Option<TerminalId> {
        let body = Self::pane_body_rect(rect);
        match &self.layout {
            lazybox_core::SessionLayout::Tabs { .. } => rect_contains_point(body, col, row)
                .then(|| self.active_terminal_id())
                .flatten(),
            lazybox_core::SessionLayout::Splits { tree, .. } => {
                tile_at(tree, body, col, row).map(TerminalId)
            }
        }
    }

    /// The `inner` body rect [`Self::render`] insets `area` to before
    /// laying out tiles: left border (+1 col), three rows of top chrome
    /// (tab strip + divider + blank), one held-back bottom margin.
    /// `terminal_at` walks the tile tree in this space, so it must stay
    /// in lockstep with the `inner` computation in `render`.
    fn pane_body_rect(rect: Rect) -> Rect {
        Rect {
            x: rect.x.saturating_add(1),
            y: rect.y.saturating_add(3),
            width: rect.width.saturating_sub(2),
            height: rect.height.saturating_sub(4),
        }
    }

    /// Scroll the tile under the cursor (#362). The wheel handler routes
    /// here so a scroll always moves the terminal the pointer is over,
    /// in both Tabs and Splits layouts — not whichever tile happens to
    /// hold keyboard focus. Falls back to the focused terminal when the
    /// point resolves to no live tile (pane chrome, or a tile whose
    /// terminal just exited) so a near-miss still scrolls something.
    pub fn scroll_at(
        &mut self,
        rect: Rect,
        col: u16,
        row: u16,
        request: ScrollRequest,
    ) -> ScrollOutcome {
        let id = self.wheel_target(rect, col, row);
        self.scroll_terminal(id, request)
    }

    /// The LIVE terminal a wheel at `(col, row)` in pane `rect` targets:
    /// the tile under the cursor, or the focused terminal when the point
    /// is over pane chrome / a divider, or when the resolved tile's
    /// terminal has gone (a tile tree that still names an exited runner).
    /// The single resolver `wheel_route_at` / `scroll_at` / `cell_at` all
    /// share, so route, scroll, and forward always name the same tile.
    fn wheel_target(&self, rect: Rect, col: u16, row: u16) -> Option<TerminalId> {
        self.terminal_at(rect, col, row)
            .filter(|id| self.terminals.contains_key(id))
            .or_else(|| self.focused_terminal_id())
    }

    /// The rect [`Self::render_one_terminal`] draws terminal `id` into:
    /// the pane body in Tabs mode, or the leaf's body (past its one-row
    /// focus rule) in Splits. `None` when `id` isn't currently laid out.
    /// Mirrors `render` / `render_tile_tree` so cell translation for a
    /// forwarded event lands on the same grid the renderer drew.
    fn leaf_render_rect(&self, pane_rect: Rect, id: TerminalId) -> Option<Rect> {
        let body = Self::pane_body_rect(pane_rect);
        match &self.layout {
            lazybox_core::SessionLayout::Tabs { .. } => {
                (self.active_terminal_id() == Some(id)).then_some(body)
            }
            lazybox_core::SessionLayout::Splits { tree, .. } => leaf_rect_of(tree, body, id.0),
        }
    }

    /// Resolve a wheel/forward point to `(target terminal, cell col, cell
    /// row)` within that terminal's grid, tile-aware for both layouts, or
    /// `None` when the point is over chrome (tab strip, borders, the
    /// scrollbar gutter, or the focus rule). Undoes exactly the insets
    /// [`Self::render_one_terminal`] applies to the terminal's render
    /// rect (recap rows off the top, the gutter column off the right).
    fn cell_at(&self, rect: Rect, col: u16, row: u16) -> Option<(TerminalId, u32, u32)> {
        let id = self.wheel_target(rect, col, row)?;
        let render_rect = self.leaf_render_rect(rect, id)?;
        let slot = self.terminals.get(&id)?;
        let recap = Self::recap_rows(slot, render_rect.height);
        let grid_x = render_rect.x;
        let grid_y = render_rect.y.saturating_add(recap);
        // Rightmost column is the scrollbar gutter; the bottom is bounded
        // by the render rect itself (the pane already held back a margin).
        let grid_right = render_rect
            .x
            .saturating_add(render_rect.width)
            .saturating_sub(1);
        let grid_bottom = render_rect.y.saturating_add(render_rect.height);
        if col < grid_x || row < grid_y || col >= grid_right || row >= grid_bottom {
            return None;
        }
        Some((id, u32::from(col - grid_x), u32::from(row - grid_y)))
    }

    /// Encode a mouse event for the tile UNDER THE CURSOR (#362), with
    /// cell coordinates translated into that tile's grid. Returns the
    /// bytes to `Write` plus the target terminal id, or `None` when the
    /// point is over chrome or the target isn't tracking the mouse. The
    /// wheel handler's SGR-forward branch routes here.
    pub fn encode_mouse_at(
        &mut self,
        rect: Rect,
        col: u16,
        row: u16,
        action: vt::mouse::Action,
        button: Option<vt::mouse::Button>,
    ) -> Option<(TerminalId, Vec<u8>)> {
        let (id, cell_col, cell_row) = self.cell_at(rect, col, row)?;
        self.encode_mouse_for(id, action, button, cell_col, cell_row)
    }

    /// The tile under the cursor for the alternate-scroll arrow-key
    /// fallback — only when the point sits on a real grid cell (not
    /// chrome), so a wheel over the tab strip never synthesizes arrows.
    pub fn wheel_arrow_target(&self, rect: Rect, col: u16, row: u16) -> Option<TerminalId> {
        self.cell_at(rect, col, row).map(|(id, _, _)| id)
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

    /// Scroll the focused terminal by `delta` rows. Returns a
    /// `ScrollOutcome` describing what actually happened so the
    /// caller can surface a clear notice — lazybox's scroll bug
    /// turned out to be "total == len, no scrollback to scroll
    /// into" silently looking identical to "delta is broken."
    /// Read the text content of the focused terminal's grid between
    /// two cell coordinates expressed in absolute viewport (frame)
    /// space. `rect` is the terminal pane's rect — used to translate
    /// the absolute `(col, row)` pair into cell offsets within the
    /// grid. Returns plain text joined with newlines between rows.
    /// Empty when nothing's focused or the range is degenerate.
    ///
    /// **Flowing-text selection** (mailer / browser style):
    /// - Same row: copy cells `[sx, ex]` on that row.
    /// - Multi-row: first row goes from `sx` to end-of-row; full
    ///   middle rows are copied whole; last row goes from start
    ///   to `ex`.
    ///
    /// This matches what users expect from "drag from word X on
    /// line 2 to word Y on line 5" — they get EVERYTHING in
    /// between, not just the rectangular cells `[sx..ex] × [sy..ey]`
    /// which is what the previous version produced.
    pub fn extract_text(
        &mut self,
        rect: tuirealm::ratatui::layout::Rect,
        start: (u16, u16),
        end: (u16, u16),
    ) -> String {
        let Some(id) = self.focused_terminal_id() else {
            return String::new();
        };
        let Some(slot) = self.terminals.get_mut(&id) else {
            return String::new();
        };
        // The grid must reflect every byte received, not just those that
        // arrived while on screen — copy can fire on a terminal that
        // gained focus but hasn't re-rendered yet. A no-op when nothing
        // was buffered.
        slot.flush_pending();
        // Translate from screen-absolute crossterm coords to the
        // terminal's CONTENT-area coords. The render path puts the
        // terminal grid at `inner = Rect { x: rect.x + 1, y: rect.y
        // + 3 }` (border on the left, tab strip + divider on top —
        // see `TerminalStack::render`), then `render_one_terminal`
        // carves the recap rows off the top of that body. Selection
        // coords came from crossterm in screen-absolute space, so we
        // undo every offset the renderer applied — skipping the recap
        // rows is what keeps an agent terminal's copy aligned with the
        // highlight instead of pulling the row below it.
        let inner_x = rect.x.saturating_add(1);
        // Use the SAME body height the renderer feeds `recap_rows` — the
        // pane minus 3 top-chrome rows AND the 1 held-back bottom margin
        // (`render()` insets to `area.height - 4` before calling
        // `render_one_terminal`). Using `- 3` here diverged from the
        // renderer at exactly height 6, where `recap_rows` flips.
        let body_height = rect.height.saturating_sub(4);
        let recap = Self::recap_rows(slot, body_height);
        let inner_y = rect.y.saturating_add(3).saturating_add(recap);
        // Normalize: anchor (anchor_x, anchor_y) is the row-then-
        // column "earlier" endpoint of the selection — i.e. the
        // smaller (y, x) pair. The other endpoint is the focus.
        // This is the *row-major* normalization the flowing-text
        // model needs (sort by y first, then x), distinct from the
        // axis-independent normalization the rectangle model used.
        let (anchor_x, anchor_y, focus_x, focus_y) = if (start.1, start.0) <= (end.1, end.0) {
            (start.0, start.1, end.0, end.1)
        } else {
            (end.0, end.1, start.0, start.1)
        };
        let anchor_col = anchor_x.saturating_sub(inner_x);
        let focus_col = focus_x.saturating_sub(inner_x);
        let row_start = anchor_y.saturating_sub(inner_y);
        let row_end = focus_y.saturating_sub(inner_y);
        let single_row = row_start == row_end;
        let Ok(snapshot) = slot.vt.render_state.update(&slot.vt.terminal) else {
            return String::new();
        };
        let Ok(mut row_iter) = slot.vt.row_iter.update(&snapshot) else {
            return String::new();
        };
        let mut out = String::new();
        let mut y: u16 = 0;
        while let Some(row) = row_iter.next() {
            if y > row_end {
                break;
            }
            if y >= row_start {
                // Decide which column range applies to THIS row.
                // Single-row selection → strict [anchor_col, focus_col].
                // Multi-row selection:
                //   - first row → [anchor_col, ∞)
                //   - middle row → [0, ∞)
                //   - last row → [0, focus_col]
                let (col_start, col_end): (u16, Option<u16>) = if single_row {
                    let (a, b) = if anchor_col <= focus_col {
                        (anchor_col, focus_col)
                    } else {
                        (focus_col, anchor_col)
                    };
                    (a, Some(b))
                } else if y == row_start {
                    (anchor_col, None)
                } else if y == row_end {
                    (0, Some(focus_col))
                } else {
                    (0, None)
                };
                let mut line = String::new();
                if let Ok(mut cell_iter) = slot.vt.cell_iter.update(row) {
                    let mut x: u16 = 0;
                    while let Some(cell) = cell_iter.next() {
                        if let Some(end_col) = col_end
                            && x > end_col
                        {
                            break;
                        }
                        if x >= col_start {
                            // The blank spacer cell of a wide glyph carries
                            // no graphemes; emitting it as a space turns
                            // "日本語" into "日 本 語". Skip it. `SpacerTail`
                            // follows a wide glyph; `SpacerHead` pads the
                            // end of a soft-wrapped row where a wide glyph
                            // couldn't fit — both are non-text.
                            if matches!(
                                cell.wide(),
                                Ok(vt::screen::CellWide::SpacerTail
                                    | vt::screen::CellWide::SpacerHead)
                            ) {
                                x += 1;
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
                        x += 1;
                    }
                }
                // Trim trailing spaces so the copy doesn't include
                // the row's blank tail — terminals pad rows with
                // spaces but the user expects "just the text."
                let line = line.trim_end().to_string();
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&line);
            }
            y += 1;
        }
        out
    }

    /// If the cell at frame-space `(col, row)` lies inside a URL,
    /// file path, or `#N` / `owner/repo#N` issue reference on its
    /// row, return the matching [`ClickTarget`]. Otherwise `None`.
    /// Drives right-click-to-open: the click coordinates arrive in
    /// the same frame-space the renderer used, so we translate the
    /// same way `extract_text` does (skip the pane border + tab strip
    /// + any recap rows via `recap_rows`). Single-row only — wrapped
    /// tokens aren't detected (per the issue: "stay simple, terminal
    /// URLs/paths are virtually always on one row").
    pub fn target_at(
        &mut self,
        rect: tuirealm::ratatui::layout::Rect,
        col: u16,
        row: u16,
    ) -> Option<ClickTarget> {
        let id = self.focused_terminal_id()?;
        let slot = self.terminals.get_mut(&id)?;
        let inner_x = rect.x.saturating_add(1);
        // Use the SAME body height the renderer feeds `recap_rows` — the
        // pane minus 3 top-chrome rows AND the 1 held-back bottom margin
        // (`render()` insets to `area.height - 4` before calling
        // `render_one_terminal`). Using `- 3` here diverged from the
        // renderer at exactly height 6, where `recap_rows` flips.
        let body_height = rect.height.saturating_sub(4);
        let recap = Self::recap_rows(slot, body_height);
        let inner_y = rect.y.saturating_add(3).saturating_add(recap);
        if col < inner_x || row < inner_y {
            return None;
        }
        let cell_col = col - inner_x;
        let target_row = row - inner_y;
        let hyperlink = hyperlink_uri_at(&slot.vt.terminal, cell_col, target_row);
        let snapshot = slot.vt.render_state.update(&slot.vt.terminal).ok()?;
        let mut row_iter = slot.vt.row_iter.update(&snapshot).ok()?;
        // Walk graphemes — wide-glyph cells contribute one grapheme
        // on cell N and an empty cell N+1, so we record the byte
        // offset at the START of each cell's contribution into
        // `row_text`. `cell_byte_starts[cell_col]` then maps the
        // clicked cell back to a byte position in the row's text.
        let mut y: u16 = 0;
        let mut row_text = String::new();
        let mut cell_byte_starts: Vec<usize> = Vec::new();
        let mut found = false;
        while let Some(row) = row_iter.next() {
            if y == target_row {
                if let Ok(mut cell_iter) = slot.vt.cell_iter.update(row) {
                    while let Some(cell) = cell_iter.next() {
                        // A blank spacer cell of a wide glyph emits no
                        // text, but it still occupies a screen column, so
                        // a click on it must resolve into the glyph. Map
                        // its byte offset back to the wide base (the last
                        // recorded start) and append nothing — otherwise
                        // the column→byte map drifts past wide text and
                        // right-click resolves the wrong token. Covers both
                        // the post-glyph `SpacerTail` and the soft-wrap
                        // `SpacerHead`.
                        if matches!(
                            cell.wide(),
                            Ok(vt::screen::CellWide::SpacerTail | vt::screen::CellWide::SpacerHead)
                        ) {
                            cell_byte_starts.push(cell_byte_starts.last().copied().unwrap_or(0));
                            continue;
                        }
                        cell_byte_starts.push(row_text.len());
                        let graphemes = cell.graphemes().unwrap_or_default();
                        if graphemes.is_empty() {
                            row_text.push(' ');
                        } else {
                            for g in graphemes {
                                row_text.push(g);
                            }
                        }
                    }
                }
                found = true;
                break;
            }
            y += 1;
        }
        if !found {
            return None;
        }
        let byte_pos = *cell_byte_starts.get(cell_col as usize)?;
        detect_target(&row_text, byte_pos, hyperlink.as_deref())
    }

    /// Translate a screen `(col, row)` into 0-based grid-cell
    /// coordinates inside the focused terminal's body, undoing the same
    /// inset the renderer applies: the left border (`+1` col), the
    /// three rows of top chrome (tab strip + divider + blank), and any
    /// recap rows. This is the coordinate space `encode_mouse_for_focused`
    /// expects. Returns `None` when the point falls in the border / tab
    /// strip / recap (left of or above the grid) so callers forwarding a
    /// click or wheel to a mouse-tracking inner program never feed it a
    /// cell the renderer never drew there. Mirrors `target_at` /
    /// `extract_text`, which previously were the ONLY paths that undid
    /// this offset — the forward path used the raw pane origin and so
    /// landed every event 1 column right and 3+ rows high.
    pub fn screen_to_cell(
        &self,
        rect: tuirealm::ratatui::layout::Rect,
        col: u16,
        row: u16,
    ) -> Option<(u32, u32)> {
        let id = self.focused_terminal_id()?;
        let slot = self.terminals.get(&id)?;
        let inner_x = rect.x.saturating_add(1);
        // Use the SAME body height the renderer feeds `recap_rows` — the
        // pane minus 3 top-chrome rows AND the 1 held-back bottom margin
        // (`render()` insets to `area.height - 4` before calling
        // `render_one_terminal`). Using `- 3` here diverged from the
        // renderer at exactly height 6, where `recap_rows` flips.
        let body_height = rect.height.saturating_sub(4);
        let recap = Self::recap_rows(slot, body_height);
        let inner_y = rect.y.saturating_add(3).saturating_add(recap);
        // Reject points OUTSIDE the inner body — the left/right border
        // columns and the bottom row are never grid cells, so a click
        // there must not be forwarded to the inner program as a bogus
        // near-edge cell. (Lower bounds guard the border/chrome above and
        // left; these guard the border below and right.)
        if col < inner_x
            || row < inner_y
            || col >= rect.x.saturating_add(rect.width).saturating_sub(1)
            || row >= rect.y.saturating_add(rect.height).saturating_sub(1)
        {
            return None;
        }
        Some(((col - inner_x) as u32, (row - inner_y) as u32))
    }

    /// Scroll the focused terminal by `delta` rows through the single
    /// scroll owner. The keyboard scrollback path uses this; the
    /// mouse-wheel path uses [`Self::scroll_at`] so it can target the
    /// tile under the cursor (#362). Both funnel into
    /// `TerminalVt::scroll`, so neither can silently no-op.
    pub fn scroll_active(&mut self, delta: isize) -> ScrollOutcome {
        self.scroll_focused(ScrollRequest::By(delta))
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

    fn append_output(&mut self, id: TerminalId, bytes: &[u8], seq: u64) {
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
            slot.pending_feed.extend_from_slice(bytes);
            if slot.pending_feed.len() > PENDING_FEED_CAP {
                let excess = slot.pending_feed.len() - PENDING_FEED_CAP;
                slot.pending_feed.drain(..excess);
                slot.pending_truncated = true;
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
        if !slot.vt.reset() {
            return;
        }
        slot.vt.feed(replay);
        // Drop any half-buffered clipboard sequence — the stream is being
        // rebuilt from the ring and we don't re-forward OSC 52 here.
        slot.osc52_carry.clear();
        // The ring replay is authoritative; any bytes buffered while
        // hidden are now stale and already covered by it.
        slot.pending_feed.clear();
        slot.pending_truncated = false;
        slot.recent.clear();
        let tail_start = replay.len().saturating_sub(RECENT_OUTPUT_CAP);
        slot.recent.extend_from_slice(&replay[tail_start..]);
        slot.last_seq = seq;
    }

    fn make_slot(
        session_key: SessionKey,
        kind: TerminalKind,
        last_seq: u64,
        no_permission: bool,
        on_main: bool,
        model_label: Option<String>,
        last_user_message: Option<String>,
    ) -> TerminalSlot {
        let vt = TerminalVt::new().expect("libghostty-vt init");
        TerminalSlot {
            session_key,
            kind,
            last_seq,
            vt,
            recent: Vec::new(),
            osc52_carry: Vec::new(),
            agent_state: lazybox_ipc::AgentState::Idle,
            last_rendered_size: None,
            composing: String::new(),
            last_user_message,
            no_permission,
            on_main,
            model_label,
            displayed: false,
            pending_feed: Vec::new(),
            pending_truncated: false,
            exited: None,
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
            .and_then(|s| s.last_user_message.as_deref())
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
    /// recap. No-op for non-Agent terminals.
    pub fn record_paste(&mut self, text: &str) {
        let Some(id) = self.focused_terminal_id() else {
            return;
        };
        if let Some(slot) = self.terminals.get_mut(&id)
            && matches!(slot.kind, TerminalKind::Agent(_))
        {
            slot.append_paste(text);
        }
    }

    /// Mirror bytes written straight to a terminal's PTY — bypassing
    /// the per-keystroke `handle_key` path — into the recap state.
    /// Used by callers that synthesise a full command and submit it in
    /// one shot (snippet expansion writes the body + a trailing `\r`),
    /// which would otherwise leave the "you ▸ …" recap showing the
    /// previous message. No-op for non-Agent terminals. Returns the
    /// committed message (if this write ended in a submit) so the
    /// caller can persist it via `Command::RecordUserMessage`.
    pub fn record_pty_write(&mut self, id: TerminalId, bytes: &[u8]) -> Option<String> {
        let slot = self.terminals.get_mut(&id)?;
        if !matches!(slot.kind, TerminalKind::Agent(_)) {
            return None;
        }
        slot.record_pty_bytes(bytes)
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

    /// Bindings shown in the hint bar. Drops the legacy
    /// `all keys → PTY` entry — that describes an implementation mode
    /// rather than an actionable shortcut, so it was noise in the
    /// footer. The user always knows their typing reaches the inner
    /// program; what they need surfaced is *escape hatches*: leave the
    /// pane (the only way back to the sidebar once the PTY owns the
    /// keyboard), toggle focus mode, split panes, send SIGINT. The
    /// `Shift-PgUp/PgDn` scroll hint is deliberately omitted —
    /// scrolling is intuitive (the mouse wheel works too) and it
    /// crowded out more useful hints (#202).
    ///
    /// `escape_char` is the configured `terminal.escape_char` (the
    /// `]` in the default `]]` leader). The leader-based hints render
    /// it doubled rather than a hardcoded `]]` so a user who remapped
    /// the escape char sees the chord they actually type (#170).
    ///
    /// Associated function (no `&self`) because the bindings don't
    /// depend on terminal-stack state — they're the same whether the
    /// pane has zero terminals or twenty. The pane wrapper still
    /// takes `&self` for symmetry with the other panes (Sidebar /
    /// Right both inspect state to decide what to surface), but
    /// reaches through to this stateless implementation.
    pub fn contextual_bindings(escape_char: char) -> Vec<crate::Binding> {
        use crate::Binding;
        use lazybox_tui_core::action::{ActionDef, ActionKind};
        use std::borrow::Cow;
        let leave = ActionDef::for_kind(ActionKind::LeaveTerminal);
        let leader = format!("{escape_char}{escape_char}");
        // The leave chord is owned by `terminal.escape_char`, NOT the
        // `leave_terminal` action_keys slot. Terminal-pane dispatch
        // (`model::keys`) matches only the configured escape char and
        // never the catalog chord, so honoring a `leave_terminal`
        // override here would advertise a key the dispatcher ignores —
        // the footer would say "Esc exit to sidebar" while Esc does
        // nothing (#188). Render the escape char doubled + `q` (`]]q`):
        // the `]]` leader is non-timed now (#252) and `q` is its exit
        // command, replacing the old idle-timeout leave.
        let leave_keys: Cow<'static, str> = Cow::Owned(format!("{leader}q"));
        let focus = ActionDef::for_kind(ActionKind::ToggleFocusMode);
        vec![
            // `]]q` (the escape char doubled, then `q`) — the way back to
            // the sidebar once the PTY owns the keyboard. The issue
            // (#170) was that this had no footer hint, so the route back
            // to focus was invisible from inside Claude Code.
            Binding {
                keys: leave_keys,
                label: Cow::Borrowed(leave.label),
            },
            // `]]f` toggles focus mode (near-fullscreen agent terminal).
            // Like the leave chord it rides the `]]` leader rather than
            // the catalog default key (`.`, which the PTY would eat), so
            // the keys are hand-built from the escape char while the
            // label tracks the catalog so a rename flows through (#202).
            Binding {
                keys: Cow::Owned(format!("{leader}f")),
                label: Cow::Borrowed(focus.label),
            },
            // `Ctrl-c` is forwarded straight to the PTY rather than
            // being a catalog action — but it's actionable knowledge
            // for the user (escape a hung process), so it stays in
            // the hint bar as a hand-curated entry.
            Binding {
                keys: Cow::Borrowed("Ctrl-c"),
                label: Cow::Borrowed("interrupt"),
            },
            // Tile management rides the `]]` leader like every other
            // lazybox chord in terminal mode (#286): `]]|` / `]]-`
            // split, `]]<arrow>` moves tile focus, `]]x` closes the
            // focused terminal. Surface the split entry point; the
            // leader popup lists the rest. Labeled "split panes"
            // rather than "tiles" — the latter meant nothing to a
            // user who'd never used tmux-style panes (#202).
            Binding {
                keys: Cow::Owned(format!("{leader}|")),
                label: Cow::Borrowed("split panes"),
            },
            // Snippet picker entry point (issues #40, #205, #252). `]]s`
            // opens the picker; typing a full key there auto-submits its
            // body to the agent (the `]]srev` fast path). Routing the
            // picker under the leader frees a lone `]` to reach the agent
            // verbatim.
            Binding {
                keys: Cow::Owned(format!("{leader}s")),
                label: Cow::Borrowed("snippets"),
            },
        ]
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
                    self.scroll_active(-STEP);
                    return PaneOutcome::Consumed;
                }
                KeyCode::PageDown => {
                    self.scroll_active(STEP);
                    return PaneOutcome::Consumed;
                }
                KeyCode::Home => {
                    self.scroll_to_top();
                    return PaneOutcome::Consumed;
                }
                KeyCode::End => {
                    self.scroll_to_bottom();
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
        let committed = if let Some(slot) = self.terminals.get_mut(&id)
            && matches!(slot.kind, TerminalKind::Agent(_))
        {
            slot.record_pty_bytes(&bytes)
        } else {
            None
        };
        cmds.push(Command::Write {
            terminal_id: id,
            bytes,
        });
        // Persist the submitted prompt daemon-side so the recap survives
        // a restart — the replay ring only carries PTY output, not the
        // input we composed here.
        if let Some(message) = committed {
            cmds.push(Command::RecordUserMessage {
                terminal_id: id,
                message,
            });
        }
        PaneOutcome::Consumed
    }

    pub fn on_event(&mut self, event: &Event) {
        match event {
            Event::Snapshot { terminals, .. } => {
                self.terminals.clear();
                for snap in terminals {
                    let mut slot = Self::make_slot(
                        snap.session_key.clone(),
                        snap.kind.clone(),
                        snap.last_seq,
                        snap.no_permission,
                        snap.on_main,
                        snap.model_label.clone(),
                        snap.last_user_message.clone(),
                    );
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
                    slot.pending_feed = snap.replay.clone();
                    self.terminals.insert(snap.terminal_id, slot);
                }
                self.clamp_active_tab();
                self.auto_collapse_on_emptiness();
                // Eagerly parse only the terminal actually in the
                // foreground: the `&self` readers (mouse-tracking
                // probe, alt-screen check) consult its live parser
                // state between now and the next render, and it's what
                // the user is looking at. Everything else parses
                // lazily on first display.
                if let Some(id) = self.focused_terminal_id()
                    && let Some(slot) = self.terminals.get_mut(&id)
                {
                    slot.flush_pending();
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
                // A restart (#356) or a fresh `w x` after a crash lands
                // here: supersede any exited pane for the same session +
                // agent so the new terminal replaces the frozen one
                // instead of leaving a dead tab beside it (and so the
                // split/tab auto-layout below doesn't count the corpse).
                if let TerminalKind::Agent(new_id) = kind {
                    let superseded: Vec<TerminalId> = self
                        .terminals
                        .iter()
                        .filter(|(_, s)| {
                            s.exited.is_some()
                                && &s.session_key == session_key
                                && matches!(&s.kind, TerminalKind::Agent(a) if a == new_id)
                        })
                        .map(|(id, _)| *id)
                        .collect();
                    for old in superseded {
                        self.drop_slot(old);
                    }
                }
                let slot = Self::make_slot(
                    session_key.clone(),
                    kind.clone(),
                    0,
                    *no_permission,
                    *on_main,
                    model_label.clone(),
                    None,
                );
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
                    if session_count >= 2 {
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
                        // First terminal in this session — stay in Tabs
                        // (cheaper render, no wasted dividers) but make the
                        // fresh spawn the active tab so the user lands on it.
                        self.active_tab_idx = idx;
                    }
                }
            }
            Event::TerminalOutput {
                terminal_id,
                bytes,
                seq,
            } => {
                self.append_output(*terminal_id, bytes, *seq);
            }
            Event::TerminalResync {
                terminal_id,
                replay,
                seq,
            } => {
                self.resync_terminal(*terminal_id, replay, *seq);
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
                }
            }
            Event::TerminalExited {
                terminal_id,
                exit_code,
            } => {
                let user_closed = self.closing.remove(terminal_id);
                let is_agent = self
                    .terminals
                    .get(terminal_id)
                    .is_some_and(|s| matches!(s.kind, TerminalKind::Agent(_)));
                // A shell going away — or any terminal the user closed
                // with `]]x` — takes its pane with it, like every other
                // terminal emulator. But an AGENT that exited on its own
                // (crash, ^D, or its binary swapped out mid-run by a
                // Homebrew self-upgrade, #355) must NOT silently vanish:
                // keep the slot frozen on its last screen so the
                // workspace survives and a restart is offered (#356).
                if is_agent && !user_closed {
                    if let Some(slot) = self.terminals.get_mut(terminal_id) {
                        slot.exited = Some(TerminalExit { code: *exit_code });
                    }
                } else {
                    self.drop_slot(*terminal_id);
                }
            }
            Event::TerminalsRebadged { from, to } => {
                // The daemon moved every terminal keyed to `from` onto
                // `to` (issue→PR collapse or manual adopt). Re-point our
                // slots so they follow — and crucially so the
                // `WorkspaceRemoved(from)` that trails a collapse no
                // longer matches them and drops the live session.
                for slot in self.terminals.values_mut() {
                    if &slot.session_key == from {
                        slot.session_key = to.clone();
                    }
                }
                if let Some(id) = self.last_focused.remove(from) {
                    self.last_focused.insert(to.clone(), id);
                }
                if self.active_session.as_ref() == Some(from) {
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
        // Title row: "Terminals" plus an icon+label per active terminal
        // (e.g. `Terminals    claude   _ shell`). Active is bold-accent;
        // inactive is dim grey. Two-tab common case looks like a tab
        // strip; single-terminal shows just one entry.
        let title_prefix = "Terminals  ";
        let mut title_spans: Vec<Span<'static>> = vec![
            Span::styled("Terminals", theme.title(focused)),
            Span::raw("  "),
        ];
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
            let exited = self.terminals.get(id).and_then(|s| s.exited);
            let (hint, hint_style) = if exited.is_some() {
                (
                    " ✗ exited",
                    Style::default()
                        .fg(theme.error)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                match agent_state {
                    Some(lazybox_ipc::AgentState::InputNeeded) => (
                        " ! needs input",
                        Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
                    ),
                    Some(lazybox_ipc::AgentState::Working) => {
                        (" · working", Style::default().fg(theme.accent))
                    }
                    Some(lazybox_ipc::AgentState::Done) => (
                        " ✓ done",
                        Style::default()
                            .fg(theme.success)
                            .add_modifier(Modifier::BOLD),
                    ),
                    _ => ("", Style::default()),
                }
            };
            if !hint.is_empty() {
                title_spans.push(Span::styled(hint, hint_style));
                cursor = cursor.saturating_add(hint.chars().count() as u16);
            }
            // No-permission / bypass mode: this session auto-accepts
            // tool-use prompts and runs unattended. Flag it so the user
            // can tell at a glance which tabs aren't gated by approvals.
            if no_permission {
                let noperm_text = " ⚠ no-perms";
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
                    self.render_one_terminal(id, body, frame, focused);
                }
            }
            lazybox_core::SessionLayout::Splits {
                tree,
                focused: focus_path,
            } => {
                // Recursive tile renderer. Dividers are drawn on the
                // boundary between adjacent leaves; the focused leaf
                // gets a brighter border so the user can tell where
                // typing lands.
                let theme_chrome = theme.chrome;
                let theme_accent = theme.accent;
                self.render_tile_tree(
                    &tree,
                    body,
                    frame,
                    focused,
                    &focus_path,
                    &[],
                    theme_chrome,
                    theme_accent,
                );
            }
        }
    }
}

impl TerminalStack {
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
        self.pending_split = Some((direction, std::time::Instant::now()));
        cmds.push(Command::Spawn {
            model_alias: None,
            session_key,
            session_id: None,
            kind: TerminalKind::Shell,
            cwd: None,
            initial_prompt: None,
            // A tile-split shell lands in the workspace's default
            // (isolated) worktree, not the shared main checkout.
            on_main: false,
        });
    }

    /// Move focus across the tile tree (`]]<arrow>`), or cycle through
    /// tabs in Tabs mode. Persists the new layout via `SetSessionLayout`.
    pub fn move_tile_focus(&mut self, dir: lazybox_core::TileDirection, cmds: &mut Vec<Command>) {
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

    /// Remove a terminal slot from the map and the tile tree,
    /// collapsing splits and re-clamping the tab strip. Shared by the
    /// exit teardown, the restart path (#356), and the
    /// spawn-supersedes-crashed-pane path.
    fn drop_slot(&mut self, terminal_id: TerminalId) {
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

    /// Re-spawn the agent behind an exited pane (#356). The daemon
    /// already swept the dead terminal's state on exit, so its
    /// singleton guard won't block this — it lands as a fresh
    /// `TerminalSpawned`, and the arriving spawn supersedes this
    /// exited slot. Leaving the frozen pane in place until then means a
    /// spawn that fails daemon-side keeps the restart banner rather than
    /// dropping to nothing.
    fn restart_exited(&mut self, terminal_id: TerminalId, cmds: &mut Vec<Command>) {
        let Some(slot) = self.terminals.get(&terminal_id) else {
            return;
        };
        if slot.exited.is_none() {
            return;
        }
        cmds.push(Command::Spawn {
            model_alias: None,
            session_key: slot.session_key.clone(),
            session_id: None,
            kind: slot.kind.clone(),
            cwd: None,
            initial_prompt: None,
            on_main: slot.on_main,
        });
    }

    /// Close the focused terminal (`]]x`). In Splits, collapses the
    /// focused leaf's parent split into the surviving sibling; in Tabs,
    /// closes the active tab's terminal (the event flow prunes the slot
    /// and re-clamps the strip). Either way the terminal's PTY is
    /// killed daemon-side via `Command::Close`.
    pub fn close_focused_tile(&mut self, cmds: &mut Vec<Command>) {
        let lazybox_core::SessionLayout::Splits { tree, focused } = &mut self.layout else {
            if let Some(id) = self.active_terminal_id() {
                if self.terminals.get(&id).is_some_and(|s| s.exited.is_some()) {
                    // Already dead server-side (#356) — no
                    // `TerminalExited` will echo to prune it, so drop the
                    // frozen pane locally.
                    self.drop_slot(id);
                } else {
                    // Tag as a user close so the returning
                    // `TerminalExited` tears the pane down instead of
                    // keeping it as an exited agent pane (#356).
                    self.closing.insert(id);
                    cmds.push(Command::Close { terminal_id: id });
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
                    // Exited pane (#356): the tree collapse above already
                    // dropped its tile; remove the map entry too since no
                    // `TerminalExited` will echo to do it.
                    self.terminals.remove(&tid);
                } else {
                    self.closing.insert(tid);
                    cmds.push(Command::Close { terminal_id: tid });
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
    fn render_user_message_recap(frame: &mut Frame, area: Rect, msg: &str) {
        let theme = crate::theme::current();
        let summary = summarize_message(msg);
        let line = ratatui::text::Line::from(vec![
            Span::styled(
                RECAP_PREFIX,
                Style::default()
                    .fg(theme.text_dim)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(summary, Style::default().fg(theme.text_dim)),
        ]);
        let line = crate::components::table::truncate_line(line, area.width as usize);
        frame.render_widget(Paragraph::new(line), area);
    }

    /// Rows carved off the top of a terminal's body for the pinned
    /// "you ▸ <recap>" line plus a blank spacer below it: 2 for an
    /// agent terminal with a remembered last user message, 0 for
    /// everything else. `body_height` is the height of the grid area
    /// (the rect handed to [`render_one_terminal`], already inside the
    /// tab strip + divider) — the recap is refused below 3 rows so a
    /// tiny split keeps every cell for the agent grid. This is the one
    /// source of truth for the offset: the render path and the
    /// selection/click coordinate mappers all read it so they map the
    /// same rows.
    fn recap_rows(slot: &TerminalSlot, body_height: u16) -> u16 {
        let show_recap = matches!(slot.kind, TerminalKind::Agent(_))
            && slot.last_user_message.is_some()
            && body_height >= 3;
        if show_recap { 2 } else { 0 }
    }

    /// Render a single terminal slot full-rect. Used by both the
    /// tabs path and the splits path's leaf case.
    fn render_one_terminal(
        &mut self,
        id: TerminalId,
        rect: Rect,
        frame: &mut Frame,
        focused: bool,
    ) {
        let _ = focused; // ghostty-vt doesn't render focus chrome itself
        if let Some(slot) = self.terminals.get_mut(&id) {
            // Coming on screen: drain whatever arrived while hidden into
            // the parser, then mark displayed so subsequent chunks feed
            // eagerly (this slot stays current as long as it's visible).
            slot.flush_pending();
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
                && let Some(msg) = slot.last_user_message.as_deref()
            {
                let header_rect = Rect {
                    x: rect.x,
                    y: rect.y,
                    width: rect.width,
                    height: 1,
                };
                Self::render_user_message_recap(frame, header_rect, msg);
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
            slot.vt.ensure_size(grid.width, grid.height);
            // Backend PTY also needs to know the new size — otherwise
            // the shell process keeps writing at its spawn dimensions
            // and the bottom rows go blank as soon as the user scrolls
            // past them. Queue a resize for the App to ship.
            let new_size = (grid.width, grid.height);
            if grid.width > 0 && grid.height > 0 && slot.last_rendered_size != Some(new_size) {
                slot.last_rendered_size = Some(new_size);
                self.pending_resizes.push((id, grid.width, grid.height));
            }
            if let Ok(snapshot) = slot.vt.render_state.update(&slot.vt.terminal) {
                let widget = GhosttyTerminal::new(
                    &snapshot,
                    &mut slot.vt.row_iter,
                    &mut slot.vt.cell_iter,
                    &mut slot.vt.shadow,
                );
                frame.render_widget(widget, grid);
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
            if let Some(exit) = slot.exited {
                Self::render_exit_banner(frame, grid, exit);
            }
        }
    }

    /// Paint the "agent exited — restart?" banner across the bottom row
    /// of an exited pane's grid (#356). The frozen last screen stays
    /// visible above it; this row is a filled bar so it reads as an
    /// alert over whatever output the crash left behind.
    fn render_exit_banner(frame: &mut Frame, grid: Rect, exit: TerminalExit) {
        if grid.width == 0 || grid.height == 0 {
            return;
        }
        let theme = crate::theme::current();
        let status = match exit.code {
            Some(code) => format!("code {code}"),
            None => "killed".to_string(),
        };
        let text = format!("⚠ agent exited ({status}) — r restart · ]]x close");
        let width = grid.width as usize;
        // Pad (or truncate) to the full row so the fill spans it.
        let display: String = if text.chars().count() > width {
            text.chars().take(width).collect()
        } else {
            let pad = width - text.chars().count();
            format!("{text}{}", " ".repeat(pad))
        };
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
        accent: Color,
    ) {
        match node {
            lazybox_core::TileTree::Leaf { terminal_id } => {
                let is_focused_leaf = pane_focused && current_path == focus_path;
                // Every leaf gets a one-cell top rule: accent on the
                // focused tile, chrome on the rest (#286). The contrast
                // between the two is what makes "where does my typing
                // land" legible at a glance — an accent bar with nothing
                // to compare against read as decoration, not focus. The
                // rule row is CARVED off the tile's rect (the PTY is
                // sized to the remainder), never painted over content —
                // overdrawing hid the tile's top grid row and the agent
                // recap. A one-row tile keeps its content instead.
                let body = if rect.height >= 2 && rect.width > 0 {
                    let bar = Rect {
                        x: rect.x,
                        y: rect.y,
                        width: rect.width,
                        height: 1,
                    };
                    let color = if is_focused_leaf { accent } else { chrome };
                    frame.render_widget(
                        Paragraph::new(Line::from(Span::styled(
                            "─".repeat(bar.width as usize),
                            Style::default().fg(color),
                        ))),
                        bar,
                    );
                    Rect {
                        x: rect.x,
                        y: rect.y + 1,
                        width: rect.width,
                        height: rect.height - 1,
                    }
                } else {
                    rect
                };
                self.render_one_terminal(TerminalId(*terminal_id), body, frame, is_focused_leaf);
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
                    accent,
                );
                self.render_tile_tree(
                    right,
                    right_rect,
                    frame,
                    pane_focused,
                    focus_path,
                    &p_right,
                    chrome,
                    accent,
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
                    accent,
                );
                self.render_tile_tree(
                    bottom,
                    bottom_rect,
                    frame,
                    pane_focused,
                    focus_path,
                    &p_bot,
                    chrome,
                    accent,
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

/// Scan `row_text` for an `http(s)://…` token whose byte range
/// contains `byte_pos`. Returns the URL as a borrowed slice when
/// found. URL terminates at the first whitespace; trailing
/// punctuation that's almost never part of the URL (`.,;:!?` plus
/// the closing brackets and quotes) is trimmed so a sentence like
/// `see https://example.com.` opens `https://example.com`.
pub(crate) fn find_url_at_byte(row_text: &str, byte_pos: usize) -> Option<&str> {
    let mut search_start = 0;
    while search_start < row_text.len() {
        let rest = &row_text[search_start..];
        let http = rest.find("http://");
        let https = rest.find("https://");
        let off = match (http, https) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }?;
        let url_start = search_start + off;
        let after_scheme = &row_text[url_start..];
        let raw_end_off = after_scheme
            .char_indices()
            .find(|(_, c)| c.is_whitespace())
            .map(|(i, _)| i)
            .unwrap_or(after_scheme.len());
        let mut url_end = url_start + raw_end_off;
        // Trim trailing punctuation that's almost never part of a
        // URL. Stop once we hit something URL-valid.
        loop {
            let slice = &row_text[url_start..url_end];
            let Some(last) = slice.chars().next_back() else {
                break;
            };
            if matches!(
                last,
                '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '\'' | '"'
            ) {
                url_end -= last.len_utf8();
            } else {
                break;
            }
        }
        if byte_pos >= url_start && byte_pos < url_end {
            return Some(&row_text[url_start..url_end]);
        }
        // Advance past this URL match (or the leading whitespace if
        // url_end == url_start after trimming) to look for the next.
        search_start = url_end.max(url_start + 1);
    }
    None
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
        let slot =
            TerminalStack::make_slot(sk.clone(), TerminalKind::Shell, 0, false, false, None, None);
        stack.terminals.insert(TerminalId(1), slot);
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
mod extract_text_offset_tests {
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
            last_user_message.map(str::to_string),
        );
        let mut payload = String::new();
        for line in lines {
            payload.push_str(line);
            payload.push_str("\r\n");
        }
        slot.vt.feed(payload.as_bytes());
        stack.terminals.insert(TerminalId(1), slot);
        stack.set_active_session(Some(sk));
        stack
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
        let text = stack.extract_text(Rect::new(0, 0, 80, 30), (1, 5), (10, 5));
        assert_eq!(text, "line0");
    }

    #[test]
    fn shell_maps_selection_to_highlighted_row() {
        let mut stack = stack_with(TerminalKind::Shell, None, &["line0", "line1", "line2"]);
        // No recap: grid top stays at screen row 3.
        let text = stack.extract_text(Rect::new(0, 0, 80, 30), (1, 3), (10, 3));
        assert_eq!(text, "line0");
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
        let text = stack.extract_text(Rect::new(0, 0, 80, 30), (1, 3), (40, 3));
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

    #[test]
    fn agent_without_remembered_message_has_no_recap_offset() {
        let mut stack = stack_with(
            TerminalKind::Agent("claude".into()),
            None,
            &["line0", "line1"],
        );
        let text = stack.extract_text(Rect::new(0, 0, 80, 30), (1, 3), (10, 3));
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
            None,
        );
        slot.last_user_message = Some("hi".into());
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
        stack.extract_text(Rect::new(0, 0, 80, 30), at, (20, at.1))
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
            seq,
        });
    }

    fn row0(stack: &mut TerminalStack) -> String {
        stack.extract_text(Rect::new(0, 0, W, H), ROW0, (20, ROW0.1))
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

    /// A buffer that overflows `PENDING_FEED_CAP` while hidden keeps only
    /// the most-recent tail and resets+refeeds the parser on display, so
    /// the visible grid still reflects the latest output.
    #[test]
    fn overflowing_hidden_buffer_replays_tail() {
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
        assert!(
            slot.pending_truncated,
            "overflow must set the truncation flag"
        );
        assert!(slot.pending_feed.len() <= PENDING_FEED_CAP);

        stack.set_active_session(Some(sk_b));
        let rows = screen_rows(&mut stack);
        let slot = &stack.terminals[&TerminalId(2)];
        assert!(slot.pending_feed.is_empty());
        assert!(!slot.pending_truncated);
        // The tail survived the truncation and rendered (reset+refeed).
        assert!(
            rows.iter().any(|r| r.contains("last line")),
            "expected the post-truncation tail on screen, got {rows:?}"
        );
    }

    /// A reconnect `Snapshot` must not pay the VT parse for every
    /// terminal synchronously on the UI thread: only the foreground
    /// terminal is fed eagerly; hidden terminals stash their replay in
    /// `pending_feed` and reconstruct the exact grid on first display.
    #[test]
    fn snapshot_defers_hidden_terminal_replays() {
        let sk_a = SessionKey::new("a");
        let sk_b = SessionKey::new("b");
        let mut stack = TerminalStack::new(PaneId::new(0));
        stack.set_active_session(Some(sk_a.clone()));

        let snap = |id: u64, sk: &SessionKey, replay: &[u8]| lazybox_ipc::TerminalSnapshot {
            terminal_id: TerminalId(id),
            session_key: sk.clone(),
            kind: TerminalKind::Shell,
            replay: replay.to_vec(),
            last_seq: 1,
            no_permission: false,
            on_main: false,
            model_label: None,
            last_user_message: None,
        };
        stack.on_event(&Event::Snapshot {
            workspaces: vec![],
            terminals: vec![
                snap(1, &sk_a, b"visible\r\n"),
                snap(2, &sk_b, b"hidden\r\n"),
            ],
            projects: vec![],
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
            crate::realm::components::footer::render(f, footer, &binds, &[], None, None);
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
            None,
        );
        slot.vt.ensure_size(W - 3, H - 4);
        let mut payload = String::new();
        for i in 0..40 {
            payload.push_str(&format!("output line {i}\r\n"));
        }
        // Mirror Claude Code's persistent bottom chrome.
        payload.push_str("? for shortcuts");
        slot.vt.feed(payload.as_bytes());
        stack.terminals.insert(TerminalId(1), slot);
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
        stack.scroll_active(-12);
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
        assert!(cmds.is_empty(), "keyboard scroll is a pure in-process move");
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
        assert!(cmds.is_empty(), "keyboard scroll is a pure in-process move");
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
            None,
        );
        stack.terminals.insert(TerminalId(1), slot);
        stack.set_active_session(Some(sk));

        let rows = render_rows(&mut stack);
        assert!(
            rows.iter().any(|r| r.contains("◆ Opus")),
            "tab strip should show the tier badge; got {rows:?}",
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
mod terminal_availability_tests {
    //! Issue #114: the splash / tour / footer advertise a set of
    //! "always available" globals (`?`, `q q`, `Shift-T`, …). In a
    //! focused terminal the PTY eats every key, so those globals don't
    //! fire — the advertised set must match what `TerminalStack`
    //! actually dispatches. These tests pin that contract: the catalog
    //! flag `available_in_terminal` is the single source of truth, and
    //! the pane's real behavior agrees with it.
    use super::*;
    use lazybox_tui_core::action::{self, ActionDef, ActionKind};

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
    fn advertised_terminal_bindings_are_what_the_catalog_allows() {
        // The hint bar must only surface keys the pane will actually
        // dispatch in terminal focus. The single catalog-backed binding
        // is the `]]` leave chord — the gateway back to the globals —
        // and none of the universal shortcuts may be advertised here.
        let bindings = TerminalStack::contextual_bindings(']');

        let leave = ActionDef::for_kind(ActionKind::LeaveTerminal);
        assert!(
            bindings.iter().any(|b| b.keys == leave.default_keys),
            "the `]]` leave chord must be advertised as the way out",
        );
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
    //! A spawned agent that exits on its own (crash, killed binary —
    //! #356) must NOT take its workspace down with it: the pane stays,
    //! frozen on its last screen, and offers a restart. Only a shell
    //! exit or an explicit user close (`]]x`) tears the pane down.
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
        });

        let slot = stack
            .terminals
            .get(&TerminalId(1))
            .expect("crashed agent pane must survive");
        assert!(
            matches!(slot.exited, Some(TerminalExit { code: Some(1) })),
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
        });

        assert!(matches!(
            stack.terminals.get(&TerminalId(1)).map(|s| s.exited),
            Some(Some(TerminalExit { code: None })),
        ));
    }

    #[test]
    fn shell_exit_removes_pane() {
        let sk = SessionKey::new("github:o/r#1");
        let mut stack = active_stack(1, &sk, TerminalKind::Shell);

        stack.on_event(&Event::TerminalExited {
            terminal_id: TerminalId(1),
            exit_code: Some(0),
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
                    terminal_id: TerminalId(1)
                }]
            ),
            "close pushes a daemon-side kill",
        );
        stack.on_event(&Event::TerminalExited {
            terminal_id: TerminalId(1),
            exit_code: Some(0),
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
        });

        let mut cmds = Vec::new();
        let outcome = stack.handle_key(
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
            &mut cmds,
        );

        assert!(matches!(outcome, PaneOutcome::Consumed));
        match cmds.as_slice() {
            [
                Command::Spawn {
                    session_key, kind, ..
                },
            ] => {
                assert_eq!(session_key, &sk);
                assert!(matches!(kind, TerminalKind::Agent(a) if a == "codex"));
            }
            other => panic!("restart must spawn the same agent, got {other:?}"),
        }
    }

    #[test]
    fn keys_do_not_reach_a_dead_pty() {
        let sk = SessionKey::new("github:o/r#1");
        let mut stack = active_stack(1, &sk, TerminalKind::Agent("codex".into()));
        stack.on_event(&Event::TerminalExited {
            terminal_id: TerminalId(1),
            exit_code: Some(1),
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
    fn restart_spawn_supersedes_exited_pane() {
        let sk = SessionKey::new("github:o/r#1");
        let mut stack = active_stack(1, &sk, TerminalKind::Agent("codex".into()));
        stack.on_event(&Event::TerminalExited {
            terminal_id: TerminalId(1),
            exit_code: Some(1),
        });

        // The restart's fresh terminal lands as a new id — the exited
        // corpse for the same session+agent is dropped, not left as a
        // dead tab beside the live one.
        stack.on_event(&Event::TerminalSpawned {
            model_label: None,
            terminal_id: TerminalId(2),
            session_key: sk.clone(),
            kind: TerminalKind::Agent("codex".into()),
            no_permission: false,
            on_main: false,
        });

        assert!(stack.terminals.get(&TerminalId(1)).is_none());
        assert!(stack.terminals.get(&TerminalId(2)).is_some());
        assert_eq!(stack.visible_terminals(), vec![TerminalId(2)]);
    }

    #[test]
    fn exited_pane_renders_a_restart_banner() {
        let sk = SessionKey::new("github:o/r#1");
        let mut stack = active_stack(1, &sk, TerminalKind::Agent("codex".into()));
        stack.on_event(&Event::TerminalExited {
            terminal_id: TerminalId(1),
            exit_code: Some(137),
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
    fn a_different_agent_spawn_leaves_the_exited_pane() {
        let sk = SessionKey::new("github:o/r#1");
        let mut stack = active_stack(1, &sk, TerminalKind::Agent("codex".into()));
        stack.on_event(&Event::TerminalExited {
            terminal_id: TerminalId(1),
            exit_code: Some(1),
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
}
