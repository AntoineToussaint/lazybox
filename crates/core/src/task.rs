use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};

/// A unique identifier for a task, scoped by source.
/// e.g. ("github", "owner/repo#123") or ("linear", "ENG-456")
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskId {
    /// Which provider created this task (e.g. "github", "linear").
    pub source: String,
    /// Provider-specific unique key.
    pub key: String,
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.source, self.key)
    }
}

/// Why this task is on your radar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TaskRole {
    /// You authored it (your PR, your issue).
    Author,
    /// You're assigned as a reviewer.
    Reviewer,
    /// You're assigned to work on it.
    Assignee,
    /// You're mentioned or subscribed.
    Mentioned,
}

/// The lifecycle state of a task (source-agnostic).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TaskState {
    Open,
    InProgress,
    InReview,
    Closed,
    Merged,
    Draft,
}

/// Whether a task is a pull/merge request or an issue/ticket. Set by
/// the provider at construction — the provider always knows which kind
/// of object it fetched — so classification never has to reverse-engineer
/// it from the URL shape. See [`Task::is_pr`] for how this authoritative
/// field supersedes the legacy URL heuristic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TaskKind {
    /// A pull request / merge request (GitHub PR, GitLab MR, …).
    Pr,
    /// An issue / ticket (GitHub issue, Linear ticket, Slack thread, …).
    Issue,
}

/// A label/tag on a task. Mostly transparent to lazybox — providers
/// give us the name + color (hex like `"d73a4a"` for GitHub) and
/// the sidebar renders the name with the color as foreground. Color
/// is best-effort: empty string means "no color known."
///
/// Serde compatibility: human-readable formats deserialize from either a
/// bare string (old persisted JSON) or `{name, color}`. Binary formats use
/// the fixed current struct shape: serde's untagged-enum visitor requires
/// `deserialize_any`, which bincode deliberately does not support. Keeping
/// the two paths explicit prevents a JSON migration shim from breaking the
/// daemon/client socket protocol.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize)]
pub struct Label {
    pub name: String,
    #[serde(default)]
    pub color: String,
}

impl Label {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    pub fn with_color(name: impl Into<String>, color: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            color: color.into(),
        }
    }
}

/// On-wire shape for [`Label`]. Either a bare string (legacy persisted
/// state) or `{name, color}`. Lives at the module root rather than
/// inside the `#[serde(from)]` attribute so callers can grep for it.
#[derive(Deserialize)]
#[serde(untagged)]
enum LabelRepr {
    Plain(String),
    Struct {
        name: String,
        #[serde(default)]
        color: String,
    },
}

impl From<LabelRepr> for Label {
    fn from(repr: LabelRepr) -> Self {
        match repr {
            LabelRepr::Plain(name) => Self {
                name,
                color: String::new(),
            },
            LabelRepr::Struct { name, color } => Self { name, color },
        }
    }
}

impl<'de> Deserialize<'de> for Label {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        if deserializer.is_human_readable() {
            return LabelRepr::deserialize(deserializer).map(Into::into);
        }

        // This helper intentionally mirrors Label's serialized field order.
        // Bincode is sequence-based and cannot drive the untagged legacy
        // visitor above; protocol-version negotiation protects this fixed
        // binary shape from mixed-version peers.
        #[derive(Deserialize)]
        struct BinaryLabel {
            name: String,
            color: String,
        }

        BinaryLabel::deserialize(deserializer).map(|wire| Self {
            name: wire.name,
            color: wire.color,
        })
    }
}

/// CI / build status rolled up from individual checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum CiStatus {
    #[default]
    None,
    Pending,
    Running,
    Success,
    Failure,
    Mixed,
}

/// Review status rolled up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum ReviewStatus {
    #[default]
    None,
    Pending,
    Approved,
    ChangesRequested,
}

/// Whether a PR's branch can merge cleanly into its base. Tristate
/// because GitHub computes the field lazily — `Unknown` means
/// "GitHub hasn't computed yet" and is observably different from
/// `Mergeable` (no conflict). The sidebar surfaces `Unknown` as a
/// `?` pill so the user can tell lazybox doesn't actually know.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum Mergeable {
    /// GitHub hasn't computed mergeability yet. Re-query nudges
    /// computation; usually resolves within a poll cycle or two.
    #[default]
    Unknown,
    /// Branch merges cleanly.
    Mergeable,
    /// Branch has conflicts with the base.
    Conflicting,
}

