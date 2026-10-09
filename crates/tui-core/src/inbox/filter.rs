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

use lazybox_core::{CiStatus, Priority, ReviewStatus, SessionKey, TaskRole, TaskState, Workspace};
use std::collections::BTreeSet;
use std::collections::HashMap;

use super::WorkspaceKind;

/// Threshold (additions + deletions) at or above which a PR counts as a
/// "big diff" for the [`Filter::BigDiff`] predicate.
pub const BIG_DIFF_LINES: u32 = 500;

/// How recent a touch keeps a workspace [`Filter::InFlight`]: long enough
/// to span a meeting or a lunch without the working set emptying, short
/// enough that yesterday's work has dropped out by morning.
pub const IN_FLIGHT_WINDOW: chrono::Duration = chrono::Duration::hours(1);

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
    Priority,
    Label,
    LinearState,
    Person,
}

impl FilterAxis {
    /// Section heading in the filter menu.
    pub fn label(self) -> &'static str {
        match self {
            FilterAxis::State => "State",
            FilterAxis::Role => "Role",
            FilterAxis::Kind => "Kind",
            FilterAxis::Priority => "Priority",
            FilterAxis::Label => "Label",
            FilterAxis::LinearState => "Linear state",
            FilterAxis::Person => "People",
        }
    }
}

/// Does `login` have any relationship to this task? The union GitHub's
/// `involves:` qualifier should have been: author ∨ requested reviewer
/// ∨ submitted reviewer ∨ assignee — one person-token answers
/// "everything alice touches", with review-requested included (the
/// role GitHub's own `involves:` famously misses). Case-insensitive,
/// matching GitHub login semantics.
pub fn task_involves(task: &lazybox_core::Task, login: &str) -> bool {
    task.author.eq_ignore_ascii_case(login)
        || task.reviewers.iter().any(|r| r.eq_ignore_ascii_case(login))
        || task
            .reviews
            .iter()
            .any(|r| r.login.eq_ignore_ascii_case(login))
        || task.assignees.iter().any(|a| a.eq_ignore_ascii_case(login))
}

/// Normalized matching form for a filter name: lowercased with every
/// non-alphanumeric character dropped. Applied to both sides of the
/// `f` menu's typeahead so punctuation never decides whether a filter
/// is findable — `rate-limited`, `rate limited`, `RateLimited` and
/// `ratelimited` are one key, and a query typed without the hyphen
/// still lands on `needs-recovery`.
pub fn search_key(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
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
    /// A coding agent in this workspace is *currently working* — a LIVE
    /// PTY reading `AgentState::Working`, not merely a recorded session
    /// (that's `WithAgent`). Same live source as the sidebar runner
    /// spinner and the `Shift-F`/attention predicates, so it never reads
    /// 0 while an agent is actually running.
    AgentWorking,
    /// Primary GitHub task has at least one active local or remote fleet
    /// claim (including a conservatively preserved legacy claim).
    Claimed,
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
    /// An agent in this workspace stopped in a shape one of the two
    /// recovery actions restarts: a provider usage / rate limit (#847),
    /// parked or alerting, or a turn that died on an infrastructure
    /// failure (`Stalled`, #1782). Named for the limit it originally
    /// covered; the label reads `needs-recovery` because it is now both.
    ///
    /// The label was `rate-limited` before that widening, and the old
    /// word is kept findable by [`Filter::search_aliases`] — renaming
    /// a label cost a reported capability its discoverability once
    /// (#1914), so the alias, not the rename, is where the bridge
    /// lives.
    RateLimited,
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
    /// A large diff — at least `BIG_DIFF_LINES` lines changed.
    BigDiff,
    /// Currently snoozed (`snoozed_until` in the future). The snoozed
    /// lens (#scale): toggling it in the `f` menu shows snoozed rows in
    /// place — `compute_visible` admits them into the Inbox while this
    /// filter is active — with their wake time, so reverting a snooze
    /// is `f` → snoozed → `z`, not a mailbox expedition.
    Snoozed,
    /// The primary task has at least one unmet dependency — a
    /// `blocked_by` edge (native GitHub/Linear relation or the
    /// `Blocked by:` / `Depends on:` body marker) or a declared
    /// `Blocked on:` reason. This is the same signal the `⊗` row badge
    /// reads; "what is waiting on something else" is one toggle.
    Blocked,
    /// An issue that can be started right now: it is an issue (not a PR),
    /// carries no dependency edge or declared blocker, has no agent
    /// session yet, and is not snoozed (a snoozed issue was deliberately
    /// deferred, so it is not something to pick up now). The direct
    /// complement of "what's blocked" — "what can I pick up" without
    /// reading every row.
    Ready,
    /// What you are juggling right now: an agent here is working or
    /// waiting on you (or just finished a turn you have not looked at),
    /// or within the last hour (`IN_FLIGHT_WINDOW`) you marked
    /// it read or your own PR / issue moved (a push, CI, a review, a
    /// comment). "In flight" rather than "active" or "recent": it is the
    /// set of things currently in the air, which is what someone running
    /// many projects at once has to keep track of.
    InFlight,
    // ── Role ───────────────────────────────────────────────────────
    Author,
    Reviewer,
    Assignee,
    Mentioned,
    /// No relationship to you at all — a row a watched repo or a
    /// repo sync pulled in. Completes the axis so the four "mine"
    /// predicates together are the complement of this one.
    Observer,
    // ── Kind ───────────────────────────────────────────────────────
    Pr,
    Issue,
    // ── Priority (Linear) ──────────────────────────────────────────
    PriorityUrgent,
    PriorityHigh,
    PriorityMedium,
    PriorityLow,
}

impl Filter {
    /// Every fixed filter, in menu order (State, Role, Kind, Priority).
    /// Value-driven axes (Label, Linear state) are enumerated separately
    /// from the candidate set — see [`FilterSet`] and `Sidebar`.
    pub const ALL: [Filter; 31] = [
        Filter::WithAgent,
        Filter::AgentWorking,
        Filter::Claimed,
        Filter::CiFailing,
        Filter::CiRunning,
        Filter::Conflict,
        Filter::Unread,
        Filter::Asking,
        Filter::RateLimited,
        Filter::ReviewRequested,
        Filter::AutoMerge,
        Filter::Draft,
        Filter::InProgress,
        Filter::NeedsReply,
        Filter::BehindBase,
        Filter::BigDiff,
        Filter::Snoozed,
        Filter::Blocked,
        Filter::Ready,
        Filter::InFlight,
        Filter::Author,
        Filter::Reviewer,
        Filter::Assignee,
        Filter::Mentioned,
        Filter::Observer,
        Filter::Pr,
        Filter::Issue,
        Filter::PriorityUrgent,
        Filter::PriorityHigh,
        Filter::PriorityMedium,
        Filter::PriorityLow,
    ];

