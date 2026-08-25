//! View-model types for the inbox: the mailbox, the sort/kind
//! taxonomy, the rendered-row tree, and per-repo summaries. All pure
//! over `lazybox-core`/`lazybox-ipc` domain types — no render context
//! — so both the ratatui TUI and the desktop client build the same
//! sidebar from the same code.

use lazybox_core::{SessionId, SessionKey};

/// Which logical mailbox the inbox is currently showing.
///
/// Three mutually-exclusive buckets, cycled via `Shift-S` in the TUI:
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub enum Mailbox {
    #[default]
    Inbox,
    Inactive,
    Snoozed,
}

impl Mailbox {
    /// Next mailbox in the cycle, matching the sidebar's `Shift-S`
    /// order (Inbox → Inactive → Snoozed → Inbox).
    pub fn next(self) -> Self {
        match self {
            Mailbox::Inbox => Mailbox::Inactive,
            Mailbox::Inactive => Mailbox::Snoozed,
            Mailbox::Snoozed => Mailbox::Inbox,
        }
    }

    /// Short label for a mailbox control (mirrors [`SortMode::chip_label`]).
    pub fn chip_label(self) -> &'static str {
        match self {
            Mailbox::Inbox => "inbox",
            Mailbox::Inactive => "inactive",
            Mailbox::Snoozed => "snoozed",
        }
    }

    /// Parse a persisted `ui.last_lens` mailbox token (the
    /// [`Self::chip_label`] round-trip). `None` for unknown tokens.
    pub fn from_chip_label(label: &str) -> Option<Self> {
        match label {
            "inbox" => Some(Mailbox::Inbox),
            "inactive" => Some(Mailbox::Inactive),
            "snoozed" => Some(Mailbox::Snoozed),
            _ => None,
        }
    }
}

/// How the inbox orders workspaces within each repo group.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
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

    /// Parse a persisted `ui.last_lens` sort token (the
    /// [`Self::chip_label`] round-trip). `None` for unknown tokens.
    pub fn from_chip_label(label: &str) -> Option<Self> {
        match label {
            "recent" => Some(SortMode::Recent),
            "by-role" => Some(SortMode::ByRole),
            "split" => Some(SortMode::ByRoleSplit),
            _ => None,
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
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
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

/// One row in the rendered sidebar list. The visual model is a grouped
/// tree whose workspace tier may itself contain parent/child tickets:
///
/// ```text
/// owner/name              <- RepoHeader
///   ▾ Parent ticket       <- Workspace (always present)
///       claude            <- Session (only when workspace has 2+)
///       shell             <- Session
///     · Child ticket      <- Workspace, indented by ticket ancestry
///   · Other ticket
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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub enum VisibleRow {
    /// Synthetic `★ Focused` group header, emitted first (above every
    /// repo header) when the user has starred one or more workspaces
    /// that are visible in the current mailbox/filter. Holds no data —
    /// the starred workspace rows follow it directly, lifted out of
    /// their repo groups regardless of which repo they belong to.
    /// Non-selectable like the other headers; j/k skips it.
    FocusedHeader,
    /// Synthetic personal queue header. Active hopper workspaces render
    /// here, directly below Focused and outside their assigned repo group.
    HopperHeader,
    /// Space group header — the higher-level grouping tier (#860),
    /// emitted above the `RepoHeader`s it contains. The string is the
    /// Space name (a user-defined bucket, an owner auto-seed, or
    /// `"Ungrouped"`). Only present when the Space tier is active (≥2
    /// distinct Spaces this pass); otherwise the tree stays flat at the
    /// repo level. Non-selectable like `RepoHeader` — navigation skips
    /// it, the cursor parks on it only for collapse / click.
    SpaceHeader(String),
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

/// Hierarchy metadata for one visible workspace row. Kept beside
/// [`VisibleRow`] rather than changing that long-lived enum's wire shape:
/// clients that only need selection still consume the row key, while tree-
/// aware clients add indentation and a disclosure control from this map.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct TicketTreeMeta {
    /// Zero-based depth within the visible parent-ticket forest.
    pub depth: usize,
    /// Whether this ticket has at least one visible direct child.
    pub has_children: bool,
    /// Whether those visible descendants are currently folded away.
    pub collapsed: bool,
    /// This row did not match the active filter/search itself, but is kept
    /// as ancestor context for a matching descendant.
    pub context_only: bool,
}

/// Free-text search over the inbox. Two flavours share this state:
///
/// - **Project-scoped** (`scope: Some(label)`) — invoked with `/`,
///   filters only that project's (repo group's) PRs + Issues, leaving
///   every other project untouched.
/// - **Global** (`scope: None`) — invoked from the header search box
///   (`#`), filters every repo group at once so a query — especially a
///   PR/issue number — surfaces matches across the whole inbox.
///
/// Both fuzzy-match on title and substring-match on number. `editing`
/// is true while the input bar is capturing keystrokes (between the
/// open key and `Enter`/`Esc`). `Enter` keeps the query applied but
/// stops capturing so j/k navigates the results; `Esc` clears the
/// query and closes the bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchState {
    /// Repo-header label the search is scoped to — matched against
    /// [`super::group_label`] so only that project's rows are
    /// filtered. `Some(label)` is captured from the row under the
    /// cursor when `/` opens; `None` is a global search across every
    /// repo group.
    pub scope: Option<String>,
    pub query: String,
    pub editing: bool,
}

/// Per-repo summary line shown in the collapsible header.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
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

#[cfg(test)]
mod mailbox_tests {
    use super::Mailbox;

    #[test]
    fn mailbox_cycles_inbox_inactive_snoozed() {
        assert_eq!(Mailbox::default(), Mailbox::Inbox);
        assert_eq!(Mailbox::Inbox.next(), Mailbox::Inactive);
        assert_eq!(Mailbox::Inactive.next(), Mailbox::Snoozed);
        assert_eq!(Mailbox::Snoozed.next(), Mailbox::Inbox);
    }

    #[test]
    fn mailbox_chip_labels_are_lowercase_names() {
        assert_eq!(Mailbox::Inbox.chip_label(), "inbox");
        assert_eq!(Mailbox::Inactive.chip_label(), "inactive");
        assert_eq!(Mailbox::Snoozed.chip_label(), "snoozed");
    }
}