impl Mergeable {
    /// Did we positively confirm a conflict? `Unknown` returns
    /// false — caller should treat ambiguity separately rather
    /// than collapsing it into "no conflict".
    pub fn is_conflicting(self) -> bool {
        matches!(self, Self::Conflicting)
    }
}

/// An individual CI check run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckRun {
    pub name: String,
    pub status: CiStatus,
    pub url: Option<String>,
}

/// A comment or activity entry on a task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Activity {
    pub author: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub kind: ActivityKind,
    /// GitHub node ID for replying to this comment.
    #[serde(default)]
    pub node_id: Option<String>,
    /// File path for inline review comments (e.g. "src/foo.rs").
    #[serde(default)]
    pub path: Option<String>,
    /// Line number for inline review comments.
    #[serde(default)]
    pub line: Option<u32>,
    /// Diff hunk showing the code context for inline review comments.
    #[serde(default)]
    pub diff_hunk: Option<String>,
    /// GraphQL node ID of the review *thread* (used for threaded replies
    /// via `addPullRequestReviewThreadReply` and for `resolveReviewThread`).
    #[serde(default)]
    pub thread_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActivityKind {
    Comment,
    Review,
    StatusChange,
    CiUpdate,
}

/// Number of body characters [`ActivityFingerprint::Content`] keeps.
/// Short on purpose — comparing whole bodies is wasteful and a prefix
/// is enough to tell row from row in practice; 64 chars survives
/// auto-formatting that trims or wraps trailing whitespace.
pub const ACTIVITY_FINGERPRINT_BODY_PREFIX: usize = 64;

/// Stable-enough identifier for one activity row, shared by client
/// and daemon so a positional index never travels alone across the
/// wire: a poll committing a new top-of-feed comment shifts every
/// older row down by one, and a raw cached index would mark the
/// wrong comment read. Prefers the upstream `node_id` (GitHub gives
/// this for comments and reviews); falls back to a content tuple for
/// kinds that don't carry one (CI events, status changes).
///
/// This is the same identity notion `Workspace::merge_activity` uses
/// to carry read state across a re-sort — node-id first, content
/// tuple fallback — kept in core precisely so the TUI's fingerprint
/// and the daemon's resolution can't drift.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActivityFingerprint {
    NodeId(String),
    Content {
        author: String,
        created_at: DateTime<Utc>,
        body_prefix: String,
    },
}

impl ActivityFingerprint {
    pub fn of(activity: &Activity) -> Self {
        if let Some(id) = &activity.node_id
            && !id.is_empty()
        {
            return Self::NodeId(id.clone());
        }
        let body_prefix: String = activity
            .body
            .chars()
            .take(ACTIVITY_FINGERPRINT_BODY_PREFIX)
            .collect();
        Self::Content {
            author: activity.author.clone(),
            created_at: activity.created_at,
            body_prefix,
        }
    }

    pub fn matches(&self, activity: &Activity) -> bool {
        *self == Self::of(activity)
    }

    /// Resolve this fingerprint to its *current* index in `activity`.
    /// `hint` is the position the sender last saw the row at — checked
    /// first so identical-content twins resolve to the row the user
    /// actually acted on when it hasn't moved; otherwise the list is
    /// scanned. `None` when no row matches (deleted upstream between
    /// snapshot and command) — callers should no-op rather than touch
    /// a stranger.
    pub fn resolve(&self, activity: &[Activity], hint: usize) -> Option<usize> {
        if let Some(act) = activity.get(hint)
            && self.matches(act)
        {
            return Some(hint);
        }
        activity.iter().position(|a| self.matches(a))
    }
}

