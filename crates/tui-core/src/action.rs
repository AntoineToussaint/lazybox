//! `Action` — the unified vocabulary of "things the user can do."
//!
//! # Why this exists
//!
//! Lazybox has three surfaces that ask "which actions are available at
//! the current cursor?":
//!   - **Keyboard**: `handle_key` matches a chord → fires an IPC.
//!   - **Footer hint bar**: lists the contextual short labels.
//!   - **Right-click / context menu**: lists the same actions as a
//!     pickable menu.
//!
//! Before this module each surface had its own gating logic, with
//! the same predicates duplicated three times (and drifting — we
//! shipped `g` as a sidebar refresh keystroke without it ever
//! appearing in the help). This module is the single source of
//! truth: every action gets one `ActionDef` with its canonical key
//! binding + label + availability predicate. All three surfaces
//! read the same catalog.
//!
//! # Plugin awareness
//!
//! Lazybox's data model is plugin-shaped: providers (github, linear,
//! …) emit `Workspace`s; lazybox wraps them in a uniform UI. Actions
//! follow the same rule — they target a `Workspace` or a `Project`
//! (the repo/sandbox container) and don't bake in provider-specific
//! verbs. `MergePr` is the one exception that surfaces a github-
//! flavored verb; we'll generalize when Linear/Jira need a merge-
//! equivalent.
//!
//! # Not in scope yet
//!
//! - Keyboard → Action lookup. Each pane still owns its key-match
//!   logic; this module just owns the vocabulary + catalog.
//! - Configurable rebinding from `~/.lazybox/config.yaml`. The
//!   catalog returns the *default* binding; user overrides will
//!   layer on top later.
//! - `Cursor` abstraction (`Sidebar::Workspace(K)::Session(S)`).
//!   Today the cursor is implicit (pane focus + selected_workspace);
//!   formalizing it is the next refactor.

use std::fmt;

/// Every distinct user-driven action. Group by surface in the
/// catalog (sidebar / right pane / global) but the enum itself is
/// flat so dispatch doesn't have to walk a nested enum.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Action {
    // ── Workspace-scoped (sidebar) ─────────────────────────────────
    /// Open the focused workspace (mount activity / focus terminals).
    OpenWorkspace,
    /// Spawn the default agent (claude) on the focused workspace,
    /// optionally with a pre-built "work on this" prompt.
    Work,
    /// "Work on this" scoped to a specific agent id — the `w c` / `w x`
    /// / `w u` leader chords. Builds the same contextual prompt as
    /// [`Action::Work`] but forces the chosen agent (injecting if it's
    /// already running, else spawning it). The id is dynamic because
    /// the agent registry is config-driven.
    WorkWith(String),
    /// Spawn a specific agent by id (claude / codex / cursor / …).
    /// The id is dynamic because the agent registry is config-driven.
    SpawnAgent(String),
    /// "Work on this" with an explicit model tier — the `w S` / `w M`
    /// chords. Same contextual prompt + target-agent resolution as
    /// [`Action::Work`], but the carried tier alias is threaded to the
    /// daemon, which resolves it against the target agent's model menu.
    WorkTier(String),
    /// Spawn the default agent with an explicit model tier — the `a S`
    /// / `a M` chords. Like [`Action::SpawnAgent`] on the default agent
    /// but carrying the picked tier alias.
    SpawnTier(String),
    /// Spawn a shell in the focused workspace's worktree.
    SpawnShell,
    /// Spawn a specific agent on the repo's shared **main checkout**
    /// (default branch) rather than an isolated worktree — the `b c` /
    /// `b x` / `b u` leader chords. Riskier (edits land on the shared
    /// branch), so it's confirm-guarded. Id is dynamic like
    /// [`Action::SpawnAgent`].
    SpawnAgentOnMain(String),
    /// Spawn a shell on the repo's shared main checkout (`b s`).
    /// Confirm-guarded for the same reason as [`Action::SpawnAgentOnMain`].
    SpawnShellOnMain,
    /// Open the workspace's worktree in the user's editor.
    OpenEditor,
    /// Create a brand-new pre-PR workspace (asks for a name).
    NewWorkspace,
    /// Create a brand-new local Project — a top-level container the
    /// sidebar groups workspaces under. Asks for a name. Idempotent
    /// on collision (re-opens the existing local project).
    NewProject,
    /// Scan the configured dev roots (`scan.roots`) for on-disk git
    /// clones and import a chosen one as a **linked (no-worktree)**
    /// workspace — lazybox works directly in the existing checkout.
    ImportCheckout,
    /// Mark every activity row on the focused workspace read.
    MarkAllRead,
    /// Toggle snooze on the focused workspace (short snooze, ~4h).
    ToggleSnooze,
    /// Long-snooze the focused workspace (~1 year — effectively "hide").
    /// Confirm-guarded because it has no obvious undo.
    LongSnooze,
    /// Archive the workspace + kill any of its sessions. Destructive.
    Archive,
    /// Close the focused GitHub issue upstream (as `NOT_PLANNED`).
    /// Only surfaces on issue-only GitHub workspaces — a workspace
    /// with a PR merges/closes the PR instead. GitHub can't truly
    /// *delete* an issue via the API without elevated permissions, so
    /// "delete" resolves to a close here (reversible, provider-side).
    /// Confirm-guarded.
    CloseIssue,
    /// Merge the workspace's PR if it's in a merge-ready state. Only
    /// surfaces for provider workspaces that have a merge concept
    /// (today: github PRs).
    MergePr,
    /// Toggle the workspace's "auto-merge on green" arm. When armed,
    /// the client auto-fires a merge the moment this workspace's own
    /// PR becomes merge-ready. Distinct from GitHub's native
    /// auto-merge; acts only while lazybox is running.
    ToggleAutoMerge,
    /// Open the unified automation-policies menu for the focused
    /// PR/issue (issue #363): one surface listing every policy
    /// (merge-on-green, per-session auto-fix arm/disarm, GitHub-native
    /// auto-merge status) with its on/off state, toggled in place.
    ManagePolicies,
    /// Move every session from the focused workspace to another.
    AdoptSessions,
    /// Hand off the focused agent's on-screen output to another
    /// session (issue #431): capture what this agent produced, pick a
    /// target workspace, edit the brief, and inject + submit it into
    /// the target's agent. The user routes it — no agent tooling — so
    /// the planner→executor pipe stays in one keystroke chain.
    SendToSession,
    /// Manually fold an issue workspace into the PR workspace that
    /// closes it. Only available when the local state already knows
    /// of a PR claiming this issue (via `closes_issues`). Same end-
    /// state as the daemon's auto-prompt path; this one bypasses
    /// the `rejected_merge` dedupe so a previously-dismissed prompt
    /// becomes actionable.
    CollapseIntoPr,
    /// Add reviewer(s) to the workspace's PR (github GraphQL mutation).
    RequestReviewers,
    /// Add assignee(s) to the workspace's PR or issue.
    AddAssignees,
    /// Open the label picker on the workspace's PR or issue. Pre-
    /// checks the currently-applied labels; submit replaces the
    /// label set on the upstream provider.
    ManageLabels,
    /// Re-poll just the focused workspace's own GitHub entities (its PR
    /// and linked issues) instead of the global refresh — the "sync
    /// this" action for when you're waiting on one PR's CI or one
    /// issue's state. Cheap next to a full sweep; updates that row's
    /// state and read markers only.
    SyncWorkspace,
    /// Open the focused workspace's PR / issue page in the host's
    /// default web browser. Useful for jumping to GitHub when the
    /// in-lazybox UI doesn't carry every affordance yet (mobile-rich
    /// review thread, full diff view, etc.).
    OpenInBrowser,
    /// Delete or close the workspace's upstream item, resolved by
    /// kind: a PR is closed without merging; an issue is hard-deleted
    /// when the token has the admin rights GitHub requires, degrading
    /// to a close-as-not-planned (with a notice) otherwise.
    /// Confirm-guarded — destructive and outward-facing.
    DeleteOrClose,
    /// Open the notes editor (a Textarea) for the focused workspace —
    /// a free-form local scratchpad that never syncs to a provider
    /// (issue #458). Pre-filled with the current note; submit persists.
    EditNotes,

    // ── Sidebar list management ────────────────────────────────────
    // These act on the sidebar's list/view rather than a single
    // workspace: filtering, sorting, switching mailbox, searching.
    // They only resolve when the sidebar has focus (Section::Sidebar)
    // so they never bleed into the activity pane the way the
    // workspace-scoped actions deliberately do.
    /// Open the composable filter menu (state / role / kind predicates).
    OpenFilterMenu,
    /// Cycle the sort order (Default → ByRole → ByRoleSplit).
    CycleSort,
    /// Cycle the mailbox view (Inbox → Inactive → Snoozed).
    CycleMailbox,
    /// Open the incremental search bar scoped to the focused project.
    OpenSearch,
    /// Collapse or expand the repo group the cursor sits in (or on).
    /// Folds a project's workspaces into a single header row — the
    /// "group the sessions" shortcut. Acts on the list, not a single
    /// workspace, so it lives in the Sidebar section.
    ToggleRepoGroup,
    /// Toggle the focused workspace row in/out of the sidebar's
    /// multi-select set — the targets a broadcast
    /// ([`Action::BroadcastToSelected`]) fans out to. Selection
    /// survives j/k navigation; Esc clears it.
    SelectWorkspace,
    /// Send one instruction — a snippet, free text, or both — to every
    /// multi-selected workspace in one shot ("merge when green" to N
    /// PRs at once). Each target's running agent gets the settle-gated
    /// inject; a plain shell gets a direct write; workspaces with no
    /// session are skipped and reported.
    BroadcastToSelected,

    // ── Activity pane (right) ──────────────────────────────────────
    /// Toggle the activity-section collapse on the focused workspace.
    ToggleActivity,
    /// Toggle a single activity row's expanded view.
    ToggleRow,
    /// Jump the activity row cursor to the first row (`g` under Right
    /// focus — the vim go-to-top reflex). Catalog-dispatched so the
    /// keystroke can't fall through to arming the Workspace `g *`
    /// github leader (where a reflexive `g g` toggled auto-merge).
    ActivityTop,
    /// Jump the activity row cursor to the last row (`Shift-G`).
    ActivityBottom,
    /// Mount the reply textarea targeted at the focused workspace.
    Reply,
    /// Toggle the multi-select state on the focused activity row.
    SelectRow,
    /// Toggle the PR / issue description visibility.
    ToggleDescription,
    /// Undo the most recent auto-mark-read (`z`).
    UndoMarkRead,

    // ── Global / cross-pane ────────────────────────────────────────
    /// Cycle pane focus (Tab).
    CyclePane,
    /// Force a fresh poll of every provider (Shift+R / g).
    Refresh,
    /// Clear the host terminal and repaint the whole UI from scratch
    /// (Ctrl-L). Recovery hatch for a screen left stale or garbled by
    /// something no event reports — e.g. display sleep/wake.
    ForceRedraw,
    /// Open Ask Lazybox (`?`): live keymap search plus conversational help.
    OpenHelp,
    /// Launch the in-app feature tour / guided walkthrough.
    OpenTour,
    /// Open the debug / sync-status window (Shift+D).
    OpenSyncStatus,
    /// Open the messages log — a scrollable, clearable list of recent
    /// footer notices (errors, warnings, info) so a notice that flashed
    /// and faded, or one the user missed, is still readable after the
    /// fact. The durable half of the footer's transient surface.
    OpenMessages,
    /// Clear the current footer notice regardless of severity. Severity
    /// still decides auto-fade (Retryable/Info fade on their timers;
    /// Permanent/Auth stay), but this lets the user swat any notice away
    /// on demand — the merge false-error (#305) that sat red with no
    /// way to clear it is the motivating case.
    DismissNotice,
    /// Open the `,` Settings palette.
    OpenSettings,
    /// Open the theme picker — a live-preview list of every registered
    /// palette. Highlighting previews; Enter keeps + persists to
    /// `ui.theme`; Esc restores the theme that was active on open.
    OpenThemePicker,
    /// Open the snippets browser — a read-only modal listing every
    /// snippet (key, origin, description, body) so the library is
    /// discoverable outside the `]]s<key>` terminal leader.
    OpenSnippets,
    /// Open a fuzzy picker over every workspace (across repos) and
    /// jump the cursor to the one chosen (default `` ` ``). The
    /// general switcher the narrow `!` / `Shift-F` jumps lacked —
    /// reachable from any pane, including inside an agent terminal
    /// via the `]]` leader.
    JumpToWorkspace,
    /// Jump the sidebar cursor to the next workspace whose agent
    /// is in `Asking` state (`!`). Wraps around.
    JumpToAsking,
    /// Jump the sidebar cursor to the next workspace whose PR has
    /// failing / mixed CI (`Shift-F`). Wraps around.
    JumpToFailingCi,
    /// Toggle focus mode — maximize the focused workspace's terminal
    /// to near-fullscreen behind a slim event header, hiding the
    /// sidebar and activity pane (`.` from the sidebar, `]]f` from
    /// inside a terminal). Jump straight to a specific agent with
    /// `]]<digit>` (sidebar order, top-down).
    ToggleFocusMode,
    /// Start a fresh agent session from anywhere (`Shift-W`): pick a
    /// project, name the workspace, and the daemon creates it and
    /// spawns the default agent in one step. The zero-friction entry
    /// point for "I just want to start working" — no need to first
    /// navigate the sidebar to a project header.
    StartAgent,
    /// Show or hide the Activity (right) pane for the focused
    /// workspace. The pane auto-hides when the workspace has no
    /// activity worth showing; this reveals it on demand (and
    /// re-hides it). The override is remembered per workspace for
    /// the session.
    ToggleActivityPane,
    /// Toggle lazybox's mouse capture so the host terminal regains
    /// native text selection (F8 / Alt-s / Ctrl-Alt-s). Reachable from
    /// every pane INCLUDING a live terminal — the whole point is
    /// escaping to a host copy gesture mid-agent-session — so the
    /// keyboard path matches it before PTY forwarding.
    ToggleMouseCapture,
    /// Begin the two-press quit chord. Single-press from a remap
    /// just fires.
    Quit,
    /// Resize the active splitter (Shift+Arrow).
    ResizeSplitter(ResizeDirection),

    // ── Terminal-pane scoped ───────────────────────────────────────
    /// Scroll the focused terminal's scrollback (Shift+PgUp/Dn).
    TerminalScroll(ScrollDirection),
    /// Escape the terminal back to sidebar focus (`]]q`).
    LeaveTerminal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResizeDirection {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScrollDirection {
    Up,
    Down,
}

/// Static definition of an action's user-facing surface.
///
/// One per `Action` variant — looked up via [`ActionDef::for_action`]
/// or iterated whole via [`ActionDef::all`].
#[derive(Debug, Clone, Copy)]
pub struct ActionDef {
    /// What the action does, abstractly.
    pub kind: ActionKind,
    /// Default key binding. Display string is canonical for the
    /// help panel; rebinding lives in user config (later).
    pub default_keys: &'static str,
    /// Short verb-phrase used in the footer hint bar.
    pub label: &'static str,
    /// Longer description for the help panel + context menu.
    pub describe: &'static str,
    /// Which surface section this lives under in the help panel.
    pub section: Section,
}

/// Static-only stand-in for `Action` (since `Action::SpawnAgent` has
/// a String payload and we want `ActionDef::ALL` to be `&'static`).
///
/// For variants that carry data, the data is supplied at lookup
/// time — see `for_action(&Action)` which builds an owned `ActionDef`
/// substituting the runtime label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ActionKind {
    // Workspace
    OpenWorkspace,
    Work,
    WorkWith,
    SpawnAgent,
    SpawnShell,
    SpawnAgentOnMain,
    SpawnShellOnMain,
    OpenEditor,
    NewWorkspace,
    NewProject,
    ImportCheckout,
    MarkAllRead,
    ToggleSnooze,
    LongSnooze,
    Archive,
    CloseIssue,
    MergePr,
    ToggleAutoMerge,
    ManagePolicies,
    AdoptSessions,
    SendToSession,
    CollapseIntoPr,
    RequestReviewers,
    AddAssignees,
    ManageLabels,
    SyncWorkspace,
    OpenInBrowser,
    DeleteOrClose,
    EditNotes,
    // Sidebar list management
    OpenFilterMenu,
    CycleSort,
    CycleMailbox,
    OpenSearch,
    ToggleRepoGroup,
    SelectWorkspace,
    BroadcastToSelected,
    // Activity
    ToggleActivity,
    ToggleRow,
    ActivityTop,
    ActivityBottom,
    Reply,
    SelectRow,
    ToggleDescription,
    UndoMarkRead,
    // Global
    CyclePane,
    ToggleMouseCapture,
    Refresh,
    ForceRedraw,
    OpenHelp,
    OpenTour,
    OpenSyncStatus,
    OpenMessages,
    DismissNotice,
    OpenSettings,
    OpenThemePicker,
    OpenSnippets,
    JumpToWorkspace,
    JumpToAsking,
    JumpToFailingCi,
    ToggleFocusMode,
    StartAgent,
    ToggleActivityPane,
    Quit,
    ResizeSplitter,
    // Terminal
    TerminalScroll,
    LeaveTerminal,
}

impl ActionKind {
    /// Number of real variants in this contiguous `repr(u8)` enum.
    /// `LeaveTerminal` deliberately stays last: the fixed-size display
    /// order below then becomes a compile-time completeness check whenever
    /// a new action kind is added.
    const COUNT: usize = Self::LeaveTerminal as usize + 1;

