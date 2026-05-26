//! Sidebar — the left pane. Lists sessions, owns the cursor, handles
//! the core navigation and session-level keybindings.
//!
//! ## Why one component, not three
//!
//! Decomposing into FilterRow + SessionList + SessionRow components
//! is tempting, but the state is tightly coupled (cursor index depends
//! on visible order, which depends on filter/search/mailbox) and
//! splitting it opens a desync surface across multiple owners. Keeping
//! Sidebar as one component with private state is the simpler correct
//! answer. When a specific part gets independently complicated (custom
//! filter UIs per provider, say), splitting it later is localised.
//!
//! ## State the sidebar owns
//!
//! - `workspaces`: the authoritative map of SessionKey → Workspace.
//!   `SessionKey` is the wire-side selection identifier — we use the
//!   workspace's key string as its value. The daemon is the source
//!   of truth; we mirror what it sends via `Event::WorkspaceUpserted`
//!   / `WorkspaceRemoved` / `Snapshot`.
//! - `visible`: derived — `workspaces` filtered by mailbox and
//!   sorted by primary task's `updated_at` descending. Recomputed
//!   on every change so the user never sees a stale order.
//! - `cursor`: index into `visible`. Preserved by key (not index)
//!   across refreshes — the same row stays under the cursor even
//!   when another workspace gets inserted above it.
//! - `mailbox`: which view we're showing (Inbox vs Snoozed).
//! - `latches`: two-press confirm guards for `Shift-X` (kill) and
//!   `Shift-Z` (long snooze). Held as a `LatchSet<SessionKey>` so
//!   "any non-matching key disarms" is one call. See
//!   [`crate::latch_set::LatchSet`].

use crate::{PaneId, PaneOutcome};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Trigger for the long-snooze confirm latch (`Shift-Z`).
const TRIGGER_LONG_SNOOZE: crate::latch_set::KeyTrigger =
    crate::latch_set::KeyTrigger::new(KeyCode::Char('Z'), KeyModifiers::SHIFT);
use pills::visual_width;
use pilot_core::{SessionId, SessionKey, Workspace};
use pilot_ipc::{Command, Event, TerminalId, TerminalKind};
use ratatui::Frame;
use ratatui::prelude::*;
use ratatui::widgets::*;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Which logical mailbox the sidebar is currently showing.
///
/// Three mutually-exclusive buckets, cycled via `Shift-S`:
///
/// - **Inbox** — actionable workspaces: not snoozed, primary task
///   is Open / Draft / In-Progress / In-Review. The default.
/// - **Inactive** — historical workspaces: primary task is Merged
///   or Closed. Useful for "where did I work on that PR last
///   week" — the data is already persisted, this just surfaces it.
/// - **Snoozed** — explicitly snoozed (Z / Shift-Z).
///
/// Future expansion: a fourth "All repo activity" view that surfaces
/// PRs the user isn't involved in. That requires a separate GH fetch
/// (today the poller filters by `role.*`) and lives with the
/// org/repo picker work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mailbox {
    #[default]
    Inbox,
    Inactive,
    Snoozed,
}

/// One row in the rendered sidebar list. The visual model is a
/// three-level tree:
///
/// ```text
/// owner/name              <- RepoHeader
///   ▸ Workspace title     <- Workspace (always present)
///       claude            <- Session (only when workspace has 2+)
///       shell             <- Session
///   ▸ Other workspace
/// ```
///
/// **Sessions are only surfaced when the workspace has more than
/// one.** A workspace with zero or one session collapses to its
/// single Workspace row — the sub-list would just be redundant. As
/// soon as a second session appears (`Event::SessionCreated`), the
/// workspace expands to show all of them.
///
/// Headers are render-only — j/k navigation and key dispatch skip
/// them, so the cursor always rests on a Workspace or Session row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VisibleRow {
    /// Repo group header. The string is the repo display name
    /// (`"owner/name"` for GitHub, the project key for Linear, or
    /// `"(no repo)"` for unattached workspaces).
    RepoHeader(String),
    /// A workspace under whichever repo header most recently appeared.
    Workspace(SessionKey),
    /// A session sub-row (workspace key + session id). Only emitted
    /// when its parent workspace has 2+ sessions; otherwise the
    /// session is implicit in the workspace row.
    Session {
        workspace: SessionKey,
        session_id: SessionId,
    },
}

/// Per-repo summary line shown in the collapsible header.
#[derive(Debug, Clone, Default)]
pub struct RepoSummary {
    /// Workspaces under this repo that are visible in the current
    /// mailbox. Roughly "active work for this repo".
    pub active: usize,
    /// Workspaces with at least one indicator demanding the user's
    /// attention: unread activity, CI failing, review pending /
    /// changes-requested, agent in `Asking` state. Configurable in
    /// the future; defaults are the indicators pilot already
    /// surfaces as badges on workspace rows.
    pub attention: usize,
}

