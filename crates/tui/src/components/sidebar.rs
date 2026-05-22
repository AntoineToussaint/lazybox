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

/// Trigger for the kill-workspace confirm latch (`Shift-X`).
/// First press arms, second press fires — see `LatchSet`.
const TRIGGER_KILL: crate::latch_set::KeyTrigger =
    crate::latch_set::KeyTrigger::new(KeyCode::Char('X'), KeyModifiers::SHIFT);
/// Trigger for the long-snooze confirm latch (`Shift-Z`).
const TRIGGER_LONG_SNOOZE: crate::latch_set::KeyTrigger =
    crate::latch_set::KeyTrigger::new(KeyCode::Char('Z'), KeyModifiers::SHIFT);
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
    /// Repos the user has subscribed to in narrowed form (e.g.
    /// `tensorzero/tensorzero`) — fed from `selected_scopes` after
    /// trimming the `github:` prefix and dropping org-level entries.
    /// Used so a freshly-added repo gets a header in the sidebar
    /// even before polling finds any open PRs/issues under it.
    subscribed_repos: BTreeSet<String>,
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
                s.register(TRIGGER_KILL);
                s.register(TRIGGER_LONG_SNOOZE);
                s
            },
            short_snooze: pilot_config::UiDefaults::default().short_snooze,
            long_snooze: pilot_config::UiDefaults::default().long_snooze,
            agent_shortcuts,
            running_terminals: HashMap::new(),
            attention: pilot_config::AttentionConfig::default(),
            subscribed_repos: BTreeSet::new(),
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

    /// Replace the set of "subscribed repo names" the sidebar should
    /// render as empty headers when no workspaces exist under them.
    /// Inputs are scope ids like `github:owner/repo`; the prefix is
    /// stripped, and org-level entries (`github:owner` with no `/`)
    /// are skipped — those mean "whole org subscription" and the
    /// repo headers will materialize as polling finds them.
    pub fn apply_subscribed_scopes(&mut self, scopes: &BTreeSet<String>) {
        let mut out = BTreeSet::new();
        for id in scopes {
            // `provider:owner/repo` → "owner/repo". Skip if there's
            // no `/` after the prefix (org-level subscription).
            if let Some((_, rest)) = id.split_once(':')
                && rest.contains('/')
            {
                out.insert(rest.to_string());
            }
        }
        if out != self.subscribed_repos {
            self.subscribed_repos = out;
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
        if pr.has_conflicts {
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

    pub fn kill_armed(&self) -> Option<&SessionKey> {
        self.latches.armed(TRIGGER_KILL)
    }

    /// Two-press archive latch. First call returns `None` (arms the
    /// latch + paints the warning chrome); second call inside the
    /// latch window returns `Some(session_key)` and the caller fires
    /// the destructive `Kill`. Sessions other than the one armed
    /// reset the latch.
    ///
    /// Used by `Model::dispatch_action(Archive)` so the catalog
    /// path drives the same latch the inline Shift+X handler used
    /// to. Returns None when there's no workspace selected.
    pub fn arm_or_fire_archive(&mut self) -> Option<SessionKey> {
        let key = self.selected_session_key().cloned()?;
        if self.latches.arm_or_fire(TRIGGER_KILL, key.clone()) {
            Some(key)
        } else {
            None
        }
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
                subscribed_repos: &self.subscribed_repos,
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
        actions.push(Action::OpenSandbox);

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
            (KeyCode::Char('z'), KeyModifiers::NONE) => {
                // Toggle: snooze if not snoozed, otherwise unsnooze.
                // The resolver makes the decision based on
                // `workspace.snoozed_until`; this handler just
                // executes whichever Intent it returns.
                let now = chrono::Utc::now();
                let intent = crate::intent::resolve_short_snooze(
                    self.selected_workspace(),
                    now,
                    self.short_snooze,
                );
                match intent {
                    crate::intent::Intent::Snooze {
                        session_key,
                        duration,
                    } => {
                        let until = now
                            + chrono::Duration::from_std(duration)
                                .unwrap_or(chrono::Duration::hours(4));
                        cmds.push(Command::Snooze { session_key, until });
                    }
                    crate::intent::Intent::Unsnooze { session_key } => {
                        cmds.push(Command::Unsnooze { session_key });
                    }
                    _ => {}
                }
                PaneOutcome::Consumed
            }
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

    pub fn render(&mut self, area: Rect, frame: &mut Frame, focused: bool) {
        // V1-style header strip:
        //   row 0: PILOT  N  ● N new  ? N input  [7d]
        //   row 1: s  filter (needs:reply ci:failed ...)
        //   row 2: N CI  N review               (omitted when both 0)
        //   row 3: ── divider ────────────────
        //   row 4: blank
        //   row 5+: content
        let theme = crate::theme::current();
        let now = chrono::Utc::now();
        let mailbox_label = match self.mailbox {
            Mailbox::Inbox => "PILOT",
            Mailbox::Inactive => "INACTIVE",
            Mailbox::Snoozed => "SNOOZED",
        };
        let count = self.workspace_count();
        let unread = self.total_unread_count();
        let input_pending = self.input_pending_count();
        let ci_failing = self.ci_failing_count();
        let review_pending = self.review_pending_count();

        let l_pad: u16 = 1;
        let r_pad: u16 = 3;
        let inner_width = area.width.saturating_sub(l_pad + r_pad);

        // Row 0 — app title + counts.
        let mut header_spans: Vec<Span> = Vec::with_capacity(12);
        header_spans.push(Span::styled(mailbox_label, theme.title(focused)));
        header_spans.push(Span::raw("  "));
        header_spans.push(Span::styled(
            count.to_string(),
            Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
        ));
        if unread > 0 {
            header_spans.push(Span::raw("  "));
            header_spans.push(Span::styled(
                "● ",
                Style::default()
                    .fg(theme.hover)
                    .add_modifier(Modifier::BOLD),
            ));
            header_spans.push(Span::styled(
                format!("{unread} new"),
                Style::default()
                    .fg(theme.hover)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        if input_pending > 0 {
            header_spans.push(Span::raw("  "));
            header_spans.push(Span::styled(
                "? ",
                Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
            ));
            header_spans.push(Span::styled(
                format!("{input_pending} input"),
                Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
            ));
        }
        header_spans.push(Span::raw("  "));
        header_spans.push(Span::styled("[7d]", Style::default().fg(theme.text_dim)));

        let row0 = Rect::new(area.x + l_pad, area.y, inner_width, 1.min(area.height));
        frame.render_widget(Paragraph::new(Line::from(header_spans)), row0);

        // Row 1 — filter hint.
        if area.height >= 2 {
            let row1 = Rect::new(area.x + l_pad, area.y + 1, inner_width, 1);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(
                        "/ ",
                        Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        "filter (needs:reply ci:failed …)",
                        Style::default().fg(theme.text_dim),
                    ),
                ])),
                row1,
            );
        }

        // Row 2 — stats summary, only when there's something to summarize.
        let mut stats_spans: Vec<Span> = Vec::new();
        if ci_failing > 0 {
            stats_spans.push(Span::styled(
                ci_failing.to_string(),
                Style::default()
                    .fg(theme.error)
                    .add_modifier(Modifier::BOLD),
            ));
            stats_spans.push(Span::styled(" CI", Style::default().fg(theme.text_dim)));
        }
        if review_pending > 0 {
            if !stats_spans.is_empty() {
                stats_spans.push(Span::raw("  "));
            }
            stats_spans.push(Span::styled(
                review_pending.to_string(),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ));
            stats_spans.push(Span::styled(" review", Style::default().fg(theme.text_dim)));
        }
        if !stats_spans.is_empty() && area.height >= 3 {
            let row2 = Rect::new(area.x + l_pad, area.y + 2, inner_width, 1);
            frame.render_widget(Paragraph::new(Line::from(stats_spans)), row2);
        }

        // Row 3 — thin grey divider.
        if area.height >= 4 {
            let div_area = Rect::new(area.x + l_pad, area.y + 3, inner_width, 1);
            let divider = "─".repeat(div_area.width as usize);
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(divider, theme.divider()))),
                div_area,
            );
        }

        // Content starts at row 5 (skipping a blank row for breathing
        // room above the first item).
        const HEADER_HEIGHT: u16 = 5;
        let inner = Rect {
            x: area.x + l_pad,
            y: area.y + HEADER_HEIGHT,
            width: inner_width,
            height: area.height.saturating_sub(HEADER_HEIGHT),
        };

        let row_budget = inner_width as usize;
        // Pre-pass: compute the widest `#NNN` across visible workspace
        // rows so every row pads to the same column. Without this,
        // `#7204 R` and `#31 R` had different role-letter positions
        // and the whole column visibly jittered. Minimum 3 ("#NN")
        // so very-short numbers still leave space for a separator.
        let max_pr_num_width = self
            .visible
            .iter()
            .filter_map(|row| match row {
                VisibleRow::Workspace(k) => self
                    .workspaces
                    .get(k)
                    .and_then(|w| w.primary_task())
                    .and_then(crate::components::task_label::pr_number)
                    .map(|n| format!("#{n}").chars().count()),
                _ => None,
            })
            .max()
            .unwrap_or(3)
            .max(3);
        // Column spec for workspace rows — built once per render
        // (max_pr_num_width is fixed across rows in this pass).
        // Each row's `render_table` call reuses this slice; the
        // table primitive owns padding + cursor fill geometry.
        let workspace_columns = crate::components::workspace_row::build_columns(max_pr_num_width);
        let lines: Vec<Line> = self
            .visible
            .iter()
            .enumerate()
            .map(|(i, row)| match row {
                VisibleRow::RepoHeader(name) => {
                    use crate::components::icons;
                    let collapsed = self.collapsed_repos.contains(name);
                    let glyph = if collapsed { "▸" } else { "▾" };
                    let is_cursor = i == self.cursor;
                    let row_bg = if is_cursor && focused {
                        Some(theme.row_focused())
                    } else if is_cursor {
                        Some(theme.row_unfocused())
                    } else {
                        None
                    };
                    // Cursor caret on the left mirrors workspace rows so
                    // the user can see the cursor parked on a header
                    // (otherwise navigating onto a header looks like a
                    // dropped key — Space-to-toggle wouldn't be
                    // discoverable).
                    let caret = if is_cursor { "▸ " } else { "  " };
                    let glyph_style = match row_bg {
                        Some(bg) => bg,
                        None => Style::default().fg(theme.text_dim),
                    };
                    let mut spans: Vec<Span> = vec![
                        Span::styled(caret.to_string(), glyph_style),
                        Span::styled(format!("{glyph} "), glyph_style),
                        Span::styled(
                            format!("{} {}", icons::REPO, name),
                            row_bg
                                .unwrap_or_default()
                                .fg(theme.warn)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ];
                    if let Some(s) = self.repo_summaries.get(name) {
                        // Active count is redundant — the workspace
                        // rows are visible directly under the header,
                        // so the user can count them. The attention
                        // pill is the only summary that adds info
                        // (and only when non-zero). Two raw numbers
                        // side-by-side looked like a broken counter.
                        if s.attention > 0 {
                            spans.push(Span::styled(
                                format!("  ● {}", s.attention),
                                row_bg
                                    .unwrap_or_default()
                                    .fg(theme.hover)
                                    .add_modifier(Modifier::BOLD),
                            ));
                        }
                    }
                    let _ = row_budget;
                    Line::from(spans)
                }
                VisibleRow::Workspace(key) => {
                    use crate::components::workspace_row::{WorkspaceRowCtx, build_row};
                    let workspace = self.workspaces.get(key);
                    let ctx = WorkspaceRowCtx {
                        workspace,
                        task: workspace.and_then(|w| w.primary_task()),
                        theme,
                        now,
                        focused,
                        is_cursor: i == self.cursor,
                        max_pr_num_width,
                        kill_armed: self.latches.armed(TRIGGER_KILL) == Some(key),
                        long_snooze_armed: self.latches.armed(TRIGGER_LONG_SNOOZE) == Some(key),
                        asking: workspace.is_some_and(|w| {
                            crate::agent_attention::workspace_is_asking(w, &self.agents_asking)
                        }),
                        badges: self.runner_badges(key),
                    };
                    let row = build_row(&ctx);
                    crate::components::table::render_table(&[row], &workspace_columns, row_budget)
                        .into_iter()
                        .next()
                        .unwrap_or_default()
                }
                VisibleRow::Session {
                    workspace,
                    session_id,
                } => {
                    // Per-session sub-row, only emitted when the
                    // workspace has 2+ sessions. Indent further under
                    // the workspace row and show the session name.
                    let name = self
                        .workspaces
                        .get(workspace)
                        .and_then(|w| w.find_session(*session_id))
                        .map(|s| s.name.as_str())
                        .unwrap_or("?");
                    let is_cursor = i == self.cursor;
                    let style = if is_cursor && focused {
                        theme.row_focused()
                    } else if is_cursor {
                        theme.row_unfocused()
                    } else {
                        Style::default().fg(theme.text_dim)
                    };
                    let prefix = if is_cursor { "      ▸ " } else { "        " };
                    let name_budget = row_budget.saturating_sub(visual_width(prefix));
                    let name_text = truncate_ellipsis(name, name_budget);
                    let used = visual_width(prefix) + visual_width(&name_text);
                    let mut spans =
                        vec![Span::styled(prefix, style), Span::styled(name_text, style)];
                    if is_cursor && used < row_budget {
                        spans.push(Span::styled(" ".repeat(row_budget - used), style));
                    }
                    Line::from(spans)
                }
            })
            .collect();

        let para = Paragraph::new(lines);
        frame.render_widget(para, inner);
    }
}