/// A source-agnostic task descriptor. Providers convert their domain objects
/// into this type. The TUI and session system only work with `Task`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub title: String,
    pub body: Option<String>,
    pub state: TaskState,
    pub role: TaskRole,
    pub ci: CiStatus,
    pub review: ReviewStatus,
    pub checks: Vec<CheckRun>,
    pub unread_count: u32,
    pub url: String,
    pub repo: Option<String>,
    pub branch: Option<String>,
    /// Target branch of the PR — usually the repo default (main / master),
    /// but for stacked PRs it's the head branch of another open PR.
    #[serde(default)]
    pub base_branch: Option<String>,
    pub updated_at: DateTime<Utc>,
    /// When the task was opened. Unlike `updated_at`, providers never
    /// bump this on later activity, so it's the stable signal for "how
    /// old is this" — what the sidebar reads to show an issue's age and
    /// fade stale ones (issue #274). `None` for older snapshots and
    /// providers that don't supply it; callers fall back to
    /// `updated_at` via [`Task::opened_at`].
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    /// When this task reached a terminal state — `closedAt` for a
    /// PR (merge closes it too) or a closed issue. Unlike
    /// `updated_at`, GitHub does NOT bump this on later activity
    /// (post-merge comments, branch deletion, linked-issue closure),
    /// so it's the stable signal for "how long ago did this finish."
    /// `None` while the task is open. `#[serde(default)]` so older
    /// snapshots deserialize.
    #[serde(default)]
    pub closed_at: Option<DateTime<Utc>>,
    /// `#[serde(default)]` so an older daemon snapshot (no labels
    /// field) still deserializes — mirrors `reviewers` / `assignees`.
    #[serde(default)]
    pub labels: Vec<Label>,
    /// Requested reviewers (user logins or team names).
    #[serde(default)]
    pub reviewers: Vec<String>,
    /// Assignees (user logins).
    #[serde(default)]
    pub assignees: Vec<String>,
    /// Auto-merge is enabled ("merge when ready" armed by someone).
    /// NOTE: this does NOT mean the PR is approved or that anything will
    /// merge right now — only that it will merge once CI + reviews pass.
    #[serde(default, alias = "in_merge_queue")]
    pub auto_merge_enabled: bool,
    /// The PR is actually sitting in GitHub's merge queue right now
    /// (separate from auto-merge; this is the real queued-to-merge state).
    #[serde(default)]
    pub is_in_merge_queue: bool,
    /// Merge-conflict state of this PR. Tristate because GitHub
    /// computes the field lazily — a stale PR or one we just
    /// asked about returns `Unknown` until GitHub finishes the
    /// background mergeability check. Surfacing `Unknown` as
    /// "definitely no conflict" misleads the user; the sidebar
    /// shows a `?` pill instead so the actual state is visible.
    #[serde(default)]
    pub mergeable: Mergeable,
    /// Whether the PR branch is behind its base (needs "Update branch").
    /// Derived from GitHub's `mergeStateStatus == BEHIND`.
    #[serde(default)]
    pub is_behind_base: bool,
    /// GraphQL node ID of the PR — required for mutations like
    /// `updatePullRequestBranch`. Populated by providers that fetch it.
    #[serde(default)]
    pub node_id: Option<String>,
    /// Whether the last comment/review on this task is from someone else
    /// (i.e. you need to reply).
    pub needs_reply: bool,
    /// Who left the last comment/review.
    pub last_commenter: Option<String>,
    /// Recent activity (comments, reviews) — newest first.
    /// Populated on fetch, used to seed session activity on first load.
    #[serde(default)]
    pub recent_activity: Vec<Activity>,
    /// Number of lines added in this PR.
    #[serde(default)]
    pub additions: u32,
    /// Number of lines deleted in this PR.
    #[serde(default)]
    pub deletions: u32,
    /// Other tasks this PR closes when merged. Populated from
    /// GitHub's `closingIssuesReferences` field. Used by the server
    /// to collapse the issue + PR workspaces so the user doesn't
    /// see two rows for the same work.
    #[serde(default)]
    pub closes_issues: Vec<TaskId>,
    /// Authoritative PR-vs-issue discriminator, set by the provider that
    /// built this task. `None` only for legacy persisted snapshots that
    /// predate the field; [`Task::is_pr`] falls back to the URL heuristic
    /// in that case. Prefer this over any URL sniffing.
    #[serde(default)]
    pub kind: Option<TaskKind>,
}

impl Task {
    /// True when this task represents a pull/merge request rather
    /// than an issue/ticket. Centralizes the URL-shape heuristic so
    /// new providers (Bitbucket, GitLab, …) extend ONE place instead
    /// of every caller. Today's providers in tree:
    ///
    /// - GitHub: `https://github.com/{owner}/{repo}/pull/{N}`
    /// - GitLab: `https://gitlab.com/.../merge_requests/{N}`
    /// - Bitbucket: `https://bitbucket.org/.../pull-requests/{N}`
    ///
    /// Linear has no PR concept — its tasks always return false
    /// here; classification routes them to the linear-issue slot.
    ///
    /// Resolution order:
    /// - The provider-set [`Task::kind`] is authoritative when present —
    ///   an empty/API PR URL no longer misclassifies as an issue, and an
    ///   issue whose body URL happens to contain `/pull/` stays an issue.
    /// - Only when `kind` is `None` (a legacy snapshot persisted before
    ///   the field existed) do we fall back to the URL-shape heuristic.
    pub fn is_pr(&self) -> bool {
        match self.kind {
            Some(TaskKind::Pr) => true,
            Some(TaskKind::Issue) => false,
            None => {
                let url = self.url.as_str();
                url.contains("/pull/")
                    || url.contains("/pull-requests/")
                    || url.contains("/merge_requests/")
            }
        }
    }

