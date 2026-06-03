//! `Terminals` — tuirealm wrapper around
//! `crate::components::terminal_stack::TerminalStack`.
//!
//! Hosts the libghostty embed (which is `!Send`). The probe earlier
//! validated `!Send` components mount cleanly inside `Application`,
//! so this wrapper just delegates render + key dispatch.

use crate::PaneId;
use crate::components::terminal_stack::TerminalStack as LazyboxTerminals;
use crate::realm::keymap::realm_key_to_crossterm;
use crate::realm::{Msg, UserEvent};
use lazybox_core::SessionKey;
use lazybox_ipc::Command as IpcCommand;
use lazybox_ipc::Event as IpcEvent;
use lazybox_ipc::TerminalId;
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::Event;
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::state::State;

/// tuirealm-shaped terminal stack.
pub struct Terminals {
    inner: LazyboxTerminals,
    focused: bool,
    pending_cmds: Vec<IpcCommand>,
}

impl Terminals {
    /// Construct.
    pub fn new(id: PaneId) -> Self {
        Self {
            inner: LazyboxTerminals::new(id),
            focused: false,
            pending_cmds: Vec::new(),
        }
    }

    /// Drain queued IPC commands (writes / resizes / etc).
    pub fn drain_cmds(&mut self) -> Vec<IpcCommand> {
        // Render-time resizes also need to drain.
        let mut cmds = std::mem::take(&mut self.pending_cmds);
        for (terminal_id, cols, rows) in self.inner.drain_pending_resizes() {
            cmds.push(IpcCommand::Resize {
                terminal_id,
                cols,
                rows,
            });
        }
        cmds
    }

    /// Set which session's terminals to display.
    /// The session whose terminals are currently visible, if any.
    /// Lets the model anchor relative-path / same-repo `#N` resolution
    /// to the focused workspace.
    pub fn active_session(&self) -> Option<&SessionKey> {
        self.inner.active_session()
    }

    pub fn set_active_session(&mut self, session: Option<SessionKey>) {
        self.inner.set_active_session(session);
    }

    /// Replace the active layout. Used when the user navigates
    /// between workspaces — each workspace's default session has
    /// its own persisted SessionLayout (Tabs vs Splits with a
    /// specific tile tree), and the terminal_stack needs to swap
    /// to it so the panes render with the right arrangement.
    /// Without this call, switching workspaces shows the previous
    /// workspace's layout against the new workspace's terminals.
    pub fn set_layout(&mut self, layout: lazybox_core::SessionLayout) {
        self.inner.set_layout(layout);
    }

    /// Currently active terminal id (the one keys route to).
    pub fn active_terminal_id(&self) -> Option<TerminalId> {
        self.inner.active_terminal_id()
    }

    /// Forward a daemon event so the inner stack stays in sync.
    pub fn on_daemon_event(&mut self, evt: &IpcEvent) {
        self.inner.on_event(evt);
    }

    /// Direct render entry point.
    pub fn view_in(&mut self, area: Rect, frame: &mut Frame) {
        self.inner.render(area, frame, self.focused);
    }

    /// Direct key dispatch.
    pub fn handle_key_direct(
        &mut self,
        key: crossterm::event::KeyEvent,
        cmds: &mut Vec<IpcCommand>,
    ) {
        let _ = self.inner.handle_key(key, cmds);
    }

    /// Update the focused-flag.
    pub fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    /// State-aware short list for the footer hint bar. Catalog-driven
    /// (scroll + leave-terminal) plus the hand-curated `Ctrl-c`
    /// interrupt hint — see `TerminalStack::contextual_bindings_static`
    /// for the per-row rationale.
    pub fn contextual_bindings(
        &self,
        overrides: &std::collections::BTreeMap<String, String>,
    ) -> Vec<crate::pane::Binding> {
        crate::components::terminal_stack::TerminalStack::contextual_bindings(overrides)
    }

    /// Detach spec for the focused tile, if any (delegates to the
    /// inner stack's `detachable()` which scopes to the active tab).
    pub fn detachable(&self) -> Option<crate::pane::DetachSpec> {
        self.inner.detachable()
    }

    /// Scroll the active terminal's viewport by `delta` rows. Negative
    /// = into scrollback; positive = back toward the live content.
    /// Driven from the orchestrator's mouse-wheel handler.
    pub fn scroll_active(
        &mut self,
        delta: isize,
    ) -> crate::components::terminal_stack::ScrollOutcome {
        self.inner.scroll_active(delta)
    }

    /// Forward `extract_text` — read the focused terminal's grid
    /// between two absolute frame-space coordinates and return the
    /// plain text. Used by the mouse-up selection-copy path.
    pub fn extract_text(
        &mut self,
        rect: tuirealm::ratatui::layout::Rect,
        start: (u16, u16),
        end: (u16, u16),
    ) -> String {
        self.inner.extract_text(rect, start, end)
    }

