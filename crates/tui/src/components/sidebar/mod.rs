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

use crate::{PaneId, PaneOutcome};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Minimum wall-clock between "working" spinner frame advances.
/// ~8 fps — fast enough to read as motion, slow enough that the
/// animation only nudges the render loop a few times a second while
/// an agent is busy (and never when nothing is working).
const WORKING_SPIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(120);
use crate::util::{truncate_ellipsis, visual_width};
use lazybox_core::{SessionId, SessionKey, Workspace};
use lazybox_ipc::{Command, Event, TerminalId, TerminalKind};
use ratatui::Frame;
use ratatui::prelude::*;
use ratatui::widgets::*;
use std::collections::{BTreeMap, BTreeSet, HashMap};

// The inbox view-model types + grouping/sort/filter/search logic moved
// to the client-free `lazybox_tui_core::inbox` module (#731) so the
// desktop client builds the same sidebar from the same code. Re-exported
// at the legacy `sidebar::*` paths so render/dispatch call sites keep
// their imports.
pub use lazybox_tui_core::inbox::{
    Mailbox, RepoSummary, SearchState, SortMode, VisibleRow, WorkspaceKind, role_rank,
};

/// At-a-glance attention tallies across the visible mailbox, surfaced
/// by the focus-mode event header so a heads-down user stays aware of
/// incoming work without the full sidebar. Each count reuses the same
/// `AttentionSignal` producer the per-row pills and header counters
/// read, so the strip can never disagree with what the sidebar shows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AttentionSummary {
    pub unread: usize,
    pub asking: usize,
    pub ci_failing: usize,
    pub review_pending: usize,
}

/// One exact running conversation that can receive a contextual work
/// prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningWorkTarget {
    pub terminal_id: TerminalId,
    pub agent_id: String,
}

/// Where `w` ("work on this") should route its prompt, resolved from the
/// workspace's live terminals by [`Sidebar::work_target`] (#418).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkTarget {
    /// No matching conversation is running; spawn this agent.
    Spawn(String),
    /// Exactly one matching conversation is running; inject into it.
    Running(RunningWorkTarget),
    /// Several conversations are running; ask which exact terminal
    /// should receive the prompt.
    Choose(Vec<RunningWorkTarget>),
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
    /// Repo groups the user pinned to the top, in pin order. Pinned
    /// groups render first (in this order); the rest keep the
    /// algorithmic order. Persisted to `ui.pinned_repos`. A `Vec`, not
    /// a set — the order the user pinned in is the display order.
    pinned_repos: Vec<String>,
    /// Per-repo counters computed during `recompute_visible`. Keys
    /// are the same display strings used by `VisibleRow::RepoHeader`.
    repo_summaries: BTreeMap<String, RepoSummary>,
    /// Index into `visible`. Always points at a `Workspace` variant
    /// when there is at least one — `recompute_visible` and the
    /// j/k handlers maintain that invariant.
    cursor: usize,
    /// Index of the topmost `visible` row drawn in the content area.
    /// Anchored to the cursor by default — `render` clamps this to
    /// keep `cursor` on screen, so it follows j/k automatically. The
    /// mouse wheel moves it directly instead (`scroll_by_wheel`),
    /// setting `scroll_detached`.
    scroll: usize,
    /// True while the wheel has moved the viewport away from the
    /// cursor. `render` then skips its keep-cursor-visible clamp so
    /// the cursor may sit off-screen; any explicit cursor move
    /// (`set_cursor`: j/k, click, jump pickers), a search-query
    /// change, or — via the model's key dispatch
    /// (`reanchor_viewport`) — ANY key pressed while the sidebar is
    /// focused clears the flag, re-anchoring the viewport to the
    /// selection (#290).
    scroll_detached: bool,
    /// Content-area height of the last render (0 before the first
    /// frame). Lets `scroll_by_wheel` clamp to the same last-full-page
    /// bound `render` uses, so a bottom-edge notch reports "no
    /// movement" instead of overshooting and snapping back a frame
    /// later.
    last_viewport: usize,
    /// `scroll` as of the last render — the offset actually on
    /// screen. Mouse hit-testing maps clicks through this, not
    /// `scroll`: a wheel event may have moved `scroll` after the
    /// frame was drawn (wheel repaints ride the render throttle), and
    /// a click must land on the row the user saw.
    rendered_scroll: usize,
    mailbox: Mailbox,
    /// Live, composable filter on top of the mailbox. Opened via `f`
    /// (a multi-select menu). Empty is a no-op; active filters narrow
    /// the visible list — see [`FilterSet`].
    filters: FilterSet,
    /// Sort order within each repo group. Default is recency; `o`
    /// cycles to `ByRole` and `ByRoleSplit`. See [`SortMode`].
    sort_mode: SortMode,
    /// Mirror of the daemon's live-terminals set, scoped to what we
    /// need for the workspace-row runner badges (e.g. ` C  S 2` for
    /// one Claude + two shells running). Populated from `Event::Snapshot`
    /// and kept in sync via `TerminalSpawned` / `TerminalExited`.
    running_terminals: HashMap<TerminalId, (SessionKey, TerminalKind)>,
    /// Built-in agent registry, consulted so an agent's display badge
    /// (`C` / `X` / `U`) comes from the agent itself rather than a
    /// hardcoded match here — a new agent declares its own letter and
    /// can't silently collide (#440).
    agent_registry: lazybox_tui_core::agents::Registry,
    /// Threshold config for the per-repo "needs attention" counter.
    /// Loaded from `~/.lazybox/config.yaml::attention` at startup;
    /// toggle individual signals there to customize.
    attention: lazybox_config::AttentionConfig,
    /// Projects mirrored from the daemon's project table. Each entry
    /// emits a sidebar header so a project with zero workspaces
    /// still appears. Populated by `apply_projects` (called from the
    /// model when `Snapshot` / `ProjectUpserted` / `ProjectRemoved`
    /// fires, and when the wizard's selected scopes are synthesized
    /// into Project entries — the model layer owns that merge).
    projects: BTreeMap<lazybox_core::ProjectKey, lazybox_core::Project>,
    /// Agent the `f` (fix) shortcut spawns. Defaults to `claude`; the
    /// AppRoot can override from YAML (`setup.default_agent`).
    default_agent: String,
    /// Surface merged + closed tasks in the Inbox view. Off by default
    /// — the Inbox stays focused on actionable work and the Inactive
    /// mailbox owns the history. Wired from
    /// `~/.lazybox/config.yaml::display.show_inactive_in_inbox`.
    show_inactive_in_inbox: bool,
    /// Render row type indicators as plain ASCII (`p`/`i`/`l`) rather
    /// than the default unicode glyphs (`⇄`/`○`/`◆`). Wired from
    /// `~/.lazybox/config.yaml::display.ascii_glyphs` — the escape
    /// hatch for fonts that don't render the glyphs as a single cell.
    ascii_glyphs: bool,
    /// Notifications queued in response to "any agent → Asking"
    /// transitions. The library NEVER fires an OS-level
    /// `osascript` / `notify-send` itself — that would break tests
    /// by triggering real banner spam during a `cargo test` run.
    /// The outer wrapper (`realm::components::sidebar`) drains this
    /// after each event delivery and routes to `platform::notify_user`.
    pending_notifications: Vec<PendingNotification>,
    /// One short string per agent-attention transition since the last
    /// drain — an agent newly Asking ("needs input") or newly Done
    /// ("finished") (#80). Surfaces in lazybox's footer alongside the OS
    /// notification so users with notifications muted still see it.
    pending_asking_notices: Vec<String>,
    /// Per-terminal source states mirrored from daemon snapshots and
    /// deltas. Keeping the terminal id prevents one of several agents in
    /// a workspace from overwriting another based on HashMap iteration
    /// order.
    agent_terminal_states:
        std::collections::HashMap<TerminalId, (SessionKey, lazybox_ipc::AgentState)>,
    /// The derived per-session agent status, keyed by workspace — the
    /// source of truth for the `?` asking pill / `? N input` header
    /// counter / `!` jump, the animated `Working` spinner, and the `✓`
    /// done mark (#80). The terminal states above are aggregated with
    /// explicit attention precedence into one mutually exclusive UI
    /// value per session (#327). Source: snapshots and
    /// `Event::AgentState` broadcasts from the
    /// daemon, sidebar-local — independent of `Workspace.sessions[i].state`
    /// (which gets clobbered every poll cycle when the daemon
    /// re-broadcasts `WorkspaceUpserted`).
    agents: std::collections::HashMap<SessionKey, lazybox_ipc::AgentState>,
    /// Current frame of the shared "working" spinner, mirrored from
    /// the free-running wall clock by [`Sidebar::tick_working`] so the
    /// render path stays a cheap field read and every working row shows
    /// the same frame. One `usize` drives them all.
    working_spinner_frame: usize,
    /// Fixed anchor the "working" spinner counts frames from. The
    /// displayed frame is `spinner_epoch.elapsed() / WORKING_SPIN_INTERVAL`
    /// — a pure function of elapsed time, so the glyph can never stick
    /// on a stale frame when ticks arrive irregularly, self-corrects to
    /// the right frame after a loop stall instead of crawling back one
    /// step per tick, and keeps its phase across a transient
    /// `Working → Idle → Working` flap (the daemon dedupes `AgentState`
    /// and its detector can momentarily misread an in-progress agent as
    /// idle) rather than snapping back to frame 0 mid-spin.
    spinner_epoch: std::time::Instant,
    /// Screen rect of the role-filter chip in row 1 of the header,
    /// stashed by `render` so mouse clicks can cycle it without
    /// re-deriving the layout. `None` before the first render.
    filter_chip_rect: Option<Rect>,
    /// Same idea for the sort chip on row 1, sitting to the right of
    /// the filter chip. Click cycles the sort mode.
    sort_chip_rect: Option<Rect>,
    /// Same idea for the always-visible global search box on row 1,
    /// sitting to the right of the sort chip. Click opens the global
    /// search (`open_global_search`).
    search_chip_rect: Option<Rect>,
    /// Screen rect of the bottom `/` search input bar, stashed by
    /// `render` while a search is open. A click anywhere off this bar
    /// (and off the header search chip) dismisses the search instead of
    /// being trapped in it (#780). `None` while no search bar is drawn.
    search_bar_rect: Option<Rect>,
    /// Test-only override for the "now" reference `render` uses to
    /// format relative timestamps (`1mo`, `2d`, …). Production leaves
    /// this `None` and reads the wall clock; golden snapshot tests set
    /// it via [`Sidebar::set_now_override`] so the rendered ages don't
    /// drift as real time passes (otherwise a `1mo` row silently
    /// becomes `2mo` a month later and breaks the snapshot).
    now_override: Option<chrono::DateTime<chrono::Utc>>,
    /// Free-text filter scoped to the focused project. `None` when no
    /// search is in flight; `Some` while the `/` input bar is open or
    /// a query stays applied after `Enter`. See [`SearchState`].
    search: Option<SearchState>,
    /// Workspace rows the user multi-selected with `v` — the targets a
    /// broadcast (`Shift-B`) fans out to. Keys, not row indices, so the
    /// marks survive re-sorts and j/k navigation; pruned when a
    /// workspace is removed and cleared by Esc or a successful send.
    broadcast_selected: std::collections::HashSet<SessionKey>,
    /// Mirror of `ui.keep_awake` as loaded at startup. When set, the
    /// header paints a small "awake" badge while any agent is
    /// `Working` — the same condition under which the daemon holds
    /// its OS sleep inhibitor — so the user can see the machine is
    /// being kept awake and why. The daemon re-reads the flag live;
    /// this client-side mirror refreshes on restart.
    keep_awake: bool,
}

