//! Generic sidebar filter model.
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

use lazybox_core::{CiStatus, ReviewStatus, SessionKey, TaskRole, Workspace};
use std::collections::BTreeSet;
use std::collections::HashMap;

use crate::components::sidebar::WorkspaceKind;

/// The axis a [`Filter`] lives on. Drives the OR-within / AND-across
/// combination in [`FilterSet::accepts`] and groups the filter menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
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
    pub const ALL: [Filter; 14] = [
        Filter::WithAgent,
        Filter::CiFailing,
        Filter::CiRunning,
        Filter::Conflict,
        Filter::Unread,
        Filter::Asking,
        Filter::ReviewRequested,
        Filter::AutoMerge,
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
            | Filter::AutoMerge => FilterAxis::State,
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

/// The active set of filters. Empty (the default) is a no-op that
/// accepts every workspace.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
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