    pub fn axis(self) -> FilterAxis {
        match self {
            Filter::WithAgent
            | Filter::AgentWorking
            | Filter::Claimed
            | Filter::CiFailing
            | Filter::CiRunning
            | Filter::Conflict
            | Filter::Unread
            | Filter::Asking
            | Filter::RateLimited
            | Filter::ReviewRequested
            | Filter::AutoMerge
            | Filter::Draft
            | Filter::InProgress
            | Filter::NeedsReply
            | Filter::BehindBase
            | Filter::BigDiff
            | Filter::Snoozed
            | Filter::Blocked
            | Filter::Ready
            | Filter::InFlight => FilterAxis::State,
            Filter::Author
            | Filter::Reviewer
            | Filter::Assignee
            | Filter::Mentioned
            | Filter::Observer => FilterAxis::Role,
            Filter::Pr | Filter::Issue => FilterAxis::Kind,
            Filter::PriorityUrgent
            | Filter::PriorityHigh
            | Filter::PriorityMedium
            | Filter::PriorityLow => FilterAxis::Priority,
        }
    }

    /// The priority tier this predicate matches, if it is one.
    fn priority(self) -> Option<Priority> {
        match self {
            Filter::PriorityUrgent => Some(Priority::Urgent),
            Filter::PriorityHigh => Some(Priority::High),
            Filter::PriorityMedium => Some(Priority::Medium),
            Filter::PriorityLow => Some(Priority::Low),
            _ => None,
        }
    }

    /// Short label — used as the header chip and the menu row.
    pub fn label(self) -> &'static str {
        match self {
            Filter::WithAgent => "with-agent",
            Filter::AgentWorking => "working",
            Filter::Claimed => "claimed",
            Filter::CiFailing => "ci-failing",
            Filter::CiRunning => "ci-running",
            Filter::Conflict => "conflict",
            Filter::Unread => "unread",
            Filter::Asking => "asking",
            Filter::RateLimited => "needs-recovery",
            Filter::ReviewRequested => "review-requested",
            Filter::AutoMerge => "auto-merge",
            Filter::Draft => "draft",
            Filter::InProgress => "in-progress",
            Filter::NeedsReply => "needs-reply",
            Filter::BehindBase => "behind-base",
            Filter::BigDiff => "big-diff",
            Filter::Snoozed => "snoozed",
            Filter::Blocked => "blocked",
            Filter::Ready => "ready",
            Filter::InFlight => "in-flight",
            Filter::Author => "author",
            Filter::Reviewer => "reviewer",
            Filter::Assignee => "assignee",
            Filter::Mentioned => "mentioned",
            Filter::Observer => "observer",
            Filter::Pr => "PR",
            Filter::Issue => "issue",
            Filter::PriorityUrgent => "urgent",
            Filter::PriorityHigh => "high",
            Filter::PriorityMedium => "medium",
            Filter::PriorityLow => "low",
        }
    }

    /// Extra words that must find this filter when typing in the `f`
    /// menu, beyond [`Filter::label`]. Matching is substring over the
    /// normalized form (see [`search_key`]), so a label already covers
    /// every prefix and infix of itself — an alias earns its place only
    /// by carrying a word the label does *not* contain.
    ///
    /// The rule each entry below is held to: the alias appears in this
    /// variant's own doc comment, in the identifiers its predicate
    /// reads, or in the words the UI prints for the states it covers.
    /// That keeps the list a record of vocabulary the product already
    /// uses rather than a thesaurus, and it is why a renamed label
    /// stops costing discoverability: the old term stays here.
    pub fn search_aliases(self) -> &'static [&'static str] {
        match self {
            // "recorded session … not a currently-live PTY" — someone
            // reaching for the session rather than the agent.
            Filter::WithAgent => &["session"],
            // The variant's own name, and the sidebar runner spinner
            // this predicate shares its source with.
            Filter::AgentWorking => &["agent-working", "running"],
            // Deliberately no `working` alias: that is
            // `AgentWorking`'s canonical label, even though GitHub's
            // claim label is literally `working` / `lazybox:w:…`.
            // Aliasing it would make one query mean two filters.
            Filter::Claimed => &[],
            // The predicate is `Failure | Mixed`; the label names only
            // the first.
            Filter::CiFailing => &["ci-failure", "ci-mixed", "ci-red"],
            // The predicate is `Pending | Running` — "queued" is the
            // word the doc comment uses for `Pending`.
            Filter::CiRunning => &["ci-pending", "ci-queued"],
            // `t.mergeable.is_conflicting()`: the code's own word is
            // the participle, which is not a substring of `conflict`.
            Filter::Conflict => &["conflicting", "merge-conflict"],
            Filter::Unread => &[],
            // `AgentState::InputNeeded`, printed as `! needs input`.
            Filter::Asking => &["input-needed", "needs-input"],
            // RENAMED. This was `rate-limited` until the predicate grew
            // past rate-limiting (see the variant's doc comment); the
            // old term is listed first and is the reason this whole
            // alias mechanism exists (#1914). The rest are the three
            // `AgentState`s `workspace_needs_recovery` matches and the
            // words the UI prints for them — `⧗ limited`,
            // `☾ waiting`, `↯ stalled` — plus "stopped", which is how
            // both recovery actions label themselves ("resume / restart
            // stopped agents").
            Filter::RateLimited => &[
                "rate-limited",
                "limit-reached",
                "awaiting-reset",
                "parked",
                "stalled",
                "stopped",
            ],
            // The predicate also admits `ChangesRequested`.
            Filter::ReviewRequested => &["changes-requested"],
            // The policy modal and the row pill call this state
            // "armed" (`● armed · ○ off`, the `ARM` pill).
            Filter::AutoMerge => &["armed"],
            Filter::Draft => &[],
            // The predicate is `InProgress | InReview`; the label names
            // only the first, so `in-review` found nothing.
            Filter::InProgress => &["in-review"],
            Filter::NeedsReply => &[],
            // `g u` is labelled "update branch" — the action a user
            // wants when they go looking for this filter.
            Filter::BehindBase => &["update-branch"],
            Filter::BigDiff => &[],
            // `Filter::Ready`'s doc comment: "a snoozed issue was
            // deliberately deferred".
            Filter::Snoozed => &["deferred"],
            // The doc comment's own vocabulary for the edges this
            // reads: `blocked_by`, `Blocked by:` / `Depends on:`.
            Filter::Blocked => &["blocked-by", "depends-on", "dependency"],
            // The doc comment: "The direct complement of 'what's
            // blocked' — 'what can I pick up'".
            Filter::Ready => &["unblocked", "pick-up"],
            // The doc comment names the two words it was chosen over:
            // "'In flight' rather than 'active' or 'recent'".
            Filter::InFlight => &["active", "recent"],
            Filter::Author
            | Filter::Reviewer
            | Filter::Assignee
            | Filter::Mentioned
            | Filter::Observer
            | Filter::Pr
            | Filter::Issue
            | Filter::PriorityUrgent
            | Filter::PriorityHigh
            | Filter::PriorityMedium
            | Filter::PriorityLow => &[],
        }
    }

    /// Does a typed `query` find this filter? Substring over the label
    /// and every [`Filter::search_aliases`] entry, both normalized by
    /// [`search_key`] — so `rate-limited`, `ratelimited`, `Rate Limited`
    /// and a bare `limit` all reach `needs-recovery`. An empty query
    /// matches everything (typing nothing hides nothing).
    pub fn matches_search(self, query: &str) -> bool {
        let q = search_key(query);
        if q.is_empty() {
            return true;
        }
        std::iter::once(self.label())
            .chain(self.search_aliases().iter().copied())
            .any(|key| search_key(key).contains(&q))
    }

    /// Exact (normalized) name lookup: the label or one of the
    /// aliases, whole. Used by [`FilterEntry::from_token`], which must
    /// not resolve a partial word to a filter the way the typeahead
    /// does.
    fn by_exact_name(token: &str) -> Option<Filter> {
        let t = search_key(token);
        if t.is_empty() {
            return None;
        }
        Filter::ALL.into_iter().find(|f| {
            std::iter::once(f.label())
                .chain(f.search_aliases().iter().copied())
                .any(|key| search_key(key) == t)
        })
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
            Filter::AgentWorking => crate::agent_attention::workspace_is_working(w, ctx.agents),
            Filter::Claimed => w.is_claimed(),
            Filter::CiFailing => {
                task.is_some_and(|t| matches!(t.ci, CiStatus::Failure | CiStatus::Mixed))
            }
            Filter::CiRunning => {
                task.is_some_and(|t| matches!(t.ci, CiStatus::Pending | CiStatus::Running))
            }
            Filter::Conflict => task.is_some_and(|t| t.mergeable.is_conflicting()),
            Filter::Unread => w.unread_count() > 0,
            Filter::Asking => crate::agent_attention::workspace_is_asking(w, ctx.agents),
            // Every stopped shape the recovery actions reach: the alerting
            // `⧗ LimitReached` block, the parked `☾ AwaitingReset` wait, and
            // the `↯ Stalled` infrastructure failure. Counting only the first
            // showed `(0)` over eight parked agents.
            Filter::RateLimited => crate::agent_attention::workspace_needs_recovery(w, ctx.agents),
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
            Filter::Snoozed => w.is_snoozed(ctx.now),
            // Blocked / Ready read the workspace-level dependency helpers
            // (not just `primary_task`) so a ticket that has acquired a PR
            // keeps its edges — the same reason `hierarchy_blocked_by`
            // exists. `Blocked` counts a declared `Blocked on:` reason too.
            Filter::Blocked => {
                w.hierarchy_blocked_by().next().is_some() || w.declared_blocker().is_some()
            }
            Filter::Ready => {
                w.hierarchy_blocked_by().next().is_none()
                    && w.declared_blocker().is_none()
                    && task.is_some_and(|t| matches!(t.kind, Some(lazybox_core::TaskKind::Issue)))
                    && !w
                        .sessions
                        .iter()
                        .any(|s| matches!(s.kind, lazybox_core::SessionKind::Agent { .. }))
                    // A snoozed issue was deliberately deferred: it lives in
                    // the Snoozed mailbox, not the working set, so it is not
                    // "ready to pick up now" even when unblocked and
                    // agent-less. Without this, `ready` and `snoozed`
                    // overlap and a deferred ticket keeps resurfacing.
                    && !w.is_snoozed(ctx.now)
            }
            Filter::InFlight => {
                let since = ctx.now - IN_FLIGHT_WINDOW;
                crate::agent_attention::workspace_is_working(w, ctx.agents)
                    || crate::agent_attention::workspace_is_asking(w, ctx.agents)
                    || crate::agent_attention::workspace_is_done(w, ctx.agents)
                    || w.last_viewed_at.is_some_and(|t| t >= since)
                    || task.is_some_and(|t| {
                        matches!(t.role, TaskRole::Author | TaskRole::Assignee)
                            && t.updated_at >= since
                    })
            }
            Filter::Author => task.is_some_and(|t| t.role == TaskRole::Author),
            Filter::Reviewer => task.is_some_and(|t| t.role == TaskRole::Reviewer),
            Filter::Assignee => task.is_some_and(|t| t.role == TaskRole::Assignee),
            Filter::Mentioned => task.is_some_and(|t| t.role == TaskRole::Mentioned),
            Filter::Observer => task.is_some_and(|t| t.role == TaskRole::Observer),
            Filter::Pr => WorkspaceKind::classify(w) == WorkspaceKind::Pr,
            Filter::Issue => WorkspaceKind::classify(w) == WorkspaceKind::Issue,
            Filter::PriorityUrgent
            | Filter::PriorityHigh
            | Filter::PriorityMedium
            | Filter::PriorityLow => task.is_some_and(|t| t.priority == self.priority()),
        }
    }
}

