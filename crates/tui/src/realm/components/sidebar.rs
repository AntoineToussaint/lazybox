//! `Sidebar` — tuirealm wrapper around pilot's existing
//! `crate::components::sidebar::Sidebar`.
//!
//! Pilot's sidebar is ~1.4k LOC of bespoke render logic (workspace
//! rows with role badges, status pills, runner badges, mailbox
//! cycling, time column, …). Rather than copying it, this wrapper
//! holds an instance and delegates `view` + `on` through to the
//! existing `Pane` impl via UFCS.
//!
//! ## Why this is the right shape during the migration
//!
//! The end-state lifts pilot's `impl tui_kit::Pane for Sidebar` body
//! into inherent methods (or a free `Sidebar::handle_key` /
//! `::render` / `::on_event`). That conversion is a one-shot
//! mechanical edit we can do once the kit is deleted. Until then,
//! UFCS keeps both code paths alive.

use crate::PaneId;
use crate::components::sidebar::Sidebar as PilotSidebar;
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

/// Wrap pilot's existing Sidebar so it can be mounted into a
/// tuirealm `Application`.
pub struct Sidebar {
    inner: PilotSidebar,
    /// Whether this pane is the focused one. tuirealm sets it via
    /// the `Attribute::Focus` flag.
    focused: bool,
    /// Outbound commands queued by `handle_key`. We drain them in the
    /// `Model::update` arm for `Msg::SidebarCmds(...)` and forward
    /// them to the daemon.
    pending_cmds: Vec<IpcCommand>,
}

impl Sidebar {
    /// Construct using the same `PaneId` the existing pilot sidebar
    /// uses, so detach specs + helper lookups continue to match.
    pub fn new(id: PaneId) -> Self {
        Self {
            inner: PilotSidebar::new(id),
            focused: true, // sidebar is the default-focused pane
            pending_cmds: Vec::new(),
        }
    }

    /// Drain any commands the inner sidebar pushed in response to a
    /// recent `handle_key`. Caller forwards to the daemon.
    pub fn drain_cmds(&mut self) -> Vec<IpcCommand> {
        std::mem::take(&mut self.pending_cmds)
    }

    /// Forward an incoming daemon event to the inner sidebar so its
    /// workspace map / live-terminal tracking stays in sync. After
    /// delegating, drain any desktop notifications the inner sidebar
    /// queued in response (currently: agent → Asking transitions)
    /// and fire them via the OS-aware `platform::notify_user`.
    ///
    /// **Why drain here and not inside the inner sidebar?** The
    /// inner sidebar is constructed directly in unit tests (`cargo
    /// test`) — if it called `osascript` itself, every test that
    /// drove an `AgentState::Asking` event would spam the user's
    /// notification center. Keeping the IO side-effect in this
    /// wrapper (which production code goes through; tests don't)
    /// keeps the inner sidebar fully deterministic.
    pub fn on_daemon_event(&mut self, evt: &IpcEvent) {
        self.inner.on_event(evt);
        for notif in self.inner.drain_pending_notifications() {
            crate::platform::notify_user(&notif.title, &notif.body);
        }
    }

    /// Drain footer-notice strings the inner sidebar queued in
    /// response to AgentState transitions. Returns one short string
    /// per Active→Asking edge, suitable for `Notice` rendering. The
    /// OS notification path (above) fires in parallel; this one
    /// surfaces the same signal inside pilot's footer for users who
    /// have notifications muted.
    pub fn drain_pending_asking_notices(&mut self) -> Vec<String> {
        self.inner.drain_pending_asking_notices()
    }

    /// Drain Shift-M "Merge PR #N?" requests. The orchestrator mounts
    /// a Confirm modal per entry.

    /// Optimistic local update: mark the workspace's PR as `Merged`
    /// so the status pill flips immediately on `Event::PrMerged`,
    /// without waiting for the next poll cycle.
    pub fn mark_workspace_merged(&mut self, key: &pilot_core::WorkspaceKey) {
        self.inner.mark_workspace_merged(key);
    }

    /// Forward `find_agent_terminal` — first running agent terminal
    /// for `(workspace_key, agent_id)` if any. The `w` flow uses
    /// this to decide between InjectPrompt (existing) and Spawn (new).
    pub fn find_agent_terminal(
        &self,
        workspace_key: &pilot_core::SessionKey,
        agent_id: &str,
    ) -> Option<pilot_ipc::TerminalId> {
        self.inner.find_agent_terminal(workspace_key, agent_id)
    }

    /// Render directly into a rect — orchestrator-friendly entry
    /// point that bypasses tuirealm's mount/active dance for panes.
    pub fn view_in(&mut self, area: Rect, frame: &mut Frame) {
        self.inner.render(area, frame, self.focused);
    }

    /// Direct (non-tuirealm) key dispatch. The orchestrator calls
    /// this after Tab routing is resolved.
    pub fn handle_key_direct(
        &mut self,
        key: crossterm::event::KeyEvent,
        cmds: &mut Vec<IpcCommand>,
    ) {
        let _ = self.inner.handle_key(key, cmds);
    }

    /// Update the focused-flag (drives border / cursor styling).
    pub fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    /// Read currently selected workspace key (for selection projection).
    pub fn selected_workspace_key(&self) -> Option<&pilot_core::SessionKey> {
        self.inner.selected_session_key()
    }

