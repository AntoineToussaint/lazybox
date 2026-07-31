//! Desktop-facing enrichment of the shared inbox view.
//!
//! The ratatui TUI renders each [`super::VisibleRow`] by resolving its
//! key against the live workspace map it already holds. A non-TUI client
//! (the desktop's `src-tauri`, #732) has the workspace map too, but its
//! thin TypeScript renderer must not reimplement the badge-priority
//! logic ([`lazybox_core::StatusTag::for_task`]) or the grouping/sort
//! from [`super::compute_visible`]. So this module runs the same
//! `compute_visible` pass and attaches, per workspace row, the derived
//! badge fields the renderer needs — status tag, CI/review, unread,
//! role, number, relative-time source — as a serializable
//! [`WorkspaceRow`]. The result, [`InboxView`], is what the desktop
//! emits to its frontend: a row tree plus a key→badge lookup, and
//! nothing the daemon knows that the renderer doesn't (no filesystem
//! paths).

use super::{
    ComputeInputs, ComputeOutcome, RepoSummary, SortMode, VisibleRow, WorkspaceKind,
    compute_visible, pr_number, workspace_needs_attention,
};
use lazybox_core::{CiStatus, Label, ReviewStatus, StatusTag, TaskRole, TaskState, Workspace};
use std::collections::BTreeMap;

/// Everything the desktop renderer needs to draw one workspace row's
/// badges without re-deriving any priority logic. Built from the
/// workspace's primary task via [`lazybox_core::StatusTag::for_task`]
/// and friends; deliberately omits the daemon-only `worktree_path` so
/// the same shape is safe for a future remote client (#738).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct WorkspaceRow {
    /// Session key of the workspace — matches the `VisibleRow::Workspace`
    /// entry that points here.
    pub key: String,
    /// Displayed title: the primary task's title, else the workspace name.
    pub title: String,
    /// Task reference (`owner/repo#123`) or a friendly label for a
    /// task-less workspace.
    pub reference: String,
    /// Trailing PR/issue number, when the reference carries one.
    pub number: Option<u32>,
    /// `owner/repo` for GitHub tasks; `None` for task-less workspaces.
    pub repo: Option<String>,
    pub kind: WorkspaceKind,
    pub role: Option<TaskRole>,
    pub state: Option<TaskState>,
    /// The single-source-of-truth status badge and its human label.
    pub status: StatusTag,
    pub status_label: String,
    pub ci: CiStatus,
    pub review: ReviewStatus,
    pub unread_count: u32,
    /// RFC 3339 timestamp the renderer turns into relative time; `None`
    /// for task-less workspaces with no update stamp.
    pub updated_at: Option<String>,
    pub additions: u32,
    pub deletions: u32,
    pub labels: Vec<Label>,
    pub needs_reply: bool,
    pub last_commenter: Option<String>,
    /// Number of sessions attached — the renderer shows a badge when > 1.
    pub session_count: u32,
    /// Whether this workspace raises any enabled attention signal.
    pub attention: bool,
}

impl WorkspaceRow {
    fn from_workspace(
        key: &str,
        w: &Workspace,
        attention: &lazybox_config::AttentionConfig,
        agents: &std::collections::HashMap<lazybox_core::SessionKey, lazybox_ipc::AgentState>,
    ) -> Self {
        let task = w.primary_task();
        let status = task.map(StatusTag::for_task).unwrap_or(StatusTag::None);
        let reference = match task {
            Some(t) => t.id.key.clone(),
            None => w.name.clone(),
        };
        WorkspaceRow {
            key: key.to_string(),
            title: task
                .map(|t| t.title.clone())
                .unwrap_or_else(|| w.name.clone()),
            reference,
            number: task.and_then(pr_number).map(|n| n as u32),
            repo: task.and_then(|t| t.repo.clone()),
            kind: WorkspaceKind::classify(w),
            role: task.map(|t| t.role),
            state: task.map(|t| t.state),
            status,
            status_label: status.label().to_string(),
            ci: task.map(|t| t.ci).unwrap_or_default(),
            review: task.map(|t| t.review).unwrap_or_default(),
            unread_count: w.unread_count() as u32,
            updated_at: task.map(|t| t.updated_at.to_rfc3339()),
            additions: task.map(|t| t.additions).unwrap_or(0),
            deletions: task.map(|t| t.deletions).unwrap_or(0),
            labels: task.map(|t| t.labels.clone()).unwrap_or_default(),
            needs_reply: task.map(|t| t.needs_reply).unwrap_or(false),
            last_commenter: task.and_then(|t| t.last_commenter.clone()),
            session_count: w.session_count() as u32,
            attention: workspace_needs_attention(w, attention, agents),
        }
    }
}