/// Everything [`Filter::matches`] needs beyond the workspace itself:
/// the sidebar-local agent-state map (for the `asking` predicate) and
/// the evaluation clock (for the `snoozed` predicate — an EXPIRED
/// `snoozed_until` must read as awake, and stale timestamps are never
/// cleared in the store). Bundled so the predicate stays a pure
/// function.
pub struct FilterCtx<'a> {
    pub w: &'a Workspace,
    pub agents: &'a HashMap<SessionKey, lazybox_ipc::AgentState>,
    pub now: chrono::DateTime<chrono::Utc>,
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
        // Menu counts are display-only, so the wall clock here (rather
        // than a caller-supplied instant) can't skew anything that
        // persists — and it keeps the desktop's call site unchanged.
        let now = chrono::Utc::now();
        Filter::ALL
            .into_iter()
            .map(|filter| {
                let count = candidates
                    .iter()
                    .filter(|w| filter.matches(&FilterCtx { w, agents, now }))
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

/// One selectable row in the `f` filter menu. A fixed predicate, or a
/// value-driven label / Linear-state row whose set of values is
/// discovered from the current inbox rather than hard-coded. Used as the
/// picker's item type so all axes live in one multi-select.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum FilterEntry {
    Predicate(Filter),
    Label(String),
    LinearState(String),
    /// A login on the People axis — matched with the
    /// [`task_involves`] role-union (author ∨ reviewer ∨ assignee).
    Person(String),
}

impl FilterEntry {
    pub fn axis(&self) -> FilterAxis {
        match self {
            FilterEntry::Predicate(f) => f.axis(),
            FilterEntry::Label(_) => FilterAxis::Label,
            FilterEntry::LinearState(_) => FilterAxis::LinearState,
            FilterEntry::Person(_) => FilterAxis::Person,
        }
    }

    /// Row label shown in the menu / header chip.
    pub fn label(&self) -> String {
        match self {
            FilterEntry::Predicate(f) => f.label().to_string(),
            FilterEntry::Label(name) => name.clone(),
            FilterEntry::LinearState(name) => name.clone(),
            FilterEntry::Person(login) => format!("@{login}"),
        }
    }

    /// Does a typed `query` find this row in the `f` menu? Predicates
    /// match their label *or* any of [`Filter::search_aliases`] (so the
    /// pre-rename `rate-limited` reaches `needs-recovery`); the
    /// value-driven axes match their own text, which is the only name
    /// they have. An empty query matches every row.
    pub fn matches_search(&self, query: &str) -> bool {
        match self {
            FilterEntry::Predicate(f) => f.matches_search(query),
            FilterEntry::Label(name)
            | FilterEntry::LinearState(name)
            | FilterEntry::Person(name) => {
                let q = search_key(query);
                q.is_empty() || search_key(name).contains(&q)
            }
        }
    }

    /// Stable string token for persisting this entry in the config's
    /// `ui.last_lens` (which must stay free of UI-crate types).
    /// Predicates use their label verbatim; value axes carry a prefix
    /// so a label literally named "unread" can't be confused with the
    /// predicate. Round-trips through [`Self::from_token`].
    pub fn to_token(&self) -> String {
        match self {
            FilterEntry::Predicate(f) => f.label().to_string(),
            FilterEntry::Label(name) => format!("label:{name}"),
            FilterEntry::LinearState(name) => format!("linear-state:{name}"),
            FilterEntry::Person(login) => format!("person:{login}"),
        }
    }

    /// Parse a persisted lens token. `None` for anything unrecognized —
    /// a stale token (predicate renamed, hand-edited config) must
    /// degrade to "that filter is gone", never wedge startup.
    pub fn from_token(token: &str) -> Option<Self> {
        if let Some(name) = token.strip_prefix("label:") {
            return Some(FilterEntry::Label(name.to_string()));
        }
        if let Some(name) = token.strip_prefix("linear-state:") {
            return Some(FilterEntry::LinearState(name.to_string()));
        }
        if let Some(login) = token.strip_prefix("person:") {
            // Normalize to the case-insensitive identity (see
            // `FilterSet::replace_entries`) so a hand-edited `person:Alice`
            // token collapses onto the discovered `alice`, not a phantom
            // second row.
            return Some(FilterEntry::Person(login.to_ascii_lowercase()));
        }
        // Exact label first, then the alias table — so a hand-edited
        // `rate-limited` (or a `PR` typed in lower case) resolves to the
        // predicate it names instead of reading as a filter that is
        // gone. The lens is re-persisted through `to_token`, which always
        // writes the canonical label, exactly as the `person:Alice`
        // normalization above collapses onto the discovered login.
        Filter::ALL
            .into_iter()
            .find(|f| f.label() == token)
            .or_else(|| Filter::by_exact_name(token))
            .map(FilterEntry::Predicate)
    }
}

/// The active set of filters. Empty (the default) is a no-op that
/// accepts every workspace. Fixed predicates live in `active`; the
/// value-driven axes carry the selected label names and Linear-state
/// names.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FilterSet {
    active: BTreeSet<Filter>,
    #[serde(default)]
    labels: BTreeSet<String>,
    #[serde(default)]
    linear_states: BTreeSet<String>,
    #[serde(default)]
    people: BTreeSet<String>,
}