    /// Canonical help/catalog order. The array length is tied to the enum's
    /// variant count, so omitting a newly-added action is a compile error;
    /// the unit test below separately rejects duplicates.
    const DISPLAY_ORDER: [Self; Self::COUNT] = [
        // Global
        Self::CyclePane,
        Self::Refresh,
        Self::ForceRedraw,
        Self::OpenSettings,
        Self::OpenThemePicker,
        Self::OpenSnippets,
        Self::OpenHelp,
        Self::OpenTour,
        Self::OpenSyncStatus,
        Self::OpenMessages,
        Self::DismissNotice,
        // The three Jump actions sit together so the help panel reads
        // them as one coherent group.
        Self::JumpToWorkspace,
        Self::JumpToAsking,
        Self::JumpToFailingCi,
        Self::ToggleFocusMode,
        Self::StartAgent,
        Self::ToggleActivityPane,
        Self::ToggleMouseCapture,
        Self::ResizeSplitter,
        Self::Quit,
        // Workspace
        Self::OpenWorkspace,
        Self::Work,
        Self::WorkWith,
        Self::SpawnAgent,
        Self::SpawnShell,
        Self::SpawnAgentOnMain,
        Self::SpawnShellOnMain,
        Self::OpenEditor,
        Self::MarkAllRead,
        Self::ToggleSnooze,
        // Workspace-management menu: creation and movement first,
        // hiding/destructive actions last. The runtime which-key popup
        // inherits this order directly.
        Self::NewWorkspace,
        Self::NewProject,
        Self::ImportCheckout,
        Self::AdoptSessions,
        Self::SendToSession,
        Self::CollapseIntoPr,
        Self::LongSnooze,
        Self::Archive,
        Self::CloseIssue,
        // GitHub menu.
        Self::MergePr,
        Self::ToggleAutoMerge,
        Self::ManagePolicies,
        Self::RequestReviewers,
        Self::AddAssignees,
        Self::ManageLabels,
        Self::SyncWorkspace,
        Self::OpenInBrowser,
        Self::DeleteOrClose,
        Self::Reply,
        Self::EditNotes,
        // Sidebar list management
        Self::OpenFilterMenu,
        Self::CycleSort,
        Self::CycleMailbox,
        Self::OpenSearch,
        Self::ToggleRepoGroup,
        Self::SelectWorkspace,
        Self::BroadcastToSelected,
        // Activity
        Self::ToggleActivity,
        Self::ToggleRow,
        Self::ActivityTop,
        Self::ActivityBottom,
        Self::ToggleDescription,
        Self::SelectRow,
        Self::UndoMarkRead,
        // Terminal
        Self::TerminalScroll,
        Self::LeaveTerminal,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    Global,
    Workspace,
    /// Sidebar list/view management (filter, sort, mailbox, search).
    /// Distinct from `Workspace` because these resolve ONLY under
    /// sidebar focus — they manage the list, not the selected row, so
    /// they must not shadow activity-pane keys the way workspace
    /// actions intentionally do.
    Sidebar,
    Activity,
    Terminal,
}

impl Section {
    /// Display / sort rank — also the order `ActionDef::all()` emits
    /// sections in, and the order the help panel renders them.
    pub fn order(self) -> u8 {
        match self {
            Section::Global => 0,
            Section::Workspace => 1,
            Section::Sidebar => 2,
            Section::Activity => 3,
            Section::Terminal => 4,
        }
    }

    /// Human-facing section title for the help panel.
    pub fn title(self) -> &'static str {
        match self {
            Section::Global => "Global",
            Section::Workspace => "Workspace",
            Section::Sidebar => "Sidebar",
            Section::Activity => "Activity",
            Section::Terminal => "Terminal",
        }
    }
}

impl Action {
    pub fn kind(&self) -> ActionKind {
        match self {
            Action::OpenWorkspace => ActionKind::OpenWorkspace,
            Action::Work => ActionKind::Work,
            Action::WorkWith(_) => ActionKind::WorkWith,
            Action::SpawnAgent(_) => ActionKind::SpawnAgent,
            // Tier variants reuse the parent leader group so the
            // which-key popup / footer / help treat `w S` as part of
            // the `w` "work" group and `a S` as part of the `a` "agent"
            // group.
            Action::WorkTier(_) => ActionKind::WorkWith,
            Action::SpawnTier(_) => ActionKind::SpawnAgent,
            Action::SpawnShell => ActionKind::SpawnShell,
            Action::SpawnAgentOnMain(_) => ActionKind::SpawnAgentOnMain,
            Action::SpawnShellOnMain => ActionKind::SpawnShellOnMain,
            Action::OpenEditor => ActionKind::OpenEditor,
            Action::NewWorkspace => ActionKind::NewWorkspace,
            Action::NewProject => ActionKind::NewProject,
            Action::ImportCheckout => ActionKind::ImportCheckout,
            Action::MarkAllRead => ActionKind::MarkAllRead,
            Action::ToggleSnooze => ActionKind::ToggleSnooze,
            Action::LongSnooze => ActionKind::LongSnooze,
            Action::Archive => ActionKind::Archive,
            Action::CloseIssue => ActionKind::CloseIssue,
            Action::MergePr => ActionKind::MergePr,
            Action::ToggleAutoMerge => ActionKind::ToggleAutoMerge,
            Action::ManagePolicies => ActionKind::ManagePolicies,
            Action::AdoptSessions => ActionKind::AdoptSessions,
            Action::SendToSession => ActionKind::SendToSession,
            Action::CollapseIntoPr => ActionKind::CollapseIntoPr,
            Action::RequestReviewers => ActionKind::RequestReviewers,
            Action::AddAssignees => ActionKind::AddAssignees,
            Action::ManageLabels => ActionKind::ManageLabels,
            Action::SyncWorkspace => ActionKind::SyncWorkspace,
            Action::OpenInBrowser => ActionKind::OpenInBrowser,
            Action::DeleteOrClose => ActionKind::DeleteOrClose,
            Action::OpenFilterMenu => ActionKind::OpenFilterMenu,
            Action::CycleSort => ActionKind::CycleSort,
            Action::CycleMailbox => ActionKind::CycleMailbox,
            Action::OpenSearch => ActionKind::OpenSearch,
            Action::ToggleRepoGroup => ActionKind::ToggleRepoGroup,
            Action::SelectWorkspace => ActionKind::SelectWorkspace,
            Action::BroadcastToSelected => ActionKind::BroadcastToSelected,
            Action::ToggleActivity => ActionKind::ToggleActivity,
            Action::ToggleRow => ActionKind::ToggleRow,
            Action::ActivityTop => ActionKind::ActivityTop,
            Action::ActivityBottom => ActionKind::ActivityBottom,
            Action::Reply => ActionKind::Reply,
            Action::EditNotes => ActionKind::EditNotes,
            Action::SelectRow => ActionKind::SelectRow,
            Action::ToggleDescription => ActionKind::ToggleDescription,
            Action::UndoMarkRead => ActionKind::UndoMarkRead,
            Action::CyclePane => ActionKind::CyclePane,
            Action::ToggleMouseCapture => ActionKind::ToggleMouseCapture,
            Action::Refresh => ActionKind::Refresh,
            Action::ForceRedraw => ActionKind::ForceRedraw,
            Action::OpenHelp => ActionKind::OpenHelp,
            Action::OpenTour => ActionKind::OpenTour,
            Action::OpenSyncStatus => ActionKind::OpenSyncStatus,
            Action::OpenMessages => ActionKind::OpenMessages,
            Action::DismissNotice => ActionKind::DismissNotice,
            Action::OpenSettings => ActionKind::OpenSettings,
            Action::OpenThemePicker => ActionKind::OpenThemePicker,
            Action::OpenSnippets => ActionKind::OpenSnippets,
            Action::JumpToWorkspace => ActionKind::JumpToWorkspace,
            Action::JumpToAsking => ActionKind::JumpToAsking,
            Action::JumpToFailingCi => ActionKind::JumpToFailingCi,
            Action::ToggleFocusMode => ActionKind::ToggleFocusMode,
            Action::StartAgent => ActionKind::StartAgent,
            Action::ToggleActivityPane => ActionKind::ToggleActivityPane,
            Action::Quit => ActionKind::Quit,
            Action::ResizeSplitter(_) => ActionKind::ResizeSplitter,
            Action::TerminalScroll(_) => ActionKind::TerminalScroll,
            Action::LeaveTerminal => ActionKind::LeaveTerminal,
        }
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Action::SpawnAgent(id) => write!(f, "spawn {id}"),
            Action::WorkWith(id) => write!(f, "work in {id}"),
            other => f.write_str(ActionDef::for_kind(other.kind()).label),
        }
    }
}

impl ActionDef {
    /// Lookup the catalog entry for a specific `ActionKind`. Constant
    /// time — `match` over a small enum is what the optimizer gets
    /// with a static lookup table without the runtime indirection.
    pub fn for_kind(kind: ActionKind) -> &'static ActionDef {
        // The ordering here also serves as the canonical Help-panel
        // order within each section. Add new actions in the section
        // where they belong rather than appending blindly.
        match kind {
            // ── Global ──────────────────────────────────────────────
            ActionKind::CyclePane => &Self {
                kind: ActionKind::CyclePane,
                default_keys: "Tab",
                label: "cycle panes",
                describe: "Move focus to the next pane.",
                section: Section::Global,
            },
            ActionKind::ToggleMouseCapture => &Self {
                kind: ActionKind::ToggleMouseCapture,
                default_keys: "F8 | Alt-s | Ctrl-Alt-s",
                label: "text selection",
                describe: "Toggle lazybox's mouse capture so the host terminal regains native text selection (trackpad-select + Cmd-C in agent scrollback). Works from any pane, including inside a live terminal; toggle back on for splitter drags and click-to-focus.",
                section: Section::Global,
            },
            ActionKind::Refresh => &Self {
                kind: ActionKind::Refresh,
                default_keys: "Shift-R",
                label: "refresh",
                describe: "Re-poll every provider for fresh tasks.",
                section: Section::Global,
            },
            ActionKind::ForceRedraw => &Self {
                kind: ActionKind::ForceRedraw,
                default_keys: "Ctrl-l",
                label: "redraw",
                describe: "Clear the terminal and repaint the whole UI from scratch. Use when the screen looks stale or garbled after a resize, fullscreen toggle, or display sleep/wake.",
                section: Section::Global,
            },
            ActionKind::OpenHelp => &Self {
                kind: ActionKind::OpenHelp,
                default_keys: "?",
                label: "ask lazybox",
                describe: "Search the live keymap or ask how to use lazybox in plain language. Press ? again at the empty prompt for the compact all-shortcuts index.",
                section: Section::Global,
            },
            ActionKind::OpenTour => &Self {
                kind: ActionKind::OpenTour,
                default_keys: "Shift-T",
                label: "tour",
                describe: "Launch the guided onboarding walkthrough (start from scratch, inbox, putting an agent on a task, juggling sessions, config).",
                section: Section::Global,
            },
            ActionKind::OpenSyncStatus => &Self {
                kind: ActionKind::OpenSyncStatus,
                default_keys: "Shift-D",
                label: "sync diagnostics",
                describe: "Show recent provider-sync outcomes, last poll times, and errors.",
                section: Section::Global,
            },
            ActionKind::OpenMessages => &Self {
                kind: ActionKind::OpenMessages,
                default_keys: "Shift-M",
                label: "messages",
                describe: "Open the messages log — a scrollable, clearable history of recent footer notices, so an error that flashed and faded is still readable. Press `c` there to clear it.",
                section: Section::Global,
            },
            ActionKind::DismissNotice => &Self {
                kind: ActionKind::DismissNotice,
                default_keys: "Esc",
                label: "dismiss",
                describe: "Clear the current footer notice, whatever its severity — retryable, info, permanent, or auth. Severity still decides whether a notice auto-fades on its own; this clears it now. Yields to a live terminal (Esc reaches the program) and to a sidebar multi-select (Esc drops the selection first).",
                section: Section::Global,
            },
            ActionKind::OpenSettings => &Self {
                kind: ActionKind::OpenSettings,
                default_keys: ",",
                label: "settings",
                describe: "Open the Settings palette.",
                section: Section::Global,
            },
            ActionKind::OpenThemePicker => &Self {
                kind: ActionKind::OpenThemePicker,
                default_keys: "t",
                label: "theme",
                describe: "Open the theme picker — arrow through the built-in palettes with a live preview, Enter to keep one. The choice persists to ui.theme and survives restart.",
                section: Section::Global,
            },
            ActionKind::OpenSnippets => &Self {
                kind: ActionKind::OpenSnippets,
                default_keys: "]",
                label: "snippets",
                describe: "Browse the snippet library — every `]]s<key>` shortcut with its description and body, so you can see what's available without already knowing the key. Press `e` to edit the YAML file; restart to reload.",
                section: Section::Global,
            },
            ActionKind::JumpToWorkspace => &Self {
                kind: ActionKind::JumpToWorkspace,
                default_keys: "`",
                label: "jump to workspace",
                describe: "Open a fuzzy picker over every workspace (across repos) and jump to the one you pick. Works from any pane; inside an agent terminal reach it via `]]` then `` ` ``.",
                section: Section::Global,
            },
            ActionKind::JumpToAsking => &Self {
                kind: ActionKind::JumpToAsking,
                default_keys: "!",
                label: "next asking",
                describe: "Jump the cursor to the next workspace whose agent is waiting on input (a quick jump; the workspace picker `` ` `` reaches any workspace).",
                section: Section::Global,
            },
            ActionKind::JumpToFailingCi => &Self {
                kind: ActionKind::JumpToFailingCi,
                default_keys: "Shift-F",
                label: "next failing",
                describe: "Jump the cursor to the next PR whose CI is failing (a quick jump; the workspace picker `` ` `` reaches any workspace).",
                section: Section::Global,
            },
            ActionKind::ToggleFocusMode => &Self {
                kind: ActionKind::ToggleFocusMode,
                default_keys: ".",
                label: "focus mode",
                describe: "Maximize the focused workspace's terminal to near-fullscreen behind a slim event header, hiding the sidebar and activity pane. From inside a terminal use `]]f`; jump straight to agent N with `]]<digit>` (sidebar order). Press again or `]]` to exit.",
                section: Section::Global,
            },
            ActionKind::StartAgent => &Self {
                kind: ActionKind::StartAgent,
                default_keys: "Shift-W",
                label: "start work",
                describe: "Pick a project, name a workspace, and start the default agent in it — all in one step, from any pane.",
                section: Section::Global,
            },
            ActionKind::ToggleActivityPane => &Self {
                kind: ActionKind::ToggleActivityPane,
                default_keys: "Shift-P",
                label: "activity pane",
                describe: "Show or hide the activity pane. It auto-hides when the workspace has no activity; this reveals it on demand and re-hides it.",
                section: Section::Global,
            },
            ActionKind::Quit => &Self {
                kind: ActionKind::Quit,
                default_keys: "q q",
                label: "quit",
                describe: "Quit lazybox. Default is the two-key chord; a single-letter remap fires on first press.",
                section: Section::Global,
            },
            ActionKind::ResizeSplitter => &Self {
                kind: ActionKind::ResizeSplitter,
                default_keys: "Shift-Arrows",
                label: "resize splitters",
                describe: "Grow / shrink the focused splitter.",
                section: Section::Global,
            },
            // ── Workspace ───────────────────────────────────────────
            ActionKind::OpenWorkspace => &Self {
                kind: ActionKind::OpenWorkspace,
                default_keys: "Enter",
                label: "open",
                describe: "Focus the workspace's activity / terminal.",
                section: Section::Workspace,
            },
            ActionKind::Work => &Self {
                kind: ActionKind::Work,
                default_keys: "w w",
                label: "work on this",
                describe: "Use the default or already-running agent with a contextual work prompt (fix CI, address review, implement issue, …). `w w` runs on the second key; `w c` / `w x` / `w u` choose an agent instead.",
                section: Section::Workspace,
            },
            ActionKind::WorkWith => &Self {
                kind: ActionKind::WorkWith,
                // Default binding is per-agent (`w c` / `w x` / `w u`),
                // generated in `catalog()`; this placeholder gives the
                // help panel a row. No parseable chord of its own.
                default_keys: "w c / w x / w u",
                label: "work in agent",
                describe: "Work on this with a specific agent (claude / codex / cursor / …): same contextual prompt as `w w`, but forced to the chosen agent — injecting if it's already running, else spawning it.",
                section: Section::Workspace,
            },
            ActionKind::SpawnAgent => &Self {
                kind: ActionKind::SpawnAgent,
                // Default binding is per-agent (`a c` / `a x` / `a u`,
                // generated in `catalog()` under the `a` agent leader);
                // the runtime label (`spawn claude`) carries the id.
                // Listed here so the help panel has a row, with the
                // literal multi-agent form in the keys column.
                default_keys: "a c / a x / a u",
                label: "spawn agent",
                describe: "Open a new agent terminal (claude / codex / cursor / …) in the workspace.",
                section: Section::Workspace,
            },
            ActionKind::SpawnShell => &Self {
                kind: ActionKind::SpawnShell,
                default_keys: "s",
                label: "shell",
                describe: "Open a shell in the workspace's worktree.",
                section: Section::Workspace,
            },
            ActionKind::SpawnAgentOnMain => &Self {
                kind: ActionKind::SpawnAgentOnMain,
                // Per-agent binding generated in `catalog()` (`b c` /
                // `b x` / `b u`); this placeholder gives the help panel
                // a row with the literal multi-agent form.
                default_keys: "b c / b x / b u",
                label: "agent on main",
                describe: "Open an agent terminal on the repo's shared main checkout (default branch) instead of an isolated worktree — confirmed first, since edits land on the shared branch.",
                section: Section::Workspace,
            },
            ActionKind::SpawnShellOnMain => &Self {
                kind: ActionKind::SpawnShellOnMain,
                default_keys: "b s",
                label: "shell on main",
                describe: "Open a shell on the repo's shared main checkout (default branch) instead of an isolated worktree — confirmed first, since edits land on the shared branch.",
                section: Section::Workspace,
            },
            ActionKind::OpenEditor => &Self {
                kind: ActionKind::OpenEditor,
                default_keys: "e",
                label: "editor",
                describe: "Open the worktree in the configured editor.",
                section: Section::Workspace,
            },
            ActionKind::NewWorkspace => &Self {
                kind: ActionKind::NewWorkspace,
                default_keys: "x n",
                label: "new workspace",
                describe: "Create a pre-PR workspace (asks for a name).",
                section: Section::Workspace,
            },
            ActionKind::NewProject => &Self {
                kind: ActionKind::NewProject,
                default_keys: "x p",
                // Distinct from NewWorkspace's "new workspace" — the two
                // used to share a label, rendering two identical footer
                // cells for different actions.
                label: "new project",
                describe: "Pick a tracked repo to start a workspace on, or create a new local project.",
                section: Section::Workspace,
            },
            ActionKind::ImportCheckout => &Self {
                kind: ActionKind::ImportCheckout,
                default_keys: "x i",
                label: "import checkout",
                describe: "Scan the configured dev roots (scan.roots) for on-disk git clones and import one as a linked, no-worktree workspace — lazybox works directly in the existing checkout.",
                section: Section::Workspace,
            },
            ActionKind::MarkAllRead => &Self {
                kind: ActionKind::MarkAllRead,
                default_keys: "m",
                label: "mark read",
                describe: "Mark every activity row on the focused workspace read.",
                section: Section::Workspace,
            },
            ActionKind::ToggleSnooze => &Self {
                kind: ActionKind::ToggleSnooze,
                default_keys: "z",
                label: "snooze",
                describe: "Snooze the workspace for ~4h (toggle).",
                section: Section::Workspace,
            },
            ActionKind::LongSnooze => &Self {
                kind: ActionKind::LongSnooze,
                default_keys: "x z",
                label: "long snooze",
                describe: "Snooze the workspace for ~1 year (effectively hide). Confirmed first.",
                section: Section::Workspace,
            },
            ActionKind::Archive => &Self {
                kind: ActionKind::Archive,
                default_keys: "x x",
                label: "archive",
                describe: "Drop the workspace and kill any sessions. Destructive.",
                section: Section::Workspace,
            },
            ActionKind::CloseIssue => &Self {
                kind: ActionKind::CloseIssue,
                default_keys: "x c",
                label: "close issue",
                describe: "Close the focused GitHub issue upstream (as not-planned). Only on issue workspaces; a true delete needs elevated permissions, so this closes instead. Confirmed first.",
                section: Section::Workspace,
            },
            ActionKind::MergePr => &Self {
                kind: ActionKind::MergePr,
                default_keys: "g m",
                label: "merge PR",
                describe: "Merge the PR (only when CI green + approved + no conflicts).",
                section: Section::Workspace,
            },
            ActionKind::ToggleAutoMerge => &Self {
                kind: ActionKind::ToggleAutoMerge,
                default_keys: "g g",
                label: "auto-merge on green",
                describe: "Toggle \"auto-merge on green\": arm the workspace so lazybox merges your PR automatically once CI goes green (own PR, no conflicts, no changes requested). Fires only while lazybox is running.",
                section: Section::Workspace,
            },
            ActionKind::ManagePolicies => &Self {
                kind: ActionKind::ManagePolicies,
                default_keys: "g p",
                label: "policies",
                describe: "Open the automation-policies menu for the focused PR/issue: one surface listing every policy (merge-on-green, per-session auto-fix arm/disarm, GitHub-native auto-merge status) with its on/off state, toggled in place.",
                section: Section::Workspace,
            },
            ActionKind::AdoptSessions => &Self {
                kind: ActionKind::AdoptSessions,
                default_keys: "x a",
                label: "adopt sessions",
                describe: "Move every session from this workspace into another.",
                section: Section::Workspace,
            },
            ActionKind::SendToSession => &Self {
                kind: ActionKind::SendToSession,
                default_keys: "x s",
                label: "send to session",
                describe: "Hand this agent's on-screen output off to another session: pick a target workspace, edit the brief, and inject + submit it into that session's agent.",
                section: Section::Workspace,
            },
            ActionKind::CollapseIntoPr => &Self {
                kind: ActionKind::CollapseIntoPr,
                default_keys: "x j",
                label: "join into PR",
                describe: "Fold this issue into the PR that closes it (one row instead of two).",
                section: Section::Workspace,
            },
            ActionKind::RequestReviewers => &Self {
                kind: ActionKind::RequestReviewers,
                default_keys: "g r",
                label: "reviewers",
                describe: "Request reviewer(s) on the workspace's PR.",
                section: Section::Workspace,
            },
            ActionKind::AddAssignees => &Self {
                kind: ActionKind::AddAssignees,
                default_keys: "g a",
                label: "assignees",
                describe: "Change assignees on the workspace's PR / issue — pre-checks existing; toggle to add or remove.",
                section: Section::Workspace,
            },
            ActionKind::ManageLabels => &Self {
                kind: ActionKind::ManageLabels,
                default_keys: "g l",
                label: "labels",
                describe: "Add / remove labels on the workspace's PR or issue. Picker pre-checks the labels currently applied; submit replaces the set.",
                section: Section::Workspace,
            },
            ActionKind::SyncWorkspace => &Self {
                kind: ActionKind::SyncWorkspace,
                default_keys: "g s",
                label: "sync",
                describe: "Re-poll just this workspace's PR / issue instead of every provider — a cheap, targeted refresh for when you're waiting on one PR's CI or one issue's state.",
                section: Section::Workspace,
            },
            ActionKind::OpenInBrowser => &Self {
                kind: ActionKind::OpenInBrowser,
                default_keys: "g o",
                label: "open in browser",
                describe: "Open the focused workspace's PR / issue page in your default web browser.",
                section: Section::Workspace,
            },
            ActionKind::DeleteOrClose => &Self {
                kind: ActionKind::DeleteOrClose,
                default_keys: "g d",
                label: "delete / close",
                describe: "Delete the focused issue (close as not-planned when the token lacks the admin rights a hard delete needs) or close the PR without merging. Confirmed first.",
                section: Section::Workspace,
            },
            // ── Sidebar list management ─────────────────────────────
            ActionKind::OpenFilterMenu => &Self {
                kind: ActionKind::OpenFilterMenu,
                default_keys: "f",
                label: "filter",
                describe: "Open the filter menu — toggle state (with-agent, CI-failing, conflict, unread, asking, …), role, and kind predicates. Filters combine and compose with search.",
                section: Section::Sidebar,
            },
            ActionKind::CycleSort => &Self {
                kind: ActionKind::CycleSort,
                default_keys: "o",
                label: "order",
                describe: "Cycle the sort order (recency → by-role → by-role with section headers).",
                section: Section::Sidebar,
            },
            ActionKind::CycleMailbox => &Self {
                kind: ActionKind::CycleMailbox,
                default_keys: "Shift-S",
                label: "switch mailbox",
                describe: "Cycle the mailbox view (Inbox → Inactive → Snoozed).",
                section: Section::Sidebar,
            },
            ActionKind::OpenSearch => &Self {
                kind: ActionKind::OpenSearch,
                default_keys: "/",
                label: "search",
                describe: "Open the incremental search bar scoped to the focused project.",
                section: Section::Sidebar,
            },
            ActionKind::ToggleRepoGroup => &Self {
                kind: ActionKind::ToggleRepoGroup,
                default_keys: "Space",
                label: "collapse group",
                describe: "Collapse or expand the repo group the cursor is in — fold a project's workspaces into a single header row, and unfold it again. The collapsed set persists across restarts.",
                section: Section::Sidebar,
            },
            ActionKind::SelectWorkspace => &Self {
                kind: ActionKind::SelectWorkspace,
                default_keys: "v",
                label: "select",
                describe: "Toggle the focused workspace in/out of the multi-select set. Selected rows are the targets Shift-B broadcasts to; Esc clears the selection.",
                section: Section::Sidebar,
            },
            ActionKind::BroadcastToSelected => &Self {
                kind: ActionKind::BroadcastToSelected,
                default_keys: "Shift-B",
                label: "broadcast",
                describe: "Send one instruction — a snippet, free text, or both — to every multi-selected workspace at once. Running agents get the prompt injected; plain shells get a direct write; workspaces with no session are skipped and reported.",
                section: Section::Sidebar,
            },
            // ── Activity ────────────────────────────────────────────
            ActionKind::ToggleActivity => &Self {
                kind: ActionKind::ToggleActivity,
                default_keys: "Enter",
                label: "toggle section",
                describe: "Collapse / expand the activity section.",
                section: Section::Activity,
            },
            ActionKind::ToggleRow => &Self {
                kind: ActionKind::ToggleRow,
                default_keys: "→/←",
                label: "expand/collapse",
                describe: "Expand or collapse the focused activity row.",
                section: Section::Activity,
            },
            ActionKind::ActivityTop => &Self {
                kind: ActionKind::ActivityTop,
                default_keys: "g",
                label: "top",
                describe: "Jump the activity cursor to the first row.",
                section: Section::Activity,
            },
            ActionKind::ActivityBottom => &Self {
                kind: ActionKind::ActivityBottom,
                default_keys: "Shift-G",
                label: "bottom",
                describe: "Jump the activity cursor to the last row.",
                section: Section::Activity,
            },
            ActionKind::Reply => &Self {
                kind: ActionKind::Reply,
                default_keys: "r",
                label: "reply",
                describe: "Open the reply textarea targeted at this workspace.",
                section: Section::Workspace,
            },
            ActionKind::EditNotes => &Self {
                kind: ActionKind::EditNotes,
                default_keys: "n",
                label: "notes",
                describe: "Edit this workspace's local scratchpad — a private note that never syncs to a provider.",
                section: Section::Workspace,
            },
            ActionKind::SelectRow => &Self {
                kind: ActionKind::SelectRow,
                default_keys: "Space",
                label: "select row",
                describe: "Toggle the focused activity row in/out of the multi-select set (also `v`).",
                section: Section::Activity,
            },
            ActionKind::ToggleDescription => &Self {
                kind: ActionKind::ToggleDescription,
                default_keys: "d",
                label: "description",
                describe: "Toggle the PR / issue description visibility.",
                section: Section::Activity,
            },
            ActionKind::UndoMarkRead => &Self {
                kind: ActionKind::UndoMarkRead,
                default_keys: "z",
                label: "undo mark-read",
                describe: "Re-unread the most recent auto-marked row.",
                section: Section::Activity,
            },
            // ── Terminal ────────────────────────────────────────────
            ActionKind::TerminalScroll => &Self {
                kind: ActionKind::TerminalScroll,
                default_keys: "Shift-PgUp/Dn",
                label: "scroll",
                describe: "Scroll the terminal's scrollback buffer. Shift-Home / Shift-End jump to the top / bottom; the mouse wheel scrolls too.",
                section: Section::Terminal,
            },
            ActionKind::LeaveTerminal => &Self {
                kind: ActionKind::LeaveTerminal,
                default_keys: "]]q",
                label: "exit to sidebar",
                describe: "`]]` is a non-timed leader from the terminal: `]]q` exits to the sidebar, `]]s` opens snippets, `]]f` toggles focus. A lone `]` is sent to the agent.",
                section: Section::Terminal,
            },
        }
    }

    /// Lookup the catalog entry for a specific `Action` instance.
    /// For variants with runtime data (`SpawnAgent(id)`), returns
    /// the static entry — callers that care about the per-instance
    /// label should use `Action::Display` or build their own row.
    pub fn for_action(action: &Action) -> &'static ActionDef {
        Self::for_kind(action.kind())
    }

    /// Every catalog entry. Iteration order = help-panel order.
    /// Use this for rendering the `?` modal.
    pub fn all() -> impl Iterator<Item = &'static ActionDef> {
        ActionKind::DISPLAY_ORDER.into_iter().map(Self::for_kind)
    }
}