/// The complete inbox the desktop emits to its renderer: the shared
/// [`VisibleRow`] tree, a key→[`WorkspaceRow`] lookup for badges, the
/// per-repo summaries, and the sort/collapse UI state the header reads.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct InboxView {
    pub rows: Vec<VisibleRow>,
    /// Keyed by session-key string (the `VisibleRow::Workspace` payload).
    pub workspaces: BTreeMap<String, WorkspaceRow>,
    pub summaries: BTreeMap<String, RepoSummary>,
    pub sort_mode: SortMode,
    pub sort_label: String,
    /// Repo labels currently collapsed, so the renderer can pick the
    /// caret glyph without inferring it from the row tree.
    pub collapsed: Vec<String>,
    /// Total workspace rows across all repos.
    pub total: u32,
    /// Sum of unread across all workspace rows.
    pub unread_total: u32,
}

/// Run the shared grouping/sort pass and attach per-row badge data,
/// producing the [`InboxView`] the desktop renders. Pure over the same
/// inputs [`compute_visible`] takes.
pub fn compute_inbox_view(input: ComputeInputs<'_>) -> InboxView {
    let sort_mode = input.sort_mode;
    let collapsed: Vec<String> = input.collapsed_repos.iter().cloned().collect();
    // Copy the borrowed handles the enrichment pass needs before
    // `compute_visible` consumes the input bundle (all are `&`, so this
    // borrows rather than moves; the bundle stays whole for the call).
    let workspaces = input.workspaces;
    let attention = input.attention;
    let agents = input.agents;

    let ComputeOutcome { visible, summaries } = compute_visible(input);

    let mut workspace_rows: BTreeMap<String, WorkspaceRow> = BTreeMap::new();
    let mut total = 0u32;
    let mut unread_total = 0u32;
    for row in &visible {
        if let VisibleRow::Workspace(key) = row {
            let key_str = key.as_str().to_string();
            if let Some(w) = workspaces.get(key) {
                let entry = WorkspaceRow::from_workspace(&key_str, w, attention, agents);
                total += 1;
                unread_total += entry.unread_count;
                workspace_rows.insert(key_str, entry);
            }
        }
    }

    InboxView {
        rows: visible,
        workspaces: workspace_rows,
        summaries,
        sort_mode,
        sort_label: sort_mode.chip_label().to_string(),
        collapsed,
        total,
        unread_total,
    }
}

#[cfg(test)]
mod tests {
    use super::super::{FilterSet, Mailbox};
    use super::*;
    use chrono::{TimeZone, Utc};
    use lazybox_core::{
        CiStatus, Mergeable, ReviewStatus, SessionKey, Task, TaskId, TaskRole, TaskState,
        Workspace, WorkspaceKey,
    };
    use std::collections::{BTreeMap, BTreeSet, HashMap};

