//! Generic inbox filter model.
//!
//! Filtering used to be a single fixed 5-value role cycle advanced by
//! `f`. This replaces it with a set of toggleable predicates over
//! workspace state — role is now one axis among several (state, kind).
//! Adding a filter is data: a new [`Filter`] variant plus a row in the
//! `axis` / `label` / `matches` matches below. No new enum-cycle.
//!
//! ## Combination semantics
//!
//! Active filters combine per-axis. Within one axis the active filters
//! OR together (a workspace matching `author` OR `reviewer` passes the
//! Role axis); across axes they AND (a Role filter AND a State filter
//! must both be satisfied). This is what makes presets fall out for
//! free — "needs attention" is just several State filters, and OR
//! within the axis is exactly the union the preset wants.

use lazybox_core::{CiStatus, ReviewStatus, SessionKey, TaskRole, TaskState, Workspace};
use std::collections::BTreeSet;
use std::collections::HashMap;

use super::WorkspaceKind;

/// Threshold (additions + deletions) at or above which a PR counts as a
/// "big diff" for the [`Filter::BigDiff`] predicate.
pub const BIG_DIFF_LINES: u32 = 500;

/// The axis a [`Filter`] lives on. Drives the OR-within / AND-across
/// combination in [`FilterSet::accepts`] and groups the filter menu.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub enum FilterAxis {
    State,
    Role,
    Kind,
}

impl FilterAxis {
    /// Section heading in the filter menu.
    pub fn label(self) -> &'static str {
        match self {
            FilterAxis::State => "State",
            FilterAxis::Role => "Role",
            FilterAxis::Kind => "Kind",
        }
    }
}

/// One toggleable predicate over a workspace. Variants are grouped by
/// their [`FilterAxis`]; [`Filter::ALL`] lists them in menu order.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub enum Filter {
    // ── State ──────────────────────────────────────────────────────
    /// Workspace has a coding-agent session. Matches on the recorded
    /// session (same source as the `]]<digit>` agent-jump list), not
    /// on a currently-live PTY — a workspace with a dormant agent
    /// session still matches even when its runner badge is dark.
    WithAgent,
    /// Primary task's CI is failing or mixed.
    CiFailing,
    /// Primary task's CI is queued or running.
    CiRunning,
    /// Primary task's branch conflicts with its base.
    Conflict,
    /// Workspace has unread activity.
    Unread,
    /// An agent in this workspace is waiting on input.
    Asking,
    /// A reviewer is requested, or a review is pending / changes-requested.
    ReviewRequested,
    /// Auto-merge is armed on the PR.
    AutoMerge,
    /// Primary task is a draft PR (or a Linear issue in a draft state).
    Draft,
    /// Primary task is actively being worked (in-progress / in-review).
    InProgress,
    /// The primary task is waiting on a reply from me (`needs_reply`).
    NeedsReply,
    /// The PR's head branch is behind its base and can be updated.
    BehindBase,
    /// A large diff — at least [`BIG_DIFF_LINES`] lines changed.
    BigDiff,
    // ── Role ───────────────────────────────────────────────────────
    Author,
    Reviewer,
    Assignee,
    Mentioned,
    // ── Kind ───────────────────────────────────────────────────────
    Pr,
    Issue,
}

impl Filter {
    /// Every filter, in menu order (State, then Role, then Kind).
    pub const ALL: [Filter; 19] = [
        Filter::WithAgent,
        Filter::CiFailing,
        Filter::CiRunning,
        Filter::Conflict,
        Filter::Unread,
        Filter::Asking,
        Filter::ReviewRequested,
        Filter::AutoMerge,
        Filter::Draft,
        Filter::InProgress,
        Filter::NeedsReply,
        Filter::BehindBase,
        Filter::BigDiff,
        Filter::Author,
        Filter::Reviewer,
        Filter::Assignee,
        Filter::Mentioned,
        Filter::Pr,
        Filter::Issue,
    ];

    pub fn axis(self) -> FilterAxis {
        match self {
            Filter::WithAgent
            | Filter::CiFailing
            | Filter::CiRunning
            | Filter::Conflict
            | Filter::Unread
            | Filter::Asking
            | Filter::ReviewRequested
            | Filter::AutoMerge
            | Filter::Draft
            | Filter::InProgress
            | Filter::NeedsReply
            | Filter::BehindBase
            | Filter::BigDiff => FilterAxis::State,
            Filter::Author | Filter::Reviewer | Filter::Assignee | Filter::Mentioned => {
                FilterAxis::Role
            }
            Filter::Pr | Filter::Issue => FilterAxis::Kind,
        }
    }