/// Visible width of the status column. Sized for the longest pill
/// (` CONFLICT ` = 10 cells) so the time column always lands at the
/// same offset.
pub(crate) const STATUS_COL_W: usize = 10;
/// Visible width of the time column. `now`/`Xm`/`Xh`/`Xd`/`Xmo` all
/// fit in 4 cells (max is `12mo`).
pub(crate) const TIME_COL_W: usize = 4;
/// Visible width of the unread-pill column. ` ●99+` is the worst
/// case (5 cells). Rows without unread render 5 row-styled spaces
/// so the badge / status / time columns to its right don't shift.
pub(crate) const UNREAD_COL_W: usize = 5;
/// Visible width of the agent-badge column. Sized to fit the common
/// "one agent + one shell" case ( ` C ` + space + ` S ` = 7 cells);
/// a single badge gets right-aligned padding, more than two get
/// truncated. Rows without any badge render this many row-styled
/// spaces so the status / time columns to the right stay anchored.
pub(crate) const BADGE_COL_W: usize = 7;

/// Right-side status pill showing the most actionable problem on the
/// PR. One pill at a time, ordered by severity: merge conflict beats
/// CI failure beats CI mixed beats CI running beats CI ok beats
/// "behind base" beats nothing. The pill is a colored block
/// (` CONFLICT ` / ` CI FAIL ` / ` CI OK ` / etc.) with strong fg +
/// colored bg — the v1 design that the user actually likes; subtle
/// text-only failed visually.
pub(crate) struct StatusPill {
    pub(crate) label: &'static str,
    pub(crate) style: Style,
}

