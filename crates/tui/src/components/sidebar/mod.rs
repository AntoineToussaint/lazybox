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
use lazybox_core::{SessionId, SessionKey, TaskId, Workspace};
use lazybox_ipc::{Command, Event, TerminalId, TerminalKind};
use ratatui::Frame;
use ratatui::prelude::*;
use ratatui::widgets::*;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

// The inbox view-model types + grouping/sort/filter/search logic moved
// to the client-free `lazybox_tui_core::inbox` module (#731) so the
// desktop client builds the same sidebar from the same code. Re-exported
// at the legacy `sidebar::*` paths so render/dispatch call sites keep
// their imports.
pub use lazybox_tui_core::inbox::{
    Mailbox, RepoSummary, SearchState, SortMode, TicketTreeMeta, VisibleRow, WorkspaceKind,
    role_rank,
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
    /// Workspace keys the user has starred ("focused"), in focus order.
    /// Starred workspaces are lifted into the synthetic `★ Focused`
    /// section at the top of the sidebar, across repos. Persisted to
    /// `ui.focused_workspaces`. A `Vec`, not a set — the order the user
    /// starred in is the display order, mirroring `pinned_repos` at
    /// workspace granularity.
    focused_workspaces: Vec<SessionKey>,
    /// User-defined Spaces — the higher-level grouping tier above repo
    /// headers (#860). Assignment maps a source label to a Space and is
    /// persisted to `ui.spaces`; the tier only renders when it yields
    /// ≥2 distinct Spaces (see `compute_visible`).
    spaces: Vec<lazybox_config::SpaceConfig>,
    /// Spaces the user collapsed. Mirrors `collapsed_repos` one tier up;
    /// persisted to `ui.collapsed_spaces`.
    collapsed_spaces: BTreeSet<String>,
    /// Set once `apply_config` has seeded the persisted view state
    /// (stars, pins, collapse, Spaces). Until then the in-memory lists
    /// are empty by construction, not by user intent, so anything that
    /// interprets "not in the list" as "user removed it" — the
    /// snapshot-time stale-star prune in particular — must not run
    /// (#1244). Persisting user *toggles* stays allowed pre-seed: they
    /// write targeted add/remove edits, never the whole list.
    config_seeded: bool,
    /// Whether the first daemon Snapshot may prune persisted stars that
    /// match none of its workspaces (#1202/#1205). True for the embedded
    /// client, whose daemon snapshot is the same store the config was
    /// written against; disabled by `Model::with_remote`, where the
    /// snapshot describes *another machine's* workspace set and a local
    /// star that doesn't appear there is not stale (#1244).
    snapshot_prune: bool,
    /// Parent tickets whose visible descendant rows are folded.
    collapsed_tickets: HashSet<TaskId>,
    /// Derived depth/disclosure metadata for each visible ticket row.
    ticket_tree: HashMap<SessionKey, TicketTreeMeta>,
    /// Per-repo counters computed during `recompute_visible`. Keys
    /// are the same display strings used by `VisibleRow::RepoHeader`.
    repo_summaries: BTreeMap<String, RepoSummary>,
    /// Stacked-PR relationships, recomputed during `recompute_visible`
    /// over the full workspace set (issue #969). Keyed by the workspace's
    /// session key; present only for workspaces whose PR participates in a
    /// stack (has a parent PR or children stacked on it). Read by the row
    /// builder for the `⇗` indicator and by the merge dispatch to warn
    /// before merging a child ahead of its still-open parent.
    stacks: HashMap<SessionKey, lazybox_core::StackPosition>,
    /// Batched-recompute state for a daemon-event drain (#1030). While
    /// `defer_recompute` is set — the model brackets a whole drain batch
    /// with `begin_recompute_batch` / `flush_recompute` — the O(N log N)
    /// `recompute_visible` records that a rebuild is owed in
    /// `recompute_pending` instead of running per event, so a poll sweep
    /// of N workspace upserts rebuilds the visible list once rather than
    /// N times. Removal and mailbox resets bypass it (they read the fresh
    /// list to place the cursor).
    defer_recompute: bool,
    recompute_pending: bool,
    /// Test-only count of full visible-list rebuilds, so the drain
    /// coalescing regression can assert one rebuild per batch (#1030).
    #[cfg(test)]
    recompute_count: usize,
    /// Monotonic revision of everything `sync_panes` projects from this
    /// sidebar (#1237): bumped on every daemon event and every visible
    /// rebuild. Lets the per-keystroke pane sync skip its Workspace
    /// clone + full projection when nothing it reads can have changed.
    pane_state_rev: u64,
    /// Monotonic version bumped on every `recompute_visible_inner` — i.e.
    /// whenever the daemon pushes workspace data (all task/CI/review/label
    /// content arrives as an upsert that recomputes) or a local
    /// filter/sort/collapse/pin change re-projects the list. Feeds the
    /// render-line cache signature (#1090) so the expensive per-row line
    /// build is skipped on the flood of `TerminalOutput`-driven redraws a
    /// chatty agent triggers, which never touch workspace state.
    data_version: u64,
    /// Memoized output of `prebuild_workspace_lines`, keyed by
    /// [`Sidebar::workspace_lines_signature`]. A cache hit (nothing the
    /// sidebar draws has changed since the last frame) skips the whole
    /// per-row `build_row` + `render_table` pass and just clones the
    /// finished lines — the fix for #1090's render stall, where streaming
    /// terminal output repainted the entire sidebar tens of times a
    /// second. A miss (or `None`) rebuilds and re-stores. A stale signature
    /// can only cost a frame of cosmetic lag that the next redraw heals —
    /// dispatch always reads live state, never this cache.
    #[cfg(not(test))]
    workspace_line_cache: Option<(u64, std::rc::Rc<Vec<Option<Line<'static>>>>)>,
    /// In tests we also track how many times the cache was actually
    /// (re)built, so the #1090 regression can assert a hit skips the
    /// rebuild.
    #[cfg(test)]
    workspace_line_cache: Option<(u64, std::rc::Rc<Vec<Option<Line<'static>>>>)>,
    #[cfg(test)]
    workspace_line_builds: std::cell::Cell<usize>,
    /// One-pass memo for the header's attention counters (2026-08-19
    /// audit, U4). The header used to run 7 independent
    /// O(visible × activity) scans per frame — each allocating an
    /// attention-signal Vec per workspace — to produce a handful of
    /// integers. Keyed by the same inputs that can change them
    /// (data_version, agent states, selection marks).
    header_counters_cache: Option<(u64, render::HeaderCounters)>,
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
    /// Model + reasoning-effort label per live agent terminal, shown next
    /// to its runner badge when `ui.show_agent_model` is on. Seeded from a
    /// spawn's tier label (`TerminalSpawned.model_label` / the snapshot)
    /// and superseded by the daemon's live PTY reading
    /// (`Event::TerminalModelChanged`, e.g. Codex's `<model> <effort>`
    /// footer). Keyed by terminal so two agents in one workspace keep
    /// distinct labels; pruned on `TerminalExited`.
    terminal_models: HashMap<TerminalId, String>,
    /// Agent-declared compact glyphs for the model badge: tier
    /// `(badge_letter, label)` → `short` (`('C', "Opus") → "O"`),
    /// aggregated across every agent's model menu. The badge (`◆O`) reads a
    /// declared short here and falls back to the label's first character
    /// when a key isn't present (#1068). Keyed by the agent's badge letter
    /// so two agents sharing a tier label keep distinct shorts. Refreshed
    /// whenever the model menus reload (`set_model_shorts`).
    model_shorts: HashMap<(char, String), String>,
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
    /// Commit/PR conventions injected into the `w` work brief. Defaults
    /// to Conventional Commits; the model wires this from YAML
    /// (`conventions:`) at startup so an interactive `w` honors the same
    /// house style as autonomous spawns.
    conventions: lazybox_core::Conventions,
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
    /// Workspaces mid-spawn: provisioning a first session — cloning,
    /// creating the worktree, running setup, launching the agent —
    /// before any terminal exists to report an `AgentState` (#1069).
    /// Driven by `Event::WorktreeProgress`: a step `Started` / `Progress`
    /// marks the workspace spawning; the first live `AgentState`, the
    /// matching `TerminalSpawned`, or a `Failed` step clears it. Renders
    /// the animated "spawning" arc in the row's shared state slot so a
    /// spawn reads as *coming up* instead of a blank row until the agent
    /// is live. Independent of `agents` above, which only gains an entry
    /// once a terminal reports state.
    spawning: std::collections::HashSet<SessionKey>,
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
    /// Workspace keys the active search's scope covers — the rows whose
    /// titles get the match highlight (#1099). Precomputed once per
    /// [`Self::recompute_visible`] from the same `search_scope_covers`
    /// predicate the visible-row filter uses, so a highlighted row is
    /// exactly a row the search kept (no drift) and `render` needs only an
    /// O(1) lookup instead of re-deriving each row's group label per frame.
    /// Empty when no search is active.
    searched_keys: std::collections::HashSet<SessionKey>,
    /// Workspace rows the user multi-selected with `v` (or swept with
    /// Shift-↑/↓). While non-empty, every bulk-appropriate workspace
    /// action targets this whole set instead of the cursor row (#932) —
    /// broadcast (`Shift-B`) is only one such consumer. Keys, not row
    /// indices, so the marks survive re-sorts and j/k navigation. Marked
    /// ⇒ visible is an invariant: `recompute_visible` prunes marks on
    /// rows the projection hides (filter / mailbox / search / removal,
    /// #1243), and Esc or a successful send clears the set.
    broadcast_selected: std::collections::HashSet<SessionKey>,
    /// Live Shift-↑/↓ sweep state: `(anchor, cursor-at-last-extend)`.
    /// The anchor is the row the sweep started on; comparing the current
    /// cursor against the stored last-extend position is what detects an
    /// uninterrupted run of Shift-arrows — any other cursor move restarts
    /// the sweep. `None` between sweeps and after `v` / Esc (#1243).
    sweep: Option<(SessionKey, Option<SessionKey>)>,
    /// Mirror of `ui.keep_awake` as loaded at startup. When set, the
    /// header paints a small "awake" badge while any agent is
    /// `Working` — the same condition under which the daemon holds
    /// its OS sleep inhibitor — so the user can see the machine is
    /// being kept awake and why. The daemon re-reads the flag live;
    /// this client-side mirror refreshes on restart.
    keep_awake: bool,
    /// Mirror of `ui.show_agent_model` (default on). When set, each agent
    /// runner badge is followed by its model + effort label
    /// ([`terminal_models`](Self::terminal_models)); off keeps the sidebar
    /// compact. Refreshes on restart.
    show_agent_model: bool,
    /// Running token totals per agent id, joined from `AgentRunStarted` +
    /// `AgentUsage`. Drives the always-visible per-provider usage summary
    /// in the header (#1059).
    usage: lazybox_tui_core::usage::UsageTracker,
    /// Last reset countdown parsed from a usage-limit banner, per agent
    /// id (`AgentUsageLimit.reset_hint`). Shown as the summary's ` ·
    /// resets 3pm` fragment while that agent is actually limited (the
    /// hint is meaningless once it recovers).
    usage_reset: HashMap<String, String>,
    /// Mirror of `ui.usage_summary` (default on) — gates the header row.
    usage_summary: bool,
    /// Mirror of `ui.usage_budgets`: agent id → plan-window token budget,
    /// the denominator for the summary's percentage.
    usage_budgets: BTreeMap<String, u64>,
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
            focused_workspaces: Vec::new(),
            spaces: Vec::new(),
            collapsed_spaces: BTreeSet::new(),
            config_seeded: false,
            snapshot_prune: true,
            collapsed_tickets: HashSet::new(),
            ticket_tree: HashMap::new(),
            repo_summaries: BTreeMap::new(),
            stacks: HashMap::new(),
            defer_recompute: false,
            recompute_pending: false,
            #[cfg(test)]
            recompute_count: 0,
            pane_state_rev: 0,
            data_version: 0,
            workspace_line_cache: None,
            header_counters_cache: None,
            #[cfg(test)]
            workspace_line_builds: std::cell::Cell::new(0),
            cursor: 0,
            scroll: 0,
            scroll_detached: false,
            last_viewport: 0,
            rendered_scroll: 0,
            mailbox: Mailbox::Inbox,
            filters: FilterSet::default(),
            sort_mode: SortMode::default(),
            running_terminals: HashMap::new(),
            terminal_models: HashMap::new(),
            model_shorts: HashMap::new(),
            agent_registry: lazybox_tui_core::agents::registry(),
            attention: lazybox_config::AttentionConfig::default(),
            projects: BTreeMap::new(),
            default_agent: "claude".to_string(),
            conventions: lazybox_core::Conventions::default(),
            show_inactive_in_inbox: false,
            ascii_glyphs: false,
            pending_notifications: Vec::new(),
            pending_asking_notices: Vec::new(),
            agents: std::collections::HashMap::new(),
            spawning: std::collections::HashSet::new(),
            agent_terminal_states: std::collections::HashMap::new(),
            working_spinner_frame: 0,
            spinner_epoch: std::time::Instant::now(),
            filter_chip_rect: None,
            sort_chip_rect: None,
            search_chip_rect: None,
            search_bar_rect: None,
            now_override: None,
            search: None,
            searched_keys: std::collections::HashSet::new(),
            broadcast_selected: std::collections::HashSet::new(),
            sweep: None,
            keep_awake: false,
            show_agent_model: true,
            usage: lazybox_tui_core::usage::UsageTracker::default(),
            usage_reset: HashMap::new(),
            usage_summary: true,
            usage_budgets: BTreeMap::new(),
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

    /// Record whether `ui.show_agent_model` is on — gates the per-agent
    /// model + effort label rendered beside each runner badge.
    pub fn set_show_agent_model(&mut self, show: bool) {
        self.show_agent_model = show;
    }

    /// Replace the tier `(badge_letter, label) → short` map that
    /// abbreviates the model badge (`◆O`). Built from every agent's model
    /// menu whenever the menus reload (#1068).
    pub fn set_model_shorts(&mut self, shorts: HashMap<(char, String), String>) {
        self.model_shorts = shorts;
    }

    /// Record whether `ui.usage_summary` is on — gates the always-visible
    /// per-provider usage row in the header.
    pub fn set_usage_summary(&mut self, show: bool) {
        self.usage_summary = show;
    }

    /// Load the per-agent plan-window token budgets (`ui.usage_budgets`),
    /// the denominator for the usage summary's percentage.
    pub fn set_usage_budgets(&mut self, budgets: BTreeMap<String, u64>) {
        self.usage_budgets = budgets;
    }

    /// Bind a structured run to its agent so the run's later usage events
    /// can be attributed (`AgentRunStarted`).
    pub fn note_agent_run(&mut self, run_id: lazybox_ipc::AgentRunId, agent_id: &str) {
        self.usage.note_run(run_id, agent_id);
    }

    /// Observe one usage report for a run's in-flight turn (`AgentUsage`).
    pub fn add_agent_usage(
        &mut self,
        run_id: lazybox_ipc::AgentRunId,
        usage: &lazybox_ipc::AgentUsage,
    ) {
        self.usage.observe_usage(run_id, usage);
    }

    /// Commit a completed turn's usage into the running per-provider total
    /// (`AgentTurnFinished`).
    pub fn commit_agent_turn(&mut self, run_id: lazybox_ipc::AgentRunId) {
        self.usage.commit_turn(&run_id);
    }

    /// Drop a finished run's binding, committing any turn still in flight
    /// first; its accumulated total stays (`AgentRunFinished`).
    pub fn finish_agent_run(&mut self, run_id: lazybox_ipc::AgentRunId) {
        self.usage.finish_run(&run_id);
    }

    /// Observe usage the metering proxy attributed to an agent directly
    /// (`AgentSessionUsage`) — the data source for interactive terminals
    /// and for Codex, which emit no structured `AgentUsage` (#1109).
    pub fn add_agent_session_usage(&mut self, agent_id: &str, usage: &lazybox_ipc::AgentUsage) {
        self.usage.observe_session_usage(agent_id, usage);
    }

    /// Record a provider plan-quota report (`AgentProviderQuota`) — the
    /// "can I keep working?" 5h/weekly headroom that mirrors Claude's
    /// `/usage` and Codex's `/status`. Merged per window by the tracker.
    pub fn note_provider_quota(&mut self, agent_id: &str, quota: lazybox_ipc::ProviderQuota) {
        self.usage.note_quota(agent_id, quota);
    }

    /// Attribute a usage-limit reset hint to the terminal's agent, so the
    /// summary can show ` · resets 3pm` while that provider is limited
    /// (`AgentUsageLimit`). A hint for a terminal we don't track is
    /// dropped.
    pub fn note_usage_limit_reset(&mut self, terminal_id: TerminalId, reset_hint: String) {
        if let Some(agent_id) = self.terminal_agent_id(terminal_id) {
            self.usage_reset.insert(agent_id, reset_hint);
        }
    }

    /// The agent id running in a terminal, if it is an agent terminal.
    fn terminal_agent_id(&self, terminal_id: TerminalId) -> Option<String> {
        match self.running_terminals.get(&terminal_id) {
            Some((_, TerminalKind::Agent(id))) => Some(id.clone()),
            _ => None,
        }
    }

    /// True while any of `agent_id`'s live terminals sits in the
    /// `LimitReached` block — the window in which its stored reset hint is
    /// still meaningful.
    fn agent_is_limited(&self, agent_id: &str) -> bool {
        self.agent_terminal_states
            .iter()
            .any(|(terminal_id, (_, state))| {
                *state == lazybox_ipc::AgentState::LimitReached
                    && self.terminal_agent_id(*terminal_id).as_deref() == Some(agent_id)
            })
    }

    /// The always-visible per-provider usage summaries, in stable (id)
    /// order. The display set is every agent with real accumulated usage,
    /// plus any agent with a live terminal *and* a configured plan budget —
    /// the budget is what turns the row into a real quota bar. A live
    /// terminal without a budget and without usage is deliberately excluded:
    /// it has no real figure to show and would render a meaningless
    /// "Claude 0 used" (#1109). Empty when the summary is disabled or
    /// nothing qualifies. The reset fragment is folded in only while that
    /// agent is actually limited.
    fn usage_summaries(&self) -> Vec<lazybox_tui_core::usage::UsageSummary> {
        if !self.usage_summary {
            return Vec::new();
        }
        let mut agent_ids: BTreeSet<&str> = BTreeSet::new();
        for (_, kind) in self.running_terminals.values() {
            if let TerminalKind::Agent(id) = kind
                && self.usage_budgets.contains_key(id.as_str())
            {
                agent_ids.insert(id.as_str());
            }
        }
        agent_ids.extend(self.usage.agents_with_usage());
        // Quota-only agents surface too: "can I keep working?" is worth
        // showing for a provider that has reported plan headroom even if no
        // committed token total has landed yet.
        agent_ids.extend(self.usage.agents_with_quota());
        let now_unix = chrono::Utc::now().timestamp();
        agent_ids
            .into_iter()
            .map(|agent_id| {
                let label = self.agent_registry.display_name_for(agent_id);
                let reset = self
                    .agent_is_limited(agent_id)
                    .then(|| self.usage_reset.get(agent_id).cloned())
                    .flatten();
                let (quota_5h, quota_weekly) = match self.usage.quota_for(agent_id) {
                    Some(quota) => (
                        format_quota_window(quota.five_hour, now_unix),
                        format_quota_window(quota.weekly, now_unix),
                    ),
                    None => (None, None),
                };
                lazybox_tui_core::usage::UsageSummary::new(
                    label,
                    self.usage.tokens_for(agent_id),
                    self.usage_budgets.get(agent_id).copied(),
                    reset,
                )
                .with_quota(quota_5h, quota_weekly)
            })
            .filter(|summary| {
                // Keep only rows that actually say something. A quota-only
                // agent (surfaced solely by `agents_with_quota`) whose every
                // window has gone stale past its reset drops both quota
                // fragments to `None` above; with no tokens, budget, or reset
                // it would otherwise render as a bare "X 0 used" — the same
                // meaningless row #1109 excludes for budget-less terminals.
                summary.tokens > 0
                    || summary.budget.is_some()
                    || summary.reset.is_some()
                    || summary.quota_5h.is_some()
                    || summary.quota_weekly.is_some()
            })
            .collect()
    }

    /// True while ≥1 agent in the sidebar is `Working` — the same
    /// predicate the daemon's keep-awake watcher inhibits sleep on.
    fn any_agent_working(&self) -> bool {
        self.agents
            .values()
            .any(|s| matches!(s, lazybox_ipc::AgentState::Working))
    }
    /// True while `session_key`'s workspace is provisioning its first
    /// spawn and no terminal has reported an `AgentState` yet — drives
    /// the row's "spawning" arc (#1069). Reads the private `spawning` set.
    pub fn is_spawning(&self, session_key: &SessionKey) -> bool {
        self.spawning.contains(session_key)
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
        // The one shared frame counter drives both the `Working` braille
        // spinner and the `Spawning` arc (#1069), so advance it while
        // either is on screen.
        if !self.any_agent_working() && self.spawning.is_empty() {
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
        // While a spawn is in flight and the arc is what's actually on
        // screen, folding in the agent's first reading *does* change the
        // display: it clears the arc. This holds even for `Idle`, which the
        // absent-entry default below also maps to, so without this the
        // orchestrator's `changed` gate (which reads this) would skip the
        // repaint and strand the arc when the `TerminalSpawned` event was
        // dropped on the lossy bus and the first `AgentState` is `Idle`
        // (#1069). Gate it on the arc *actually being displayed* — matching
        // `cell_state`'s precedence, the arc shows only when no higher live
        // signal does, i.e. the stored state is absent / `Idle` / `Exited`.
        // A live sibling session (`Working`/`Done`/`InputNeeded`/
        // `LimitReached`) owns the slot instead, so its repeated pings must
        // still dedup here rather than force a needless repaint every tick.
        if self.spawning.contains(session_key)
            && matches!(
                self.agents.get(session_key),
                None | Some(lazybox_ipc::AgentState::Idle)
                    | Some(lazybox_ipc::AgentState::Exited { .. })
            )
        {
            return false;
        }
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

    /// Optimistically tag a workspace's row as running on a remote box
    /// (client-side UI state) so the sidebar's remote indicator shows
    /// immediately, before the local daemon's snapshot — which doesn't know
    /// about the box — catches up. `remote` is the box's display name (the
    /// `sandbox:` box) this session was spawned on; `None` clears the tag.
    pub fn mark_remote(&mut self, sk: SessionKey, remote: String) {
        if let Some(workspace) = self.workspaces.get_mut(&sk) {
            workspace.remote = Some(remote);
            self.recompute_visible();
        }
    }

    /// Roll back [`Self::mark_remote`] — the spawn the tag advertised was
    /// dropped, so the `⇅` glyph would name a session that never existed.
    pub fn unmark_remote(&mut self, sk: &SessionKey) {
        if let Some(workspace) = self.workspaces.get_mut(sk) {
            workspace.remote = None;
            self.recompute_visible();
        }
    }

    /// Optimistically flip a workspace's merge-on-green arm so the `⚡`
    /// row glyph lands the instant the user presses `g g`, instead of only
    /// after the daemon persists the flag and rebroadcasts the workspace
    /// (a full round-trip that's invisible under output-heavy load). Mirrors
    /// [`Self::mark_workspace_merged`] / [`Self::mark_remote`]. If the daemon
    /// ultimately declines to arm (the merge-on-green author gate), its next
    /// `WorkspaceUpserted` echo carries the real `false` and the glyph clears
    /// — the same self-correcting contract every optimistic tag here uses.
    /// Returns whether a workspace was found to update.
    pub fn mark_auto_merge_on_green(&mut self, sk: &SessionKey, enabled: bool) -> bool {
        if let Some(workspace) = self.workspaces.get_mut(sk) {
            if workspace.auto_merge_on_green != enabled {
                workspace.auto_merge_on_green = enabled;
                self.recompute_visible();
            }
            true
        } else {
            false
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

    /// The commit/PR conventions the `w` work brief injects. See
    /// [`Self::set_conventions`].
    pub fn conventions(&self) -> &lazybox_core::Conventions {
        &self.conventions
    }

    /// Wire the YAML-configured `conventions:` block at startup so an
    /// interactive `w` builds the same convention-aware brief the
    /// daemon uses for autonomous spawns.
    pub fn set_conventions(&mut self, conventions: lazybox_core::Conventions) {
        self.conventions = conventions;
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
    #[allow(clippy::too_many_arguments)]
    pub fn apply_config(
        &mut self,
        attention: lazybox_config::AttentionConfig,
        collapsed_repos: BTreeSet<String>,
        pinned_repos: Vec<String>,
        focused_workspaces: Vec<SessionKey>,
        spaces: Vec<lazybox_config::SpaceConfig>,
        collapsed_spaces: BTreeSet<String>,
        default_agent: Option<String>,
        display: &lazybox_config::DisplayConfig,
    ) {
        self.attention = attention;
        self.collapsed_repos = collapsed_repos;
        self.pinned_repos = pinned_repos;
        // `ui.focused_workspaces` is a user-editable file, so normalize
        // it at this boundary: a duplicate key would otherwise render the
        // same workspace twice in the section AND break unstar (which
        // removes only the first occurrence, leaving the row starred).
        // Dedup preserving first-seen (focus) order.
        let mut seen = HashSet::new();
        self.focused_workspaces = focused_workspaces
            .into_iter()
            .filter(|k| seen.insert(k.clone()))
            .collect();
        self.spaces = spaces;
        self.collapsed_spaces = collapsed_spaces;
        if let Some(agent) = default_agent.filter(|s| !s.is_empty()) {
            self.default_agent = agent;
        }
        self.set_show_inactive_in_inbox(display.show_inactive_in_inbox);
        self.ascii_glyphs = display.ascii_glyphs;
        // The persisted lists are now user intent, not empty defaults —
        // seed-gated behavior (the snapshot-time star prune) may run.
        self.config_seeded = true;
    }

    /// Seed the persisted `ui.last_lens` (filters / sort / mailbox) at
    /// startup (#scale) — the write-side counterpart is
    /// `persist_lens`. Unknown tokens are dropped silently: a
    /// stale lens degrades, it never wedges boot. Fields are set
    /// directly (not via `set_filter_entries` / `cycle_*`) so seeding
    /// can't echo a persist of the very value just loaded.
    pub fn seed_lens(&mut self, lens: &lazybox_config::LensSection) {
        self.filters.replace_entries(
            lens.filters
                .iter()
                .filter_map(|t| FilterEntry::from_token(t)),
        );
        if let Some(sort) = lens
            .sort
            .as_deref()
            .and_then(lazybox_tui_core::inbox::SortMode::from_chip_label)
        {
            self.sort_mode = sort;
        }
        if let Some(mailbox) = lens
            .mailbox
            .as_deref()
            .and_then(lazybox_tui_core::inbox::Mailbox::from_chip_label)
        {
            self.mailbox = mailbox;
        }
        self.reset_cursor_and_recompute();
    }

    /// Disable the snapshot-time stale-star prune. Called (via the realm
    /// wrapper) by `Model::with_remote`: an attach client's daemon
    /// snapshot describes another machine's workspaces, so a local star
    /// absent from it is not evidence of staleness (#1244).
    pub fn set_snapshot_prune(&mut self, enabled: bool) {
        self.snapshot_prune = enabled;
    }

    // ── Observability helpers (for tests + for AppRoot / RightPane) ────

    pub fn selected_session_key(&self) -> Option<&SessionKey> {
        match self.visible.get(self.cursor)? {
            VisibleRow::Workspace(k) => Some(k),
            VisibleRow::Session { workspace, .. } => Some(workspace),
            VisibleRow::FocusedHeader
            | VisibleRow::HopperHeader
            | VisibleRow::SpaceHeader(_)
            | VisibleRow::RepoHeader(_)
            | VisibleRow::KindHeader(_) => None,
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
        // An explicit `v` toggle restarts any Shift-arrow sweep — the
        // next extend re-anchors on the (possibly re-marked) cursor row.
        self.sweep = None;
        let key = self.selected_session_key()?.clone();
        if self.broadcast_selected.insert(key.clone()) {
            Some(true)
        } else {
            self.broadcast_selected.remove(&key);
            Some(false)
        }
    }

    /// Extend the multi-select by one row in `dir` (−1 up, +1 down),
    /// spreadsheet-style: mark the workspace under the cursor, step the
    /// cursor one visible row, and mark whatever workspace it lands on.
    /// Directional (#932, #1243): the sweep is anchored on the row it
    /// started from, and reversing direction back toward that anchor
    /// deselects the row the cursor leaves — an over-sweep can shrink.
    /// Moving away from the anchor grows as before, so it still composes
    /// with `v` toggles (and Esc to clear). Returns the count that's
    /// currently visible-and-selected, for the footer notice.
    pub fn extend_selection(&mut self, dir: isize) -> usize {
        let cur = self.selected_session_key().cloned();
        let cur_idx = self.cursor;
        // The anchor survives only an *uninterrupted* run of
        // Shift-arrows: any other cursor move (j/k, a click, a jump)
        // leaves the cursor somewhere the last extend didn't, which
        // restarts the sweep at the current row.
        let continuing = self.sweep.as_ref().is_some_and(|(_, last)| *last == cur);
        let anchor = if continuing {
            self.sweep.as_ref().map(|(a, _)| a.clone())
        } else {
            cur.clone()
        };
        let anchor_idx = self
            .workspace_visible_index(anchor.as_ref())
            .unwrap_or(cur_idx);
        if let Some(key) = &cur {
            self.broadcast_selected.insert(key.clone());
        }
        self.move_cursor_by(dir);
        let landed_idx = self.cursor;
        let landed = self.selected_session_key().cloned();
        // Moving back toward the anchor shrinks: the departed row is
        // unmarked (the anchor itself always stays). Anywhere else grows.
        let toward_anchor = landed_idx == anchor_idx
            || (landed_idx > anchor_idx.min(cur_idx) && landed_idx < anchor_idx.max(cur_idx));
        if toward_anchor {
            if let Some(key) = &cur
                && anchor.as_ref() != Some(key)
            {
                self.broadcast_selected.remove(key);
            }
        } else if let Some(key) = &landed {
            self.broadcast_selected.insert(key.clone());
        }
        // A sweep that hasn't crossed a workspace row yet anchors on the
        // first one it reaches.
        self.sweep = anchor.or_else(|| landed.clone()).map(|a| (a, landed));
        self.visible_broadcast_selected_count()
    }

    /// Visible index of a workspace's row (its `Workspace` row, or the
    /// first `Session` sub-row when the workspace row itself isn't in
    /// the list). `None` for `None` keys and hidden workspaces.
    fn workspace_visible_index(&self, key: Option<&SessionKey>) -> Option<usize> {
        let key = key?;
        self.visible.iter().position(|row| match row {
            VisibleRow::Workspace(k) => k == key,
            VisibleRow::Session { workspace, .. } => workspace == key,
            _ => false,
        })
    }

    /// Shift-click range extend: mark every workspace row between the
    /// current cursor and the clicked row (inclusive) and move the
    /// cursor there. Row math mirrors [`click_to_select`](Self::click_to_select).
    /// Additive, like [`extend_selection`](Self::extend_selection).
    /// Returns whether the click landed on a real row (#932).
    pub fn extend_selection_to(&mut self, area: Rect, click_row: u16) -> bool {
        let header_height = 5 + self.usage_row_height(area);
        if click_row < area.y + header_height {
            return false;
        }
        let idx = (click_row - area.y - header_height) as usize + self.rendered_scroll;
        if idx >= self.visible.len() {
            return false;
        }
        let (lo, hi) = if idx >= self.cursor {
            (self.cursor, idx)
        } else {
            (idx, self.cursor)
        };
        for row in &self.visible[lo..=hi] {
            match row {
                VisibleRow::Workspace(k) => {
                    self.broadcast_selected.insert(k.clone());
                }
                VisibleRow::Session { workspace, .. } => {
                    self.broadcast_selected.insert(workspace.clone());
                }
                _ => {}
            }
        }
        self.set_cursor(idx);
        true
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

    /// How many marked workspaces are currently *visible* — the count
    /// the header surfaces and the number a broadcast / bulk-update
    /// actually targets (`selected_broadcast_keys`). Marks on rows
    /// hidden by the active mailbox / filter / search are excluded, so
    /// the count stays in lockstep with the on-screen `✓` gutter marks
    /// rather than overstating what's actionable (issue #786).
    pub fn visible_broadcast_selected_count(&self) -> usize {
        self.visible
            .iter()
            .filter(|row| match row {
                VisibleRow::Workspace(k) => self.broadcast_selected.contains(k),
                _ => false,
            })
            .count()
    }

    /// Drop the whole multi-select set. Bound to Esc and called after
    /// a successful broadcast so the marks don't outlive the send.
    /// Returns whether anything was cleared (so Esc can fall through
    /// when there was no selection).
    pub fn clear_broadcast_selection(&mut self) -> bool {
        self.sweep = None;
        let had = !self.broadcast_selected.is_empty();
        self.broadcast_selected.clear();
        had
    }

    /// The workspace's live AGENT terminal (lowest terminal id when
    /// several run) and its agent id — the target for
    /// workspace-addressed agent actions like reset-context (#1204).
    /// `None` when no agent is running here (shells don't count).
    pub fn agent_terminal_for(&self, key: &SessionKey) -> Option<(TerminalId, String)> {
        self.running_terminals
            .iter()
            .filter(|(_, (sk, _))| sk == key)
            .filter_map(|(tid, (_, kind))| match kind {
                TerminalKind::Agent(id) => Some((*tid, id.clone())),
                _ => None,
            })
            .min_by_key(|(tid, _)| tid.0)
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
        // Mirror the header layout from `render` — including the
        // always-visible usage row, which shifts content down by one when
        // present (#1059).
        let header_height = 5 + self.usage_row_height(area);
        if click_row < area.y + header_height {
            return false;
        }
        // Add the scroll offset the renderer applied so a click lands
        // on the row actually drawn under the cursor — `rendered_scroll`,
        // not `scroll`, because a wheel notch dispatched after the last
        // frame may have moved `scroll` past what's on screen.
        let idx = (click_row - area.y - header_height) as usize + self.rendered_scroll;
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
            | Some(VisibleRow::FocusedHeader)
            | Some(VisibleRow::HopperHeader)
            | Some(VisibleRow::SpaceHeader(_))
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
        self.ensure_visible_fresh();
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
            now: self.now(),
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
        self.ensure_visible_fresh();
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

    /// Move the cursor onto the next workspace whose agent is blocked on
    /// a usage / rate limit, starting AFTER the current row and wrapping
    /// (`Shift-L`, #847) — the rate-limited analog of
    /// [`Self::focus_next_asking_workspace`].
    pub fn focus_next_limit_reached_workspace(&mut self) -> bool {
        let keys_order = self.visible_workspace_keys();
        let current = self.selected_session_key().cloned();
        let Some(target) = crate::agent_attention::next_limit_reached_workspace(
            &self.agents,
            &keys_order,
            current.as_ref(),
        ) else {
            return false;
        };
        self.focus_workspace_key(&target)
    }

    /// Every agent terminal currently blocked on a usage / rate limit,
    /// lowest id first — the exact target set for the bulk "resume all
    /// rate-limited agents" action (`Shift-K`, #847).
    ///
    /// Targets *terminals*, not workspaces: a workspace's aggregate reads
    /// `LimitReached` when ANY of its terminals is, but the resume must
    /// inject into the blocked terminal(s) themselves — routing through
    /// the workspace's lowest-id agent (as broadcast does) could hit a
    /// still-working sibling and skip the blocked one entirely.
    pub fn limit_reached_terminals(&self) -> Vec<TerminalId> {
        let mut ids: Vec<TerminalId> = self
            .agent_terminal_states
            .iter()
            .filter(|(_, (_, state))| *state == lazybox_ipc::AgentState::LimitReached)
            .map(|(id, _)| *id)
            .collect();
        ids.sort_by_key(|id| id.0);
        ids
    }

    pub fn credit_exhausted_terminals(&self) -> Vec<TerminalId> {
        let mut ids: Vec<TerminalId> = self
            .agent_terminal_states
            .iter()
            .filter(|(_, (_, state))| *state == lazybox_ipc::AgentState::CreditExhausted)
            .map(|(id, _)| *id)
            .collect();
        ids.sort_by_key(|id| id.0);
        ids
    }

    pub fn credit_exhausted_terminals_for(&self, key: &SessionKey) -> Vec<TerminalId> {
        let mut ids: Vec<TerminalId> = self
            .agent_terminal_states
            .iter()
            .filter(|(_, (session_key, state))| {
                session_key == key && *state == lazybox_ipc::AgentState::CreditExhausted
            })
            .map(|(id, _)| *id)
            .collect();
        ids.sort_by_key(|id| id.0);
        ids
    }

    /// Number of distinct workspaces with at least one agent terminal in
    /// `LimitReached` — the `⏳ N limited` header count and the size the
    /// escalating usage-limit alert (#1012) reports. Counts workspaces,
    /// not terminals: two blocked agents in one workspace are one row's
    /// worth of "act externally" signal.
    pub fn limit_reached_workspace_count(&self) -> usize {
        self.agent_terminal_states
            .values()
            .filter(|(_, state)| *state == lazybox_ipc::AgentState::LimitReached)
            .map(|(sk, _)| sk)
            .collect::<std::collections::HashSet<_>>()
            .len()
    }

    /// The visible workspace keys in sidebar (top-down) order — shared by
    /// the attention jumps and the rate-limited target set.
    fn visible_workspace_keys(&self) -> Vec<SessionKey> {
        self.visible
            .iter()
            .filter_map(|r| match r {
                VisibleRow::Workspace(k) => Some(k.clone()),
                _ => None,
            })
            .collect()
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

    /// The workspaces that carry a jump number, in sidebar (top-down)
    /// order: the **focused** (starred) workspaces, deduped so one that
    /// the `★ Focused` pin lifts to the top isn't also counted in its
    /// repo group. The 1-based index here is the badge number and the
    /// `]]<digit>` target, so numbering only what the user curated keeps
    /// the sidebar quiet and the digits stable.
    pub fn numbered_workspace_keys(&self) -> Vec<SessionKey> {
        let mut seen = std::collections::HashSet::new();
        self.visible
            .iter()
            .filter_map(|r| match r {
                VisibleRow::Workspace(k) => Some(k),
                _ => None,
            })
            .filter(|k| self.is_focused(k))
            .filter(|k| seen.insert((*k).clone()))
            .cloned()
            .collect()
    }

    /// Move the cursor onto the `n`th (1-based) numbered (focused)
    /// workspace in sidebar order. Returns true when that slot exists and
    /// the cursor moved. Backs the `]]<digit>` focus-mode jump.
    pub fn focus_nth_numbered_workspace(&mut self, n: usize) -> bool {
        let Some(target) = n
            .checked_sub(1)
            .and_then(|i| self.numbered_workspace_keys().into_iter().nth(i))
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
        self.persist_lens();
        self.sort_mode
    }

    /// Replace the active filter set and rebuild the visible list.
    /// Cursor is reset because the row the user was parked on may have
    /// just been filtered out — landing on the new top is less
    /// surprising than landing off-screen.
    pub fn set_filters(&mut self, filters: impl IntoIterator<Item = Filter>) {
        self.filters.replace(filters);
        self.reset_cursor_and_recompute();
        self.persist_lens();
    }

    /// Replace the active filters from picker entries (fixed predicates
    /// plus the value-driven label / Linear-state / person axes).
    pub fn set_filter_entries(&mut self, entries: impl IntoIterator<Item = FilterEntry>) {
        self.filters.replace_entries(entries);
        self.reset_cursor_and_recompute();
        self.persist_lens();
    }

    /// The current lens (filters / sort / mailbox) as config tokens.
    fn current_lens(&self) -> lazybox_config::LensSection {
        let mut filters: Vec<String> = self.filters.iter().map(|f| f.label().to_string()).collect();
        filters.extend(self.filters.labels().iter().map(|n| format!("label:{n}")));
        filters.extend(
            self.filters
                .linear_states()
                .iter()
                .map(|n| format!("linear-state:{n}")),
        );
        filters.extend(self.filters.people().iter().map(|l| format!("person:{l}")));
        lazybox_config::LensSection {
            filters,
            sort: Some(self.sort_mode.chip_label().to_string()),
            mailbox: Some(self.mailbox.chip_label().to_string()),
        }
    }

    /// Persist the lens after a user-driven change (#scale: filters,
    /// sort, and mailbox used to evaporate on restart). Whole-value
    /// assignment like `ui.last_space` — the lens has a single writer
    /// (the user's own action in this client), so the `mutate_ui_list`
    /// read-modify-write machinery isn't needed. Seed-gated: before
    /// `apply_config` has run, a lens change is programmatic (boot,
    /// tests building a bare sidebar), not user intent — and unit
    /// tests exercising `cycle_sort_mode` / `set_filters` without a
    /// sandboxed `LAZYBOX_HOME` must never enqueue a write against the
    /// developer's real config.yaml.
    fn persist_lens(&self) {
        if !self.config_seeded {
            return;
        }
        let lens = self.current_lens();
        lazybox_config::Config::save_with_async(move |c| c.ui.last_lens = Some(lens));
    }

    /// Every row the `f` filter menu offers, with its match count: the
    /// fixed predicates ([`Filter::ALL`]) followed by the label and
    /// Linear-state values discovered in the current mailbox, plus any
    /// currently-active value (count 0 when nothing in the mailbox
    /// carries it) so an active filter always has a re-checkable row. A
    /// value axis with no discovered and no active values adds no rows.
    pub fn filter_menu_entries(&self) -> Vec<(FilterEntry, usize)> {
        use std::collections::BTreeMap;
        let now = self.now();
        let candidates: Vec<&Workspace> = self
            .workspaces
            .values()
            .filter(|w| mailbox_membership(w, self.mailbox, now, self.show_inactive_in_inbox))
            .collect();

        let mut out: Vec<(FilterEntry, usize)> = Filter::ALL
            .into_iter()
            .map(|f| {
                let n = candidates
                    .iter()
                    .filter(|w| {
                        f.matches(&FilterCtx {
                            w,
                            agents: &self.agents,
                            now,
                        })
                    })
                    .count();
                (FilterEntry::Predicate(f), n)
            })
            .collect();

        // The `snoozed` count over Inbox candidates is always 0 — the
        // mailbox excludes snoozed rows, which is exactly what the
        // snoozed lens un-hides. Count what toggling would SURFACE
        // (every currently-snoozed workspace), matching the menu's
        // "what would this toggle show" contract.
        if self.mailbox == Mailbox::Inbox
            && let Some(row) = out
                .iter_mut()
                .find(|(e, _)| matches!(e, FilterEntry::Predicate(Filter::Snoozed)))
        {
            row.1 = self
                .workspaces
                .values()
                .filter(|w| w.is_snoozed(now))
                .count();
        }

        // Distinct label names across candidates' primary tasks, counted
        // per workspace (a workspace with the label once, not per label
        // instance). BTreeMap keeps the rows in a stable alpha order.
        let mut labels: BTreeMap<String, usize> = BTreeMap::new();
        let mut states: BTreeMap<String, usize> = BTreeMap::new();
        // People axis (#scale): distinct logins across the candidates'
        // primary tasks — author + requested reviewers + submitted
        // reviewers + assignees, the same role-union
        // `filter::task_involves` matches against — counted per
        // workspace. Bots (a submitted review's `is_bot`, or the
        // GitHub `…[bot]` login convention) collect separately so
        // they sort after every human in the menu.
        let mut people: BTreeMap<String, usize> = BTreeMap::new();
        let mut bots: BTreeMap<String, usize> = BTreeMap::new();
        for w in &candidates {
            let Some(task) = w.primary_task() else {
                continue;
            };
            let mut seen = std::collections::BTreeSet::new();
            for l in &task.labels {
                if !(task.id.source == lazybox_core::GITHUB_SOURCE
                    && lazybox_core::is_working_claim_label_name(&l.name))
                    && seen.insert(l.name.as_str())
                {
                    *labels.entry(l.name.clone()).or_default() += 1;
                }
            }
            if let Some(state) = &task.state_label {
                *states.entry(state.clone()).or_default() += 1;
            }
            // `is_bot` only rides submitted reviews, but a bot can also
            // appear as author / requested reviewer / assignee — plain
            // strings with no flag. Bucketing per-field would let
            // whichever field names the login FIRST win (the per-workspace
            // `seen_people` is first-wins), mis-sorting a suffix-less bot
            // into the humans list the moment it's also a requested
            // reviewer. Scan the reviews up front so `is_bot` is
            // authoritative regardless of which field mentions the login.
            let bot_logins: std::collections::BTreeSet<&str> = task
                .reviews
                .iter()
                .filter(|r| r.is_bot)
                .map(|r| r.login.as_str())
                .collect();
            let mut seen_people = std::collections::BTreeSet::new();
            let mut tally = |login: &str| {
                if login.is_empty() || !seen_people.insert(login.to_string()) {
                    return;
                }
                let bucket = if bot_logins.contains(login) || login.ends_with("[bot]") {
                    &mut bots
                } else {
                    &mut people
                };
                *bucket.entry(login.to_string()).or_default() += 1;
            };
            tally(&task.author);
            for r in &task.reviewers {
                tally(r);
            }
            for r in &task.reviews {
                tally(&r.login);
            }
            for a in &task.assignees {
                tally(a);
            }
        }
        // Always surface currently-active values, even when no candidate
        // carries them right now (count 0). Otherwise an active
        // `Label`/`LinearState` filter whose matching workspaces have all
        // left the mailbox would have no row to pre-check, and the next
        // apply — which rebuilds the set from the checked rows — would
        // silently drop it (review finding). Fixed predicates can't hit
        // this because they're always in `Filter::ALL`.
        for name in self.filters.labels() {
            labels.entry(name.clone()).or_insert(0);
        }
        for name in self.filters.linear_states() {
            states.entry(name.clone()).or_insert(0);
        }
        for login in self.filters.people() {
            if !people.contains_key(login) && !bots.contains_key(login) {
                people.insert(login.clone(), 0);
            }
        }
        // A login can straddle buckets across workspaces (tallied as a
        // requested reviewer in one, flagged `is_bot` by a submitted
        // review in another) — fold the human count into the bot row so
        // one login never renders twice.
        for (login, count) in std::mem::take(&mut people) {
            if let Some(bot_count) = bots.get_mut(&login) {
                *bot_count += count;
            } else {
                people.insert(login, count);
            }
        }
        out.extend(labels.into_iter().map(|(n, c)| (FilterEntry::Label(n), c)));
        out.extend(
            states
                .into_iter()
                .map(|(n, c)| (FilterEntry::LinearState(n), c)),
        );
        // Humans in alpha order, bots after — a review bot shows up on
        // most PRs, and a high count must not drown the humans the
        // People axis exists for.
        out.extend(
            people
                .into_iter()
                .map(|(login, c)| (FilterEntry::Person(login), c)),
        );
        out.extend(
            bots.into_iter()
                .map(|(login, c)| (FilterEntry::Person(login), c)),
        );
        out
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
        self.persist_lens();
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

    /// Every tracked GitHub repo as its canonical `owner/repo`, in the
    /// sidebar's stable `BTreeMap` order. Drives the unmapped-Linear-team
    /// repo picker (#1041), which persists the pick as
    /// `providers.linear.teams.<team>` — so it offers only real,
    /// clonable GitHub repos, never `linear/<team>` or local projects.
    pub fn github_repos_for_picker(&self) -> Vec<String> {
        self.projects
            .values()
            .filter_map(|p| p.github_repo().map(str::to_string))
            .collect()
    }

    /// Tracked GitHub repos to offer for an unmapped Linear `team`, ranked
    /// so the likely answer is one keystroke (#1041). Repos that other
    /// Linear tickets in the *same team* already link a GitHub PR to float
    /// to the top — the team's real repos, learned from its own tickets —
    /// followed by the rest in their existing order. A blank picker is never
    /// what the user wants: even with no signal this still lists every repo.
    pub fn github_repos_ranked_for_linear_team(&self, team: &str) -> Vec<String> {
        let mut repos = self.github_repos_for_picker();
        let linked: std::collections::HashSet<String> = self
            .workspaces
            .values()
            .filter_map(|w| w.primary_task())
            .filter(|t| {
                t.id.source == "linear"
                    && t.repo.as_deref().and_then(|r| r.strip_prefix("linear/")) == Some(team)
            })
            .flat_map(|t| {
                t.linked_tasks
                    .iter()
                    .filter(|id| id.source == "github")
                    .filter_map(|id| id.key.split_once('#').map(|(repo, _)| repo.to_string()))
            })
            .collect();
        repos.sort_by_key(|repo| !linked.contains(repo));
        repos
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
            // The `★ Focused` header isn't a project — starring is a
            // cross-repo shortlist, not a group you create workspaces in.
            VisibleRow::FocusedHeader | VisibleRow::HopperHeader => None,
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
            // A Space header spans multiple projects — no single one.
            VisibleRow::SpaceHeader(_) => None,
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

    /// Clear a *committed* search — one `Enter` left applied as a filter
    /// while it stopped capturing keys. This is the keyboard `Esc`
    /// counterpart to the editing-time `Esc` (which `handle_search_key`
    /// owns): without it a committed search could only be cleared by
    /// re-editing first, so a bare `Esc` did nothing and the user was
    /// stuck in a narrowed tree. Returns true when it cleared something,
    /// so the caller consumes the key. No-op while editing (that `Esc`
    /// belongs to `handle_search_key`) or with no search.
    pub fn clear_committed_search(&mut self) -> bool {
        match self.search.as_ref() {
            Some(s) if !s.editing => {
                self.search = None;
                self.scroll_detached = false;
                self.recompute_visible();
                true
            }
            _ => false,
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
    #[cfg(test)]
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

    /// The model + effort label to show beside each agent runner badge on
    /// `key`, as `(badge_letter, label)` pairs. A label is surfaced only
    /// when a single terminal carries that letter — with two same-kind
    /// agents in one workspace their models are ambiguous against the
    /// collapsed `C×2` badge, so it's dropped rather than guessed. Empty
    /// when `ui.show_agent_model` is off or nothing has a known model.
    #[cfg(test)]
    fn agent_models(&self, key: &SessionKey) -> Vec<(char, String)> {
        if !self.show_agent_model {
            return Vec::new();
        }
        // letter → (count, one candidate label)
        let mut per_letter: HashMap<char, (usize, Option<String>)> = HashMap::new();
        for (tid, (sk, kind)) in &self.running_terminals {
            if sk != key || !matches!(kind, TerminalKind::Agent(_)) {
                continue;
            }
            let entry = per_letter.entry(self.badge_letter(kind)).or_default();
            entry.0 += 1;
            if let Some(model) = self.terminal_models.get(tid) {
                entry.1 = Some(model.clone());
            }
        }
        per_letter
            .into_iter()
            .filter_map(|(letter, (count, model))| match model {
                Some(model) if count == 1 => Some((letter, model)),
                _ => None,
            })
            .collect()
    }

    /// Every workspace's [`Self::runner_badges`] aggregated in a single
    /// O(terminals) pass, so the sidebar render can look each row's
    /// badges up in O(1) instead of re-scanning all running terminals
    /// once per row (#1031 — the per-row scan was O(rows × terminals)).
    /// The per-key result matches `runner_badges` exactly.
    fn runner_badges_by_key(&self) -> HashMap<SessionKey, Vec<(char, usize)>> {
        let mut counts: HashMap<SessionKey, HashMap<char, usize>> = HashMap::new();
        for (sk, kind) in self.running_terminals.values() {
            *counts
                .entry(sk.clone())
                .or_default()
                .entry(self.badge_letter(kind))
                .or_default() += 1;
        }
        counts
            .into_iter()
            .map(|(key, letters)| {
                let mut entries: Vec<(char, usize)> = letters.into_iter().collect();
                entries.sort_by_key(|(c, _)| match *c {
                    'S' => (1, 'S'),
                    other => (0, other),
                });
                (key, entries)
            })
            .collect()
    }

    /// Every workspace's [`Self::agent_models`] aggregated in a single
    /// O(terminals) pass (see [`Self::runner_badges_by_key`]). Empty when
    /// `ui.show_agent_model` is off. The per-key result matches
    /// `agent_models` exactly.
    fn agent_models_by_key(&self) -> HashMap<SessionKey, Vec<(char, String)>> {
        if !self.show_agent_model {
            return HashMap::new();
        }
        let mut per_key: HashMap<SessionKey, HashMap<char, (usize, Option<String>)>> =
            HashMap::new();
        for (tid, (sk, kind)) in &self.running_terminals {
            if !matches!(kind, TerminalKind::Agent(_)) {
                continue;
            }
            let entry = per_key
                .entry(sk.clone())
                .or_default()
                .entry(self.badge_letter(kind))
                .or_default();
            entry.0 += 1;
            if let Some(model) = self.terminal_models.get(tid) {
                entry.1 = Some(model.clone());
            }
        }
        per_key
            .into_iter()
            .map(|(key, per_letter)| {
                let labels = per_letter
                    .into_iter()
                    .filter_map(|(letter, (count, model))| match model {
                        Some(model) if count == 1 => Some((letter, model)),
                        _ => None,
                    })
                    .collect();
                (key, labels)
            })
            .collect()
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
            // Neither the `★ Focused` header (nor rows lifted under it)
            // nor a Space header belongs to a repo group — pin / collapse
            // no-op there.
            Some(VisibleRow::FocusedHeader)
            | Some(VisibleRow::HopperHeader)
            | Some(VisibleRow::SpaceHeader(_))
            | None => None,
        }
    }

    /// The Space group the cursor's row belongs to, if any — the
    /// higher-tier analogue of [`Self::cursor_repo`]. A Space header
    /// resolves to itself; a repo header / workspace / session / kind
    /// row resolves to the nearest Space header above it. `None` when
    /// the Space tier isn't active (no Space header precedes the row).
    pub fn cursor_space(&self) -> Option<String> {
        if let Some(VisibleRow::SpaceHeader(name)) = self.visible.get(self.cursor) {
            return Some(name.clone());
        }
        self.visible
            .iter()
            .take(self.cursor + 1)
            .rev()
            .find_map(|r| match r {
                VisibleRow::SpaceHeader(name) => Some(name.clone()),
                _ => None,
            })
    }

    /// True when the cursor sits directly on a Space header row.
    pub fn cursor_on_space_header(&self) -> bool {
        matches!(
            self.visible.get(self.cursor),
            Some(VisibleRow::SpaceHeader(_))
        )
    }

    /// True when the cursor's repo group is currently collapsed. `None`
    /// when the cursor isn't in a group at all. Drives the header-row
    /// footer's collapse-vs-expand verb (#338).
    pub fn cursor_repo_collapsed(&self) -> Option<bool> {
        self.cursor_repo()
            .map(|repo| self.collapsed_repos.contains(&repo))
    }

    /// Hierarchy metadata for the ticket under the cursor, when it is a
    /// parent or descendant in the current visible forest.
    pub fn cursor_ticket_tree(&self) -> Option<TicketTreeMeta> {
        let key = self.selected_session_key()?;
        self.ticket_tree.get(key).copied()
    }

    /// Fold or unfold the visible descendants of the parent ticket under
    /// the cursor. Returns false for leaf/non-ticket rows so callers can
    /// fall back to the existing repo/Space collapse behavior.
    pub fn toggle_ticket_at_cursor(&mut self) -> bool {
        let Some(meta) = self.cursor_ticket_tree().filter(|meta| meta.has_children) else {
            return false;
        };
        let Some(task_id) = self
            .selected_workspace()
            .and_then(|workspace| workspace.primary_task())
            .map(|task| task.id.clone())
        else {
            return false;
        };
        if meta.collapsed {
            self.collapsed_tickets.remove(&task_id);
        } else {
            self.collapsed_tickets.insert(task_id);
        }
        self.recompute_visible_inner(true);
        true
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
        // Persist to ~/.lazybox/config.yaml::ui.collapsed_repos so the
        // layout survives restart — as a targeted add/remove on the
        // on-disk set, because the in-memory copy may be stale or
        // unseeded and must not clobber another writer's flags (#1244).
        // Best-effort; an I/O error here just means next launch starts
        // expanded.
        let op = if was_collapsed {
            lazybox_config::UiListOp::Remove(repo.clone())
        } else {
            lazybox_config::UiListOp::Add(repo.clone())
        };
        lazybox_config::Config::mutate_ui_list(|c| &mut c.ui.collapsed_repos, op);
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

    /// Toggle the collapsed flag for the Space at or above the cursor —
    /// the higher-tier analogue of [`Self::toggle_repo_at_cursor`], fired
    /// by `Space` when the cursor rests on a Space header. Persists to
    /// `ui.collapsed_spaces` and re-parks the cursor on the toggled
    /// header so a double-tap toggles the same Space (#860).
    pub fn toggle_space_at_cursor(&mut self) -> bool {
        let Some(space) = self.cursor_space() else {
            return false;
        };
        let was_collapsed = self.collapsed_spaces.contains(&space);
        if was_collapsed {
            self.collapsed_spaces.remove(&space);
        } else {
            self.collapsed_spaces.insert(space.clone());
        }
        self.recompute_visible();
        // Targeted add/remove on the on-disk set, like the repo-tier
        // toggle above (#1244).
        let op = if was_collapsed {
            lazybox_config::UiListOp::Remove(space.clone())
        } else {
            lazybox_config::UiListOp::Add(space.clone())
        };
        lazybox_config::Config::mutate_ui_list(|c| &mut c.ui.collapsed_spaces, op);
        if let Some(idx) = self
            .visible
            .iter()
            .position(|r| matches!(r, VisibleRow::SpaceHeader(n) if n == &space))
        {
            self.set_cursor(idx);
        }
        true
    }

    /// True when the Space is currently collapsed (used by the header
    /// render to pick `▾` vs `▸`).
    pub fn is_space_collapsed(&self, name: &str) -> bool {
        self.collapsed_spaces.contains(name)
    }

    /// Test-only: park the cursor on the named Space / repo header —
    /// the keyboard path a mouse click takes via `click_to_select`.
    #[doc(hidden)]
    pub fn focus_header_row(&mut self, name: &str) -> bool {
        if let Some(idx) = self.visible.iter().position(|r| match r {
            VisibleRow::SpaceHeader(n) | VisibleRow::RepoHeader(n) => n == name,
            _ => false,
        }) {
            self.set_cursor(idx);
            return true;
        }
        false
    }

    /// Test-only: the rendered header rows in order — `(is_repo, name)`.
    #[doc(hidden)]
    pub fn __test_header_rows(&self) -> Vec<(bool, String)> {
        self.visible
            .iter()
            .filter_map(|r| match r {
                VisibleRow::RepoHeader(n) => Some((true, n.clone())),
                VisibleRow::SpaceHeader(n) => Some((false, n.clone())),
                _ => None,
            })
            .collect()
    }

    /// The exact header row under the cursor, if any — `(is_repo,
    /// name)`. Distinguishes the right-click header menu (#1211) from
    /// the workspace context menu; the at-or-above helpers
    /// (`cursor_repo`, `cursor_space`) deliberately don't.
    pub fn cursor_header(&self) -> Option<(bool, String)> {
        match self.visible.get(self.cursor) {
            Some(VisibleRow::RepoHeader(n)) => Some((true, n.clone())),
            Some(VisibleRow::SpaceHeader(n)) => Some((false, n.clone())),
            _ => None,
        }
    }

    /// Reorder the group at the cursor (#1211): a cursor sitting
    /// exactly on a Space header moves that Space within the Space
    /// tier; anywhere inside a repo group (its header, a workspace, a
    /// session row) moves that repo within its Space. Both rewrite the
    /// rendered order into `ui.spaces` via the pure tui-core movers and
    /// persist. Returns `(what, name)` for the footer notice, `None`
    /// when there is nothing movable under the cursor (e.g. a lone
    /// group — advise-level no-op, never an error).
    pub fn move_group_at_cursor(
        &mut self,
        dir: lazybox_tui_core::inbox::MoveDir,
    ) -> Option<(&'static str, String)> {
        let moved: (&'static str, String) = if let Some(VisibleRow::SpaceHeader(space)) =
            self.visible.get(self.cursor)
        {
            let space = space.clone();
            let rendered: Vec<String> = self
                .visible
                .iter()
                .filter_map(|r| match r {
                    VisibleRow::SpaceHeader(n) => Some(n.clone()),
                    _ => None,
                })
                .collect();
            if !lazybox_tui_core::inbox::move_space(&mut self.spaces, &rendered, &space, dir) {
                return None;
            }
            // Persist by replaying the same pure mover against the
            // freshly loaded on-disk list (#1244): the operation is a
            // function of the captured render order, so a stale or
            // unseeded in-memory snapshot never clobbers Spaces
            // another writer created.
            let space_cfg = space.clone();
            lazybox_config::Config::save_with_async(move |c| {
                lazybox_tui_core::inbox::move_space(&mut c.ui.spaces, &rendered, &space_cfg, dir);
            });
            ("Space", space)
        } else {
            let repo = self.cursor_repo()?;
            let space = self.space_of_source(&repo);
            // The repo's on-screen siblings: every rendered repo
            // header resolving to the same Space (covers both the
            // tiered and the flat single-Space shape).
            let rendered: Vec<String> = self
                .visible
                .iter()
                .filter_map(|r| match r {
                    VisibleRow::RepoHeader(n) => Some(n.clone()),
                    _ => None,
                })
                .filter(|r| self.space_of_source(r) == space)
                .collect();
            if rendered.len() < 2 {
                return None;
            }
            if !lazybox_tui_core::inbox::move_source_in_space(
                &mut self.spaces,
                &space,
                &rendered,
                &repo,
                dir,
            ) {
                return None;
            }
            // Same replay-the-operation persistence as the Space
            // branch above (#1244).
            let repo_cfg = repo.clone();
            lazybox_config::Config::save_with_async(move |c| {
                lazybox_tui_core::inbox::move_source_in_space(
                    &mut c.ui.spaces,
                    &space,
                    &rendered,
                    &repo_cfg,
                    dir,
                );
            });
            ("repo", repo)
        };
        self.recompute_visible();
        // A moved *header* cursor re-parks on the moved header (its row
        // index changed with the group); a workspace-row cursor is
        // preserved by identity in `recompute_visible` already.
        if let Some(VisibleRow::SpaceHeader(_) | VisibleRow::RepoHeader(_)) =
            self.visible.get(self.cursor)
        {
            let (kind, name) = &moved;
            if let Some(idx) = self.visible.iter().position(|r| match r {
                VisibleRow::SpaceHeader(n) => *kind == "Space" && n == name,
                VisibleRow::RepoHeader(n) => *kind == "repo" && n == name,
                _ => false,
            }) {
                self.set_cursor(idx);
            }
        }
        Some(moved)
    }

    /// Rename the Space at/above the cursor (#1211): its claimed +
    /// currently-rendered sources move to the new name (merging into an
    /// existing Space of that name), the collapse flag follows, and
    /// both persist. Returns the resolved `(old, new)` for the notice;
    /// `None` when the name is blank/unchanged.
    pub fn rename_space(&mut self, old: &str, new: &str) -> Option<(String, String)> {
        let rendered: Vec<String> = self
            .visible
            .iter()
            .filter_map(|r| match r {
                VisibleRow::RepoHeader(n) => Some(n.clone()),
                _ => None,
            })
            .filter(|r| self.space_of_source(r) == old)
            .collect();
        if !lazybox_tui_core::inbox::rename_space(&mut self.spaces, old, new, &rendered) {
            return None;
        }
        let new = new.trim().to_string();
        if self.collapsed_spaces.remove(old) {
            self.collapsed_spaces.insert(new.clone());
        }
        self.recompute_visible();
        // Persist by replaying the rename against the freshly loaded
        // on-disk config (#1244): the collapse flag and picker
        // preselection follow as targeted edits, so nothing outside this
        // Space's entries is rewritten.
        let (old_for_cfg, new_for_cfg) = (old.to_string(), new.clone());
        lazybox_config::Config::save_with_async(move |c| {
            lazybox_tui_core::inbox::rename_space(
                &mut c.ui.spaces,
                &old_for_cfg,
                &new_for_cfg,
                &rendered,
            );
            if c.ui.collapsed_spaces.remove(&old_for_cfg) {
                c.ui.collapsed_spaces.insert(new_for_cfg.clone());
            }
            // The picker preselection follows the rename (#1206).
            if c.ui.last_space.as_deref() == Some(old_for_cfg.as_str()) {
                c.ui.last_space = Some(new_for_cfg);
            }
        });
        if let Some(idx) = self
            .visible
            .iter()
            .position(|r| matches!(r, VisibleRow::SpaceHeader(n) if n == &new))
        {
            self.set_cursor(idx);
        }
        Some((old.to_string(), new))
    }

    /// The Space a source label currently resolves to (explicit
    /// assignment, else owner auto-seed) — used to prefill the
    /// move-to-Space prompt.
    pub fn space_of_source(&self, source: &str) -> String {
        lazybox_tui_core::inbox::space_of(source, &self.spaces)
    }

    /// The hand-created Spaces, in display order. Exactly the
    /// `ui.spaces` entries — auto-seeded owner Spaces never appear in
    /// config, so this is the list the move-to-Space picker offers
    /// (#1206).
    pub fn hand_created_spaces(&self) -> Vec<String> {
        self.spaces.iter().map(|s| s.name.clone()).collect()
    }

    /// What `source` falls back to when unassigned — the owner
    /// auto-seed (`owner/repo` → `owner`) or the ungrouped bucket.
    /// Names the "unassign" row in the move-to-Space picker.
    pub fn auto_space_of_source(&self, source: &str) -> String {
        lazybox_tui_core::inbox::space_of(source, &[])
    }

    /// Assign a source group (repo / Linear label) to a Space,
    /// persisting to `ui.spaces` (#860). A blank `space` unassigns the
    /// source (it falls back to owner auto-seed / `Ungrouped`); a name
    /// not yet in `ui.spaces` creates that Space at the end (its display
    /// order). The source is first removed from any Space it was in, so
    /// re-assigning within the same Space moves it to the end — the
    /// within-Space reorder handle. Returns the resolved Space name (the
    /// auto-seed name when unassigned) for a footer notice.
    pub fn assign_source_to_space(&mut self, source: &str, space: &str) -> String {
        lazybox_tui_core::inbox::assign_source(&mut self.spaces, source, space);
        self.recompute_visible();
        // Persist by replaying the assignment against the freshly loaded
        // on-disk list (#1244): only this source moves; Spaces another
        // writer created stay put.
        let (source_cfg, space_cfg) = (source.to_string(), space.to_string());
        lazybox_config::Config::save_with_async(move |c| {
            lazybox_tui_core::inbox::assign_source(&mut c.ui.spaces, &source_cfg, &space_cfg);
        });
        self.space_of_source(source)
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
        // order survives restart — as a targeted add/remove on the
        // on-disk list, never a whole-list overwrite from a possibly
        // stale or unseeded snapshot (#1244). Best-effort; a write error
        // just means the pins reset next launch.
        let op = if now_pinned {
            lazybox_config::UiListOp::Add(repo.clone())
        } else {
            lazybox_config::UiListOp::Remove(repo.clone())
        };
        lazybox_config::Config::mutate_ui_list(|c| &mut c.ui.pinned_repos, op);
        Some((repo, now_pinned))
    }

    /// True when the repo is currently pinned (used by the header
    /// render to draw the pin marker).
    pub fn is_repo_pinned(&self, name: &str) -> bool {
        self.pinned_repos.iter().any(|r| r == name)
    }

    /// Star / unstar the workspace under the cursor (`*`), lifting it
    /// into (or out of) the `★ Focused` section at the top. A fresh
    /// star is appended so focus order tracks the order the user
    /// starred in. Returns `(label, now_focused)` so the caller can
    /// surface a footer notice, or `None` when the cursor isn't on a
    /// workspace / session row.
    ///
    /// Like [`Self::toggle_pin_at_cursor`] this only reorders — no rows
    /// are hidden — so `recompute_visible` keeps the cursor on the same
    /// workspace by key even though its row physically moved into the
    /// Focused section.
    pub fn toggle_focus_at_cursor(&mut self) -> Option<(String, bool)> {
        let key = self.selected_session_key()?.clone();
        let label = self.workspace_label(&key);
        let now_focused = if let Some(idx) = self.focused_workspaces.iter().position(|k| *k == key)
        {
            self.focused_workspaces.remove(idx);
            Self::persist_focus_edit(lazybox_config::UiListOp::Remove(key.as_str().to_string()));
            false
        } else {
            Self::persist_focus_edit(lazybox_config::UiListOp::Add(key.as_str().to_string()));
            self.focused_workspaces.push(key);
            true
        };
        self.recompute_visible();
        Some((label, now_focused))
    }

    /// Drop a workspace from the focus set when it's genuinely removed
    /// (archived / deleted), so `ui.focused_workspaces` doesn't
    /// accumulate keys for workspaces that no longer exist — the star
    /// append is otherwise unbounded and, unlike a repo pin, workspaces
    /// churn constantly. No-op (and no write) when the key wasn't
    /// starred. Driven only by the authoritative `WorkspaceRemoved`
    /// event, never the optimistic `take_workspace`, so a rolled-back
    /// archive keeps the star. The config write is a targeted remove of
    /// exactly this key, so it can only ever fire for a star this client
    /// actually tracked — an unseeded list has nothing to forget and an
    /// unseeded writer can no longer erase the rest of the set (#1244).
    pub fn forget_focused_workspace(&mut self, key: &SessionKey) {
        if let Some(idx) = self.focused_workspaces.iter().position(|k| k == key) {
            self.focused_workspaces.remove(idx);
            Self::persist_focus_edit(lazybox_config::UiListOp::Remove(key.as_str().to_string()));
        }
    }

    /// Drop focus keys that don't match any tracked workspace, persisting
    /// each removal as a targeted edit. Called once the daemon `Snapshot`
    /// has repopulated the authoritative workspace map (its `workspaces`
    /// field is the store's full contents, every mailbox), so a key with
    /// no match is genuinely stale — an archived/deleted workspace missed
    /// by the per-removal `forget_focused_workspace`, or a placeholder
    /// that leaked into the hand-editable config (#1202/#1205, reinstated
    /// after #1213 silently dropped it). Without this the set only ever
    /// grows: the persisted string round-trips fine for a real key, but a
    /// bogus one never matches a row and so never gets unstarred by the
    /// user either. Gated twice (#1244): never before `apply_config`
    /// seeded the list (pre-seed, "unknown" means "not loaded yet"), and
    /// never on a remote attach client, whose snapshot describes another
    /// machine's workspaces (`snapshot_prune`).
    fn prune_focused_workspaces(&mut self) {
        if !self.config_seeded || !self.snapshot_prune {
            return;
        }
        let workspaces = &self.workspaces;
        let stale: Vec<SessionKey> = self
            .focused_workspaces
            .iter()
            .filter(|k| !workspaces.contains_key(*k))
            .cloned()
            .collect();
        if stale.is_empty() {
            return;
        }
        self.focused_workspaces
            .retain(|k| workspaces.contains_key(k));
        for key in stale {
            Self::persist_focus_edit(lazybox_config::UiListOp::Remove(key.as_str().to_string()));
        }
    }

    /// Persist one star/unstar to
    /// `~/.lazybox/config.yaml::ui.focused_workspaces` as a targeted
    /// edit on the on-disk list, so the shortlist survives restart and a
    /// stale or unseeded in-memory set can never overwrite stars another
    /// process persisted (#1244). Best-effort; a write error just means
    /// this toggle resets next launch.
    fn persist_focus_edit(op: lazybox_config::UiListOp) {
        lazybox_config::Config::mutate_ui_list(|c| &mut c.ui.focused_workspaces, op);
    }

    /// True when the workspace is currently starred (used by the row
    /// render to draw the `★` marker).
    pub fn is_focused(&self, key: &SessionKey) -> bool {
        self.focused_workspaces.iter().any(|k| k == key)
    }

    /// A short display label for a workspace — its primary task title,
    /// else the workspace name, else the raw key. Used for the
    /// star/unstar footer notice.
    fn workspace_label(&self, key: &SessionKey) -> String {
        self.workspaces
            .get(key)
            .map(|w| {
                w.primary_task()
                    .map(|t| t.title.clone())
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(|| w.name.clone())
            })
            .unwrap_or_else(|| key.as_str().to_string())
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
        self.pane_state_rev = self.pane_state_rev.wrapping_add(1);
        // During a batched daemon-event drain, record that a rebuild is
        // owed and defer it to `flush_recompute` — collapsing a poll
        // sweep's N per-upsert rebuilds into one (#1030).
        if self.defer_recompute {
            self.recompute_pending = true;
            return;
        }
        self.recompute_visible_inner(true);
    }

    /// Enter batched-recompute mode for a daemon-event drain: subsequent
    /// `recompute_visible` calls only mark a rebuild pending. Mirrors the
    /// model's per-batch `flush_pane_sync` deferral (#1030).
    pub fn begin_recompute_batch(&mut self) {
        self.defer_recompute = true;
    }

    /// Leave batched mode and rebuild the visible list once if any
    /// deferred event asked for it.
    pub fn flush_recompute(&mut self) {
        self.defer_recompute = false;
        self.ensure_visible_fresh();
    }

    /// Flush a batched recompute deferred by `begin_recompute_batch`
    /// before a by-key scan of `self.visible`, so a lookup never misses a
    /// row an upsert added — or re-sorted — earlier in the same drain
    /// batch (#1030). A `WorkspaceFocusRequested` / `ProjectUpserted` /
    /// merge-follow can land mid-batch, right after a deferred upsert, and
    /// must see the fresh list. No-op when nothing is pending.
    fn ensure_visible_fresh(&mut self) {
        if self.recompute_pending {
            self.recompute_visible_inner(true);
        }
    }

    /// Test-only: number of full visible-list rebuilds performed so far.
    #[cfg(test)]
    pub fn recompute_count(&self) -> usize {
        self.recompute_count
    }

    /// See the `pane_state_rev` field (#1237).
    pub fn pane_state_rev(&self) -> u64 {
        self.pane_state_rev
    }

    /// Test-only: number of workspace rows in the current visible list.
    #[cfg(test)]
    pub fn visible_workspace_count(&self) -> usize {
        self.visible
            .iter()
            .filter(|r| matches!(r, VisibleRow::Workspace(_)))
            .count()
    }

    /// Rebuild the stacked-PR index (#969) over the full workspace set.
    /// A workspace lands in the index only when its PR is part of a stack
    /// — `detect_stacks` already drops standalone PRs based on the repo
    /// default branch.
    fn recompute_stacks(&mut self) {
        let prs: Vec<&lazybox_core::Task> = self
            .workspaces
            .values()
            .filter_map(|w| w.pr.as_ref())
            .collect();
        let by_task = lazybox_core::detect_stacks(prs);
        self.stacks = self
            .workspaces
            .iter()
            .filter_map(|(key, w)| {
                let pos = by_task.get(&w.pr.as_ref()?.id)?;
                Some((key.clone(), pos.clone()))
            })
            .collect();
    }

    /// Stack position of a workspace's PR, if it participates in a stack.
    /// Read by the merge dispatch (warn before merging a child ahead of
    /// its still-open parent) and by the right pane's header.
    pub fn stack_info(&self, key: &SessionKey) -> Option<&lazybox_core::StackPosition> {
        self.stacks.get(key)
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
        // Rebuild immediately — the cursor fixup below reads the fresh
        // `self.visible`, so this path must not be deferred by a batch.
        self.recompute_visible_inner(true);
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
        // This rebuild fulfills any recompute deferred during a batch.
        self.recompute_pending = false;
        // Every workspace-data change the daemon pushes lands here (the
        // upsert handlers recompute), as does any local filter/sort/
        // collapse/pin re-projection — so bumping the version here is the
        // one place that covers all workspace-content changes for the
        // render-line cache (#1090). Render-time-only inputs (cursor,
        // agent state, spinner, selection) are folded into the signature
        // separately, since those don't recompute the list.
        self.data_version = self.data_version.wrapping_add(1);
        #[cfg(test)]
        {
            self.recompute_count += 1;
        }
        // Snapshot cursor anchors before the rebuild so we can
        // restore the user's focused row when the new visible list
        // is in place. Two anchors: (a) parked-on-header preserves
        // the header name; (b) parked-on-workspace/session preserves
        // (workspace_key, session_id?) — fallbacks handle the case
        // where the prior row vanished.
        let prior_key = self.selected_session_key().cloned();
        let prior_session = self.selected_session_id();
        // A header park is preserved by identity — a repo header and a
        // Space header can share a name (owner auto-seed), so we keep
        // the whole row to re-park on the exact same variant.
        let prior_header = if preserve_header_park {
            match self.visible.get(self.cursor) {
                Some(row @ (VisibleRow::RepoHeader(_) | VisibleRow::SpaceHeader(_))) => {
                    Some(row.clone())
                }
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
                focused_workspaces: &self.focused_workspaces,
                spaces: &self.spaces,
                collapsed_spaces: &self.collapsed_spaces,
                collapsed_tickets: &self.collapsed_tickets,
                attention: &self.attention,
                agents: &self.agents,
                now: self.now(),
                search: self.search.as_ref(),
            },
        );
        self.visible = outcome.visible;
        self.repo_summaries = outcome.summaries;
        self.ticket_tree = outcome.ticket_tree;
        self.recompute_stacks();
        self.recompute_searched_keys();
        // Prune multi-select marks the new projection hid (#1243): a
        // hidden mark can't be seen or un-marked, yet it used to linger
        // in the set and silently re-join the target pool when the
        // filter / mailbox / search changed again. Pruning here keeps
        // one invariant everywhere — marked ⇒ visible — so the header
        // count, the Esc gate, and `resolve_targets` can never disagree.
        if !self.broadcast_selected.is_empty() {
            let shown: std::collections::HashSet<&SessionKey> = self
                .visible
                .iter()
                .filter_map(|row| match row {
                    VisibleRow::Workspace(k) => Some(k),
                    VisibleRow::Session { workspace, .. } => Some(workspace),
                    _ => None,
                })
                .collect();
            self.broadcast_selected.retain(|k| shown.contains(k));
        }

        // Preserve cursor on a repo header across reorderings — j/k
        // can land on headers (collapse target), and snapshots
        // arriving while parked there shouldn't yank focus.
        if let Some(header) = prior_header
            && let Some(idx) = self.visible.iter().position(|r| *r == header)
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
                    VisibleRow::FocusedHeader
                    | VisibleRow::HopperHeader
                    | VisibleRow::SpaceHeader(_)
                    | VisibleRow::RepoHeader(_)
                    | VisibleRow::KindHeader(_) => false,
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

    /// Rebuild [`Self::searched_keys`] from the freshly-computed visible
    /// list — the workspace keys the active search's scope covers, which
    /// `render` highlights. Uses the shared `search_scope_covers`
    /// predicate (the same one the visible-row filter uses) so a
    /// highlighted row can never diverge from what the filter kept, and
    /// keeps the per-row `group_label` work here (once per recompute)
    /// rather than in the per-frame render path (#1099).
    fn recompute_searched_keys(&mut self) {
        let keys: std::collections::HashSet<SessionKey> = self
            .visible
            .iter()
            .filter_map(|r| match r {
                VisibleRow::Workspace(k) => {
                    let w = self.workspaces.get(k)?;
                    crate::components::visible_rows::search_scope_covers(
                        self.search.as_ref(),
                        w,
                        &self.projects,
                        &self.workspaces,
                    )
                    .then(|| k.clone())
                }
                _ => None,
            })
            .collect();
        self.searched_keys = keys;
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
        // Footer PRIORITY heuristic only: cached readiness decides
        // whether merge or work/fix-CI is the better hint for this row.
        // `g m` availability itself is structural (#1203) — an open PR
        // always merges on request, with the cached state as a send
        // advisory — so a "not ready" cache here hides nothing, it just
        // promotes the likelier next action.
        let is_ready = self.merge_target_for_cursor().is_some()
            && workspace
                .and_then(|w| w.pr.as_ref())
                .is_some_and(|pr| lazybox_tui_core::intent::merge_block_reason(pr).is_none());
        let mut actions: Vec<Action> = Vec::with_capacity(6);

        // A live multi-select means every normal Workspace action now
        // targets the whole set (#932); surface the free-text broadcast
        // first as the one selection-only action that has no single-row
        // equivalent.
        if !self.broadcast_selected.is_empty() {
            actions.push(Action::BroadcastToSelected);
        }

        let focused_credit_blocks = self
            .selected_session_key()
            .map(|key| self.credit_exhausted_terminals_for(key))
            .unwrap_or_default();
        if focused_credit_blocks.len() == 1 {
            actions.push(Action::RecoverAgentCredit);
        }
        if self.credit_exhausted_terminals().len() > 1 {
            actions.push(Action::RecoverAllAgentCredit);
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
            actions.push(Action::ToggleFocusWorkspace);
        }
        // Repo-group collapse/expand (`Space`) and pin (`p`) are
        // dropped from the footer on WORKSPACE rows (#1026): there
        // they're obvious, always-available, and mouse-discoverable
        // (click the ▾/▸ header triangle), so a permanent cell just
        // crowds out the state-driven hints (merge/work/mark-read/…)
        // that matter on the selected row. They stay in `?` help
        // (catalog-driven) and dispatch unchanged.
        //
        // Collapse is restored on a repo/space HEADER row, where no
        // workspace is selected so nothing state-driven competes and
        // folding the group you're sitting on IS the likely next action
        // — the "show only when nothing better competes" case. Pin stays
        // dropped even here: it's the secondary action on a header.
        if self
            .cursor_ticket_tree()
            .is_some_and(|meta| meta.has_children)
            || (workspace.is_none() && self.cursor_repo().is_some())
        {
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
                        // On a header row the verb tracks the group's
                        // state so the footer never says "collapse" over
                        // an already-collapsed group (#338).
                        Action::ToggleRepoGroup => std::borrow::Cow::Borrowed(
                            if self
                                .cursor_ticket_tree()
                                .is_some_and(|meta| meta.has_children)
                            {
                                if self.cursor_ticket_tree().is_some_and(|meta| meta.collapsed) {
                                    "expand children"
                                } else {
                                    "collapse children"
                                }
                            } else if self.cursor_repo_collapsed() == Some(true) {
                                "expand group"
                            } else {
                                "collapse group"
                            },
                        ),
                        // The verb tracks the cursor workspace's star
                        // state so the footer never says "focus" over an
                        // already-starred row.
                        Action::ToggleFocusWorkspace => std::borrow::Cow::Borrowed(
                            if self
                                .selected_session_key()
                                .is_some_and(|k| self.is_focused(k))
                            {
                                "unfocus"
                            } else {
                                "focus"
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

/// Format one plan-quota window as the header fragment the summary shows:
/// `"45%"`, or `"45% · 2h"` when a reset countdown is known and still in the
/// future. `None` when the window is absent *or* its reset has already
/// passed. `utilization_bp` is in basis points (0..=10000); it rounds to a
/// whole percent. `now_unix` is passed in (not read from the clock here) so
/// the mapping stays testable.
fn format_quota_window(window: Option<lazybox_ipc::QuotaWindow>, now_unix: i64) -> Option<String> {
    let window = window?;
    // A reset that has already passed means the provider rolled the window
    // over: the utilization we last observed describes the *previous* window,
    // not the current one, so the honest answer is "unknown" — not the
    // pre-reset ceiling. Drop the window entirely rather than reporting a
    // stale percentage; the whole point of "can I keep working?" is that
    // restored headroom reads as restored. A window with no reset at all is
    // left alone: its staleness is unknowable, so we still show what we saw.
    if let Some(reset_at) = window.reset_at
        && reset_at <= now_unix
    {
        return None;
    }
    let pct = ((window.utilization_bp + 50) / 100).min(100);
    match window
        .reset_at
        .and_then(|at| format_reset_countdown(at, now_unix))
    {
        Some(countdown) => Some(format!("{pct}% · {countdown}")),
        None => Some(format!("{pct}%")),
    }
}

/// A compact relative countdown to `reset_at_unix` from `now_unix`:
/// `"45s"`, `"7m"`, `"2h"`, `"3d"`. `None` once the reset is in the past
/// (the caller drops a passed-reset window entirely; this never emits a
/// negative countdown).
fn format_reset_countdown(reset_at_unix: i64, now_unix: i64) -> Option<String> {
    let secs = reset_at_unix.checked_sub(now_unix)?;
    if secs <= 0 {
        return None;
    }
    let secs = secs as u64;
    Some(if secs < 60 {
        format!("{secs}s")
    } else if secs < 3_600 {
        format!("{}m", secs.div_ceil(60))
    } else if secs < 86_400 {
        format!("{}h", secs.div_ceil(3_600))
    } else {
        format!("{}d", secs.div_ceil(86_400))
    })
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
pub use lazybox_tui_core::inbox::{Filter, FilterAxis, FilterCtx, FilterEntry, FilterSet};
// `workspace_needs_attention`'s production caller moved into
// `inbox::compute_visible` with the grouping logic (#731); inside `tui`
// only the attention-signal unit tests still exercise it, so the
// re-export is test-only.
#[cfg(test)]
pub(crate) use lazybox_tui_core::inbox::workspace_needs_attention;

// Re-export the ratatui-styled pills.rs items so callers in the rest of
// the crate keep their `crate::components::sidebar::*` import paths.
pub(crate) use pills::{
    ARM_GLYPH, AUTO_GLYPH, CLAIM_GLYPH, FIX_GLYPH, LegendRow, TRACK_GLYPH, badge_pill_style,
    relative_time, role_badge, status_legend, status_pills, workspace_type_label,
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
