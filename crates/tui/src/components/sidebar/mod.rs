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

/// Which logical mailbox the sidebar is currently showing.
///
/// Three mutually-exclusive buckets, cycled via `Shift-S`:
///
/// - **Inbox** — actionable workspaces: not snoozed, primary task
///   is Open / Draft / In-Progress / In-Review. The default.
/// - **Inactive** — historical workspaces: primary task is Merged
///   or Closed. Useful for "where did I work on that PR last
///   week" — the data is already persisted, this just surfaces it.
/// - **Snoozed** — explicitly snoozed (`z` / `x z`).
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

/// Quick role-based filter layered on top of the mailbox. Default
/// `All` shows everything the mailbox would normally show; the other
/// variants drop workspaces whose primary task carries a different
/// `TaskRole`. Cycled with `f` in the sidebar. Workspaces with no
/// primary task fail any non-`All` filter (role lives on the task,
/// so there's nothing to compare).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RoleFilter {
    #[default]
    All,
    Author,
    Reviewer,
    Assignee,
    Mentioned,
}

/// How the sidebar orders workspaces within each repo group.
/// `Recent` is the legacy `updated_at desc` order; `ByRole` puts
/// authored PRs first then reviews-requested etc.; `ByRoleSplit`
/// (chip label `split`) splits each repo into a `PRs` and an
/// `Issues` section, preserving role ordering within each section.
/// Cycled via `o` in the sidebar.
///
/// Default is `ByRoleSplit` — most repos surface a mix of PRs and
/// issues, and the visual split is the natural way to scan ("what's
/// review-blocked vs what's still scoped as an issue?"). Recency
/// is one `o` press away.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortMode {
    Recent,
    ByRole,
    #[default]
    ByRoleSplit,
}

impl SortMode {
    pub fn next(self) -> Self {
        match self {
            SortMode::Recent => SortMode::ByRole,
            SortMode::ByRole => SortMode::ByRoleSplit,
            SortMode::ByRoleSplit => SortMode::Recent,
        }
    }

    pub fn chip_label(self) -> &'static str {
        match self {
            SortMode::Recent => "recent",
            SortMode::ByRole => "by-role",
            SortMode::ByRoleSplit => "split",
        }
    }
}

/// Which kind of work a workspace primarily represents. Used by the
/// `[split]` sort mode to bucket each repo into a `PRs` section, an
/// `Issues` section, and an `Other` section. A workspace with an
/// attached PR is a `Pr` regardless of whether it also links issues —
/// the PR is what the user typically interacts with first; GitHub
/// issues and Linear tickets are `Issue`; an empty scratch workspace
/// (no PR, no issues) is `Other` so it isn't mislabeled as an issue.
///
/// Variant order matters: `Pr` is declared first so the derived
/// `Ord` puts PRs ahead of issues ahead of untyped — same intent the
/// `[split]` mode needs when partitioning the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WorkspaceKind {
    Pr,
    Issue,
    Other,
}

impl WorkspaceKind {
    /// Classify a workspace. A filled PR slot wins; otherwise any
    /// attached issue (GitHub or Linear) makes it an issue bucket; a
    /// workspace with no task at all is `Other`.
    pub fn classify(w: &lazybox_core::Workspace) -> Self {
        if w.pr.is_some() {
            WorkspaceKind::Pr
        } else if !w.gh_issues.is_empty() || !w.linear_issues.is_empty() {
            WorkspaceKind::Issue
        } else {
            WorkspaceKind::Other
        }
    }

    /// Header label rendered in the sidebar list.
    pub fn header_label(self) -> &'static str {
        match self {
            WorkspaceKind::Pr => "PRs",
            WorkspaceKind::Issue => "Issues",
            WorkspaceKind::Other => "Other",
        }
    }

    /// Single-letter marker rendered before the header label —
    /// mirrors the per-row `[PR]` / `[I]` pill colouring so the
    /// section header lines up visually with the rows under it.
    /// Untyped workspaces have no per-row glyph, so the section gets
    /// a neutral dot rather than a type letter.
    pub fn header_marker(self) -> char {
        match self {
            WorkspaceKind::Pr => 'P',
            WorkspaceKind::Issue => 'I',
            WorkspaceKind::Other => '·',
        }
    }
}