/// Does this workspace want the user's attention right now? Drives
/// the "needs attention" counter on the collapsed repo header.
/// Each signal (unread / CI / review / agent-asking / mentioned)
/// is independently toggleable via `~/.pilot/config.yaml::attention`.
/// Short type marker rendered before the `#number` on each workspace
/// row. Returns `Some("[PR]")` for workspaces holding a pull request,
/// `Some("[I]")` for issue-only workspaces, and `None` for empty
/// scratch workspaces (no PR, no issues — those have no number to
/// label anyway).
pub(crate) fn workspace_type_label(workspace: &Workspace) -> Option<&'static str> {
    // Pad to 4 cells so [PR] and [I ] line up column-wise — the
    // `#NNN` number after the label needs the same x position on
    // every row or the eye can't scan the column.
    if workspace.pr.is_some() {
        return Some("[PR]");
    }
    if !workspace.gh_issues.is_empty() || !workspace.linear_issues.is_empty() {
        return Some("[I ]");
    }
    None
}

// Prompt builders moved to `pilot_tui_core::prompts` (so `intent`,
// which also lives there, can call them without creating a dep
// cycle). Re-exported below at the original paths for back-compat.
pub use pilot_tui_core::prompts::{
    build_fix_ci_prompt, build_fix_conflict_prompt, build_work_prompt,
};

/// Pure predicate: does `workspace` belong in `mailbox` right now?
///
/// Single source of truth for the inbox / inactive / snoozed
/// classification. The body used to live inline in
/// `recompute_visible_inner`, where each branch was hand-rolled
/// and the snoozed-wins-over-merged subtlety wasn't covered by
/// any test. Pulling it out lets the test file exercise every
/// (workspace state, mailbox) cell directly.
///
/// Rules (snooze always wins — a snoozed workspace appears ONLY
/// in the Snoozed mailbox, never leaks into Inbox / Inactive):
///
/// - **Inbox**: not snoozed AND
///   (`show_inactive_in_inbox` OR the primary task is alive —
///   open / draft / in-progress / in-review). Empty workspaces
///   (no primary task at all) show in Inbox so the user can act
///   on them.
/// - **Inactive**: not snoozed AND primary task is `Merged` /
///   `Closed`.
/// - **Snoozed**: workspace is snoozed.
/// How long a freshly-merged/closed PR stays visible in the Inbox
/// before falling into the Inactive mailbox. The point: when a PR
/// merges between polls, the gh-provider's recently-merged sweep
/// catches it and updates `state=Merged`. Without this grace
/// window the row would IMMEDIATELY disappear from the Inbox view
/// the user was looking at — they'd never see the MERGED pill.
///
/// 30 minutes is enough to give pilot a poll cycle (or two) to
/// surface the state transition while the user is still around,
/// without permanently cluttering Inbox with completed work.
pub const INACTIVE_GRACE: chrono::Duration = chrono::Duration::minutes(30);

pub fn mailbox_membership(
    workspace: &Workspace,
    mailbox: Mailbox,
    now: chrono::DateTime<chrono::Utc>,
    show_inactive_in_inbox: bool,
) -> bool {
    let snoozed = workspace.is_snoozed(now);
    // "Recently inactivated" = task is Merged/Closed AND its
    // `updated_at` (which GitHub touches at merge/close time) is
    // within the grace window. Such workspaces appear in BOTH
    // Inbox (so the user sees the MERGED/CLOSED transition) and
    // Inactive (so they're already in their permanent home).
    let recently_inactivated = workspace
        .primary_task()
        .map(|t| {
            matches!(
                t.state,
                pilot_core::TaskState::Merged | pilot_core::TaskState::Closed
            ) && (now - t.updated_at) < INACTIVE_GRACE
        })
        .unwrap_or(false);
    match mailbox {
        Mailbox::Snoozed => snoozed,
        Mailbox::Inbox => {
            if snoozed {
                return false;
            }
            if show_inactive_in_inbox {
                return true;
            }
            match workspace.primary_task() {
                Some(t) => {
                    let is_terminal = matches!(
                        t.state,
                        pilot_core::TaskState::Merged | pilot_core::TaskState::Closed
                    );
                    !is_terminal || recently_inactivated
                }
                None => true,
            }
        }
        Mailbox::Inactive => {
            if snoozed {
                return false;
            }
            matches!(
                workspace.primary_task().map(|t| t.state),
                Some(pilot_core::TaskState::Merged) | Some(pilot_core::TaskState::Closed)
            )
        }
    }
}

/// One reason a workspace might want the user's attention. Single
/// vocabulary used by `workspace_attention_signals` (pure producer),
/// `workspace_needs_attention` (gated by config), and the per-
/// signal header counters (`Unread`/`AgentAsking`/`CiFailing`/…).
///
/// Adding a new signal means: add a variant here, add a producer
/// branch in `workspace_attention_signals`, add a config flag, and
/// — because the gate match below is exhaustive — the compiler
/// catches the missing wiring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AttentionSignal {
    Unread,
    AgentAsking,
    CiFailing,
    ReviewPending,
    Mentioned,
}

/// Pure: every attention signal currently active for `w`. Order is
/// stable (matches the producer below) so callers that want
/// priority-aware behavior can read the first hit.
///
/// **Single source of truth.** The per-repo `needs attention`
/// counter, the header signal totals (`? N input`, `N CI`,
/// `N review`), and the row pill all derive from this one
/// producer. Before unification, each consumer had its own ad-hoc
/// check: `review_pending_count` counted "reviewers requested"
/// (even without ChangesRequested), `workspace_needs_attention`
/// didn't — so a repo with reviewers-only PRs lit the header
/// counter but not the repo badge.
pub fn workspace_attention_signals(
    w: &Workspace,
    agents_asking: &std::collections::HashSet<SessionKey>,
) -> Vec<AttentionSignal> {
    let mut out = Vec::new();
    if w.unread_count() > 0 {
        out.push(AttentionSignal::Unread);
    }
    // AgentAsking signal: source of truth is the sidebar-local
    // `agents_asking` set (driven by `Event::AgentState` deltas).
    // NOT `w.sessions[i].state` — that gets blown away every poll
    // when `WorkspaceUpserted` re-loads from the persisted store.
    if crate::agent_attention::workspace_is_asking(w, agents_asking) {
        out.push(AttentionSignal::AgentAsking);
    }
    if let Some(t) = w.primary_task() {
        if matches!(
            t.ci,
            pilot_core::CiStatus::Failure | pilot_core::CiStatus::Mixed
        ) {
            out.push(AttentionSignal::CiFailing);
        }
        // ReviewPending unifies: explicit ReviewStatus + reviewers
        // requested. The previous split (header counter had the
        // `reviewers.is_empty()` extra, attention badge didn't) led
        // to "1 review" in the header next to a repo header with no
        // attention dot — confusing.
        if matches!(
            t.review,
            pilot_core::ReviewStatus::Pending | pilot_core::ReviewStatus::ChangesRequested,
        ) || !t.reviewers.is_empty()
        {
            out.push(AttentionSignal::ReviewPending);
        }
        if matches!(t.role, pilot_core::TaskRole::Mentioned) {
            out.push(AttentionSignal::Mentioned);
        }
    }
    out
}

