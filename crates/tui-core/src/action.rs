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

/// A *leader group* for the grouped two-step (leader-key) chords from
/// issue #126: the first key picks the group, the second picks an
/// action within it. The point is to free the single-letter namespace
/// — actions slot into a group instead of fighting for a top-level
/// letter — and to make the bindings self-documenting through a
/// which-key popup that appears once the leader is pressed.
///
/// Only `Github` is wired today. It collapses the awkward
/// `Shift-M` / `Shift-V` / `Shift-G` / `Shift-L` / `Shift-O` cluster
/// (which the issue calls out as modifier-inconsistent) into a single
/// `g`-led chord: `g m` merge, `g v` reviewers, `g a` assignees,
/// `g l` labels, `g o` open-in-browser. The original `Shift-*` keys
/// stay live as direct aliases during the transition — this is purely
/// additive. The taxonomy is meant to grow (workspace lifecycle,
/// activity, terminal/tiles) as the rest of the catalog migrates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActionGroup {
    /// GitHub / PR actions on the focused workspace.
    Github,
}

impl ActionGroup {
    /// Leader key that opens this group's which-key popup. Single
    /// char, no modifiers — pressed first, then the in-group key.
    pub fn leader(self) -> char {
        match self {
            ActionGroup::Github => 'g',
        }
    }