// ──────────────────────────────────────────────────────────────────
// Chord / KeyStroke — typed representation of a binding, with parser
// ──────────────────────────────────────────────────────────────────

/// A single keystroke: modifiers + a key code. The atom every chord
/// is built from. `Copy` so `Chord::Seq` can live in a `&'static`
/// slice in the catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyStroke {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub code: ChordCode,
}

/// A bound chord: either one keystroke, or an ordered *sequence* of
/// keystrokes (a leader chord like `g m`, or the two-press `q q`).
///
/// `Seq` subsumes every leader mechanism the catalog used to express
/// out-of-band: the github `g`-group (`g m`, `g r`, …) and the two-press
/// quit (`q q`). The catalog which-key popup is then a pure function of the armed prefix — "which catalog
/// entries have a `Seq` starting with this stroke?" — instead of a
/// hardcoded `ActionGroup` table.
///
/// Parsed from the catalog's `default_keys` string so the catalog
/// stays human-readable: alternatives are separated by ` | `
/// (`"g r | Shift-V"` in a user override), and the keystrokes WITHIN
/// one alternative are space-separated (`"g m"`, `"q q"`).
/// Presentation-only strings (`"g/G"`, `"↑/↓"`, `"all keys"`) still
/// don't parse to a chord.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Chord {
    /// Single keystroke.
    Key(KeyStroke),
    /// Ordered sequence of keystrokes (`g m`, `q q`, `] ]`).
    Seq(Vec<KeyStroke>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChordCode {
    Char(char),
    Named(NamedKey),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NamedKey {
    Tab,
    Enter,
    Esc,
    Backspace,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Delete,
    Insert,
    /// Function key `F1`..`F12`.
    Function(u8),
}

impl NamedKey {
    /// Canonical display label — the same token [`KeyStroke::parse`]
    /// accepts, so display round-trips back through the parser. Returns
    /// `Cow` because a function key's label (`F8`) carries its number
    /// and can't be a `&'static str`.
    pub fn label(self) -> std::borrow::Cow<'static, str> {
        use std::borrow::Cow;
        Cow::Borrowed(match self {
            NamedKey::Tab => "Tab",
            NamedKey::Enter => "Enter",
            NamedKey::Esc => "Esc",
            NamedKey::Backspace => "Backspace",
            NamedKey::Up => "Up",
            NamedKey::Down => "Down",
            NamedKey::Left => "Left",
            NamedKey::Right => "Right",
            NamedKey::Home => "Home",
            NamedKey::End => "End",
            NamedKey::PageUp => "PgUp",
            NamedKey::PageDown => "PgDn",
            NamedKey::Delete => "Del",
            NamedKey::Insert => "Insert",
            NamedKey::Function(n) => return Cow::Owned(format!("F{n}")),
        })
    }
}

impl KeyStroke {
    /// Const helper for catalog literals + tests.
    pub const fn new(ctrl: bool, shift: bool, alt: bool, code: ChordCode) -> Self {
        Self {
            ctrl,
            shift,
            alt,
            code,
        }
    }

    /// Human-readable display, the inverse of [`KeyStroke::parse`]:
    /// modifier prefixes then the key. A `Shift-`-modified letter is
    /// shown uppercase without the prefix (`M`), matching how the
    /// catalog writes single-letter shifted keys. Used by the which-key
    /// popup and the generated Keys screen.
    pub fn display(&self) -> String {
        let mut out = String::new();
        if self.ctrl {
            out.push_str("Ctrl-");
        }
        let named = matches!(self.code, ChordCode::Named(_));
        // For a shifted single letter, fold Shift into uppercase
        // (`M`); for a shifted named key keep the explicit prefix
        // (`Shift-Tab`).
        if self.shift && named {
            out.push_str("Shift-");
        }
        if self.alt {
            out.push_str("Alt-");
        }
        match self.code {
            ChordCode::Char(' ') => out.push_str("Space"),
            ChordCode::Char(c) => {
                if self.shift {
                    out.extend(c.to_uppercase());
                } else {
                    out.push(c);
                }
            }
            ChordCode::Named(n) => out.push_str(&n.label()),
        }
        out
    }

    /// Parse one keystroke token (`"Shift-M"`, `"Ctrl-Shift-D"`,
    /// `"Tab"`, `"g"`). Returns `None` for presentation-only / unknown
    /// tokens.
    ///
    /// Char/named unification: `Space` is the one named key crossterm
    /// reports as a printable (`Char(' ')`), so the catalog form
    /// `"Space"` parses to `Char(' ')` rather than a distinct named
    /// variant — otherwise a catalog `Space` binding could never match
    /// the runtime keystroke (the snag deferred from #99).
    pub fn parse(s: &str) -> Option<Self> {
        // Presentation strings — explicitly not a keystroke. A
        // multi-glyph string with a `/` is a "this key OR that key"
        // display form (`g/G`, `→/←`); a lone `/` is the literal
        // slash key (sidebar search) and parses normally below.
        if (s.chars().count() > 1 && s.contains('/')) || s == "all keys" || s.is_empty() {
            return None;
        }
        // Strip modifier prefixes one at a time. Order matters:
        // "Ctrl-Shift-X" peels Ctrl first, then Shift.
        let mut ctrl = false;
        let mut shift = false;
        let mut alt = false;
        let mut rest = s.to_string();
        loop {
            if let Some(r) = rest.strip_prefix("Ctrl-") {
                ctrl = true;
                rest = r.to_string();
            } else if let Some(r) = rest.strip_prefix("Shift-") {
                shift = true;
                rest = r.to_string();
            } else if let Some(r) = rest.strip_prefix("Alt-") {
                alt = true;
                rest = r.to_string();
            } else {
                break;
            }
        }
        let code = match rest.as_str() {
            "Tab" => ChordCode::Named(NamedKey::Tab),
            "Enter" => ChordCode::Named(NamedKey::Enter),
            "Esc" => ChordCode::Named(NamedKey::Esc),
            // Space unifies to the printable form crossterm delivers.
            "Space" => ChordCode::Char(' '),
            "Backspace" => ChordCode::Named(NamedKey::Backspace),
            "Up" => ChordCode::Named(NamedKey::Up),
            "Down" => ChordCode::Named(NamedKey::Down),
            "Left" => ChordCode::Named(NamedKey::Left),
            "Right" => ChordCode::Named(NamedKey::Right),
            "Home" => ChordCode::Named(NamedKey::Home),
            "End" => ChordCode::Named(NamedKey::End),
            "PageUp" | "PgUp" => ChordCode::Named(NamedKey::PageUp),
            "PageDown" | "PgDn" => ChordCode::Named(NamedKey::PageDown),
            "Delete" | "Del" => ChordCode::Named(NamedKey::Delete),
            "Insert" => ChordCode::Named(NamedKey::Insert),
            // Function keys: `F1`..`F12`.
            fk if fk.starts_with('F')
                && fk[1..].parse::<u8>().is_ok_and(|n| (1..=12).contains(&n)) =>
            {
                ChordCode::Named(NamedKey::Function(fk[1..].parse().unwrap()))
            }
            // Single ASCII letter / symbol — uppercase letters
            // mean Shift-letter; lowercase stays as-is. The Shift
            // prefix takes precedence (`"Shift-M"` parses to
            // `shift=true, code=Char('m')` either way).
            other if other.chars().count() == 1 => {
                let c = other.chars().next().unwrap();
                if c.is_ascii_uppercase() {
                    shift = true;
                }
                ChordCode::Char(c.to_ascii_lowercase())
            }
            _ => return None,
        };
        Some(KeyStroke {
            ctrl,
            shift,
            alt,
            code,
        })
    }
}

impl Chord {
    /// Parse one alternative — space-separated keystrokes. A single
    /// token yields `Chord::Key`; two or more yield `Chord::Seq`.
    /// Returns `None` if any token fails to parse (presentation form,
    /// unknown key).
    pub fn parse(s: &str) -> Option<Self> {
        let trimmed = s.trim();
        let strokes: Vec<&str> = trimmed.split_whitespace().collect();
        match strokes.as_slice() {
            [] => None,
            [one] => KeyStroke::parse(one).map(Chord::Key),
            // A `/` token inside a multi-key string is the "this key OR
            // that key" presentation separator (`a c / a x / a u`), not a
            // real sequence — a lone `/` only ever binds as a single
            // chord (sidebar search). Reject so presentation strings
            // don't fabricate a bogus sequence.
            many if many.iter().any(|t| t.contains('/')) => None,
            many => {
                let parsed: Option<Vec<KeyStroke>> =
                    many.iter().map(|t| KeyStroke::parse(t)).collect();
                parsed.map(Chord::Seq)
            }
        }
    }

    /// The first keystroke of this chord — the one that arms a leader
    /// (for `Seq`) or fires directly (for `Key`).
    pub fn head(&self) -> &KeyStroke {
        match self {
            Chord::Key(k) => k,
            // A `Seq` is never constructed empty (the parser yields
            // `Key` for a single token), so `[0]` is sound.
            Chord::Seq(strokes) => &strokes[0],
        }
    }

    /// Number of keystrokes in the chord (1 for `Key`). Never zero —
    /// the parser yields `Key` for a single token and `Seq` only for
    /// two-plus, so there is deliberately no `is_empty`.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        match self {
            Chord::Key(_) => 1,
            Chord::Seq(s) => s.len(),
        }
    }
}