/// A queued user-facing notification that the outer (IO-aware) layer
/// will translate into an OS-level banner. Pure data so the sidebar
/// is fully testable without involving any subprocess.
#[derive(Debug, Clone)]
pub struct PendingNotification {
    pub title: String,
    pub body: String,
    pub workspace_key: SessionKey,
}

impl Sidebar {
    pub fn new(id: PaneId) -> Self {
        Self {
            id,
            workspaces: HashMap::new(),
            visible: Vec::new(),
            collapsed_repos: BTreeSet::new(),
            pinned_repos: Vec::new(),
            repo_summaries: BTreeMap::new(),
            cursor: 0,
            scroll: 0,
            scroll_detached: false,
            last_viewport: 0,
            rendered_scroll: 0,
            mailbox: Mailbox::Inbox,
            filters: FilterSet::default(),
            sort_mode: SortMode::default(),
            running_terminals: HashMap::new(),
            agent_registry: lazybox_tui_core::agents::registry(),
            attention: lazybox_config::AttentionConfig::default(),
            projects: BTreeMap::new(),
            default_agent: "claude".to_string(),
            show_inactive_in_inbox: false,
            ascii_glyphs: false,
            pending_notifications: Vec::new(),
            pending_asking_notices: Vec::new(),
            agents: std::collections::HashMap::new(),
            agent_terminal_states: std::collections::HashMap::new(),
            working_spinner_frame: 0,
            spinner_epoch: std::time::Instant::now(),
            filter_chip_rect: None,
            sort_chip_rect: None,
            search_chip_rect: None,
            search_bar_rect: None,
            now_override: None,
            search: None,
            broadcast_selected: std::collections::HashSet::new(),
            keep_awake: false,
        }
    }