/// Is `signal` enabled in the user's attention config? Exhaustive
/// match so a new `AttentionSignal` variant fails to compile until
/// it's wired up here AND in `AttentionConfig`.
fn attention_gate(signal: AttentionSignal, cfg: &pilot_config::AttentionConfig) -> bool {
    match signal {
        AttentionSignal::Unread => cfg.unread,
        AttentionSignal::AgentAsking => cfg.agent_asking,
        AttentionSignal::CiFailing => cfg.ci_failing,
        AttentionSignal::ReviewPending => cfg.review_pending,
        AttentionSignal::Mentioned => cfg.mentioned,
    }
}

pub(crate) fn workspace_needs_attention(
    w: &Workspace,
    cfg: &pilot_config::AttentionConfig,
    agents_asking: &std::collections::HashSet<SessionKey>,
) -> bool {
    workspace_attention_signals(w, agents_asking)
        .iter()
        .any(|s| attention_gate(*s, cfg))
}

/// Render the right-trailer pill for a task. **Pure mapping** from
/// `StatusTag::for_task(task)` — no priority logic lives here, all
/// of that is in `pilot_core::task::StatusTag::for_task`. Adding a
/// new visual state means adding a `StatusTag` variant first; the
/// match below is exhaustive, so the compiler then catches the
/// missing pill arm.
///
/// Returning `Option<StatusPill>` so the `StatusTag::None` case
/// renders as a hole (no pill) without making every caller filter.
pub(crate) fn status_pill(task: &pilot_core::Task) -> Option<StatusPill> {
    pill_for_tag(pilot_core::StatusTag::for_task(task))
}

/// Pure tag → pill mapping. Exists as its own function so the
/// contract tests (`status_pill_consistency_tests`) can pin every
/// `(StatusTag, StatusPill)` pair without going through a
/// constructed `Task`.
fn pill_for_tag(tag: pilot_core::StatusTag) -> Option<StatusPill> {
    use pilot_core::StatusTag::*;
    let theme = crate::theme::current();
    // Indexed palette colors render as the terminal's "bright"
    // red/yellow on most setups — punchy without the muddy mid-red
    // `Color::Red` produces on dark themes. Black-on-color reads
    // cleaner than white-on-color at this size.
    let pill_red = Style::default()
        .bg(Color::Indexed(196))
        .fg(Color::Black)
        .add_modifier(Modifier::BOLD);
    let pill_amber = Style::default()
        .bg(Color::Indexed(214))
        .fg(Color::Black)
        .add_modifier(Modifier::BOLD);
    let pill_yellow = Style::default()
        .bg(Color::Indexed(220))
        .fg(Color::Black)
        .add_modifier(Modifier::BOLD);
    let pill_green = Style::default()
        .bg(Color::Indexed(40))
        .fg(Color::Black)
        .add_modifier(Modifier::BOLD);
    let pill = |label: &'static str, style: Style| Some(StatusPill { label, style });
    match tag {
        Merged => pill(
            " MERGED   ",
            Style::default()
                .bg(theme.hover)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        ),
        Closed => pill(
            " CLOSED   ",
            Style::default()
                .bg(theme.error)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        ),
        Conflict => pill(" CONFLICT ", pill_red),
        CiFailed => pill(" CI FAIL  ", pill_red),
        CiMixed => pill(" CI MIX   ", pill_amber),
        ChangesRequested => pill(" CHANGES  ", pill_red),
        Queued => pill(" QUEUED   ", pill_green),
        Draft => pill(
            " DRAFT    ",
            Style::default()
                .bg(theme.chrome)
                .fg(theme.text_strong)
                .add_modifier(Modifier::BOLD),
        ),
        Ready => pill(" READY    ", pill_green),
        Approved => pill(
            " APPROVED ",
            Style::default()
                .bg(theme.accent)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        ),
        AutoMerge => pill(
            " AUTO     ",
            Style::default()
                .bg(theme.accent)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        ),
        ReviewPending => pill(
            " REVIEW   ",
            Style::default()
                .bg(theme.warn)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        ),
        CiRunning => pill(" CI RUN   ", pill_yellow),
        CiOk => pill(" CI OK    ", pill_green),
        Behind => pill(
            " BEHIND   ",
            Style::default()
                .fg(theme.text_dim)
                .add_modifier(Modifier::BOLD),
        ),
        None => Option::None,
    }
}

/// Compact relative time for the right-side trailer. `now` < 1m → "now",
/// < 1h → `Xm`, < 24h → `Xh`, < 30d → `Xd`, else `Xmo`. Always 2-3
/// cells so the column lines up.
pub(crate) fn relative_time(
    then: chrono::DateTime<chrono::Utc>,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    let delta = now.signed_duration_since(then);
    let secs = delta.num_seconds().max(0);
    if secs < 60 {
        return "now".into();
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h");
    }
    let days = hours / 24;
    if days < 30 {
        return format!("{days}d");
    }
    let months = days / 30;
    format!("{months}mo")
}

/// Single-letter role marker + color for the workspace row. Renders
/// as a leading badge (`A` for author, `R` for reviewer, `@` for
/// assignee, dim `·` for mentioned/done) so the user can scan the
/// inbox and pick out "PRs I have to act on" without reading titles.
///
/// All colors come from the active theme — no hardcoded RGB — so
/// the badges sit on the same palette as the rest of the UI and
/// don't fight for attention.
pub(crate) fn role_badge(theme: &crate::theme::Theme, role: pilot_core::TaskRole) -> (char, Color) {
    match role {
        pilot_core::TaskRole::Author => ('A', theme.success),
        pilot_core::TaskRole::Reviewer => ('R', theme.accent),
        pilot_core::TaskRole::Assignee => ('@', theme.warn),
        pilot_core::TaskRole::Mentioned => ('·', theme.text_dim),
    }
}

/// Per-letter pill style for the runner badge. Subtle bg tint (chrome
/// grey, not a saturated accent) + colored bold fg so the pills read
/// clearly without competing with status colors elsewhere on the row.
pub(crate) fn badge_pill_style(theme: &crate::theme::Theme, letter: char) -> Style {
    let fg = match letter {
        'C' => theme.accent,
        'X' => theme.warn,
        'U' => theme.success,
        'S' => theme.text_strong,
        _ => theme.text_strong,
    };
    Style::default()
        .bg(theme.fill)
        .fg(fg)
        .add_modifier(Modifier::BOLD)
}

/// Visual width in cells of a string, treating each char as one cell.
/// PR titles in the sidebar are ASCII in practice; the prefixes use
/// `▸` (1-cell box-drawing) which `chars().count()` gets right.
fn visual_width(s: &str) -> usize {
    s.chars().count()
}