impl ActionDef {
    /// Every chord bound to this action — the ` | `-separated
    /// alternatives in `default_keys`, each parsed. Presentation-only
    /// alternatives are silently dropped, so an entry like `"g/G"`
    /// yields an empty list (no parseable chord).
    pub fn default_chords(&self) -> Vec<Chord> {
        self.default_keys
            .split('|')
            .filter_map(Chord::parse)
            .collect()
    }

    /// First parseable default chord, or `None` for presentation-only
    /// `default_keys`. Kept for the singular callers (quit-chord
    /// resolution, the catalog collision test).
    pub fn default_chord(&self) -> Option<Chord> {
        self.default_chords().into_iter().next()
    }

    /// Whether this action's keystroke actually fires while the
    /// terminal pane holds focus.
    ///
    /// The terminal forwards every key to the PTY by design
    /// (`TerminalStack::handle_key`) — only the Terminal-section
    /// chords (`]]` to leave, scrollback) escape that. So every
    /// "global" the splash / tour / footer advertise as universally
    /// available — `?` help, `q q` quit, `Shift-T` tour, settings,
    /// refresh, cycle-pane — does NOT fire here: the user has to press
    /// `]]` to return to the sidebar first. Surfaces gate their
    /// "always available" claims on this so the advertised set can't
    /// drift from what the terminal really dispatches (issue #114).
    pub fn available_in_terminal(&self) -> bool {
        matches!(self.section, Section::Terminal) || self.kind == ActionKind::ToggleMouseCapture
    }

    /// The guard standing between a keypress and this action firing —
    /// the third gap the catalog closed (#102 P3). Moves the
    /// double-press / confirm machinery that used to live in per-pane
    /// latches onto the catalog row:
    ///
    /// - `None` — fires immediately.
    /// - `DoublePress` — needs a timed two-press (quit's `q q`).
    /// - `Confirm(prompt)` — mounts a Confirm modal first (archive,
    ///   merge, long-snooze). The prompt is the static default; a
    ///   surface that knows specifics (the PR number) can override at
    ///   mount time.
    pub fn guard(&self) -> Guard {
        match self.kind {
            ActionKind::Quit => Guard::DoublePress,
            // Kills live sessions and drops the row — no undo. Enter backs out.
            ActionKind::Archive => Guard::Confirm {
                prompt: "Archive the focused workspace? Active sessions \
                 are killed and the row drops from the inbox.",
                default_yes: false,
            },
            // Mutates the upstream issue (reopen on GitHub to undo). Enter backs out.
            ActionKind::CloseIssue => Guard::Confirm {
                prompt: "Close this issue upstream (as not planned)? It drops \
                 out of the inbox once the close lands. Reopen on \
                 GitHub to undo.",
                default_yes: false,
            },
            // Mutates (or destroys) the upstream item. The static copy
            // covers both resolutions; the dispatcher overrides with a
            // prompt naming the focused issue/PR number + title.
            ActionKind::DeleteOrClose => Guard::Confirm {
                prompt: "Delete this issue (or close this PR) upstream? \
                 Deleting an issue is permanent; without admin rights it \
                 is closed as not-planned instead. A PR is closed without \
                 merging.",
                default_yes: false,
            },
            // Explicitly invoked, but merging mutates the mainline branch
            // immediately and is hard to undo — a reflexive Enter shouldn't merge.
            ActionKind::MergePr => Guard::Confirm {
                prompt: "Merge the focused PR? Mainline branch updates \
                 immediately and the PR closes.",
                default_yes: false,
            },
            // Hides the workspace for a year. Reversible, but a mis-hit shouldn't
            // make a row vanish — Enter backs out.
            ActionKind::LongSnooze => Guard::Confirm {
                prompt: "Long-snooze this workspace (~1 year)? It drops from \
                 the inbox until then — effectively hidden.",
                default_yes: false,
            },
            // The `b`-variant chords are a deliberate, distinct request to work
            // on main; the confirm is only an awareness gate and opening the
            // terminal destroys nothing, so Enter affirms the explicit intent.
            ActionKind::SpawnAgentOnMain | ActionKind::SpawnShellOnMain => Guard::Confirm {
                prompt: "Start this session on the shared main checkout instead of \
                 an isolated worktree? Edits and commits land on the shared \
                 branch directly, not a throwaway tree.",
                default_yes: true,
            },
            _ => Guard::None,
        }
    }

    /// True when this action is gated behind a Confirm modal — the
    /// dispatch path (`Model::dispatch_action`) routes these through a
    /// unified Confirm before firing. (Named for history; really
    /// "guard is `Confirm`".)
    pub fn is_destructive(&self) -> bool {
        matches!(self.guard(), Guard::Confirm { .. })
    }

    /// Confirm-modal prompt text, for a `Confirm`-guarded action;
    /// `None` otherwise — those shouldn't be routed through the
    /// confirm path.
    pub fn confirm_prompt(&self) -> Option<&'static str> {
        match self.guard() {
            Guard::Confirm { prompt, .. } => Some(prompt),
            _ => None,
        }
    }

    /// Which button the Confirm modal defaults Enter to for a
    /// `Confirm`-guarded action; `None` for non-confirmed actions. The
    /// value is declared per action next to its prompt in [`Self::guard`]
    /// so each destructive action owns its default instead of inheriting
    /// a blanket No at the mount site.
    pub fn confirm_default_yes(&self) -> Option<bool> {
        match self.guard() {
            Guard::Confirm { default_yes, .. } => Some(default_yes),
            _ => None,
        }
    }

    /// Resolve the effective chords (alternatives): a user override
    /// from the config map if present and parseable, otherwise the
    /// catalog defaults.
    ///
    /// `overrides` keys are the snake_case `ActionKind` names
    /// (`"merge_pr"`, `"spawn_shell"`, etc.) — see [`ActionKind::name`].
    /// A user-supplied string follows the same grammar as
    /// `default_keys` (` | `-separated alternatives, space-separated
    /// sequences); if NONE of its alternatives parse it falls back to
    /// the default rather than leaving the action unbound — a typo in
    /// YAML shouldn't break the keyboard.
    pub fn effective_chords(
        &self,
        overrides: &std::collections::BTreeMap<String, String>,
    ) -> Vec<Chord> {
        if let Some(raw) = overrides.get(self.kind.name()) {
            let parsed: Vec<Chord> = raw.split('|').filter_map(Chord::parse).collect();
            if !parsed.is_empty() {
                return parsed;
            }
        }
        self.default_chords()
    }

    /// First effective chord — user override if present and parseable,
    /// else the catalog default. Used by the singular callers
    /// (quit-chord resolution).
    pub fn effective_chord(
        &self,
        overrides: &std::collections::BTreeMap<String, String>,
    ) -> Option<Chord> {
        self.effective_chords(overrides).into_iter().next()
    }

    /// Resolve the display string for this action's effective key
    /// binding: user override if present and parseable, otherwise the
    /// catalog default. Mirrors `effective_chord` but returns the raw
    /// string for footer / help rendering — surfaces what the user
    /// actually has to press, not the catalog's default.
    pub fn effective_keys_display(
        &self,
        overrides: &std::collections::BTreeMap<String, String>,
    ) -> std::borrow::Cow<'static, str> {
        if let Some(raw) = overrides.get(self.kind.name())
            && raw.split('|').any(|alt| Chord::parse(alt).is_some())
        {
            return std::borrow::Cow::Owned(raw.clone());
        }
        std::borrow::Cow::Borrowed(self.default_keys)
    }
}

impl ActionKind {
    /// Stable snake_case identifier used as the key in the user's
    /// `action_keys` config map. Keep this in sync with the
    /// variant name — a rename here is a config-file breaking
    /// change for the user.
    pub fn name(self) -> &'static str {
        match self {
            ActionKind::OpenWorkspace => "open_workspace",
            ActionKind::Work => "work",
            ActionKind::WorkWith => "work_with",
            ActionKind::SpawnAgent => "spawn_agent",
            ActionKind::SpawnShell => "spawn_shell",
            ActionKind::SpawnAgentOnMain => "spawn_agent_on_main",
            ActionKind::SpawnShellOnMain => "spawn_shell_on_main",
            ActionKind::OpenEditor => "open_editor",
            ActionKind::NewWorkspace => "new_workspace",
            ActionKind::NewProject => "new_project",
            ActionKind::ImportCheckout => "import_checkout",
            ActionKind::MarkAllRead => "mark_all_read",
            ActionKind::ToggleSnooze => "toggle_snooze",
            ActionKind::LongSnooze => "long_snooze",
            ActionKind::Archive => "archive",
            ActionKind::CloseIssue => "close_issue",
            ActionKind::MergePr => "merge_pr",
            ActionKind::ToggleAutoMerge => "toggle_auto_merge",
            ActionKind::ManagePolicies => "manage_policies",
            ActionKind::AdoptSessions => "adopt_sessions",
            ActionKind::SendToSession => "send_to_session",
            ActionKind::CollapseIntoPr => "collapse_into_pr",
            ActionKind::RequestReviewers => "request_reviewers",
            ActionKind::AddAssignees => "add_assignees",
            ActionKind::ManageLabels => "manage_labels",
            ActionKind::SyncWorkspace => "sync_workspace",
            ActionKind::OpenInBrowser => "open_in_browser",
            ActionKind::DeleteOrClose => "delete_or_close",
            ActionKind::OpenFilterMenu => "open_filter_menu",
            ActionKind::CycleSort => "cycle_sort",
            ActionKind::CycleMailbox => "cycle_mailbox",
            ActionKind::OpenSearch => "open_search",
            ActionKind::ToggleRepoGroup => "toggle_repo_group",
            ActionKind::SelectWorkspace => "select_workspace",
            ActionKind::BroadcastToSelected => "broadcast_to_selected",
            ActionKind::ToggleActivity => "toggle_activity",
            ActionKind::ToggleRow => "toggle_row",
            ActionKind::ActivityTop => "activity_top",
            ActionKind::ActivityBottom => "activity_bottom",
            ActionKind::Reply => "reply",
            ActionKind::EditNotes => "edit_notes",
            ActionKind::SelectRow => "select_row",
            ActionKind::ToggleDescription => "toggle_description",
            ActionKind::UndoMarkRead => "undo_mark_read",
            ActionKind::CyclePane => "cycle_pane",
            ActionKind::ToggleMouseCapture => "toggle_mouse_capture",
            ActionKind::Refresh => "refresh",
            ActionKind::ForceRedraw => "force_redraw",
            ActionKind::OpenHelp => "open_help",
            ActionKind::OpenTour => "open_tour",
            ActionKind::OpenSyncStatus => "open_sync_status",
            ActionKind::OpenMessages => "open_messages",
            ActionKind::DismissNotice => "dismiss_notice",
            ActionKind::OpenSettings => "open_settings",
            ActionKind::OpenThemePicker => "open_theme_picker",
            ActionKind::OpenSnippets => "open_snippets",
            ActionKind::JumpToWorkspace => "jump_to_workspace",
            ActionKind::JumpToAsking => "jump_to_asking",
            ActionKind::JumpToFailingCi => "jump_to_failing_ci",
            ActionKind::ToggleFocusMode => "toggle_focus_mode",
            ActionKind::StartAgent => "start_agent",
            ActionKind::ToggleActivityPane => "toggle_activity_pane",
            ActionKind::Quit => "quit",
            ActionKind::ResizeSplitter => "resize_splitter",
            ActionKind::TerminalScroll => "terminal_scroll",
            ActionKind::LeaveTerminal => "leave_terminal",
        }
    }
}

// ──────────────────────────────────────────────────────────────────
// Runtime catalog — static rows + generated parameterized rows
// ──────────────────────────────────────────────────────────────────

/// Runtime parameterization of a catalog row. The static `ActionKind`
/// is the verb; `Param` carries the operand resolved at startup — today
/// only the agent id for the per-agent `SpawnAgent` rows (#102 P2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Param {
    /// Agent id for a generated `SpawnAgent` row (`claude`, `codex`, …).
    Agent(String),
    /// Model-tier alias for a generated tier row (`S`, `M`, `L`) under
    /// the `w` / `a` leaders.
    Tier(String),
}

/// The guard between a keypress and an action firing — the per-row
/// replacement for the scattered double-press / confirm latches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Guard {
    /// Fires immediately.
    None,
    /// Needs a timed two-press of the chord (e.g. quit's `q q`).
    DoublePress,
    /// Mounts a Confirm modal before firing. `prompt` is the body copy;
    /// `default_yes` picks which button Enter selects — `false` for a
    /// destructive / hard-to-undo action (a reflexive Enter backs out),
    /// `true` when the confirm is only an awareness gate in front of an
    /// explicitly-requested, benign-at-confirm-time action.
    Confirm {
        prompt: &'static str,
        default_yes: bool,
    },
}

/// A resolved catalog row: a static action plus any runtime
/// parameterization, with its effective chords (`ui.action_keys`
/// overrides already applied). Built once at startup by
/// [`ActionDef::catalog`] and consulted by keyboard dispatch, the help
/// panel, and the collision detector — the single surface that knows
/// about the generated per-agent rows the static table can't express.
#[derive(Debug, Clone)]
pub struct CatalogEntry {
    pub kind: ActionKind,
    /// Operand for a parameterized row (the agent id). `None` for the
    /// fixed actions.
    pub param: Option<Param>,
    pub section: Section,
    /// Footer / help label. Owned because generated rows carry a
    /// per-agent label (`spawn claude`).
    pub label: std::borrow::Cow<'static, str>,
    pub describe: &'static str,
    /// Effective chords — overrides already applied.
    pub chords: Vec<Chord>,
    /// Effective display string for the help panel / Keys screen.
    pub keys_display: std::borrow::Cow<'static, str>,
    /// The `ui.action_keys` map key that remaps this row
    /// (`ActionKind::name`, or `spawn_agent.<id>` for an agent row).
    pub config_key: String,
}

/// Built-in keymap preset names (#102 P4, #98 possibility F). A
/// preset is just an `action_keys` map shipped in-tree; the user
/// selects one with `ui.keymap_preset` and their own `ui.action_keys`
/// still layers on top.
pub const KEYMAP_PRESETS: &[&str] = &["default", "vim"];

/// Resolve a named keymap preset to its `action_keys` overrides, or
/// `None` for an unknown name. `default` is the bare catalog (no
/// overrides) — which is itself leaders-primary since #304: grouped
/// actions (github, agent spawns) ship only their leader chord, no
/// direct-key aliases. What remains distinct about `vim` is
/// pane-cycling on `Ctrl-w` (vim's window key). Every preset is
/// collision-checked by the `preset_*_has_no_collisions` tests so a
/// bad entry can't ship.
pub fn keymap_preset(name: &str) -> Option<std::collections::BTreeMap<String, String>> {
    let mut m = std::collections::BTreeMap::new();
    match name {
        "default" => {}
        "vim" => {
            // Vim's window key cycles panes.
            m.insert("cycle_pane".into(), "Ctrl-w".into());
        }
        _ => return None,
    }
    Some(m)
}

/// Startup-validation helper: a warning line for an unknown
/// `ui.keymap_preset` name, or `None` when the name resolves. The
/// config is never rejected — the caller surfaces the warning (footer
/// notice + messages log) and continues on the default keymap.
pub fn unknown_preset_warning(name: &str) -> Option<String> {
    if keymap_preset(name).is_some() {
        return None;
    }
    Some(format!(
        "ui.keymap_preset: unknown preset {name:?} — known presets: {} (using default)",
        KEYMAP_PRESETS.join(", ")
    ))
}

/// The in-group key a known agent binds to under the `a` agent leader
/// (`a c` spawns claude) — also the second stroke of the scoped
/// `w <key>` / `b <key>` chords. `None` for agents lazybox doesn't
/// ship a convention for — they still get a catalog row (in help,
/// remappable), just no default chord.
pub fn agent_default_key(id: &str) -> Option<char> {
    match id {
        "claude" => Some('c'),
        "codex" => Some('x'),
        "cursor" | "cursor-agent" => Some('u'),
        _ => None,
    }
}

/// The keystroke that completes a tier chord under the `w` / `a`
/// leader, derived from the tier's `alias`. A single uppercase letter
/// (`"S"`) folds into a `Shift`-modified stroke (`Shift-s`) so it reads
/// as `S` and stays clear of the lowercase agent keys (`c`/`x`/`u`) in
/// the same leader namespace; a single lowercase letter binds verbatim.
/// A multi-character alias (`"XL"`) gets no chord — it still configures
/// a model, it just isn't reachable by a two-key chord.
pub fn tier_chord_stroke(alias: &str) -> Option<KeyStroke> {
    let mut chars = alias.chars();
    let c = chars.next()?;
    if chars.next().is_some() || !c.is_ascii_alphanumeric() {
        return None;
    }
    if c.is_ascii_uppercase() {
        Some(KeyStroke::new(
            false,
            true,
            false,
            ChordCode::Char(c.to_ascii_lowercase()),
        ))
    } else {
        Some(KeyStroke::new(false, false, false, ChordCode::Char(c)))
    }
}