    /// When this task was opened, for age/staleness purposes. Uses the
    /// provider-supplied creation time when known, else falls back to
    /// `updated_at` — the best available lower bound on age when a
    /// provider (or an older persisted snapshot) didn't record
    /// `created_at`.
    pub fn opened_at(&self) -> DateTime<Utc> {
        self.created_at.unwrap_or(self.updated_at)
    }
}

/// Computed urgency level for a session. Used for inbox sorting.
/// Ordered from most urgent (0) to least urgent (7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ActionPriority {
    /// Someone commented on your PR and you haven't responded.
    NeedsReply = 0,
    /// CI failed on a PR you authored.
    CiFailed = 1,
    /// A reviewer requested changes on your PR.
    ChangesRequested = 2,
    /// You're requested as a reviewer — you're blocking someone.
    NeedsYourReview = 3,
    /// Your PR is approved and ready to merge.
    ApprovedReadyToMerge = 4,
    /// New unread activity (comments, CI updates, etc.).
    NewActivity = 5,
    /// No action needed — waiting on others.
    WaitingOnOthers = 6,
    /// Old, no recent activity.
    Stale = 7,
}

/// Status badge displayed on a PR row — derived purely from task fields.
///
/// **Single source of truth for the right-trailer pill on every PR row.**
/// The renderer (`crate::tui::components::sidebar::status_pill`) is a
/// pure mapping `StatusTag → Option<StatusPill>` over the active theme;
/// no priority logic lives there. Adding a new visual state means
/// adding a variant here, updating `for_task`, and adding the pill
/// label/style — the renderer mapping is exhaustive so the compiler
/// catches a missing arm.
///
/// Priority order matters: the first matching variant in `for_task`
/// wins. Comments at each branch explain why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusTag {
    /// PR is merged. Past-tense state — trumps everything CI / review
    /// related because there's nothing actionable left.
    Merged,
    /// PR is closed (without merge). Same logic as Merged.
    Closed,
    /// Has merge conflicts with the base branch.
    Conflict,
    /// CI is failing.
    CiFailed,
    /// CI is partially green / partially failing.
    CiMixed,
    /// A reviewer requested changes.
    ChangesRequested,
    /// Actually sitting in GitHub's merge queue.
    Queued,
    /// PR is a draft. Sits ABOVE approval/CI green so a draft PR with
    /// passing CI still reads as DRAFT — the user needs the reminder
    /// that it isn't ready for review.
    Draft,
    /// Approved + CI green or unset — ready to merge now.
    Ready,
    /// Approved but CI not yet green. Signals "human half done."
    Approved,
    /// Auto-merge is armed (will merge when reviews + CI pass). Not the
    /// same as Queued — the PR may still be un-approved.
    AutoMerge,
    /// Awaiting review (review explicitly requested or pending).
    ReviewPending,
    /// CI is still running / pending.
    CiRunning,
    /// CI is green (and nothing more interesting applies).
    CiOk,
    /// Branch is behind base — informational.
    Behind,
    /// Nothing interesting — no badge.
    None,
}

