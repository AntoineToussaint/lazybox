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
    /// Spawn a specific agent by id (claude / codex / cursor / …).
    /// The id is dynamic because the agent registry is config-driven.
    SpawnAgent(String),
    /// Spawn a shell in the focused workspace's worktree.
    SpawnShell,
    /// Open the workspace's worktree in the user's editor.
    OpenEditor,
    /// Create a brand-new pre-PR workspace (asks for a name).
    NewWorkspace,
    /// Create a brand-new local Project — a top-level container the
    /// sidebar groups workspaces under. Asks for a name. Idempotent
    /// on collision (re-opens the existing local project).
    NewProject,
    /// Mark every activity row on the focused workspace read.
    MarkAllRead,
    /// Toggle snooze on the focused workspace (short snooze, ~4h).
    ToggleSnooze,
    /// Long-snooze the focused workspace (~1 year — effectively "hide").
    /// Confirm-guarded because it has no obvious undo.
    LongSnooze,
    /// Archive the workspace + kill any of its sessions. Destructive.
    Archive,
    /// Merge the workspace's PR if it's in a merge-ready state. Only
    /// surfaces for provider workspaces that have a merge concept
    /// (today: github PRs).
    MergePr,
    /// Move every session from the focused workspace to another.
    AdoptSessions,
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
    /// Open the focused workspace's PR / issue page in the host's
    /// default web browser. Useful for jumping to GitHub when the
    /// in-lazybox UI doesn't carry every affordance yet (mobile-rich
    /// review thread, full diff view, etc.).
    OpenInBrowser,

    // ── Sidebar list management ────────────────────────────────────
    // These act on the sidebar's list/view rather than a single
    // workspace: filtering, sorting, switching mailbox, searching.
    // They only resolve when the sidebar has focus (Section::Sidebar)
    // so they never bleed into the activity pane the way the
    // workspace-scoped actions deliberately do.
    /// Cycle the role filter (All → Author → Reviewer → …).
    CycleRoleFilter,
    /// Cycle the sort order (Default → ByRole → ByRoleSplit).
    CycleSort,
    /// Cycle the mailbox view (Inbox → Inactive → Snoozed).
    CycleMailbox,
    /// Open the incremental search bar scoped to the focused project.
    OpenSearch,

    // ── Activity pane (right) ──────────────────────────────────────
    /// Toggle the activity-section collapse on the focused workspace.
    ToggleActivity,
    /// Toggle a single activity row's expanded view.
    ToggleRow,
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
    /// Open the `?` help modal.
    OpenHelp,
    /// Launch the in-app feature tour / guided walkthrough.
    OpenTour,
    /// Open the debug / sync-status window (Shift+D).
    OpenSyncStatus,
    /// Open the `,` Settings palette.
    OpenSettings,
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
    /// Begin the two-press quit chord. Single-press from a remap
    /// just fires.
    Quit,
    /// Resize the active splitter (Shift+Arrow).
    ResizeSplitter(ResizeDirection),

    // ── Terminal-pane scoped ───────────────────────────────────────
    /// Scroll the focused terminal's scrollback (Shift+PgUp/Dn).
    TerminalScroll(ScrollDirection),
    /// Escape the terminal back to sidebar focus (`]]`).
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
pub enum ActionKind {
    // Workspace
    OpenWorkspace,
    Work,
    SpawnAgent,
    SpawnShell,
    OpenEditor,
    NewWorkspace,
    NewProject,
    MarkAllRead,
    ToggleSnooze,
    LongSnooze,
    Archive,
    MergePr,
    AdoptSessions,
    CollapseIntoPr,
    RequestReviewers,
    AddAssignees,
    ManageLabels,
    OpenInBrowser,
    // Sidebar list management
    CycleRoleFilter,
    CycleSort,
    CycleMailbox,
    OpenSearch,
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
    Refresh,
    OpenHelp,
    OpenTour,
    OpenSyncStatus,
    OpenSettings,
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
            Action::SpawnAgent(_) => ActionKind::SpawnAgent,
            Action::SpawnShell => ActionKind::SpawnShell,
            Action::OpenEditor => ActionKind::OpenEditor,
            Action::NewWorkspace => ActionKind::NewWorkspace,
            Action::NewProject => ActionKind::NewProject,
            Action::MarkAllRead => ActionKind::MarkAllRead,
            Action::ToggleSnooze => ActionKind::ToggleSnooze,
            Action::LongSnooze => ActionKind::LongSnooze,
            Action::Archive => ActionKind::Archive,
            Action::MergePr => ActionKind::MergePr,
            Action::AdoptSessions => ActionKind::AdoptSessions,
            Action::CollapseIntoPr => ActionKind::CollapseIntoPr,
            Action::RequestReviewers => ActionKind::RequestReviewers,
            Action::AddAssignees => ActionKind::AddAssignees,
            Action::ManageLabels => ActionKind::ManageLabels,
            Action::OpenInBrowser => ActionKind::OpenInBrowser,
            Action::CycleRoleFilter => ActionKind::CycleRoleFilter,
            Action::CycleSort => ActionKind::CycleSort,
            Action::CycleMailbox => ActionKind::CycleMailbox,
            Action::OpenSearch => ActionKind::OpenSearch,
            Action::ToggleActivity => ActionKind::ToggleActivity,
            Action::ToggleRow => ActionKind::ToggleRow,
            Action::Reply => ActionKind::Reply,
            Action::SelectRow => ActionKind::SelectRow,
            Action::ToggleDescription => ActionKind::ToggleDescription,
            Action::UndoMarkRead => ActionKind::UndoMarkRead,
            Action::CyclePane => ActionKind::CyclePane,
            Action::Refresh => ActionKind::Refresh,
            Action::OpenHelp => ActionKind::OpenHelp,
            Action::OpenTour => ActionKind::OpenTour,
            Action::OpenSyncStatus => ActionKind::OpenSyncStatus,
            Action::OpenSettings => ActionKind::OpenSettings,
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
            ActionKind::Refresh => &Self {
                kind: ActionKind::Refresh,
                default_keys: "Shift-R",
                label: "refresh",
                describe: "Re-poll every provider for fresh tasks.",
                section: Section::Global,
            },
            ActionKind::OpenHelp => &Self {
                kind: ActionKind::OpenHelp,
                default_keys: "?",
                label: "help",
                describe: "Show this list of shortcuts.",
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
                label: "sync status",
                describe: "Show recent provider-sync outcomes, last poll times, and errors.",
                section: Section::Global,
            },
            ActionKind::OpenSettings => &Self {
                kind: ActionKind::OpenSettings,
                default_keys: ",",
                label: "settings",
                describe: "Open the Settings palette.",
                section: Section::Global,
            },
            ActionKind::JumpToAsking => &Self {
                kind: ActionKind::JumpToAsking,
                default_keys: "!",
                label: "next asking",
                describe: "Jump the sidebar cursor to the next workspace whose agent is waiting on input.",
                section: Section::Global,
            },
            ActionKind::JumpToFailingCi => &Self {
                kind: ActionKind::JumpToFailingCi,
                default_keys: "Shift-F",
                label: "next failing",
                describe: "Jump the sidebar cursor to the next PR whose CI is failing.",
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
                label: "start agent",
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
                default_keys: "w",
                label: "work on this",
                describe: "Spawn the default agent with a contextual work prompt (fix CI, address review, implement issue, …).",
                section: Section::Workspace,
            },
            ActionKind::SpawnAgent => &Self {
                kind: ActionKind::SpawnAgent,
                // Default binding is per-agent; the runtime label
                // (`spawn claude`) carries the id. Listed here so the
                // help panel has a row, with the literal multi-agent
                // form in the keys column.
                default_keys: "c / x / u",
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
            ActionKind::OpenEditor => &Self {
                kind: ActionKind::OpenEditor,
                default_keys: "e",
                label: "editor",
                describe: "Open the worktree in the configured editor.",
                section: Section::Workspace,
            },
            ActionKind::NewWorkspace => &Self {
                kind: ActionKind::NewWorkspace,
                default_keys: "n",
                label: "new workspace",
                describe: "Create a pre-PR workspace (asks for a name).",
                section: Section::Workspace,
            },
            ActionKind::NewProject => &Self {
                kind: ActionKind::NewProject,
                default_keys: "Shift-N",
                label: "new workspace",
                describe: "Pick a tracked repo to start a workspace on, or create a new local project.",
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
                default_keys: "Shift-Z",
                label: "long snooze",
                describe: "Snooze the workspace for ~1 year (effectively hide). Confirmed first.",
                section: Section::Workspace,
            },
            ActionKind::Archive => &Self {
                kind: ActionKind::Archive,
                default_keys: "Shift-X",
                label: "archive",
                describe: "Drop the workspace and kill any sessions. Destructive.",
                section: Section::Workspace,
            },
            ActionKind::MergePr => &Self {
                kind: ActionKind::MergePr,
                default_keys: "g m | Shift-M",
                label: "merge PR",
                describe: "Merge the PR (only when CI green + approved + no conflicts).",
                section: Section::Workspace,
            },
            ActionKind::AdoptSessions => &Self {
                kind: ActionKind::AdoptSessions,
                default_keys: "Shift-A",
                label: "adopt sessions",
                describe: "Move every session from this workspace into another.",
                section: Section::Workspace,
            },
            ActionKind::CollapseIntoPr => &Self {
                kind: ActionKind::CollapseIntoPr,
                default_keys: "Shift-J",
                label: "join into PR",
                describe: "Fold this issue into the PR that closes it (one row instead of two).",
                section: Section::Workspace,
            },
            ActionKind::RequestReviewers => &Self {
                kind: ActionKind::RequestReviewers,
                default_keys: "g v | Shift-V",
                label: "reviewers",
                describe: "Request reviewer(s) on the workspace's PR.",
                section: Section::Workspace,
            },
            ActionKind::AddAssignees => &Self {
                kind: ActionKind::AddAssignees,
                default_keys: "g a | Shift-G",
                label: "assignees",
                describe: "Change assignees on the workspace's PR / issue — pre-checks existing; toggle to add or remove.",
                section: Section::Workspace,
            },
            ActionKind::ManageLabels => &Self {
                kind: ActionKind::ManageLabels,
                default_keys: "g l | Shift-L",
                label: "labels",
                describe: "Add / remove labels on the workspace's PR or issue. Picker pre-checks the labels currently applied; submit replaces the set.",
                section: Section::Workspace,
            },
            ActionKind::OpenInBrowser => &Self {
                kind: ActionKind::OpenInBrowser,
                default_keys: "g o | Shift-O",
                label: "open in browser",
                describe: "Open the focused workspace's PR / issue page in your default web browser.",
                section: Section::Workspace,
            },
            // ── Sidebar list management ─────────────────────────────
            ActionKind::CycleRoleFilter => &Self {
                kind: ActionKind::CycleRoleFilter,
                default_keys: "f",
                label: "filter",
                describe: "Cycle the role filter (All → Author → Reviewer → Assignee → Mentioned).",
                section: Section::Sidebar,
            },
            ActionKind::CycleSort => &Self {
                kind: ActionKind::CycleSort,
                default_keys: "o",
                label: "sort",
                describe: "Cycle the sort order (recency → by-role → by-role with section headers).",
                section: Section::Sidebar,
            },
            ActionKind::CycleMailbox => &Self {
                kind: ActionKind::CycleMailbox,
                default_keys: "Shift-S",
                label: "mailbox",
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
                describe: "Scroll the terminal's scrollback buffer.",
                section: Section::Terminal,
            },
            ActionKind::LeaveTerminal => &Self {
                kind: ActionKind::LeaveTerminal,
                default_keys: "] ]",
                label: "exit to sidebar",
                describe: "Double-tap the escape char to leave the terminal. The same `]]` is a leader: `]]<key>` opens snippets; a lone `]` is sent to the agent.",
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
        // The `for_kind` arms enumerate every variant of ActionKind.
        // Listing them here in display order avoids macro magic and
        // keeps the canonical ordering inspectable.
        [
            // Global
            ActionKind::CyclePane,
            ActionKind::Refresh,
            ActionKind::OpenSettings,
            ActionKind::OpenHelp,
            ActionKind::OpenTour,
            ActionKind::OpenSyncStatus,
            ActionKind::JumpToAsking,
            ActionKind::JumpToFailingCi,
            ActionKind::ToggleFocusMode,
            ActionKind::StartAgent,
            ActionKind::ToggleActivityPane,
            ActionKind::ResizeSplitter,
            ActionKind::Quit,
            // Workspace
            ActionKind::OpenWorkspace,
            ActionKind::Work,
            ActionKind::SpawnAgent,
            ActionKind::SpawnShell,
            ActionKind::OpenEditor,
            ActionKind::MarkAllRead,
            ActionKind::ToggleSnooze,
            ActionKind::LongSnooze,
            // Project comes before Workspace — projects are
            // containers; the user reads "create a project, then
            // create workspaces inside it." Help modal + any other
            // catalog-driven UI inherits this ordering.
            ActionKind::NewProject,
            ActionKind::NewWorkspace,
            ActionKind::MergePr,
            ActionKind::RequestReviewers,
            ActionKind::AddAssignees,
            ActionKind::ManageLabels,
            ActionKind::OpenInBrowser,
            ActionKind::Reply,
            ActionKind::AdoptSessions,
            ActionKind::CollapseIntoPr,
            ActionKind::Archive,
            // Sidebar list management
            ActionKind::CycleRoleFilter,
            ActionKind::CycleSort,
            ActionKind::CycleMailbox,
            ActionKind::OpenSearch,
            // Activity
            ActionKind::ToggleActivity,
            ActionKind::ToggleRow,
            ActionKind::ActivityTop,
            ActionKind::ActivityBottom,
            ActionKind::ToggleDescription,
            ActionKind::SelectRow,
            ActionKind::UndoMarkRead,
            // Terminal
            ActionKind::TerminalScroll,
            ActionKind::LeaveTerminal,
        ]
        .into_iter()
        .map(Self::for_kind)
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
/// out-of-band: the github `g`-group (`g m`, `g v`, …), the two-press
/// quit (`q q`), and the terminal escape (`] ]`). The which-key popup
/// is then a pure function of the armed prefix — "which catalog
/// entries have a `Seq` starting with this stroke?" — instead of a
/// hardcoded `ActionGroup` table.
///
/// Parsed from the catalog's `default_keys` string so the catalog
/// stays human-readable: alternatives are separated by ` | `
/// (`"g m | Shift-M"`), and the keystrokes WITHIN one alternative are
/// space-separated (`"g m"`, `"q q"`). Presentation-only strings
/// (`"g/G"`, `"↑/↓"`, `"all keys"`) still don't parse to a chord.
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
            // that key" presentation separator (`c / x / u`), not a
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
        matches!(self.section, Section::Terminal)
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
            ActionKind::Archive => Guard::Confirm(
                "Archive the focused workspace? Active sessions \
                 are killed and the row drops from the inbox.",
            ),
            ActionKind::MergePr => Guard::Confirm(
                "Merge the focused PR? Mainline branch updates \
                 immediately and the PR closes.",
            ),
            ActionKind::LongSnooze => Guard::Confirm(
                "Long-snooze this workspace (~1 year)? It drops from \
                 the inbox until then — effectively hidden.",
            ),
            _ => Guard::None,
        }
    }