/// Label of the leader *group* an action's default chord lives under —
/// the name shown wherever the group is advertised as one unit: the
/// footer's collapsed group cell (`g ▸ github`), the which-key popup
/// title, and the help panel's leader headings (issue #304). Keyed by
/// `ActionKind` rather than the leader keystroke so the label follows
/// the actions when a remap moves the whole group to another key.
pub fn leader_group_label(kind: ActionKind) -> Option<&'static str> {
    match kind {
        ActionKind::MergePr
        | ActionKind::ToggleAutoMerge
        | ActionKind::ManagePolicies
        | ActionKind::RequestReviewers
        | ActionKind::AddAssignees
        | ActionKind::ManageLabels
        | ActionKind::SyncWorkspace
        | ActionKind::OpenInBrowser
        | ActionKind::DeleteOrClose => Some("github"),
        ActionKind::SpawnAgent => Some("agent"),
        ActionKind::Work | ActionKind::WorkWith => Some("work"),
        ActionKind::SpawnAgentOnMain | ActionKind::SpawnShellOnMain => Some("main branch"),
        ActionKind::NewWorkspace
        | ActionKind::NewProject
        | ActionKind::ImportCheckout
        | ActionKind::LongSnooze
        | ActionKind::Archive
        | ActionKind::CloseIssue
        | ActionKind::AdoptSessions
        | ActionKind::SendToSession
        | ActionKind::CollapseIntoPr => Some("workspace"),
        _ => None,
    }
}

/// Deliberate reading order for the five non-terminal command families.
/// Consumers use this instead of catalog insertion order, so the footer,
/// compact index, generated docs, and help-agent context all teach the same
/// mental model: do work, choose an agent, use main deliberately, operate on
/// GitHub, then manage the workspace itself.
pub const LEADER_GROUP_ORDER: &[&str] = &["work", "agent", "main branch", "github", "workspace"];

pub fn leader_group_rank(label: &str) -> usize {
    LEADER_GROUP_ORDER
        .iter()
        .position(|candidate| *candidate == label)
        .unwrap_or(LEADER_GROUP_ORDER.len())
}

impl ActionDef {
    /// Build the runtime catalog: every static action EXCEPT the
    /// generic `SpawnAgent` placeholder, plus one concrete `SpawnAgent`
    /// row per enabled agent. The agent rows are what let `a c` /
    /// `a x` / `a u` live in the catalog — remappable via
    /// `ui.action_keys` (`spawn_agent.<id>`), listed in help, and
    /// collision-checked — instead of in a side map.
    ///
    /// `overrides` (`ui.action_keys`) are applied here so every
    /// consumer reads resolved chords + display strings.
    pub fn catalog(
        agents: &[String],
        overrides: &std::collections::BTreeMap<String, String>,
    ) -> Vec<CatalogEntry> {
        Self::catalog_with_tiers(agents, overrides, &[])
    }

    /// [`ActionDef::catalog`] plus the model-tier chords: one `w S` /
    /// `a S` row per entry in `tiers` (the default work agent's model
    /// menu). The tier alias is agent-agnostic at the chord level — the
    /// daemon maps it to the actual target agent's tier at spawn — so a
    /// single set of tier chords serves whichever agent `w` resolves to.
    pub fn catalog_with_tiers(
        agents: &[String],
        overrides: &std::collections::BTreeMap<String, String>,
        tiers: &[lazybox_core::ModelTier],
    ) -> Vec<CatalogEntry> {
        let mut out: Vec<CatalogEntry> = Vec::new();
        for def in ActionDef::all() {
            // The static SpawnAgent / WorkWith / SpawnAgentOnMain rows
            // are placeholders for the generated per-agent rows below —
            // drop them. (SpawnShellOnMain is a real static row and
            // stays.)
            if matches!(
                def.kind,
                ActionKind::SpawnAgent | ActionKind::WorkWith | ActionKind::SpawnAgentOnMain
            ) {
                continue;
            }
            out.push(CatalogEntry {
                kind: def.kind,
                param: None,
                section: def.section,
                label: std::borrow::Cow::Borrowed(def.label),
                describe: def.describe,
                chords: def.effective_chords(overrides),
                keys_display: def.effective_keys_display(overrides),
                config_key: def.kind.name().to_string(),
            });
        }
        let spawn = ActionDef::for_kind(ActionKind::SpawnAgent);
        // Chords already claimed by an earlier agent row's BUILT-IN
        // default. Two agents sharing a default key (`cursor` and
        // `cursor-agent` both want `u`) would otherwise resolve
        // ambiguously; the second keeps its row (in help, remappable)
        // but loses the colliding default binding. An explicit
        // override is always honored — the user asked for it.
        let mut claimed_defaults: Vec<Chord> = Vec::new();
        for id in agents {
            let config_key = format!("spawn_agent.{id}");
            // Override wins when it has at least one parseable
            // alternative; otherwise the built-in single-letter default
            // (which may be empty for an agent with no convention).
            let (mut chords, mut keys_display): (Vec<Chord>, std::borrow::Cow<'static, str>) =
                match overrides.get(&config_key) {
                    Some(raw) => {
                        let parsed: Vec<Chord> = raw.split('|').filter_map(Chord::parse).collect();
                        if parsed.is_empty() {
                            default_agent_chords(id)
                        } else {
                            (parsed, std::borrow::Cow::Owned(raw.clone()))
                        }
                    }
                    None => default_agent_chords(id),
                };
            // Drop a built-in default chord already taken by an earlier
            // agent (only when it's the default, never an override).
            if !overrides.contains_key(&config_key)
                && chords.iter().any(|c| claimed_defaults.contains(c))
            {
                chords.clear();
                keys_display = std::borrow::Cow::Borrowed("");
            } else {
                claimed_defaults.extend(chords.iter().cloned());
            }
            out.push(CatalogEntry {
                kind: ActionKind::SpawnAgent,
                param: Some(Param::Agent(id.clone())),
                section: spawn.section,
                label: std::borrow::Cow::Owned(format!("spawn {id}")),
                describe: spawn.describe,
                chords,
                keys_display,
                config_key,
            });
        }
        // Scoped "work on this" rows: `<work-leader> <agent-key>` (e.g.
        // `w c` / `w x` / `w u`). The leader is the first stroke of
        // Work's two-stroke binding (honoring a remap), and the second
        // key is the agent's own shortcut. A single-stroke Work override
        // intentionally has no scoped family: one stroke cannot both fire
        // immediately and wait as a prefix without reintroducing the old
        // ambiguity timer.
        let work = ActionDef::for_kind(ActionKind::WorkWith);
        let work_leader: Option<KeyStroke> = ActionDef::for_kind(ActionKind::Work)
            .effective_chords(overrides)
            .into_iter()
            .find_map(|c| match c {
                Chord::Seq(keys) => keys.first().copied(),
                Chord::Key(_) => None,
            });
        if let Some(leader) = work_leader {
            for id in agents {
                let Some(key) = agent_default_key(id) else {
                    continue;
                };
                let second = KeyStroke::new(false, false, false, ChordCode::Char(key));
                let seq = Chord::Seq(vec![leader, second]);
                let keys_display =
                    std::borrow::Cow::Owned(format!("{} {}", leader.display(), second.display()));
                let config_key = format!("work_with.{id}");
                let (chords, keys_display) =
                    generated_row_chords(overrides, &config_key, (vec![seq], keys_display));
                out.push(CatalogEntry {
                    kind: ActionKind::WorkWith,
                    param: Some(Param::Agent(id.clone())),
                    section: work.section,
                    label: std::borrow::Cow::Owned(format!("work in {id}")),
                    describe: work.describe,
                    chords,
                    keys_display,
                    config_key,
                });
            }
        }
        // Scoped "spawn on main" agent rows: `<main-leader> <agent-key>`
        // (e.g. `b c` / `b x` / `b u`). The leader is the first key of
        // whatever `spawn_shell_on_main` resolves to (its default `b s`,
        // honoring a remap) so the whole main-checkout family moves
        // together, and the second key is the agent's own default
        // shortcut. Generated only for agents with a default key; the
        // shell-on-main row (`b s`) is the static counterpart.
        let on_main = ActionDef::for_kind(ActionKind::SpawnAgentOnMain);
        let main_leader: Option<KeyStroke> = ActionDef::for_kind(ActionKind::SpawnShellOnMain)
            .effective_chords(overrides)
            .into_iter()
            .find_map(|c| match c {
                Chord::Seq(keys) => keys.first().copied(),
                Chord::Key(k) => Some(k),
            });
        if let Some(leader) = main_leader {
            for id in agents {
                let Some(key) = agent_default_key(id) else {
                    continue;
                };
                let second = KeyStroke::new(false, false, false, ChordCode::Char(key));
                let seq = Chord::Seq(vec![leader, second]);
                let keys_display =
                    std::borrow::Cow::Owned(format!("{} {}", leader.display(), second.display()));
                let config_key = format!("spawn_agent_on_main.{id}");
                let (chords, keys_display) =
                    generated_row_chords(overrides, &config_key, (vec![seq], keys_display));
                out.push(CatalogEntry {
                    kind: ActionKind::SpawnAgentOnMain,
                    param: Some(Param::Agent(id.clone())),
                    section: on_main.section,
                    label: std::borrow::Cow::Owned(format!("{id} on main")),
                    describe: on_main.describe,
                    chords,
                    keys_display,
                    config_key,
                });
            }
        }
        // Model-tier chords: `<work-leader> <tier-key>` (`w S`) and
        // `a <tier-key>` (`a S`). The tier alias is agent-agnostic — the
        // daemon resolves it against whichever agent the spawn targets —
        // so one row per tier serves both leaders. Rows are dropped for
        // an alias that can't form a chord (multi-char) so the tier
        // still configures a model without claiming a key.
        let spawn_leader = KeyStroke::new(false, false, false, ChordCode::Char('a'));
        for tier in tiers {
            let Some(stroke) = tier_chord_stroke(&tier.alias) else {
                continue;
            };
            if let Some(leader) = work_leader {
                let seq = Chord::Seq(vec![leader, stroke]);
                let config_key = format!("work_tier.{}", tier.alias);
                let default_display =
                    std::borrow::Cow::Owned(format!("{} {}", leader.display(), stroke.display()));
                let (chords, keys_display) =
                    generated_row_chords(overrides, &config_key, (vec![seq], default_display));
                out.push(CatalogEntry {
                    kind: ActionKind::WorkWith,
                    param: Some(Param::Tier(tier.alias.clone())),
                    section: work.section,
                    label: std::borrow::Cow::Owned(tier.label.clone()),
                    describe: work.describe,
                    chords,
                    keys_display,
                    config_key,
                });
            }
            let seq = Chord::Seq(vec![spawn_leader, stroke]);
            let config_key = format!("spawn_tier.{}", tier.alias);
            let default_display =
                std::borrow::Cow::Owned(format!("{} {}", spawn_leader.display(), stroke.display()));
            let (chords, keys_display) =
                generated_row_chords(overrides, &config_key, (vec![seq], default_display));
            out.push(CatalogEntry {
                kind: ActionKind::SpawnAgent,
                param: Some(Param::Tier(tier.alias.clone())),
                section: spawn.section,
                label: std::borrow::Cow::Owned(tier.label.clone()),
                describe: spawn.describe,
                chords,
                keys_display,
                config_key,
            });
        }
        out
    }
}

/// Resolve a generated catalog row's effective chords: a parseable
/// `ui.action_keys` override for `config_key` wins; otherwise the
/// built-in `default` pair. Same fallback semantics as the static
/// rows' [`ActionDef::effective_chords`] — an override whose
/// alternatives all fail to parse falls back to the default rather
/// than unbinding the row (a YAML typo shouldn't break the keyboard).
/// Shared by the `work_with.<id>`, `spawn_agent_on_main.<id>`,
/// `work_tier.<alias>` and `spawn_tier.<alias>` builders, which used
/// to set `config_key` without ever consulting the override map.
fn generated_row_chords(
    overrides: &std::collections::BTreeMap<String, String>,
    config_key: &str,
    default: (Vec<Chord>, std::borrow::Cow<'static, str>),
) -> (Vec<Chord>, std::borrow::Cow<'static, str>) {
    match overrides.get(config_key) {
        Some(raw) => {
            let parsed: Vec<Chord> = raw.split('|').filter_map(Chord::parse).collect();
            if parsed.is_empty() {
                default
            } else {
                (parsed, std::borrow::Cow::Owned(raw.clone()))
            }
        }
        None => default,
    }
}

/// Default chord(s) + display for an agent row from the built-in key
/// convention: the `a` agent leader then the agent's own key (`a c` /
/// `a x` / `a u`) — one leader group instead of scattered top-level
/// keys (#304). An agent lazybox has no convention for gets an empty
/// chord list (no default binding) and a blank display.
fn default_agent_chords(id: &str) -> (Vec<Chord>, std::borrow::Cow<'static, str>) {
    match agent_default_key(id) {
        Some(c) => (
            vec![Chord::Seq(vec![
                KeyStroke::new(false, false, false, ChordCode::Char('a')),
                KeyStroke::new(false, false, false, ChordCode::Char(c)),
            ])],
            std::borrow::Cow::Owned(format!("a {c}")),
        ),
        None => (Vec::new(), std::borrow::Cow::Borrowed("")),
    }
}

/// State-aware label for the footer / context menu, defaulting to
/// the catalog's static `label` when no override applies. The
/// override exists because a handful of actions want a workspace-
/// dependent verb in the footer — e.g. `Work` says "fix CI" when CI
/// failed vs "implement issue" for an open issue (the `classify_work`
/// resolver already knows this), and `Archive` reads as "archive
/// (kills sessions)" when there are running sessions. Centralized
/// here so every surface (footer, menu, future remap UI) reads the
/// same label.
pub fn contextual_label(
    action: &Action,
    workspace: Option<&lazybox_core::Workspace>,
) -> &'static str {
    use crate::intent;
    let default = ActionDef::for_action(action).label;
    match action {
        Action::Work => intent::classify_work(workspace, &[])
            .map(|p| p.label())
            .unwrap_or(default),
        Action::Archive => {
            if workspace.is_some_and(|w| !w.sessions.is_empty()) {
                "archive (kills sessions)"
            } else {
                default
            }
        }
        // Name the resolution the keypress would actually take so the
        // which-key popup / footer don't advertise an ambiguous verb.
        Action::DeleteOrClose => match workspace {
            Some(w) if w.pr.is_some() => "close PR",
            Some(_) => "delete issue",
            None => default,
        },
        _ => default,
    }
}

/// Workspace-scoped availability lookup for an `ActionKind`.
///
/// Defers to the existing `intent::*` resolvers for the actions
/// that already have one — i.e. we don't reinvent the merge-ready
/// or work-classifier predicates; the catalog reuses them. Actions
/// without a resolver (Refresh, OpenHelp, OpenSettings, Quit, …)
/// return `true` unconditionally — they're always usable when the
/// pane that owns them has focus.
///
/// Returns `false` when `workspace` is `None` and the action needs
/// one (most Workspace-section actions). Use the section info to
/// avoid passing `None` to actions that can't sensibly act on it.
pub fn availability(kind: ActionKind, workspace: Option<&lazybox_core::Workspace>) -> bool {
    use crate::intent;
    let has_ws = workspace.is_some();
    match kind {
        // Workspace actions that DO have a resolver — reuse it so
        // the catalog and the keyboard path never disagree on
        // whether a thing is doable.
        ActionKind::MergePr => matches!(
            intent::resolve_merge(workspace),
            intent::Intent::MergePr { .. },
        ),
        // Arming applies to a PR (armed or not — it toggles). Gate on
        // the workspace having a PR so `g g` only surfaces where it
        // can do something; the resolver Notices on non-PR workspaces
        // if the user presses it anyway.
        ActionKind::ToggleAutoMerge => workspace.map(|w| w.pr.is_some()).unwrap_or(false),
        // The policies menu surfaces on any workspace carrying a PR or a
        // GitHub issue — the "tag this PR/issue" surface (issue #363).
        // The menu itself marks which policies apply to PRs vs issues.
        ActionKind::ManagePolicies => workspace
            .map(|w| w.pr.is_some() || !w.gh_issues.is_empty())
            .unwrap_or(false),
        // Targeted re-poll only has something to fetch when the
        // workspace owns a GitHub entity — a PR or a linked issue.
        ActionKind::SyncWorkspace => workspace
            .map(|w| w.pr.is_some() || !w.gh_issues.is_empty())
            .unwrap_or(false),
        ActionKind::Work | ActionKind::WorkWith => intent::classify_work(workspace, &[]).is_some(),
        ActionKind::OpenEditor => matches!(
            intent::resolve_open_editor(workspace),
            intent::Intent::OpenEditor,
        ),
        ActionKind::AdoptSessions => matches!(
            intent::resolve_adopt(workspace),
            intent::Intent::MountAdoptPicker { .. },
        ),
        // Only surfaces on ISSUE workspaces — folding a PR into
        // itself makes no sense. We can't tell here whether the
        // local state actually knows a claiming PR (that requires
        // the cross-workspace lookup), so the dispatcher does the
        // second-stage gate + surfaces a "no claiming PR known"
        // footer when the user presses it on an orphan issue.
        ActionKind::CollapseIntoPr => workspace
            .map(|w| w.pr.is_none() && (!w.gh_issues.is_empty() || !w.linear_issues.is_empty()))
            .unwrap_or(false),
        ActionKind::Archive => matches!(
            intent::resolve_kill(workspace),
            intent::Intent::KillWorkspace { .. },
        ),
        // Only on GitHub issue-only workspaces whose issue is still
        // open — a workspace with a PR acts on the PR, an
        // already-closed issue has nothing to close, and Linear closes
        // aren't wired through the provider yet, so gate on a still-open
        // github issue.
        ActionKind::CloseIssue => workspace
            .map(|w| {
                w.pr.is_none()
                    && w.gh_issues
                        .first()
                        .is_some_and(|i| i.state != lazybox_core::TaskState::Closed)
            })
            .unwrap_or(false),
        // Resolves by workspace kind: close the PR while it's still
        // open (a merged/closed PR has nothing left to close), else
        // delete the still-open GitHub issue. GitHub-only for now,
        // like CloseIssue.
        ActionKind::DeleteOrClose => workspace
            .map(|w| match w.pr.as_ref() {
                Some(pr) => matches!(
                    pr.state,
                    lazybox_core::TaskState::Open
                        | lazybox_core::TaskState::InProgress
                        | lazybox_core::TaskState::InReview
                        | lazybox_core::TaskState::Draft
                ),
                None => w
                    .gh_issues
                    .first()
                    .is_some_and(|i| i.state != lazybox_core::TaskState::Closed),
            })
            .unwrap_or(false),
        ActionKind::SpawnShell => matches!(
            intent::resolve_spawn_shell(workspace),
            intent::Intent::SpawnShell { .. },
        ),
        ActionKind::Reply => matches!(
            intent::resolve_reply(workspace),
            intent::Intent::MountReply { .. },
        ),
        // "On main" only makes sense when the workspace resolves to a
        // repo/project scope — that's what gives a shared main checkout
        // to sit on. A repo-less/standalone workspace has no "main", so
        // the `b …` chords don't surface there.
        ActionKind::SpawnAgentOnMain | ActionKind::SpawnShellOnMain => workspace
            .map(|w| w.worktree_scope().is_some())
            .unwrap_or(false),
        // Workspace actions without a resolver yet — gate purely on
        // the workspace's existence. These all need a target.
        ActionKind::OpenWorkspace
        | ActionKind::SpawnAgent
        | ActionKind::MarkAllRead
        | ActionKind::ToggleSnooze
        | ActionKind::LongSnooze
        | ActionKind::RequestReviewers
        | ActionKind::AddAssignees
        | ActionKind::ManageLabels
        | ActionKind::OpenInBrowser
        // Notes attach to any workspace — even a session-less/empty
        // one — so gate purely on a workspace being under the cursor.
        | ActionKind::EditNotes => has_ws,
        // Needs a source workspace; the dispatcher checks it actually
        // carries a running agent terminal and nudges when it doesn't
        // (the catalog can't see live terminals).
        ActionKind::SendToSession => has_ws,
        // Activity actions need a workspace AND that workspace
        // having some activity to act on. The pane that owns this
        // section already enforces "has activity"; the catalog
        // returns `has_ws` for the looser check + lets the surface
        // tighten further if it wants.
        ActionKind::ToggleActivity
        | ActionKind::ToggleRow
        | ActionKind::ActivityTop
        | ActionKind::ActivityBottom
        | ActionKind::SelectRow
        | ActionKind::ToggleDescription
        | ActionKind::UndoMarkRead => has_ws,
        // Sidebar list-management actions act on the list / view,
        // not a selected workspace — always usable while the sidebar
        // has focus (which `section_rank` already gates). Repo-group
        // collapse walks back to the nearest header, so it's usable
        // from any row (the resolver no-ops on an empty list).
        ActionKind::OpenFilterMenu
        | ActionKind::CycleSort
        | ActionKind::CycleMailbox
        | ActionKind::OpenSearch
        | ActionKind::ToggleRepoGroup => true,
        // Toggling a selection mark needs a row under the cursor; the
        // broadcast itself acts on the selection set (which the catalog
        // can't see), so the dispatcher gates on it and surfaces a
        // footer nudge when nothing is selected.
        ActionKind::SelectWorkspace => has_ws,
        ActionKind::BroadcastToSelected => true,
        // Global / no-workspace-needed actions.
        ActionKind::NewWorkspace
        | ActionKind::NewProject
        | ActionKind::ImportCheckout
        | ActionKind::StartAgent
        | ActionKind::CyclePane
        | ActionKind::ToggleMouseCapture
        | ActionKind::Refresh
        | ActionKind::ForceRedraw
        | ActionKind::OpenHelp
        | ActionKind::OpenTour
        | ActionKind::OpenSyncStatus
        | ActionKind::OpenMessages
        | ActionKind::DismissNotice
        | ActionKind::OpenSettings
        | ActionKind::OpenThemePicker
        | ActionKind::OpenSnippets
        | ActionKind::JumpToWorkspace
        | ActionKind::JumpToAsking
        | ActionKind::JumpToFailingCi
        | ActionKind::ToggleActivityPane
        | ActionKind::ToggleFocusMode
        | ActionKind::Quit
        | ActionKind::ResizeSplitter
        | ActionKind::TerminalScroll
        | ActionKind::LeaveTerminal => true,
    }
}

/// The orientation shortcuts lazybox advertises as universally
/// reachable — help, tour, settings, refresh, cycle-pane, quit. The
/// splash card, the footer's globals tail, and the `?` help all read
/// this one list so "always available" can't quietly mean different
/// things on different surfaces (issue #114). Order is advertise
/// order, with `quit` last — it's the most important escape hatch, so
/// it should survive footer truncation on a narrow line.
///
/// "Universal" holds from the sidebar / activity panes. From the
/// terminal pane the PTY eats every key, so each of these needs the
/// `]]q` leave chord first — see [`ActionDef::available_in_terminal`].
pub fn universal_shortcuts() -> Vec<&'static ActionDef> {
    [
        ActionKind::OpenHelp,
        ActionKind::OpenTour,
        ActionKind::OpenSettings,
        ActionKind::Refresh,
        ActionKind::CyclePane,
        ActionKind::Quit,
    ]
    .into_iter()
    .map(ActionDef::for_kind)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_has_a_def() {
        // DISPLAY_ORDER's fixed length makes omissions a compile error;
        // this uniqueness check prevents a duplicate from masking one.
        let kinds: Vec<ActionKind> = ActionDef::all().map(|def| def.kind).collect();
        assert_eq!(kinds.len(), ActionKind::COUNT);
        for (index, kind) in kinds.iter().enumerate() {
            assert!(
                !kinds[..index].contains(kind),
                "{kind:?} appears more than once in ActionKind::DISPLAY_ORDER"
            );
        }

        // `for_kind` is an exhaustive match, so the compiler guards the
        // definition side. Labels must also stay renderable.
        //
        // `default_keys` is allowed to be empty for actions that
        // exist in the catalog but aren't bound to a key (still
        // reachable via menus / palette). `label` must always be
        // present — every catalog row renders somewhere.
        for def in ActionDef::all() {
            assert!(!def.label.is_empty(), "{:?} missing label", def.kind);
        }
    }

