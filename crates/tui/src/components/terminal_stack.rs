//! TerminalStack — multi-terminal right-pane surface per session.
//!
//! Each session can have several terminals open simultaneously: the
//! agent (Claude / Codex / Cursor), a shell, a log tail. This
//! component owns the per-terminal libghostty-vt parser state, feeds
//! it the bytes the daemon streams, and renders the resulting cell
//! grid via `pilot_tui_term::GhosttyTerminal`.
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
use libghostty_vt as vt;
use pilot_core::SessionKey;
use pilot_ipc::{Command, Event, TerminalId, TerminalKind};
use pilot_tui_term::GhosttyTerminal;
use ratatui::Frame;
use ratatui::prelude::*;
use ratatui::widgets::*;
use std::collections::HashMap;

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
/// gap. Single-pass; no intermediate Vec.
fn summarize_message(msg: &str) -> String {
    let mut out = String::with_capacity(msg.len());
    for word in msg.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

/// Outcome of a scroll attempt on the focused terminal. Used by the
/// orchestrator's mouse-wheel handler to surface why a scroll might
/// have looked like nothing happened — without this, "no scrollback
/// content yet" was indistinguishable from a broken Delta path.
#[derive(Debug, Clone, Copy)]
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
    layout: pilot_core::SessionLayout,
    /// `Ctrl-w` tile-management prefix latch (tmux-style). When
    /// armed, the next keystroke is interpreted as a tile action
    /// (split, focus move, close); otherwise keys forward to the
    /// active PTY. Generic `PrefixLatch` shared with future two-key
    /// chord features — see `crate::confirm_latch::PrefixLatch`.
    ctrl_w_latch: crate::confirm_latch::PrefixLatch,
    /// Pending split operation: when the user hits `Ctrl-w |` we
    /// emit `Command::Spawn` for a new shell, then once the
    /// `TerminalSpawned` event arrives we wrap the focused leaf in a
    /// fresh split with the new terminal. `Some(direction)` means
    /// "the next spawn becomes the new sibling on this axis".
    pending_split: Option<PendingSplit>,
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
}

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
/// Returned ranges are non-overlapping and in input order. An
/// unterminated OSC 52 (sequence starts but no terminator before
/// end-of-bytes) is dropped — the inner program is expected to
/// terminate within a single write. Same chunk; we don't span.
pub(crate) fn osc52_ranges(bytes: &[u8]) -> Vec<std::ops::Range<usize>> {
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
                    // Unterminated — drop this match, stop scanning.
                    return out;
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
    out
}

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
/// Best-effort: stdout write failures are ignored. Writing to the
/// host while ratatui is mid-frame is safe in practice — terminals
/// pop OSC out of the stream and don't paint it.
fn forward_osc52(bytes: &[u8]) {
    let ranges = osc52_ranges(bytes);
    if ranges.is_empty() {
        return;
    }
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    for range in ranges {
        let _ = out.write_all(&bytes[range]);
    }
    let _ = out.flush();
}