/// Sort key for the `ByRole*` modes. Author first (your own PRs are
/// usually the most actionable), then Reviewer (someone's waiting on
/// you), then Assignee, then Mentioned. Lower number sorts first.
pub fn role_rank(role: Option<lazybox_core::TaskRole>) -> u8 {
    match role {
        Some(lazybox_core::TaskRole::Author) => 0,
        Some(lazybox_core::TaskRole::Reviewer) => 1,
        Some(lazybox_core::TaskRole::Assignee) => 2,
        Some(lazybox_core::TaskRole::Mentioned) => 3,
        None => 4,
    }
}

impl RoleFilter {
    /// Cycle order matches the `f`-key rotation.
    pub fn next(self) -> Self {
        match self {
            RoleFilter::All => RoleFilter::Author,
            RoleFilter::Author => RoleFilter::Reviewer,
            RoleFilter::Reviewer => RoleFilter::Assignee,
            RoleFilter::Assignee => RoleFilter::Mentioned,
            RoleFilter::Mentioned => RoleFilter::All,
        }
    }

    /// Short label for the title chip.
    pub fn chip_label(self) -> &'static str {
        match self {
            RoleFilter::All => "all",
            RoleFilter::Author => "author",
            RoleFilter::Reviewer => "reviewer",
            RoleFilter::Assignee => "assignee",
            RoleFilter::Mentioned => "mentioned",
        }
    }

    /// Decide whether a workspace passes this filter. `None` means
    /// "no primary task" — only `All` accepts it.
    pub fn accepts(self, role: Option<lazybox_core::TaskRole>) -> bool {
        let Some(role) = role else {
            return matches!(self, RoleFilter::All);
        };
        match self {
            RoleFilter::All => true,
            RoleFilter::Author => role == lazybox_core::TaskRole::Author,
            RoleFilter::Reviewer => role == lazybox_core::TaskRole::Reviewer,
            RoleFilter::Assignee => role == lazybox_core::TaskRole::Assignee,
            RoleFilter::Mentioned => role == lazybox_core::TaskRole::Mentioned,
        }
    }
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
    /// Item-kind group header — only emitted in
    /// `SortMode::ByRoleSplit`, nested under each repo header.
    /// Splits the workspaces of one repo into `PRs` and `Issues`
    /// sections so the visual hierarchy is `repo > kind > workspace`.
    /// Non-selectable like `RepoHeader`; j/k navigation walks
    /// straight past it (cursor still parks on it for click /
    /// collapse interactions, same as a repo header).
    KindHeader(WorkspaceKind),
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