    fn now() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 4, 1, 12, 0, 0).unwrap()
    }

    fn task(repo: &str, num: u32, ci: CiStatus) -> Task {
        Task {
            id: TaskId {
                source: "github".into(),
                key: format!("{repo}#{num}"),
            },
            title: format!("Task {num}"),
            body: None,
            state: TaskState::Open,
            role: TaskRole::Author,
            ci,
            review: ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: "x".into(),
            repo: Some(repo.into()),
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
            additions: 3,
            deletions: 1,
            kind: Some(lazybox_core::TaskKind::Pr),
            closes_issues: vec![],
        }
    }

    fn pr_workspace(key: &str, repo: &str, num: u32, ci: CiStatus) -> Workspace {
        let mut ws = Workspace::from_task(task(repo, num, ci), now());
        ws.key = WorkspaceKey(key.into());
        ws
    }

    fn inputs<'a>(
        workspaces: &'a HashMap<SessionKey, Workspace>,
        collapsed: &'a BTreeSet<String>,
        attention: &'a lazybox_config::AttentionConfig,
        agents: &'a HashMap<SessionKey, lazybox_ipc::AgentState>,
        projects: &'a BTreeMap<lazybox_core::ProjectKey, lazybox_core::Project>,
        filters: &'a FilterSet,
    ) -> ComputeInputs<'a> {
        ComputeInputs {
            workspaces,
            mailbox: Mailbox::Inbox,
            filters,
            sort_mode: SortMode::ByRoleSplit,
            show_inactive_in_inbox: false,
            projects,
            collapsed_repos: collapsed,
            attention,
            agents,
            now: now(),
            search: None,
        }
    }

    #[test]
    fn view_carries_badge_data_for_each_workspace_row() {
        let mut ws = HashMap::new();
        let w = pr_workspace("k1", "owner/r", 7, CiStatus::Failure);
        ws.insert(SessionKey::from(&w.key), w);
        let collapsed = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let agents = HashMap::new();
        let projects = BTreeMap::new();
        let filters = FilterSet::new();
        let view = compute_inbox_view(inputs(&ws, &collapsed, &att, &agents, &projects, &filters));

        assert_eq!(view.total, 1);
        assert_eq!(view.sort_label, "split");
        let row = view.workspaces.get("k1").expect("workspace row data");
        assert_eq!(row.status, StatusTag::CiFailed);
        assert_eq!(row.status_label, "CI FAIL");
        assert_eq!(row.number, Some(7));
        assert_eq!(row.repo.as_deref(), Some("owner/r"));
        assert_eq!(row.kind, WorkspaceKind::Pr);
        assert!(row.attention, "a failing PR needs attention");
        assert!(row.updated_at.is_some());
    }

    #[test]
    fn split_sort_emits_a_kind_header_and_rows_match_the_lookup() {
        let mut ws = HashMap::new();
        let w = pr_workspace("k1", "owner/r", 7, CiStatus::Success);
        ws.insert(SessionKey::from(&w.key), w);
        let collapsed = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let agents = HashMap::new();
        let projects = BTreeMap::new();
        let filters = FilterSet::new();
        let view = compute_inbox_view(inputs(&ws, &collapsed, &att, &agents, &projects, &filters));

        assert!(matches!(view.rows[0], VisibleRow::RepoHeader(_)));
        assert!(matches!(
            view.rows[1],
            VisibleRow::KindHeader(WorkspaceKind::Pr)
        ));
        assert!(matches!(view.rows[2], VisibleRow::Workspace(_)));
        // Every Workspace row has a matching lookup entry.
        for row in &view.rows {
            if let VisibleRow::Workspace(key) = row {
                assert!(view.workspaces.contains_key(key.as_str()));
            }
        }
    }

    #[test]
    fn collapsed_repo_is_reported_and_hides_rows() {
        let mut ws = HashMap::new();
        let w = pr_workspace("k1", "owner/r", 7, CiStatus::Success);
        ws.insert(SessionKey::from(&w.key), w);
        let mut collapsed = BTreeSet::new();
        collapsed.insert("owner/r".to_string());
        let att = lazybox_config::AttentionConfig::default();
        let agents = HashMap::new();
        let projects = BTreeMap::new();
        let filters = FilterSet::new();
        let view = compute_inbox_view(inputs(&ws, &collapsed, &att, &agents, &projects, &filters));

        assert_eq!(view.collapsed, vec!["owner/r".to_string()]);
        assert_eq!(view.total, 0, "collapsed repo emits no workspace rows");
        assert_eq!(view.summaries.get("owner/r").map(|s| s.active), Some(1));
    }
}