    /// Short label — used as the header chip and the menu row.
    pub fn label(self) -> &'static str {
        match self {
            Filter::WithAgent => "with-agent",
            Filter::CiFailing => "ci-failing",
            Filter::CiRunning => "ci-running",
            Filter::Conflict => "conflict",
            Filter::Unread => "unread",
            Filter::Asking => "asking",
            Filter::ReviewRequested => "review-requested",
            Filter::AutoMerge => "auto-merge",
            Filter::Draft => "draft",
            Filter::InProgress => "in-progress",
            Filter::NeedsReply => "needs-reply",
            Filter::BehindBase => "behind-base",
            Filter::BigDiff => "big-diff",
            Filter::Author => "author",
            Filter::Reviewer => "reviewer",
            Filter::Assignee => "assignee",
            Filter::Mentioned => "mentioned",
            Filter::Pr => "PR",
            Filter::Issue => "issue",
        }
    }

    /// Does `ctx`'s workspace satisfy this predicate?
    pub fn matches(self, ctx: &FilterCtx<'_>) -> bool {
        let w = ctx.w;
        let task = w.primary_task();
        match self {
            Filter::WithAgent => w
                .sessions
                .iter()
                .any(|s| matches!(s.kind, lazybox_core::SessionKind::Agent { .. })),
            Filter::CiFailing => {
                task.is_some_and(|t| matches!(t.ci, CiStatus::Failure | CiStatus::Mixed))
            }
            Filter::CiRunning => {
                task.is_some_and(|t| matches!(t.ci, CiStatus::Pending | CiStatus::Running))
            }
            Filter::Conflict => task.is_some_and(|t| t.mergeable.is_conflicting()),
            Filter::Unread => w.unread_count() > 0,
            Filter::Asking => crate::agent_attention::workspace_is_asking(w, ctx.agents),
            Filter::ReviewRequested => task.is_some_and(|t| {
                matches!(
                    t.review,
                    ReviewStatus::Pending | ReviewStatus::ChangesRequested
                ) || !t.reviewers.is_empty()
            }),
            Filter::AutoMerge => task.is_some_and(|t| t.auto_merge_enabled),
            Filter::Draft => task.is_some_and(|t| t.state == TaskState::Draft),
            Filter::InProgress => {
                task.is_some_and(|t| matches!(t.state, TaskState::InProgress | TaskState::InReview))
            }
            Filter::NeedsReply => task.is_some_and(|t| t.needs_reply),
            Filter::BehindBase => task.is_some_and(|t| t.is_behind_base),
            Filter::BigDiff => task.is_some_and(|t| t.additions + t.deletions >= BIG_DIFF_LINES),
            Filter::Author => task.is_some_and(|t| t.role == TaskRole::Author),
            Filter::Reviewer => task.is_some_and(|t| t.role == TaskRole::Reviewer),
            Filter::Assignee => task.is_some_and(|t| t.role == TaskRole::Assignee),
            Filter::Mentioned => task.is_some_and(|t| t.role == TaskRole::Mentioned),
            Filter::Pr => WorkspaceKind::classify(w) == WorkspaceKind::Pr,
            Filter::Issue => WorkspaceKind::classify(w) == WorkspaceKind::Issue,
        }
    }
}

/// Everything [`Filter::matches`] needs beyond the workspace itself:
/// the sidebar-local agent-state map (for the `asking` predicate).
/// Bundled so the predicate stays a pure function.
pub struct FilterCtx<'a> {
    pub w: &'a Workspace,
    pub agents: &'a HashMap<SessionKey, lazybox_ipc::AgentState>,
}

/// One row of the filter menu, carrying everything a non-TUI client
/// needs to draw it without re-deriving any predicate metadata: the
/// [`Filter`] itself, its [`FilterAxis`] (for the State/Role/Kind
/// grouping), the human label, how many of the candidate workspaces
/// match this predicate, and whether it's currently active. Built by
/// [`Filter::menu`] in [`Filter::ALL`] order so the desktop can group
/// by axis with a single linear pass.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct FilterMenuItem {
    pub filter: Filter,
    pub axis: FilterAxis,
    pub label: String,
    pub count: u32,
    pub active: bool,
}

