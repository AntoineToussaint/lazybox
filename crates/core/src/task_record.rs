//! The daemon's cached tracker record, shaped for an agent to read (#1799).
//!
//! The daemon already pays GitHub for every field of a [`Task`] on each
//! sweep. Before this module the agent it spawned on that task got none of
//! it — only a session key — so the first thing every session did was
//! `gh issue view N` for text the daemon had fetched minutes earlier, and
//! every subagent did it again. Five parallel sessions exhausted the shared
//! 5,000/hour token budget in seven minutes, which then starved the daemon's
//! own poller: the inbox showed twenty issues open that had been closed for
//! forty minutes.
//!
//! [`TaskRecord`] is the read-only projection the daemon hands over instead —
//! written to `.lazybox/task.json` in the worktree at spawn and served by the
//! `task` / `get_issue` / `list_issues` / `get_pr` MCP tools. It is
//! deliberately *not* [`Task`] itself: a `Task` carries daemon-internal
//! routing state (node ids, engagement bookkeeping, review-thread handles)
//! that an agent has no use for and that would tie the agent-facing shape to
//! every internal refactor.
//!
//! Every record carries [`TaskRecord::fetched_at`] — when the daemon last
//! read this record from the provider, not when the agent asked — so a
//! session can tell a thirty-second-old copy from a stale one and spend a
//! real GitHub call only when the age warrants it.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{Activity, ActivityKind, CiStatus, Task, TaskId, TaskState};

/// How many comments a record carries. The daemon keeps more, but a record
/// rides in an agent's context window: the newest handful is what a session
/// acts on, and the full thread is one `gh` call away when it genuinely
/// needs one.
pub const RECORD_COMMENT_LIMIT: usize = 20;

/// One comment or review on a record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RecordComment {
    pub author: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub kind: ActivityKind,
}

impl RecordComment {
    fn of(activity: &Activity) -> Self {
        Self {
            author: activity.author.clone(),
            body: activity.body.clone(),
            created_at: activity.created_at,
            kind: activity.kind,
        }
    }
}

/// The PR-only half of a record: everything a `gh pr view` would have cost
/// a GitHub call to learn.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PullRequestRecord {
    pub head_branch: Option<String>,
    pub base_branch: Option<String>,
    pub additions: u32,
    pub deletions: u32,
    pub changed_files: u32,
    pub ci: CiStatus,
    /// Failing/pending check names, so an agent can see *which* check is red
    /// without re-listing check runs. Green checks are omitted — a passing
    /// run is fully described by `ci`.
    pub unsuccessful_checks: Vec<String>,
    pub mergeable: crate::Mergeable,
    pub merge_blocked: bool,
    pub is_behind_base: bool,
    pub auto_merge_enabled: bool,
    pub reviewers_pending: Vec<String>,
    pub reviews: Vec<crate::Reviewer>,
    /// Issues this PR closes when merged.
    pub closes: Vec<String>,
}

/// The daemon's cached copy of one tracker record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskRecord {
    /// Canonical id (`github:owner/repo#123`).
    pub id: String,
    pub source: String,
    pub repo: Option<String>,
    /// The trailing `#N`, when the source numbers its records.
    pub number: Option<u64>,
    pub kind: RecordKind,
    pub title: String,
    pub body: String,
    pub state: TaskState,
    /// The source's own workflow-state name, when it has one (Linear's
    /// `In Review`). `None` for GitHub.
    pub state_label: Option<String>,
    pub url: String,
    pub author: String,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
    /// Parent record — the epic this one hangs under.
    pub parent: Option<String>,
    /// Records whose parent is this one. Only populated by callers that
    /// hold the whole cache; empty otherwise.
    pub sub_issues: Vec<String>,
    pub blocked_by: Vec<String>,
    pub merge_after: Vec<String>,
    pub blocked_on: Option<String>,
    pub comments: Vec<RecordComment>,
    /// Comments the daemon holds beyond the newest [`RECORD_COMMENT_LIMIT`]
    /// carried in `comments`. Non-zero means the thread is truncated.
    pub comments_omitted: usize,
    /// Present only for a pull request.
    pub pull_request: Option<PullRequestRecord>,
    /// When the daemon last read this record from the provider. `None` for
    /// a row persisted before the daemon started stamping it.
    pub fetched_at: Option<DateTime<Utc>>,
}

/// Issue-or-PR, resolved from the authoritative [`Task::kind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordKind {
    Issue,
    PullRequest,
}

