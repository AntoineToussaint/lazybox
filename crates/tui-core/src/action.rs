//! `Action` — the unified vocabulary of "things the user can do."
//!
//! # Why this exists
//!
//! Pilot has three surfaces that ask "which actions are available at
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
//! Pilot's data model is plugin-shaped: providers (github, linear,
//! …) emit `Workspace`s; pilot wraps them in a uniform UI. Actions
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
//! - Configurable rebinding from `~/.pilot/config.yaml`. The
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
    /// Create-or-focus the shared local Sandbox project.
    OpenSandbox,
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
    /// Add reviewer(s) to the workspace's PR (github GraphQL mutation).
    RequestReviewers,
    /// Add assignee(s) to the workspace's PR or issue.
    AddAssignees,

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
    /// Open the `,` Settings palette.
    OpenSettings,
    /// Begin the two-press quit chord. Single-press from a remap
    /// just fires.
    Quit,
    /// Detach the focused pane to a new pilot process (Ctrl+Shift+D).
    DetachPane,
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
/// or iterated whole via [`ActionDef::ALL`].
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
    OpenSandbox,
    MarkAllRead,
    ToggleSnooze,
    Archive,
    MergePr,
    AdoptSessions,
    RequestReviewers,
    AddAssignees,
    // Activity
    ToggleActivity,
    ToggleRow,
    Reply,
    SelectRow,
    ToggleDescription,
    UndoMarkRead,
    // Global
    CyclePane,
    Refresh,
    OpenHelp,
    OpenSettings,
    Quit,
    DetachPane,
    ResizeSplitter,
    // Terminal
    TerminalScroll,
    LeaveTerminal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Global,
    Workspace,
    Activity,
    Terminal,
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
            Action::OpenSandbox => ActionKind::OpenSandbox,
            Action::MarkAllRead => ActionKind::MarkAllRead,
            Action::ToggleSnooze => ActionKind::ToggleSnooze,
            Action::Archive => ActionKind::Archive,
            Action::MergePr => ActionKind::MergePr,
            Action::AdoptSessions => ActionKind::AdoptSessions,
            Action::RequestReviewers => ActionKind::RequestReviewers,
            Action::AddAssignees => ActionKind::AddAssignees,
            Action::ToggleActivity => ActionKind::ToggleActivity,
            Action::ToggleRow => ActionKind::ToggleRow,
            Action::Reply => ActionKind::Reply,
            Action::SelectRow => ActionKind::SelectRow,
            Action::ToggleDescription => ActionKind::ToggleDescription,
            Action::UndoMarkRead => ActionKind::UndoMarkRead,
            Action::CyclePane => ActionKind::CyclePane,
            Action::Refresh => ActionKind::Refresh,
            Action::OpenHelp => ActionKind::OpenHelp,
            Action::OpenSettings => ActionKind::OpenSettings,
            Action::Quit => ActionKind::Quit,
            Action::DetachPane => ActionKind::DetachPane,
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
            ActionKind::OpenSettings => &Self {
                kind: ActionKind::OpenSettings,
                default_keys: ",",
                label: "settings",
                describe: "Open the Settings palette.",
                section: Section::Global,
            },
            ActionKind::Quit => &Self {
                kind: ActionKind::Quit,
                default_keys: "q q",
                label: "quit",
                describe: "Quit pilot. Default is the two-key chord; a single-letter remap fires on first press.",
                section: Section::Global,
            },
            ActionKind::DetachPane => &Self {
                kind: ActionKind::DetachPane,
                default_keys: "Ctrl-Shift-D",
                label: "detach pane",
                describe: "Spawn a new pilot process pinned to the focused pane.",
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
            ActionKind::OpenSandbox => &Self {
                kind: ActionKind::OpenSandbox,
                default_keys: "Shift-N",
                label: "sandbox",
                describe: "Focus the shared local sandbox project (non-provider).",
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
                describe: "Add assignee(s) to the workspace's PR / issue.",
                section: Section::Workspace,
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
            ActionKind::Reply => &Self {
                kind: ActionKind::Reply,
                default_keys: "r",
                label: "reply",
                describe: "Open the reply textarea targeted at this workspace.",
                section: Section::Activity,
            },
            ActionKind::SelectRow => &Self {
                kind: ActionKind::SelectRow,
                default_keys: "v",
                label: "select row",
                describe: "Toggle the focused activity row in/out of the multi-select set.",
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
                describe: "Escape the terminal back to the sidebar (double-tap the configured escape char).",
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
            ActionKind::DetachPane,
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
            ActionKind::NewWorkspace,
            ActionKind::OpenSandbox,
            ActionKind::MergePr,
            ActionKind::RequestReviewers,
            ActionKind::AddAssignees,
            ActionKind::AdoptSessions,
            ActionKind::Archive,
            // Activity
            ActionKind::ToggleActivity,
            ActionKind::ToggleRow,
            ActionKind::ToggleDescription,
            ActionKind::Reply,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_has_a_def() {
        // If `for_kind` ever gets out of sync with `ActionKind` it
        // would panic at compile time on an unmatched variant. This
        // test additionally guards against `for_kind` shadowing a
        // variant with a stale label by accident.
        for def in ActionDef::all() {
            assert!(!def.default_keys.is_empty(), "{:?} missing default key", def.kind);
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
    fn all_is_sorted_by_section() {
        // The `all()` iterator emits Global first, then Workspace,
        // then Activity, then Terminal. Help relies on this for its
        // section dividers — assert here so a reorder surfaces.
        let order: Vec<Section> = ActionDef::all().map(|d| d.section).collect();
        let mut last_idx = 0;
        let order_of = |s: Section| match s {
            Section::Global => 0,
            Section::Workspace => 1,
            Section::Activity => 2,
            Section::Terminal => 3,
        };
        for s in order {
            let idx = order_of(s);
            assert!(idx >= last_idx, "section {s:?} appeared out of order");
            last_idx = idx;
        }
    }
}