    /// Short title shown at the top of the which-key popup.
    pub fn title(self) -> &'static str {
        match self {
            ActionGroup::Github => "github",
        }
    }

    /// Resolve a group from its leader key, or `None` when the key
    /// isn't a group leader.
    pub fn from_leader(c: char) -> Option<Self> {
        match c {
            'g' => Some(ActionGroup::Github),
            _ => None,
        }
    }

    /// The actions in this group, in popup display order, each paired
    /// with the in-group key that fires it (the SECOND press of the
    /// chord). In-group keys are unique within a group.
    pub fn members(self) -> &'static [(char, ActionKind)] {
        match self {
            ActionGroup::Github => &[
                ('m', ActionKind::MergePr),
                ('v', ActionKind::RequestReviewers),
                ('a', ActionKind::AddAssignees),
                ('l', ActionKind::ManageLabels),
                ('o', ActionKind::OpenInBrowser),
            ],
        }
    }

    /// Look up the action bound to the in-group key `c`, if any.
    pub fn action_for_key(self, c: char) -> Option<ActionKind> {
        self.members()
            .iter()
            .find(|(k, _)| *k == c)
            .map(|(_, kind)| *kind)
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
                describe: "Launch the guided feature walkthrough (inbox, work, snippets, navigation, config).",
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
                label: "new project",
                describe: "Create a local project (a top-level container, asks for a name).",
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
            ActionKind::Archive => &Self {
                kind: ActionKind::Archive,
                default_keys: "Shift-X",
                label: "archive",
                describe: "Drop the workspace and kill any sessions. Destructive.",
                section: Section::Workspace,
            },
            ActionKind::MergePr => &Self {
                kind: ActionKind::MergePr,
                default_keys: "Shift-M",
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
                default_keys: "Shift-V",
                label: "reviewers",
                describe: "Request reviewer(s) on the workspace's PR.",
                section: Section::Workspace,
            },
            ActionKind::AddAssignees => &Self {
                kind: ActionKind::AddAssignees,
                default_keys: "Shift-G",
                label: "assignees",
                describe: "Change assignees on the workspace's PR / issue — pre-checks existing; toggle to add or remove.",
                section: Section::Workspace,
            },
            ActionKind::ManageLabels => &Self {
                kind: ActionKind::ManageLabels,
                default_keys: "Shift-L",
                label: "labels",
                describe: "Add / remove labels on the workspace's PR or issue. Picker pre-checks the labels currently applied; submit replaces the set.",
                section: Section::Workspace,
            },
            ActionKind::OpenInBrowser => &Self {
                kind: ActionKind::OpenInBrowser,
                default_keys: "Shift-O",
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
                default_keys: "]]",
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
// KeyChord — typed representation of a keystroke, with parser
// ──────────────────────────────────────────────────────────────────

/// A single keystroke (or two-press chord) matched against a
/// `KeyEvent` at dispatch time. Parsed from the catalog's
/// `default_keys` string so the catalog stays human-readable
/// (`"Shift-M"`, `"Ctrl-Shift-D"`, `"q q"`) but the matcher can
/// compare strictly typed.
///
/// Not every catalog `default_keys` parses into a chord:
/// - `"g/G"`, `"↑/↓"`, `"→/←"`, `"PgUp/Dn"` are *presentation*
///   strings meaning "multiple keys, multiple actions" — they don't
///   round-trip to a single chord and `parse` returns `None`.
/// - `"all keys"` is the terminal pane's "forward everything"
///   marker and likewise doesn't parse.
///
/// Callers that need to migrate the matcher should add explicit
/// catalog entries for the secondary keys (e.g. `NavTop` /
/// `NavBottom` for `g` / `G`) so the parser stays simple.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum KeyChord {
    /// Single keystroke. Modifiers + code.
    Single {
        ctrl: bool,
        shift: bool,
        alt: bool,
        code: ChordCode,
    },
    /// Two-key chord (e.g. `q q` for quit). Both keys are single
    /// presses of the same chord; runtime tracks the latch.
    Double(Box<KeyChord>),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ChordCode {
    Char(char),
    Named(NamedKey),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NamedKey {
    Tab,
    Enter,
    Esc,
    Space,
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
}

impl KeyChord {
    /// Parse a `default_keys` string into a typed chord. Returns
    /// `None` for the presentation-only forms (`g/G`, `↑/↓`,
    /// `all keys`, etc.) — those need explicit catalog entries
    /// rather than a runtime parse.
    pub fn parse(s: &str) -> Option<Self> {
        let trimmed = s.trim();
        // Two-key chord: "q q" → Double(single 'q').
        if let Some((a, b)) = trimmed.split_once(' ')
            && a == b
            && let Some(inner) = Self::parse_single(a)
        {
            return Some(KeyChord::Double(Box::new(inner)));
        }
        Self::parse_single(trimmed)
    }

    fn parse_single(s: &str) -> Option<Self> {
        // Presentation strings — explicitly not single chords. A
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
            "Space" => ChordCode::Named(NamedKey::Space),
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
            // Single ASCII letter / symbol — uppercase letters
            // mean Shift-letter; lowercase stays as-is. The Shift
            // prefix takes precedence (`"Shift-M"` parses to
            // `shift=true, code=Char('M')` either way).
            other if other.chars().count() == 1 => {
                let c = other.chars().next().unwrap();
                if c.is_ascii_uppercase() {
                    shift = true;
                }
                ChordCode::Char(c.to_ascii_lowercase())
            }
            _ => return None,
        };
        Some(KeyChord::Single {
            ctrl,
            shift,
            alt,
            code,
        })
    }
}

impl ActionDef {
    /// Typed chord for this action's default keystroke. `None` for
    /// presentation-only `default_keys` like `g/G` or `↑/↓` (see
    /// `KeyChord` docs). Cheap; parses each call. Re-parse cost is
    /// negligible vs. caching given how rarely we look up.
    pub fn default_chord(&self) -> Option<KeyChord> {
        KeyChord::parse(self.default_keys)
    }

    /// True when this action is *destructive* — invoking it commits
    /// state the user can't trivially undo (merging a PR, archiving
    /// a workspace, killing a session). The dispatch path
    /// (`Model::dispatch_action`) routes destructive actions
    /// through a unified Confirm modal BEFORE firing; non-
    /// destructive actions fire immediately.
    ///
    /// Adding a new destructive action: mark it here AND add the
    /// matching `confirm_prompt` arm. Forgetting one half is a
    /// catalog bug — the type system can't catch it directly, but
    /// the test `destructive_actions_have_prompts` does.
    pub fn is_destructive(&self) -> bool {
        matches!(self.kind, ActionKind::Archive | ActionKind::MergePr,)
    }

    /// Confirm-modal prompt text for a destructive action. Returns
    /// `None` for non-destructive actions — those shouldn't be
    /// routed through the confirm path. The catalog default is
    /// static; specific surfaces (e.g. the merge-PR flow knows the
    /// PR number) can override at mount time.
    pub fn confirm_prompt(&self) -> Option<&'static str> {
        match self.kind {
            ActionKind::Archive => Some(
                "Archive the focused workspace? Active sessions \
                 are killed and the row drops from the inbox.",
            ),
            ActionKind::MergePr => Some(
                "Merge the focused PR? Mainline branch updates \
                 immediately and the PR closes.",
            ),
            _ => None,
        }
    }

    /// Resolve the effective key chord: user override from the
    /// config map if present, otherwise the catalog default.
    ///
    /// `overrides` keys are the snake_case `ActionKind` names
    /// (`"merge_pr"`, `"spawn_shell"`, etc.) — see [`ActionKind::name`].
    /// A user-supplied string that fails to parse falls back to the
    /// default rather than erroring out: a typo in YAML shouldn't
    /// break the keyboard.
    pub fn effective_chord(
        &self,
        overrides: &std::collections::BTreeMap<String, String>,
    ) -> Option<KeyChord> {
        if let Some(raw) = overrides.get(self.kind.name())
            && let Some(c) = KeyChord::parse(raw)
        {
            return Some(c);
        }
        self.default_chord()
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
            && KeyChord::parse(raw).is_some()
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
            ActionKind::Quit => "quit",
            ActionKind::ResizeSplitter => "resize_splitter",
            ActionKind::TerminalScroll => "terminal_scroll",
            ActionKind::LeaveTerminal => "leave_terminal",
        }
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
        | ActionKind::CyclePane
        | ActionKind::Refresh
        | ActionKind::OpenHelp
        | ActionKind::OpenTour
        | ActionKind::OpenSyncStatus
        | ActionKind::OpenSettings
        | ActionKind::JumpToAsking
        | ActionKind::JumpToFailingCi
        | ActionKind::Quit
        | ActionKind::ResizeSplitter
        | ActionKind::TerminalScroll
        | ActionKind::LeaveTerminal => true,
    }
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
            KeyChord::Single {
                ctrl: true,
                shift: false,
                alt: false,
                code: ChordCode::Char('m'),
            }
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
        overrides.insert("refresh".into(), "F5".into());
        let def = ActionDef::for_kind(ActionKind::Refresh);
        // F5 doesn't parse as a chord (no Function-key support yet),
        // so it should fall back to the default — typo guard.
        assert_eq!(def.effective_keys_display(&overrides), "Shift-R");

        // A parseable override surfaces.
        overrides.insert("refresh".into(), "Ctrl-r".into());
        assert_eq!(def.effective_keys_display(&overrides), "Ctrl-r");
    }

    #[test]
    fn key_chord_parses_simple_letter() {
        let c = KeyChord::parse("s").unwrap();
        assert_eq!(
            c,
            KeyChord::Single {
                ctrl: false,
                shift: false,
                alt: false,
                code: ChordCode::Char('s'),
            }
        );
    }

    #[test]
    fn key_chord_parses_uppercase_as_shift() {
        // `Shift-M` and `M` should yield the same chord; the
        // catalog uses either form interchangeably.
        let explicit = KeyChord::parse("Shift-M").unwrap();
        let implicit = KeyChord::parse("M").unwrap();
        assert_eq!(explicit, implicit);
        assert_eq!(
            explicit,
            KeyChord::Single {
                ctrl: false,
                shift: true,
                alt: false,
                code: ChordCode::Char('m'),
            }
        );
    }

    #[test]
    fn key_chord_parses_modifier_stack() {
        let c = KeyChord::parse("Ctrl-Shift-D").unwrap();
        assert_eq!(
            c,
            KeyChord::Single {
                ctrl: true,
                shift: true,
                alt: false,
                code: ChordCode::Char('d'),
            }
        );
    }

    #[test]
    fn key_chord_parses_named_keys() {
        for (s, expected) in [
            ("Tab", NamedKey::Tab),
            ("Enter", NamedKey::Enter),
            ("PgUp", NamedKey::PageUp),
            ("PgDn", NamedKey::PageDown),
            ("Space", NamedKey::Space),
        ] {
            let c = KeyChord::parse(s).unwrap();
            match c {
                KeyChord::Single { code, .. } => assert_eq!(code, ChordCode::Named(expected)),
                _ => panic!("{s} should parse to a Single chord"),
            }
        }
    }

    #[test]
    fn key_chord_parses_double() {
        let c = KeyChord::parse("q q").unwrap();
        assert!(matches!(c, KeyChord::Double(_)));
    }

    #[test]
    fn presentation_forms_are_not_parsed() {
        // `g/G` etc. are display-only — we don't try to fabricate
        // a chord. Callers needing the secondary key add an
        // explicit catalog entry.
        assert!(KeyChord::parse("g/G").is_none());
        assert!(KeyChord::parse("↑/↓").is_none());
        assert!(KeyChord::parse("Shift-PgUp/Dn").is_none());
        assert!(KeyChord::parse("all keys").is_none());
    }

    #[test]
    fn lone_slash_parses_as_a_chord() {
        // A bare `/` is the literal slash key (sidebar search), not a
        // "this OR that" presentation form — it must parse so the key
        // resolves through the catalog like any other.
        assert_eq!(
            KeyChord::parse("/").unwrap(),
            KeyChord::Single {
                ctrl: false,
                shift: false,
                alt: false,
                code: ChordCode::Char('/'),
            }
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
        let mut seen: HashMap<(Section, KeyChord), ActionKind> = HashMap::new();
        for def in ActionDef::all() {
            let Some(chord) = def.default_chord() else {
                continue;
            };
            if let Some(prev) = seen.insert((def.section, chord.clone()), def.kind) {
                panic!(
                    "chord {:?} bound twice in {:?}: {:?} and {:?}",
                    chord, def.section, prev, def.kind,
                );
            }
        }
    }

    #[test]
    fn every_parseable_default_round_trips_to_chord() {
        // Smoke: every catalog entry whose default_keys is a
        // single chord (not a presentation form) must parse. Catches
        // a typo in `default_keys` that would silently break the
        // matcher.
        // Presentation-only `default_keys` — not a single chord.
        let presentation = [
            "c / x / u",
            "g/G",
            "↑/↓",
            "→/←",
            "Shift-PgUp/Dn",
            "Shift-Arrows",
            "all keys",
            "]]",
            "q q",
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
                def.default_chord().is_some(),
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
    fn group_leader_round_trips() {
        // `from_leader` is the inverse of `leader`.
        let group = ActionGroup::Github;
        assert_eq!(ActionGroup::from_leader(group.leader()), Some(group));
        // A non-leader key resolves to nothing.
        assert_eq!(ActionGroup::from_leader('q'), None);
    }

    #[test]
    fn group_members_resolve_to_real_catalog_entries() {
        // Every (key, kind) in a group must have a catalog def and be
        // reachable via `action_for_key`. Catches a member that names
        // an ActionKind the group's chord can't actually fire.
        let group = ActionGroup::Github;
        for (key, kind) in group.members() {
            assert_eq!(group.action_for_key(*key), Some(*kind));
            // `for_kind` panics on an unmatched variant, so this also
            // asserts the kind is a real catalog entry.
            let _ = ActionDef::for_kind(*kind);
        }
    }

    #[test]
    fn group_in_keys_are_unique() {
        // Two members sharing an in-group key would make the second
        // unreachable — the chord would always fire the first match.
        let group = ActionGroup::Github;
        let mut keys: Vec<char> = group.members().iter().map(|(k, _)| *k).collect();
        keys.sort_unstable();
        let before = keys.len();
        keys.dedup();
        assert_eq!(before, keys.len(), "{group:?} has duplicate in-group keys");
    }

    #[test]
    fn github_group_excludes_its_own_leader_key() {
        // `g` is the leader; an in-group key of `g` would be
        // ambiguous (`g g`). Guard against it.
        let g = ActionGroup::Github;
        assert!(
            g.members().iter().all(|(k, _)| *k != g.leader()),
            "in-group key collides with the leader",
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