    /// The sidebar's single source of "now". Every time-dependent
    /// decision — relative-timestamp rendering, the mailbox
    /// grace-window classification, snooze deadlines, the optimistic
    /// merge stamp — reads the clock through here so they all agree
    /// within a frame and so a test can pin them with
    /// [`set_now_override`](Self::set_now_override). Production leaves
    /// the override `None` and reads the wall clock.
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        self.now_override.unwrap_or_else(chrono::Utc::now)
    }

    /// Pin the clock `now()` returns so golden snapshots and
    /// time-sensitive behavior stay deterministic regardless of when
    /// the test runs. Set it *before* feeding events so the
    /// visible-set classification observes the same instant as render.
    /// Intended for tests only; production reads the wall clock.
    pub fn set_now_override(&mut self, now: chrono::DateTime<chrono::Utc>) {
        self.now_override = Some(now);
    }

    /// Record whether `ui.keep_awake` is on, so the header can badge
    /// active sleep inhibition. Display-only — the daemon holds the
    /// actual inhibitor.
    pub fn set_keep_awake(&mut self, keep_awake: bool) {
        self.keep_awake = keep_awake;
    }

    /// True while ≥1 agent in the sidebar is `Working` — the same
    /// predicate the daemon's keep-awake watcher inhibits sleep on.
    fn any_agent_working(&self) -> bool {
        self.agents
            .values()
            .any(|s| matches!(s, lazybox_ipc::AgentState::Working))
    }
    /// Sync the "working" spinner to the wall clock. Returns `true`
    /// when the displayed frame changed, so the caller knows a
    /// re-render is warranted.
    ///
    /// The frame is derived from elapsed time, not accumulated per
    /// tick: `spinner_epoch.elapsed() / WORKING_SPIN_INTERVAL`. That is
    /// what makes the indicator resilient — it advances purely with the
    /// clock no matter how irregularly the run loop calls this, jumps
    /// straight to the correct frame after a stall (instead of crawling
    /// back one step at a time), and holds its phase across a transient
    /// `Working → Idle → Working` flap rather than snapping to frame 0
    /// mid-spin. The working-set only gates whether the spinner is
    /// shown; its phase is owned by the clock.
    ///
    /// Cheap by construction: a no-op when nothing is working, and at
    /// most one frame change per `WORKING_SPIN_INTERVAL` (~8 fps) the
    /// rest of the time, so it never forces a faster redraw and a single
    /// shared frame index means no per-row work on each tick.
    pub fn tick_working(&mut self) -> bool {
        if !self.any_agent_working() {
            return false;
        }
        let frame =
            (self.spinner_epoch.elapsed().as_millis() / WORKING_SPIN_INTERVAL.as_millis()) as usize;
        if frame == self.working_spinner_frame {
            return false;
        }
        self.working_spinner_frame = frame;
        true
    }

    /// True when the sidebar already displays `state` for this
    /// session — i.e. applying the event again would change nothing
    /// on screen. The orchestrator uses this to skip the redraw (and
    /// the workspace re-projection) for the daemon's repeated
    /// `AgentState` pings, which otherwise arrive every detector
    /// tick while an agent streams.
    /// The agent status currently stored for `session_key`, or `None`
    /// when the daemon has never reported one (treated as `Idle`).
    /// Test-facing read of the single per-session state map.
    #[cfg(test)]
    pub(crate) fn agent_state(&self, session_key: &SessionKey) -> Option<lazybox_ipc::AgentState> {
        self.agents.get(session_key).copied()
    }

    pub fn displays_agent_state(
        &self,
        session_key: &SessionKey,
        state: lazybox_ipc::AgentState,
    ) -> bool {
        // "Already displays it" = the stored state already equals this
        // reading, so folding it in would be a no-op. An absent entry
        // renders as `Idle`, so treat it as such.
        self.agents
            .get(session_key)
            .copied()
            .unwrap_or(lazybox_ipc::AgentState::Idle)
            == state
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

    /// Exact running agent terminals in `workspace_key`, sorted by agent
    /// id and then terminal id for stable chooser order.
    pub fn running_work_targets(&self, workspace_key: &SessionKey) -> Vec<RunningWorkTarget> {
        let mut targets = Vec::new();
        for (terminal_id, (sk, kind)) in &self.running_terminals {
            if sk != workspace_key {
                continue;
            }
            if let TerminalKind::Agent(agent_id) = kind {
                targets.push(RunningWorkTarget {
                    terminal_id: *terminal_id,
                    agent_id: agent_id.clone(),
                });
            }
        }
        targets.sort_unstable_by(|left, right| {
            left.agent_id
                .cmp(&right.agent_id)
                .then_with(|| left.terminal_id.0.cmp(&right.terminal_id.0))
        });
        targets
    }

    /// Pick the conversation `w` ("work on this") should target for a
    /// workspace (#418). One running conversation wins over the default
    /// so `w` continues it instead of spawning a fresh agent.
    ///
    /// When several conversations run at once there is no right guess,
    /// including when they use the same agent id, so the caller must ask.
    /// The default agent is the answer only when nothing is running.
    pub fn work_target(&self, workspace_key: &SessionKey, default_agent: &str) -> WorkTarget {
        let running = self.running_work_targets(workspace_key);
        match running.as_slice() {
            [] => WorkTarget::Spawn(default_agent.to_string()),
            [only] => WorkTarget::Running(only.clone()),
            _ => WorkTarget::Choose(running),
        }
    }

    /// Resolve a scoped `w <agent>` action. Other agent kinds do not
    /// participate, but multiple conversations for the requested agent
    /// still require an exact terminal choice.
    pub fn work_target_for_agent(&self, workspace_key: &SessionKey, agent_id: &str) -> WorkTarget {
        let running: Vec<_> = self
            .running_work_targets(workspace_key)
            .into_iter()
            .filter(|target| target.agent_id == agent_id)
            .collect();
        match running.as_slice() {
            [] => WorkTarget::Spawn(agent_id.to_string()),
            [only] => WorkTarget::Running(only.clone()),
            _ => WorkTarget::Choose(running),
        }
    }

    /// Optimistic local update: flip a task to `Merged` so the
    /// status pill changes immediately, before the next poll cycle
    /// catches up with GitHub's response. Called when `Event::PrMerged`
    /// arrives from the daemon — GitHub accepted the merge, so the
    /// pill should reflect that NOW, not 30s from now when the next
    /// poll rebroadcasts the workspace.
    pub fn mark_workspace_merged(&mut self, key: &lazybox_core::WorkspaceKey) {
        // Read the clock before the mutable borrow of `workspaces`.
        let now = self.now();
        let sk: SessionKey = key.into();
        if let Some(workspace) = self.workspaces.get_mut(&sk) {
            if let Some(pr) = workspace.pr.as_mut() {
                pr.state = lazybox_core::TaskState::Merged;
                // Stamp the close moment so the grace window keys off
                // it (not the stale `updated_at`) until the next poll
                // backfills GitHub's real `closedAt`.
                pr.closed_at = Some(now);
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

    /// Live-update the default agent (Settings → "Change default
    /// agent"). Mirrors `with_default_agent` for the in-session path.
    pub fn set_default_agent(&mut self, agent: impl Into<String>) {
        self.default_agent = agent.into();
    }

    /// Replace the mirrored project table. Driven from the model's
    /// `Snapshot::projects` + `ProjectUpserted` / `ProjectRemoved`
    /// handlers; the sidebar's headers render from this on the next
    /// `recompute_visible`. Cheap to call on every snapshot — the
    /// map clones the daemon's view, no diffing required.
    pub fn apply_projects(
        &mut self,
        projects: BTreeMap<lazybox_core::ProjectKey, lazybox_core::Project>,
    ) {
        if projects != self.projects {
            self.projects = projects;
            self.recompute_visible();
        }
    }

    /// Override the attention thresholds + initial collapse / pin sets
    /// from `~/.lazybox/config.yaml`. Call once after construction
    /// (typically in main, between `Sidebar::new` and the first
    /// daemon Subscribe).
    pub fn apply_config(
        &mut self,
        attention: lazybox_config::AttentionConfig,
        collapsed_repos: BTreeSet<String>,
        pinned_repos: Vec<String>,
        default_agent: Option<String>,
        display: &lazybox_config::DisplayConfig,
    ) {
        self.attention = attention;
        self.collapsed_repos = collapsed_repos;
        self.pinned_repos = pinned_repos;
        if let Some(agent) = default_agent.filter(|s| !s.is_empty()) {
            self.default_agent = agent;
        }
        self.set_show_inactive_in_inbox(display.show_inactive_in_inbox);
        self.ascii_glyphs = display.ascii_glyphs;
    }

    // ── Observability helpers (for tests + for AppRoot / RightPane) ────

    pub fn selected_session_key(&self) -> Option<&SessionKey> {
        match self.visible.get(self.cursor)? {
            VisibleRow::Workspace(k) => Some(k),
            VisibleRow::Session { workspace, .. } => Some(workspace),
            VisibleRow::RepoHeader(_) | VisibleRow::KindHeader(_) => None,
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

    /// Toggle the workspace under the cursor in/out of the broadcast
    /// multi-select set. Returns the new state (`true` = now selected)
    /// so the caller can surface a footer notice, or `None` when the
    /// cursor isn't on a workspace / session row.
    pub fn toggle_broadcast_select(&mut self) -> Option<bool> {
        let key = self.selected_session_key()?.clone();
        if self.broadcast_selected.insert(key.clone()) {
            Some(true)
        } else {
            self.broadcast_selected.remove(&key);
            Some(false)
        }
    }

    /// The multi-selected workspaces, in sidebar (visible) order — the
    /// order the broadcast targets them and the modal header lists
    /// them. Rows hidden by the current mailbox / filter don't
    /// broadcast: what you see marked is what gets sent.
    pub fn selected_broadcast_keys(&self) -> Vec<SessionKey> {
        self.visible
            .iter()
            .filter_map(|row| match row {
                VisibleRow::Workspace(k) if self.broadcast_selected.contains(k) => Some(k.clone()),
                _ => None,
            })
            .collect()
    }

    /// Is this workspace in the broadcast multi-select set? Drives the
    /// `✓` mark in the row's selection gutter.
    pub fn is_broadcast_selected(&self, key: &SessionKey) -> bool {
        self.broadcast_selected.contains(key)
    }

    pub fn broadcast_selected_count(&self) -> usize {
        self.broadcast_selected.len()
    }

    /// Drop the whole multi-select set. Bound to Esc and called after
    /// a successful broadcast so the marks don't outlive the send.
    /// Returns whether anything was cleared (so Esc can fall through
    /// when there was no selection).
    pub fn clear_broadcast_selection(&mut self) -> bool {
        let had = !self.broadcast_selected.is_empty();
        self.broadcast_selected.clear();
        had
    }

    /// The terminal a broadcast should deliver to for `key`, plus
    /// whether it runs an agent. Agents win over shells (the
    /// settle-gated inject path vs. a raw write); ties break on the
    /// lowest terminal id so repeated broadcasts land on the same
    /// terminal. `None` when the workspace has no running session.
    pub fn broadcast_terminal(&self, key: &SessionKey) -> Option<(TerminalId, bool)> {
        let mut agent: Option<TerminalId> = None;
        let mut shell: Option<TerminalId> = None;
        for (tid, (sk, kind)) in &self.running_terminals {
            if sk != key {
                continue;
            }
            let slot = match kind {
                TerminalKind::Agent(_) => &mut agent,
                TerminalKind::Shell => &mut shell,
                _ => continue,
            };
            if slot.is_none_or(|t| tid.0 < t.0) {
                *slot = Some(*tid);
            }
        }
        agent.map(|t| (t, true)).or(shell.map(|t| (t, false)))
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

    /// Return the workspace key the `g m` merge shortcut would
    /// target. Delegates to [`resolve_merge`] so the contextual footer
    /// advertises the key under exactly the same conditions the merge
    /// dispatch fires — no second predicate to drift out of sync.
    ///
    /// [`resolve_merge`]: lazybox_tui_core::intent::resolve_merge
    pub fn merge_target_for_cursor(&self) -> Option<lazybox_core::WorkspaceKey> {
        match lazybox_tui_core::intent::resolve_merge(self.selected_workspace()) {
            lazybox_tui_core::intent::Intent::MergePr { workspace_key } => Some(workspace_key),
            _ => None,
        }
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
    /// True when `(col, row)` falls on the header filter chip — a hit
    /// opens the filter menu (same effect as pressing `f`). Pure hit
    /// test: the menu is mounted by the model, which owns the modal
    /// stack.
    pub fn filter_chip_hit(&self, col: u16, row: u16) -> bool {
        let Some(rect) = self.filter_chip_rect else {
            return false;
        };
        row == rect.y && col >= rect.x && col < rect.x + rect.width
    }

    /// Click on the sort chip cycles it — same effect as `o`.
    pub fn click_to_cycle_sort(&mut self, col: u16, row: u16) -> bool {
        let Some(rect) = self.sort_chip_rect else {
            return false;
        };
        if row != rect.y || col < rect.x || col >= rect.x + rect.width {
            return false;
        }
        self.cycle_sort_mode();
        true
    }

    /// True when `(col, row)` falls on the header search box — a hit
    /// opens the global search (same effect as `#`). Pure hit test:
    /// the caller fires `open_global_search`.
    pub fn search_chip_hit(&self, col: u16, row: u16) -> bool {
        let Some(rect) = self.search_chip_rect else {
            return false;
        };
        row == rect.y && col >= rect.x && col < rect.x + rect.width
    }

    /// True when `(col, row)` falls on the bottom `/` search input bar.
    /// A click here keeps the search alive (it's the input itself); a
    /// click anywhere else dismisses it (#780).
    pub fn search_bar_hit(&self, col: u16, row: u16) -> bool {
        let Some(rect) = self.search_bar_rect else {
            return false;
        };
        row == rect.y && col >= rect.x && col < rect.x + rect.width
    }

    pub fn click_to_select(&mut self, area: Rect, click_row: u16) -> bool {
        // Mirror the constants from `render`.
        const HEADER_HEIGHT: u16 = 5;
        if click_row < area.y + HEADER_HEIGHT {
            return false;
        }
        // Add the scroll offset the renderer applied so a click lands
        // on the row actually drawn under the cursor — `rendered_scroll`,
        // not `scroll`, because a wheel notch dispatched after the last
        // frame may have moved `scroll` past what's on screen.
        let idx = (click_row - area.y - HEADER_HEIGHT) as usize + self.rendered_scroll;
        match self.visible.get(idx) {
            // Headers ARE selectable now (post-Stage-4): the user
            // needs to land cursor on a project header to fire
            // `n` (new workspace) against that project, or to
            // `Space`-toggle the repo's collapsed state. Without
            // this a newly-created local project was unreachable
            // via mouse — the user had to keyboard-navigate j/k
            // through every workspace above it.
            Some(VisibleRow::Workspace(_))
            | Some(VisibleRow::Session { .. })
            | Some(VisibleRow::RepoHeader(_))
            | Some(VisibleRow::KindHeader(_)) => {
                self.set_cursor(idx);
                true
            }
            None => false,
        }
    }

    /// Explicit-navigation cursor assignment (keys, clicks, jump
    /// pickers): moves the cursor AND re-anchors a wheel-detached
    /// viewport. The passive index fixups in `recompute_visible_inner`
    /// write `self.cursor` directly instead — a background resort must
    /// never yank a wheel-scrolled display back to the cursor.
    fn set_cursor(&mut self, idx: usize) {
        self.cursor = idx;
        self.scroll_detached = false;
    }

    /// Re-anchor a wheel-detached viewport: the next render clamps
    /// the scroll offset back to the cursor. Returns whether the
    /// viewport was detached (callers repaint on true). The model
    /// calls this for every key pressed while the sidebar is focused
    /// — keys act on (or move) the selection, so it must be on
    /// screen.
    pub fn reanchor_viewport(&mut self) -> bool {
        std::mem::take(&mut self.scroll_detached)
    }

    /// Mouse-wheel scroll over the sidebar: move the viewport offset
    /// by `delta` rows, leaving the cursor untouched. Selection
    /// changes have side effects — the right pane, terminal stack,
    /// and focus all follow the selected workspace — so a trackpad
    /// flick must never change it (#290). Detaches the offset from
    /// the cursor (`scroll_detached`); the next explicit cursor move
    /// re-anchors. Returns whether the offset moved.
    pub fn scroll_by_wheel(&mut self, delta: isize) -> bool {
        // Clamp to the same last-full-page bound `render` settles on
        // (`last_viewport` is 0 before the first frame — fall back to
        // a 1-row page) so a bottom-edge notch reports no movement
        // instead of overshooting and snapping back next frame.
        let max = self.visible.len().saturating_sub(self.last_viewport.max(1));
        let target = self.scroll.saturating_add_signed(delta).min(max);
        if target == self.scroll {
            return false;
        }
        self.scroll = target;
        self.scroll_detached = true;
        true
    }

    /// Look up a workspace by its session key. Used by paths that
    /// need workspace data without disturbing the cursor (e.g. the
    /// editor-deferred-by-spawn flow that has to find the
    /// worktree of a specific workspace, not the focused one).
    pub fn workspace_by_key(&self, key: &SessionKey) -> Option<&Workspace> {
        self.workspaces.get(key)
    }

    /// Remove a workspace row locally, without waiting for the daemon's
    /// `WorkspaceRemoved` echo — the optimistic half of an archive /
    /// delete (#476). Returns the removed workspace so the caller can
    /// restore it if the round-trip fails. Mirrors the cleanup the
    /// `WorkspaceRemoved` event handler does so a later echo is a no-op.
    pub fn take_workspace(&mut self, key: &SessionKey) -> Option<Workspace> {
        let removed = self.workspaces.remove(key);
        if removed.is_some() {
            self.broadcast_selected.remove(key);
            self.agents.remove(key);
            self.recompute_after_workspace_removed(key);
        }
        removed
    }

    /// Re-insert (or replace) a workspace optimistically edited or
    /// removed, to roll back a failed round-trip (#476). Unlike the
    /// `WorkspaceUpserted` event path this fires no desktop
    /// notification — it restores state the user already saw.
    pub fn restore_workspace(&mut self, workspace: Workspace) {
        let key: SessionKey = (&workspace.key).into();
        self.workspaces.insert(key, workspace);
        self.recompute_visible();
    }

    /// Move the cursor onto the workspace row matching `key`. Returns
    /// true on a hit. Used by `--workspace` preselect on startup.
    pub fn focus_workspace_key(&mut self, key: &SessionKey) -> bool {
        for (i, row) in self.visible.iter().enumerate() {
            if let VisibleRow::Workspace(k) = row
                && k == key
            {
                self.set_cursor(i);
                return true;
            }
        }
        false
    }

    /// Make a tracked workspace visible, then move the cursor to it.
    /// User-driven jumps cross mailbox, filter, search, and collapsed
    /// group boundaries; a missing workspace leaves the current view
    /// untouched.
    pub fn reveal_workspace_key(&mut self, key: &SessionKey) -> bool {
        if self.focus_workspace_key(key) {
            return true;
        }

        let now = self.now();
        let Some(workspace) = self.workspaces.get(key) else {
            return false;
        };
        let mailbox = [
            self.mailbox,
            Mailbox::Inbox,
            Mailbox::Inactive,
            Mailbox::Snoozed,
        ]
        .into_iter()
        .find(|mailbox| mailbox_membership(workspace, *mailbox, now, self.show_inactive_in_inbox));
        let Some(mailbox) = mailbox else {
            return false;
        };
        let group = crate::components::visible_rows::group_label(
            workspace,
            &self.projects,
            &self.workspaces,
        );
        let filter_hides = !self.filters.accepts(&FilterCtx {
            w: workspace,
            agents: &self.agents,
        });
        let search_hides = self.search.as_ref().is_some_and(|search| {
            !search.query.is_empty()
                && search.scope.as_deref().is_none_or(|scope| scope == group)
                && !crate::components::visible_rows::search_matches(&search.query, workspace)
        });

        self.mailbox = mailbox;
        if filter_hides {
            self.filters.replace(std::iter::empty());
        }
        if search_hides {
            self.search = None;
        }
        self.collapsed_repos.remove(&group);
        self.recompute_visible();
        self.focus_workspace_key(key)
    }

    /// Move the cursor onto the RepoHeader row for the given project.
    /// Returns true on a hit. Used by the just-created-a-project flow
    /// so the user lands on their new project ready to press `n` to
    /// add a workspace. Without this, the cursor stays wherever it
    /// was before and the new RepoHeader is unreachable via j/k
    /// (header rows are skipped by `move_cursor_by`), which made the
    /// new-project flow feel broken.
    pub fn focus_project_header(&mut self, key: &lazybox_core::ProjectKey) -> bool {
        let label = match self.projects.get(key) {
            Some(p) => crate::components::visible_rows::project_label(p, &self.workspaces),
            None => return false,
        };
        for (i, row) in self.visible.iter().enumerate() {
            if let VisibleRow::RepoHeader(name) = row
                && name == &label
            {
                self.set_cursor(i);
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
            &self.agents,
            &keys_order,
            current.as_ref(),
        ) else {
            return false;
        };
        self.focus_workspace_key(&target)
    }

    /// Move the cursor onto the next workspace whose PR has failing
    /// (or mixed) CI, starting AFTER the current row and wrapping —
    /// so `Shift-F` cycles through broken PRs rather than re-selecting
    /// the current one. Returns true when a target was found.
    ///
    /// Membership comes from the same `CiFailing` attention signal the
    /// header counter and row pill read, so what the user jumps to
    /// always matches the red ` CI FAIL ` / ` CI MIX ` pills they see.
    pub fn focus_next_failing_ci_workspace(&mut self) -> bool {
        let keys_order: Vec<SessionKey> = self
            .visible
            .iter()
            .filter_map(|r| match r {
                VisibleRow::Workspace(k) => Some(k.clone()),
                _ => None,
            })
            .collect();
        let failing: std::collections::HashSet<SessionKey> = keys_order
            .iter()
            .filter(|k| {
                self.workspaces.get(*k).is_some_and(|w| {
                    workspace_attention_signals(w, &self.agents)
                        .contains(&AttentionSignal::CiFailing)
                })
            })
            .cloned()
            .collect();
        let current = self.selected_session_key().cloned();
        let Some(target) =
            crate::agent_attention::next_flagged_workspace(&failing, &keys_order, current.as_ref())
        else {
            return false;
        };
        self.focus_workspace_key(&target)
    }

    /// The visible workspaces that have a coding-agent session, in
    /// sidebar (top-down) order. The 1-based index into this list is
    /// the number shown on the row's jump badge and dialed by the
    /// `]]<digit>` focus-mode jump, so both read from the same source
    /// and can't drift.
    pub fn agent_workspace_keys(&self) -> Vec<SessionKey> {
        self.visible
            .iter()
            .filter_map(|r| match r {
                VisibleRow::Workspace(k) => Some(k),
                _ => None,
            })
            .filter(|k| {
                self.workspaces.get(*k).is_some_and(|w| {
                    w.sessions
                        .iter()
                        .any(|s| matches!(s.kind, lazybox_core::SessionKind::Agent { .. }))
                })
            })
            .cloned()
            .collect()
    }

    /// Move the cursor onto the `n`th (1-based) agent workspace in
    /// sidebar order. Returns true when that slot exists and the
    /// cursor moved. Backs the `]]<digit>` focus-mode jump — the
    /// deterministic replacement for the old `F3` cycle.
    pub fn focus_nth_agent_workspace(&mut self, n: usize) -> bool {
        let Some(target) = n
            .checked_sub(1)
            .and_then(|i| self.agent_workspace_keys().into_iter().nth(i))
        else {
            return false;
        };
        self.focus_workspace_key(&target)
    }

    /// Build the fuzzy-switcher target list (`JumpToWorkspace`): every
    /// tracked workspace across all repos as `(session key, label)`.
    /// Workspaces needing attention — agent asking or failing CI —
    /// sort to the top (and carry a tag) so the picker doubles as a
    /// consolidated `!` / `Shift-F` jump; the rest follow in label
    /// order. The label embeds the provider id (`owner/repo#N`) so the
    /// user can filter by repo, number, or title.
    pub fn jump_targets(&self) -> Vec<(SessionKey, String)> {
        let mut rows: Vec<(SessionKey, String, bool)> = self
            .workspaces
            .iter()
            .map(|(key, w)| {
                let signals = workspace_attention_signals(w, &self.agents);
                let asking = signals.contains(&AttentionSignal::AgentAsking);
                let ci = signals.contains(&AttentionSignal::CiFailing);
                let mut label = match w.primary_task() {
                    Some(t) => format!("{}  {}", t.id.key, w.name),
                    None => w.name.clone(),
                };
                let mut tags: Vec<&str> = Vec::new();
                if asking {
                    tags.push("asking");
                }
                if ci {
                    tags.push("CI✗");
                }
                if !tags.is_empty() {
                    label = format!("{label}  [{}]", tags.join(" "));
                }
                (key.clone(), label, asking || ci)
            })
            .collect();
        rows.sort_by(|a, b| {
            b.2.cmp(&a.2)
                .then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase()))
        });
        rows.into_iter().map(|(k, l, _)| (k, l)).collect()
    }

    /// At-a-glance attention tallies for the focus-mode event header.
    /// Reuses the same per-signal counters that drive the sidebar's
    /// own header badges so the two never drift.
    pub fn attention_summary(&self) -> AttentionSummary {
        AttentionSummary {
            unread: self.total_unread_count(),
            asking: self.input_pending_count(),
            ci_failing: self.ci_failing_count(),
            review_pending: self.review_pending_count(),
        }
    }

    /// Move the cursor onto the session sub-row matching `id`. No-op
    /// when the row isn't visible — caller must already have aligned
    /// the workspace via `focus_workspace_key`.
    pub fn focus_session_id(&mut self, id: SessionId) -> bool {
        for (i, row) in self.visible.iter().enumerate() {
            if let VisibleRow::Session { session_id, .. } = row
                && *session_id == id
            {
                self.set_cursor(i);
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

    /// The active filter set — read by the header renderer for its
    /// chips and by the model to pre-check the filter menu.
    pub fn filters(&self) -> &FilterSet {
        &self.filters
    }

    pub fn sort_mode(&self) -> SortMode {
        self.sort_mode
    }

    /// Cycle the sort mode (`Default → ByRole → ByRoleSplit →
    /// Default`) and rebuild. Cursor resets — re-sorted lists put a
    /// different row at the top, and the user re-anchors visually.
    pub fn cycle_sort_mode(&mut self) -> SortMode {
        self.sort_mode = self.sort_mode.next();
        self.reset_cursor_and_recompute();
        self.sort_mode
    }

    /// Replace the active filter set and rebuild the visible list.
    /// Cursor is reset because the row the user was parked on may have
    /// just been filtered out — landing on the new top is less
    /// surprising than landing off-screen.
    pub fn set_filters(&mut self, filters: impl IntoIterator<Item = Filter>) {
        self.filters.replace(filters);
        self.reset_cursor_and_recompute();
    }

    /// Per-filter match counts over the workspaces the current mailbox
    /// admits (before the active filters narrow further). Drives the
    /// `(N)` counts in the filter menu so the user can see what each
    /// toggle would surface. Order matches [`Filter::ALL`].
    pub fn filter_counts(&self) -> Vec<(Filter, usize)> {
        let now = self.now();
        let candidates: Vec<&Workspace> = self
            .workspaces
            .values()
            .filter(|w| mailbox_membership(w, self.mailbox, now, self.show_inactive_in_inbox))
            .collect();
        Filter::ALL
            .into_iter()
            .map(|f| {
                let n = candidates
                    .iter()
                    .filter(|w| {
                        f.matches(&FilterCtx {
                            w,
                            agents: &self.agents,
                        })
                    })
                    .count();
                (f, n)
            })
            .collect()
    }

    pub fn mailbox(&self) -> Mailbox {
        self.mailbox
    }

    /// Cycle the mailbox view (`Inbox → Inactive → Snoozed → Inbox`)
    /// and rebuild. Cursor resets — the row the user was on almost
    /// certainly isn't visible in the next mailbox.
    pub fn cycle_mailbox(&mut self) -> Mailbox {
        self.mailbox = match self.mailbox {
            Mailbox::Inbox => Mailbox::Inactive,
            Mailbox::Inactive => Mailbox::Snoozed,
            Mailbox::Snoozed => Mailbox::Inbox,
        };
        self.reset_cursor_and_recompute();
        self.mailbox
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Test accessor — the row-window scroll offset, as moved by
    /// `scroll_by_wheel` and settled by the last `render` (which
    /// re-anchors it to the cursor unless wheel-detached). Used to
    /// assert the visible list actually scrolled, not just the cursor.
    #[doc(hidden)]
    pub fn __test_scroll(&self) -> usize {
        self.scroll
    }

    pub fn visible_count(&self) -> usize {
        self.visible.len()
    }

    /// True when the inbox is genuinely empty — no rows at all on the
    /// default, unfiltered Inbox view, with no search narrowing it.
    /// A first-run user with little/no GitHub data lands here, so the
    /// renderer swaps the blank list for a getting-started panel that
    /// teaches the next actions (issue #100). A list emptied by an
    /// active filter, a non-Inbox mailbox, or a search query is NOT
    /// this case — those are user-driven narrowings, not first-run.
    pub fn is_getting_started(&self) -> bool {
        self.visible.is_empty()
            && self.mailbox == Mailbox::Inbox
            && self.filters.is_empty()
            && self.search.as_ref().is_none_or(|s| s.query.is_empty())
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

    /// Every known Project as `(key, display name)`, in the sidebar's
    /// stable `BTreeMap` order. Drives the global "start agent"
    /// (`Shift-W`) picker, which — unlike `n` — can't lean on the
    /// cursor to resolve a project, so it offers the full list.
    pub fn projects_for_picker(&self) -> Vec<(lazybox_core::ProjectKey, String)> {
        self.projects
            .values()
            .map(|p| {
                (
                    p.key.clone(),
                    crate::components::visible_rows::project_label(p, &self.workspaces),
                )
            })
            .collect()
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
    pub fn focused_project_key(&self) -> Option<lazybox_core::ProjectKey> {
        match self.visible.get(self.cursor)? {
            VisibleRow::Workspace(k) => {
                let w = self.workspaces.get(k)?;
                lazybox_core::workspace_project_key(w)
            }
            VisibleRow::Session { workspace, .. } => {
                let w = self.workspaces.get(workspace)?;
                lazybox_core::workspace_project_key(w)
            }
            VisibleRow::RepoHeader(name) => self
                .projects
                .values()
                .find(|p| {
                    crate::components::visible_rows::project_label(p, &self.workspaces) == *name
                })
                .map(|p| p.key.clone()),
            // Kind headers (PRs / Issues) don't belong to a single
            // project — they partition workspaces within a project,
            // so the parent project is whichever RepoHeader came
            // above. Walk back.
            VisibleRow::KindHeader(_) => {
                self.visible
                    .iter()
                    .take(self.cursor)
                    .rev()
                    .find_map(|r| match r {
                        VisibleRow::RepoHeader(name) => self
                            .projects
                            .values()
                            .find(|p| {
                                crate::components::visible_rows::project_label(p, &self.workspaces)
                                    == *name
                            })
                            .map(|p| p.key.clone()),
                        _ => None,
                    })
            }
        }
    }

    /// Iterate every workspace in local state. Used by the
    /// `CollapseIntoPr` dispatcher to find a PR that closes the
    /// focused issue — the cross-workspace relationship lookup
    /// doesn't fit the per-workspace `intent::resolve_*` shape, so
    /// the dispatcher walks the map directly.
    pub fn workspaces_iter(&self) -> impl Iterator<Item = &lazybox_core::Workspace> {
        self.workspaces.values()
    }

    /// Repo-header label the cursor currently sits under — the nearest
    /// `RepoHeader` at or above the cursor row. `None` when there's no
    /// header above (empty list). Drives the search scope: `/` filters
    /// exactly this project's rows.
    fn focused_repo_header(&self) -> Option<String> {
        self.visible
            .iter()
            .take(self.cursor + 1)
            .rev()
            .find_map(|r| match r {
                VisibleRow::RepoHeader(name) => Some(name.clone()),
                _ => None,
            })
    }

    /// Open (or re-focus) the `/` search bar, scoped to the project
    /// under the cursor. Re-pressing `/` while a query for the same
    /// project is already applied resumes editing it; targeting a
    /// different project starts fresh. No-op when the cursor isn't
    /// under any project header.
    pub fn open_search(&mut self) {
        let Some(scope) = self.focused_repo_header() else {
            return;
        };
        match self.search.as_mut() {
            Some(s) if s.scope.as_deref() == Some(scope.as_str()) => s.editing = true,
            _ => {
                self.search = Some(SearchState {
                    scope: Some(scope),
                    query: String::new(),
                    editing: true,
                });
            }
        }
        // The query is scoped to the CURSOR's project — make sure
        // that's the project on screen while the user types.
        self.scroll_detached = false;
    }

    /// Open (or re-focus) the global search box — an incremental
    /// search across every repo group (catalog `OpenGlobalSearch`,
    /// default `#`). Re-pressing `#` while a global query is already
    /// applied resumes editing it; a scoped `/` search in flight is
    /// replaced with a fresh global one. Unlike `/`, this needs no
    /// project under the cursor.
    pub fn open_global_search(&mut self) {
        match self.search.as_mut() {
            Some(s) if s.scope.is_none() => s.editing = true,
            _ => {
                self.search = Some(SearchState {
                    scope: None,
                    query: String::new(),
                    editing: true,
                });
            }
        }
        self.scroll_detached = false;
    }

    /// True while the `/` input bar is capturing keystrokes. The
    /// orchestrator routes keys straight here (bypassing pane / catalog
    /// dispatch) so typing a query never triggers a shortcut.
    pub fn search_editing(&self) -> bool {
        self.search.as_ref().is_some_and(|s| s.editing)
    }

    /// The active search state, if any (query may be empty). Read by
    /// `render` to draw the bottom bar and the per-project match count.
    pub fn search(&self) -> Option<&SearchState> {
        self.search.as_ref()
    }

    /// Dismiss the search from a click that landed outside the input —
    /// the mouse equivalent of the keyboard exit, so a user who never
    /// learned `Esc` isn't trapped in "find land" (#780). Mirrors
    /// `Enter`: an empty query closes the bar outright; a non-empty one
    /// keeps its filter applied but stops capturing keystrokes so the
    /// click can focus the pane it landed in. No-op when nothing is
    /// being edited.
    pub fn dismiss_search(&mut self) {
        match self.search.as_mut() {
            Some(s) if !s.editing => {}
            Some(s) if s.query.is_empty() => {
                self.search = None;
                self.scroll_detached = false;
                self.recompute_visible();
            }
            Some(s) => s.editing = false,
            None => {}
        }
    }

    /// Feed one keystroke into the open search bar. Printable chars
    /// extend the query, `Backspace` trims it, `Enter` keeps the query
    /// applied while closing the editor, `Esc` clears + closes. Each
    /// query mutation rebuilds the visible list so filtering is live.
    pub fn handle_search_key(&mut self, key: KeyEvent) {
        let mut query_changed = false;
        match key.code {
            KeyCode::Esc => {
                if self.search.is_some() {
                    self.search = None;
                    query_changed = true;
                }
            }
            KeyCode::Enter => {
                // Empty query → nothing to keep, so Enter just closes.
                // Otherwise drop out of editing but leave the filter on.
                let empty = self.search.as_ref().is_some_and(|s| s.query.is_empty());
                if empty {
                    self.search = None;
                    query_changed = true;
                } else if let Some(s) = self.search.as_mut() {
                    s.editing = false;
                }
            }
            KeyCode::Backspace => {
                if let Some(s) = self.search.as_mut() {
                    s.query.pop();
                    query_changed = true;
                }
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                if let Some(s) = self.search.as_mut() {
                    s.query.push(c);
                    query_changed = true;
                }
            }
            _ => {}
        }
        if query_changed {
            // Filtering re-lands the cursor on the best match; the
            // viewport must follow so the user sees what they're
            // narrowing to, even mid-wheel-detach.
            self.scroll_detached = false;
            self.recompute_visible();
        }
    }

    /// Look up the display label of a project by key. Used by the
    /// destructive-delete confirm modal so the prompt reads
    /// "Delete project foo/bar" instead of the raw key. Returns
    /// `None` when the key isn't in the local project cache (which
    /// shouldn't happen for any user-driven action, since the user
    /// can only target a project that's on screen).
    pub fn project_label_for(&self, key: &lazybox_core::ProjectKey) -> Option<String> {
        self.projects
            .get(key)
            .map(|p| crate::components::visible_rows::project_label(p, &self.workspaces))
    }

    /// Count how many workspaces in the local cache belong to the
    /// given project. Used by the project-delete confirm so the
    /// prompt can tell the user how much carnage they're authorizing
    /// ("Delete project X? Its 3 workspaces…").
    pub fn workspaces_in_project(&self, key: &lazybox_core::ProjectKey) -> usize {
        self.workspaces
            .values()
            .filter(|w| w.project_key.as_ref() == Some(key))
            .count()
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
        // Even an edge-clamped press (j on the last row) re-anchors a
        // wheel-detached viewport back onto the cursor.
        self.scroll_detached = false;
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
        self.set_cursor(selectable[target]);
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
            .filter(|w| workspace_attention_signals(w, &self.agents).contains(&signal))
            .count()
    }

    /// Drives the `? N input` indicator in the top header — a quick
    /// "agents stuck on prompts" tally.
    fn input_pending_count(&self) -> usize {
        self.count_visible_with_signal(AttentionSignal::AgentAsking)
    }

    /// Whether any visible workspace currently has an agent waiting on
    /// input. Drives the agent-waiting feature tip (#115); reads the
    /// same `AgentAsking` signal as the header counter and the `!`
    /// jump so the tip only shows when that jump would do something.
    pub fn has_asking_agent(&self) -> bool {
        self.input_pending_count() > 0
    }

    /// Whether any visible workspace's PR has failing / mixed CI.
    /// Drives the failing-CI feature tip (#115), keyed off the same
    /// signal as `Shift-F`.
    pub fn has_failing_ci(&self) -> bool {
        self.ci_failing_count() > 0
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
    /// row badge. Agent letters are declared by the agent itself
    /// ([`lazybox_agents::Agent::badge`] (via `lazybox_tui_core::agents`)) — `claude` → `C`, `codex` →
    /// `X`, `cursor` → `U` — so identity lives in one place and a new
    /// agent can't silently collide (#440). Resolution (registered badge
    /// or first-char fallback) belongs to the registry, not here.
    /// Non-agent kinds: `shell` → `S`, log tail → `L`.
    fn badge_letter(&self, kind: &TerminalKind) -> char {
        match kind {
            TerminalKind::Agent(id) => self.agent_registry.badge_for(id),
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
                *counts.entry(self.badge_letter(kind)).or_default() += 1;
            }
        }
        let mut entries: Vec<(char, usize)> = counts.into_iter().collect();
        entries.sort_by_key(|(c, _)| match *c {
            'S' => (1, 'S'),
            other => (0, other),
        });
        entries
    }

    /// The repo group the cursor's row belongs to, if any. Resolution:
    ///
    /// - cursor on a `RepoHeader` → that header.
    /// - cursor on a workspace / session / kind sub-row → the nearest
    ///   header above it (the cursor's group).
    ///
    /// The single source of truth for "which group does `Space` fold?" —
    /// shared by [`Self::toggle_repo_at_cursor`] and the footer hint so
    /// the two never disagree about when the shortcut applies (#338).
    pub fn cursor_repo(&self) -> Option<String> {
        match self.visible.get(self.cursor) {
            Some(VisibleRow::RepoHeader(name)) => Some(name.clone()),
            Some(VisibleRow::Workspace(_))
            | Some(VisibleRow::Session { .. })
            | Some(VisibleRow::KindHeader(_)) => self
                .visible
                .iter()
                .take(self.cursor + 1)
                .rev()
                .find_map(|r| match r {
                    VisibleRow::RepoHeader(name) => Some(name.clone()),
                    _ => None,
                }),
            None => None,
        }
    }

    /// True when the cursor's repo group is currently collapsed. `None`
    /// when the cursor isn't in a group at all. Drives the footer's
    /// collapse-vs-expand verb (#338).
    pub fn cursor_repo_collapsed(&self) -> Option<bool> {
        self.cursor_repo()
            .map(|repo| self.collapsed_repos.contains(&repo))
    }

    /// Toggle the collapsed flag for the repo at or above the
    /// cursor. Used by `Space`.
    ///
    /// On collapse, cursor snaps to the now-collapsed header so
    /// j/k from there land on adjacent headers cleanly.
    pub fn toggle_repo_at_cursor(&mut self) -> bool {
        let Some(repo) = self.cursor_repo() else {
            return false;
        };
        let was_collapsed = self.collapsed_repos.contains(&repo);
        if was_collapsed {
            self.collapsed_repos.remove(&repo);
        } else {
            self.collapsed_repos.insert(repo.clone());
        }
        self.recompute_visible();
        // Persist the new set to ~/.lazybox/config.yaml::ui.collapsed_repos
        // so the layout survives restart. Best-effort; an I/O
        // error here just means next launch starts expanded.
        let snapshot = self.collapsed_repos.clone();
        if let Err(e) = lazybox_config::Config::save_with(|c| c.ui.collapsed_repos = snapshot) {
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
            self.set_cursor(idx);
        }
        true
    }

    /// True when the cursor's repo group is currently pinned. `None`
    /// when the cursor isn't in a group at all. Drives the footer's
    /// pin-vs-unpin verb (#760).
    pub fn cursor_repo_pinned(&self) -> Option<bool> {
        self.cursor_repo()
            .map(|repo| self.pinned_repos.contains(&repo))
    }

    /// Pin / unpin the repo group at or above the cursor to the top of
    /// the sidebar (`p`). A fresh pin is appended, so pin order tracks
    /// the order the user pinned in. Returns `(repo, now_pinned)` so the
    /// caller can surface a footer notice, or `None` when the cursor
    /// isn't in a group.
    ///
    /// Pinning only reorders the groups — no rows are hidden — so
    /// `recompute_visible` keeps the cursor on the exact row the user
    /// was on (a workspace by key, a header by name). We deliberately do
    /// NOT re-park onto the header the way `toggle_repo_at_cursor`
    /// does: collapse needs that because it removes the rows under the
    /// cursor, whereas re-parking here would silently drop the user's
    /// workspace selection (and the right-pane / terminal context that
    /// follows it) on a pin that left their row fully visible.
    pub fn toggle_pin_at_cursor(&mut self) -> Option<(String, bool)> {
        let repo = self.cursor_repo()?;
        let now_pinned = if let Some(idx) = self.pinned_repos.iter().position(|r| r == &repo) {
            self.pinned_repos.remove(idx);
            false
        } else {
            self.pinned_repos.push(repo.clone());
            true
        };
        self.recompute_visible();
        // Persist to ~/.lazybox/config.yaml::ui.pinned_repos so the
        // order survives restart. Best-effort; a write error just means
        // the pins reset next launch.
        let snapshot = self.pinned_repos.clone();
        if let Err(e) = lazybox_config::Config::save_with(|c| c.ui.pinned_repos = snapshot) {
            tracing::warn!("save pinned_repos failed: {e}");
        }
        Some((repo, now_pinned))
    }

    /// True when the repo is currently pinned (used by the header
    /// render to draw the pin marker).
    pub fn is_repo_pinned(&self, name: &str) -> bool {
        self.pinned_repos.iter().any(|r| r == name)
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
        self.set_cursor(0);
        self.recompute_visible_inner(false);
    }

    fn recompute_after_workspace_removed(&mut self, removed_key: &SessionKey) {
        let removed_index = if self.selected_session_key() == Some(removed_key) {
            self.visible
                .iter()
                .position(|row| matches!(row, VisibleRow::Workspace(key) if key == removed_key))
        } else {
            None
        };
        self.recompute_visible();
        let Some(removed_index) = removed_index else {
            return;
        };

        let agent_workspaces = self.agent_workspace_keys();
        let target = self
            .visible
            .iter()
            .enumerate()
            .skip(removed_index)
            .find_map(|(index, row)| match row {
                VisibleRow::Workspace(key) if agent_workspaces.contains(key) => Some(index),
                _ => None,
            })
            .or_else(|| {
                self.visible
                    .iter()
                    .enumerate()
                    .skip(removed_index)
                    .find_map(|(index, row)| match row {
                        VisibleRow::Workspace(_) => Some(index),
                        _ => None,
                    })
            })
            .or_else(|| {
                self.visible
                    .iter()
                    .rposition(|row| matches!(row, VisibleRow::Workspace(_)))
            });
        if let Some(target) = target {
            self.set_cursor(target);
        }
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
                filters: &self.filters,
                sort_mode: self.sort_mode,
                show_inactive_in_inbox: self.show_inactive_in_inbox,
                projects: &self.projects,
                collapsed_repos: &self.collapsed_repos,
                pinned_repos: &self.pinned_repos,
                attention: &self.attention,
                agents: &self.agents,
                now: self.now(),
                search: self.search.as_ref(),
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
                    VisibleRow::RepoHeader(_) | VisibleRow::KindHeader(_) => false,
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

    /// State-aware short list for the footer hint bar.
    ///
    /// Catalog-driven: this method only decides *which* actions to
    /// surface right now. `catalog` is the model's runtime catalog
    /// ([`ActionDef::catalog`](lazybox_tui_core::action::ActionDef::catalog))
    /// — effective chords with
    /// `ui.action_keys` overrides already applied, including the
    /// generated per-agent rows — and `contextual_label` resolves the
    /// state-aware verb. Adding a new sidebar action means landing it
    /// in the catalog and pushing it here — the footer, `?` help, and
    /// right-click menu all pick it up automatically, and a user
    /// rebind shows up in the footer without any extra plumbing.
    ///
    /// Actions whose effective chord is a two-step leader sequence
    /// collapse into ONE group cell per leader (`g ▸ github`,
    /// `a ▸ agent`) — the which-key popup teaches the second level, so
    /// the footer only points at the door instead of listing every
    /// room (issue #304).
    pub fn contextual_bindings(
        &self,
        catalog: &[lazybox_tui_core::action::CatalogEntry],
        remote: bool,
    ) -> Vec<crate::Binding> {
        use crate::Binding;
        use lazybox_tui_core::action::{
            Action, ActionKind, Chord, KeyStroke, Param, contextual_label, leader_group_label,
        };

        let workspace = self.selected_workspace();
        let is_ready = self.merge_target_for_cursor().is_some();
        let mut actions: Vec<Action> = Vec::with_capacity(6);

        // A live multi-select makes the broadcast THE next action —
        // surface it first so the `v` marks visibly lead somewhere.
        // When any marked row is a PR behind its base, the bulk
        // update-branch rides alongside it.
        if !self.broadcast_selected.is_empty() {
            actions.push(Action::BroadcastToSelected);
            if self
                .broadcast_selected
                .iter()
                .filter_map(|k| self.workspace_by_key(k))
                .any(|w| w.pr.as_ref().is_some_and(|p| p.is_behind_base))
            {
                actions.push(Action::UpdateBranchSelected);
            }
        }

        // A PR behind its base can update its branch (the `g u` /
        // "Update branch" affordance).
        if workspace.is_some_and(|w| w.pr.as_ref().is_some_and(|p| p.is_behind_base)) {
            actions.push(Action::UpdateBranch);
        }

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
        // surface whenever a workspace is selected. `x x`
        // archive's "(kills sessions)" suffix flips automatically
        // via `contextual_label`.
        //
        // NOTE: `x x` ALSO deletes the project when the cursor
        // sits on a project header — wired in
        // `Model::dispatch_action(Archive)` via the polymorphic
        // session_key / focused_project_key fallback. We
        // deliberately don't add a second footer entry for the
        // header case: `x x` is the universal archive key,
        // visible muscle-memory is enough.
        if workspace.is_some() {
            // Any bound agent row stands in for the whole `a ▸ agent`
            // group — the leader collapse below folds every sibling
            // into the one cell, so which agent it names is moot.
            if let Some(id) = catalog.iter().find_map(|e| match (&e.kind, &e.param) {
                (ActionKind::SpawnAgent, Some(Param::Agent(id))) if !e.chords.is_empty() => {
                    Some(id.clone())
                }
                _ => None,
            }) {
                actions.push(Action::SpawnAgent(id));
            }
            actions.push(Action::SpawnShell);
            // The editor launches locally against a server-side worktree
            // path, so a remote client can't offer it (#742).
            if !remote {
                actions.push(Action::OpenEditor);
            }
            actions.push(Action::ToggleSnooze);
            actions.push(Action::Archive);
        }
        // Repo-group collapse/expand (`Space`) — the "group the
        // sessions" shortcut users couldn't find (#338). Surfaces
        // wherever the key would actually fold something: anywhere the
        // cursor resolves to a repo group (header, workspace, session,
        // or kind sub-row) — the same predicate the key dispatches on.
        if self.cursor_repo().is_some() {
            actions.push(Action::ToggleRepoGroup);
            actions.push(Action::ToggleRepoPin);
        }
        // Focus mode (`.`) surfaces only when the selected workspace
        // has a coding agent to maximize — otherwise the key is a
        // no-op, so advertising it would be noise. The `]]<digit>`
        // jumps live under the terminal `]]` leader (and its popup),
        // not the sidebar footer.
        if workspace.is_some_and(|w| {
            w.sessions
                .iter()
                .any(|s| matches!(s.kind, lazybox_core::SessionKind::Agent { .. }))
        }) {
            actions.push(Action::ToggleFocusMode);
        }
        // Creation actions live last in the row but Project comes
        // BEFORE Workspace: projects are containers; you need one
        // before a workspace makes sense. Reversed order read
        // backwards to dogfood users.
        actions.push(Action::NewProject);
        actions.push(Action::NewWorkspace);

        let mut out: Vec<Binding> = Vec::with_capacity(actions.len());
        let mut seen_leaders: Vec<KeyStroke> = Vec::new();
        for a in actions {
            let Some(entry) = catalog.iter().find(|e| {
                e.kind == a.kind()
                    && match (&a, &e.param) {
                        (Action::SpawnAgent(id), Some(Param::Agent(p))) => p == id,
                        _ => true,
                    }
            }) else {
                continue;
            };
            match entry.chords.first() {
                // A leader chord renders as its group cell — once per
                // leader, no matter how many siblings surfaced.
                Some(Chord::Seq(strokes)) => {
                    let head = strokes[0];
                    if seen_leaders.contains(&head) {
                        continue;
                    }
                    seen_leaders.push(head);
                    // Work is both a named leader menu and the primary
                    // contextual action. Keep the footer's useful verb
                    // (`fix CI`, `implement issue`, …); the popup itself
                    // still carries the stable `work` group title.
                    let label = if matches!(a, Action::Work) {
                        std::borrow::Cow::Borrowed(contextual_label(&a, workspace))
                    } else {
                        leader_group_label(entry.kind)
                            .map(std::borrow::Cow::Borrowed)
                            .unwrap_or_else(|| entry.label.clone())
                    };
                    out.push(Binding {
                        keys: std::borrow::Cow::Owned(format!("{} ▸", head.display())),
                        label,
                    });
                }
                _ => {
                    let label: std::borrow::Cow<'static, str> = match &a {
                        // A single-key remap of an agent row keeps its
                        // own name — there's no group cell to defer to.
                        Action::SpawnAgent(_) => entry.label.clone(),
                        // The verb tracks the cursor's group state so the
                        // footer never says "collapse" over an already-
                        // collapsed group (#338).
                        Action::ToggleRepoGroup => std::borrow::Cow::Borrowed(
                            if self.cursor_repo_collapsed() == Some(true) {
                                "expand group"
                            } else {
                                "collapse group"
                            },
                        ),
                        // The verb tracks the cursor's pin state so the
                        // footer never says "pin" over an already-pinned
                        // group.
                        Action::ToggleRepoPin => {
                            std::borrow::Cow::Borrowed(if self.cursor_repo_pinned() == Some(true) {
                                "unpin group"
                            } else {
                                "pin group"
                            })
                        }
                        _ => std::borrow::Cow::Borrowed(contextual_label(&a, workspace)),
                    };
                    out.push(Binding {
                        keys: entry.keys_display.clone(),
                        label,
                    });
                }
            }
        }
        out
    }
}

mod handlers;
pub(crate) mod pills;
mod render;

#[cfg(test)]
mod tests;

pub(crate) use lazybox_tui_core::inbox::{
    AttentionSignal, attention_gate, mailbox_membership, workspace_attention_signals,
};
/// The filter model + the pure attention producers moved to the
/// client-free `lazybox_tui_core::inbox` module (#731), re-exported at
/// the legacy `sidebar::*` paths so the rest of the crate keeps its
/// `crate::components::sidebar::*` imports.
///
/// Visibility mirrors the pre-move split: the filter model was `pub`,
/// so it stays reachable from outside the crate:
///
/// ```
/// use lazybox_tui::components::sidebar::Filter;
/// let _ = Filter::Author;
/// ```
///
/// The pills-side attention producers were `pub(crate)`, so reaching
/// one from outside `lazybox_tui` must NOT compile (guards against the
/// move accidentally widening `pub(crate)` to `pub`):
///
/// ```compile_fail
/// use lazybox_tui::components::sidebar::attention_gate;
/// ```
pub use lazybox_tui_core::inbox::{Filter, FilterAxis, FilterCtx, FilterSet};
// `workspace_needs_attention`'s production caller moved into
// `inbox::compute_visible` with the grouping logic (#731); inside `tui`
// only the attention-signal unit tests still exercise it, so the
// re-export is test-only.
#[cfg(test)]
pub(crate) use lazybox_tui_core::inbox::workspace_needs_attention;

// Re-export the ratatui-styled pills.rs items so callers in the rest of
// the crate keep their `crate::components::sidebar::*` import paths.
pub(crate) use pills::{
    badge_pill_style, relative_time, role_badge, status_pills, workspace_type_label,
};
#[cfg(test)]
pub(crate) use pills::{pill_for_tag, status_pill};

// Prompt builders moved to `lazybox_tui_core::prompts` (so `intent`,
// which also lives there, can call them without creating a dep
// cycle). Re-exported here at the legacy `sidebar::*` paths for
// back-compat.
pub use lazybox_tui_core::prompts::{
    build_fix_ci_prompt, build_fix_conflict_prompt, build_work_prompt,
};