impl TaskRecord {
    /// Project `task` as the daemon last saw it at `fetched_at`.
    ///
    /// `sub_issues` stays empty — only a caller holding the whole workspace
    /// cache can enumerate children, so it fills them in with
    /// [`TaskRecord::with_sub_issues`].
    pub fn of(task: &Task, fetched_at: Option<DateTime<Utc>>) -> Self {
        let is_pr = task.is_pr();
        Self {
            id: task.id.to_string(),
            source: task.id.source.clone(),
            repo: task.repo.clone(),
            number: task.id.number(),
            kind: if is_pr {
                RecordKind::PullRequest
            } else {
                RecordKind::Issue
            },
            title: task.title.clone(),
            body: task.body.clone().unwrap_or_default(),
            state: task.state,
            state_label: task.state_label.clone(),
            url: task.url.clone(),
            author: task.author.clone(),
            labels: task.labels.iter().map(|l| l.name.clone()).collect(),
            assignees: task.assignees.clone(),
            created_at: task.created_at,
            updated_at: task.updated_at,
            closed_at: task.closed_at,
            parent: task.parent.as_ref().map(TaskId::to_string),
            sub_issues: Vec::new(),
            blocked_by: task.blocked_by.iter().map(TaskId::to_string).collect(),
            merge_after: task.merge_after.iter().map(TaskId::to_string).collect(),
            blocked_on: task.blocked_on.clone(),
            comments: task
                .recent_activity
                .iter()
                .take(RECORD_COMMENT_LIMIT)
                .map(RecordComment::of)
                .collect(),
            comments_omitted: task
                .recent_activity
                .len()
                .saturating_sub(RECORD_COMMENT_LIMIT),
            pull_request: is_pr.then(|| PullRequestRecord {
                head_branch: task.branch.clone(),
                base_branch: task.base_branch.clone(),
                additions: task.additions,
                deletions: task.deletions,
                changed_files: task.changed_files,
                ci: task.ci,
                unsuccessful_checks: task
                    .checks
                    .iter()
                    .filter(|check| check.status != CiStatus::Success)
                    .map(|check| check.name.clone())
                    .collect(),
                mergeable: task.mergeable,
                merge_blocked: task.merge_blocked,
                is_behind_base: task.is_behind_base,
                auto_merge_enabled: task.auto_merge_enabled,
                reviewers_pending: task.reviewers.clone(),
                reviews: task.reviews.clone(),
                closes: task.closes_issues.iter().map(TaskId::to_string).collect(),
            }),
            fetched_at,
        }
    }

    /// Fill `sub_issues` with every id in `all` whose parent is this record.
    pub fn with_sub_issues<'a>(mut self, all: impl IntoIterator<Item = &'a Task>) -> Self {
        self.sub_issues = sub_issue_ids(&self.id, all);
        self
    }
}

/// Every id in `all` whose parent is `id`, sorted and deduplicated.
///
/// Free-standing so a caller holding a built record (a list result, past
/// its sort and truncate) can fill children in without rebuilding it.
pub fn sub_issue_ids<'a>(id: &str, all: impl IntoIterator<Item = &'a Task>) -> Vec<String> {
    let mut ids: Vec<String> = all
        .into_iter()
        .filter(|task| {
            task.parent
                .as_ref()
                .is_some_and(|parent| parent.to_string() == id)
        })
        .map(|task| task.id.to_string())
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// The record file the daemon writes into a worktree at spawn: every task
/// linked to this workspace, primary first.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceRecordFile {
    /// Schema of this file, so a future shape change is detectable rather
    /// than silently misread.
    pub schema: u32,
    pub workspace: String,
    pub repo: Option<String>,
    pub branch: String,
    /// The record this workspace is *about* — the PR when there is one,
    /// else the first linked issue. `None` for a repo-less scratch
    /// workspace, which has no tracker record at all.
    pub primary: Option<TaskRecord>,
    /// Every other linked record (the issues a PR closes, a linked Linear
    /// ticket). Excludes `primary`.
    pub also_linked: Vec<TaskRecord>,
    /// When the daemon wrote this file.
    pub written_at: DateTime<Utc>,
}

/// Current [`WorkspaceRecordFile::schema`].
pub const WORKSPACE_RECORD_FILE_SCHEMA: u32 = 1;

