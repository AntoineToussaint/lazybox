//! `Right` — tuirealm wrapper around `crate::components::right_pane::RightPane`.
//!
//! Same pattern as the sidebar wrapper: hold an instance of the
//! existing pilot pane and delegate `view`/`on_event`/`handle_key`
//! through UFCS.

use crate::PaneId;
use crate::components::right_pane::RightPane as PilotRight;
use crate::realm::keymap::realm_key_to_crossterm;
use crate::realm::{Msg, UserEvent};
use pilot_ipc::Command as IpcCommand;
use pilot_ipc::Event as IpcEvent;
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::Event;
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::state::State;

/// tuirealm-shaped right pane.
pub struct Right {
    inner: PilotRight,
    focused: bool,
    pending_cmds: Vec<IpcCommand>,
}

impl Right {
    /// Construct.
    pub fn new(id: PaneId) -> Self {
        Self {
            inner: PilotRight::new(id),
            focused: false,
            pending_cmds: Vec::new(),
        }
    }

    /// Set the workspace whose details + activity feed the pane
    /// renders. Called from `Model::update` after sidebar selection
    /// changes.
    pub fn set_workspace(&mut self, workspace: Option<pilot_core::Workspace>) {
        self.inner.set_workspace(workspace);
    }

    /// Forward the YAML-configured `setup.default_agent` to the inner
    /// pane so `f`-on-selection spawns the user's preferred agent.
    pub fn set_default_agent(&mut self, agent: impl Into<String>) {
        self.inner.set_default_agent(agent);
    }

    /// Forward viewer identities (source → login) to the inner pane
    /// so activity bylines authored by the local user render as `@me`.
    /// Called by the orchestrator on `IpcEvent::ViewerIdentities`.
    pub fn set_viewer_logins(&mut self, logins: std::collections::HashMap<String, String>) {
        self.inner.set_viewer_logins(logins);
    }

    /// Drain queued IPC commands.
    pub fn drain_cmds(&mut self) -> Vec<IpcCommand> {
        std::mem::take(&mut self.pending_cmds)
    }

    /// Drive the auto-mark-on-hover timer; the orchestrator calls
    /// this each tick. Returns the `(SessionKey, index)` to mark read
    /// when the timer fires, otherwise None.
    pub fn tick(&mut self) -> Option<(pilot_core::SessionKey, usize)> {
        self.inner.tick(self.focused)
    }

    /// Forward a daemon event so the inner pane can refresh.
    pub fn on_daemon_event(&mut self, evt: &IpcEvent) {
        self.inner.on_event(evt);
    }

    /// Direct render entry point. See `Sidebar::view_in`.
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

    /// State-aware short list for the footer hint bar. Catalog-driven;
    /// `overrides` carries the user's `ui.action_keys` map (empty when
    /// untouched) and flows into the catalog's `effective_keys_display`.
    pub fn contextual_bindings(
        &self,
        overrides: &std::collections::BTreeMap<String, String>,
    ) -> Vec<crate::pane::Binding> {
        self.inner.contextual_bindings(overrides)
    }

    /// Apply resolved `UiDefaults` (auto-mark delay, task body cap,
    /// etc.) to the inner pane. Called once at startup from the
    /// model's `apply_sidebar_config`.
    pub fn apply_ui_defaults(&mut self, ui: &pilot_config::UiDefaults) {
        self.inner.apply_ui_defaults(ui);
    }

    /// Forward to the inner pane's detach spec, if any.
    pub fn detachable(&self) -> Option<crate::pane::DetachSpec> {
        self.inner.detachable()
    }

    /// Forward a mouse click to the inner pane. Returns `true` when
    /// the click landed on a known target (section header or activity
    /// card) and the caller should redraw.
    pub fn handle_mouse_click(&mut self, col: u16, row: u16) -> bool {
        self.inner.handle_mouse_click(col, row)
    }

    /// Forward a double-click — toggles expand/collapse on the card
    /// under the cursor. Section-header double-clicks are no-ops;
    /// single-click already toggles those.
    pub fn handle_mouse_double_click(&mut self, col: u16, row: u16) -> bool {
        self.inner.handle_mouse_double_click(col, row)
    }

    /// Wheel scroll routed to the activity feed. `delta` is in rows
    /// (negative = up, positive = down). Returns `true` if the scroll
    /// moved so the caller redraws.
    pub fn scroll_activity(&mut self, delta: isize) -> bool {
        self.inner.scroll_activity(delta)
    }

    /// Drain the queued click-to-select notice, if any. Forwarded
    /// to the footer by the orchestrator's mouse-up handler.
    pub fn drain_selection_notice(&mut self) -> Option<String> {
        self.inner.drain_selection_notice()
    }

    /// Indices the user explicitly multi-selected. Used by `m`
    /// dispatch to mark only those instead of the whole workspace.
    pub fn selected_activity_indices(&self) -> Vec<usize> {
        self.inner.selected_activity_indices()
    }
}

impl Component for Right {
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

impl AppComponent<Msg, UserEvent> for Right {
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
                    return Some(Msg::RightCmds);
                }
                None
            }
            _ => None,
        }
    }
}