/// Free-text search scoped to a single project (repo group). Invoked
/// with `/` and filters that project's PRs + Issues live as the user
/// types — fuzzy match on title, substring match on number. Other
/// projects are left untouched (the search is deliberately scoped to
/// one group, not global).
///
/// `editing` is true while the bottom input bar is capturing
/// keystrokes (between `/` and `Enter`/`Esc`). `Enter` keeps the
/// query applied but stops capturing so j/k navigates the results;
/// `Esc` clears the query and closes the bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchState {
    /// Repo-header label the search is scoped to — matched against
    /// `visible_rows::group_label` so only that project's rows are
    /// filtered. Captured from the row under the cursor when `/` opens.
    pub scope: String,
    pub query: String,
    pub editing: bool,
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
    /// the future; defaults are the indicators lazybox already
    /// surfaces as badges on workspace rows.
    pub attention: usize,
}

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
    /// Live filter on top of the mailbox. Cycles via `f`. Default
    /// `All` is a no-op; the other variants restrict the visible
    /// list to workspaces whose primary task role matches.
    role_filter: RoleFilter,
    /// Sort order within each repo group. Default is recency; `o`
    /// cycles to `ByRole` and `ByRoleSplit`. See [`SortMode`].
    sort_mode: SortMode,
    /// Mirror of the daemon's live-terminals set, scoped to what we
    /// need for the workspace-row runner badges (e.g. ` C  S 2` for
    /// one Claude + two shells running). Populated from `Event::Snapshot`
    /// and kept in sync via `TerminalSpawned` / `TerminalExited`.
    running_terminals: HashMap<TerminalId, (SessionKey, TerminalKind)>,
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
    /// The single per-session agent status, keyed by workspace — the
    /// source of truth for the `?` asking pill / `? N input` header
    /// counter / `!` jump, the animated `Working` spinner, and the `✓`
    /// done mark (#80). One [`AgentState`] per session (the four states
    /// share one UI slot, so they're mutually exclusive by
    /// construction; #327). Every reading folds in through
    /// [`crate::agent_attention::apply_agent_state`], which validates the
    /// transition. Source: `Event::AgentState` broadcasts from the
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
        Self {
            id,
            workspaces: HashMap::new(),
            visible: Vec::new(),
            collapsed_repos: BTreeSet::new(),
            repo_summaries: BTreeMap::new(),
            cursor: 0,
            scroll: 0,
            scroll_detached: false,
            last_viewport: 0,
            rendered_scroll: 0,
            mailbox: Mailbox::Inbox,
            role_filter: RoleFilter::default(),
            sort_mode: SortMode::default(),
            running_terminals: HashMap::new(),
            attention: lazybox_config::AttentionConfig::default(),
            projects: BTreeMap::new(),
            default_agent: "claude".to_string(),
            show_inactive_in_inbox: false,
            ascii_glyphs: false,
            pending_notifications: Vec::new(),
            pending_asking_notices: Vec::new(),
            agents: std::collections::HashMap::new(),
            working_spinner_frame: 0,
            spinner_epoch: std::time::Instant::now(),
            filter_chip_rect: None,
            sort_chip_rect: None,
            now_override: None,
            search: None,
            broadcast_selected: std::collections::HashSet::new(),
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
        if !self
            .agents
            .values()
            .any(|s| matches!(s, lazybox_ipc::AgentState::Working))
        {
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

    /// Distinct agent ids with a running terminal in `workspace_key`.
    /// The order is unspecified (it walks a hash map), but the only
    /// consumer ([`Self::work_target_agent`]) keys off the *set*, never
    /// the order.
    pub fn running_agent_ids(&self, workspace_key: &SessionKey) -> Vec<String> {
        let mut ids: Vec<String> = Vec::new();
        for (sk, kind) in self.running_terminals.values() {
            if sk != workspace_key {
                continue;
            }
            if let TerminalKind::Agent(id) = kind
                && !ids.contains(id)
            {
                ids.push(id.clone());
            }
        }
        ids
    }

    /// Pick the agent `w` ("work on this") should target for a
    /// workspace. An agent already running here wins over the default
    /// so `w` continues the existing conversation (e.g. an open Codex
    /// session) instead of always spawning a fresh default agent —
    /// `rewrite_spawn_to_inject` then injects into it because the
    /// resolved agent id matches the running one.
    ///
    /// Tie-break when several DIFFERENT agents run at once: prefer the
    /// default if it's among them, otherwise fall back to the default
    /// (a fresh spawn) so the outcome stays predictable. The scoped
    /// `w c` / `w x` chords are how the user targets a specific one.
    pub fn work_target_agent(&self, workspace_key: &SessionKey, default_agent: &str) -> String {
        let running = self.running_agent_ids(workspace_key);
        if running.iter().any(|id| id == default_agent) {
            return default_agent.to_string();
        }
        match running.as_slice() {
            [only] => only.clone(),
            _ => default_agent.to_string(),
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

    /// Override the attention thresholds + initial collapse set
    /// from `~/.lazybox/config.yaml`. Call once after construction
    /// (typically in main, between `Sidebar::new` and the first
    /// daemon Subscribe).
    pub fn apply_config(
        &mut self,
        attention: lazybox_config::AttentionConfig,
        collapsed_repos: BTreeSet<String>,
        default_agent: Option<String>,
        display: &lazybox_config::DisplayConfig,
    ) {
        self.attention = attention;
        self.collapsed_repos = collapsed_repos;
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
    /// Click on the role-filter chip cycles it — same effect as
    /// pressing `f`. Returns true on a hit so the caller knows the
    /// click was consumed and a redraw is needed.
    pub fn click_to_cycle_filter(&mut self, col: u16, row: u16) -> bool {
        let Some(rect) = self.filter_chip_rect else {
            return false;
        };
        if row != rect.y || col < rect.x || col >= rect.x + rect.width {
            return false;
        }
        self.cycle_role_filter();
        true
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

    /// Move the cursor onto the RepoHeader row for the given project.
    /// Returns true on a hit. Used by the just-created-a-project flow
    /// so the user lands on their new project ready to press `n` to
    /// add a workspace. Without this, the cursor stays wherever it
    /// was before and the new RepoHeader is unreachable via j/k
    /// (header rows are skipped by `move_cursor_by`), which made the
    /// new-project flow feel broken.
    pub fn focus_project_header(&mut self, key: &lazybox_core::ProjectKey) -> bool {
        let label = match self.projects.get(key) {
            Some(p) => p.name.clone(),
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

    pub fn role_filter(&self) -> RoleFilter {
        self.role_filter
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

    /// Cycle the role filter (`All → Author → … → All`) and rebuild
    /// the visible list. Returns the new filter so the caller can
    /// surface a footer notice if it wants. Cursor is reset because
    /// the row the user was parked on may have just been filtered
    /// out — landing on the new top is less surprising than landing
    /// off-screen.
    pub fn cycle_role_filter(&mut self) -> RoleFilter {
        self.role_filter = self.role_filter.next();
        self.reset_cursor_and_recompute();
        self.role_filter
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
    /// teaches the next actions (issue #100). A list emptied by a
    /// role filter, a non-Inbox mailbox, or a search query is NOT
    /// this case — those are user-driven narrowings, not first-run.
    pub fn is_getting_started(&self) -> bool {
        self.visible.is_empty()
            && self.mailbox == Mailbox::Inbox
            && self.role_filter == RoleFilter::All
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
            .map(|p| (p.key.clone(), p.name.clone()))
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
                .find(|p| &p.name == name)
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
                            .find(|p| &p.name == name)
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
            Some(s) if s.scope == scope => s.editing = true,
            _ => {
                self.search = Some(SearchState {
                    scope,
                    query: String::new(),
                    editing: true,
                });
            }
        }
        // The query is scoped to the CURSOR's project — make sure
        // that's the project on screen while the user types.
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
        self.projects.get(key).map(|p| p.display_name())
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
                role_filter: self.role_filter,
                sort_mode: self.sort_mode,
                show_inactive_in_inbox: self.show_inactive_in_inbox,
                projects: &self.projects,
                collapsed_repos: &self.collapsed_repos,
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
        if !self.broadcast_selected.is_empty() {
            actions.push(Action::BroadcastToSelected);
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
            actions.push(Action::OpenEditor);
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
mod pills;
mod render;

#[cfg(test)]
mod tests;

// Re-export pills.rs items so callers in the rest of the crate
// keep their `crate::components::sidebar::*` import paths.
pub(crate) use pills::{
    AttentionSignal, attention_gate, badge_pill_style, mailbox_membership, relative_time,
    role_badge, status_pills, workspace_attention_signals, workspace_needs_attention,
    workspace_type_label,
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