    /// Currently configured default agent (drives `w` work-on-this
    /// spawn). Used by the orchestrator's `dispatch_action` so the
    /// catalog path makes the same `resolve_work` call as the
    /// sidebar's inline handler.
    pub fn default_agent(&self) -> &str {
        self.inner.default_agent()
    }

    /// Selected session id, if the cursor is on a session sub-row of
    /// a workspace. `None` when the cursor is on a top-level
    /// workspace row OR when the workspace has no sessions yet.
    /// Used by `dispatch_action` to honor "spawn into this specific
    /// session" semantics when the user has a session focused.
    pub fn selected_session_id(&self) -> Option<pilot_core::SessionId> {
        self.inner.selected_session_id()
    }

    /// Read the full Workspace under the cursor (for projection into
    /// `Right::set_workspace`).
    pub fn selected_workspace(&self) -> Option<&pilot_core::Workspace> {
        self.inner.selected_workspace()
    }

    /// Look up a workspace by key (independent of cursor).
    pub fn workspace_by_key(&self, key: &pilot_core::SessionKey) -> Option<&pilot_core::Workspace> {
        self.inner.workspace_by_key(key)
    }

    /// Iterate every known workspace. The adopt-sessions picker uses
    /// this to build its candidate list.
    pub fn workspace_iter(
        &self,
    ) -> impl Iterator<Item = (&pilot_core::SessionKey, &pilot_core::Workspace)> {
        self.inner.workspace_iter()
    }

    /// Apply `~/.pilot/config.yaml` overrides to the inner pane in
    /// place. Used by `Model::apply_sidebar_config` once at startup.
    pub fn apply_inner_config(
        &mut self,
        attention: pilot_config::AttentionConfig,
        collapsed_repos: std::collections::BTreeSet<String>,
        agent_shortcuts: std::collections::HashMap<char, String>,
        default_agent: Option<String>,
        display: &pilot_config::DisplayConfig,
        ui: &pilot_config::UiDefaults,
    ) {
        self.inner.apply_config(
            attention,
            collapsed_repos,
            agent_shortcuts,
            default_agent,
            display,
            ui,
        );
    }

    /// Replace the set of subscribed-repo names that should show up
    /// as headers even before polling finds anything under them.
    /// See `Sidebar::apply_subscribed_scopes`.
    pub fn apply_subscribed_scopes(&mut self, scopes: &std::collections::BTreeSet<String>) {
        self.inner.apply_subscribed_scopes(scopes);
    }

    /// Move the cursor onto the workspace whose key matches.
    /// Returns true if found.
    pub fn focus_workspace_key(&mut self, key: &pilot_core::SessionKey) -> bool {
        self.inner.focus_workspace_key(key)
    }

    /// Move the cursor onto the named session sub-row. Caller is
    /// expected to have first selected the parent workspace.
    pub fn focus_session_id(&mut self, id: pilot_core::SessionId) -> bool {
        self.inner.focus_session_id(id)
    }

    /// Move the cursor onto the next workspace whose agent is in
    /// `Asking` state, wrapping around. Returns true when a target
    /// was found. Backs the `!` global key.
    pub fn focus_next_asking_workspace(&mut self) -> bool {
        self.inner.focus_next_asking_workspace()
    }

    /// Forward to the inner pane's keymap so the help panel can list
    /// the same bindings the legacy hint bar showed.
    pub fn keymap(&self) -> &'static [crate::pane::Binding] {
        self.inner.keymap()
    }

    /// State-aware short list for the footer hint bar.
    pub fn contextual_bindings(&self) -> Vec<crate::pane::Binding> {
        self.inner.contextual_bindings()
    }

    /// Click-to-select a row. Returns true on a hit.
    pub fn click_to_select(&mut self, area: Rect, click_row: u16) -> bool {
        self.inner.click_to_select(area, click_row)
    }

    /// Forward to the inner pane's detach spec, if any.
    pub fn detachable(&self) -> Option<crate::pane::DetachSpec> {
        self.inner.detachable()
    }
}

impl Component for Sidebar {
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

impl AppComponent<Msg, UserEvent> for Sidebar {
    fn on(&mut self, ev: &Event<UserEvent>) -> Option<Msg> {
        match ev {
            // Daemon events route through the inner pilot sidebar's
            // `on_event` so its `workspaces` map + `running_terminals`
            // stay current.
            Event::User(UserEvent::Daemon(evt)) => {
                self.inner.on_event(evt);
                for notif in self.inner.drain_pending_notifications() {
                    crate::platform::notify_user(&notif.title, &notif.body);
                }
                None
            }
            Event::Keyboard(key) if self.focused => {
                // Translate tuirealm KeyEvent → crossterm KeyEvent so
                // we can delegate to the existing `handle_key`.
                let ct_key = realm_key_to_crossterm(key);
                let mut cmds: Vec<IpcCommand> = Vec::new();
                let outcome = self.inner.handle_key(ct_key, &mut cmds);
                if !cmds.is_empty() {
                    self.pending_cmds.extend(cmds);
                    return Some(Msg::SidebarCmds);
                }
                let _ = outcome;
                None
            }
            _ => None,
        }
    }
}