impl FilterSet {
    /// An empty (no-op) filter set. `const` so it can seed a shared
    /// default without a heap allocation.
    pub const fn new() -> Self {
        Self {
            active: BTreeSet::new(),
            labels: BTreeSet::new(),
            linear_states: BTreeSet::new(),
            people: BTreeSet::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.active.is_empty()
            && self.labels.is_empty()
            && self.linear_states.is_empty()
            && self.people.is_empty()
    }

    pub fn toggle(&mut self, f: Filter) {
        if !self.active.insert(f) {
            self.active.remove(&f);
        }
    }

    /// Whether one fixed predicate is active. `compute_visible` reads
    /// this for [`Filter::Snoozed`] to widen mailbox membership.
    pub fn has(&self, f: Filter) -> bool {
        self.active.contains(&f)
    }

    /// Replace the whole set with fixed `filters` (an empty iterator
    /// clears everything, including the value axes).
    pub fn replace(&mut self, filters: impl IntoIterator<Item = Filter>) {
        self.active = filters.into_iter().collect();
        self.labels.clear();
        self.linear_states.clear();
        self.people.clear();
    }

    /// Replace the whole set from menu entries (fixed predicates + label
    /// / Linear-state / person values). An empty iterator clears all axes.
    pub fn replace_entries(&mut self, entries: impl IntoIterator<Item = FilterEntry>) {
        self.active.clear();
        self.labels.clear();
        self.linear_states.clear();
        self.people.clear();
        for entry in entries {
            match entry {
                FilterEntry::Predicate(f) => {
                    self.active.insert(f);
                }
                FilterEntry::Label(name) => {
                    self.labels.insert(name);
                }
                FilterEntry::LinearState(name) => {
                    self.linear_states.insert(name);
                }
                FilterEntry::Person(login) => {
                    // A GitHub login is case-insensitive, so the People
                    // set's identity is the ASCII-lowercased login — the
                    // same normalization `task_involves` matches with. The
                    // menu discovery lowercases its bucket keys to match,
                    // so an active login and its discovered row are ONE
                    // entry regardless of the case a token or task field
                    // carried; without this a `person:Alice` token and a
                    // discovered `alice` render as two rows, the active one
                    // showing an unchecked, count-0 duplicate.
                    self.people.insert(login.to_ascii_lowercase());
                }
            }
        }
    }

    /// Number of active filters across every axis.
    pub fn len(&self) -> usize {
        self.active.len() + self.labels.len() + self.linear_states.len() + self.people.len()
    }

    /// Active fixed filters in [`Filter::ALL`] (menu) order.
    pub fn iter(&self) -> impl Iterator<Item = Filter> + '_ {
        Filter::ALL.into_iter().filter(|f| self.active.contains(f))
    }

    /// Selected label names (Label axis).
    pub fn labels(&self) -> &BTreeSet<String> {
        &self.labels
    }

    /// Selected Linear-state names (Linear-state axis).
    pub fn linear_states(&self) -> &BTreeSet<String> {
        &self.linear_states
    }

    /// Selected logins (People axis).
    pub fn people(&self) -> &BTreeSet<String> {
        &self.people
    }

    /// Whether `entry` is currently active — drives the menu's
    /// pre-checked rows.
    pub fn contains_entry(&self, entry: &FilterEntry) -> bool {
        match entry {
            FilterEntry::Predicate(f) => self.active.contains(f),
            FilterEntry::Label(name) => self.labels.contains(name),
            FilterEntry::LinearState(name) => self.linear_states.contains(name),
            FilterEntry::Person(login) => self.people.contains(&login.to_ascii_lowercase()),
        }
    }

    /// The header chips for the active filters, in menu order (fixed
    /// predicates first, then label / Linear-state / person values).
    pub fn chips(&self) -> Vec<String> {
        let mut chips: Vec<String> = self.iter().map(|f| f.label().to_string()).collect();
        chips.extend(self.labels.iter().cloned());
        chips.extend(self.linear_states.iter().cloned());
        chips.extend(self.people.iter().map(|login| format!("@{login}")));
        chips
    }