pub struct Sidebar {
    id: PaneId,
    workspaces: HashMap<SessionKey, Workspace>,
    /// Derived view: workspaces filtered by mailbox, grouped by repo,
    /// each group sorted by updated_at desc. Headers are interleaved
    /// with workspace rows in render order; the cursor navigates
    /// only over workspace rows (headers are skipped).
    visible: Vec<VisibleRow>,
    /// Repos the user has collapsed. Workspace rows under collapsed
    /// repos are still tracked in `workspaces` but are skipped when
    /// `recompute_visible` rebuilds the view.
    collapsed_repos: BTreeSet<String>,
    /// Per-repo counters computed during `recompute_visible`. Keys
    /// are the same display strings used by `VisibleRow::RepoHeader`.
    repo_summaries: BTreeMap<String, RepoSummary>,
    /// Index into `visible`. Always points at a `Workspace` variant
    /// when there is at least one — `recompute_visible` and the
    /// j/k handlers maintain that invariant.
    cursor: usize,
    mailbox: Mailbox,
    /// Two-press confirm latches keyed by trigger. Registers
    /// entries for `Shift-X` (kill) + `Shift-Z` (long snooze).
    /// Disarms every non-matching entry on each keypress, so
    /// pressing `j` between `Shift-X` arm and re-press cancels.
    /// See [`crate::latch_set::LatchSet`].
    latches: crate::latch_set::LatchSet<SessionKey>,
    /// `z` snooze duration. Configurable via
    /// `~/.pilot/config.yaml::ui.short_snooze` (default 4h).
    short_snooze: std::time::Duration,
    /// `Shift-Z` long-snooze duration. Configurable via
    /// `ui.long_snooze` (default 1 year).
    long_snooze: std::time::Duration,
    /// Per-key agent id map. Defaults to `c => "claude", x => "codex",
    /// u => "cursor"`. AppRoot can override via `with_agent_shortcuts`
    /// for users with Aider / custom CLIs configured.
    agent_shortcuts: HashMap<char, String>,
    /// Mirror of the daemon's live-terminals set, scoped to what we
    /// need for the workspace-row runner badges (e.g. ` C  S 2` for
    /// one Claude + two shells running). Populated from `Event::Snapshot`
    /// and kept in sync via `TerminalSpawned` / `TerminalExited`.
    running_terminals: HashMap<TerminalId, (SessionKey, TerminalKind)>,
    /// Threshold config for the per-repo "needs attention" counter.
    /// Loaded from `~/.pilot/config.yaml::attention` at startup;
    /// toggle individual signals there to customize.
    attention: pilot_config::AttentionConfig,
    /// Projects mirrored from the daemon's project table. Each entry
    /// emits a sidebar header so a project with zero workspaces
    /// still appears. Populated by `apply_projects` (called from the
    /// model when `Snapshot` / `ProjectUpserted` / `ProjectRemoved`
    /// fires, and when the wizard's selected scopes are synthesized
    /// into Project entries — the model layer owns that merge).
    projects: BTreeMap<pilot_core::ProjectKey, pilot_core::Project>,
    /// Agent the `f` (fix) shortcut spawns. Defaults to `claude`; the
    /// AppRoot can override from YAML (`setup.default_agent`).
    default_agent: String,
    /// Surface merged + closed tasks in the Inbox view. Off by default
    /// — the Inbox stays focused on actionable work and the Inactive
    /// mailbox owns the history. Wired from
    /// `~/.pilot/config.yaml::display.show_inactive_in_inbox`.
    show_inactive_in_inbox: bool,
    /// Notifications queued in response to "any agent → Asking"
    /// transitions. The library NEVER fires an OS-level
    /// `osascript` / `notify-send` itself — that would break tests
    /// by triggering real banner spam during a `cargo test` run.
    /// The outer wrapper (`realm::components::sidebar`) drains this
    /// after each event delivery and routes to `platform::notify_user`.
    pending_notifications: Vec<PendingNotification>,
    /// One short string per Active→Asking transition since the last
    /// drain. Surfaces in pilot's footer alongside the OS notification
    /// so users with notifications muted still see the prompt.
    pending_asking_notices: Vec<String>,
    /// Workspace keys whose agent is currently in `AgentState::Asking`.
    /// Single source of truth for the `?` row pill, the `? N input`
    /// header counter, and `!` jump-to-asking. Source: `Event::AgentState`
    /// broadcasts from the daemon, sidebar-local — independent of
    /// `Workspace.sessions[i].state` (which gets clobbered every
    /// poll cycle when the daemon re-broadcasts `WorkspaceUpserted`).
    agents_asking: std::collections::HashSet<SessionKey>,
}

/// A queued user-facing notification that the outer (IO-aware) layer
/// will translate into an OS-level banner. Pure data so the sidebar
/// is fully testable without involving any subprocess.
#[derive(Debug, Clone)]
pub struct PendingNotification {
    pub title: String,
    pub body: String,
}

impl Sidebar {
    pub fn new(id: PaneId) -> Self {
        // Lowercase, easy to type, and mirrors the hint-bar:
        //   c → claude, x → codex, u → cursor (`s` is the shell, handled
        //   separately because it isn't an agent registered in the
        //   agent registry).
        let mut agent_shortcuts = HashMap::new();
        agent_shortcuts.insert('c', "claude".to_string());
        agent_shortcuts.insert('x', "codex".to_string());
        agent_shortcuts.insert('u', "cursor".to_string());
        Self {
            id,
            workspaces: HashMap::new(),
            visible: Vec::new(),
            collapsed_repos: BTreeSet::new(),
            repo_summaries: BTreeMap::new(),
            cursor: 0,
            mailbox: Mailbox::Inbox,
            latches: {
                let mut s: crate::latch_set::LatchSet<SessionKey> =
                    crate::latch_set::LatchSet::new();
                s.register(TRIGGER_LONG_SNOOZE);
                s
            },
            short_snooze: pilot_config::UiDefaults::default().short_snooze,
            long_snooze: pilot_config::UiDefaults::default().long_snooze,
            agent_shortcuts,
            running_terminals: HashMap::new(),
            attention: pilot_config::AttentionConfig::default(),
            projects: BTreeMap::new(),
            default_agent: "claude".to_string(),
            show_inactive_in_inbox: false,
            pending_notifications: Vec::new(),
            pending_asking_notices: Vec::new(),
            agents_asking: std::collections::HashSet::new(),
        }
    }

    /// Take any pending desktop notifications queued by event
    /// handling since the last drain. The outer (IO-aware) layer is
    /// responsible for actually firing them via
    /// `crate::platform::notify_user`. Callers must invoke this
    /// after each batch of `on_event` calls — un-drained
    /// notifications sit until the next call.
    ///
    /// Returning the queue rather than firing inline keeps the
    /// sidebar pure: a `cargo test` constructing `Sidebar::new(...)`
    /// and feeding it events will never trigger a real `osascript`
    /// banner.
    pub fn drain_pending_notifications(&mut self) -> Vec<PendingNotification> {
        std::mem::take(&mut self.pending_notifications)
    }

