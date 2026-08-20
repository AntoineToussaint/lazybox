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
use lazybox_ipc::TerminalResyncRequest;
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

    pub fn terminal_agent_state(&self, id: TerminalId) -> Option<lazybox_ipc::AgentState> {
        self.inner.terminal_agent_state(id)
    }

    pub(crate) fn terminal_agent_id(&self, id: TerminalId) -> Option<&str> {
        self.inner.terminal_agent_id(id)
    }

    /// Label of the terminal keystrokes currently flow to — the agent id
    /// (`claude`, `codex`, …) or `shell` for a plain shell. `None` when
    /// no terminal is active. Drives the footer's `▶ typing to: <agent>`
    /// focus chip (#1110); uses the focused leaf (Splits) / active tab
    /// (Tabs), which is exactly where a keystroke would land.
    pub fn active_typing_target(&self) -> Option<String> {
        let id = self.inner.focused_terminal_id()?;
        Some(match self.inner.terminal_agent_id(id) {
            Some(agent) => agent.to_string(),
            None => "shell".to_string(),
        })
    }

    pub(crate) fn terminal_is_on_main(&self, id: TerminalId) -> bool {
        self.inner.terminal_is_on_main(id)
    }

    pub(crate) fn terminal_no_permission(&self, id: TerminalId) -> bool {
        self.inner.terminal_no_permission(id)
    }

    pub(crate) fn prepare_agent_replacement(
        &mut self,
        id: TerminalId,
        client_request_id: &str,
        cmds: &mut Vec<IpcCommand>,
    ) -> bool {
        self.inner
            .prepare_agent_replacement(id, client_request_id, cmds)
    }

    pub(crate) fn cancel_agent_replacement(&mut self, id: TerminalId) {
        self.inner.cancel_agent_replacement(id);
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

    /// Client-observed terminal gaps that need a daemon replay request.
    pub fn drain_pending_resync_requests(&mut self) -> Vec<TerminalResyncRequest> {
        self.inner.drain_pending_resync_requests()
    }

    /// Restore recovery debt when the bounded command channel rejects a
    /// batch; the next daemon event retries it.
    pub fn requeue_resync_requests(&mut self, requests: Vec<TerminalResyncRequest>) {
        self.inner.requeue_resync_requests(requests);
    }

    /// Direct render entry point.
    pub fn view_in(&mut self, area: Rect, frame: &mut Frame) {
        self.inner.render(area, frame, self.focused);
    }

    /// Per-frame prologue for the focus-mode multi-pane render path
    /// (#1258); call once before the first `render_terminal_by_id`.
    pub fn begin_focus_frame(&mut self) {
        self.inner.begin_focus_frame();
    }

    /// Render one terminal into a focus-mode workspace pane (#1258).
    /// `pane_focused` marks the focused pane; the terminal renders as
    /// keyboard-focused only when the stack itself holds pane focus too.
    pub fn render_terminal_by_id(
        &mut self,
        id: TerminalId,
        area: Rect,
        frame: &mut Frame,
        pane_focused: bool,
    ) {
        self.inner
            .render_terminal_by_id(id, area, frame, self.focused && pane_focused);
    }

    /// The terminal a focus-mode pane displays for a session: agent
    /// first, else its most recent terminal of any kind (#1258).
    pub fn display_terminal_for(&self, session_key: &SessionKey) -> Option<TerminalId> {
        self.inner.display_terminal_for(session_key)
    }

    /// Sessions with a live agent terminal, most recent first — the
    /// focus-pane roster's shortfall fallback (#1258).
    pub fn recent_agent_sessions(&self) -> Vec<SessionKey> {
        self.inner.recent_agent_sessions()
    }

    /// Compact agent-state badge for a focus-pane header (#1258).
    pub fn pane_state_badge(
        &self,
        id: TerminalId,
        theme: &crate::theme::Theme,
    ) -> Option<(&'static str, tuirealm::ratatui::style::Style)> {
        self.inner.pane_state_badge(id, theme)
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

    /// Focus a terminal tile directly and persist the split layout.
    pub fn focus_tile(&mut self, target: TerminalId, cmds: &mut Vec<IpcCommand>) -> bool {
        self.inner.focus_tile(target, cmds)
    }

    /// `]]x` — close the focused terminal (tile or active tab) and
    /// its PTY.
    pub fn close_focused_tile(&mut self, cmds: &mut Vec<IpcCommand>) {
        self.inner.close_focused_tile(cmds);
    }

    /// `]]z` — toggle tmux-style zoom of the focused tile. Returns the
    /// resulting state (`Some(true/false)`), or `None` when there is
    /// nothing to zoom (Tabs, or a single tile).
    pub fn toggle_zoom(&mut self) -> Option<bool> {
        self.inner.toggle_zoom()
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

    /// The terminal command leader shown in the footer hint bar.
    pub fn contextual_bindings(&self, escape_char: char) -> Vec<crate::pane::Binding> {
        crate::components::terminal_stack::TerminalStack::contextual_bindings(escape_char)
    }

    /// Scroll a specific terminal's viewport — the tile under the mouse
    /// cursor (#362). The wheel handler resolves the hovered tile via
    /// [`Self::scroll_terminal_at`] and routes the scroll here so focus never
    /// has to move for the wheel to hit the right pane.
    pub fn scroll_terminal(
        &mut self,
        id: TerminalId,
        delta: isize,
    ) -> crate::components::terminal_stack::ScrollOutcome {
        self.inner.scroll_terminal(id, delta)
    }

    /// Terminal whose tile the point `(col, row)` lands in. Drives
    /// hover-to-scroll in the wheel handler. `None` over pane chrome.
    pub fn scroll_terminal_at(&self, col: u16, row: u16) -> Option<TerminalId> {
        self.inner.scroll_terminal_at(col, row)
    }

    /// Terminal whose full tile contains `(col, row)`, including its
    /// focus bar. Drives click-to-focus without widening wheel targets.
    pub fn tile_at(&self, col: u16, row: u16) -> Option<TerminalId> {
        self.inner.tile_at(col, row)
    }

    /// Take the deep-scrollback fetch an upward scroll armed, if any.
    /// The orchestrator ships it as a `Command::FetchScrollback`.
    pub fn take_scrollback_fetch(&mut self) -> Option<TerminalId> {
        self.inner.take_scrollback_fetch()
    }

    /// Scroll the focused terminal's viewport by `delta` rows. Used by
    /// the edge drag-selection auto-scroll (#432).
    pub fn scroll_active(
        &mut self,
        delta: isize,
    ) -> crate::components::terminal_stack::ScrollOutcome {
        self.inner.scroll_active(delta)
    }

    /// Crossterm `(col, row)` → screen-absolute grid coords
    /// `(col, screen_row)` for terminal `id`, clamped into `id`'s grid. The
    /// anchor / focus of a lazybox drag-selection are stored in this space
    /// so they stay pinned to content across an auto-scroll, and clamping
    /// to `id`'s grid scopes the selection to the tile the drag started in
    /// (#1101).
    pub fn selection_point(
        &self,
        id: TerminalId,
        rect: tuirealm::ratatui::layout::Rect,
        col: u16,
        row: u16,
    ) -> Option<(u16, u32)> {
        self.inner.selection_point(id, rect, col, row)
    }

    /// Extract the plain text of a screen-absolute selection span from
    /// terminal `id` — the whole passage, including rows scrolled off the
    /// viewport. Used by the mouse-up selection-copy path.
    pub fn extract_selection(
        &mut self,
        id: TerminalId,
        anchor: (u16, u32),
        focus: (u16, u32),
    ) -> String {
        self.inner.extract_selection(id, anchor, focus)
    }

    /// Project a screen-absolute selection span back to terminal `id`'s
    /// on-screen crossterm cells currently visible in `rect`, for the
    /// reverse-video highlight overlay. `None` when `id` is unknown.
    pub fn selection_screen_span(
        &self,
        id: TerminalId,
        rect: tuirealm::ratatui::layout::Rect,
        anchor: (u16, u32),
        focus: (u16, u32),
    ) -> Option<((u16, u16), (u16, u16))> {
        self.inner.selection_screen_span(id, rect, anchor, focus)
    }

    /// Forward `visible_text` — dump a terminal's whole visible grid
    /// as plain text. Seeds the agent-to-agent handoff compose step
    /// (`x s`) with the source agent's on-screen output.
    pub fn visible_text(&mut self, id: TerminalId) -> Option<String> {
        self.inner.visible_text(id)
    }

    /// Forward `focused_urls` — every `http(s)://…` URL visible in the
    /// focused terminal's grid, screen order, de-duplicated. Drives the
    /// `]]u` keyboard URL picker (#596).
    pub fn focused_urls(&mut self) -> Option<Vec<String>> {
        self.inner.focused_urls()
    }

    /// Like [`Self::focused_urls`] but scans an explicit terminal — the
    /// sidebar `]]u` scans the cursor workspace's terminal (#871).
    pub fn urls_for(&mut self, id: TerminalId) -> Option<Vec<String>> {
        self.inner.urls_for(id)
    }

    /// Forward `agent_terminal_for` — the agent terminal for a session
    /// (live, else a kept exited pane). Resolves the handoff source so a
    /// finished-and-exited agent's final output can still be captured.
    pub fn agent_terminal_for(&self, session_key: &SessionKey) -> Option<TerminalId> {
        self.inner.agent_terminal_for(session_key)
    }

    /// Resolve frame coordinates against the tile rectangles recorded
    /// by the last render, then classify the clicked terminal's cell.
    pub(crate) fn rendered_target_at(
        &mut self,
        col: u16,
        row: u16,
    ) -> Option<crate::components::terminal_stack::RenderedClickTarget> {
        self.inner.rendered_target_at(col, row)
    }

    /// Screen `(col, row)` → 0-based grid cell coords inside the focused
    /// terminal body, or `None` when the point is in the pane chrome.
    /// Used before forwarding a click to a mouse-tracking inner
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
    /// Used to forward clicks; wheels stay in lazybox scrollback.
    pub fn focused_terminal_tracks_mouse(&self) -> bool {
        self.inner.focused_terminal_tracks_mouse()
    }

    pub fn terminal_tracks_mouse(&self, terminal_id: lazybox_ipc::TerminalId) -> bool {
        self.inner.terminal_tracks_mouse(terminal_id)
    }

    /// Wire id of the currently focused terminal, if any.
    pub fn focused_terminal_id(&self) -> Option<lazybox_ipc::TerminalId> {
        self.inner.focused_terminal_id()
    }

    /// Mirror a bracketed-paste payload into the focused terminal's
    /// user-message tracker so the pinned recap line picks up text
    /// that arrives via paste (not just key-by-key typing). No-op
    /// when the focused terminal isn't an Agent. The caller is still
    /// responsible for sending the paste bytes to the PTY — this
    /// only updates lazybox's own composing buffer. Returns the focused
    /// terminal id and its updated draft so the caller can persist it
    /// via `Command::RecordComposingBuffer`.
    pub fn record_paste(&mut self, text: &str) -> Option<(lazybox_ipc::TerminalId, String)> {
        self.inner.record_paste(text)
    }

    /// Append a compose-only insert (the `Shift-Enter` snippet body, #791)
    /// into a specific terminal's composing buffer, returning the target id
    /// and updated draft for `Command::RecordComposingBuffer`. No-op for
    /// non-Agent terminals.
    pub fn record_compose_insert(
        &mut self,
        id: lazybox_ipc::TerminalId,
        text: &str,
    ) -> Option<(lazybox_ipc::TerminalId, String)> {
        self.inner.record_compose_insert(id, text)
    }

    /// The text a `]]r` recall should drop back into the focused agent
    /// composer (in-flight draft, else last submitted message), with the
    /// target terminal id. `None` when there's nothing to recall.
    pub fn recall_prompt(&mut self) -> Option<(lazybox_ipc::TerminalId, String)> {
        self.inner.recall_prompt()
    }

    /// Like [`Self::recall_prompt`] but for an explicit terminal — the
    /// sidebar `]]r` targets the cursor workspace's agent (#871).
    pub fn recall_prompt_for(
        &mut self,
        id: lazybox_ipc::TerminalId,
    ) -> Option<(lazybox_ipc::TerminalId, String)> {
        self.inner.recall_prompt_for(id)
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
        source: lazybox_ipc::PromptSource,
    ) -> Option<lazybox_ipc::UserPrompt> {
        self.inner.record_pty_write(id, bytes, source)
    }

    pub fn apply_delivered_prompt(
        &mut self,
        id: lazybox_ipc::TerminalId,
        prompt: lazybox_ipc::UserPrompt,
    ) {
        self.inner.apply_delivered_prompt(id, prompt);
    }

    /// The focused agent terminal's prompt history, newest-first, for the
    /// `]]h` history picker (issue #523). See
    /// [`crate::components::terminal_stack::TerminalStack::focused_prompt_history`].
    pub fn focused_prompt_history(
        &self,
    ) -> Option<(lazybox_ipc::TerminalId, Vec<lazybox_ipc::UserPrompt>)> {
        self.inner.focused_prompt_history()
    }

    /// Like [`Self::focused_prompt_history`] but for an explicit terminal —
    /// the sidebar `]]h` targets the cursor workspace's agent (#871).
    pub fn prompt_history_for(
        &self,
        id: lazybox_ipc::TerminalId,
    ) -> Option<(lazybox_ipc::TerminalId, Vec<lazybox_ipc::UserPrompt>)> {
        self.inner.prompt_history_for(id)
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

    pub fn encode_mouse_for(
        &mut self,
        terminal_id: lazybox_ipc::TerminalId,
        action: libghostty_vt::mouse::Action,
        button: Option<libghostty_vt::mouse::Button>,
        cell_col: u32,
        cell_row: u32,
    ) -> Option<(lazybox_ipc::TerminalId, Vec<u8>)> {
        self.inner
            .encode_mouse_for(terminal_id, action, button, cell_col, cell_row)
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