/// Path of the record file inside a worktree, relative to its root.
pub const TASK_FILE_RELATIVE_PATH: &str = ".lazybox/task.json";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CheckRun, Label, Mergeable, ReviewStatus, TaskKind, TaskRole};

    fn bare(key: &str, title: &str) -> Task {
        Task {
            id: TaskId {
                source: "github".into(),
                key: key.into(),
            },
            title: title.into(),
            body: None,
            state: TaskState::Open,
            role: TaskRole::Author,
            ci: CiStatus::None,
            review: ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: format!("https://github.com/o/r/issues/{}", key),
            repo: Some("o/r".into()),
            branch: None,
            base_branch: None,
            updated_at: Utc::now(),
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            reviews: vec![],
            approval_policy: Default::default(),
            assignees: vec![],
            author: "someone".into(),
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: Mergeable::Unknown,
            is_behind_base: false,
            merge_blocked: false,
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            changed_files: 0,
            closes_issues: vec![],
            linked_tasks: vec![],
            blocked_by: vec![],
            merge_after: vec![],
            contracts: vec![],
            blocked_on: None,
            parent: None,
            kind: None,
            priority: None,
            state_label: None,
        }
    }

    fn task(kind: TaskKind) -> Task {
        let mut task = bare("o/r#7", "Title");
        task.kind = Some(kind);
        task.body = Some("Body text".into());
        task.labels = vec![Label::new("bug")];
        task
    }

    #[test]
    fn issue_record_carries_the_text_an_agent_would_have_paid_for() {
        let record = TaskRecord::of(&task(TaskKind::Issue), None);
        assert_eq!(record.id, "github:o/r#7");
        assert_eq!(record.number, Some(7));
        assert_eq!(record.kind, RecordKind::Issue);
        assert_eq!(record.title, "Title");
        assert_eq!(record.body, "Body text");
        assert_eq!(record.labels, vec!["bug".to_string()]);
        // An issue has no PR half — an agent must not read a fabricated
        // empty `pull_request` block as "this PR has no checks".
        assert!(record.pull_request.is_none());
    }

    #[test]
    fn pr_record_names_only_the_checks_that_are_not_green() {
        let mut task = task(TaskKind::Pr);
        task.ci = CiStatus::Failure;
        task.checks = vec![
            CheckRun {
                name: "build".into(),
                status: CiStatus::Success,
                url: None,
            },
            CheckRun {
                name: "test".into(),
                status: CiStatus::Failure,
                url: None,
            },
            CheckRun {
                name: "lint".into(),
                status: CiStatus::Pending,
                url: None,
            },
        ];
        let pr = TaskRecord::of(&task, None).pull_request.expect("pr half");
        assert_eq!(pr.ci, CiStatus::Failure);
        assert_eq!(
            pr.unsuccessful_checks,
            vec!["test".to_string(), "lint".to_string()],
            "a green run is already described by `ci`; the red ones are what an agent acts on"
        );
    }

    #[test]
    fn comments_are_capped_and_the_remainder_is_reported() {
        let mut task = task(TaskKind::Issue);
        task.recent_activity = (0..RECORD_COMMENT_LIMIT + 3)
            .map(|i| Activity {
                author: format!("a{i}"),
                body: format!("c{i}"),
                created_at: Utc::now(),
                kind: ActivityKind::Comment,
                node_id: None,
                path: None,
                line: None,
                diff_hunk: None,
                thread_id: None,
            })
            .collect();
        let record = TaskRecord::of(&task, None);
        assert_eq!(record.comments.len(), RECORD_COMMENT_LIMIT);
        // Silent truncation is the failure mode that matters: an agent that
        // believes it has the whole thread will act on a stale conclusion.
        assert_eq!(record.comments_omitted, 3);
    }

    #[test]
    fn sub_issues_are_the_children_pointing_back_at_this_record() {
        let parent = task(TaskKind::Issue);
        let mut child = bare("o/r#8", "Child");
        child.parent = Some(parent.id.clone());
        let unrelated = bare("o/r#9", "Unrelated");
        let record = TaskRecord::of(&parent, None).with_sub_issues([&child, &unrelated]);
        assert_eq!(record.sub_issues, vec!["github:o/r#8".to_string()]);
    }

    #[test]
    fn fetched_at_is_the_daemons_read_time_not_the_upstream_update() {
        let mut task = task(TaskKind::Issue);
        task.updated_at = "2026-09-17T10:00:00Z".parse().expect("ts");
        let fetched = "2026-09-17T12:30:00Z".parse::<DateTime<Utc>>().expect("ts");
        let record = TaskRecord::of(&task, Some(fetched));
        assert_eq!(record.fetched_at, Some(fetched));
        assert_ne!(record.fetched_at, Some(record.updated_at));
    }
}