    /// Drain the footer-notice queue. Each entry is a short message
    /// ready to be set as a `NoticeSeverity::Hint`.
    pub fn drain_pending_asking_notices(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending_asking_notices)
    }

    /// Find the FIRST running agent terminal for `workspace_key`
    /// whose agent id matches `agent_id`. Returns `None` when no
    /// matching terminal is running. Used by the `w` flow to reuse
    /// an existing claude tab instead of spawning a second one.
    pub fn find_agent_terminal(
        &self,
        workspace_key: &SessionKey,
        agent_id: &str,
    ) -> Option<TerminalId> {
        for (tid, (sk, kind)) in &self.running_terminals {
            if sk != workspace_key {
                continue;
            }
            if let TerminalKind::Agent(id) = kind
                && id == agent_id
            {
                return Some(*tid);
            }
        }
        None
    }

    /// Optimistic local update: flip a task to `Merged` so the
    /// status pill changes immediately, before the next poll cycle
    /// catches up with GitHub's response. Called when `Event::PrMerged`
    /// arrives from the daemon — GitHub accepted the merge, so the
    /// pill should reflect that NOW, not 30s from now when the next
    /// poll rebroadcasts the workspace.
    pub fn mark_workspace_merged(&mut self, key: &pilot_core::WorkspaceKey) {
        let sk: SessionKey = key.into();
        if let Some(workspace) = self.workspaces.get_mut(&sk) {
            if let Some(pr) = workspace.pr.as_mut() {
                pr.state = pilot_core::TaskState::Merged;
            }
            self.recompute_visible();
        }
    }

    /// Toggle whether merged + closed PRs surface in the Inbox view.
    /// Wired from `DisplayConfig::show_inactive_in_inbox`; idempotent
    /// — calling with the current value is a no-op so a YAML hot-
    /// reload (future) won't churn the cursor.
    pub fn set_show_inactive_in_inbox(&mut self, on: bool) {
        if self.show_inactive_in_inbox == on {
            return;
        }
        self.show_inactive_in_inbox = on;
        self.recompute_visible();
    }

    /// Override the agent the `f` (fix) shortcut spawns. Defaults to
    /// `claude` when not configured; AppRoot wires this from YAML.
    /// Read the currently-configured default agent id (the one `w`
    /// spawns when the resolver picks "work on this"). Exposed so
    /// the orchestrator's `dispatch_action` can drive the same
    /// `resolve_work` call without duplicating the storage.
    pub fn default_agent(&self) -> &str {
        &self.default_agent
    }

    pub fn with_default_agent(mut self, agent: impl Into<String>) -> Self {
        self.default_agent = agent.into();
        self
    }

    /// Replace the mirrored project table. Driven from the model's
    /// `Snapshot::projects` + `ProjectUpserted` / `ProjectRemoved`
    /// handlers; the sidebar's headers render from this on the next
    /// `recompute_visible`. Cheap to call on every snapshot — the
    /// map clones the daemon's view, no diffing required.
    pub fn apply_projects(
        &mut self,
        projects: BTreeMap<pilot_core::ProjectKey, pilot_core::Project>,
    ) {
        if projects != self.projects {
            self.projects = projects;
            self.recompute_visible();
        }
    }

    /// Override the attention thresholds + initial collapse set
    /// from `~/.pilot/config.yaml`. Call once after construction
    /// (typically in main, between `Sidebar::new` and the first
    /// daemon Subscribe).
    pub fn apply_config(
        &mut self,
        attention: pilot_config::AttentionConfig,
        collapsed_repos: BTreeSet<String>,
        agent_shortcuts: HashMap<char, String>,
        default_agent: Option<String>,
        display: &pilot_config::DisplayConfig,
        ui: &pilot_config::UiDefaults,
    ) {
        self.attention = attention;
        self.collapsed_repos = collapsed_repos;
        if !agent_shortcuts.is_empty() {
            self.agent_shortcuts = agent_shortcuts;
        }
        if let Some(agent) = default_agent.filter(|s| !s.is_empty()) {
            self.default_agent = agent;
        }
        self.short_snooze = ui.short_snooze;
        self.long_snooze = ui.long_snooze;
        self.set_show_inactive_in_inbox(display.show_inactive_in_inbox);
    }

    /// Override the default c→claude / C→codex mapping. Keys are
    /// single characters; case matters (`c` and `C` are distinct).
    /// AppRoot wires this from the user's config at startup.
    pub fn with_agent_shortcuts(
        mut self,
        shortcuts: impl IntoIterator<Item = (char, String)>,
    ) -> Self {
        self.agent_shortcuts = shortcuts.into_iter().collect();
        self
    }

    /// Which agents are currently keymapped. For overlays / help
    /// rendering that want to show the user what's available.
    pub fn agent_shortcuts(&self) -> &HashMap<char, String> {
        &self.agent_shortcuts
    }

    // ── Observability helpers (for tests + for AppRoot / RightPane) ────

    pub fn selected_session_key(&self) -> Option<&SessionKey> {
        match self.visible.get(self.cursor)? {
            VisibleRow::Workspace(k) => Some(k),
            VisibleRow::Session { workspace, .. } => Some(workspace),
            VisibleRow::RepoHeader(_) => None,
        }
    }

    /// The specific session id under the cursor, if the cursor is on
    /// a Session sub-row. Workspace rows return `None`, leaving the
    /// daemon to pick the workspace's default session.
    pub fn selected_session_id(&self) -> Option<SessionId> {
        match self.visible.get(self.cursor)? {
            VisibleRow::Session { session_id, .. } => Some(*session_id),
            _ => None,
        }
    }

    /// If the row under the cursor is something the user can "work
    /// on" right now, return `(session_key, work_prompt)` ready for
    /// `Command::Spawn`. Polymorphic by task type:
    ///
    /// - **GitHub issue** → ask the agent to implement it (branch
    ///   from the default base, code it up, open a PR closing the
    ///   issue).
    /// - **PR with `ci == Failure`** → reuse the existing
    ///   `fix_target_for_cursor` prompt.
    /// - **Anything else** → None (key hides itself in the hint bar).
    ///
    /// This is the entry point for the `w` ("work on this") keybinding.
    /// It supersedes the narrower `f` (kept for muscle memory and the
    /// CI-fail case it originally covered).
    pub fn work_target_for_cursor(&self) -> Option<(SessionKey, String)> {
        build_work_prompt(self.selected_workspace()?)
    }

    /// Return the workspace key the `Shift-M` merge shortcut would
    /// target. Only fires when the focused row is a PR in a state
    /// GitHub would let us merge — Approved + CI green / none — so
    /// the contextual footer can advertise the key only when it'll
    /// actually work.
    pub fn merge_target_for_cursor(&self) -> Option<pilot_core::WorkspaceKey> {
        let workspace = self.selected_workspace()?;
        let pr = workspace.pr.as_ref()?;
        if !matches!(
            pr.state,
            pilot_core::TaskState::Open | pilot_core::TaskState::InReview
        ) {
            return None;
        }
        if !matches!(pr.review, pilot_core::ReviewStatus::Approved) {
            return None;
        }
        if !matches!(
            pr.ci,
            pilot_core::CiStatus::Success | pilot_core::CiStatus::None
        ) {
            return None;
        }
        if pr.mergeable.is_conflicting() {
            return None;
        }
        Some(pilot_core::WorkspaceKey::new(workspace.key.as_str()))
    }

    /// If the row under the cursor is a PR with `ci == Fail`, return
    /// `(session_key, fix_prompt)` ready for `Command::Spawn`. None
    /// otherwise — used both by the `f` keybinding match guard and
    /// by the hint bar so the key only advertises when it'll fire.
    pub fn fix_target_for_cursor(&self) -> Option<(SessionKey, String)> {
        build_fix_ci_prompt(self.selected_workspace()?)
    }

    /// Read-only view of the rendered rows. Tests + the layout helper
    /// use this to assert grouping without poking at internals.
    pub fn visible_rows(&self) -> &[VisibleRow] {
        &self.visible
    }

    /// Translate a mouse click row inside `area` to a visible-row
    /// index, and move the cursor onto it. Returns true if the click
    /// landed on a selectable row (not a repo header / outside the
    /// content area). Header rows + clicks above the content area
    /// are ignored.
    pub fn click_to_select(&mut self, area: Rect, click_row: u16) -> bool {
        // Mirror the constants from `render`.
        const HEADER_HEIGHT: u16 = 5;
        if click_row < area.y + HEADER_HEIGHT {
            return false;
        }
        let idx = (click_row - area.y - HEADER_HEIGHT) as usize;
        match self.visible.get(idx) {
            Some(VisibleRow::Workspace(_)) | Some(VisibleRow::Session { .. }) => {
                self.cursor = idx;
                true
            }
            // RepoHeader / out-of-bounds → no-op so the click doesn't
            // strand the cursor on a non-selectable row.
            _ => false,
        }
    }

    /// Look up a workspace by its session key. Used by paths that
    /// need workspace data without disturbing the cursor (e.g. the
    /// editor-deferred-by-spawn flow that has to find the
    /// worktree of a specific workspace, not the focused one).
    pub fn workspace_by_key(&self, key: &SessionKey) -> Option<&Workspace> {
        self.workspaces.get(key)
    }

    /// Move the cursor onto the workspace row matching `key`. Returns
    /// true on a hit. Used by `--workspace` preselect on startup.
    pub fn focus_workspace_key(&mut self, key: &SessionKey) -> bool {
        for (i, row) in self.visible.iter().enumerate() {
            if let VisibleRow::Workspace(k) = row
                && k == key
            {
                self.cursor = i;
                return true;
            }
        }
        false
    }

    /// Move the cursor onto the next workspace whose agent is in the
    /// `Asking` state, starting AFTER the row currently selected (so
    /// `!` cycles through asking workspaces rather than re-selecting
    /// the current one). Wraps around the visible list. Returns true
    /// when a target was found and the cursor moved.
    ///
    /// Pure decision lives in `agent_attention::next_asking_workspace`;
    /// this method just glues it to the sidebar's cursor + visible
    /// row state.
    pub fn focus_next_asking_workspace(&mut self) -> bool {
        let keys_order: Vec<SessionKey> = self
            .visible
            .iter()
            .filter_map(|r| match r {
                VisibleRow::Workspace(k) => Some(k.clone()),
                _ => None,
            })
            .collect();
        let current = self.selected_session_key().cloned();
        let Some(target) = crate::agent_attention::next_asking_workspace(
            &self.agents_asking,
            &keys_order,
            current.as_ref(),
        ) else {
            return false;
        };
        self.focus_workspace_key(&target)
    }

    /// Move the cursor onto the session sub-row matching `id`. No-op
    /// when the row isn't visible — caller must already have aligned
    /// the workspace via `focus_workspace_key`.
    pub fn focus_session_id(&mut self, id: SessionId) -> bool {
        for (i, row) in self.visible.iter().enumerate() {
            if let VisibleRow::Session { session_id, .. } = row
                && *session_id == id
            {
                self.cursor = i;
                return true;
            }
        }
        false
    }

    /// The workspace under the cursor, or `None` if the visible list
    /// is empty. The TUI's right pane / terminal stack consume this
    /// so they always reflect the sidebar's selection.
    pub fn selected_workspace(&self) -> Option<&Workspace> {
        self.selected_session_key()
            .and_then(|k| self.workspaces.get(k))
    }

    pub fn mailbox(&self) -> Mailbox {
        self.mailbox
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn visible_count(&self) -> usize {
        self.visible.len()
    }

    /// How many *workspace* rows are visible (excluding repo headers).
    /// Title bar uses this — counting headers would be confusing
    /// because they're navigation chrome, not items.
    pub fn workspace_count(&self) -> usize {
        self.visible
            .iter()
            .filter(|r| matches!(r, VisibleRow::Workspace(_)))
            .count()
    }

    /// Iterate every workspace this sidebar knows about, regardless
    /// of visibility filter. The adopt-target picker uses this to
    /// build its candidate list — including ones currently hidden
    /// by the active mailbox so the user isn't forced to swap views
    /// before moving sessions.
    pub fn workspace_iter(&self) -> impl Iterator<Item = (&SessionKey, &Workspace)> {
        self.workspaces.iter()
    }

    /// The Project the cursor is currently "in" — drives the `n` (new
    /// workspace) flow. Resolution:
    ///
    /// - Cursor on a `Workspace` row → the workspace's project_key
    ///   (with the standard `workspace_project_key` fallback so
    ///   pre-Stage-1 records still resolve).
    /// - Cursor on a `RepoHeader` → look up the project whose
    ///   display name matches the header string.
    /// - Anything else → `None`. The model surfaces a footer notice
    ///   ("select a project first") instead of mounting the prompt.
    pub fn focused_project_key(&self) -> Option<pilot_core::ProjectKey> {
        match self.visible.get(self.cursor)? {
            VisibleRow::Workspace(k) => {
                let w = self.workspaces.get(k)?;
                pilot_core::workspace_project_key(w)
            }
            VisibleRow::Session { workspace, .. } => {
                let w = self.workspaces.get(workspace)?;
                pilot_core::workspace_project_key(w)
            }
            VisibleRow::RepoHeader(name) => self
                .projects
                .values()
                .find(|p| &p.name == name)
                .map(|p| p.key.clone()),
        }
    }

    /// Step the cursor `delta` selectable rows from its current
    /// position, skipping repo headers. Workspace rows AND session
    /// sub-rows are selectable; only headers are not. Clamps at the
    /// first/last selectable row.
    /// True if the cursor sits on a repo header row.
    pub fn cursor_on_repo_header(&self) -> bool {
        matches!(
            self.visible.get(self.cursor),
            Some(VisibleRow::RepoHeader(_))
        )
    }

    fn move_cursor_by(&mut self, delta: isize) {
        if delta == 0 || self.visible.is_empty() {
            return;
        }
        // Navigate over EVERYTHING — workspaces, sessions, and repo
        // headers. Stopping on headers is what lets the user expand
        // a collapsed repo (Space toggles whatever the cursor's on).
        let selectable: Vec<usize> = (0..self.visible.len()).collect();
        if selectable.is_empty() {
            return;
        }
        let pos = selectable
            .iter()
            .position(|i| *i == self.cursor)
            .unwrap_or(0);
        let target = (pos as isize + delta).clamp(0, selectable.len() as isize - 1) as usize;
        self.cursor = selectable[target];
    }

    /// Total unread activity items across all VISIBLE workspaces. Used
    /// by the top header's `N new` badge — only the current mailbox's
    /// unread is counted, so cycling Inbox→Snoozed shows different
    /// totals (snoozed PRs aren't "in your face" by definition).
    fn total_unread_count(&self) -> usize {
        self.visible
            .iter()
            .filter_map(|r| match r {
                VisibleRow::Workspace(k) => self.workspaces.get(k),
                _ => None,
            })
            .map(|w| w.unread_count())
            .sum()
    }

    /// Number of visible workspaces currently carrying `signal`. All
    /// per-signal header counters go through this helper so the
    /// `? N input` / `N CI` / `N review` totals agree with the
    /// per-repo "needs attention" badge — they read the same
    /// producer (`workspace_attention_signals`).
    fn count_visible_with_signal(&self, signal: AttentionSignal) -> usize {
        self.visible
            .iter()
            .filter_map(|r| match r {
                VisibleRow::Workspace(k) => self.workspaces.get(k),
                _ => None,
            })
            .filter(|w| workspace_attention_signals(w, &self.agents_asking).contains(&signal))
            .count()
    }

    /// Drives the `? N input` indicator in the top header — a quick
    /// "agents stuck on prompts" tally.
    fn input_pending_count(&self) -> usize {
        self.count_visible_with_signal(AttentionSignal::AgentAsking)
    }

    /// Drives the `N CI` summary — at-a-glance "how many of my PRs
    /// are broken right now."
    fn ci_failing_count(&self) -> usize {
        self.count_visible_with_signal(AttentionSignal::CiFailing)
    }

    /// Visible workspaces where a reviewer is requested or a review
    /// is pending — the "N review" half of the stats row.
    fn review_pending_count(&self) -> usize {
        self.count_visible_with_signal(AttentionSignal::ReviewPending)
    }

    /// Stable single-letter key for a runner kind. Drives the workspace
    /// row badge — `claude` → `C`, `codex` → `X`, `cursor` → `U`,
    /// `shell` → `S`, log tail → `L`, generic agent → `A`.
    fn badge_letter(kind: &TerminalKind) -> char {
        match kind {
            TerminalKind::Agent(id) => match id.as_str() {
                "claude" => 'C',
                "codex" => 'X',
                "cursor" => 'U',
                _ => id
                    .chars()
                    .next()
                    .map(|c| c.to_ascii_uppercase())
                    .unwrap_or('A'),
            },
            TerminalKind::Shell => 'S',
            TerminalKind::LogTail { .. } => 'L',
        }
    }

    /// Aggregate live terminals on `key` into a list of `(letter, count)`
    /// pairs for the sidebar's runner badge. Stable order: agents first
    /// (alphabetical by letter), shells last so the eye lands on the
    /// agent state first. Returns `[]` when the workspace has no live
    /// terminals.
    fn runner_badges(&self, key: &SessionKey) -> Vec<(char, usize)> {
        let mut counts: HashMap<char, usize> = HashMap::new();
        for (sk, kind) in self.running_terminals.values() {
            if sk == key {
                *counts.entry(Self::badge_letter(kind)).or_default() += 1;
            }
        }
        let mut entries: Vec<(char, usize)> = counts.into_iter().collect();
        entries.sort_by_key(|(c, _)| match *c {
            'S' => (1, 'S'),
            other => (0, other),
        });
        entries
    }

    /// Toggle the collapsed flag for the repo at or above the
    /// cursor. Used by `Space`. Resolution:
    ///
    /// - cursor on a `RepoHeader` → toggle that header.
    /// - cursor on a workspace / session → walk back to find the
    ///   nearest header (the cursor's group) and toggle that.
    ///
    /// On collapse, cursor snaps to the now-collapsed header so
    /// j/k from there land on adjacent headers cleanly.
    pub fn toggle_repo_at_cursor(&mut self) -> bool {
        let repo = match self.visible.get(self.cursor).cloned() {
            Some(VisibleRow::RepoHeader(name)) => Some(name),
            Some(VisibleRow::Workspace(_)) | Some(VisibleRow::Session { .. }) => self
                .visible
                .iter()
                .take(self.cursor + 1)
                .rev()
                .find_map(|r| match r {
                    VisibleRow::RepoHeader(name) => Some(name.clone()),
                    _ => None,
                }),
            None => None,
        };
        let Some(repo) = repo else { return false };
        let was_collapsed = self.collapsed_repos.contains(&repo);
        if was_collapsed {
            self.collapsed_repos.remove(&repo);
        } else {
            self.collapsed_repos.insert(repo.clone());
        }
        self.recompute_visible();
        // Persist the new set to ~/.pilot/config.yaml::ui.collapsed_repos
        // so the layout survives restart. Best-effort; an I/O
        // error here just means next launch starts expanded.
        let snapshot = self.collapsed_repos.clone();
        if let Err(e) = pilot_config::Config::save_with(|c| c.ui.collapsed_repos = snapshot) {
            tracing::warn!("save collapsed_repos failed: {e}");
        }
        // Always park the cursor on the toggled header so
        // collapse + immediately re-expand works (Space twice in a
        // row toggles the same repo).
        if let Some(idx) = self
            .visible
            .iter()
            .position(|r| matches!(r, VisibleRow::RepoHeader(n) if n == &repo))
        {
            self.cursor = idx;
        }
        true
    }

    /// Read-only view of the per-repo summary for render. Headers
    /// look up by their display name.
    pub fn repo_summary(&self, name: &str) -> Option<&RepoSummary> {
        self.repo_summaries.get(name)
    }

    /// True when the repo is currently collapsed (used by the
    /// header render to pick `▾` vs `▸`).
    pub fn is_repo_collapsed(&self, name: &str) -> bool {
        self.collapsed_repos.contains(name)
    }

    fn recompute_visible(&mut self) {
        self.recompute_visible_inner(true);
    }

    /// Variant for callers that have just *reset* `self.cursor` (e.g.
    /// mailbox cycle, fresh snapshot). The reset clobbered whatever
    /// row the user was on, so the regular "park me back on the same
    /// header" preservation is wrong here — without this, cursor=0
    /// lands on the OLD header row and gets re-parked on the matching
    /// header in the new visible list, leaving the cursor stuck on a
    /// non-selectable header instead of falling through to the first
    /// workspace row.
    fn reset_cursor_and_recompute(&mut self) {
        self.cursor = 0;
        self.recompute_visible_inner(false);
    }

    fn recompute_visible_inner(&mut self, preserve_header_park: bool) {
        // Snapshot cursor anchors before the rebuild so we can
        // restore the user's focused row when the new visible list
        // is in place. Two anchors: (a) parked-on-header preserves
        // the header name; (b) parked-on-workspace/session preserves
        // (workspace_key, session_id?) — fallbacks handle the case
        // where the prior row vanished.
        let prior_key = self.selected_session_key().cloned();
        let prior_session = self.selected_session_id();
        let prior_header = if preserve_header_park {
            match self.visible.get(self.cursor) {
                Some(VisibleRow::RepoHeader(name)) => Some(name.clone()),
                _ => None,
            }
        } else {
            None
        };

        // Rebuild via the pure `compute_visible` builder. Every
        // grouping / classification rule (sandbox bucket, empty
        // subscribed-repo headers, sorted-by-updated-at) is unit-
        // tested over there in isolation.
        let outcome = crate::components::visible_rows::compute_visible(
            crate::components::visible_rows::ComputeInputs {
                workspaces: &self.workspaces,
                mailbox: self.mailbox,
                show_inactive_in_inbox: self.show_inactive_in_inbox,
                projects: &self.projects,
                collapsed_repos: &self.collapsed_repos,
                attention: &self.attention,
                agents_asking: &self.agents_asking,
                now: chrono::Utc::now(),
            },
        );
        self.visible = outcome.visible;
        self.repo_summaries = outcome.summaries;

        // Preserve cursor on a repo header across reorderings — j/k
        // can land on headers (collapse target), and snapshots
        // arriving while parked there shouldn't yank focus.
        if let Some(name) = prior_header
            && let Some(idx) = self
                .visible
                .iter()
                .position(|r| matches!(r, VisibleRow::RepoHeader(n) if n == &name))
        {
            self.cursor = idx;
            return;
        }

        // Preserve cursor across reorderings. Match by (workspace
        // key, session id) tuple so a cursor sitting on a session
        // sub-row stays on that exact row when sibling sessions
        // come and go. Workspace-row cursors match by key alone.
        if let Some(key) = prior_key {
            for (i, row) in self.visible.iter().enumerate() {
                let matched = match row {
                    VisibleRow::Workspace(k) => *k == key && prior_session.is_none(),
                    VisibleRow::Session {
                        workspace,
                        session_id,
                    } => *workspace == key && Some(*session_id) == prior_session,
                    VisibleRow::RepoHeader(_) => false,
                };
                if matched {
                    self.cursor = i;
                    return;
                }
            }
            // Session vanished but workspace still here — fall back
            // to the workspace row.
            for (i, row) in self.visible.iter().enumerate() {
                if matches!(row, VisibleRow::Workspace(k) if *k == key) {
                    self.cursor = i;
                    return;
                }
            }
        }
        // Prior selection vanished entirely. Land on the first
        // selectable row (workspace or session), or 0 if nothing left.
        self.cursor = self
            .visible
            .iter()
            .position(|r| matches!(r, VisibleRow::Workspace(_) | VisibleRow::Session { .. }))
            .unwrap_or(0);
    }
}