impl StatusTag {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Merged => "MERGED",
            Self::Closed => "CLOSED",
            Self::Conflict => "CONFLICT",
            Self::CiFailed => "CI FAIL",
            Self::CiMixed => "CI MIX",
            Self::ChangesRequested => "CHANGES",
            Self::Queued => "QUEUED",
            Self::Draft => "DRAFT",
            Self::Ready => "READY",
            Self::Approved => "APPROVED",
            Self::AutoMerge => "AUTO",
            Self::ReviewPending => "REVIEW",
            Self::CiRunning => "CI...",
            Self::CiOk => "CI OK",
            Self::Behind => "BEHIND",
            Self::None => "",
        }
    }

    /// Derive the status tag for a task. Pure — unit-testable.
    ///
    /// Priority (first match wins):
    /// `Merged` → `Closed` → `Conflict` → `CiFailed` → `CiMixed` →
    /// `ChangesRequested` → `Queued` → `Draft` → `Ready` → `Approved`
    /// → `AutoMerge` → `ReviewPending` → `CiRunning` → `CiOk` →
    /// `Behind` → `None`.
    pub fn for_task(task: &Task) -> Self {
        // Inactive states win over everything: a merged PR's CI
        // history isn't actionable, so showing MERGED beats showing
        // a stale CI FAIL.
        match task.state {
            TaskState::Merged => return Self::Merged,
            TaskState::Closed => return Self::Closed,
            _ => {}
        }
        if task.mergeable.is_conflicting() {
            return Self::Conflict;
        }
        match task.ci {
            CiStatus::Failure => return Self::CiFailed,
            CiStatus::Mixed => return Self::CiMixed,
            _ => {}
        }
        if task.review == ReviewStatus::ChangesRequested {
            return Self::ChangesRequested;
        }
        if task.is_in_merge_queue {
            return Self::Queued;
        }
        // Draft sits between CI failure (acutely actionable) and the
        // approval/CI-green signals. A draft PR with green CI still
        // wants the DRAFT badge so the user remembers it isn't ready
        // for review.
        if matches!(task.state, TaskState::Draft) {
            return Self::Draft;
        }
        if task.review == ReviewStatus::Approved {
            if matches!(task.ci, CiStatus::Success | CiStatus::None) {
                return Self::Ready;
            }
            return Self::Approved;
        }
        if task.auto_merge_enabled {
            return Self::AutoMerge;
        }
        if task.review == ReviewStatus::Pending {
            return Self::ReviewPending;
        }
        match task.ci {
            CiStatus::Pending | CiStatus::Running => return Self::CiRunning,
            CiStatus::Success => return Self::CiOk,
            _ => {}
        }
        if task.is_behind_base {
            return Self::Behind;
        }
        Self::None
    }
}

impl ActionPriority {
    /// Human-readable label for section headers.
    pub fn label(&self) -> &'static str {
        match self {
            Self::NeedsReply => "Needs Your Reply",
            Self::CiFailed => "CI Failed",
            Self::ChangesRequested => "Changes Requested",
            Self::NeedsYourReview => "Needs Your Review",
            Self::ApprovedReadyToMerge => "Ready to Merge",
            Self::NewActivity => "New Activity",
            Self::WaitingOnOthers => "Waiting on Others",
            Self::Stale => "Stale",
        }
    }
}

#[cfg(test)]
mod activity_fingerprint_tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn act(node_id: Option<&str>, author: &str, body: &str, secs: i64) -> Activity {
        Activity {
            author: author.into(),
            body: body.into(),
            created_at: Utc.timestamp_opt(1_700_000_000 + secs, 0).unwrap(),
            kind: ActivityKind::Comment,
            node_id: node_id.map(Into::into),
            path: None,
            line: None,
            diff_hunk: None,
            thread_id: None,
        }
    }

    #[test]
    fn prefers_node_id_and_falls_back_to_content() {
        let with_id = act(Some("IC_1"), "alice", "hello", 0);
        assert_eq!(
            ActivityFingerprint::of(&with_id),
            ActivityFingerprint::NodeId("IC_1".into())
        );
        // Empty node id is treated as absent, like the TUI always did.
        let empty_id = act(Some(""), "alice", "hello", 0);
        assert!(matches!(
            ActivityFingerprint::of(&empty_id),
            ActivityFingerprint::Content { .. }
        ));
    }

    /// The wire scenario `MarkActivityRead` exists for: the client
    /// resolved index 1, a poll committed a new top-of-feed row, and
    /// the daemon must land the mark on the fingerprinted row at its
    /// SHIFTED position — not on whatever now sits at index 1.
    #[test]
    fn resolve_finds_the_row_after_a_list_shift() {
        let target = act(Some("IC_target"), "alice", "the one", 10);
        let fp = ActivityFingerprint::of(&target);
        let shifted = vec![
            act(Some("IC_fresh"), "bot", "brand new", 20),
            act(Some("IC_other"), "bob", "innocent bystander", 15),
            target,
        ];
        assert_eq!(fp.resolve(&shifted, 1), Some(2));
    }

    /// A vanished row resolves to None — the daemon no-ops instead of
    /// marking a stranger.
    #[test]
    fn resolve_returns_none_when_the_row_is_gone() {
        let fp = ActivityFingerprint::of(&act(Some("IC_gone"), "alice", "deleted", 0));
        let list = vec![act(Some("IC_other"), "bob", "still here", 5)];
        assert_eq!(fp.resolve(&list, 0), None);
    }

    /// Identical-content twins (no node id): the hint index picks the
    /// exact row the user acted on when it still matches, instead of
    /// always collapsing onto the first twin.
    #[test]
    fn resolve_hint_disambiguates_content_twins() {
        let twin = act(None, "bot", "same text same second", 0);
        let list = vec![twin.clone(), twin.clone()];
        let fp = ActivityFingerprint::of(&twin);
        assert_eq!(fp.resolve(&list, 1), Some(1), "matching hint wins");
        assert_eq!(
            fp.resolve(&list, 5),
            Some(0),
            "stale hint falls back to scan"
        );
    }
}

