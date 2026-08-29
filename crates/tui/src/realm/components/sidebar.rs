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
use crate::notify_coalesce::NotificationCoalescer;
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
    /// Debounces desktop-notification bursts so N agents changing state
    /// at once collapse into one summary banner instead of N popups
    /// (#1370). Fed by `on_daemon_event`, drained by
    /// `flush_due_notifications` from the run loop.
    coalescer: NotificationCoalescer,
}

impl Sidebar {
    /// Construct using the same `PaneId` the existing lazybox sidebar
    /// uses, so detach specs + helper lookups continue to match.
    pub fn new(id: PaneId) -> Self {
        Self {
            inner: LazyboxSidebar::new(id),
            focused: true, // sidebar is the default-focused pane
            pending_cmds: Vec::new(),
            coalescer: NotificationCoalescer::default(),
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
    /// keeps the inner sidebar fully deterministic. Model tests that
    /// do reach this wrapper are covered by the second gate:
    /// `notify_user` stays disarmed until the binary calls
    /// `platform::set_notifier_backend` at startup.
    pub fn on_daemon_event(&mut self, evt: &IpcEvent) {
        self.inner.on_event(evt);
        // Buffer rather than fire immediately: a state storm across many
        // agents would otherwise emit one banner per workspace. The
        // coalescer collapses a same-kind burst into a single summary
        // once its debounce window elapses (#1370).
        let now = std::time::Instant::now();
        for notif in self.inner.drain_pending_notifications() {
            self.coalescer.push(now, notif);
        }
    }

    /// Fire any coalesced desktop notifications whose debounce window
    /// has elapsed, collapsing a same-kind burst into one summary
    /// banner. Called each run-loop iteration (#1370).
    pub fn flush_due_notifications(&mut self) {
        for notif in self.coalescer.flush_due(std::time::Instant::now()) {
            crate::platform::notify_user(&notif.title, &notif.body, &notif.workspace_key);
        }
    }

    /// Bracket a daemon-event drain batch so a poll sweep of N workspace
    /// upserts rebuilds the visible list once rather than N times (#1030).
    /// Paired with [`Self::flush_recompute`], mirroring the model's
    /// per-batch `flush_pane_sync`.
    pub fn begin_recompute_batch(&mut self) {
        self.inner.begin_recompute_batch();
    }

    /// Close the batch and rebuild the visible list once if any deferred
    /// event asked for it.
    pub fn flush_recompute(&mut self) {
        self.inner.flush_recompute();
    }

    /// Test-only: number of full visible-list rebuilds performed so far —
    /// lets the drain-coalescing regression assert one rebuild per batch.
    #[cfg(test)]
    pub fn recompute_count(&self) -> usize {
        self.inner.recompute_count()
    }

    /// Test-only: number of workspace rows currently in the visible list.
    #[cfg(test)]
    pub fn visible_workspace_count(&self) -> usize {
        self.inner.visible_workspace_count()
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

    pub fn credit_exhausted_terminals(&self) -> Vec<lazybox_ipc::TerminalId> {
        self.inner.credit_exhausted_terminals()
    }

    pub fn credit_exhausted_terminals_for(
        &self,
        key: &lazybox_core::SessionKey,
    ) -> Vec<lazybox_ipc::TerminalId> {
        self.inner.credit_exhausted_terminals_for(key)
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

    /// Drop any per-row "spawning" arc a spawn stranded past the guard
    /// window (#1372). Returns `true` when it cleared one so the run loop
    /// can redraw.
    pub fn prune_stale_spawning(&mut self) -> bool {
        self.inner.prune_stale_spawning()
    }

    /// Cancel one workspace's "spawning" arc — the `Esc` escape from a
    /// stuck spinner (#1372). Returns `true` when there was one to clear.
    pub fn clear_spawning(&mut self, session_key: &lazybox_core::SessionKey) -> bool {
        self.inner.clear_spawning(session_key)
    }

    /// Drain `g m` "Merge PR #N?" requests. The orchestrator mounts
    /// a Confirm modal per entry.

    /// Optimistic local update: mark the workspace's PR as `Merged`
    /// so the status pill flips immediately on `Event::PrMerged`,
    /// without waiting for the next poll cycle.
    pub fn mark_workspace_merged(&mut self, key: &lazybox_core::WorkspaceKey) {
        self.inner.mark_workspace_merged(key);
    }

    /// Optimistic local update: tag a workspace's row as running on the
    /// remote box (the `sandbox:` box) so the sidebar's `⇅` indicator shows
    /// immediately after an `r`-prefix spawn, before the local snapshot syncs.
    pub fn mark_remote(&mut self, sk: lazybox_core::SessionKey, remote: String) {
        self.inner.mark_remote(sk, remote);
    }

    /// Roll back [`Self::mark_remote`] when the advertised spawn dropped.
    pub fn unmark_remote(&mut self, sk: &lazybox_core::SessionKey) {
        self.inner.unmark_remote(sk);
    }

    /// Optimistic local update: flip a workspace's merge-on-green arm so the
    /// `⚡` row glyph lands the instant `g g` is pressed, before the daemon
    /// persists the flag and rebroadcasts the workspace. Returns whether a
    /// workspace was found to update.
    pub fn mark_auto_merge_on_green(
        &mut self,
        sk: &lazybox_core::SessionKey,
        enabled: bool,
    ) -> bool {
        self.inner.mark_auto_merge_on_green(sk, enabled)
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

    /// Record whether `ui.keep_awake` is on so the header can badge
    /// active sleep inhibition.
    pub fn set_keep_awake(&mut self, keep_awake: bool) {
        self.inner.set_keep_awake(keep_awake);
    }

    /// Record whether `ui.auto_wait_on_limit` is on so the rising-edge
    /// rate-limit alert stays quiet for a block the daemon auto-handles.
    pub fn set_auto_wait_on_limit(&mut self, auto_wait_on_limit: bool) {
        self.inner.set_auto_wait_on_limit(auto_wait_on_limit);
    }

    /// Record whether `ui.show_agent_model` is on — gates the per-agent
    /// model + effort label beside each runner badge.
    pub fn set_show_agent_model(&mut self, show: bool) {
        self.inner.set_show_agent_model(show);
    }

    /// Replace the tier `(badge_letter, label) → short` map that
    /// abbreviates the model badge (`◆O`, #1068).
    pub fn set_model_shorts(&mut self, shorts: std::collections::HashMap<(char, String), String>) {
        self.inner.set_model_shorts(shorts);
    }

    /// Record whether `ui.usage_summary` is on — gates the always-visible
    /// per-provider usage row in the header (#1059).
    pub fn set_usage_summary(&mut self, show: bool) {
        self.inner.set_usage_summary(show);
    }

    /// Load the per-agent plan-window token budgets (`ui.usage_budgets`).
    pub fn set_usage_budgets(&mut self, budgets: std::collections::BTreeMap<String, u64>) {
        self.inner.set_usage_budgets(budgets);
    }

    /// Record whether `ui.today_summary` is on — gates the always-visible
    /// "today" stats strip in the header (#1344).
    pub fn set_today_summary(&mut self, show: bool) {
        self.inner.set_today_summary(show);
    }

    /// Install the latest daily rollup (`Event::Stats`) the header strip
    /// re-sums today's slice from (#1344).
    pub fn set_today_buckets(&mut self, buckets: Vec<lazybox_ipc::StatBucket>) {
        self.inner.set_today_buckets(buckets);
    }

    /// Bind a structured run to its agent for usage accounting
    /// (`AgentRunStarted`).
    pub fn note_agent_run(&mut self, run_id: lazybox_ipc::AgentRunId, agent_id: &str) {
        self.inner.note_agent_run(run_id, agent_id);
    }

    /// Observe one usage report for a run's in-flight turn (`AgentUsage`).
    pub fn add_agent_usage(
        &mut self,
        run_id: lazybox_ipc::AgentRunId,
        usage: &lazybox_ipc::AgentUsage,
    ) {
        self.inner.add_agent_usage(run_id, usage);
    }

    /// Commit a completed turn's usage into the running total
    /// (`AgentTurnFinished`).
    pub fn commit_agent_turn(&mut self, run_id: lazybox_ipc::AgentRunId) {
        self.inner.commit_agent_turn(run_id);
    }

    /// Drop a finished run's usage binding (`AgentRunFinished`).
    pub fn finish_agent_run(&mut self, run_id: lazybox_ipc::AgentRunId) {
        self.inner.finish_agent_run(run_id);
    }

    /// Observe proxy-metered usage attributed straight to an agent
    /// (`AgentSessionUsage`), with the workspace/session it belongs to when
    /// the proxy path carried one (#per-session cost).
    pub fn add_agent_session_usage(
        &mut self,
        agent_id: &str,
        session_key: Option<&lazybox_core::SessionKey>,
        usage: &lazybox_ipc::AgentUsage,
    ) {
        self.inner
            .add_agent_session_usage(agent_id, session_key, usage);
    }

    /// Record a provider plan-quota report (`AgentProviderQuota`) — the
    /// 5h/weekly "can I keep working?" headroom.
    pub fn note_provider_quota(
        &mut self,
        agent_id: &str,
        session_key: Option<&lazybox_core::SessionKey>,
        quota: lazybox_ipc::ProviderQuota,
    ) {
        self.inner.note_provider_quota(agent_id, session_key, quota);
    }

    /// Attribute a usage-limit reset hint to a terminal's agent
    /// (`AgentUsageLimit`).
    pub fn note_usage_limit_reset(&mut self, terminal_id: lazybox_ipc::TerminalId, hint: String) {
        self.inner.note_usage_limit_reset(terminal_id, hint);
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

    /// True when `(col, row)` lands on the bottom `/` search input bar.
    /// See `Sidebar::search_bar_hit`.
    pub fn search_bar_hit(&self, col: u16, row: u16) -> bool {
        self.inner.search_bar_hit(col, row)
    }

    /// Dismiss the search from a click outside the input. See
    /// `Sidebar::dismiss_search`.
    pub fn dismiss_search(&mut self) {
        self.inner.dismiss_search();
    }

    /// The active search state, if any. See `Sidebar::search`.
    pub fn search(&self) -> Option<&crate::components::sidebar::SearchState> {
        self.inner.search()
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

    /// Live-update the default agent (Settings → "Change default
    /// agent"). Delegates to the inner pane.
    pub fn set_default_agent(&mut self, agent: impl Into<String>) {
        self.inner.set_default_agent(agent);
    }

    /// The commit/PR conventions the `w` work brief injects. Delegates
    /// to the inner pane.
    pub fn conventions(&self) -> &lazybox_core::Conventions {
        self.inner.conventions()
    }

    /// Wire the YAML-configured `conventions:` block at startup.
    /// Delegates to the inner pane.
    pub fn set_conventions(&mut self, conventions: lazybox_core::Conventions) {
        self.inner.set_conventions(conventions);
    }

    /// Conversation `w` should target on `workspace_key`: one running
    /// conversation wins over `default_agent`; several ask the user
    /// which exact terminal should receive the prompt (#418).
    /// See [`crate::components::sidebar::Sidebar::work_target`].
    pub fn work_target(
        &self,
        workspace_key: &lazybox_core::SessionKey,
        default_agent: &str,
    ) -> crate::components::sidebar::WorkTarget {
        self.inner.work_target(workspace_key, default_agent)
    }

    /// Scoped `w <agent>` target resolution. Multiple terminals using
    /// the requested agent still require a chooser.
    pub fn work_target_for_agent(
        &self,
        workspace_key: &lazybox_core::SessionKey,
        agent_id: &str,
    ) -> crate::components::sidebar::WorkTarget {
        self.inner.work_target_for_agent(workspace_key, agent_id)
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

    /// See `Sidebar::agent_terminal_for` (#1204).
    pub fn agent_terminal_for(
        &self,
        key: &lazybox_core::SessionKey,
    ) -> Option<(lazybox_ipc::TerminalId, String)> {
        self.inner.agent_terminal_for(key)
    }

    /// See `Sidebar::toggle_broadcast_select`.
    pub fn toggle_broadcast_select(&mut self) -> Option<bool> {
        self.inner.toggle_broadcast_select()
    }

    /// See `Sidebar::extend_selection`.
    pub fn extend_selection(&mut self, dir: isize) -> usize {
        self.inner.extend_selection(dir)
    }

    /// See `Sidebar::extend_selection_to`.
    pub fn extend_selection_to(&mut self, area: Rect, click_row: u16) -> bool {
        self.inner.extend_selection_to(area, click_row)
    }

    /// See `Sidebar::selected_broadcast_keys`.
    pub fn selected_broadcast_keys(&self) -> Vec<lazybox_core::SessionKey> {
        self.inner.selected_broadcast_keys()
    }

    /// See `Sidebar::broadcast_selected_count`.
    pub fn broadcast_selected_count(&self) -> usize {
        self.inner.broadcast_selected_count()
    }

    /// See `Sidebar::visible_broadcast_selected_count`.
    pub fn visible_broadcast_selected_count(&self) -> usize {
        self.inner.visible_broadcast_selected_count()
    }

    /// See `Sidebar::clear_broadcast_selection`.
    pub fn clear_broadcast_selection(&mut self) -> bool {
        self.inner.clear_broadcast_selection()
    }

    /// See `Sidebar::broadcast_terminal`.
    pub fn broadcast_terminal(
        &self,
        key: &lazybox_core::SessionKey,
    ) -> Option<(lazybox_ipc::TerminalId, bool)> {
        self.inner.broadcast_terminal(key)
    }

    /// Look up a workspace by key (independent of cursor).
    pub fn workspace_by_key(
        &self,
        key: &lazybox_core::SessionKey,
    ) -> Option<&lazybox_core::Workspace> {
        self.inner.workspace_by_key(key)
    }

    /// See `Sidebar::stack_info` — the workspace's stacked-PR position
    /// (issue #969), read by the merge dispatch and the right pane.
    pub fn stack_info(
        &self,
        key: &lazybox_core::SessionKey,
    ) -> Option<&lazybox_core::StackPosition> {
        self.inner.stack_info(key)
    }

    /// See `Sidebar::take_workspace` — optimistic archive/delete (#476).
    pub fn take_workspace(
        &mut self,
        key: &lazybox_core::SessionKey,
    ) -> Option<lazybox_core::Workspace> {
        self.inner.take_workspace(key)
    }

    /// See `Sidebar::restore_workspace` — optimistic rollback (#476).
    pub fn restore_workspace(&mut self, workspace: lazybox_core::Workspace) {
        self.inner.restore_workspace(workspace);
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

    /// Every tracked GitHub repo as `owner/repo`. Backs the unmapped-
    /// Linear-team repo picker (#1041).
    pub fn github_repos_for_picker(&self) -> Vec<String> {
        self.inner.github_repos_for_picker()
    }

    /// See `Sidebar::github_repos_ranked_for_linear_team`.
    pub fn github_repos_ranked_for_linear_team(&self, team: &str) -> Vec<String> {
        self.inner.github_repos_ranked_for_linear_team(team)
    }

    /// Apply `~/.lazybox/config.yaml` overrides to the inner pane in
    /// place. Used by `Model::apply_sidebar_config` once at startup.
    #[allow(clippy::too_many_arguments)]
    pub fn apply_inner_config(
        &mut self,
        attention: lazybox_config::AttentionConfig,
        collapsed_repos: std::collections::BTreeSet<String>,
        pinned_repos: Vec<String>,
        focused_workspaces: Vec<lazybox_core::SessionKey>,
        spaces: Vec<lazybox_config::SpaceConfig>,
        collapsed_spaces: std::collections::BTreeSet<String>,
        metered_spaces: std::collections::BTreeSet<String>,
        default_agent: Option<String>,
        display: &lazybox_config::DisplayConfig,
    ) {
        self.inner.apply_config(
            attention,
            collapsed_repos,
            pinned_repos,
            focused_workspaces,
            spaces,
            collapsed_spaces,
            metered_spaces,
            default_agent,
            display,
        );
    }

    /// See `Sidebar::seed_lens` — the persisted `ui.last_lens`
    /// applied at startup by `Model::apply_client_config` (#scale).
    pub fn seed_lens(&mut self, lens: &lazybox_config::LensSection) {
        self.inner.seed_lens(lens);
    }

    /// See `Sidebar::apply_lens` — apply + persist a saved view's
    /// lens (#scale).
    pub fn apply_lens(&mut self, lens: &lazybox_config::LensSection) {
        self.inner.apply_lens(lens);
    }

    /// See `Sidebar::current_lens` — the active lens as config tokens,
    /// frozen by the save-view flow (#scale).
    pub fn current_lens(&self) -> lazybox_config::LensSection {
        self.inner.current_lens()
    }

    /// See `Sidebar::seed_source_attention` — the persisted
    /// `ui.source_attention` ladder applied at startup (#scale).
    pub fn seed_source_attention(
        &mut self,
        map: std::collections::BTreeMap<String, lazybox_config::SourceAttention>,
    ) {
        self.inner.seed_source_attention(map);
    }

    /// See `Sidebar::source_attention_for` (#scale).
    pub fn source_attention_for(&self, label: &str) -> lazybox_config::SourceAttention {
        self.inner.source_attention_for(label)
    }

    /// See `Sidebar::set_source_attention` — apply + persist one
    /// source's ladder entry (#scale).
    pub fn set_source_attention(&mut self, key: &str, entry: lazybox_config::SourceAttention) {
        self.inner.set_source_attention(key, entry);
    }

    /// See `Sidebar::set_snapshot_prune` — disabled by
    /// `Model::with_remote` so an attach client never prunes local stars
    /// against another machine's workspace set (#1244).
    pub fn set_snapshot_prune(&mut self, enabled: bool) {
        self.inner.set_snapshot_prune(enabled);
    }

    /// See `Sidebar::is_focused`. Observability passthrough (the row's
    /// `★` marker) used by the boot-path config tests.
    pub fn is_focused(&self, key: &lazybox_core::SessionKey) -> bool {
        self.inner.is_focused(key)
    }

    /// See `Sidebar::is_repo_pinned`. Observability passthrough (the
    /// header's pin marker) used by the boot-path config tests.
    pub fn is_repo_pinned(&self, name: &str) -> bool {
        self.inner.is_repo_pinned(name)
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

    /// Reveal and select a workspace even when the current sidebar view
    /// hides it.
    pub fn reveal_workspace_key(&mut self, key: &lazybox_core::SessionKey) -> bool {
        self.inner.reveal_workspace_key(key)
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

    /// Move the cursor onto the next workspace whose agent is blocked on
    /// a usage / rate limit, wrapping around. Returns true when a target
    /// was found. Backs the `Shift-L` global key (#847).
    pub fn focus_next_limit_reached_workspace(&mut self) -> bool {
        self.inner.focus_next_limit_reached_workspace()
    }

    /// See `Sidebar::limit_reached_terminals`.
    pub fn limit_reached_terminals(&self) -> Vec<lazybox_ipc::TerminalId> {
        self.inner.limit_reached_terminals()
    }

    /// See `Sidebar::limit_reached_workspace_count`.
    pub fn limit_reached_workspace_count(&self) -> usize {
        self.inner.limit_reached_workspace_count()
    }

    /// Move the cursor onto the `n`th (1-based) numbered (focused)
    /// workspace in sidebar order. Returns true when that slot exists.
    /// Backs the `]]<digit>` focus-mode jump.
    pub fn focus_nth_numbered_workspace(&mut self, n: usize) -> bool {
        self.inner.focus_nth_numbered_workspace(n)
    }

    /// The visible agent workspaces in sidebar (top-down) order.
    pub fn agent_workspace_keys(&self) -> Vec<lazybox_core::SessionKey> {
        self.inner.agent_workspace_keys()
    }

    /// The numbered (focused) workspaces in sidebar order — the roster
    /// the `]]<digit>` jump and its badges read from.
    pub fn numbered_workspace_keys(&self) -> Vec<lazybox_core::SessionKey> {
        self.inner.numbered_workspace_keys()
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
    /// `catalog` is the model's runtime catalog (`ui.action_keys`
    /// overrides + generated per-agent rows already applied), so
    /// leader-group cells and remapped keys both come out resolved.
    pub fn contextual_bindings(
        &self,
        catalog: &[lazybox_tui_core::action::CatalogEntry],
        remote: bool,
    ) -> Vec<crate::pane::Binding> {
        self.inner.contextual_bindings(catalog, remote)
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

    /// Re-anchor a wheel-detached viewport to the cursor. Returns
    /// true when the viewport was detached (caller should repaint).
    pub fn reanchor_viewport(&mut self) -> bool {
        self.inner.reanchor_viewport()
    }

    /// True when a click at `(col, row)` lands on the header filter
    /// chip. The orchestrator opens the filter menu on a hit — the
    /// menu is a modal it owns, so this is a pure hit test.
    pub fn filter_chip_hit(&self, col: u16, row: u16) -> bool {
        self.inner.filter_chip_hit(col, row)
    }

    /// Click the sort chip in the sidebar header → cycle it.
    pub fn click_to_cycle_sort(&mut self, col: u16, row: u16) -> bool {
        self.inner.click_to_cycle_sort(col, row)
    }

    /// True when a click at `(col, row)` lands on the header search
    /// box. The orchestrator opens the global search on a hit.
    pub fn search_chip_hit(&self, col: u16, row: u16) -> bool {
        self.inner.search_chip_hit(col, row)
    }

    /// True iff the cursor sits on a repo header row. Used by the
    /// orchestrator's double-click handler to decide whether to
    /// fire `toggle_repo_at_cursor`.
    pub fn cursor_on_repo_header(&self) -> bool {
        self.inner.cursor_on_repo_header()
    }

    /// See `Sidebar::cursor_space` — the Space header at/above the
    /// cursor. Used by the header-scoped source-attention actions
    /// (#scale) together with `cursor_on_space_header`.
    pub fn cursor_space(&self) -> Option<String> {
        self.inner.cursor_space()
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

    /// Toggle descendants when the cursor is on a parent ticket.
    pub fn toggle_ticket_at_cursor(&mut self) -> bool {
        self.inner.toggle_ticket_at_cursor()
    }

    /// Pin / unpin the cursor's repo group. Same effect as the `p`
    /// key. Returns `(repo, now_pinned)` for the footer notice.
    pub fn toggle_pin_at_cursor(&mut self) -> Option<(String, bool)> {
        self.inner.toggle_pin_at_cursor()
    }

    /// Star / unstar the cursor's workspace. Same effect as the `*`
    /// key. Returns `(label, now_focused)` for the footer notice.
    pub fn toggle_focus_at_cursor(&mut self) -> Option<(String, bool)> {
        self.inner.toggle_focus_at_cursor()
    }

    /// The repo group (source label) at or above the cursor, if any.
    pub fn cursor_repo(&self) -> Option<String> {
        self.inner.cursor_repo()
    }

    /// True iff the cursor sits on a Space header row (#860). Drives the
    /// tier-aware branch of the `Space` collapse + double-click.
    pub fn cursor_on_space_header(&self) -> bool {
        self.inner.cursor_on_space_header()
    }

    /// Toggle the Space header under the cursor. Same effect as `Space`
    /// on a Space header row (#860).
    pub fn toggle_space_at_cursor(&mut self) -> bool {
        self.inner.toggle_space_at_cursor()
    }

    /// Toggle Space-tier metering for the Space under the cursor (`x $`,
    /// approach C). Returns `(space_name, now_metered)`, or `None` off a
    /// Space header. Delegates to the domain `Sidebar` method of the same name.
    pub fn toggle_space_metering_at_cursor(&mut self) -> Option<(String, bool)> {
        self.inner.toggle_space_metering_at_cursor()
    }

    /// The Space a source currently resolves to — prefills the
    /// move-to-Space prompt.
    pub fn space_of_source(&self, source: &str) -> String {
        self.inner.space_of_source(source)
    }

    /// The hand-created Spaces in display order — the move-to-Space
    /// picker's rows (#1206).
    pub fn hand_created_spaces(&self) -> Vec<String> {
        self.inner.hand_created_spaces()
    }

    /// The auto-seed fallback Space for `source` — names the picker's
    /// unassign row (#1206).
    pub fn auto_space_of_source(&self, source: &str) -> String {
        self.inner.auto_space_of_source(source)
    }

    /// The exact header row under the cursor (`(is_repo, name)`) —
    /// routes right-click to the header menu (#1211).
    pub fn cursor_header(&self) -> Option<(bool, String)> {
        self.inner.cursor_header()
    }

    /// Monotonic revision of everything `sync_panes` projects (#1237).
    pub fn pane_state_rev(&self) -> u64 {
        self.inner.pane_state_rev()
    }

    /// Rename the Space at the cursor (#1211) — claims + rendered
    /// sources move, collapse flag follows, config persists.
    pub fn rename_space(&mut self, old: &str, new: &str) -> Option<(String, String)> {
        self.inner.rename_space(old, new)
    }

    /// Test-only: park the cursor on a header row by name.
    #[doc(hidden)]
    pub fn focus_header_row(&mut self, name: &str) -> bool {
        self.inner.focus_header_row(name)
    }

    /// Test-only: rendered header rows in order — `(is_repo, name)`.
    #[doc(hidden)]
    pub fn __test_header_rows(&self) -> Vec<(bool, String)> {
        self.inner.__test_header_rows()
    }

    /// Reorder the group at the cursor (#1211) — Space within the
    /// Space tier, repo within its Space. Returns `(what, name)` for
    /// the footer notice.
    pub fn move_group_at_cursor(
        &mut self,
        dir: lazybox_tui_core::inbox::MoveDir,
    ) -> Option<(&'static str, String)> {
        self.inner.move_group_at_cursor(dir)
    }

    /// Assign a source group to a Space (#860), persisting to
    /// `ui.spaces`. Returns the resolved Space name for a footer notice.
    pub fn assign_source_to_space(&mut self, source: &str, space: &str) -> String {
        self.inner.assign_source_to_space(source, space)
    }

    /// Replace the active filter set from the filter menu's picks.
    pub fn set_filters(
        &mut self,
        filters: impl IntoIterator<Item = crate::components::sidebar::Filter>,
    ) {
        self.inner.set_filters(filters);
    }

    /// Replace the active filter set from picker entries (fixed
    /// predicates plus the label / Linear-state value axes).
    pub fn set_filter_entries(
        &mut self,
        entries: impl IntoIterator<Item = crate::components::sidebar::FilterEntry>,
    ) {
        self.inner.set_filter_entries(entries);
    }

    /// The active filter set — read to pre-check the filter menu.
    pub fn filters(&self) -> &crate::components::sidebar::FilterSet {
        self.inner.filters()
    }

    /// Every `f`-menu row (fixed predicates + discovered label /
    /// Linear-state values) with its match count.
    pub fn filter_menu_entries(&self) -> Vec<(crate::components::sidebar::FilterEntry, usize)> {
        self.inner.filter_menu_entries()
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

    /// Open the global search box (catalog `OpenGlobalSearch`, default `#`).
    pub fn open_global_search(&mut self) {
        self.inner.open_global_search();
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
                self.on_daemon_event(evt);
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