    /// Does `ctx`'s workspace pass the active set? Empty = accept all.
    /// Within an axis the active filters OR; across axes they AND.
    pub fn accepts(&self, ctx: &FilterCtx<'_>) -> bool {
        if self.is_empty() {
            return true;
        }
        for axis in [
            FilterAxis::State,
            FilterAxis::Role,
            FilterAxis::Kind,
            FilterAxis::Priority,
        ] {
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
        // Label axis: OR within — the primary task must carry at least
        // one of the selected labels.
        if !self.labels.is_empty() {
            let matched = ctx
                .w
                .primary_task()
                .is_some_and(|t| t.labels.iter().any(|l| self.labels.contains(&l.name)));
            if !matched {
                return false;
            }
        }
        // Linear-state axis: the primary task's native state name must be
        // one of the selected states.
        if !self.linear_states.is_empty() {
            let matched = ctx
                .w
                .primary_task()
                .and_then(|t| t.state_label.as_deref())
                .is_some_and(|s| self.linear_states.contains(s));
            if !matched {
                return false;
            }
        }
        // People axis: OR within — the primary task must involve at
        // least one selected login (author ∨ reviewer ∨ assignee, the
        // [`task_involves`] role-union).
        if !self.people.is_empty() {
            let matched = ctx
                .w
                .primary_task()
                .is_some_and(|t| self.people.iter().any(|login| task_involves(t, login)));
            if !matched {
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
            author: String::new(),
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
            reviews: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: Mergeable::Mergeable,
            is_behind_base: false,
            merge_blocked: false,
            approval_policy: Default::default(),
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            changed_files: 0,
            kind: Some(kind),
            closes_issues: vec![],
            linked_tasks: vec![],
            parent: None,
            priority: None,
            state_label: None,
            blocked_by: vec![],
            merge_after: vec![],
            contracts: vec![],
            blocked_on: None,
        };
        let mut ws = Workspace::from_task(task, now());
        ws.key = WorkspaceKey(key.into());
        ws
    }

    /// Build a workspace from a task the caller tweaks — lets the new
    /// State predicates be exercised without a fixed fixture per field.
    fn workspace_with(key: &str, tweak: impl FnOnce(&mut Task)) -> Workspace {
        let mut task = Task {
            author: String::new(),
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
            reviews: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: Mergeable::Mergeable,
            is_behind_base: false,
            merge_blocked: false,
            approval_policy: Default::default(),
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            changed_files: 0,
            kind: Some(TaskKind::Pr),
            closes_issues: vec![],
            linked_tasks: vec![],
            parent: None,
            priority: None,
            state_label: None,
            blocked_by: vec![],
            merge_after: vec![],
            contracts: vec![],
            blocked_on: None,
        };
        tweak(&mut task);
        let mut ws = Workspace::from_task(task, now());
        ws.key = WorkspaceKey(key.into());
        ws
    }

    #[test]
    fn agent_working_matches_live_working_state_not_recorded_sessions() {
        use lazybox_ipc::AgentState;
        let ws = workspace_with("owner/repo#1", |_| {});
        let sk = lazybox_core::SessionKey::from(&ws.key);

        // No live agent state → does not match (this is the case where the
        // recorded-session `WithAgent` filter reads 0 but a spinner is live).
        let empty = HashMap::new();
        assert!(!Filter::AgentWorking.matches(&FilterCtx {
            w: &ws,
            agents: &empty,
            now: now(),
        }));

        // A live agent reading `Working` → matches.
        let working = HashMap::from([(sk.clone(), AgentState::Working)]);
        assert!(Filter::AgentWorking.matches(&FilterCtx {
            w: &ws,
            agents: &working,
            now: now(),
        }));

        // Every other live state is NOT "working" (idle/done are handled by
        // their own indicators; asking has the `Asking` filter).
        for state in [AgentState::Idle, AgentState::Done, AgentState::InputNeeded] {
            let m = HashMap::from([(sk.clone(), state)]);
            assert!(
                !Filter::AgentWorking.matches(&FilterCtx {
                    w: &ws,
                    agents: &m,
                    now: now()
                }),
                "{state:?} must not match the working filter"
            );
        }
    }

    /// The `needs-recovery` axis counts every stopped shape the two
    /// recovery keys reach: an agent parked on the auto-continue wait
    /// (`AwaitingReset`) is as held as one alerting on the block
    /// (`LimitReached`), and so is one that stopped on an infrastructure
    /// failure (`Stalled`, #1782). Counting only the alerting limit showed
    /// `(0)` in the filter menu over eight parked agents.
    #[test]
    fn rate_limited_matches_parked_agents_as_well_as_blocked_ones() {
        use lazybox_ipc::AgentState;
        let ws = workspace_with("owner/repo#1", |_| {});
        let sk = lazybox_core::SessionKey::from(&ws.key);
        for state in [
            AgentState::LimitReached,
            AgentState::AwaitingReset,
            AgentState::Stalled,
        ] {
            let m = HashMap::from([(sk.clone(), state)]);
            assert!(
                Filter::RateLimited.matches(&FilterCtx {
                    w: &ws,
                    agents: &m,
                    now: now()
                }),
                "{state:?} needs recovery"
            );
        }
        for state in [
            AgentState::Working,
            AgentState::InputNeeded,
            AgentState::Done,
        ] {
            let m = HashMap::from([(sk.clone(), state)]);
            assert!(
                !Filter::RateLimited.matches(&FilterCtx {
                    w: &ws,
                    agents: &m,
                    now: now()
                }),
                "{state:?} is not rate-limited"
            );
        }
    }

    #[test]
    fn new_state_predicates_match_their_field() {
        let agents = HashMap::new();
        let matches = |ws: &Workspace, f: Filter| {
            f.matches(&FilterCtx {
                w: ws,
                agents: &agents,
                now: now(),
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
    fn claimed_filter_matches_the_stable_label_and_legacy_qualified_ownership() {
        let agents = HashMap::new();
        let mut claimed = workspace("claimed", TaskRole::Author, CiStatus::None, TaskKind::Issue);
        let label = lazybox_core::qualified_working_claim_label(
            "0123456789abcdef0123456789abcdef",
            lazybox_core::SessionId::default().0,
            Utc::now() + chrono::Duration::hours(1),
        )
        .unwrap();
        claimed
            .primary_task_mut()
            .unwrap()
            .labels
            .push(lazybox_core::Label::new(label));
        let unclaimed = workspace("plain", TaskRole::Author, CiStatus::None, TaskKind::Issue);

        assert!(Filter::Claimed.matches(&FilterCtx {
            w: &claimed,
            agents: &agents,
            now: now(),
        }));
        assert!(!Filter::Claimed.matches(&FilterCtx {
            w: &unclaimed,
            agents: &agents,
            now: now(),
        }));

        // The current shape (#1922): one stable `working` label, with the
        // holder and the lease in lazybox's claim comment. The filter reads
        // presence off the label, which is the half that rides free in the
        // poll payload — so the lens works with no GitHub request at all.
        let mut stable = workspace("stable", TaskRole::Author, CiStatus::None, TaskKind::Issue);
        stable
            .primary_task_mut()
            .unwrap()
            .labels
            .push(lazybox_core::Label::new(lazybox_core::WORKING_LABEL_NAME));
        assert!(Filter::Claimed.matches(&FilterCtx {
            w: &stable,
            agents: &agents,
            now: now(),
        }));
    }

    #[test]
    fn label_and_linear_state_axes_filter_by_value() {
        use lazybox_core::Label;
        let agents = HashMap::new();
        let accepts = |set: &FilterSet, ws: &Workspace| {
            set.accepts(&FilterCtx {
                w: ws,
                agents: &agents,
                now: now(),
            })
        };

        let bug = workspace_with("a", |t| {
            t.labels = vec![Label::new("bug"), Label::new("p1")];
            t.state_label = Some("In Review".into());
        });
        let chore = workspace_with("b", |t| {
            t.labels = vec![Label::new("chore")];
            t.state_label = Some("Todo".into());
        });

        // Label axis: OR within.
        let mut set = FilterSet::new();
        set.replace_entries([FilterEntry::Label("bug".into())]);
        assert!(accepts(&set, &bug));
        assert!(!accepts(&set, &chore));

        // Linear-state axis, AND-across with the label axis.
        set.replace_entries([
            FilterEntry::Label("bug".into()),
            FilterEntry::LinearState("Todo".into()),
        ]);
        // `bug` has label bug but state In Review (not Todo) → rejected.
        assert!(!accepts(&set, &bug));
        // `chore` has state Todo but not label bug → rejected.
        assert!(!accepts(&set, &chore));

        // chips reflect every axis; clearing resets all.
        set.replace_entries([
            FilterEntry::Predicate(Filter::Unread),
            FilterEntry::Label("bug".into()),
            FilterEntry::LinearState("In Review".into()),
        ]);
        assert_eq!(set.chips(), vec!["unread", "bug", "In Review"]);
        assert_eq!(set.len(), 3);
        set.replace_entries(std::iter::empty());
        assert!(set.is_empty());
    }

    /// People axis (#scale): a login matches through the
    /// `task_involves` role-union — author ∨ requested reviewer ∨
    /// submitted reviewer ∨ assignee — OR within the axis, AND against
    /// other axes, and case-insensitively (GitHub login semantics).
    #[test]
    fn person_axis_filters_by_role_union() {
        use lazybox_core::{ReviewState, Reviewer};
        let agents = HashMap::new();
        let accepts = |set: &FilterSet, ws: &Workspace| {
            set.accepts(&FilterCtx {
                w: ws,
                agents: &agents,
                now: now(),
            })
        };

        let authored = workspace_with("a", |t| t.author = "Alice".into());
        let requested = workspace_with("b", |t| t.reviewers = vec!["alice".into()]);
        let reviewed = workspace_with("c", |t| {
            t.reviews = vec![Reviewer {
                login: "alice".into(),
                state: ReviewState::Approved,
                is_bot: false,
            }];
        });
        let assigned = workspace_with("d", |t| t.assignees = vec!["alice".into()]);
        let uninvolved = workspace_with("e", |t| t.author = "bob".into());

        let mut set = FilterSet::new();
        set.replace_entries([FilterEntry::Person("alice".into())]);
        for ws in [&authored, &requested, &reviewed, &assigned] {
            assert!(accepts(&set, ws), "alice involves {:?}", ws.key);
        }
        assert!(!accepts(&set, &uninvolved));

        // OR within the axis: alice ∨ bob.
        set.replace_entries([
            FilterEntry::Person("alice".into()),
            FilterEntry::Person("bob".into()),
        ]);
        assert!(accepts(&set, &authored));
        assert!(accepts(&set, &uninvolved));

        // AND across axes: involves alice AND ci-failing.
        set.replace_entries([
            FilterEntry::Person("alice".into()),
            FilterEntry::Predicate(Filter::CiFailing),
        ]);
        assert!(!accepts(&set, &authored), "alice's row isn't ci-failing");
        let failing = workspace_with("f", |t| {
            t.author = "alice".into();
            t.ci = CiStatus::Failure;
        });
        assert!(accepts(&set, &failing));

        // Chips carry the @ prefix; clearing resets the axis.
        set.replace_entries([FilterEntry::Person("alice".into())]);
        assert_eq!(set.chips(), vec!["@alice"]);
        set.replace_entries(std::iter::empty());
        assert!(set.is_empty());
    }

    /// A GitHub login is case-insensitive, so the People axis's identity
    /// is the lowercased login. A mixed-case entry (a hand-edited
    /// `person:Alice` lens token round-tripped through `from_token`, or
    /// any caller passing canonical GitHub casing) must collapse onto that
    /// identity — stored, matched via `contains_entry`, and displayed as
    /// one `@alice`, never a second phantom row that a case-sensitive set
    /// would mint.
    #[test]
    fn person_axis_identity_is_case_insensitive() {
        let mut set = FilterSet::new();
        set.replace_entries([FilterEntry::Person("Alice".into())]);
        assert_eq!(
            set.people().iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            vec!["alice"],
            "the login is normalized to its lowercase identity on insert"
        );
        // `contains_entry` (drives the menu's pre-checked rows) matches
        // regardless of the query's case, so the discovered `alice` row
        // reads as active.
        assert!(set.contains_entry(&FilterEntry::Person("alice".into())));
        assert!(set.contains_entry(&FilterEntry::Person("ALICE".into())));
        assert_eq!(set.chips(), vec!["@alice"]);

        // Still matches the task whatever case the field carries.
        let agents = HashMap::new();
        let authored = workspace_with("a", |t| t.author = "Alice".into());
        assert!(set.accepts(&FilterCtx {
            w: &authored,
            agents: &agents,
            now: now(),
        }));

        // The persisted-token round-trip normalizes too.
        assert_eq!(
            FilterEntry::from_token("person:Bob"),
            Some(FilterEntry::Person("bob".into())),
        );
    }

    #[test]
    fn priority_predicates_match_the_task_priority() {
        let agents = HashMap::new();
        let matches = |ws: &Workspace, f: Filter| {
            f.matches(&FilterCtx {
                w: ws,
                agents: &agents,
                now: now(),
            })
        };

        let urgent = workspace_with("a", |t| t.priority = Some(Priority::Urgent));
        assert!(matches(&urgent, Filter::PriorityUrgent));
        assert!(!matches(&urgent, Filter::PriorityLow));

        let low = workspace_with("b", |t| t.priority = Some(Priority::Low));
        assert!(matches(&low, Filter::PriorityLow));

        // No priority (e.g. a GitHub task) matches no priority predicate.
        let none = workspace_with("c", |t| t.priority = None);
        for f in [
            Filter::PriorityUrgent,
            Filter::PriorityHigh,
            Filter::PriorityMedium,
            Filter::PriorityLow,
        ] {
            assert_eq!(f.axis(), FilterAxis::Priority);
            assert!(!matches(&none, f));
        }
    }

    /// The `blocked` predicate reads the workspace-level dependency
    /// helpers, so it fires on a native / body-marker `blocked_by` edge
    /// and on a declared `Blocked on:` reason alike — the same signal the
    /// `⊗` row badge shows.
    #[test]
    fn blocked_filter_matches_edges_and_declared_reasons() {
        let agents = HashMap::new();
        let matches = |ws: &Workspace, f: Filter| {
            f.matches(&FilterCtx {
                w: ws,
                agents: &agents,
                now: now(),
            })
        };

        // A task-edge blocker.
        let edge = workspace_with("a", |t| {
            t.kind = Some(TaskKind::Issue);
            t.blocked_by = vec![TaskId {
                source: "github".into(),
                key: "owner/r#7".into(),
            }];
        });
        assert!(matches(&edge, Filter::Blocked));
        assert!(
            !matches(&edge, Filter::Ready),
            "a blocked issue is not ready"
        );

        // A declared free-text blocker (no edge) still counts as blocked.
        let declared = workspace_with("b", |t| {
            t.kind = Some(TaskKind::Issue);
            t.blocked_on = Some("waiting on legal".into());
        });
        assert!(matches(&declared, Filter::Blocked));
        assert!(!matches(&declared, Filter::Ready));

        // No blockers at all → not blocked.
        let clear = workspace_with("c", |t| t.kind = Some(TaskKind::Issue));
        assert!(!matches(&clear, Filter::Blocked));

        assert_eq!(Filter::Blocked.axis(), FilterAxis::State);
        assert_eq!(Filter::Blocked.label(), "blocked");
    }

    /// `in-flight` is what is in the air right now: a live agent working,
    /// asking or just done; a workspace marked read in the last hour; or
    /// your own task moving in the last hour. Someone else's PR moving, or
    /// your own work from yesterday, is not.
    #[test]
    fn in_flight_is_what_you_touched_or_what_moved_in_the_last_hour() {
        let no_agents = HashMap::new();
        let check = |ws: &Workspace, agents: &HashMap<SessionKey, lazybox_ipc::AgentState>| {
            Filter::InFlight.matches(&FilterCtx {
                w: ws,
                agents,
                now: now(),
            })
        };
        let an_hour_and_more = now() - IN_FLIGHT_WINDOW - chrono::Duration::minutes(1);
        let stale = |key: &str, role: TaskRole| {
            workspace_with(key, |t| {
                t.role = role;
                t.updated_at = an_hour_and_more;
            })
        };

        let quiet = stale("a", TaskRole::Author);
        assert!(
            !check(&quiet, &no_agents),
            "yesterday's own work is not in flight"
        );

        let moved = workspace_with("b", |t| t.role = TaskRole::Author);
        assert!(check(&moved, &no_agents), "your own PR moved just now");
        let theirs = workspace_with("c", |t| t.role = TaskRole::Observer);
        assert!(
            !check(&theirs, &no_agents),
            "someone else's PR moving is not yours"
        );

        let mut viewed = stale("d", TaskRole::Observer);
        viewed.last_viewed_at = Some(now() - chrono::Duration::minutes(10));
        assert!(check(&viewed, &no_agents), "marked read ten minutes ago");

        for state in [
            lazybox_ipc::AgentState::Working,
            lazybox_ipc::AgentState::InputNeeded,
            lazybox_ipc::AgentState::Done,
        ] {
            let agents = HashMap::from([(SessionKey::from(&quiet.key), state)]);
            assert!(check(&quiet, &agents), "a live agent that is {state:?}");
        }

        assert_eq!(Filter::InFlight.axis(), FilterAxis::State);
        assert_eq!(Filter::InFlight.label(), "in-flight");
    }

    /// `ready` = an issue with no dependency edge, no declared blocker,
    /// and no agent session yet: "what can I pick up right now". A PR, a
    /// blocked issue, or an issue already under an agent all fail it.
    #[test]
    fn ready_filter_matches_only_startable_issues() {
        use std::path::PathBuf;
        let agents = HashMap::new();
        let matches = |ws: &Workspace, f: Filter| {
            f.matches(&FilterCtx {
                w: ws,
                agents: &agents,
                now: now(),
            })
        };

        // A clean issue with nothing in its way.
        let ready = workspace_with("a", |t| t.kind = Some(TaskKind::Issue));
        assert!(matches(&ready, Filter::Ready));
        assert!(!matches(&ready, Filter::Blocked));

        // A PR is never "ready" — the predicate is issue-only.
        let pr = workspace_with("b", |t| t.kind = Some(TaskKind::Pr));
        assert!(!matches(&pr, Filter::Ready));

        // An issue already carrying an agent session is under way, not
        // waiting to be picked up.
        let mut working = workspace_with("c", |t| t.kind = Some(TaskKind::Issue));
        working.sessions.push(lazybox_core::WorkspaceSession::new(
            working.key.clone(),
            lazybox_core::SessionKind::Agent {
                agent_id: "claude".into(),
            },
            PathBuf::from("/tmp/wt"),
            now(),
        ));
        assert!(!matches(&working, Filter::Ready));

        assert_eq!(Filter::Ready.axis(), FilterAxis::State);
        assert_eq!(Filter::Ready.label(), "ready");
    }

    /// A snoozed issue was deliberately deferred into the Snoozed mailbox,
    /// so it must not read as "ready to pick up now" even when it is
    /// otherwise startable (an issue, no blockers, no agent). Without the
    /// `!is_snoozed` guard, `ready` and `snoozed` overlapped and a
    /// deferred ticket kept resurfacing in the ready set.
    #[test]
    fn ready_filter_excludes_a_snoozed_issue() {
        let agents = HashMap::new();
        let clock = now();
        let matches = |ws: &Workspace, f: Filter| {
            f.matches(&FilterCtx {
                w: ws,
                agents: &agents,
                now: clock,
            })
        };

        // Startable but snoozed a few hours out → not ready, and snoozed.
        let mut snoozed = workspace_with("a", |t| t.kind = Some(TaskKind::Issue));
        snoozed.snoozed_until = Some(clock + chrono::Duration::hours(5));
        assert!(matches(&snoozed, Filter::Snoozed));
        assert!(
            !matches(&snoozed, Filter::Ready),
            "a snoozed issue is deferred, not ready to pick up"
        );

        // An EXPIRED snooze reads as awake, so the same issue is ready
        // again once its deadline has passed (stale timestamps are never
        // cleared in the store, so the predicate must gate on the clock).
        let mut woke = workspace_with("b", |t| t.kind = Some(TaskKind::Issue));
        woke.snoozed_until = Some(clock - chrono::Duration::hours(1));
        assert!(!matches(&woke, Filter::Snoozed));
        assert!(
            matches(&woke, Filter::Ready),
            "an expired snooze is awake, so the issue is ready again"
        );
    }

    #[test]
    fn menu_lists_every_filter_in_axis_order_with_counts() {
        let agents = HashMap::new();
        let a = workspace("a", TaskRole::Author, CiStatus::Failure, TaskKind::Pr);
        let b = workspace("b", TaskRole::Reviewer, CiStatus::Success, TaskKind::Issue);
        let candidates = vec![&a, &b];
        let menu = Filter::menu(&candidates, &agents, &FilterSet::new());

        // Every fixed filter, in ALL order, none active.
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
            agents: &agents,
            now: now(),
        }));
        assert!(roles.accepts(&FilterCtx {
            w: &reviewer_issue,
            agents: &agents,
            now: now(),
        }));

        roles.toggle(Filter::Pr);
        assert!(roles.accepts(&FilterCtx {
            w: &author_pr,
            agents: &agents,
            now: now(),
        }));
        assert!(
            !roles.accepts(&FilterCtx {
                w: &reviewer_issue,
                agents: &agents,
                now: now(),
            }),
            "PR-kind axis ANDs, so the reviewer issue is filtered out"
        );
    }

    /// `observer` sits on the Role axis and matches only rows that never
    /// name the viewer — the complement of the four "mine" predicates —
    /// so a watched repo's foreign PRs are selectable, and excluded by
    /// selecting the other four, without leaking into `mentioned`.
    #[test]
    fn observer_is_a_role_predicate_disjoint_from_mentioned() {
        let agents = HashMap::new();
        let observer = workspace("o", TaskRole::Observer, CiStatus::None, TaskKind::Pr);
        let mentioned = workspace("m", TaskRole::Mentioned, CiStatus::None, TaskKind::Pr);
        assert_eq!(Filter::Observer.axis(), FilterAxis::Role);

        let mut only_observer = FilterSet::default();
        only_observer.toggle(Filter::Observer);
        assert!(only_observer.accepts(&FilterCtx {
            w: &observer,
            agents: &agents,
            now: now(),
        }));
        assert!(!only_observer.accepts(&FilterCtx {
            w: &mentioned,
            agents: &agents,
            now: now(),
        }));

        let mut mine = FilterSet::default();
        for f in [
            Filter::Author,
            Filter::Reviewer,
            Filter::Assignee,
            Filter::Mentioned,
        ] {
            mine.toggle(f);
        }
        assert!(mine.accepts(&FilterCtx {
            w: &mentioned,
            agents: &agents,
            now: now(),
        }));
        assert!(!mine.accepts(&FilterCtx {
            w: &observer,
            agents: &agents,
            now: now(),
        }));
    }

    #[test]
    fn chips_are_active_labels_in_menu_order() {
        let mut set = FilterSet::new();
        set.toggle(Filter::Issue);
        set.toggle(Filter::CiFailing);
        // Insertion order was Issue then CiFailing, but chips follow ALL order.
        assert_eq!(
            set.chips(),
            vec!["ci-failing".to_string(), "issue".to_string()]
        );
    }

    /// The reported defect (#1914): the capability shipped, but its
    /// label had moved off the word the user reached for, so the `f`
    /// menu's typeahead found nothing. Every spelling of the old name
    /// must reach the entry — and reach ONLY it, or "found" would mean
    /// "somewhere in a list of candidates".
    #[test]
    fn the_old_rate_limited_name_finds_the_needs_recovery_entry() {
        let entry = FilterEntry::Predicate(Filter::RateLimited);
        for query in [
            "rate-limited",
            "ratelimited",
            "rate limited",
            "Rate-Limited",
            "RATELIMITED",
            "limit",
            "rate",
        ] {
            assert!(
                entry.matches_search(query),
                "typing {query:?} must reach the needs-recovery entry",
            );
            let hits: Vec<&'static str> = Filter::ALL
                .into_iter()
                .filter(|f| f.matches_search(query))
                .map(|f| f.label())
                .collect();
            assert_eq!(
                hits,
                vec!["needs-recovery"],
                "{query:?} must reach needs-recovery and nothing else",
            );
        }
    }

    /// The other words the predicate's own doc comment and the two
    /// recovery actions use for the states it matches — each the word a
    /// user who saw that state printed on screen would type.
    #[test]
    fn the_recovery_states_vocabulary_finds_the_entry() {
        for query in ["parked", "stalled", "stopped", "awaiting-reset", "recovery"] {
            assert!(
                Filter::RateLimited.matches_search(query),
                "{query:?} must reach needs-recovery",
            );
        }
    }

    /// Each drifted label found by the #1914 audit, reaching its filter
    /// by the word the predicate itself covers but the label omits.
    #[test]
    fn audited_aliases_reach_their_filter() {
        for (query, want) in [
            ("in-review", Filter::InProgress),
            ("conflicting", Filter::Conflict),
            ("input-needed", Filter::Asking),
            ("changes-requested", Filter::ReviewRequested),
            ("ci-mixed", Filter::CiFailing),
            ("ci-queued", Filter::CiRunning),
            ("armed", Filter::AutoMerge),
            ("update-branch", Filter::BehindBase),
            ("depends-on", Filter::Blocked),
            ("unblocked", Filter::Ready),
            ("active", Filter::InFlight),
            ("recent", Filter::InFlight),
            ("agent-working", Filter::AgentWorking),
        ] {
            let hits: Vec<Filter> = Filter::ALL
                .into_iter()
                .filter(|f| f.matches_search(query))
                .collect();
            assert!(
                hits.contains(&want),
                "{query:?} must reach {want:?}, reached {hits:?}",
            );
        }
    }

    /// An alias must never collide with another filter's canonical
    /// label: a query that lands on two filters makes the typeahead
    /// ambiguous and `from_token` arbitrary. This is the guard that
    /// keeps the next alias honest — `working` is deliberately absent
    /// from `Claimed` for exactly this reason.
    #[test]
    fn no_alias_shadows_another_filters_label() {
        for f in Filter::ALL {
            for alias in f.search_aliases() {
                let key = search_key(alias);
                assert!(!key.is_empty(), "{f:?} has an empty alias");
                if let Some(other) = Filter::ALL
                    .into_iter()
                    .find(|o| *o != f && search_key(o.label()) == key)
                {
                    panic!("{f:?}'s alias {alias:?} is {other:?}'s own label");
                }
                assert!(
                    !f.search_aliases()
                        .iter()
                        .any(|a| *a != *alias && search_key(a) == key),
                    "{f:?} lists {alias:?} twice",
                );
            }
        }
    }

    /// Normalization drops punctuation, so two labels that differ only
    /// by a hyphen would become one search key — and `by_exact_name`
    /// would resolve a token to whichever came first in `ALL`.
    #[test]
    fn labels_stay_distinct_after_normalization() {
        let mut keys: Vec<String> = Filter::ALL
            .into_iter()
            .map(|f| search_key(f.label()))
            .collect();
        let before = keys.len();
        keys.sort();
        keys.dedup();
        assert_eq!(before, keys.len(), "two filter labels normalize alike");
    }

    /// An empty query hides nothing — opening the menu and typing
    /// nothing must still show every row.
    #[test]
    fn an_empty_query_matches_every_entry() {
        for f in Filter::ALL {
            assert!(f.matches_search(""));
            assert!(f.matches_search("   "));
        }
        assert!(FilterEntry::Label("bug".into()).matches_search(""));
    }

    /// The value-driven axes have only their own text to match, and it
    /// is normalized the same way (a Person row renders as `@login`).
    #[test]
    fn value_axis_entries_match_their_own_text() {
        assert!(FilterEntry::Label("needs triage".into()).matches_search("needstriage"));
        assert!(FilterEntry::Person("Alice".into()).matches_search("alice"));
        assert!(!FilterEntry::LinearState("Backlog".into()).matches_search("done"));
    }

    /// A persisted or hand-edited lens token spelled with the old name
    /// resolves to the predicate instead of reading as a filter that is
    /// gone — and is rewritten canonically on the next save, the same
    /// normalization the `person:Alice` token already gets.
    #[test]
    fn from_token_accepts_an_alias_and_renormalizes_it() {
        let entry = FilterEntry::from_token("rate-limited").expect("alias resolves");
        assert_eq!(entry, FilterEntry::Predicate(Filter::RateLimited));
        assert_eq!(entry.to_token(), "needs-recovery");
        // Lower-cased `PR` resolves too, by the same normalization.
        assert_eq!(
            FilterEntry::from_token("pr"),
            Some(FilterEntry::Predicate(Filter::Pr)),
        );
        // A partial word is NOT a token: exact names only here, or a
        // stale config would silently acquire a filter nobody chose.
        assert_eq!(FilterEntry::from_token("limit"), None);
        assert_eq!(FilterEntry::from_token("banana"), None);
        assert_eq!(FilterEntry::from_token(""), None);
    }

    /// Every canonical label still round-trips, aliases notwithstanding.
    #[test]
    fn every_label_round_trips_through_a_token() {
        for f in Filter::ALL {
            let entry = FilterEntry::Predicate(f);
            assert_eq!(
                FilterEntry::from_token(&entry.to_token()),
                Some(entry.clone()),
                "{f:?} must round-trip",
            );
        }
    }
}