#[cfg(test)]
mod status_tag_tests {
    use super::*;
    use chrono::Utc;

    fn base() -> Task {
        Task {
            id: TaskId {
                source: "gh".into(),
                key: "o/r#1".into(),
            },
            title: "t".into(),
            body: None,
            state: TaskState::Open,
            role: TaskRole::Author,
            ci: CiStatus::None,
            review: ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: "u".into(),
            repo: Some("o/r".into()),
            branch: Some("b".into()),
            base_branch: None,
            updated_at: Utc::now(),
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: crate::Mergeable::Mergeable,
            is_behind_base: false,
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            kind: None,
            closes_issues: vec![],
        }
    }

    #[test]
    fn opened_at_prefers_created_at_and_falls_back_to_updated_at() {
        let created = Utc::now() - chrono::Duration::days(90);
        let updated = Utc::now();
        let mut t = base();
        t.updated_at = updated;

        // No creation time recorded (older snapshot / provider that
        // doesn't supply it) → age falls back to last activity.
        t.created_at = None;
        assert_eq!(t.opened_at(), updated);

        // Creation time known → used verbatim, even when far older than
        // the last activity.
        t.created_at = Some(created);
        assert_eq!(t.opened_at(), created);
    }

    #[test]
    fn auto_merge_armed_is_not_queued() {
        // REGRESSION: auto_merge_enabled used to render as "QUEUED" even
        // though the PR was neither approved nor actually in the queue.
        // Now it renders as AutoMerge (distinct from Queued).
        let mut t = base();
        t.auto_merge_enabled = true;
        t.review = ReviewStatus::None;
        t.is_in_merge_queue = false;
        assert_eq!(StatusTag::for_task(&t), StatusTag::AutoMerge);
        assert_ne!(StatusTag::for_task(&t), StatusTag::Queued);
    }

    #[test]
    fn actually_in_merge_queue_is_queued() {
        let mut t = base();
        t.is_in_merge_queue = true;
        assert_eq!(StatusTag::for_task(&t), StatusTag::Queued);
    }

    #[test]
    fn approved_plus_green_ci_is_ready() {
        let mut t = base();
        t.review = ReviewStatus::Approved;
        t.ci = CiStatus::Success;
        assert_eq!(StatusTag::for_task(&t), StatusTag::Ready);
    }

    #[test]
    fn conflict_trumps_everything() {
        let mut t = base();
        t.mergeable = crate::Mergeable::Conflicting;
        t.ci = CiStatus::Failure;
        t.review = ReviewStatus::ChangesRequested;
        t.is_in_merge_queue = true;
        assert_eq!(StatusTag::for_task(&t), StatusTag::Conflict);
    }

    #[test]
    fn ci_failed_beats_everything_but_conflict() {
        let mut t = base();
        t.ci = CiStatus::Failure;
        t.review = ReviewStatus::Approved;
        t.is_in_merge_queue = true;
        assert_eq!(StatusTag::for_task(&t), StatusTag::CiFailed);
    }

    #[test]
    fn changes_requested_shown_before_queue_hints() {
        let mut t = base();
        t.review = ReviewStatus::ChangesRequested;
        t.auto_merge_enabled = true;
        assert_eq!(StatusTag::for_task(&t), StatusTag::ChangesRequested);
    }

    #[test]
    fn queue_beats_ready_when_both_apply() {
        // If it's actually in the queue, that's the more specific fact.
        let mut t = base();
        t.review = ReviewStatus::Approved;
        t.ci = CiStatus::Success;
        t.is_in_merge_queue = true;
        assert_eq!(StatusTag::for_task(&t), StatusTag::Queued);
    }

    #[test]
    fn draft_beats_approval_signals() {
        // A draft PR with passing CI + approval must still read as
        // DRAFT — APPROVED on a draft is misleading because the PR
        // isn't ready for review. Pinned by the sidebar's pill
        // rendering; the priority lives in `StatusTag::for_task` so
        // there's a single source of truth.
        let mut t = base();
        t.state = TaskState::Draft;
        t.review = ReviewStatus::Approved;
        t.ci = CiStatus::Success;
        assert_eq!(StatusTag::for_task(&t), StatusTag::Draft);
        // CI failure still wins — that's acutely actionable.
        t.ci = CiStatus::Failure;
        assert_eq!(StatusTag::for_task(&t), StatusTag::CiFailed);
    }

