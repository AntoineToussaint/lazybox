//! `Sidebar` — tuirealm wrapper around lazybox's existing
//! `crate::components::sidebar::Sidebar`.
//!
//! Lazybox's sidebar is ~1.4k LOC of bespoke render logic (workspace
//! rows with role badges, status pills, runner badges, mailbox
//! cycling, time column, …). Rather than copying it, this wrapper
//! holds an instance and delegates `view` + `on` through to the
//! existing `Pane` impl via UFCS.
//!
//! ## Why this is the right shape during the migration
//!
//! The end-state lifts lazybox's `impl tui_kit::Pane for Sidebar` body
//! into inherent methods (or a free `Sidebar::handle_key` /
//! `::render` / `::on_event`). That conversion is a one-shot
//! mechanical edit we can do once the kit is deleted. Until then,
//! UFCS keeps both code paths alive.

use crate::PaneId;
use crate::components::sidebar::Sidebar as LazyboxSidebar;
use crate::realm::keymap::realm_key_to_crossterm;
use crate::realm::{Msg, UserEvent};
use lazybox_ipc::Command as IpcCommand;
use lazybox_ipc::Event as IpcEvent;
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::Event;
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::state::State;

/// Wrap lazybox's existing Sidebar so it can be mounted into a
/// tuirealm `Application`.
pub struct Sidebar {
    inner: LazyboxSidebar,
    /// Whether this pane is the focused one. tuirealm sets it via
    /// the `Attribute::Focus` flag.
    focused: bool,
    /// Outbound commands queued by `handle_key`. We drain them in the
    /// `Model::update` arm for `Msg::SidebarCmds(...)` and forward
    /// them to the daemon.
    pending_cmds: Vec<IpcCommand>,
}