    #[test]
    fn action_to_def_round_trip() {
        // Build a runtime `Action`, look up the def, confirm the
        // kind round-trips. Catches a future mismatch between the
        // `Action::kind()` arm and the `for_kind` table.
        let a = Action::SpawnAgent("claude".into());
        let def = ActionDef::for_action(&a);
        assert_eq!(def.kind, ActionKind::SpawnAgent);
    }

    #[test]
    fn destructive_actions_have_prompts() {
        // Catalog invariant: every action flagged destructive must
        // have a confirm-modal prompt declared. Forgetting the
        // prompt would let the dispatcher fire a destructive
        // action with no Confirm body to render — UB-ish for UX.
        for def in ActionDef::all() {
            if def.is_destructive() {
                assert!(
                    def.confirm_prompt().is_some(),
                    "{:?} is destructive but has no confirm_prompt",
                    def.kind,
                );
            }
        }
    }

    #[test]
    fn confirm_defaults_match_intent() {
        // Issue #312: each destructive catalog action declares its own
        // Enter default next to its prompt. Destructive / hard-to-undo
        // actions default No (a reflexive Enter backs out); the on-main
        // awareness gates default Yes (explicitly-requested, benign at
        // confirm time). This locks the per-action choice so a future
        // edit can't silently regress it back to a blanket No.
        let expect = |kind: ActionKind, yes: bool| {
            assert_eq!(
                ActionDef::for_kind(kind).confirm_default_yes(),
                Some(yes),
                "{kind:?} default"
            );
        };
        expect(ActionKind::Archive, false);
        expect(ActionKind::CloseIssue, false);
        expect(ActionKind::MergePr, false);
        expect(ActionKind::LongSnooze, false);
        expect(ActionKind::SpawnAgentOnMain, true);
        expect(ActionKind::SpawnShellOnMain, true);

        // Non-confirmed actions carry no default.
        assert_eq!(
            ActionDef::for_kind(ActionKind::Work).confirm_default_yes(),
            None
        );
    }

    #[test]
    fn nondestructive_actions_have_no_prompt() {
        // The inverse — if `confirm_prompt` returns Some for a
        // non-destructive action, the dispatcher would route it
        // through the modal path needlessly.
        for def in ActionDef::all() {
            if !def.is_destructive() {
                assert!(
                    def.confirm_prompt().is_none(),
                    "{:?} isn't destructive but has a prompt",
                    def.kind,
                );
            }
        }
    }

    #[test]
    fn effective_chord_uses_override_when_present() {
        use std::collections::BTreeMap;
        let mut overrides = BTreeMap::new();
        // Lowercase 'm' so the parser doesn't auto-shift on the
        // uppercase letter convention. Uppercase Ctrl-M would parse
        // to Ctrl+Shift+m which is also valid but tests the
        // override mechanism, not the auto-shift rule.
        overrides.insert("merge_pr".into(), "Ctrl-m".into());
        let def = ActionDef::for_kind(ActionKind::MergePr);
        let chord = def.effective_chord(&overrides).unwrap();
        assert_eq!(
            chord,
            Chord::Key(KeyStroke::new(true, false, false, ChordCode::Char('m'))),
        );
    }

    #[test]
    fn effective_chord_falls_back_when_override_unparseable() {
        // Typo in YAML shouldn't break the keyboard — return the
        // default chord instead.
        use std::collections::BTreeMap;
        let mut overrides = BTreeMap::new();
        overrides.insert("merge_pr".into(), "garbage-key-spec".into());
        let def = ActionDef::for_kind(ActionKind::MergePr);
        let chord = def.effective_chord(&overrides).unwrap();
        assert_eq!(chord, def.default_chord().unwrap());
    }

    #[test]
    fn effective_chord_falls_back_when_no_override() {
        use std::collections::BTreeMap;
        let overrides = BTreeMap::new();
        let def = ActionDef::for_kind(ActionKind::Refresh);
        assert_eq!(def.effective_chord(&overrides), def.default_chord());
    }

    #[test]
    fn effective_keys_display_returns_default_without_override() {
        // Footer / help should show the catalog default when the user
        // hasn't remapped anything. Borrowed Cow keeps zero-alloc.
        use std::borrow::Cow;
        use std::collections::BTreeMap;
        let overrides = BTreeMap::new();
        let def = ActionDef::for_kind(ActionKind::Refresh);
        assert_eq!(
            def.effective_keys_display(&overrides),
            Cow::Borrowed("Shift-R")
        );
    }

    #[test]
    fn effective_keys_display_returns_override_string() {
        use std::collections::BTreeMap;
        let mut overrides = BTreeMap::new();
        overrides.insert("refresh".into(), "Hyper-Q".into());
        let def = ActionDef::for_kind(ActionKind::Refresh);
        // An unparseable override falls back to the default — typo guard.
        assert_eq!(def.effective_keys_display(&overrides), "Shift-R");

        // A parseable override surfaces — including function keys, now
        // that the parser models `F1`..`F12`.
        overrides.insert("refresh".into(), "F5".into());
        assert_eq!(def.effective_keys_display(&overrides), "F5");

        overrides.insert("refresh".into(), "Ctrl-r".into());
        assert_eq!(def.effective_keys_display(&overrides), "Ctrl-r");
    }

    #[test]
    fn key_stroke_parses_simple_letter() {
        let c = Chord::parse("s").unwrap();
        assert_eq!(
            c,
            Chord::Key(KeyStroke::new(false, false, false, ChordCode::Char('s'))),
        );
    }

    #[test]
    fn key_stroke_parses_uppercase_as_shift() {
        // `Shift-M` and `M` should yield the same chord; the
        // catalog uses either form interchangeably.
        let explicit = Chord::parse("Shift-M").unwrap();
        let implicit = Chord::parse("M").unwrap();
        assert_eq!(explicit, implicit);
        assert_eq!(
            explicit,
            Chord::Key(KeyStroke::new(false, true, false, ChordCode::Char('m'))),
        );
    }

    #[test]
    fn key_stroke_parses_modifier_stack() {
        let c = Chord::parse("Ctrl-Shift-D").unwrap();
        assert_eq!(
            c,
            Chord::Key(KeyStroke::new(true, true, false, ChordCode::Char('d'))),
        );
    }

    #[test]
    fn key_stroke_parses_named_keys() {
        for (s, expected) in [
            ("Tab", ChordCode::Named(NamedKey::Tab)),
            ("Enter", ChordCode::Named(NamedKey::Enter)),
            ("PgUp", ChordCode::Named(NamedKey::PageUp)),
            ("PgDn", ChordCode::Named(NamedKey::PageDown)),
            // Space unifies to the printable form crossterm reports.
            ("Space", ChordCode::Char(' ')),
        ] {
            match Chord::parse(s).unwrap() {
                Chord::Key(k) => assert_eq!(k.code, expected),
                other => panic!("{s} should parse to a Key chord, got {other:?}"),
            }
        }
    }

    #[test]
    fn chord_parses_sequence() {
        // `q q`, `g m`, `] ]` all parse to a Seq of their keystrokes.
        let c = Chord::parse("q q").unwrap();
        assert_eq!(
            c,
            Chord::Seq(vec![
                KeyStroke::new(false, false, false, ChordCode::Char('q')),
                KeyStroke::new(false, false, false, ChordCode::Char('q')),
            ]),
        );
        let g = Chord::parse("g m").unwrap();
        assert_eq!(
            g,
            Chord::Seq(vec![
                KeyStroke::new(false, false, false, ChordCode::Char('g')),
                KeyStroke::new(false, false, false, ChordCode::Char('m')),
            ]),
        );
        assert_eq!(g.head().code, ChordCode::Char('g'));
        assert_eq!(g.len(), 2);
    }

    #[test]
    fn github_defaults_are_leader_only() {
        // The default keymap is pure two-level (#304): every github
        // action binds only its `g …` leader chord — the legacy
        // `Shift-{V,G,L,O}` direct aliases are gone. Pin the negative
        // so a re-added alias fails the build.
        for kind in [
            ActionKind::RequestReviewers,
            ActionKind::AddAssignees,
            ActionKind::ManageLabels,
            ActionKind::OpenInBrowser,
        ] {
            let chords = ActionDef::for_kind(kind).default_chords();
            assert_eq!(chords.len(), 1, "{kind:?} must bind its leader chord only");
            assert!(matches!(chords[0], Chord::Seq(_)));
        }
    }

    #[test]
    fn override_splits_alternatives() {
        // ` | `-separated alternatives still work through
        // `ui.action_keys` — users who want the old direct aliases
        // back re-add them there.
        use std::collections::BTreeMap;
        let mut overrides = BTreeMap::new();
        overrides.insert("request_reviewers".into(), "g r | Shift-V".into());
        let def = ActionDef::for_kind(ActionKind::RequestReviewers);
        let chords = def.effective_chords(&overrides);
        assert_eq!(chords.len(), 2, "override carries a leader + an alias");
        assert!(matches!(chords[0], Chord::Seq(_)));
        assert_eq!(
            chords[1],
            Chord::Key(KeyStroke::new(false, true, false, ChordCode::Char('v'))),
        );
    }

    #[test]
    fn default_merge_is_a_single_g_m_chord() {
        // Merge dropped its legacy `Shift-M` direct alias (#264):
        // the `g m` leader is now its only default chord. Pin the
        // negative so a re-added alias fails the build.
        let chords = ActionDef::for_kind(ActionKind::MergePr).default_chords();
        assert_eq!(chords.len(), 1, "merge no longer has a Shift-M alias");
        assert_eq!(
            chords[0],
            Chord::Seq(vec![
                KeyStroke::new(false, false, false, ChordCode::Char('g')),
                KeyStroke::new(false, false, false, ChordCode::Char('m')),
            ]),
        );
    }

    #[test]
    fn presentation_forms_are_not_parsed() {
        // `g/G` etc. are display-only — we don't try to fabricate
        // a chord. Callers needing the secondary key add an
        // explicit catalog entry.
        assert!(Chord::parse("g/G").is_none());
        assert!(Chord::parse("↑/↓").is_none());
        assert!(Chord::parse("Shift-PgUp/Dn").is_none());
        assert!(Chord::parse("all keys").is_none());
        // Space-separated presentation form with `/` separators — the
        // static SpawnAgent placeholder — must not fabricate a Seq.
        assert!(Chord::parse("c / x / u").is_none());
    }

    #[test]
    fn lone_slash_parses_as_a_chord() {
        // A bare `/` is the literal slash key (sidebar search), not a
        // "this OR that" presentation form — it must parse so the key
        // resolves through the catalog like any other.
        assert_eq!(
            Chord::parse("/").unwrap(),
            Chord::Key(KeyStroke::new(false, false, false, ChordCode::Char('/'))),
        );
    }

    #[test]
    fn no_chord_collisions_within_a_section() {
        // Collision detector (issue #98): within a single section
        // every parseable default chord must be unique. Two actions
        // in the same section sharing a chord is a genuine ambiguity —
        // dispatch (`find_action_for_chord`) breaks the tie by
        // iteration order, so the second action is silently
        // unreachable. Cross-section shadowing (e.g. `z` = snooze in
        // Workspace vs undo-mark-read in Activity) is a DELIBERATE,
        // focus-ranked override and intentionally not flagged here. This is the single audit surface the catalog
        // gained so collisions surface at build time instead of as
        // tribal knowledge in CLAUDE.md.
        use std::collections::HashMap;
        let mut seen: HashMap<(Section, Chord), ActionKind> = HashMap::new();
        for def in ActionDef::all() {
            // Every alternative is a distinct binding — check each.
            for chord in def.default_chords() {
                if let Some(prev) = seen.insert((def.section, chord.clone()), def.kind) {
                    panic!(
                        "chord {:?} bound twice in {:?}: {:?} and {:?}",
                        chord, def.section, prev, def.kind,
                    );
                }
            }
        }
    }