/// Read-only walk of a `TileTree` along a path. Returns None if the
/// path tries to descend through a leaf.
fn subtree_at_path<'a>(
    root: &'a pilot_core::TileTree,
    path: &[u8],
) -> Option<&'a pilot_core::TileTree> {
    let mut node = root;
    for &step in path {
        node = match node {
            pilot_core::TileTree::HSplit { left, right, .. }
            | pilot_core::TileTree::VSplit {
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
            pilot_core::TileTree::Leaf { .. } => return None,
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
    /// Agent state cached from the daemon's `Event::AgentState`
    /// broadcasts. Drives the "needs input" badge in the tab strip.
    /// Default Active so non-agent terminals (shells) carry a
    /// neutral state.
    agent_state: pilot_ipc::AgentState,
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
}

impl TerminalSlot {
    /// Apply a user keystroke to the composing buffer + last-message
    /// state. Mirrors how the agent's own prompt-line reads keys:
    ///   - printable Char → append
    ///   - Backspace → pop
    ///   - Enter → commit (Shift-Enter inserts a newline instead,
    ///     matching Claude Code's "newline-without-submit" binding)
    ///   - Ctrl-C / Ctrl-U / Esc → clear the line
    ///
    /// Other keys (arrows, function keys, Tab) leave both buffers
    /// untouched — they don't change the literal text the user is
    /// composing. Per-char appends respect [`COMPOSING_CAP`] so a
    /// rogue auto-typer can't grow the buffer unbounded.
    fn apply_user_key(&mut self, key: &KeyEvent) {
        use KeyCode::*;
        let mods = key.modifiers;
        match key.code {
            Char(c) => {
                if mods.contains(KeyModifiers::CONTROL) {
                    if c == 'c' || c == 'u' {
                        self.composing.clear();
                    }
                } else if self.composing.len() + c.len_utf8() <= COMPOSING_CAP {
                    self.composing.push(c);
                }
            }
            Enter => {
                if mods.contains(KeyModifiers::SHIFT) {
                    if self.composing.len() < COMPOSING_CAP {
                        self.composing.push('\n');
                    }
                } else {
                    let trimmed = self.composing.trim();
                    if !trimmed.is_empty() {
                        self.last_user_message = Some(trimmed.to_string());
                    }
                    self.composing.clear();
                }
            }
            Backspace => {
                self.composing.pop();
            }
            Esc => {
                self.composing.clear();
            }
            _ => {}
        }
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
    /// Per-terminal render cache. `GhosttyTerminal::render` writes
    /// every dirty cell to BOTH `buf` and this shadow, then on the
    /// next frame copies clean rows straight from here — skipping
    /// the per-cell FFI walk for unchanged rows.
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

    fn ensure_size(&mut self, cols: u16, rows: u16) {
        if cols == self.cols && rows == self.rows {
            return;
        }
        let _ = self.terminal.resize(cols, rows, 0, 0);
        self.cols = cols;
        self.rows = rows;
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
            layout: pilot_core::SessionLayout::default(),
            ctrl_w_latch: crate::confirm_latch::PrefixLatch::new(),
            pending_split: None,
            pending_resizes: Vec::new(),
            tab_strip_hits: Vec::new(),
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
    pub fn set_layout(&mut self, layout: pilot_core::SessionLayout) {
        self.layout = layout;
        self.ctrl_w_latch.disarm();
        self.pending_split = None;
    }

    pub fn layout(&self) -> &pilot_core::SessionLayout {
        &self.layout
    }

    /// Terminal id at the focused leaf (Splits mode), or the active
    /// tab's terminal id (Tabs mode). Returns None when nothing is
    /// renderable.
    pub fn focused_terminal_id(&self) -> Option<TerminalId> {
        match &self.layout {
            pilot_core::SessionLayout::Tabs { .. } => self.active_terminal_id(),
            pilot_core::SessionLayout::Splits { tree, focused } => {
                let leaves = tree.leaves();
                let path = focused.as_slice();
                let id = subtree_at_path(tree, path).and_then(|n| match n {
                    pilot_core::TileTree::Leaf { terminal_id } => Some(*terminal_id),
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
    /// Also resets the active tab to 0 so switching sessions doesn't
    /// dump the user on a tab index that happens to still be valid
    /// but represents a totally different terminal.
    pub fn set_active_session(&mut self, session: Option<SessionKey>) {
        let changed = self.active_session != session;
        if changed {
            self.active_tab_idx = 0;
            // Drop the user's explicit collapse override on session
            // change — each session gets its own auto-default.
            self.collapse_user_set = false;
        }
        self.active_session = session;
        if changed {
            self.auto_collapse_on_emptiness();
        }
    }

    pub fn active_session(&self) -> Option<&SessionKey> {
        self.active_session.as_ref()
    }

    /// TerminalIds visible in the current session, in stable order
    /// (by u64 id so tab positions are deterministic).
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
        ids.sort_by_key(|id| id.0);
        ids
    }

    pub fn active_terminal_id(&self) -> Option<TerminalId> {
        self.visible_terminals().get(self.active_tab_idx).copied()
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

    /// True if the focused terminal's inner process is on the
    /// alternate screen (tmux, vim, less, claude code, etc. — i.e.
    /// any full-screen TUI). Used by the wheel handler to decide
    /// between SGR mouse encoding (slow: forces a full inner-app
    /// re-render per tick) and the xterm "alternateScroll" pattern
    /// (fast: synthesize arrow-key presses, which alt-screen TUIs
    /// scroll cheaply).
    pub fn focused_terminal_in_alt_screen(&self) -> bool {
        let Some(id) = self.focused_terminal_id() else {
            return false;
        };
        let Some(slot) = self.terminals.get(&id) else {
            return false;
        };
        slot.vt
            .terminal
            .mode(vt::terminal::Mode::ALT_SCREEN)
            .unwrap_or(false)
            || slot
                .vt
                .terminal
                .mode(vt::terminal::Mode::ALT_SCREEN_SAVE)
                .unwrap_or(false)
            || slot
                .vt
                .terminal
                .mode(vt::terminal::Mode::ALT_SCREEN_LEGACY)
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

    /// Last-resort fallback for scrollback navigation: drive
    /// the viewport directly via Top/Bottom anchors rather than
    /// Delta. The Delta path appears to no-op against libghostty-vt
    /// (offset doesn't change even though `scroll_viewport(Delta)`
    /// is called). Top/Bottom is a known-good API that lets us at
    /// least verify the terminal HAS scrollback content to look at.
    /// Returns the scrollbar state for the diagnostic notice.
    pub fn scroll_to_top(&mut self) -> Option<String> {
        let id = self.focused_terminal_id()?;
        let slot = self.terminals.get_mut(&id)?;
        slot.vt
            .terminal
            .scroll_viewport(vt::terminal::ScrollViewport::Top);
        self.scrollbar_summary()
    }

    pub fn scroll_to_bottom(&mut self) -> Option<String> {
        let id = self.focused_terminal_id()?;
        let slot = self.terminals.get_mut(&id)?;
        slot.vt
            .terminal
            .scroll_viewport(vt::terminal::ScrollViewport::Bottom);
        self.scrollbar_summary()
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
    /// caller can surface a clear notice — pilot's scroll bug
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
        // Translate from screen-absolute crossterm coords to the
        // terminal's CONTENT-area coords. The render path puts the
        // terminal grid at `inner = Rect { x: rect.x + 1, y: rect.y
        // + 3 }` (border on the left, tab strip + divider on top —
        // see `TerminalStack::render`). Selection coords came from
        // crossterm in screen-absolute space; subtracting only
        // `rect.x/y` left them 1 column too far right and 3 rows
        // too high, so every copied line was actually the row 3
        // ABOVE what the user highlighted. Bug user reported as
        // "doesn't copy what I selected."
        let inner_x = rect.x.saturating_add(1);
        let inner_y = rect.y.saturating_add(3);
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
    /// via `inner_x = rect.x + 1`, `inner_y = rect.y + 3`). Single-row
    /// only — wrapped tokens aren't detected (per the issue: "stay
    /// simple, terminal URLs/paths are virtually always on one row").
    pub fn target_at(
        &mut self,
        rect: tuirealm::ratatui::layout::Rect,
        col: u16,
        row: u16,
    ) -> Option<ClickTarget> {
        let id = self.focused_terminal_id()?;
        let slot = self.terminals.get_mut(&id)?;
        let inner_x = rect.x.saturating_add(1);
        let inner_y = rect.y.saturating_add(3);
        if col < inner_x || row < inner_y {
            return None;
        }
        let cell_col = col - inner_x;
        let target_row = row - inner_y;
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
        detect_target(&row_text, byte_pos)
    }

    pub fn scroll_active(&mut self, delta: isize) -> ScrollOutcome {
        if delta == 0 {
            return ScrollOutcome::NoTerminal;
        }
        let Some(id) = self.focused_terminal_id() else {
            return ScrollOutcome::NoTerminal;
        };
        let Some(slot) = self.terminals.get_mut(&id) else {
            return ScrollOutcome::NoTerminal;
        };
        let screen = slot.vt.terminal.active_screen().ok();
        let before = slot.vt.terminal.scrollbar().ok();
        slot.vt
            .terminal
            .scroll_viewport(vt::terminal::ScrollViewport::Delta(delta));
        let after = slot.vt.terminal.scrollbar().ok();
        tracing::info!(
            terminal_id = ?id,
            delta = delta,
            screen = ?screen,
            before_total = before.as_ref().map(|s| s.total),
            before_offset = before.as_ref().map(|s| s.offset),
            after_total = after.as_ref().map(|s| s.total),
            after_offset = after.as_ref().map(|s| s.offset),
            "scroll_active: viewport state",
        );
        let alternate = matches!(screen, Some(vt::screen::Screen::Alternate));
        match after {
            Some(s) if s.total <= s.len => ScrollOutcome::NoScrollback { alternate },
            Some(s) => ScrollOutcome::Moved {
                offset: s.offset,
                total: s.total,
                len: s.len,
            },
            None => ScrollOutcome::NoTerminal,
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

    fn append_output(&mut self, id: TerminalId, bytes: &[u8], seq: u64) {
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
        // own clipboard (which pilot doesn't surface).
        forward_osc52(bytes);
        slot.vt.feed(bytes);
        slot.recent.extend_from_slice(bytes);
        if slot.recent.len() > RECENT_OUTPUT_CAP {
            let excess = slot.recent.len() - RECENT_OUTPUT_CAP;
            slot.recent.drain(..excess);
        }
        slot.last_seq = seq;
    }

    fn make_slot(session_key: SessionKey, kind: TerminalKind, last_seq: u64) -> TerminalSlot {
        let vt = TerminalVt::new().expect("libghostty-vt init");
        TerminalSlot {
            session_key,
            kind,
            last_seq,
            vt,
            recent: Vec::new(),
            agent_state: pilot_ipc::AgentState::Active,
            last_rendered_size: None,
            composing: String::new(),
            last_user_message: None,
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

    /// Whether this pane can pop into a detached window. Terminals
    /// don't (yet); the legacy trait default returned `None` and we
    /// preserve that here.
    pub fn detachable(&self) -> Option<crate::DetachSpec> {
        None
    }

    /// Bindings shown in the hint bar. Drops the legacy
    /// `all keys → PTY` entry — that describes an implementation mode
    /// rather than an actionable shortcut, so it was noise in the
    /// footer. The user always knows their typing reaches the inner
    /// program; what they need surfaced is *escape hatches*: scroll
    /// the scrollback, leave the pane, send SIGINT. Keys are sourced
    /// from the catalog where possible so a rebind / rename in
    /// `ActionDef` flows through automatically.
    ///
    /// Associated function (no `&self`) because the bindings don't
    /// depend on terminal-stack state — they're the same whether the
    /// pane has zero terminals or twenty. The pane wrapper still
    /// takes `&self` for symmetry with the other panes (Sidebar /
    /// Right both inspect state to decide what to surface), but
    /// reaches through to this stateless implementation.
    pub fn contextual_bindings(
        overrides: &std::collections::BTreeMap<String, String>,
    ) -> Vec<crate::Binding> {
        use crate::Binding;
        use pilot_tui_core::action::{ActionDef, ActionKind};
        // `Shift-PgUp/Dn scroll` removed in #11 — the mouse wheel
        // is the primary scroll path and the keyboard fallback
        // wasn't worth its slot in the hint bar. Leave + interrupt
        // are the only escape hatches that need surfacing here.
        let leave = ActionDef::for_kind(ActionKind::LeaveTerminal);
        vec![
            Binding {
                keys: leave.effective_keys_display(overrides),
                label: std::borrow::Cow::Borrowed(leave.label),
            },
            // `Ctrl-c` is forwarded straight to the PTY rather than
            // being a catalog action — but it's actionable knowledge
            // for the user (escape a hung process), so it stays in
            // the hint bar as a hand-curated entry.
            Binding {
                keys: std::borrow::Cow::Borrowed("Ctrl-c"),
                label: std::borrow::Cow::Borrowed("interrupt"),
            },
        ]
    }

    pub fn handle_key(&mut self, key: KeyEvent, cmds: &mut Vec<Command>) -> PaneOutcome {
        // Tile-management prefix. Once `Ctrl-w` arms the latch, the
        // next key is a tile action (split, focus move, close);
        // anything unrecognised disarms cleanly. Same vocabulary as
        // tmux/vim windows so existing muscle memory transfers.
        if self.ctrl_w_latch.take() {
            return self.handle_tile_action(key, cmds);
        }
        if key.code == KeyCode::Char('w') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.ctrl_w_latch.arm();
            return PaneOutcome::Consumed;
        }

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
                    if let Some(id) = self.focused_terminal_id()
                        && let Some(slot) = self.terminals.get_mut(&id)
                    {
                        slot.vt
                            .terminal
                            .scroll_viewport(vt::terminal::ScrollViewport::Top);
                    }
                    return PaneOutcome::Consumed;
                }
                KeyCode::End => {
                    if let Some(id) = self.focused_terminal_id()
                        && let Some(slot) = self.terminals.get_mut(&id)
                    {
                        slot.vt
                            .terminal
                            .scroll_viewport(vt::terminal::ScrollViewport::Bottom);
                    }
                    return PaneOutcome::Consumed;
                }
                _ => {}
            }
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
        // Mirror the keystroke into our own composing buffer so the
        // pinned "you ▸ …" recap reflects the latest submitted
        // message. Scoped to Agent terminals — shells don't have a
        // single semantic "user prompt", so the recap would be noisy
        // (every cd, every grep) and surprising.
        if let Some(slot) = self.terminals.get_mut(&id)
            && matches!(slot.kind, TerminalKind::Agent(_))
        {
            slot.apply_user_key(&key);
        }
        cmds.push(Command::Write {
            terminal_id: id,
            bytes,
        });
        PaneOutcome::Consumed
    }

    pub fn on_event(&mut self, event: &Event) {
        match event {
            Event::Snapshot { terminals, .. } => {
                self.terminals.clear();
                for snap in terminals {
                    let mut slot =
                        Self::make_slot(snap.session_key.clone(), snap.kind.clone(), snap.last_seq);
                    // Replay the daemon-side ring through the VT so
                    // the cell grid reflects what was on screen
                    // before this client connected.
                    slot.vt.feed(&snap.replay);
                    self.terminals.insert(snap.terminal_id, slot);
                }
                self.clamp_active_tab();
                self.auto_collapse_on_emptiness();
            }
            Event::TerminalSpawned {
                terminal_id,
                session_key,
                kind,
            } => {
                let slot = Self::make_slot(session_key.clone(), kind.clone(), 0);
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

                // Stage 2 of a Ctrl-w split: wrap the focused leaf
                // in a fresh split with this new terminal as the
                // sibling. Without this, the new shell shows up as a
                // tab but never enters the tile tree.
                if let Some(direction) = self.pending_split.take()
                    && Some(session_key) == self.active_session.as_ref()
                {
                    self.commit_pending_split(*terminal_id, direction);
                } else if Some(session_key) == self.active_session.as_ref()
                    && matches!(self.layout, pilot_core::SessionLayout::Tabs { .. })
                    && self
                        .terminals
                        .iter()
                        .filter(|(_, slot)| Some(&slot.session_key) == self.active_session.as_ref())
                        .count()
                        >= 2
                {
                    // Two-or-more terminals on the same session: the
                    // user wants to see both. The Tabs default hides
                    // everything but the active tab; auto-promote to
                    // a vertical split so the new arrival lands beside
                    // the previous one. Single-terminal sessions stay
                    // in Tabs (cheaper render, no wasted dividers).
                    self.commit_pending_split(*terminal_id, PendingSplit::Vertical);
                }
            }
            Event::TerminalOutput {
                terminal_id,
                bytes,
                seq,
            } => {
                self.append_output(*terminal_id, bytes, *seq);
            }
            Event::TerminalFocusRequested { terminal_id } => {
                // Daemon-driven focus from the singleton guard.
                // Make the matching tab active + bring the pane up.
                self.focus_terminal(*terminal_id);
            }
            Event::AgentState {
                session_key, state, ..
            } => {
                // Update every agent slot in this session — the
                // daemon broadcasts one event per terminal, but the
                // sidebar's needs-input indicator is session-keyed so
                // we apply by session_key. The wire `terminal_id` is
                // for per-terminal consumers (chat dispatcher); it's
                // intentionally unused here.
                for slot in self.terminals.values_mut() {
                    if &slot.session_key == session_key
                        && matches!(slot.kind, TerminalKind::Agent(_))
                    {
                        slot.agent_state = *state;
                    }
                }
            }
            Event::TerminalExited { terminal_id, .. } => {
                // Process exited (`exit`, ^D, segfault, kill from
                // outside) — close the window. Mirrors how every other
                // terminal emulator behaves: the prompt goes away, the
                // pane goes with it. Auto-spawn won't re-fire because
                // it's gated on first selection of the session.
                self.terminals.remove(terminal_id);
                // Prune the tile tree so the kill surfaces visually:
                // a single-leaf split collapses to a Leaf root; an
                // n-way split loses just the dead branch. Tabs mode
                // doesn't carry tile state — no work to do there.
                if let pilot_core::SessionLayout::Splits { tree, focused } = &mut self.layout {
                    if let Some(path) = tree.path_to(terminal_id.0) {
                        match tree.remove_at(&path) {
                            Ok(new_focus) => {
                                *focused = new_focus;
                            }
                            Err(_) => {
                                // path was empty (the killed leaf was
                                // the only tile) → drop back to the
                                // tabs default so a future spawn opens
                                // a fresh layout instead of leaving an
                                // orphan tree.
                                self.layout = pilot_core::SessionLayout::Tabs { active: 0 };
                            }
                        }
                    }
                    // If the post-collapse tree is just a Leaf, drop
                    // back to Tabs — keeping a Splits-with-single-leaf
                    // payload renders fine but means the next spawn
                    // promotes us right back into Splits, which is
                    // confusing UX.
                    if let pilot_core::SessionLayout::Splits { tree, .. } = &self.layout
                        && matches!(tree, pilot_core::TileTree::Leaf { .. })
                    {
                        self.layout = pilot_core::SessionLayout::Tabs { active: 0 };
                    }
                }
                self.clamp_active_tab();
                self.auto_collapse_on_emptiness();
            }
            Event::WorkspaceRemoved(workspace_key) => {
                // Drop every terminal that belonged to the removed
                // workspace. Wire-side the slot's session_key carries
                // the workspace's key string, so a literal compare
                // is enough.
                let key_str = workspace_key.as_str();
                self.terminals
                    .retain(|_, slot| slot.session_key.as_str() != key_str);
                self.clamp_active_tab();
                self.auto_collapse_on_emptiness();
            }
            _ => {}
        }
    }

    pub fn render(&mut self, area: Rect, frame: &mut Frame, focused: bool) {
        // Modern minimal: title row + thin divider, no surrounding box.
        let theme = crate::theme::current();

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
            let (icon, label, is_asking) = self
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
                    let asking = matches!(s.agent_state, pilot_ipc::AgentState::Asking);
                    (icon, Self::tab_label(&s.kind), asking)
                })
                .unwrap_or((crate::components::icons::SHELL, "?".into(), false));
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
            // Bold yellow "!" next to an agent waiting on the user.
            // Stays prominent regardless of which tab is active so
            // the user notices a Claude prompt even while typing in
            // a different shell.
            if is_asking {
                let asking_text = " ! needs input";
                title_spans.push(Span::styled(
                    asking_text,
                    Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
                ));
                cursor = cursor.saturating_add(asking_text.chars().count() as u16);
            }
        }
        frame.render_widget(Paragraph::new(Line::from(title_spans)), title_area);

        if area.height >= 2 {
            let div_area = Rect::new(area.x + 1, area.y + 1, area.width.saturating_sub(2), 1);
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "─".repeat(div_area.width as usize),
                    theme.divider(),
                ))),
                div_area,
            );
        }

        let inner = Rect {
            x: area.x + 1,
            y: area.y + 3,
            width: area.width.saturating_sub(2),
            height: area.height.saturating_sub(3),
        };

        if visible.is_empty() {
            let line = Line::from(Span::styled(
                "(no terminals — press s for shell, c for claude)",
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
            pilot_core::SessionLayout::Tabs { .. } => {
                // Render the active tab full-pane (existing behavior).
                if let Some(id) = self.active_terminal_id() {
                    self.render_one_terminal(id, body, frame, focused);
                }
            }
            pilot_core::SessionLayout::Splits {
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
            pilot_core::SessionLayout::Splits { tree, .. } => tree,
            pilot_core::SessionLayout::Tabs { .. } => {
                let Some(current_id) = self.active_terminal_id() else {
                    // No terminal at all yet — the new spawn is just
                    // the first tab. Stay in Tabs mode.
                    return;
                };
                pilot_core::TileTree::Leaf {
                    terminal_id: current_id.0,
                }
            }
        };
        let focused_path = match &self.layout {
            pilot_core::SessionLayout::Splits { focused, .. } => focused.clone(),
            pilot_core::SessionLayout::Tabs { .. } => Vec::new(),
        };

        // Read the existing leaf at the focused path, build the new
        // split with [old, new] (so the new tile lands to the right
        // / below — matches tmux defaults), put it back at the path.
        let Some(existing) = subtree_at_path(&tree, &focused_path).cloned() else {
            return;
        };
        let new_leaf = pilot_core::TileTree::Leaf {
            terminal_id: new_id.0,
        };
        let new_split = match direction {
            PendingSplit::Vertical => pilot_core::TileTree::HSplit {
                left: Box::new(existing),
                right: Box::new(new_leaf),
                ratio: 50,
            },
            PendingSplit::Horizontal => pilot_core::TileTree::VSplit {
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
        self.layout = pilot_core::SessionLayout::Splits {
            tree,
            focused: new_focus,
        };
    }

    /// Tile-action dispatch: a key arriving right after `Ctrl-w`.
    /// Splits, focus moves, close, escape. Anything unrecognised is
    /// a clean no-op (the prefix has already been consumed; the user
    /// just has to retry).
    fn handle_tile_action(&mut self, key: KeyEvent, cmds: &mut Vec<Command>) -> PaneOutcome {
        use pilot_core::TileDirection;

        // Need an active session to know where to spawn into. Without
        // one, splits + new shells have nowhere to land.
        let Some(session_key) = self.active_session.clone() else {
            return PaneOutcome::Consumed;
        };

        match (key.code, key.modifiers) {
            (KeyCode::Char('|'), _) | (KeyCode::Char('\\'), _) => {
                self.begin_split(session_key, PendingSplit::Vertical, cmds);
            }
            (KeyCode::Char('-'), _) => {
                self.begin_split(session_key, PendingSplit::Horizontal, cmds);
            }
            (KeyCode::Left, _) => self.move_focus(TileDirection::Left, cmds),
            (KeyCode::Down, _) => self.move_focus(TileDirection::Down, cmds),
            (KeyCode::Up, _) => self.move_focus(TileDirection::Up, cmds),
            (KeyCode::Right, _) => self.move_focus(TileDirection::Right, cmds),
            (KeyCode::Char('q'), _) => self.close_focused_tile(cmds),
            _ => {}
        }
        PaneOutcome::Consumed
    }

    /// Stage 1 of a split: arm the pending-split flag and emit a
    /// shell-spawn command. The new terminal id arrives on
    /// `Event::TerminalSpawned`; that's where we mutate the layout.
    fn begin_split(
        &mut self,
        session_key: SessionKey,
        direction: PendingSplit,
        cmds: &mut Vec<Command>,
    ) {
        self.pending_split = Some(direction);
        cmds.push(Command::Spawn {
            session_key,
            session_id: None,
            kind: TerminalKind::Shell,
            cwd: None,
            initial_prompt: None,
        });
    }

    /// Move focus across the tile tree (or cycle through tabs in
    /// Tabs mode). Persists the new layout via `SetSessionLayout`.
    fn move_focus(&mut self, dir: pilot_core::TileDirection, cmds: &mut Vec<Command>) {
        match &mut self.layout {
            pilot_core::SessionLayout::Tabs { active } => {
                // In tabs mode h/l cycle the tab strip; j/k are no-ops
                // since there's only one row of "tabs" stacked vertically.
                let n = self.terminals.len();
                if n == 0 {
                    return;
                }
                match dir {
                    pilot_core::TileDirection::Left => {
                        *active = if *active == 0 { n - 1 } else { *active - 1 };
                    }
                    pilot_core::TileDirection::Right => {
                        *active = (*active + 1) % n;
                    }
                    _ => {}
                }
                self.active_tab_idx = *active;
            }
            pilot_core::SessionLayout::Splits { tree, focused } => {
                if let Some(new_path) = tree.neighbor(focused, dir) {
                    *focused = new_path;
                }
            }
        }
        self.persist_layout(cmds);
    }

    /// Close the focused leaf, collapsing its parent split into the
    /// surviving sibling. Single-leaf trees are refused (would leave
    /// the session with nothing visible).
    fn close_focused_tile(&mut self, cmds: &mut Vec<Command>) {
        let pilot_core::SessionLayout::Splits { tree, focused } = &mut self.layout else {
            return;
        };
        // Capture the terminal that's about to disappear before we
        // mutate the tree — we'll close its PTY too.
        let target_id = subtree_at_path(tree, focused).and_then(|n| match n {
            pilot_core::TileTree::Leaf { terminal_id } => Some(*terminal_id),
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
                self.layout = pilot_core::SessionLayout::Tabs { active: 0 };
                self.active_tab_idx = 0;
            }
            if let Some(id) = target_id {
                cmds.push(Command::Close {
                    terminal_id: TerminalId(id),
                });
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

    /// Render the "you ▸ <recap>" pin into `area`. Dim styling so
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
            // Carve off one row for the pinned "you ▸ <recap>" line
            // when this is an agent terminal with a remembered last
            // user message. Refuses to take the row at h ≤ 1 — leaves
            // every cell for the agent grid rather than blank-out the
            // pane entirely on a 1-row split.
            let show_recap = matches!(slot.kind, TerminalKind::Agent(_))
                && slot.last_user_message.is_some()
                && rect.height >= 2;
            let body = if show_recap {
                Rect {
                    x: rect.x,
                    y: rect.y + 1,
                    width: rect.width,
                    height: rect.height - 1,
                }
            } else {
                rect
            };
            if show_recap && let Some(msg) = slot.last_user_message.as_deref() {
                let header_rect = Rect {
                    x: rect.x,
                    y: rect.y,
                    width: rect.width,
                    height: 1,
                };
                Self::render_user_message_recap(frame, header_rect, msg);
            }
            slot.vt.ensure_size(body.width, body.height);
            // Backend PTY also needs to know the new size — otherwise
            // the shell process keeps writing at its spawn dimensions
            // and the bottom rows go blank as soon as the user scrolls
            // past them. Queue a resize for the App to ship.
            let new_size = (body.width, body.height);
            if body.width > 0 && body.height > 0 && slot.last_rendered_size != Some(new_size) {
                slot.last_rendered_size = Some(new_size);
                self.pending_resizes.push((id, body.width, body.height));
            }
            if let Ok(snapshot) = slot.vt.render_state.update(&slot.vt.terminal) {
                let widget = GhosttyTerminal::new(
                    &snapshot,
                    &mut slot.vt.row_iter,
                    &mut slot.vt.cell_iter,
                    &mut slot.vt.shadow,
                );
                frame.render_widget(widget, body);
            }
        }
    }

    /// Recursive walk of the tile tree. Each Leaf gets its own rect
    /// rendered via the existing per-terminal pipeline; each Split
    /// divides its rect according to `ratio` and recurses, drawing a
    /// thin divider line between the two children.
    #[allow(clippy::too_many_arguments)]
    fn render_tile_tree(
        &mut self,
        node: &pilot_core::TileTree,
        rect: Rect,
        frame: &mut Frame,
        pane_focused: bool,
        focus_path: &[u8],
        current_path: &[u8],
        chrome: Color,
        accent: Color,
    ) {
        match node {
            pilot_core::TileTree::Leaf { terminal_id } => {
                let is_focused_leaf = pane_focused && current_path == focus_path;
                self.render_one_terminal(TerminalId(*terminal_id), rect, frame, is_focused_leaf);
                // Highlight the focused leaf with a one-cell top
                // accent line. Subtle but enough to disambiguate
                // when two shells look identical.
                if is_focused_leaf && rect.height > 0 && rect.width > 0 {
                    let bar = Rect {
                        x: rect.x,
                        y: rect.y,
                        width: rect.width,
                        height: 1,
                    };
                    frame.render_widget(
                        Paragraph::new(Line::from(Span::styled(
                            "─".repeat(bar.width as usize),
                            Style::default().fg(accent),
                        ))),
                        bar,
                    );
                }
            }
            pilot_core::TileTree::HSplit { left, right, ratio } => {
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
            pilot_core::TileTree::VSplit { top, bottom, ratio } => {
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
    match key.code {
        Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            // Ctrl-<letter>: low control byte.
            Some(vec![(c as u8) & 0x1f])
        }
        Char(c) => {
            let mut buf = [0u8; 4];
            Some(c.encode_utf8(&mut buf).as_bytes().to_vec())
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
        Backspace => Some(vec![0x7f]),
        Esc => Some(vec![0x1b]),
        Tab => Some(vec![b'\t']),
        BackTab => Some(b"\x1b[Z".to_vec()),
        Up => Some(b"\x1b[A".to_vec()),
        Down => Some(b"\x1b[B".to_vec()),
        Right => Some(b"\x1b[C".to_vec()),
        Left => Some(b"\x1b[D".to_vec()),
        Home => Some(b"\x1b[H".to_vec()),
        End => Some(b"\x1b[F".to_vec()),
        Delete => Some(b"\x1b[3~".to_vec()),
        _ => None,
    }
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

/// Classify the token at `byte_pos` in `row_text`. Tries detectors
/// in specificity order: a URL (begins with a scheme) wins over an
/// issue reference, which wins over a bare file path. Returns `None`
/// when the click landed on whitespace or an unrecognized token.
pub(crate) fn detect_target(row_text: &str, byte_pos: usize) -> Option<ClickTarget> {
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
    // `pilot_core::issue_links`: the repo part must contain a `/` and
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
    let looks_like_path = tok.starts_with('/')
        || tok == "~"
        || tok.starts_with("~/")
        || tok.starts_with("./")
        || tok.starts_with("../");
    if !looks_like_path {
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
            detect_target(row, 6),
            Some(ClickTarget::Url("https://github.com/o/r/issues/9".into()))
        );
    }

    #[test]
    fn detects_absolute_path() {
        let row = "see /etc/hosts for config";
        assert_eq!(
            detect_target(row, 5),
            Some(ClickTarget::Path {
                path: "/etc/hosts".into(),
                line: None,
                col: None,
            })
        );
    }

    #[test]
    fn detects_home_relative_path() {
        let row = "edit ~/.config/pilot.yaml please";
        assert_eq!(
            detect_target(row, 6),
            Some(ClickTarget::Path {
                path: "~/.config/pilot.yaml".into(),
                line: None,
                col: None,
            })
        );
    }

    #[test]
    fn detects_dot_relative_path() {
        let row = "open ./src/main.rs here";
        assert_eq!(
            detect_target(row, 5),
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
            detect_target(row, 4),
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
            detect_target(row, 10),
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
            detect_target(row, 6),
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
        assert_eq!(detect_target(row, 5), None);
    }

    #[test]
    fn relative_without_dot_prefix_is_not_a_path() {
        // `src/main.rs` (no leading ./) is too ambiguous — could be
        // prose — so we require an explicit prefix.
        let row = "src/main.rs changed";
        assert_eq!(detect_target(row, 2), None);
    }

    #[test]
    fn detects_same_repo_issue() {
        let row = "fixed in #42 today";
        assert_eq!(
            detect_target(row, 10),
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
            detect_target(row, 4),
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
            detect_target(row, 7),
            Some(ClickTarget::Issue {
                repo: None,
                number: 99,
            })
        );
    }

    #[test]
    fn hash_without_digits_is_not_an_issue() {
        let row = "a #section heading";
        assert_eq!(detect_target(row, 2), None);
    }

    #[test]
    fn whitespace_click_returns_none() {
        let row = "see /etc/hosts here";
        // Column 3 is the space before the path.
        assert_eq!(detect_target(row, 3), None);
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
mod osc52_tests {
    use super::osc52_ranges;

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
    fn unterminated_sequence_is_dropped() {
        // Sequence starts but no BEL/ST in the chunk. Drop rather
        // than write a half-sequence to the host that could leave
        // it in OSC-parsing mode.
        let bytes = b"\x1b]52;c;aGVsbG8=";
        assert!(osc52_ranges(bytes).is_empty());
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