impl Sidebar {
    /// Construct using the same `PaneId` the existing lazybox sidebar
    /// uses, so detach specs + helper lookups continue to match.
    pub fn new(id: PaneId) -> Self {
        Self {
            inner: LazyboxSidebar::new(id),
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
    /// drove an `AgentState::InputNeeded` event would spam the user's
    /// notification center. Keeping the IO side-effect in this
    /// wrapper (which production code goes through; tests don't)
    /// keeps the inner sidebar fully deterministic.
    pub fn on_daemon_event(&mut self, evt: &IpcEvent) {
        self.inner.on_event(evt);
        for notif in self.inner.drain_pending_notifications() {
            crate::platform::notify_user(&notif.title, &notif.body);
        }
    }

    /// Forward `displays_agent_state` — true when the inner sidebar
    /// already shows `state` for this session, so a repeated
    /// `AgentState` ping needs no redraw.
    pub fn displays_agent_state(
        &self,
        session_key: &lazybox_core::SessionKey,
        state: lazybox_ipc::AgentState,
    ) -> bool {
        self.inner.displays_agent_state(session_key, state)
    }

    /// Drain footer-notice strings the inner sidebar queued in
    /// response to AgentState transitions. Returns one short string
    /// per Active→Asking edge, suitable for `Notice` rendering. The
    /// OS notification path (above) fires in parallel; this one
    /// surfaces the same signal inside lazybox's footer for users who
    /// have notifications muted.
    pub fn drain_pending_asking_notices(&mut self) -> Vec<String> {
        self.inner.drain_pending_asking_notices()
    }

    /// Whether any visible workspace has an agent waiting on input —
    /// drives the agent-waiting feature tip (#115).
    pub fn has_asking_agent(&self) -> bool {
        self.inner.has_asking_agent()
    }

    /// Whether any visible workspace's PR has failing / mixed CI —
    /// drives the failing-CI feature tip (#115).
    pub fn has_failing_ci(&self) -> bool {
        self.inner.has_failing_ci()
    }

    /// Advance the "working" spinner on a low-rate tick. Returns
    /// `true` when the glyph changed so the run loop can mark the
    /// frame dirty. Delegates to the inner lazybox sidebar.
    pub fn tick_working(&mut self) -> bool {
        self.inner.tick_working()
    }

    /// Drain `g m` "Merge PR #N?" requests. The orchestrator mounts
    /// a Confirm modal per entry.

    /// Optimistic local update: mark the workspace's PR as `Merged`
    /// so the status pill flips immediately on `Event::PrMerged`,
    /// without waiting for the next poll cycle.
    pub fn mark_workspace_merged(&mut self, key: &lazybox_core::WorkspaceKey) {
        self.inner.mark_workspace_merged(key);
    }

    /// Forward `find_agent_terminal` — first running agent terminal
    /// for `(workspace_key, agent_id)` if any. The `w` flow uses
    /// this to decide between InjectPrompt (existing) and Spawn (new).
    pub fn find_agent_terminal(
        &self,
        workspace_key: &lazybox_core::SessionKey,
        agent_id: &str,
    ) -> Option<lazybox_ipc::TerminalId> {
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

    /// Record how many commits this build trails `main` so the header
    /// paints the persistent outdated-build warning (#234).
    pub fn set_outdated_build(&mut self, commits_behind: Option<u32>) {
        self.inner.set_outdated_build(commits_behind);
    }

    pub fn outdated_commits_behind(&self) -> Option<u32> {
        self.inner.outdated_commits_behind()
    }

    /// True while the `/` search bar is capturing keystrokes. The
    /// orchestrator checks this before its normal key routing so a
    /// query can swallow keys that would otherwise fire shortcuts.
    pub fn search_editing(&self) -> bool {
        self.inner.search_editing()
    }

    /// Feed one keystroke into the open search bar. See
    /// `Sidebar::handle_search_key`.
    pub fn handle_search_key(&mut self, key: crossterm::event::KeyEvent) {
        self.inner.handle_search_key(key);
    }

    /// Read currently selected workspace key (for selection projection).
    pub fn selected_workspace_key(&self) -> Option<&lazybox_core::SessionKey> {
        self.inner.selected_session_key()
    }

    /// Currently configured default agent (drives `w` work-on-this
    /// spawn). Used by the orchestrator's `dispatch_action` so the
    /// catalog path makes the same `resolve_work` call as the
    /// sidebar's inline handler.
    pub fn default_agent(&self) -> &str {
        self.inner.default_agent()
    }

    /// Agent `w` should target on `workspace_key`: an agent already
    /// running there (so `w` injects into it) wins over `default_agent`.
    /// See [`crate::components::sidebar::Sidebar::work_target_agent`].
    pub fn work_target_agent(
        &self,
        workspace_key: &lazybox_core::SessionKey,
        default_agent: &str,
    ) -> String {
        self.inner.work_target_agent(workspace_key, default_agent)
    }

    /// Selected session id, if the cursor is on a session sub-row of
    /// a workspace. `None` when the cursor is on a top-level
    /// workspace row OR when the workspace has no sessions yet.
    /// Used by `dispatch_action` to honor "spawn into this specific
    /// session" semantics when the user has a session focused.
    pub fn selected_session_id(&self) -> Option<lazybox_core::SessionId> {
        self.inner.selected_session_id()
    }

    /// Read the full Workspace under the cursor (for projection into
    /// `Right::set_workspace`).
    pub fn selected_workspace(&self) -> Option<&lazybox_core::Workspace> {
        self.inner.selected_workspace()
    }

    /// Look up a workspace by key (independent of cursor).
    pub fn workspace_by_key(
        &self,
        key: &lazybox_core::SessionKey,
    ) -> Option<&lazybox_core::Workspace> {
        self.inner.workspace_by_key(key)
    }

    /// Iterate every known workspace. The adopt-sessions picker uses
    /// this to build its candidate list.
    pub fn workspace_iter(
        &self,
    ) -> impl Iterator<Item = (&lazybox_core::SessionKey, &lazybox_core::Workspace)> {
        self.inner.workspace_iter()
    }

    /// Every known Project as `(key, display name)`. Backs the global
    /// "start agent" (`Shift-W`) project picker.
    pub fn projects_for_picker(&self) -> Vec<(lazybox_core::ProjectKey, String)> {
        self.inner.projects_for_picker()
    }

    /// Apply `~/.lazybox/config.yaml` overrides to the inner pane in
    /// place. Used by `Model::apply_sidebar_config` once at startup.
    pub fn apply_inner_config(
        &mut self,
        attention: lazybox_config::AttentionConfig,
        collapsed_repos: std::collections::BTreeSet<String>,
        default_agent: Option<String>,
        display: &lazybox_config::DisplayConfig,
    ) {
        self.inner
            .apply_config(attention, collapsed_repos, default_agent, display);
    }

    /// Replace the set of subscribed-repo names that should show up
    /// as headers even before polling finds anything under them.
    /// See `Sidebar::apply_projects`.
    pub fn apply_projects(
        &mut self,
        projects: std::collections::BTreeMap<lazybox_core::ProjectKey, lazybox_core::Project>,
    ) {
        self.inner.apply_projects(projects);
    }

    /// See `Sidebar::focused_project_key`.
    pub fn focused_project_key(&self) -> Option<lazybox_core::ProjectKey> {
        self.inner.focused_project_key()
    }

    /// See `Sidebar::project_label_for`.
    pub fn project_label_for(&self, key: &lazybox_core::ProjectKey) -> Option<String> {
        self.inner.project_label_for(key)
    }

    /// See `Sidebar::workspaces_iter`.
    pub fn workspaces_iter(&self) -> impl Iterator<Item = &lazybox_core::Workspace> {
        self.inner.workspaces_iter()
    }

    /// See `Sidebar::workspaces_in_project`.
    pub fn workspaces_in_project(&self, key: &lazybox_core::ProjectKey) -> usize {
        self.inner.workspaces_in_project(key)
    }

    /// Move the cursor onto the workspace whose key matches.
    /// Returns true if found.
    pub fn focus_workspace_key(&mut self, key: &lazybox_core::SessionKey) -> bool {
        self.inner.focus_workspace_key(key)
    }

    /// Move the cursor onto the RepoHeader row for the given project.
    /// See `Sidebar::focus_project_header`.
    pub fn focus_project_header(&mut self, key: &lazybox_core::ProjectKey) -> bool {
        self.inner.focus_project_header(key)
    }

    /// Move the cursor onto the named session sub-row. Caller is
    /// expected to have first selected the parent workspace.
    pub fn focus_session_id(&mut self, id: lazybox_core::SessionId) -> bool {
        self.inner.focus_session_id(id)
    }

    /// Move the cursor onto the next workspace whose agent is in
    /// `Asking` state, wrapping around. Returns true when a target
    /// was found. Backs the `!` global key.
    pub fn focus_next_asking_workspace(&mut self) -> bool {
        self.inner.focus_next_asking_workspace()
    }

    /// Move the cursor onto the next workspace whose PR has failing
    /// CI, wrapping around. Returns true when a target was found.
    /// Backs the `Shift-F` global key.
    pub fn focus_next_failing_ci_workspace(&mut self) -> bool {
        self.inner.focus_next_failing_ci_workspace()
    }

    /// Move the cursor onto the `n`th (1-based) agent workspace in
    /// sidebar order. Returns true when that slot exists. Backs the
    /// `]]<digit>` focus-mode jump.
    pub fn focus_nth_agent_workspace(&mut self, n: usize) -> bool {
        self.inner.focus_nth_agent_workspace(n)
    }

    /// The visible agent workspaces in sidebar (top-down) order — the
    /// roster the `]]<digit>` jump and its badges read from.
    pub fn agent_workspace_keys(&self) -> Vec<lazybox_core::SessionKey> {
        self.inner.agent_workspace_keys()
    }

    /// At-a-glance attention tallies for the focus-mode event header.
    pub fn attention_summary(&self) -> crate::components::sidebar::AttentionSummary {
        self.inner.attention_summary()
    }

    /// Fuzzy-switcher targets: every workspace across repos as
    /// `(session key, label)`, attention-needing ones first. Backs the
    /// `JumpToWorkspace` picker.
    pub fn jump_targets(&self) -> Vec<(lazybox_core::SessionKey, String)> {
        self.inner.jump_targets()
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

    /// Click-to-select a row. Returns true on a hit.
    pub fn click_to_select(&mut self, area: Rect, click_row: u16) -> bool {
        self.inner.click_to_select(area, click_row)
    }

    /// Mouse-wheel scroll over the sidebar. Moves the viewport offset
    /// by `delta` rows; the selection cursor is untouched. Returns
    /// true when the offset moved.
    pub fn scroll_by_wheel(&mut self, delta: isize) -> bool {
        self.inner.scroll_by_wheel(delta)
    }

    /// Click the role-filter chip in the sidebar header → cycle it.
    /// Returns true on a hit (caller should mark a redraw).
    pub fn click_to_cycle_filter(&mut self, col: u16, row: u16) -> bool {
        self.inner.click_to_cycle_filter(col, row)
    }

    /// Click the sort chip in the sidebar header → cycle it.
    pub fn click_to_cycle_sort(&mut self, col: u16, row: u16) -> bool {
        self.inner.click_to_cycle_sort(col, row)
    }

    /// True iff the cursor sits on a repo header row. Used by the
    /// orchestrator's double-click handler to decide whether to
    /// fire `toggle_repo_at_cursor`.
    pub fn cursor_on_repo_header(&self) -> bool {
        self.inner.cursor_on_repo_header()
    }

    /// Index of the cursor row within the visible list. Observability
    /// passthrough mirroring `Sidebar::cursor` — used by tests to map a
    /// workspace onto the screen row a click would land on.
    pub fn cursor(&self) -> usize {
        self.inner.cursor()
    }

    /// Test accessor — the row-window scroll offset from the last render.
    #[doc(hidden)]
    pub fn __test_scroll(&self) -> usize {
        self.inner.__test_scroll()
    }

    /// Toggle the repo header under the cursor. Same effect as the
    /// `Space` key on a header.
    pub fn toggle_repo_at_cursor(&mut self) -> bool {
        self.inner.toggle_repo_at_cursor()
    }

    /// Cycle the role filter (catalog `CycleRoleFilter`, default `f`).
    pub fn cycle_role_filter(&mut self) {
        self.inner.cycle_role_filter();
    }

    /// Cycle the sort order (catalog `CycleSort`, default `o`).
    pub fn cycle_sort(&mut self) {
        self.inner.cycle_sort_mode();
    }

    /// Cycle the mailbox view (catalog `CycleMailbox`, default `Shift-S`).
    pub fn cycle_mailbox(&mut self) {
        self.inner.cycle_mailbox();
    }

    /// Open the incremental search bar (catalog `OpenSearch`, default `/`).
    pub fn open_search(&mut self) {
        self.inner.open_search();
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
            // Daemon events route through the inner lazybox sidebar's
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