/// Inherent methods. Names match what the legacy `tui_kit::Pane`
/// trait used to require, so the old `app::run` path's concrete-type
/// calls (`app.sidebar.handle_key(...)`) still resolve here without
/// the trait being in scope.
impl Sidebar {
    /// Stable pane id assigned at construction.
    pub fn id(&self) -> PaneId {
        self.id
    }

    /// Title rendered in the pane border.
    pub fn title(&self) -> &str {
        "Inbox"
    }

    /// Bindings advertised in the hint bar.
    /// State-aware short list for the footer hint bar.
    ///
    /// Catalog-driven: the actions worth surfacing right now are
    /// pushed as `pilot_tui_core::action::Action`s, then converted
    /// to `Binding`s through `ActionDef::for_action` + the centralized
    /// `contextual_label` helper. Adding a new sidebar action means
    /// landing it in the catalog and pushing it here — the footer,
    /// `?` help, and right-click menu all pick it up automatically.
    pub fn contextual_bindings(&self) -> Vec<crate::Binding> {
        use crate::Binding;
        use pilot_tui_core::action::{Action, ActionDef, contextual_label};

        let workspace = self.selected_workspace();
        let is_ready = self.merge_target_for_cursor().is_some();
        let mut actions: Vec<Action> = Vec::with_capacity(6);

        // Primary action: what's most likely useful on THIS row.
        // Merge takes precedence over Work when the PR is ready.
        if is_ready {
            actions.push(Action::MergePr);
        } else if crate::intent::classify_work(workspace, &[]).is_some() {
            actions.push(Action::Work);
        }

        // Mark-all-read surfaces when there's unread activity.
        if workspace.is_some_and(|w| w.unread_count() > 0) {
            actions.push(Action::MarkAllRead);
        }

        // Session lifecycle. Spawn shortcuts + editor + archive
        // surface whenever a workspace is selected. `Shift-X`
        // archive's "(kills sessions)" suffix flips automatically
        // via `contextual_label`.
        if workspace.is_some() {
            actions.push(Action::SpawnAgent("claude".into()));
            actions.push(Action::SpawnShell);
            actions.push(Action::OpenEditor);
            actions.push(Action::Archive);
        }
        actions.push(Action::NewWorkspace);
        actions.push(Action::NewProject);

        // Convert to Binding rows. `default_keys` flows through
        // unchanged today; user rebinding will override here once
        // the config layer lands.
        actions
            .into_iter()
            .map(|a| {
                let def = ActionDef::for_action(&a);
                // SpawnAgent's runtime id needs surface-specific
                // labeling — the catalog's static "spawn agent"
                // would be too generic.
                let label = match &a {
                    Action::SpawnAgent(id) if id == "claude" => "claude",
                    Action::SpawnAgent(_) => def.label,
                    _ => contextual_label(&a, workspace),
                };
                Binding {
                    keys: def.default_keys,
                    label,
                }
            })
            .collect()
    }