    /// Classify whatever the focused terminal's grid shows at the
    /// frame-space cell `(col, row)` — a URL, file path, or issue
    /// reference. Used by the right-click handler to detect "the user
    /// clicked on something openable" and route it before falling
    /// through to PTY mouse forwarding.
    pub fn target_at(
        &mut self,
        rect: tuirealm::ratatui::layout::Rect,
        col: u16,
        row: u16,
    ) -> Option<crate::components::terminal_stack::ClickTarget> {
        self.inner.target_at(rect, col, row)
    }

    /// Human-readable scrollbar diagnostic for the focused
    /// terminal. Used by the orchestrator's scroll-event handler
    /// to surface viewport state in the footer.
    pub fn scrollbar_summary(&self) -> Option<String> {
        self.inner.scrollbar_summary()
    }

    /// Click-to-switch tabs. Returns the tab index when the click
    /// landed on a tab label; the caller invokes `set_active_tab`
    /// to actually flip.
    pub fn tab_at(&self, col: u16, row: u16) -> Option<usize> {
        self.inner.tab_at(col, row)
    }

    pub fn set_active_tab(&mut self, idx: usize) {
        self.inner.set_active_tab(idx);
    }

    /// True when this stack has no visible terminals for the active
    /// session. Used by the orchestrator to fall back from "key into
    /// PTY" to "key into sidebar's spawn binding" so `s`/`c`/`x`/`u`
    /// from the empty-state hint actually create a session.
    pub fn is_empty(&self) -> bool {
        self.inner.visible_terminals().is_empty()
    }

    /// True when the focused terminal's inner program has enabled
    /// mouse tracking (CSI ?1000h / ?1002h / ?1003h / ?1006h SGR).
    /// Drives the "forward to PTY vs scroll the scrollback"
    /// decision in `Model::handle_mouse`.
    pub fn focused_terminal_tracks_mouse(&self) -> bool {
        self.inner.focused_terminal_tracks_mouse()
    }

    /// True if the focused terminal's inner process is on the
    /// alternate screen. Drives the "wheel → arrow keys" fast
    /// path in `Model::handle_mouse`.
    pub fn focused_terminal_in_alt_screen(&self) -> bool {
        self.inner.focused_terminal_in_alt_screen()
    }

    /// Wire id of the currently focused terminal, if any. Needed by
    /// the wheel handler so it can address its synthetic arrow-key
    /// `Write` at the right pane.
    pub fn focused_terminal_id(&self) -> Option<lazybox_ipc::TerminalId> {
        self.inner.focused_terminal_id()
    }

    /// Mirror a bracketed-paste payload into the focused terminal's
    /// user-message tracker so the pinned recap line picks up text
    /// that arrives via paste (not just key-by-key typing). No-op
    /// when the focused terminal isn't an Agent. The caller is still
    /// responsible for sending the paste bytes to the PTY — this
    /// only updates lazybox's own composing buffer.
    pub fn record_paste(&mut self, text: &str) {
        self.inner.record_paste(text);
    }

    /// Mirror bytes written straight to a terminal's PTY (e.g. a
    /// snippet body submitted in one shot) into that terminal's
    /// user-message tracker, so the pinned recap reflects commands
    /// that never flow through the key-by-key path. No-op for
    /// non-Agent terminals. The caller still sends the bytes to the
    /// PTY — this only updates lazybox's own composing buffer.
    pub fn record_pty_write(&mut self, id: lazybox_ipc::TerminalId, bytes: &[u8]) {
        self.inner.record_pty_write(id, bytes);
    }

    /// Encode a mouse event for the focused terminal. Returns the
    /// bytes to `Write` to the PTY (paired with the target terminal
    /// id), or None when the terminal isn't tracking mouse or the
    /// event encodes to nothing under its active protocol.
    pub fn encode_mouse(
        &mut self,
        action: libghostty_vt::mouse::Action,
        button: Option<libghostty_vt::mouse::Button>,
        cell_col: u32,
        cell_row: u32,
    ) -> Option<(lazybox_ipc::TerminalId, Vec<u8>)> {
        self.inner
            .encode_mouse_for_focused(action, button, cell_col, cell_row)
    }
}

impl Component for Terminals {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        self.inner.render(area, frame, self.focused);
    }

    fn query(&self, _: Attribute) -> Option<QueryResult<'_>> {
        None
    }

    fn attr(&mut self, attr: Attribute, value: AttrValue) {
        if let (Attribute::Focus, AttrValue::Flag(f)) = (attr, value) {
            self.focused = f;
        }
    }

    fn state(&self) -> State {
        State::None
    }

    fn perform(&mut self, _: Cmd) -> CmdResult {
        CmdResult::NoChange
    }
}

impl AppComponent<Msg, UserEvent> for Terminals {
    fn on(&mut self, ev: &Event<UserEvent>) -> Option<Msg> {
        match ev {
            Event::User(UserEvent::Daemon(evt)) => {
                self.inner.on_event(evt);
                None
            }
            Event::Keyboard(key) if self.focused => {
                let ct_key = realm_key_to_crossterm(key);
                let mut cmds: Vec<IpcCommand> = Vec::new();
                let _ = self.inner.handle_key(ct_key, &mut cmds);
                if !cmds.is_empty() {
                    self.pending_cmds.extend(cmds);
                    return Some(Msg::TerminalCmds);
                }
                None
            }
            _ => None,
        }
    }
}