    #[test]
    fn default_keymap_uses_only_named_nontrivial_leader_families() {
        use std::collections::{BTreeMap, BTreeSet};

        let agents = ["claude", "codex", "cursor"].map(str::to_string);
        let catalog = ActionDef::catalog(&agents, &BTreeMap::new());
        let mut groups: BTreeMap<String, (BTreeSet<&'static str>, BTreeSet<String>)> =
            BTreeMap::new();
        for entry in &catalog {
            if matches!(ActionDef::for_kind(entry.kind).guard(), Guard::DoublePress) {
                continue;
            }
            for chord in &entry.chords {
                let Chord::Seq(strokes) = chord else { continue };
                assert_eq!(strokes.len(), 2, "leaders are exactly two strokes");
                let label = leader_group_label(entry.kind).unwrap_or_else(|| {
                    panic!("leader action {:?} has no designed group label", entry.kind)
                });
                let group = groups.entry(strokes[0].display()).or_default();
                group.0.insert(label);
                group.1.insert(strokes[1].display());
            }
        }

        assert_eq!(
            groups.keys().cloned().collect::<Vec<_>>(),
            ["a", "b", "g", "w", "x"],
            "a new leader must be an intentional addition to the keymap grammar",
        );
        for (leader, (labels, continuations)) in groups {
            assert_eq!(labels.len(), 1, "{leader} has conflicting group names");
            assert!(
                continuations.len() >= 2,
                "{leader} should be a direct key, not a one-item menu",
            );
        }
    }

    #[test]
    fn compatibility_aliases_are_exceptional_not_the_default_style() {
        for def in ActionDef::all() {
            let count = def.default_chords().len();
            if count > 1 {
                assert_eq!(
                    def.kind,
                    ActionKind::ToggleMouseCapture,
                    "{:?} has {count} default aliases; grouped actions belong behind a leader",
                    def.kind,
                );
            }
        }
    }

    #[test]
    fn cross_scope_key_reuse_is_small_and_intentional() {
        use std::collections::{BTreeSet, HashMap};
        let mut by_chord: HashMap<Chord, Vec<&ActionDef>> = HashMap::new();
        for def in ActionDef::all() {
            for chord in def.default_chords() {
                by_chord.entry(chord).or_default().push(def);
            }
        }
        let reused: BTreeSet<String> = by_chord
            .into_iter()
            .filter(|(_, defs)| {
                defs.iter()
                    .map(|d| d.section)
                    .collect::<std::collections::HashSet<_>>()
                    .len()
                    > 1
            })
            .map(|(chord, _)| chord_display_for_test(&chord))
            .collect();
        assert_eq!(
            reused,
            ["Enter", "Space", "z"].map(str::to_string).into(),
            "cross-scope reuse needs an explicit design decision",
        );
    }

    fn chord_display_for_test(chord: &Chord) -> String {
        match chord {
            Chord::Key(key) => key.display(),
            Chord::Seq(strokes) => strokes
                .iter()
                .map(KeyStroke::display)
                .collect::<Vec<_>>()
                .join(" "),
        }
    }

    #[test]
    fn confirmed_actions_never_live_on_a_bare_key() {
        for def in ActionDef::all().filter(|def| def.is_destructive()) {
            let chords = def.default_chords();
            if chords.is_empty() {
                assert_eq!(
                    def.kind,
                    ActionKind::SpawnAgentOnMain,
                    "only the generated per-agent main-branch rows use a placeholder",
                );
                continue;
            }
            for chord in chords {
                let Chord::Seq(strokes) = chord else {
                    panic!("confirmed action {:?} is bound to a bare key", def.kind)
                };
                assert!(
                    matches!(strokes[0].display().as_str(), "b" | "g" | "x"),
                    "confirmed action {:?} lives outside a risk-signaling leader",
                    def.kind,
                );
            }
        }
    }

    #[test]
    fn every_parseable_default_round_trips_to_chord() {
        // Smoke: every catalog entry whose default_keys carries at
        // least one parseable alternative must yield a chord. Catches
        // a typo in `default_keys` that would silently break the
        // matcher.
        // Presentation-only `default_keys` — no parseable chord.
        let presentation = [
            "a c / a x / a u",
            "w c / w x / w u",
            "b c / b x / b u",
            "g/G",
            "↑/↓",
            "→/←",
            "Shift-PgUp/Dn",
            "Shift-Arrows",
            "all keys",
            // The terminal `]]q` leave chord is dispatched by the
            // terminal-pane escape-char latch + leader (rendered from the
            // configured `terminal.escape_char`, #170/#252), never by
            // the catalog matcher — so it carries no parseable catalog
            // chord.
            "]]q",
        ];
        for def in ActionDef::all() {
            if presentation.contains(&def.default_keys) {
                continue;
            }
            // Empty `default_keys` is a catalog entry that has no
            // default chord (still reachable via menus / palette,
            // not via a key). No current entries use this — kept
            // for future "menu-only" actions.
            if def.default_keys.is_empty() {
                continue;
            }
            assert!(
                !def.default_chords().is_empty(),
                "{:?} default_keys `{}` failed to parse",
                def.kind,
                def.default_keys,
            );
        }
    }

    #[test]
    fn jump_to_workspace_is_a_global_backtick_chord() {
        // The new general jump is a no-workspace-needed global, so it
        // fires from the sidebar / activity panes and resolves with no
        // selection. Its default chord is the (vim-jump-mnemonic)
        // backtick.
        let def = ActionDef::for_kind(ActionKind::JumpToWorkspace);
        assert_eq!(def.section, Section::Global);
        assert!(availability(ActionKind::JumpToWorkspace, None));
        assert_eq!(
            def.default_chord(),
            Some(Chord::Key(KeyStroke::new(
                false,
                false,
                false,
                ChordCode::Char('`'),
            ))),
        );
    }

    #[test]
    fn jump_actions_are_grouped_in_the_catalog_order() {
        // Help renders `all()` in order; the three Jump actions must be
        // contiguous so the panel reads them as one coherent group.
        let order: Vec<ActionKind> = ActionDef::all().map(|d| d.kind).collect();
        let pos = |k: ActionKind| order.iter().position(|x| *x == k).unwrap();
        let ws = pos(ActionKind::JumpToWorkspace);
        assert_eq!(pos(ActionKind::JumpToAsking), ws + 1);
        assert_eq!(pos(ActionKind::JumpToFailingCi), ws + 2);
    }

    #[test]
    fn close_issue_is_a_confirmed_workspace_action() {
        // Issue #270: closing an issue is Confirm-guarded (nothing
        // deleted without an explicit yes) and lives in the Workspace
        // section next to Archive.
        let def = ActionDef::for_kind(ActionKind::CloseIssue);
        assert_eq!(def.section, Section::Workspace);
        assert!(def.is_destructive(), "close must route through Confirm");
        assert!(def.confirm_prompt().is_some());
        assert_eq!(
            def.default_chord(),
            Some(Chord::Seq(vec![
                KeyStroke::new(false, false, false, ChordCode::Char('x')),
                KeyStroke::new(false, false, false, ChordCode::Char('c')),
            ])),
            "close-issue lives in the workspace-management menu",
        );
    }

    #[test]
    fn delete_or_close_is_a_confirmed_github_leader_action() {
        // Issue #408: `g d` deletes an issue / closes a PR — always
        // behind a Confirm modal, advertised as part of the github
        // leader group.
        let def = ActionDef::for_kind(ActionKind::DeleteOrClose);
        assert_eq!(def.section, Section::Workspace);
        assert!(def.is_destructive(), "delete must route through Confirm");
        assert!(def.confirm_prompt().is_some());
        assert_eq!(def.confirm_default_yes(), Some(false));
        assert_eq!(
            leader_group_label(ActionKind::DeleteOrClose),
            Some("github")
        );
        assert_eq!(
            def.default_chord(),
            Some(Chord::Seq(vec![
                KeyStroke::new(false, false, false, ChordCode::Char('g')),
                KeyStroke::new(false, false, false, ChordCode::Char('d')),
            ])),
            "delete/close lives under the github leader",
        );
    }

    #[test]
    fn delete_or_close_resolves_by_workspace_kind() {
        use chrono::Utc;
        use lazybox_core::{
            CiStatus, ReviewStatus, Task, TaskId, TaskRole, TaskState, Workspace, WorkspaceKey,
        };
        let task = |key: &str| Task {
            id: TaskId {
                source: "github".into(),
                key: key.into(),
            },
            title: key.into(),
            body: None,
            state: TaskState::Open,
            role: TaskRole::Author,
            ci: CiStatus::None,
            review: ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: String::new(),
            repo: Some("acme/widget".into()),
            branch: None,
            base_branch: None,
            updated_at: Utc::now(),
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Mergeable,
            is_behind_base: false,
            node_id: Some("node".into()),
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            closes_issues: vec![],
        };

        // No workspace → not offered.
        assert!(!availability(ActionKind::DeleteOrClose, None));

        // An open github issue workspace offers it, labeled as a delete.
        let mut ws = Workspace::empty(
            WorkspaceKey("github-acme-widget-7".into()),
            "main",
            Utc::now(),
        );
        ws.attach_task(task("acme/widget#7"));
        assert!(availability(ActionKind::DeleteOrClose, Some(&ws)));
        assert_eq!(
            contextual_label(&Action::DeleteOrClose, Some(&ws)),
            "delete issue"
        );

        // A closed issue → nothing to delete.
        let mut closed = task("acme/widget#7");
        closed.state = TaskState::Closed;
        ws.attach_task(closed);
        assert!(!availability(ActionKind::DeleteOrClose, Some(&ws)));

        // An open PR resolves to a PR close, whatever the issues say.
        ws.pr = Some(task("acme/widget#8"));
        assert!(availability(ActionKind::DeleteOrClose, Some(&ws)));
        assert_eq!(
            contextual_label(&Action::DeleteOrClose, Some(&ws)),
            "close PR"
        );

        // A merged PR has nothing left to close.
        let mut merged = task("acme/widget#8");
        merged.state = TaskState::Merged;
        ws.pr = Some(merged);
        assert!(!availability(ActionKind::DeleteOrClose, Some(&ws)));
    }

    #[test]
    fn close_issue_only_offered_on_github_issue_workspaces() {
        use chrono::Utc;
        use lazybox_core::{
            CiStatus, ReviewStatus, Task, TaskId, TaskRole, TaskState, Workspace, WorkspaceKey,
        };
        let task = |key: &str| Task {
            id: TaskId {
                source: "github".into(),
                key: key.into(),
            },
            title: key.into(),
            body: None,
            state: TaskState::Open,
            role: TaskRole::Author,
            ci: CiStatus::None,
            review: ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: String::new(),
            repo: Some("acme/widget".into()),
            branch: None,
            base_branch: None,
            updated_at: Utc::now(),
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Mergeable,
            is_behind_base: false,
            node_id: Some("I_node".into()),
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            closes_issues: vec![],
        };

        // No workspace → not offered.
        assert!(!availability(ActionKind::CloseIssue, None));

        // A github issue-only workspace offers close.
        let mut ws = Workspace::empty(
            WorkspaceKey("github-acme-widget-7".into()),
            "main",
            Utc::now(),
        );
        ws.attach_task(task("acme/widget#7"));
        assert!(!ws.gh_issues.is_empty() && ws.pr.is_none());
        assert!(availability(ActionKind::CloseIssue, Some(&ws)));

        // An already-closed issue → nothing to close.
        let mut closed = task("acme/widget#7");
        closed.state = TaskState::Closed;
        ws.attach_task(closed); // upsert-by-id replaces the open #7
        assert!(!availability(ActionKind::CloseIssue, Some(&ws)));
        ws.attach_task(task("acme/widget#7")); // re-open for the PR case

        // A PR present → act on the PR instead, not the issue.
        ws.pr = Some(task("acme/widget#8"));
        assert!(!availability(ActionKind::CloseIssue, Some(&ws)));
    }

    #[test]
    fn availability_without_workspace_blocks_workspace_actions() {
        // Sanity: Workspace-scoped actions can't fire without a
        // target. Global ones still can.
        assert!(!availability(ActionKind::Work, None));
        assert!(!availability(ActionKind::MergePr, None));
        assert!(!availability(ActionKind::SyncWorkspace, None));
        assert!(!availability(ActionKind::Archive, None));
        assert!(availability(ActionKind::Refresh, None));
        assert!(availability(ActionKind::OpenHelp, None));
        assert!(availability(ActionKind::NewWorkspace, None));
    }

    #[test]
    fn github_leader_chords_share_one_prefix() {
        // The github actions migrated off the `ActionGroup` table onto
        // catalog data: each carries a `g <key>` leader sequence as its
        // first alternative. Assert they all share the `g` prefix and
        // their second keystrokes are unique — the property the old
        // `group_in_keys_are_unique` test guarded, now derived from the
        // catalog itself.
        let g = KeyStroke::new(false, false, false, ChordCode::Char('g'));
        let github = [
            ActionKind::MergePr,
            ActionKind::ToggleAutoMerge,
            ActionKind::RequestReviewers,
            ActionKind::AddAssignees,
            ActionKind::ManageLabels,
            ActionKind::SyncWorkspace,
            ActionKind::OpenInBrowser,
            ActionKind::DeleteOrClose,
        ];
        let mut seconds = Vec::new();
        for kind in github {
            let def = ActionDef::for_kind(kind);
            let seq = def
                .default_chords()
                .into_iter()
                .find_map(|c| match c {
                    Chord::Seq(strokes) if strokes.first() == Some(&g) => Some(strokes),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("{kind:?} missing a `g` leader sequence"));
            assert_eq!(seq.len(), 2, "{kind:?} leader should be two keystrokes");
            seconds.push(seq[1]);
        }
        let before = seconds.len();
        seconds.sort_by_key(|k| format!("{k:?}"));
        seconds.dedup();
        assert_eq!(before, seconds.len(), "duplicate in-group keys");
    }

    #[test]
    fn available_in_terminal_tracks_real_dispatch_exceptions() {
        // Terminal-section actions survive PTY focus. Mouse capture is
        // the one intentional global exception: it is matched before PTY
        // forwarding specifically so users can regain native selection.
        for def in ActionDef::all() {
            let expected =
                def.section == Section::Terminal || def.kind == ActionKind::ToggleMouseCapture;
            assert_eq!(
                def.available_in_terminal(),
                expected,
                "{:?} availability disagrees with its section",
                def.kind,
            );
        }
    }

    #[test]
    fn universal_shortcuts_do_not_fire_in_terminal_focus() {
        // The lie this issue guards against: the splash / tour / footer
        // present these as "always available," but in a focused
        // terminal the PTY eats them. None may report
        // `available_in_terminal` — the honest contract is "press `]]q`
        // first." The `]]q` leave chord is the one that genuinely works.
        for def in universal_shortcuts() {
            assert!(
                !def.available_in_terminal(),
                "{:?} is advertised as universal but does not fire in a \
                 focused terminal — surfaces would mislead the user",
                def.kind,
            );
        }
        assert!(
            ActionDef::for_kind(ActionKind::LeaveTerminal).available_in_terminal(),
            "the `]]q` leave chord is the gateway back to the globals",
        );
    }

    #[test]
    fn catalog_generates_one_row_per_agent() {
        use std::collections::BTreeMap;
        let agents: Vec<String> = ["claude", "codex", "aider"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let catalog = ActionDef::catalog(&agents, &BTreeMap::new());

        // No generic SpawnAgent placeholder survives — only concrete
        // per-agent rows.
        let spawn_rows: Vec<&CatalogEntry> = catalog
            .iter()
            .filter(|e| e.kind == ActionKind::SpawnAgent)
            .collect();
        assert_eq!(spawn_rows.len(), 3, "one row per agent");

        let claude = spawn_rows
            .iter()
            .find(|e| e.param == Some(Param::Agent("claude".into())))
            .expect("claude row");
        assert_eq!(claude.config_key, "spawn_agent.claude");
        // Built-in convention: claude → `a c` under the agent leader.
        assert_eq!(
            claude.chords,
            vec![Chord::Seq(vec![
                KeyStroke::new(false, false, false, ChordCode::Char('a')),
                KeyStroke::new(false, false, false, ChordCode::Char('c')),
            ])],
        );

        // An agent with no built-in convention gets a row but no chord.
        let aider = spawn_rows
            .iter()
            .find(|e| e.param == Some(Param::Agent("aider".into())))
            .expect("aider row");
        assert!(aider.chords.is_empty(), "aider has no default key");
    }

    #[test]
    fn catalog_generates_scoped_work_chords_per_agent() {
        // Issue #224: `w c` / `w x` / `w u` scoped "work on this" rows.
        use std::collections::BTreeMap;
        let agents: Vec<String> = ["claude", "codex", "cursor", "aider"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let catalog = ActionDef::catalog(&agents, &BTreeMap::new());

        let work_rows: Vec<&CatalogEntry> = catalog
            .iter()
            .filter(|e| e.kind == ActionKind::WorkWith)
            .collect();
        // One row per agent WITH a default key — `aider` has no
        // convention, so it gets no scoped chord (`w w` still reaches
        // the default-agent path).
        assert_eq!(work_rows.len(), 3, "one scoped row per known agent");

        let w = KeyStroke::new(false, false, false, ChordCode::Char('w'));
        let codex = work_rows
            .iter()
            .find(|e| e.param == Some(Param::Agent("codex".into())))
            .expect("codex work row");
        assert_eq!(codex.config_key, "work_with.codex");
        assert_eq!(
            codex.chords,
            vec![Chord::Seq(vec![
                w,
                KeyStroke::new(false, false, false, ChordCode::Char('x')),
            ])],
            "codex scoped work chord is `w x`",
        );
        assert!(
            !work_rows
                .iter()
                .any(|e| e.param == Some(Param::Agent("aider".into()))),
            "an agent with no default key gets no scoped work chord",
        );
    }

    #[test]
    fn tier_chord_stroke_folds_case_and_rejects_multichar() {
        // Uppercase alias → Shift-modified lowercase (`S` = Shift-s).
        assert_eq!(
            tier_chord_stroke("S"),
            Some(KeyStroke::new(false, true, false, ChordCode::Char('s')))
        );
        // Lowercase alias binds verbatim.
        assert_eq!(
            tier_chord_stroke("q"),
            Some(KeyStroke::new(false, false, false, ChordCode::Char('q')))
        );
        // Multi-char and non-alphanumeric aliases get no chord.
        assert_eq!(tier_chord_stroke("XL"), None);
        assert_eq!(tier_chord_stroke(""), None);
    }

    #[test]
    fn catalog_generates_tier_chords_under_work_and_spawn_leaders() {
        use std::collections::BTreeMap;
        let agents = vec!["claude".to_string()];
        let tiers = lazybox_core::AgentModels::builtin("claude").unwrap().tiers;
        let catalog = ActionDef::catalog_with_tiers(&agents, &BTreeMap::new(), &tiers);

        let w = KeyStroke::new(false, false, false, ChordCode::Char('w'));
        let a = KeyStroke::new(false, false, false, ChordCode::Char('a'));
        let shift_s = KeyStroke::new(false, true, false, ChordCode::Char('s'));

        // `w S` → a WorkWith row tagged with the tier alias, labeled by
        // the model name so the which-key popup reads "Haiku".
        let work_tier = catalog
            .iter()
            .find(|e| e.kind == ActionKind::WorkWith && e.param == Some(Param::Tier("S".into())))
            .expect("w S tier row");
        assert_eq!(work_tier.chords, vec![Chord::Seq(vec![w, shift_s])]);
        assert_eq!(work_tier.label, "Haiku");
        assert_eq!(work_tier.config_key, "work_tier.S");

        // `a S` → a SpawnAgent row under the agent leader.
        let spawn_tier = catalog
            .iter()
            .find(|e| e.kind == ActionKind::SpawnAgent && e.param == Some(Param::Tier("S".into())))
            .expect("a S tier row");
        assert_eq!(spawn_tier.chords, vec![Chord::Seq(vec![a, shift_s])]);

        // The tier chords must not collide with the agent chords that
        // share the same leaders (`w c`, `a c`). Every Seq chord in the
        // catalog is unique.
        let mut seqs: Vec<&Chord> = catalog
            .iter()
            .flat_map(|e| e.chords.iter())
            .filter(|c| matches!(c, Chord::Seq(_)))
            .collect();
        let before = seqs.len();
        seqs.sort_by(|x, y| format!("{x:?}").cmp(&format!("{y:?}")));
        seqs.dedup();
        assert_eq!(before, seqs.len(), "no two catalog chords may collide");
    }

    #[test]
    fn catalog_generates_on_main_chords_per_agent_plus_shell() {
        // Issue #271: `b c` / `b x` / `b u` spawn an agent on the shared
        // main checkout; `b s` a shell. All share the `b` leader and are
        // confirm-guarded.
        use std::collections::BTreeMap;
        let agents: Vec<String> = ["claude", "codex", "cursor", "aider"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let catalog = ActionDef::catalog(&agents, &BTreeMap::new());

        let agent_rows: Vec<&CatalogEntry> = catalog
            .iter()
            .filter(|e| e.kind == ActionKind::SpawnAgentOnMain)
            .collect();
        // One row per agent WITH a default key — `aider` has none.
        assert_eq!(agent_rows.len(), 3, "one on-main row per known agent");

        let b = KeyStroke::new(false, false, false, ChordCode::Char('b'));
        let codex = agent_rows
            .iter()
            .find(|e| e.param == Some(Param::Agent("codex".into())))
            .expect("codex on-main row");
        assert_eq!(codex.config_key, "spawn_agent_on_main.codex");
        assert_eq!(
            codex.chords,
            vec![Chord::Seq(vec![
                b,
                KeyStroke::new(false, false, false, ChordCode::Char('x')),
            ])],
            "codex on-main chord is `b x`",
        );

        // The shell-on-main row is a plain static `b s`.
        let shell = catalog
            .iter()
            .find(|e| e.kind == ActionKind::SpawnShellOnMain)
            .expect("shell-on-main row");
        assert_eq!(
            shell.chords,
            vec![Chord::Seq(vec![
                b,
                KeyStroke::new(false, false, false, ChordCode::Char('s')),
            ])],
            "shell-on-main chord is `b s`",
        );

        // Both are confirm-guarded — main is riskier than an isolated
        // worktree.
        assert!(ActionDef::for_kind(ActionKind::SpawnAgentOnMain).is_destructive());
        assert!(ActionDef::for_kind(ActionKind::SpawnShellOnMain).is_destructive());
    }

    #[test]
    fn on_main_leader_follows_a_shell_on_main_remap() {
        // The generated agent-on-main chords take their leader from the
        // shell-on-main binding, so remapping it moves `b c` → `n c`.
        use std::collections::BTreeMap;
        let mut overrides = BTreeMap::new();
        overrides.insert("spawn_shell_on_main".to_string(), "n s".to_string());
        let catalog = ActionDef::catalog(&["claude".to_string()], &overrides);
        let claude = catalog
            .iter()
            .find(|e| e.kind == ActionKind::SpawnAgentOnMain)
            .expect("claude on-main row");
        assert_eq!(
            claude.chords,
            vec![Chord::Seq(vec![
                KeyStroke::new(false, false, false, ChordCode::Char('n')),
                KeyStroke::new(false, false, false, ChordCode::Char('c')),
            ])],
        );
    }

    #[test]
    fn scoped_work_leader_follows_a_work_remap() {
        // The scoped chords' leader tracks the `work` binding, so a
        // remap of `w w` moves `w c` → `g c` too.
        use std::collections::BTreeMap;
        let mut overrides = BTreeMap::new();
        overrides.insert("work".to_string(), "g g".to_string());
        let catalog = ActionDef::catalog(&["claude".to_string()], &overrides);
        let claude = catalog
            .iter()
            .find(|e| e.kind == ActionKind::WorkWith)
            .expect("claude work row");
        assert_eq!(
            claude.chords,
            vec![Chord::Seq(vec![
                KeyStroke::new(false, false, false, ChordCode::Char('g')),
                KeyStroke::new(false, false, false, ChordCode::Char('c')),
            ])],
        );
    }

    #[test]
    fn single_key_work_remap_does_not_create_an_ambiguous_leader() {
        use std::collections::BTreeMap;
        let mut overrides = BTreeMap::new();
        overrides.insert("work".to_string(), "Ctrl-k".to_string());
        let catalog = ActionDef::catalog(&["claude".to_string()], &overrides);
        let work = catalog
            .iter()
            .find(|e| e.kind == ActionKind::Work)
            .expect("work row");
        assert_eq!(
            work.chords,
            vec![Chord::Key(KeyStroke::new(
                true,
                false,
                false,
                ChordCode::Char('k'),
            ))],
        );
        assert!(
            !catalog.iter().any(|e| e.kind == ActionKind::WorkWith),
            "a direct Work key must not also become a delayed leader",
        );
    }

    #[test]
    fn catalog_dedupes_shared_agent_default_key() {
        // `cursor` and `cursor-agent` both default to `a u`; only the
        // first keeps the binding, the second still gets a (remappable)
        // row but no colliding default chord.
        use std::collections::BTreeMap;
        let agents: Vec<String> = ["cursor", "cursor-agent"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let catalog = ActionDef::catalog(&agents, &BTreeMap::new());
        let u = Chord::Seq(vec![
            KeyStroke::new(false, false, false, ChordCode::Char('a')),
            KeyStroke::new(false, false, false, ChordCode::Char('u')),
        ]);
        let bound_to_u: Vec<&CatalogEntry> =
            catalog.iter().filter(|e| e.chords.contains(&u)).collect();
        assert_eq!(bound_to_u.len(), 1, "only one agent keeps `u`");
        // Both agents still have a row.
        assert_eq!(
            catalog
                .iter()
                .filter(|e| e.kind == ActionKind::SpawnAgent)
                .count(),
            2,
        );
    }

    #[test]
    fn catalog_agent_row_honors_override() {
        use std::collections::BTreeMap;
        let mut overrides = BTreeMap::new();
        overrides.insert("spawn_agent.claude".to_string(), "Ctrl-j".to_string());
        let catalog = ActionDef::catalog(&["claude".to_string()], &overrides);
        let claude = catalog
            .iter()
            .find(|e| e.param == Some(Param::Agent("claude".into())))
            .unwrap();
        assert_eq!(
            claude.chords,
            vec![Chord::Key(KeyStroke::new(
                true,
                false,
                false,
                ChordCode::Char('j')
            ))],
        );
    }

    #[test]
    fn every_preset_resolves_and_is_collision_free() {
        // Each shipped preset must (a) resolve, and (b) produce a
        // catalog with no two bindings sharing a chord within a
        // section — the same invariant the default catalog holds, now
        // guarded for presets so a bad in-tree keymap can't ship.
        use std::collections::HashMap;
        let agents: Vec<String> = ["claude", "codex", "cursor"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        for name in KEYMAP_PRESETS {
            let overrides = keymap_preset(name).unwrap_or_else(|| panic!("{name} must resolve"));
            let catalog = ActionDef::catalog(&agents, &overrides);
            let mut seen: HashMap<(Section, Chord), String> = HashMap::new();
            for entry in &catalog {
                for chord in &entry.chords {
                    let id = format!("{:?}/{:?}", entry.kind, entry.param);
                    if let Some(prev) = seen.insert((entry.section, chord.clone()), id.clone()) {
                        panic!(
                            "preset `{name}`: chord {chord:?} bound twice in \
                             {:?}: {prev} and {id}",
                            entry.section,
                        );
                    }
                }
            }
        }
        assert!(keymap_preset("nope").is_none());
    }

    #[test]
    fn vim_preset_is_leaders_primary() {
        // Leaders-primary is the default policy now (#304), so the vim
        // preset inherits `g m` as merge's only chord without carrying
        // its own github overrides — what stays vim-specific is
        // pane-cycling on Ctrl-w.
        let overrides = keymap_preset("vim").unwrap();
        assert!(
            !overrides.contains_key("merge_pr"),
            "vim no longer needs github overrides — the default is leaders-only",
        );
        let catalog = ActionDef::catalog(&[], &overrides);
        let merge = catalog
            .iter()
            .find(|e| e.kind == ActionKind::MergePr)
            .unwrap();
        assert_eq!(
            merge.chords,
            vec![Chord::Seq(vec![
                KeyStroke::new(false, false, false, ChordCode::Char('g')),
                KeyStroke::new(false, false, false, ChordCode::Char('m')),
            ])],
            "vim merge keeps only the g-leader, no Shift-M",
        );
    }

    #[test]
    fn agent_rows_group_under_the_a_leader() {
        // Issue #304: agent spawns are a leader group. Every generated
        // `SpawnAgent` chord starts with the `a` leader, their second
        // keystrokes are unique, and the group carries the "agent"
        // label the footer cell / which-key title / help heading share.
        use std::collections::BTreeMap;
        let agents: Vec<String> = ["claude", "codex", "cursor"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let catalog = ActionDef::catalog(&agents, &BTreeMap::new());
        let a = KeyStroke::new(false, false, false, ChordCode::Char('a'));
        let mut seconds = Vec::new();
        for entry in catalog.iter().filter(|e| e.kind == ActionKind::SpawnAgent) {
            let seq = entry
                .chords
                .iter()
                .find_map(|c| match c {
                    Chord::Seq(strokes) if strokes.first() == Some(&a) => Some(strokes.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("{:?} missing an `a` leader sequence", entry.param));
            assert_eq!(seq.len(), 2);
            seconds.push(seq[1]);
        }
        assert_eq!(seconds.len(), 3, "one `a …` chord per known agent");
        let before = seconds.len();
        seconds.sort_by_key(|k| format!("{k:?}"));
        seconds.dedup();
        assert_eq!(before, seconds.len(), "duplicate in-group keys");
        assert_eq!(leader_group_label(ActionKind::SpawnAgent), Some("agent"));
    }

    #[test]
    fn leader_group_labels_cover_the_grouped_actions() {
        // The registry names every shipped leader group; ungrouped
        // actions stay unlabeled so surfaces don't invent group cells
        // for them.
        assert_eq!(leader_group_label(ActionKind::MergePr), Some("github"));
        assert_eq!(
            leader_group_label(ActionKind::RequestReviewers),
            Some("github"),
        );
        assert_eq!(leader_group_label(ActionKind::WorkWith), Some("work"));
        assert_eq!(
            leader_group_label(ActionKind::SpawnAgentOnMain),
            Some("main branch"),
        );
        assert_eq!(
            leader_group_label(ActionKind::SpawnShellOnMain),
            Some("main branch"),
        );
        assert_eq!(leader_group_label(ActionKind::Work), Some("work"));
        assert_eq!(
            leader_group_label(ActionKind::NewWorkspace),
            Some("workspace"),
        );
        assert_eq!(leader_group_label(ActionKind::Archive), Some("workspace"),);
        assert_eq!(leader_group_label(ActionKind::Quit), None);
    }

    #[test]
    fn workspace_management_actions_share_the_x_leader() {
        let expected = [
            (ActionKind::NewWorkspace, 'n'),
            (ActionKind::NewProject, 'p'),
            (ActionKind::ImportCheckout, 'i'),
            (ActionKind::AdoptSessions, 'a'),
            (ActionKind::SendToSession, 's'),
            (ActionKind::CollapseIntoPr, 'j'),
            (ActionKind::LongSnooze, 'z'),
            (ActionKind::Archive, 'x'),
            (ActionKind::CloseIssue, 'c'),
        ];
        let leader = KeyStroke::new(false, false, false, ChordCode::Char('x'));
        for (kind, key) in expected {
            assert_eq!(
                ActionDef::for_kind(kind).default_chord(),
                Some(Chord::Seq(vec![
                    leader,
                    KeyStroke::new(false, false, false, ChordCode::Char(key)),
                ])),
                "{kind:?} must stay in the workspace menu",
            );
            assert_eq!(leader_group_label(kind), Some("workspace"));
        }
    }

    /// Regression (advertised-but-ignored namespaces): each generated
    /// row family must consult `ui.action_keys` under its documented
    /// config key. Pre-fix only `spawn_agent.<id>` did; the other four
    /// set `config_key` but never read the map.
    #[test]
    fn work_with_override_is_honored() {
        use std::collections::BTreeMap;
        let mut overrides = BTreeMap::new();
        overrides.insert("work_with.claude".to_string(), "Ctrl-k".to_string());
        let catalog = ActionDef::catalog(&["claude".to_string()], &overrides);
        let row = catalog
            .iter()
            .find(|e| {
                e.kind == ActionKind::WorkWith && e.param == Some(Param::Agent("claude".into()))
            })
            .expect("work_with.claude row");
        assert_eq!(
            row.chords,
            vec![Chord::Key(KeyStroke::new(
                true,
                false,
                false,
                ChordCode::Char('k')
            ))],
            "work_with.<id> override must replace the default `w c` chord",
        );
        assert_eq!(row.keys_display, "Ctrl-k");
    }

    #[test]
    fn spawn_agent_on_main_override_is_honored() {
        use std::collections::BTreeMap;
        let mut overrides = BTreeMap::new();
        overrides.insert(
            "spawn_agent_on_main.codex".to_string(),
            "Ctrl-b".to_string(),
        );
        let catalog = ActionDef::catalog(&["codex".to_string()], &overrides);
        let row = catalog
            .iter()
            .find(|e| {
                e.kind == ActionKind::SpawnAgentOnMain
                    && e.param == Some(Param::Agent("codex".into()))
            })
            .expect("spawn_agent_on_main.codex row");
        assert_eq!(
            row.chords,
            vec![Chord::Key(KeyStroke::new(
                true,
                false,
                false,
                ChordCode::Char('b')
            ))],
        );
    }

    #[test]
    fn work_tier_and_spawn_tier_overrides_are_honored() {
        use std::collections::BTreeMap;
        let tiers = vec![lazybox_core::ModelTier {
            alias: "S".to_string(),
            label: "Haiku".to_string(),
            args: vec![],
        }];
        let mut overrides = BTreeMap::new();
        overrides.insert("work_tier.S".to_string(), "Ctrl-1".to_string());
        overrides.insert("spawn_tier.S".to_string(), "Ctrl-2".to_string());
        let catalog = ActionDef::catalog_with_tiers(&["claude".to_string()], &overrides, &tiers);
        let work_tier = catalog
            .iter()
            .find(|e| e.kind == ActionKind::WorkWith && e.param == Some(Param::Tier("S".into())))
            .expect("work_tier.S row");
        assert_eq!(
            work_tier.chords,
            vec![Chord::Key(KeyStroke::new(
                true,
                false,
                false,
                ChordCode::Char('1')
            ))],
        );
        let spawn_tier = catalog
            .iter()
            .find(|e| e.kind == ActionKind::SpawnAgent && e.param == Some(Param::Tier("S".into())))
            .expect("spawn_tier.S row");
        assert_eq!(
            spawn_tier.chords,
            vec![Chord::Key(KeyStroke::new(
                true,
                false,
                false,
                ChordCode::Char('2')
            ))],
        );
    }

    /// An unparseable override on a generated row falls back to the
    /// built-in chord — same typo-guard semantics as the static rows.
    #[test]
    fn generated_row_override_falls_back_when_unparseable() {
        use std::collections::BTreeMap;
        let mut overrides = BTreeMap::new();
        overrides.insert("work_with.claude".to_string(), "garbage-spec".to_string());
        let catalog = ActionDef::catalog(&["claude".to_string()], &overrides);
        let row = catalog
            .iter()
            .find(|e| {
                e.kind == ActionKind::WorkWith && e.param == Some(Param::Agent("claude".into()))
            })
            .expect("work_with.claude row");
        let w = KeyStroke::new(false, false, false, ChordCode::Char('w'));
        let c = KeyStroke::new(false, false, false, ChordCode::Char('c'));
        assert_eq!(row.chords, vec![Chord::Seq(vec![w, c])]);
    }

    /// NewProject and NewWorkspace must not share a footer label —
    /// two identical "new workspace" cells rendered for different
    /// actions (#8 of the keybinding audit).
    #[test]
    fn new_project_label_is_distinct_from_new_workspace() {
        assert_eq!(
            ActionDef::for_kind(ActionKind::NewProject).label,
            "new project"
        );
        assert_eq!(
            ActionDef::for_kind(ActionKind::NewWorkspace).label,
            "new workspace"
        );
    }

    /// The mouse-capture toggle is a catalog row (discoverable in `?`,
    /// remappable) with all three chord alternatives parseable.
    #[test]
    fn mouse_capture_row_parses_all_alternatives() {
        let def = ActionDef::for_kind(ActionKind::ToggleMouseCapture);
        let chords = def.default_chords();
        assert_eq!(
            chords.len(),
            3,
            "F8 / Alt-s / Ctrl-Alt-s all parse: {chords:?}"
        );
        assert_eq!(def.section, Section::Global);
    }

    #[test]
    fn unknown_preset_warning_fires_only_for_unknown_names() {
        assert!(unknown_preset_warning("default").is_none());
        assert!(unknown_preset_warning("vim").is_none());
        let warning = unknown_preset_warning("emacs").expect("unknown preset warns");
        assert!(warning.contains("emacs"), "{warning}");
        assert!(
            warning.contains("vim"),
            "names the known presets: {warning}"
        );
    }

    #[test]
    fn all_is_sorted_by_section() {
        // The `all()` iterator emits Global first, then Workspace,
        // then Sidebar, then Activity, then Terminal. Help relies on
        // this for its section dividers — assert here so a reorder
        // surfaces.
        let order: Vec<Section> = ActionDef::all().map(|d| d.section).collect();
        let mut last_idx = 0;
        let order_of = Section::order;
        for s in order {
            let idx = order_of(s);
            assert!(idx >= last_idx, "section {s:?} appeared out of order");
            last_idx = idx;
        }
    }
}