impl Filter {
    /// The full filter menu in [`Filter::ALL`] order: every predicate
    /// with its axis, label, per-filter match count over `candidates`,
    /// and active flag. `candidates` are the workspaces the current
    /// mailbox admits *before* the active set narrows further — the
    /// count answers "what would this toggle surface", matching the
    /// TUI's `filter_counts`. Both clients build their menu from this,
    /// so the 14 predicates and their grouping live in one place.
    pub fn menu(
        candidates: &[&Workspace],
        agents: &HashMap<SessionKey, lazybox_ipc::AgentState>,
        active: &FilterSet,
    ) -> Vec<FilterMenuItem> {
        Filter::ALL
            .into_iter()
            .map(|filter| {
                let count = candidates
                    .iter()
                    .filter(|w| filter.matches(&FilterCtx { w, agents }))
                    .count() as u32;
                FilterMenuItem {
                    filter,
                    axis: filter.axis(),
                    label: filter.label().to_string(),
                    count,
                    active: active.active.contains(&filter),
                }
            })
            .collect()
    }
}

/// The active set of filters. Empty (the default) is a no-op that
/// accepts every workspace.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FilterSet {
    active: BTreeSet<Filter>,
}

impl FilterSet {
    /// An empty (no-op) filter set. `const` so it can seed a shared
    /// default without a heap allocation.
    pub const fn new() -> Self {
        Self {
            active: BTreeSet::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.active.is_empty()
    }

    pub fn toggle(&mut self, f: Filter) {
        if !self.active.insert(f) {
            self.active.remove(&f);
        }
    }

    /// Replace the whole set with `filters` (an empty iterator clears).
    pub fn replace(&mut self, filters: impl IntoIterator<Item = Filter>) {
        self.active = filters.into_iter().collect();
    }

    /// Active filters in [`Filter::ALL`] (menu) order.
    pub fn iter(&self) -> impl Iterator<Item = Filter> + '_ {
        Filter::ALL.into_iter().filter(|f| self.active.contains(f))
    }