/// Truncate `s` so it fits in `budget` cells, adding `…` when clipped.
/// Returns `s` unchanged when it already fits (cheap fast-path).
pub(crate) fn truncate_ellipsis(s: &str, budget: usize) -> String {
    let w = visual_width(s);
    if w <= budget {
        return s.to_string();
    }
    if budget == 0 {
        return String::new();
    }
    if budget == 1 {
        return "…".to_string();
    }
    // Take `budget - 1` chars, append the ellipsis. Iterating chars
    // (not bytes) keeps multi-byte UTF-8 intact.
    let mut out: String = s.chars().take(budget - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod truncate_tests {
    use super::truncate_ellipsis;

    #[test]
    fn fits_unchanged() {
        assert_eq!(truncate_ellipsis("hello", 10), "hello");
        assert_eq!(truncate_ellipsis("hello", 5), "hello");
    }

    #[test]
    fn clips_with_ellipsis() {
        assert_eq!(truncate_ellipsis("hello world", 8), "hello w…");
    }

    #[test]
    fn zero_and_one_budgets() {
        assert_eq!(truncate_ellipsis("hello", 0), "");
        assert_eq!(truncate_ellipsis("hello", 1), "…");
    }

    #[test]
    fn handles_multibyte() {
        // Characters are kept whole (no byte-slicing into UTF-8).
        let s = "naïve résumé";
        let out = truncate_ellipsis(s, 6);
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().count(), 6);
    }
}

#[cfg(test)]
mod status_pill_tests {
    use super::status_pill;
    use pilot_core::{CiStatus, ReviewStatus, Task, TaskId, TaskRole, TaskState};

    pub(super) fn base_task() -> Task {
        Task {
            id: TaskId {
                source: "gh".into(),
                key: "o/r#1".into(),
            },
            title: "t".into(),
            body: None,
            state: TaskState::Open,
            role: TaskRole::Author,
            ci: CiStatus::None,
            review: ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: "u".into(),
            repo: Some("o/r".into()),
            branch: Some("b".into()),
            base_branch: None,
            updated_at: chrono::Utc::now(),
            labels: vec![],
            reviewers: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            has_conflicts: false,
            is_behind_base: false,
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            closes_issues: vec![],
        }
    }

    /// All pill labels render in a fixed 10-cell column so the time
    /// column lines up across rows. Regression-guard the width.
    #[test]
    fn every_pill_label_is_ten_cells_wide() {
        let ci_cases: &[CiStatus] = &[
            CiStatus::Failure,
            CiStatus::Mixed,
            CiStatus::Running,
            CiStatus::Pending,
            CiStatus::Success,
        ];
        for ci in ci_cases {
            let mut t = base_task();
            t.ci = *ci;
            let pill = status_pill(&t).expect("CI status should produce a pill");
            assert_eq!(
                pill.label.chars().count(),
                10,
                "label {:?} for {:?} is not 10 cells wide",
                pill.label,
                ci,
            );
        }
        let state_cases: &[TaskState] = &[TaskState::Draft, TaskState::Merged, TaskState::Closed];
        for state in state_cases {
            let mut t = base_task();
            t.state = *state;
            let pill = status_pill(&t).expect("state should produce a pill");
            assert_eq!(
                pill.label.chars().count(),
                10,
                "label {:?} for {:?} is not 10 cells wide",
                pill.label,
                state,
            );
        }
        // Approval pills.
        for ci in [CiStatus::Success, CiStatus::Running] {
            let mut t = base_task();
            t.review = ReviewStatus::Approved;
            t.ci = ci;
            let pill = status_pill(&t).expect("approval should produce a pill");
            assert_eq!(
                pill.label.chars().count(),
                10,
                "label {:?} for approval + {:?} is not 10 cells wide",
                pill.label,
                ci,
            );
        }
    }

    #[test]
    fn ci_failure_renders_ci_fail() {
        let mut t = base_task();
        t.ci = CiStatus::Failure;
        assert_eq!(status_pill(&t).unwrap().label, " CI FAIL  ");
    }

    #[test]
    fn ci_success_renders_ci_ok() {
        // New behaviour: CI passing now renders an explicit green
        // ` CI OK    ` pill instead of an empty status column.
        let mut t = base_task();
        t.ci = CiStatus::Success;
        let pill = status_pill(&t).expect("Success should produce a pill");
        assert_eq!(pill.label, " CI OK    ");
    }

    #[test]
    fn ci_running_renders_ci_run() {
        // New behaviour: Running was previously a barely-visible amber
        // fg `       CI `. Now it renders a yellow-bg ` CI RUN   ` pill
        // matching the FAIL / MIX styling so users actually see it.
        let mut t = base_task();
        t.ci = CiStatus::Running;
        assert_eq!(status_pill(&t).unwrap().label, " CI RUN   ");
        t.ci = CiStatus::Pending;
        assert_eq!(status_pill(&t).unwrap().label, " CI RUN   ");
    }

    #[test]
    fn ci_mixed_renders_ci_mix() {
        let mut t = base_task();
        t.ci = CiStatus::Mixed;
        assert_eq!(status_pill(&t).unwrap().label, " CI MIX   ");
    }

    #[test]
    fn conflicts_trump_ci_status() {
        let mut t = base_task();
        t.has_conflicts = true;
        t.ci = CiStatus::Success;
        assert_eq!(status_pill(&t).unwrap().label, " CONFLICT ");
    }

    #[test]
    fn merged_renders_merged_pill_overriding_ci() {
        // A closed PR's CI history is frozen; the user can't act on
        // it. Show the inactive-state badge instead of a stale
        // CI FAIL.
        let mut t = base_task();
        t.state = TaskState::Merged;
        t.ci = CiStatus::Failure;
        assert_eq!(status_pill(&t).unwrap().label, " MERGED   ");
    }

    #[test]
    fn closed_renders_closed_pill_overriding_ci() {
        let mut t = base_task();
        t.state = TaskState::Closed;
        t.ci = CiStatus::Failure;
        assert_eq!(status_pill(&t).unwrap().label, " CLOSED   ");
    }

    #[test]
    fn draft_renders_draft_pill_when_ci_is_quiet() {
        // CI green or running, state Draft → DRAFT wins so the user
        // remembers the PR isn't ready for review.
        let mut t = base_task();
        t.state = TaskState::Draft;
        t.ci = CiStatus::Success;
        assert_eq!(status_pill(&t).unwrap().label, " DRAFT    ");
    }

    #[test]
    fn ci_failure_beats_draft() {
        // A draft with red CI still needs the user's attention more
        // urgently than the draft state itself — CI FAIL wins.
        let mut t = base_task();
        t.state = TaskState::Draft;
        t.ci = CiStatus::Failure;
        assert_eq!(status_pill(&t).unwrap().label, " CI FAIL  ");
    }

    #[test]
    fn ci_none_with_no_conflicts_renders_no_pill() {
        let t = base_task();
        assert!(status_pill(&t).is_none());
    }

    #[test]
    fn approved_plus_green_ci_renders_ready() {
        // The "this is mergeable right now" signal — both the human
        // half (review) and the machine half (CI) are done.
        let mut t = base_task();
        t.review = ReviewStatus::Approved;
        t.ci = CiStatus::Success;
        assert_eq!(status_pill(&t).unwrap().label, " READY    ");
    }

    #[test]
    fn approved_with_no_ci_yet_still_renders_ready() {
        // Some repos don't run CI on every PR (or the rollup is still
        // empty after a fresh push). Approval alone is enough to call
        // it READY rather than holding back forever.
        let mut t = base_task();
        t.review = ReviewStatus::Approved;
        t.ci = CiStatus::None;
        assert_eq!(status_pill(&t).unwrap().label, " READY    ");
    }

    #[test]
    fn approved_with_running_ci_renders_approved() {
        // Human approval landed; CI is still chewing. The user can
        // safely walk away — once green, the PR is mergeable.
        let mut t = base_task();
        t.review = ReviewStatus::Approved;
        t.ci = CiStatus::Running;
        assert_eq!(status_pill(&t).unwrap().label, " APPROVED ");
    }

    #[test]
    fn ci_failure_overrides_approval() {
        // Approval is great but red CI still trumps — that's the
        // actionable problem.
        let mut t = base_task();
        t.review = ReviewStatus::Approved;
        t.ci = CiStatus::Failure;
        assert_eq!(status_pill(&t).unwrap().label, " CI FAIL  ");
    }
}

#[cfg(test)]
mod status_pill_consistency_tests {
    //! The renderer (`status_pill` → `pill_for_tag`) is a pure
    //! mapping from `StatusTag::for_task`. These tests pin the
    //! contract:
    //!
    //! - Every non-`None` tag produces `Some(pill)`. A missing arm
    //!   would mean the rendered row silently drops a real signal
    //!   (the bug that motivated this audit: status_pill used to
    //!   skip `ChangesRequested` / `Queued` / `AutoMerge` / `ReviewPending`
    //!   entirely).
    //!
    //! - Every pill label is the same 10-cell width so the time
    //!   column stays right-aligned across rows.
    //!
    //! - The `None` tag is the only tag that renders no pill.
    //!
    //! Adding a new `StatusTag` variant without a `pill_for_tag`
    //! arm is a compile error; adding a new arm without these
    //! tests catching it is the gap this module closes.

    use super::status_pill_tests::base_task;
    use super::{pill_for_tag, status_pill};
    use pilot_core::{CiStatus, ReviewStatus, StatusTag, TaskState};

    /// Every variant of `StatusTag` the contract sweeps over. Keep
    /// this list exhaustive — a new variant on `StatusTag` should
    /// fail to compile here until added, because the match below
    /// is exhaustive. (The `let _: () = match` is the
    /// exhaustiveness pin.)
    const ALL_TAGS: &[StatusTag] = &[
        StatusTag::Merged,
        StatusTag::Closed,
        StatusTag::Conflict,
        StatusTag::CiFailed,
        StatusTag::CiMixed,
        StatusTag::ChangesRequested,
        StatusTag::Queued,
        StatusTag::Draft,
        StatusTag::Ready,
        StatusTag::Approved,
        StatusTag::AutoMerge,
        StatusTag::ReviewPending,
        StatusTag::CiRunning,
        StatusTag::CiOk,
        StatusTag::Behind,
        StatusTag::None,
    ];

    #[test]
    fn all_tags_list_is_exhaustive() {
        // Compile-time exhaustiveness pin. If a new `StatusTag`
        // variant lands without being added to `ALL_TAGS`, this
        // arm-by-arm match stops compiling — forcing the
        // contributor to extend the sweep below at the same time.
        for tag in ALL_TAGS {
            let _: () = match tag {
                StatusTag::Merged => (),
                StatusTag::Closed => (),
                StatusTag::Conflict => (),
                StatusTag::CiFailed => (),
                StatusTag::CiMixed => (),
                StatusTag::ChangesRequested => (),
                StatusTag::Queued => (),
                StatusTag::Draft => (),
                StatusTag::Ready => (),
                StatusTag::Approved => (),
                StatusTag::AutoMerge => (),
                StatusTag::ReviewPending => (),
                StatusTag::CiRunning => (),
                StatusTag::CiOk => (),
                StatusTag::Behind => (),
                StatusTag::None => (),
            };
        }
    }

    #[test]
    fn every_non_none_tag_renders_a_pill() {
        // No tag (except None) should silently drop. This was the
        // original bug: status_pill skipped CHANGES / QUEUED /
        // AUTO / REVIEW entirely, so a PR with changes requested
        // showed no signal in the trailer.
        for tag in ALL_TAGS {
            let pill = pill_for_tag(*tag);
            match tag {
                StatusTag::None => assert!(
                    pill.is_none(),
                    "StatusTag::None must render no pill, got {:?}",
                    pill.map(|p| p.label),
                ),
                other => assert!(pill.is_some(), "StatusTag::{other:?} must render a pill"),
            }
        }
    }

    #[test]
    fn every_pill_label_is_ten_cells_wide() {
        // The right-trailer reserves 10 cells for the status pill
        // so the time column stays aligned. Width is checked here
        // for every tag, not just the ones reachable from a Task —
        // the renderer is the truth, the producer is the input.
        for tag in ALL_TAGS {
            if let Some(p) = pill_for_tag(*tag) {
                assert_eq!(
                    p.label.chars().count(),
                    10,
                    "StatusTag::{tag:?} label {:?} is not 10 cells wide",
                    p.label,
                );
            }
        }
    }

    #[test]
    fn changes_requested_now_renders_a_pill() {
        // Regression for the original bug: a PR with
        // ReviewStatus::ChangesRequested and no other CI/conflict
        // signal used to fall through to None and show no pill.
        let mut t = base_task();
        t.review = ReviewStatus::ChangesRequested;
        let pill = status_pill(&t).expect("changes-requested must produce a pill");
        assert_eq!(pill.label, " CHANGES  ");
    }

    #[test]
    fn auto_merge_now_renders_a_pill() {
        // Same bug class: auto_merge_enabled with no other signal
        // used to produce no pill. Now renders AUTO.
        let mut t = base_task();
        t.auto_merge_enabled = true;
        let pill = status_pill(&t).expect("auto-merge must produce a pill");
        assert_eq!(pill.label, " AUTO     ");
    }

    #[test]
    fn queued_now_renders_a_pill() {
        let mut t = base_task();
        t.is_in_merge_queue = true;
        let pill = status_pill(&t).expect("in-merge-queue must produce a pill");
        assert_eq!(pill.label, " QUEUED   ");
    }

    #[test]
    fn review_pending_now_renders_a_pill() {
        let mut t = base_task();
        t.review = ReviewStatus::Pending;
        let pill = status_pill(&t).expect("review-pending must produce a pill");
        assert_eq!(pill.label, " REVIEW   ");
    }

    #[test]
    fn task_pill_matches_tag_priority() {
        // Sanity-check the pipeline: for a handful of (task) inputs
        // the pill rendered must match the pill mapped from the
        // tag computed by `StatusTag::for_task`. Catches drift if
        // someone reintroduces priority logic into `pill_for_tag`.
        let mut cases: Vec<pilot_core::Task> = Vec::new();
        cases.push({
            let mut t = base_task();
            t.has_conflicts = true;
            t
        });
        cases.push({
            let mut t = base_task();
            t.state = TaskState::Draft;
            t.review = ReviewStatus::Approved;
            t.ci = CiStatus::Success;
            t
        });
        cases.push({
            let mut t = base_task();
            t.state = TaskState::Merged;
            t
        });
        cases.push({
            let mut t = base_task();
            t.review = ReviewStatus::Approved;
            t.ci = CiStatus::Running;
            t
        });
        for t in &cases {
            let via_task = status_pill(t).map(|p| p.label);
            let via_tag = pill_for_tag(StatusTag::for_task(t)).map(|p| p.label);
            assert_eq!(
                via_task, via_tag,
                "status_pill must equal pill_for_tag(StatusTag::for_task(task))",
            );
        }
    }
}

#[cfg(test)]
mod workspace_type_label_tests {
    use super::*;
    use pilot_core::{Workspace, WorkspaceKey};

    fn empty_ws() -> Workspace {
        Workspace::empty(WorkspaceKey::new("k"), "main", chrono::Utc::now())
    }

    fn task(url: &str) -> pilot_core::Task {
        let mut t = status_pill_tests::base_task();
        t.url = url.into();
        t
    }

    #[test]
    fn pr_workspace_returns_pr_label() {
        let mut w = empty_ws();
        w.attach_task(task("https://github.com/o/r/pull/1"));
        assert_eq!(workspace_type_label(&w), Some("[PR]"));
    }

    #[test]
    fn issue_workspace_returns_i_label() {
        let mut w = empty_ws();
        w.attach_task(task("https://github.com/o/r/issues/42"));
        assert_eq!(workspace_type_label(&w), Some("[I ]"));
    }

    #[test]
    fn pr_workspace_with_linked_issue_still_labels_pr() {
        // Merged via closingIssuesReferences: workspace has both a
        // PR slot and a gh_issue. PR is the primary identity.
        let mut w = empty_ws();
        w.attach_task(task("https://github.com/o/r/pull/1"));
        w.attach_task(task("https://github.com/o/r/issues/42"));
        assert_eq!(workspace_type_label(&w), Some("[PR]"));
    }

    #[test]
    fn empty_workspace_returns_none() {
        let w = empty_ws();
        assert_eq!(workspace_type_label(&w), None);
    }
}

#[cfg(test)]
mod mailbox_membership_tests {
    //! Cell tests for the `mailbox_membership` predicate. The
    //! filter used to live inline in `recompute_visible_inner` with
    //! the snoozed-merged interaction untested — exactly the kind
    //! of state-cell drift the user has been pushing back on.
    //! Each `(workspace state, mailbox)` cell gets one assertion;
    //! a new mailbox semantic is one helper + ~6 assertions.

    use super::{Mailbox, mailbox_membership};
    use chrono::{Duration, Utc};
    use pilot_core::{TaskState, Workspace, WorkspaceKey};

    fn ws(state: Option<TaskState>) -> Workspace {
        ws_with_updated_at(state, Utc::now() - Duration::hours(2))
    }

    /// Build a workspace with an explicit `updated_at` so the
    /// grace-window tests can pin both ends (within grace = shown
    /// in Inbox; outside grace = not shown).
    ///
    /// Default `ws()` uses `now - 2h` so it's OUTSIDE the 30-min
    /// grace — most tests don't want the grace path to fire and
    /// would otherwise need to re-specify updated_at every time.
    fn ws_with_updated_at(
        state: Option<TaskState>,
        updated_at: chrono::DateTime<chrono::Utc>,
    ) -> Workspace {
        let now = Utc::now();
        let mut w = Workspace::empty(WorkspaceKey::new("k"), "main", now);
        if let Some(s) = state {
            let mut task = super::status_pill_tests::base_task();
            task.state = s;
            task.updated_at = updated_at;
            task.url = "https://github.com/o/r/pull/1".into();
            w.attach_task(task);
        }
        w
    }

    fn snoozed(mut w: Workspace) -> Workspace {
        w.snoozed_until = Some(Utc::now() + Duration::hours(1));
        w
    }

    // ── Inbox ────────────────────────────────────────────────────

    #[test]
    fn open_pr_is_in_inbox() {
        let w = ws(Some(TaskState::Open));
        assert!(mailbox_membership(&w, Mailbox::Inbox, Utc::now(), false));
    }

    #[test]
    fn draft_pr_is_in_inbox() {
        let w = ws(Some(TaskState::Draft));
        assert!(mailbox_membership(&w, Mailbox::Inbox, Utc::now(), false));
    }

    #[test]
    fn merged_pr_is_not_in_inbox_by_default() {
        let w = ws(Some(TaskState::Merged));
        assert!(!mailbox_membership(&w, Mailbox::Inbox, Utc::now(), false));
    }

    #[test]
    fn closed_pr_is_not_in_inbox_by_default() {
        let w = ws(Some(TaskState::Closed));
        assert!(!mailbox_membership(&w, Mailbox::Inbox, Utc::now(), false));
    }

    #[test]
    fn merged_pr_is_in_inbox_when_show_inactive_in_inbox_is_on() {
        let w = ws(Some(TaskState::Merged));
        assert!(mailbox_membership(&w, Mailbox::Inbox, Utc::now(), true));
    }

    #[test]
    fn freshly_merged_pr_stays_in_inbox_during_grace_window() {
        // User watches a PR merge between polls. The
        // recently-merged sweep brings it back with state=Merged.
        // The grace window (INACTIVE_GRACE) keeps it visible in
        // Inbox so the user sees the MERGED pill instead of the
        // row vanishing on their next refresh.
        let now = Utc::now();
        let w = ws_with_updated_at(
            Some(TaskState::Merged),
            now - Duration::minutes(5), // well inside the 30-min grace
        );
        assert!(
            mailbox_membership(&w, Mailbox::Inbox, now, false),
            "merged within grace must stay visible in Inbox",
        );
        assert!(
            mailbox_membership(&w, Mailbox::Inactive, now, false),
            "and is also in Inactive — its permanent home",
        );
    }

    #[test]
    fn freshly_closed_pr_stays_in_inbox_during_grace_window() {
        let now = Utc::now();
        let w = ws_with_updated_at(Some(TaskState::Closed), now - Duration::minutes(10));
        assert!(mailbox_membership(&w, Mailbox::Inbox, now, false));
    }

    #[test]
    fn merged_pr_past_grace_window_falls_out_of_inbox() {
        // 2 hours after merge: the row belongs in Inactive only.
        let now = Utc::now();
        let w = ws_with_updated_at(Some(TaskState::Merged), now - Duration::hours(2));
        assert!(!mailbox_membership(&w, Mailbox::Inbox, now, false));
        assert!(mailbox_membership(&w, Mailbox::Inactive, now, false));
    }

    #[test]
    fn empty_workspace_is_in_inbox() {
        let w = ws(None);
        assert!(mailbox_membership(&w, Mailbox::Inbox, Utc::now(), false));
    }

    // ── Inactive ─────────────────────────────────────────────────

    #[test]
    fn merged_pr_is_in_inactive() {
        let w = ws(Some(TaskState::Merged));
        assert!(mailbox_membership(&w, Mailbox::Inactive, Utc::now(), false));
    }

    #[test]
    fn closed_pr_is_in_inactive() {
        let w = ws(Some(TaskState::Closed));
        assert!(mailbox_membership(&w, Mailbox::Inactive, Utc::now(), false));
    }

    #[test]
    fn open_pr_is_not_in_inactive() {
        let w = ws(Some(TaskState::Open));
        assert!(!mailbox_membership(
            &w,
            Mailbox::Inactive,
            Utc::now(),
            false
        ));
    }

    #[test]
    fn empty_workspace_is_not_in_inactive() {
        let w = ws(None);
        assert!(!mailbox_membership(
            &w,
            Mailbox::Inactive,
            Utc::now(),
            false
        ));
    }

    // ── Snoozed wins over everything ─────────────────────────────

    #[test]
    fn snoozed_open_pr_is_only_in_snoozed() {
        let w = snoozed(ws(Some(TaskState::Open)));
        assert!(!mailbox_membership(&w, Mailbox::Inbox, Utc::now(), false));
        assert!(!mailbox_membership(
            &w,
            Mailbox::Inactive,
            Utc::now(),
            false
        ));
        assert!(mailbox_membership(&w, Mailbox::Snoozed, Utc::now(), false));
    }

    #[test]
    fn snoozed_merged_pr_is_only_in_snoozed_not_inactive() {
        // The exact failure mode the audit called out: a merged-AND-
        // snoozed PR must NOT leak into Inactive. Snoozed wins.
        let w = snoozed(ws(Some(TaskState::Merged)));
        assert!(!mailbox_membership(
            &w,
            Mailbox::Inactive,
            Utc::now(),
            false
        ));
        assert!(mailbox_membership(&w, Mailbox::Snoozed, Utc::now(), false));
    }

    #[test]
    fn snoozed_merged_pr_is_not_in_inbox_even_with_show_inactive() {
        // `show_inactive_in_inbox` flips merged → Inbox, but snooze
        // still wins over that.
        let w = snoozed(ws(Some(TaskState::Merged)));
        assert!(!mailbox_membership(&w, Mailbox::Inbox, Utc::now(), true));
    }

    #[test]
    fn unsnoozed_open_pr_is_not_in_snoozed() {
        let w = ws(Some(TaskState::Open));
        assert!(!mailbox_membership(&w, Mailbox::Snoozed, Utc::now(), false));
    }
}

#[cfg(test)]
mod attention_signal_tests {
    //! Single-source-of-truth contract: every "needs attention"
    //! signal flows through `workspace_attention_signals`. The
    //! per-repo badge (`workspace_needs_attention`) and the header
    //! counters (`input_pending_count` / `ci_failing_count` /
    //! `review_pending_count`) used to compute their own predicates
    //! and drifted — a workspace with reviewers requested but no
    //! ChangesRequested/Pending status used to bump the `N review`
    //! header counter but NOT the repo attention dot. Now both
    //! read the same signals.

    use super::status_pill_tests::base_task;
    use super::*;
    use pilot_core::{ReviewStatus, TaskRole, Workspace};

    fn ws_from_pr(mut task: pilot_core::Task) -> Workspace {
        // The classifier slots tasks based on URL — `/pull/N` lands in
        // the PR slot, everything else falls through to gh_issues.
        // Force a PR URL so `primary_task` returns this task.
        if !task.url.contains("/pull/") {
            task.url = "https://github.com/o/r/pull/1".into();
        }
        Workspace::from_task(task, chrono::Utc::now())
    }

    fn empty_set() -> std::collections::HashSet<SessionKey> {
        std::collections::HashSet::new()
    }

    fn set_with(ws: &Workspace) -> std::collections::HashSet<SessionKey> {
        let mut s = std::collections::HashSet::new();
        s.insert(SessionKey::from(&ws.key));
        s
    }

    #[test]
    fn no_signals_when_quiet() {
        // Plain open PR, no review, no CI, no unread: no signals.
        let w = ws_from_pr(base_task());
        assert!(workspace_attention_signals(&w, &empty_set()).is_empty());
    }

    #[test]
    fn ci_failure_emits_ci_failing_signal() {
        let mut t = base_task();
        t.ci = pilot_core::CiStatus::Failure;
        let w = ws_from_pr(t);
        assert!(
            workspace_attention_signals(&w, &empty_set()).contains(&AttentionSignal::CiFailing),
        );
    }

    #[test]
    fn ci_mixed_also_emits_ci_failing_signal() {
        // CI Mixed is a "partial failure" — treated the same as
        // Failure for attention purposes.
        let mut t = base_task();
        t.ci = pilot_core::CiStatus::Mixed;
        let w = ws_from_pr(t);
        assert!(
            workspace_attention_signals(&w, &empty_set()).contains(&AttentionSignal::CiFailing),
        );
    }

    #[test]
    fn reviewers_requested_emits_review_signal_even_without_pending_status() {
        let mut t = base_task();
        t.review = ReviewStatus::None;
        t.reviewers = vec!["alice".into()];
        let w = ws_from_pr(t);
        assert!(
            workspace_attention_signals(&w, &empty_set()).contains(&AttentionSignal::ReviewPending),
        );
    }

    #[test]
    fn changes_requested_emits_review_signal() {
        let mut t = base_task();
        t.review = ReviewStatus::ChangesRequested;
        let w = ws_from_pr(t);
        assert!(
            workspace_attention_signals(&w, &empty_set()).contains(&AttentionSignal::ReviewPending),
        );
    }

    #[test]
    fn mentioned_role_emits_mentioned_signal() {
        let mut t = base_task();
        t.role = TaskRole::Mentioned;
        let w = ws_from_pr(t);
        assert!(
            workspace_attention_signals(&w, &empty_set()).contains(&AttentionSignal::Mentioned),
        );
    }

    #[test]
    fn agent_asking_signal_comes_from_asking_set_not_workspace_sessions() {
        // Regression for the silent-clobber bug fixed in this
        // commit: the AgentAsking signal MUST be driven by the
        // sidebar-local `agents_asking` set, NOT
        // `Workspace.sessions[i].state`. The poll cycle reloads
        // workspace data from store every minute, which would
        // wipe a state-mutation-based signal.
        let w = ws_from_pr(base_task());

        // No entry in the set → no signal even if sessions claim
        // Asking (in production they never do, but the test pins
        // the contract).
        assert!(
            !workspace_attention_signals(&w, &empty_set()).contains(&AttentionSignal::AgentAsking),
        );

        // Add the workspace's key to the set → signal fires.
        assert!(
            workspace_attention_signals(&w, &set_with(&w)).contains(&AttentionSignal::AgentAsking),
        );
    }

    // ── needs_attention vs the gate ───────────────────────────────

    #[test]
    fn needs_attention_returns_false_when_all_signals_gated_off() {
        let mut t = base_task();
        t.ci = pilot_core::CiStatus::Failure;
        t.review = ReviewStatus::ChangesRequested;
        let w = ws_from_pr(t);
        let cfg = pilot_config::AttentionConfig {
            unread: false,
            ci_failing: false,
            review_pending: false,
            agent_asking: false,
            mentioned: false,
        };
        assert!(!workspace_needs_attention(&w, &cfg, &empty_set()));
    }

    #[test]
    fn needs_attention_returns_true_when_any_gated_on_signal_active() {
        let mut t = base_task();
        t.ci = pilot_core::CiStatus::Failure;
        let w = ws_from_pr(t);
        let mut cfg = pilot_config::AttentionConfig {
            unread: false,
            ci_failing: false,
            review_pending: false,
            agent_asking: false,
            mentioned: false,
        };
        assert!(
            !workspace_needs_attention(&w, &cfg, &empty_set()),
            "all gates off → false",
        );
        cfg.ci_failing = true;
        assert!(
            workspace_needs_attention(&w, &cfg, &empty_set()),
            "CI gate on → true",
        );
    }

    // ── consistency contract: badge vs counter ─────────────────────

    #[test]
    fn reviewers_requested_workspace_lights_both_counter_and_attention() {
        let mut t = base_task();
        t.review = ReviewStatus::None;
        t.reviewers = vec!["alice".into()];
        let w = ws_from_pr(t);
        let signals = workspace_attention_signals(&w, &empty_set());
        assert!(signals.contains(&AttentionSignal::ReviewPending));
        let cfg = pilot_config::AttentionConfig::default();
        assert!(workspace_needs_attention(&w, &cfg, &empty_set()));
    }
}