    pub fn keymap(&self) -> &'static [crate::Binding] {
        use crate::Binding;
        // Pane-local bindings only — Tab / q-q / ? / Shift-arrows /
        // Ctrl-Shift-D etc. live in the Global section of the Help
        // modal so they don't duplicate across every pane's hint bar.
        &[
            Binding {
                keys: "↑/↓",
                label: "navigate",
            },
            Binding {
                keys: "Enter",
                label: "focus activity",
            },
            Binding {
                keys: "n",
                label: "new workspace",
            },
            Binding {
                keys: "e",
                label: "open editor",
            },
            Binding {
                keys: "Space",
                label: "fold repo",
            },
            Binding {
                keys: "s",
                label: "shell",
            },
            Binding {
                keys: "c",
                label: "claude",
            },
            Binding {
                keys: "x",
                label: "codex",
            },
            Binding {
                keys: "u",
                label: "cursor",
            },
            Binding {
                keys: "w",
                label: "work on this",
            },
            Binding {
                keys: "Shift-M",
                label: "merge PR (when READY)",
            },
            Binding {
                keys: "Shift-A",
                label: "adopt sessions",
            },
            Binding {
                keys: "m",
                label: "mark all read",
            },
            Binding {
                keys: "/",
                label: "search",
            },
        ]
    }

    pub fn detachable(&self) -> Option<crate::DetachSpec> {
        // Cursor on a session sub-row → detach that specific session.
        // Cursor on a workspace row → detach the whole workspace
        // (both spawn the same kind of child pilot — different
        // arg shape).
        match self.visible.get(self.cursor)? {
            VisibleRow::Session {
                workspace,
                session_id,
            } => Some(crate::DetachSpec {
                layout: "session",
                args: vec![
                    "--workspace".to_string(),
                    workspace.as_str().to_string(),
                    "--session".to_string(),
                    session_id.0.to_string(),
                ],
            }),
            VisibleRow::Workspace(k) => Some(crate::DetachSpec {
                layout: "workspace",
                args: vec!["--workspace".to_string(), k.as_str().to_string()],
            }),
            VisibleRow::RepoHeader(_) => None,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent, cmds: &mut Vec<Command>) -> PaneOutcome {
        // Each two-press latch disarms when its trigger key isn't
        // the next press. Single source of truth for the "first
        // press arms, second press fires, anything else disarms"
        // contract is owned by `LatchSet`. One call disarms every
        // registered latch whose trigger doesn't match this key —
        // no per-action `if !is_shift_X { latch.disarm() }` line.
        self.latches.disarm_others(key.code, key.modifiers);

        match (key.code, key.modifiers) {
            // ── Navigation ────────────────────────────────────────────
            (KeyCode::Down, m) if !m.contains(KeyModifiers::SHIFT) => {
                self.move_cursor_by(1);
                PaneOutcome::Consumed
            }
            (KeyCode::Up, m) if !m.contains(KeyModifiers::SHIFT) => {
                self.move_cursor_by(-1);
                PaneOutcome::Consumed
            }
            // ── Collapse / expand the cursor's repo group ─────────────
            // Space toggles the parent repo of whatever workspace /
            // session row the cursor is on. Mimics file-tree TUIs
            // (yazi, nnn, lf) where Space folds a directory.
            (KeyCode::Char(' '), KeyModifiers::NONE) => {
                self.toggle_repo_at_cursor();
                PaneOutcome::Consumed
            }

            // ── Spawn / open ──────────────────────────────────────────
            // Any Char key listed in `agent_shortcuts` spawns that
            // agent for the selected session. Defaults: `c` → Claude,
            // `x` → Codex, `u` → Cursor. AppRoot can remap at startup
            // via `with_agent_shortcuts`. Keys NOT in the map bubble
            // up, so overlays / other components get a fair shot.
            //
            // The (workspace_state, agent_id) → Intent decision lives
            // in `intent::resolve_spawn_agent`; this handler is the
            // execute side. Returning `Intent::NoOp` when nothing is
            // selected is now testable in isolation instead of being
            // a silent inline branch.
            (KeyCode::Char(c), m)
                if self.agent_shortcuts.contains_key(&c)
                    && !m.contains(KeyModifiers::CONTROL)
                    && !m.contains(KeyModifiers::ALT) =>
            {
                let agent_id = self.agent_shortcuts.get(&c).cloned().unwrap_or_default();
                match crate::intent::resolve_spawn_agent(self.selected_workspace(), &agent_id) {
                    crate::intent::Intent::SpawnAgent {
                        workspace_key,
                        agent_id,
                        prompt,
                    } => {
                        tracing::info!(
                            key = %c, %workspace_key, agent_id = %agent_id,
                            "sidebar: emitting Spawn(Agent)"
                        );
                        cmds.push(Command::Spawn {
                            session_key: workspace_key,
                            // The selected session sub-row, if any,
                            // scopes the spawn into a specific
                            // worktree. None → daemon picks the
                            // workspace's default session.
                            session_id: self.selected_session_id(),
                            kind: TerminalKind::Agent(agent_id),
                            cwd: None,
                            initial_prompt: prompt,
                        });
                    }
                    _ => {
                        tracing::warn!(
                            key = %c,
                            "sidebar: agent shortcut pressed but resolver returned NoOp \
                             (no workspace selected or empty agent id)"
                        );
                    }
                }
                PaneOutcome::Consumed
            }
            // `w` for "work on this" — single polymorphic key. Spawns
            // the default agent with a context-aware prompt:
            //  - on an issue row → implement the issue
            //  - on a PR row with CI failing → fix the failing checks
            // Match guard hides the key in the hint bar when neither
            // case applies, so users see `w` only where it'll fire.
            // (We removed the old `f` mnemonic — `w` covers both
            // cases, plus the right-pane `w` for selected comments,
            // so the user has one work key everywhere.)
            (KeyCode::Char('w'), KeyModifiers::NONE)
                if matches!(
                    crate::intent::resolve_work(
                        self.selected_workspace(),
                        &[],
                        &self.default_agent,
                    ),
                    crate::intent::Intent::SpawnAgent { .. }
                ) =>
            {
                // Sidebar `w` never has selected-comments (the activity
                // pane owns that selection state), so pass an empty
                // slice — the resolver does the priority chain.
                let intent = crate::intent::resolve_work(
                    self.selected_workspace(),
                    &[],
                    &self.default_agent,
                );
                if let crate::intent::Intent::SpawnAgent {
                    workspace_key,
                    agent_id,
                    prompt,
                } = intent
                {
                    tracing::info!(%workspace_key, %agent_id, "sidebar: emitting Spawn(Agent) with work prompt");
                    cmds.push(Command::Spawn {
                        session_key: workspace_key,
                        session_id: self.selected_session_id(),
                        kind: TerminalKind::Agent(agent_id),
                        cwd: None,
                        initial_prompt: prompt,
                    });
                }
                PaneOutcome::Consumed
            }

            // `s` for shell — used to be `b` (for "bash") but the
            // hint bar reads better as "S shell / C claude / X codex /
            // U cursor" all-lowercase, and `s` is mnemonic.
            //
            // Decision lives in `intent::resolve_spawn_shell` — same
            // (workspace_state, key) → Intent shape as every other
            // spawn key.
            (KeyCode::Char('s'), KeyModifiers::NONE) => {
                match crate::intent::resolve_spawn_shell(self.selected_workspace()) {
                    crate::intent::Intent::SpawnShell { workspace_key } => {
                        tracing::info!(%workspace_key, "sidebar: emitting Spawn(Shell)");
                        cmds.push(Command::Spawn {
                            session_key: workspace_key,
                            session_id: self.selected_session_id(),
                            kind: TerminalKind::Shell,
                            cwd: None,
                            initial_prompt: None,
                        });
                    }
                    _ => {
                        tracing::warn!(
                            "sidebar: shell shortcut pressed but resolver returned NoOp \
                             (no workspace selected)"
                        );
                    }
                }
                PaneOutcome::Consumed
            }

            // ── Session actions ───────────────────────────────────────
            (KeyCode::Char('m'), KeyModifiers::NONE) => {
                if let crate::intent::Intent::MarkAllRead { session_key } =
                    crate::intent::resolve_mark_read(self.selected_workspace())
                {
                    cmds.push(Command::MarkRead { session_key });
                }
                PaneOutcome::Consumed
            }
            // `g` used to be sidebar-local refresh. Removed: it
            // duplicated `Shift+R` (global, discoverable from `?`
            // help) and collided with the vim `g`/`G` "go to
            // top/bottom" convention the right pane already uses.
            // One refresh binding, accessible from every pane.
            // `z` toggle-snooze is now handled by the catalog
            // dispatch in `Model::dispatch_action(ToggleSnooze)` —
            // same resolver, same effect, reads
            // `ui_defaults.short_snooze` instead of the sidebar's
            // local copy (one fewer place to keep in sync).
            (KeyCode::Char('Z'), m) if m.contains(KeyModifiers::SHIFT) => {
                // Two-press confirm — 1-year snooze is effectively
                // "hide forever" with no obvious undo. The
                // `ConfirmLatch::arm_or_fire` returns true on the
                // SECOND consecutive press; otherwise it arms +
                // returns false. The actual snooze duration lives
                // in the Intent the resolver returns.
                let Some(session_key) = self.selected_session_key().cloned() else {
                    return PaneOutcome::Consumed;
                };
                if !self
                    .latches
                    .arm_or_fire(TRIGGER_LONG_SNOOZE, session_key.clone())
                {
                    return PaneOutcome::Consumed;
                }
                let workspace = self.selected_workspace();
                let intent = crate::intent::resolve_long_snooze(workspace, self.long_snooze);
                if let crate::intent::Intent::Snooze {
                    session_key,
                    duration,
                } = intent
                {
                    let until = chrono::Utc::now()
                        + chrono::Duration::from_std(duration)
                            .unwrap_or(chrono::Duration::days(365));
                    cmds.push(Command::Snooze { session_key, until });
                }
                PaneOutcome::Consumed
            }

            // ── Mailbox cycle (Inbox → Inactive → Snoozed → Inbox)
            (KeyCode::Char('S'), m) if m.contains(KeyModifiers::SHIFT) => {
                self.mailbox = match self.mailbox {
                    Mailbox::Inbox => Mailbox::Inactive,
                    Mailbox::Inactive => Mailbox::Snoozed,
                    Mailbox::Snoozed => Mailbox::Inbox,
                };
                // New mailbox → reset cursor to top; old cursor key is
                // almost certainly not visible in the other mailbox.
                self.reset_cursor_and_recompute();
                PaneOutcome::Consumed
            }

            // Shift+X Archive is now handled by the catalog
            // dispatch path in `Model::dispatch_action` — it calls
            // `Sidebar::arm_or_fire_archive` which drives the same
            // two-press latch this match arm used to. First press
            // arms (sidebar chrome shows "press again to confirm");
            // second within the latch window fires `Kill`.

            // Shift+M MergePr is now handled by the catalog
            // dispatch path in `Model::dispatch_action` — it does
            // the same `resolve_merge` precondition check, then
            // mounts the Confirm modal. The match arm here used to
            // queue a `pending_merge_requests` entry that the
            // orchestrator drained post-dispatch; that queue is
            // gone now (one fewer indirection).

            // Anything else: bubble up. Tab / Help / `?` / overlays /
            // quit are handled by parent components.
            _ => PaneOutcome::Pass,
        }
    }

    pub fn on_event(&mut self, event: &Event) {
        match event {
            Event::Snapshot {
                workspaces,
                terminals,
                ..
            } => {
                self.workspaces.clear();
                for w in workspaces {
                    let key: SessionKey = (&w.key).into();
                    self.workspaces.insert(key, w.clone());
                }
                self.running_terminals.clear();
                for t in terminals {
                    self.running_terminals
                        .insert(t.terminal_id, (t.session_key.clone(), t.kind.clone()));
                }
                self.reset_cursor_and_recompute();
            }
            Event::TerminalSpawned {
                terminal_id,
                session_key,
                kind,
            } => {
                self.running_terminals
                    .insert(*terminal_id, (session_key.clone(), kind.clone()));
            }
            Event::TerminalExited { terminal_id, .. } => {
                self.running_terminals.remove(terminal_id);
            }
            Event::WorkspaceUpserted(workspace) => {
                let key: SessionKey = (&workspace.key).into();
                self.workspaces.insert(key, (**workspace).clone());
                self.recompute_visible();
            }
            Event::WorkspaceRemoved(key) => {
                let session_key: SessionKey = key.into();
                self.workspaces.remove(&session_key);
                self.recompute_visible();
            }
            Event::SessionCreated(session) => {
                let key: SessionKey = (&session.workspace_key).into();
                if let Some(w) = self.workspaces.get_mut(&key) {
                    // Idempotent — the canonical add_session will
                    // refuse to duplicate if the daemon resends.
                    if w.find_session(session.id).is_none() {
                        w.sessions.push((**session).clone());
                    }
                    self.recompute_visible();
                }
            }
            Event::SessionEnded {
                workspace_key,
                session_id,
            } => {
                let key: SessionKey = workspace_key.into();
                if let Some(w) = self.workspaces.get_mut(&key) {
                    w.remove_session(*session_id);
                    self.recompute_visible();
                }
            }
            Event::AgentState { session_key, state } => {
                tracing::info!(
                    %session_key,
                    state = ?state,
                    "sidebar: received Event::AgentState",
                );
                // The daemon-side detector flipped an agent into
                // `Asking` (yes/no prompt) or back to `Active`.
                // Update the sidebar-local `agents_asking` set —
                // the canonical store for this transient signal.
                //
                // Why a sidebar-local set instead of mutating
                // `workspace.sessions[i].state`: the next poll
                // cycle's `WorkspaceUpserted` rebuilds the workspace
                // from the persisted store, which doesn't (and
                // shouldn't) carry transient agent state. Mutating
                // it here would be silently undone within 60s. The
                // set survives poll broadcasts because nothing
                // touches it except `Event::AgentState`.
                //
                // On the Active → Asking edge, enqueue a desktop
                // notification (drained + fired by the outer
                // wrapper so library tests never trigger a real
                // `osascript` / `notify-send`).
                let transition = crate::agent_attention::apply_agent_state(
                    &mut self.agents_asking,
                    session_key,
                    *state,
                );
                if matches!(
                    transition,
                    crate::agent_attention::AttentionTransition::NowAsking
                ) {
                    if let Some(workspace) = self.workspaces.get(session_key) {
                        let title = format!("pilot — {} needs input", workspace.name);
                        let body = workspace
                            .primary_task()
                            .map(|t| t.title.clone())
                            .unwrap_or_else(|| workspace.name.clone());
                        self.pending_notifications.push(PendingNotification {
                            title: title.clone(),
                            body,
                        });
                        // Inline footer notice in addition to the OS
                        // popup — covers users with notifications muted
                        // (which is most of them while focused). Hint
                        // severity = 3s fade, dim color.
                        self.pending_asking_notices
                            .push(format!("{} needs input — press ! to jump", workspace.name));
                    }
                }
                if !matches!(
                    transition,
                    crate::agent_attention::AttentionTransition::NoChange
                ) {
                    self.recompute_visible();
                }
            }
            _ => {}
        }
    }
}

mod pills;
mod render;

#[cfg(test)]
mod tests;

// Re-export pills.rs items so callers in the rest of the crate
// keep their `crate::components::sidebar::*` import paths.
pub(crate) use pills::{
    AttentionSignal, BADGE_COL_W, STATUS_COL_W, TIME_COL_W, UNREAD_COL_W, badge_pill_style,
    mailbox_membership, relative_time, role_badge, status_pills, truncate_ellipsis,
    workspace_attention_signals, workspace_needs_attention, workspace_type_label,
};
#[cfg(test)]
pub(crate) use pills::{pill_for_tag, status_pill};

// Prompt builders moved to `pilot_tui_core::prompts` (so `intent`,
// which also lives there, can call them without creating a dep
// cycle). Re-exported here at the legacy `sidebar::*` paths for
// back-compat.
pub use pilot_tui_core::prompts::{
    build_fix_ci_prompt, build_fix_conflict_prompt, build_work_prompt,
};