    /// True when this action is gated behind a Confirm modal — the
    /// dispatch path (`Model::dispatch_action`) routes these through a
    /// unified Confirm before firing. (Named for history; really
    /// "guard is `Confirm`".)
    pub fn is_destructive(&self) -> bool {
        matches!(self.guard(), Guard::Confirm(_))
    }

    /// Confirm-modal prompt text, for a `Confirm`-guarded action;
    /// `None` otherwise — those shouldn't be routed through the
    /// confirm path.
    pub fn confirm_prompt(&self) -> Option<&'static str> {
        match self.guard() {
            Guard::Confirm(prompt) => Some(prompt),
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
            ActionKind::SpawnAgent => "spawn_agent",
            ActionKind::SpawnShell => "spawn_shell",
            ActionKind::OpenEditor => "open_editor",
            ActionKind::NewWorkspace => "new_workspace",
            ActionKind::NewProject => "new_project",
            ActionKind::MarkAllRead => "mark_all_read",
            ActionKind::ToggleSnooze => "toggle_snooze",
            ActionKind::LongSnooze => "long_snooze",
            ActionKind::Archive => "archive",
            ActionKind::MergePr => "merge_pr",
            ActionKind::AdoptSessions => "adopt_sessions",
            ActionKind::CollapseIntoPr => "collapse_into_pr",
            ActionKind::RequestReviewers => "request_reviewers",
            ActionKind::AddAssignees => "add_assignees",
            ActionKind::ManageLabels => "manage_labels",
            ActionKind::OpenInBrowser => "open_in_browser",
            ActionKind::CycleRoleFilter => "cycle_role_filter",
            ActionKind::CycleSort => "cycle_sort",
            ActionKind::CycleMailbox => "cycle_mailbox",
            ActionKind::OpenSearch => "open_search",
            ActionKind::ToggleActivity => "toggle_activity",
            ActionKind::ToggleRow => "toggle_row",
            ActionKind::ActivityTop => "activity_top",
            ActionKind::ActivityBottom => "activity_bottom",
            ActionKind::Reply => "reply",
            ActionKind::SelectRow => "select_row",
            ActionKind::ToggleDescription => "toggle_description",
            ActionKind::UndoMarkRead => "undo_mark_read",
            ActionKind::CyclePane => "cycle_pane",
            ActionKind::Refresh => "refresh",
            ActionKind::OpenHelp => "open_help",
            ActionKind::OpenTour => "open_tour",
            ActionKind::OpenSyncStatus => "open_sync_status",
            ActionKind::OpenSettings => "open_settings",
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
}

/// The guard between a keypress and an action firing — the per-row
/// replacement for the scattered double-press / confirm latches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Guard {
    /// Fires immediately.
    None,
    /// Needs a timed two-press of the chord (e.g. quit's `q q`).
    DoublePress,
    /// Mounts a Confirm modal carrying this prompt before firing.
    Confirm(&'static str),
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
/// overrides). `vim` goes leaders-primary — the github actions bind to
/// their `g …` chord only, dropping the legacy `Shift-*` aliases — and
/// moves pane-cycling onto `Ctrl-w` (vim's window key). Every preset is
/// collision-checked by the `preset_*_has_no_collisions` tests so a
/// bad entry can't ship.
pub fn keymap_preset(name: &str) -> Option<std::collections::BTreeMap<String, String>> {
    let mut m = std::collections::BTreeMap::new();
    match name {
        "default" => {}
        "vim" => {
            // Leaders-primary: keep only the `g …` chord for the github
            // actions (the `Shift-*` aliases drop away).
            m.insert("merge_pr".into(), "g m".into());
            m.insert("request_reviewers".into(), "g v".into());
            m.insert("add_assignees".into(), "g a".into());
            m.insert("manage_labels".into(), "g l".into());
            m.insert("open_in_browser".into(), "g o".into());
            // Vim's window key cycles panes.
            m.insert("cycle_pane".into(), "Ctrl-w".into());
        }
        _ => return None,
    }
    Some(m)
}

/// The default single-letter key a known agent binds to. `None` for
/// agents lazybox doesn't ship a convention for — they still get a
/// catalog row (in help, remappable), just no default chord.
pub fn agent_default_key(id: &str) -> Option<char> {
    match id {
        "claude" => Some('c'),
        "codex" => Some('x'),
        "cursor" | "cursor-agent" => Some('u'),
        _ => None,
    }
}

impl ActionDef {
    /// Build the runtime catalog: every static action EXCEPT the
    /// generic `SpawnAgent` placeholder, plus one concrete `SpawnAgent`
    /// row per enabled agent. The agent rows are what let `c` / `x` /
    /// `u` live in the catalog — remappable via `ui.action_keys`
    /// (`spawn_agent.<id>`), listed in help, and collision-checked —
    /// instead of in a side map.
    ///
    /// `overrides` (`ui.action_keys`) are applied here so every
    /// consumer reads resolved chords + display strings.
    pub fn catalog(
        agents: &[String],
        overrides: &std::collections::BTreeMap<String, String>,
    ) -> Vec<CatalogEntry> {
        let mut out: Vec<CatalogEntry> = Vec::new();
        for def in ActionDef::all() {
            // The static SpawnAgent row is a placeholder for the
            // generated per-agent rows below — drop it.
            if def.kind == ActionKind::SpawnAgent {
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
        out
    }
}

/// Default chord(s) + display for an agent row from the built-in
/// key convention. An agent lazybox has no convention for gets an
/// empty chord list (no default binding) and a blank display.
fn default_agent_chords(id: &str) -> (Vec<Chord>, std::borrow::Cow<'static, str>) {
    match agent_default_key(id) {
        Some(c) => (
            vec![Chord::Key(KeyStroke::new(
                false,
                false,
                false,
                ChordCode::Char(c),
            ))],
            std::borrow::Cow::Owned(c.to_string()),
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
        ActionKind::Work => intent::classify_work(workspace, &[]).is_some(),
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
        ActionKind::SpawnShell => matches!(
            intent::resolve_spawn_shell(workspace),
            intent::Intent::SpawnShell { .. },
        ),
        ActionKind::Reply => matches!(
            intent::resolve_reply(workspace),
            intent::Intent::MountReply { .. },
        ),
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
        | ActionKind::OpenInBrowser => has_ws,
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
        // has focus (which `section_rank` already gates).
        ActionKind::CycleRoleFilter
        | ActionKind::CycleSort
        | ActionKind::CycleMailbox
        | ActionKind::OpenSearch => true,
        // Global / no-workspace-needed actions.
        ActionKind::NewWorkspace
        | ActionKind::NewProject
        | ActionKind::StartAgent
        | ActionKind::CyclePane
        | ActionKind::Refresh
        | ActionKind::OpenHelp
        | ActionKind::OpenTour
        | ActionKind::OpenSyncStatus
        | ActionKind::OpenSettings
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
/// `]]` leave chord first — see [`ActionDef::available_in_terminal`].
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
        // If `for_kind` ever gets out of sync with `ActionKind` it
        // would panic at compile time on an unmatched variant. This
        // test additionally guards against `for_kind` shadowing a
        // variant with a stale label by accident.
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
    fn default_chords_splits_alternatives() {
        // `g m | Shift-M` yields the leader sequence AND the legacy
        // modifier alias as two alternatives.
        let def = ActionDef::for_kind(ActionKind::MergePr);
        let chords = def.default_chords();
        assert_eq!(chords.len(), 2, "merge has a leader + a Shift alias");
        assert!(matches!(chords[0], Chord::Seq(_)));
        assert_eq!(
            chords[1],
            Chord::Key(KeyStroke::new(false, true, false, ChordCode::Char('m'))),
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
        // unreachable. Cross-section shadowing (e.g. `Shift-G` =
        // assignees in Workspace vs jump-to-bottom in Activity) is a
        // DELIBERATE, focus-ranked override and intentionally not
        // flagged here. This is the single audit surface the catalog
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
    fn every_parseable_default_round_trips_to_chord() {
        // Smoke: every catalog entry whose default_keys carries at
        // least one parseable alternative must yield a chord. Catches
        // a typo in `default_keys` that would silently break the
        // matcher.
        // Presentation-only `default_keys` — no parseable chord.
        let presentation = [
            "c / x / u",
            "g/G",
            "↑/↓",
            "→/←",
            "Shift-PgUp/Dn",
            "Shift-Arrows",
            "all keys",
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
    fn availability_without_workspace_blocks_workspace_actions() {
        // Sanity: Workspace-scoped actions can't fire without a
        // target. Global ones still can.
        assert!(!availability(ActionKind::Work, None));
        assert!(!availability(ActionKind::MergePr, None));
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
            ActionKind::RequestReviewers,
            ActionKind::AddAssignees,
            ActionKind::ManageLabels,
            ActionKind::OpenInBrowser,
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
    fn available_in_terminal_tracks_section() {
        // The terminal pane forwards every key to the PTY; only its
        // own section's chords (`]]`, scrollback) survive that. The
        // predicate must equal "is a Terminal-section action" for
        // every catalog entry — that equivalence is the single source
        // of truth every surface gates its terminal claims on (#114).
        for def in ActionDef::all() {
            assert_eq!(
                def.available_in_terminal(),
                def.section == Section::Terminal,
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
        // `available_in_terminal` — the honest contract is "press `]]`
        // first." The `]]` leave chord is the one that genuinely works.
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
            "the `]]` leave chord is the gateway back to the globals",
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
        // Built-in convention: claude → `c`.
        assert_eq!(
            claude.chords,
            vec![Chord::Key(KeyStroke::new(
                false,
                false,
                false,
                ChordCode::Char('c')
            ))],
        );

        // An agent with no built-in convention gets a row but no chord.
        let aider = spawn_rows
            .iter()
            .find(|e| e.param == Some(Param::Agent("aider".into())))
            .expect("aider row");
        assert!(aider.chords.is_empty(), "aider has no default key");
    }

    #[test]
    fn catalog_dedupes_shared_agent_default_key() {
        // `cursor` and `cursor-agent` both default to `u`; only the
        // first keeps the binding, the second still gets a (remappable)
        // row but no colliding default chord.
        use std::collections::BTreeMap;
        let agents: Vec<String> = ["cursor", "cursor-agent"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let catalog = ActionDef::catalog(&agents, &BTreeMap::new());
        let u = Chord::Key(KeyStroke::new(false, false, false, ChordCode::Char('u')));
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
        // The vim preset drops the legacy `Shift-M` alias for merge —
        // only the `g m` leader survives.
        let overrides = keymap_preset("vim").unwrap();
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