    #[test]
    fn merged_trumps_everything() {
        let mut t = base();
        t.state = TaskState::Merged;
        t.mergeable = crate::Mergeable::Conflicting; // ignored
        t.ci = CiStatus::Failure; // ignored
        assert_eq!(StatusTag::for_task(&t), StatusTag::Merged);
    }

    #[test]
    fn closed_trumps_ci_and_review() {
        let mut t = base();
        t.state = TaskState::Closed;
        t.review = ReviewStatus::Approved;
        t.ci = CiStatus::Failure;
        assert_eq!(StatusTag::for_task(&t), StatusTag::Closed);
    }

    #[test]
    fn ci_mixed_after_failure_before_changes() {
        let mut t = base();
        t.ci = CiStatus::Mixed;
        t.review = ReviewStatus::ChangesRequested;
        assert_eq!(StatusTag::for_task(&t), StatusTag::CiMixed);
        // CI Failure still wins over Mixed.
        t.ci = CiStatus::Failure;
        assert_eq!(StatusTag::for_task(&t), StatusTag::CiFailed);
    }

    #[test]
    fn approved_without_ci_green_is_approved_not_ready() {
        // Approved but CI is still running → APPROVED (human half
        // done, build still going). Approved + green CI → READY.
        let mut t = base();
        t.review = ReviewStatus::Approved;
        t.ci = CiStatus::Running;
        assert_eq!(StatusTag::for_task(&t), StatusTag::Approved);
        t.ci = CiStatus::Success;
        assert_eq!(StatusTag::for_task(&t), StatusTag::Ready);
        // CI None counts as "no CI configured" → also Ready.
        t.ci = CiStatus::None;
        assert_eq!(StatusTag::for_task(&t), StatusTag::Ready);
    }

    #[test]
    fn ci_ok_when_green_with_no_review_signals() {
        let mut t = base();
        t.ci = CiStatus::Success;
        assert_eq!(StatusTag::for_task(&t), StatusTag::CiOk);
    }

    #[test]
    fn behind_only_when_nothing_else_applies() {
        let mut t = base();
        t.is_behind_base = true;
        assert_eq!(StatusTag::for_task(&t), StatusTag::Behind);
        // But CI signals still win.
        t.ci = CiStatus::Running;
        assert_eq!(StatusTag::for_task(&t), StatusTag::CiRunning);
    }

    #[test]
    fn no_tag_for_plain_open_pr() {
        assert_eq!(StatusTag::for_task(&base()), StatusTag::None);
    }

    #[test]
    fn labels_match_expectations() {
        assert_eq!(StatusTag::Conflict.label(), "CONFLICT");
        assert_eq!(StatusTag::CiFailed.label(), "CI FAIL");
        assert_eq!(StatusTag::Queued.label(), "QUEUED");
        assert_eq!(StatusTag::AutoMerge.label(), "AUTO");
        assert_eq!(StatusTag::Ready.label(), "READY");
        assert_eq!(StatusTag::None.label(), "");
    }

    #[test]
    fn is_pr_recognizes_github_url() {
        let mut t = base();
        t.url = "https://github.com/o/r/pull/42".into();
        assert!(t.is_pr());
    }

    #[test]
    fn is_pr_recognizes_gitlab_merge_request_url() {
        // GitLab uses `/-/merge_requests/N`. The substring check
        // catches the relevant slug even with the `-` segment.
        let mut t = base();
        t.url = "https://gitlab.com/group/project/-/merge_requests/42".into();
        assert!(
            t.is_pr(),
            "GitLab merge_request URLs must classify as PR — extending \
             Task::is_pr is the one-line knob future providers tweak",
        );
    }

    #[test]
    fn is_pr_recognizes_bitbucket_pull_request_url() {
        let mut t = base();
        t.url = "https://bitbucket.org/team/project/pull-requests/42".into();
        assert!(t.is_pr());
    }

    #[test]
    fn is_pr_is_false_for_github_issue_url() {
        let mut t = base();
        t.url = "https://github.com/o/r/issues/42".into();
        assert!(!t.is_pr(), "issue URLs must not classify as PR");
    }

    #[test]
    fn is_pr_is_false_for_linear_issue_url() {
        let mut t = base();
        t.id.source = "linear".into();
        t.url = "https://linear.app/team/issue/ENG-42".into();
        assert!(!t.is_pr(), "Linear has no PR concept");
    }