    /// The header chips for the active filters, in menu order.
    pub fn chips(&self) -> Vec<&'static str> {
        self.iter().map(|f| f.label()).collect()
    }

    /// Does `ctx`'s workspace pass the active set? Empty = accept all.
    /// Within an axis the active filters OR; across axes they AND.
    pub fn accepts(&self, ctx: &FilterCtx<'_>) -> bool {
        if self.active.is_empty() {
            return true;
        }
        for axis in [FilterAxis::State, FilterAxis::Role, FilterAxis::Kind] {
            let mut present = false;
            let mut matched = false;
            for f in self.active.iter().filter(|f| f.axis() == axis) {
                present = true;
                if f.matches(ctx) {
                    matched = true;
                    break;
                }
            }
            if present && !matched {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use lazybox_core::{
        CiStatus, Mergeable, ReviewStatus, Task, TaskId, TaskKind, TaskRole, TaskState, Workspace,
        WorkspaceKey,
    };

    fn now() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 4, 1, 12, 0, 0).unwrap()
    }

    fn workspace(key: &str, role: TaskRole, ci: CiStatus, kind: TaskKind) -> Workspace {
        let task = Task {
            id: TaskId {
                source: "github".into(),
                key: format!("owner/r#{key}"),
            },
            title: "t".into(),
            body: None,
            state: TaskState::Open,
            role,
            ci,
            review: ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: "x".into(),
            repo: Some("owner/r".into()),
            branch: Some("main".into()),
            base_branch: None,
            updated_at: now(),
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: Mergeable::Mergeable,
            is_behind_base: false,
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            kind: Some(kind),
            closes_issues: vec![],
        };
        let mut ws = Workspace::from_task(task, now());
        ws.key = WorkspaceKey(key.into());
        ws
    }

    /// Build a workspace from a task the caller tweaks — lets the new
    /// State predicates be exercised without a fixed fixture per field.
    fn workspace_with(key: &str, tweak: impl FnOnce(&mut Task)) -> Workspace {
        let mut task = Task {
            id: TaskId {
                source: "github".into(),
                key: format!("owner/r#{key}"),
            },
            title: "t".into(),
            body: None,
            state: TaskState::Open,
            role: TaskRole::Author,
            ci: CiStatus::None,
            review: ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: "x".into(),
            repo: Some("owner/r".into()),
            branch: Some("feature".into()),
            base_branch: Some("main".into()),
            updated_at: now(),
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: Mergeable::Mergeable,
            is_behind_base: false,
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            kind: Some(TaskKind::Pr),
            closes_issues: vec![],
        };
        tweak(&mut task);
        let mut ws = Workspace::from_task(task, now());
        ws.key = WorkspaceKey(key.into());
        ws
    }

    #[test]
    fn new_state_predicates_match_their_field() {
        let agents = HashMap::new();
        let matches = |ws: &Workspace, f: Filter| {
            f.matches(&FilterCtx {
                w: ws,
                agents: &agents,
            })
        };

        let draft = workspace_with("a", |t| t.state = TaskState::Draft);
        assert!(matches(&draft, Filter::Draft));
        assert!(!matches(&draft, Filter::InProgress));

        let in_review = workspace_with("b", |t| t.state = TaskState::InReview);
        assert!(matches(&in_review, Filter::InProgress));

        let needs_reply = workspace_with("c", |t| t.needs_reply = true);
        assert!(matches(&needs_reply, Filter::NeedsReply));

        let behind = workspace_with("d", |t| t.is_behind_base = true);
        assert!(matches(&behind, Filter::BehindBase));

        let big = workspace_with("e", |t| {
            t.additions = BIG_DIFF_LINES;
            t.deletions = 0;
        });
        assert!(matches(&big, Filter::BigDiff));
        let small = workspace_with("f", |t| t.additions = BIG_DIFF_LINES - 1);
        assert!(!matches(&small, Filter::BigDiff));

        // All new predicates live on the State axis.
        for f in [
            Filter::Draft,
            Filter::InProgress,
            Filter::NeedsReply,
            Filter::BehindBase,
            Filter::BigDiff,
        ] {
            assert_eq!(f.axis(), FilterAxis::State);
        }
    }

    #[test]
    fn menu_lists_every_filter_in_axis_order_with_counts() {
        let agents = HashMap::new();
        let a = workspace("a", TaskRole::Author, CiStatus::Failure, TaskKind::Pr);
        let b = workspace("b", TaskRole::Reviewer, CiStatus::Success, TaskKind::Issue);
        let candidates = vec![&a, &b];
        let menu = Filter::menu(&candidates, &agents, &FilterSet::new());

        // All 14, in ALL order, none active.
        assert_eq!(menu.len(), Filter::ALL.len());
        assert!(menu.iter().all(|item| !item.active));
        assert_eq!(menu.first().map(|i| i.filter), Some(Filter::WithAgent));
        assert_eq!(menu[0].axis, FilterAxis::State);

        let count = |f: Filter| menu.iter().find(|i| i.filter == f).map(|i| i.count);
        assert_eq!(count(Filter::CiFailing), Some(1));
        assert_eq!(count(Filter::Author), Some(1));
        assert_eq!(count(Filter::Reviewer), Some(1));
        assert_eq!(count(Filter::Pr), Some(1));
        assert_eq!(count(Filter::Issue), Some(1));
    }

    #[test]
    fn menu_marks_active_filters() {
        let agents = HashMap::new();
        let a = workspace("a", TaskRole::Author, CiStatus::Success, TaskKind::Pr);
        let mut active = FilterSet::new();
        active.toggle(Filter::Author);
        let menu = Filter::menu(&[&a], &agents, &active);
        let author = menu.iter().find(|i| i.filter == Filter::Author).unwrap();
        assert!(author.active);
        assert!(
            menu.iter()
                .filter(|i| i.filter != Filter::Author)
                .all(|i| !i.active)
        );
    }

    /// Within an axis filters OR; across axes they AND. Author-OR-Reviewer
    /// keeps either role, but adding the PR kind axis drops the issue.
    #[test]
    fn within_axis_is_or_across_axes_is_and() {
        let agents = HashMap::new();
        let author_pr = workspace("a", TaskRole::Author, CiStatus::Success, TaskKind::Pr);
        let reviewer_issue = workspace("b", TaskRole::Reviewer, CiStatus::Success, TaskKind::Issue);

        let mut roles = FilterSet::new();
        roles.toggle(Filter::Author);
        roles.toggle(Filter::Reviewer);
        assert!(roles.accepts(&FilterCtx {
            w: &author_pr,
            agents: &agents
        }));
        assert!(roles.accepts(&FilterCtx {
            w: &reviewer_issue,
            agents: &agents
        }));

        roles.toggle(Filter::Pr);
        assert!(roles.accepts(&FilterCtx {
            w: &author_pr,
            agents: &agents
        }));
        assert!(
            !roles.accepts(&FilterCtx {
                w: &reviewer_issue,
                agents: &agents
            }),
            "PR-kind axis ANDs, so the reviewer issue is filtered out"
        );
    }

    #[test]
    fn chips_are_active_labels_in_menu_order() {
        let mut set = FilterSet::new();
        set.toggle(Filter::Issue);
        set.toggle(Filter::CiFailing);
        // Insertion order was Issue then CiFailing, but chips follow ALL order.
        assert_eq!(set.chips(), vec!["ci-failing", "issue"]);
    }
}
