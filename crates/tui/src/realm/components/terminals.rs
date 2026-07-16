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

    /// Fold resolved UI config into the terminal stack: the
    /// dead-on-arrival grace window that gates auto-close of exited
    /// agent panes (#367) and the `terminal_new_layout` preference
    /// (tab vs split for auto spawns, #361).
    pub fn apply_ui_defaults(&mut self, ui: &lazybox_config::UiDefaults) {
        self.inner.apply_ui_defaults(ui);
    }

    /// Current new-terminal layout preference (tab vs split for auto
    /// spawns). Read by the `]]` leader popup to label the `]]t` row.
    pub fn terminal_new_layout(&self) -> lazybox_config::NewTerminalLayout {
        self.inner.terminal_new_layout()
    }

    /// `]]t` — flip the new-terminal layout preference, returning the
    /// new value so the caller can persist + flash it.
    pub fn toggle_terminal_new_layout(&mut self) -> lazybox_config::NewTerminalLayout {
        self.inner.toggle_terminal_new_layout()
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

    /// Whether the tracked terminal runs an agent (vs a plain shell) —
    /// drives whether snippet submission uses the daemon inject path.
    pub fn terminal_is_agent(&self, id: TerminalId) -> bool {
        self.inner.terminal_is_agent(id)
    }

    /// The session a tracked terminal belongs to. Used by the spawn-
    /// follow pin to recover the workspace for a `TerminalFocusRequested`.
    pub fn session_key_for(&self, id: TerminalId) -> Option<&SessionKey> {
        self.inner.session_key_for(id)
    }

    /// Terminal-slot count for a session — the baseline a non-singleton
    /// spawn (shell) is measured against by the spawn-spinner projection.
    pub fn terminal_count_for(&self, session_key: &SessionKey) -> usize {
        self.inner.terminal_count_for(session_key)
    }

    /// Whether a spawn of `kind` into `session_key` has produced its
    /// terminal yet (the spawn-spinner projection, #206).
    pub fn spawn_satisfied(
        &self,
        session_key: &SessionKey,
        kind: &lazybox_ipc::TerminalKind,
        baseline_count: usize,
    ) -> bool {
        self.inner
            .spawn_satisfied(session_key, kind, baseline_count)
    }

    /// Promote `target` to the active tab (no-op if it isn't in the
    /// active session's visible set). Used by the spawn-follow pin.
    pub fn focus_terminal(&mut self, target: TerminalId) -> bool {
        self.inner.focus_terminal(target)
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

    /// `]]|` / `]]-` — split the focused tile by spawning a shell
    /// sibling (stage 2 completes on `TerminalSpawned`).
    pub fn split_tile(
        &mut self,
        direction: crate::components::terminal_stack::PendingSplit,
        cmds: &mut Vec<IpcCommand>,
    ) {
        self.inner.split_tile(direction, cmds);
    }

    /// `]]<arrow>` — move tile focus (or cycle tabs in Tabs mode).
    pub fn move_tile_focus(
        &mut self,
        dir: lazybox_core::TileDirection,
        cmds: &mut Vec<IpcCommand>,
    ) {
        self.inner.move_tile_focus(dir, cmds);
    }

    /// `]]x` — close the focused terminal (tile or active tab) and
    /// its PTY.
    pub fn close_focused_tile(&mut self, cmds: &mut Vec<IpcCommand>) {
        self.inner.close_focused_tile(cmds);
    }

    /// Whether the active session renders as a tile tree (vs Tabs).
    /// Drives the layout-tailored rows of the `]]` leader popup.
    pub fn layout_is_splits(&self) -> bool {
        matches!(
            self.inner.layout(),
            lazybox_core::SessionLayout::Splits { .. }
        )
    }

    /// Number of terminals in the active session's visible set. Also
    /// feeds the leader popup's layout-tailored rows.
    pub fn visible_terminal_count(&self) -> usize {
        self.inner.visible_terminals().len()
    }

    /// State-aware short list for the footer hint bar — the `]]` leave,
    /// `]]f` focus-mode and `]]|` split leader chords plus the
    /// hand-curated `Ctrl-c` interrupt hint. See
    /// `TerminalStack::contextual_bindings` for the per-row rationale.
    pub fn contextual_bindings(&self, escape_char: char) -> Vec<crate::pane::Binding> {
        crate::components::terminal_stack::TerminalStack::contextual_bindings(escape_char)
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

    /// Screen `(col, row)` → 0-based grid cell coords inside the focused
    /// terminal body, or `None` when the point is in the pane chrome.
    /// Used before forwarding a click/wheel to a mouse-tracking inner
    /// program so the coordinate matches what the renderer drew.
    pub fn screen_to_cell(
        &self,
        rect: tuirealm::ratatui::layout::Rect,
        col: u16,
        row: u16,
    ) -> Option<(u32, u32)> {
        self.inner.screen_to_cell(rect, col, row)
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
    /// PTY" to "key into sidebar's spawn binding" so `s` and the
    /// `a c`/`a x`/`a u` chords from the empty-state hint actually
    /// create a session.
    pub fn is_empty(&self) -> bool {
        self.inner.visible_terminals().is_empty()
    }

    /// True when `id` belongs to the active session's visible set and
    /// the pane isn't collapsed to its header — i.e. bytes appended to
    /// it can change pixels on screen. The orchestrator uses this to
    /// skip redraws for output addressed at terminals the user isn't
    /// looking at.
    pub fn is_terminal_visible(&self, id: TerminalId) -> bool {
        !self.inner.is_collapsed() && self.inner.visible_terminals().contains(&id)
    }

    /// Forward `displays_agent_state` — true when every agent tab
    /// badge for `session_key` already shows `state`, so the event
    /// can't change pixels in the terminal stack.
    pub fn displays_agent_state(
        &self,
        session_key: &SessionKey,
        state: lazybox_ipc::AgentState,
    ) -> bool {
        self.inner.displays_agent_state(session_key, state)
    }

    /// True when the focused terminal's inner program has enabled
    /// mouse tracking (CSI ?1000h / ?1002h / ?1003h / ?1006h SGR).
    /// Drives the "forward to PTY vs scroll the scrollback"
    /// decision in `Model::handle_mouse`.
    pub fn focused_terminal_tracks_mouse(&self) -> bool {
        self.inner.focused_terminal_tracks_mouse()
    }

    /// How a mouse-wheel tick over the focused terminal routes —
    /// local scrollback, SGR forward, or synthesized arrow keys. See
    /// [`crate::components::terminal_stack::WheelRoute`]. Drives the
    /// wheel branch in `Model::handle_mouse`.
    pub fn wheel_route(&self) -> crate::components::terminal_stack::WheelRoute {
        self.inner.wheel_route()
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
    /// PTY — this only updates lazybox's own composing buffer. Returns
    /// the committed message (when the write ended in a submit) so the
    /// caller can persist it via `Command::RecordUserMessage`.
    pub fn record_pty_write(
        &mut self,
        id: lazybox_ipc::TerminalId,
        bytes: &[u8],
    ) -> Option<String> {
        self.inner.record_pty_write(id, bytes)
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