    #[test]
    fn typed_kind_overrides_url_heuristic() {
        // The authoritative provider-set `kind` must win over the URL
        // shape in BOTH directions — this is the whole point of #512.

        // An empty/API URL that the heuristic would read as "not a PR"
        // is still a PR when the provider said so. Previously this PR
        // silently classified as an issue and was never auto-fixed.
        let mut pr = base();
        pr.url = String::new();
        pr.kind = Some(TaskKind::Pr);
        assert!(pr.is_pr(), "typed Pr wins over an empty/API URL");

        // An issue whose body-derived URL happens to contain `/pull/`
        // stays an issue when the provider said Issue.
        let mut issue = base();
        issue.url = "https://github.com/o/r/pull/42".into();
        issue.kind = Some(TaskKind::Issue);
        assert!(!issue.is_pr(), "typed Issue wins over a /pull/ URL");
    }

    #[test]
    fn missing_kind_falls_back_to_url_heuristic() {
        // Legacy persisted snapshots (serialized before the field
        // existed) deserialize `kind == None` and must keep classifying
        // via the URL shape.
        let mut t = base();
        assert!(t.kind.is_none(), "the test base carries no typed kind");
        t.url = "https://github.com/o/r/pull/42".into();
        assert!(t.is_pr(), "None kind falls back to the URL heuristic (PR)");
        t.url = "https://github.com/o/r/issues/42".into();
        assert!(
            !t.is_pr(),
            "None kind falls back to the URL heuristic (issue)"
        );
    }

    #[test]
    fn kind_defaults_to_none_on_legacy_json() {
        // A snapshot persisted before `kind` existed omits the field;
        // `#[serde(default)]` must read it back as `None` so the URL
        // fallback stays in force rather than erroring.
        let json = r#"{
            "id": {"source": "github", "key": "o/r#1"},
            "title": "t", "body": null, "state": "Open", "role": "Author",
            "ci": "None", "review": "None", "checks": [], "unread_count": 0,
            "url": "https://github.com/o/r/pull/1", "repo": "o/r",
            "branch": "b", "updated_at": "2026-01-01T00:00:00Z",
            "needs_reply": false, "last_commenter": null
        }"#;
        let t: Task = serde_json::from_str(json).unwrap();
        assert_eq!(t.kind, None);
        assert!(t.is_pr(), "legacy PR snapshot still classifies via URL");
    }

    #[test]
    fn label_deserializes_old_string_shape() {
        // Pre-color persisted state stored labels as bare strings.
        // The custom Deserialize must keep accepting that shape so a
        // running lazybox upgrade doesn't lose label data.
        let json = r#"["bug", "ci"]"#;
        let labels: Vec<Label> = serde_json::from_str(json).unwrap();
        assert_eq!(
            labels,
            vec![Label::new("bug"), Label::new("ci")],
            "back-compat: bare string labels must still deserialize",
        );
    }

    #[test]
    fn label_deserializes_new_struct_shape() {
        let json = r#"[{"name":"bug","color":"d73a4a"}]"#;
        let labels: Vec<Label> = serde_json::from_str(json).unwrap();
        assert_eq!(labels, vec![Label::with_color("bug", "d73a4a")]);
    }

    #[test]
    fn label_struct_color_is_optional() {
        // Color absent → defaults to empty (same as new(name)).
        let json = r#"[{"name":"bug"}]"#;
        let labels: Vec<Label> = serde_json::from_str(json).unwrap();
        assert_eq!(labels, vec![Label::new("bug")]);
    }

    #[test]
    fn label_round_trips_through_non_self_describing_bincode() {
        // #512 drift guard: `Label` serializes through its derived impl
        // but decodes through the hand-mirrored `BinaryLabel` in the
        // custom `Deserialize`. Adding or reordering a `Label` field
        // without updating `BinaryLabel` would shear the wire decode.
        // This round-trip catches that: the derived encoder writes N
        // fields, the mirror reads its own field list, and the
        // `consumed == encoded.len()` assertion fails the build the
        // moment the two shapes diverge (a stray trailing field is left
        // unconsumed; a missing one under-reads).
        let label = Label::with_color("bug", "d73a4a");
        let config = bincode::config::legacy();
        let encoded = bincode::serde::encode_to_vec(&label, config).unwrap();
        let (decoded, consumed): (Label, usize) =
            bincode::serde::decode_from_slice(&encoded, config).unwrap();

        assert_eq!(decoded, label);
        assert_eq!(
            consumed,
            encoded.len(),
            "every encoded Label byte must be consumed — a mismatch \
             means Label and BinaryLabel field shapes have drifted",
        );
    }
}
