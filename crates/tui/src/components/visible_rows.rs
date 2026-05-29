//! Pure builder for the sidebar's `Vec<VisibleRow>`.
//!
//! What "visible" means: the workspaces in the focused mailbox,
//! grouped by their parent Project, with a `(no repo)` bucket for
//! task-less / project-less workspaces. Each group emits a
//! `RepoHeader`; if not collapsed, the workspace rows follow (and
//! their session sub-rows when a workspace has 2+ sessions).
//!
//! Extracted from `Sidebar::recompute_visible_inner` so the
//! classification matrix — which project a workspace lands under,
//! whether an empty subscribed repo emits a header — is testable
//! as a free function with no `Sidebar` instance. Cursor
//! preservation stays on `Sidebar` (it reads/writes `self.cursor`);
//! this function is purely the rebuild half.

use crate::components::sidebar::{
    Mailbox, RepoSummary, RoleFilter, SortMode, VisibleRow, WorkspaceKind, mailbox_membership,
    role_rank,
};
use pilot_core::{Project, ProjectKey, SessionKey, Workspace};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// Output of `compute_visible`. Held together because the
/// summaries are derived during the same pass that builds the
/// row list — re-deriving them would duplicate the grouping.
pub struct ComputeOutcome {
    pub visible: Vec<VisibleRow>,
    pub summaries: BTreeMap<String, RepoSummary>,
}

/// Inputs to the visible-rows pass. Borrowed so the function
/// doesn't take ownership; lifetime threads from the caller.
pub struct ComputeInputs<'a> {
    pub workspaces: &'a HashMap<SessionKey, Workspace>,
    pub mailbox: Mailbox,
    /// Quick role-based filter layered on top of the mailbox. `All`
    /// (default) is the no-op identity. See `RoleFilter::accepts`.
    pub role_filter: RoleFilter,
    /// How to order workspaces within each repo group. `Default`
    /// (recency) is the legacy behavior; `ByRole` / `ByRoleSplit`
    /// promote Author rows then Reviewer etc. See [`SortMode`].
    pub sort_mode: SortMode,
    pub show_inactive_in_inbox: bool,
    /// Projects mirrored from the daemon's project table — plus
    /// synthetic entries for the user's subscribed scopes (the model
    /// merges them in via `refresh_subscribed_projects`). Each entry
    /// emits a sidebar header so a freshly-subscribed repo with no
    /// workspaces still shows up.
    pub projects: &'a BTreeMap<ProjectKey, Project>,
    pub collapsed_repos: &'a BTreeSet<String>,
    pub attention: &'a pilot_config::AttentionConfig,
    pub agents_asking: &'a HashSet<SessionKey>,
    pub now: chrono::DateTime<chrono::Utc>,
}

const NO_REPO: &str = "(no repo)";

/// Pure function: build the sidebar's visible-row list + per-repo
/// summaries from the workspace map, mailbox filter, and
/// repo-subscription config. No `Sidebar` borrow.
pub fn compute_visible(input: ComputeInputs<'_>) -> ComputeOutcome {
    // Step 1: filter by mailbox membership. Uses the cell-tested
    // `mailbox_membership` predicate so snooze/merged/empty cases
    // can't drift from their unit tests.
    let filtered: Vec<(&SessionKey, &Workspace)> = input
        .workspaces
        .iter()
        .filter(|(_, w)| {
            mailbox_membership(w, input.mailbox, input.now, input.show_inactive_in_inbox)
        })
        .filter(|(_, w)| input.role_filter.accepts(w.primary_task().map(|t| t.role)))
        .collect();

    // Step 2: bucket by project. A workspace's parent project is
    // looked up via `pilot_core::workspace_project_key` → resolved
    // through the daemon's project table to get the display name.
    // Workspaces with no project_key (back-compat reads of pre-
    // Stage-1 records OR orphans whose task.repo failed to derive)
    // land under `(no repo)`.
    let mut by_repo: BTreeMap<String, Vec<(&SessionKey, &Workspace)>> = BTreeMap::new();
    for (k, w) in &filtered {
        by_repo
            .entry(group_label(w, input.projects))
            .or_default()
            .push((k, w));
    }

    // Step 3: sort each bucket. `Recent` keeps the legacy recency
    // order. `ByRole` promotes Author rows, then Reviewer, then
    // Assignee, then Mentioned. `ByRoleSplit` additionally pushes
    // PR workspaces above issue workspaces so a single linear pass
    // in step 5 can drop a `KindHeader` between the two sections.
    // Within each role/kind band, recency breaks ties — so the
    // most recently updated PR-you-authored is still on top.
    for rows in by_repo.values_mut() {
        rows.sort_by(|(ka, a), (kb, b)| {
            let a_ts = a
                .primary_task()
                .map(|t| t.updated_at)
                .unwrap_or(a.created_at);
            let b_ts = b
                .primary_task()
                .map(|t| t.updated_at)
                .unwrap_or(b.created_at);
            let recency = b_ts.cmp(&a_ts);
            let tie = ka.as_str().cmp(kb.as_str());
            let role_cmp = || {
                role_rank(a.primary_task().map(|t| t.role))
                    .cmp(&role_rank(b.primary_task().map(|t| t.role)))
            };
            // `WorkspaceKind` derives `Ord` with `Pr < Issue`, so
            // a plain `cmp` does the PR-first ordering.
            let kind_cmp = || WorkspaceKind::classify(a).cmp(&WorkspaceKind::classify(b));
            match input.sort_mode {
                SortMode::Recent => recency.then(tie),
                SortMode::ByRole => role_cmp().then(recency).then(tie),
                SortMode::ByRoleSplit => kind_cmp().then_with(role_cmp).then(recency).then(tie),
            }
        });
    }

    // Step 4: collect the header set. Every project from the
    // (daemon-mirrored + scope-synthesized) projects map gets a
    // header in the Inbox mailbox, so an empty project still shows
    // up. Omitted from Inactive / Snoozed (alternate views, not
    // subscriptions).
    let mut all_repos: BTreeSet<String> = by_repo.keys().cloned().collect();
    if input.mailbox == Mailbox::Inbox {
        all_repos.extend(input.projects.values().map(|p| p.name.clone()));
    }

    // Step 5: emit headers + workspace rows + session sub-rows.
    let mut visible: Vec<VisibleRow> = Vec::with_capacity(filtered.len() + all_repos.len() + 4);
    let mut summaries: BTreeMap<String, RepoSummary> = BTreeMap::new();
    for repo in &all_repos {
        visible.push(VisibleRow::RepoHeader(repo.clone()));
        let mut summary = RepoSummary::default();
        if let Some(rows) = by_repo.get(repo) {
            summary.active = rows.len();
            for (_, w) in rows {
                if crate::components::sidebar::workspace_needs_attention(
                    w,
                    input.attention,
                    input.agents_asking,
                ) {
                    summary.attention += 1;
                }
            }
            if !input.collapsed_repos.contains(repo) {
                // ByRoleSplit drops a `KindHeader` between the PR
                // workspaces and the Issue workspaces of this repo.
                // Step 3 already sorted PRs ahead of issues, so a
                // single linear pass detects the boundary cleanly.
                // In other sort modes the kind header is suppressed.
                let split = input.sort_mode == SortMode::ByRoleSplit;
                let mut prev_kind: Option<WorkspaceKind> = None;
                for (k, w) in rows {
                    let cur_kind = WorkspaceKind::classify(w);
                    if split && prev_kind != Some(cur_kind) {
                        visible.push(VisibleRow::KindHeader(cur_kind));
                        prev_kind = Some(cur_kind);
                    }
                    visible.push(VisibleRow::Workspace((*k).clone()));
                    // Session sub-rows only when 2+ sessions —
                    // showing the single-session case would be
                    // visual noise (the workspace row itself
                    // represents that session).
                    if w.session_count() >= 2 {
                        let mut sessions: Vec<&pilot_core::WorkspaceSession> =
                            w.sessions.iter().collect();
                        sessions.sort_by_key(|s| s.created_at);
                        for s in sessions {
                            visible.push(VisibleRow::Session {
                                workspace: (*k).clone(),
                                session_id: s.id,
                            });
                        }
                    }
                }
            }
        }
        summaries.insert(repo.clone(), summary);
    }

    ComputeOutcome { visible, summaries }
}

fn group_label(w: &Workspace, projects: &BTreeMap<ProjectKey, Project>) -> String {
    // Prefer the project_key → project record path: the display
    // name matches the standalone Project header so they collapse
    // into one bucket.
    if let Some(pk) = pilot_core::workspace_project_key(w)
        && let Some(p) = projects.get(&pk)
    {
        return p.name.clone();
    }
    // Workspace knows its project but we haven't seen the record
    // yet (startup race, or polling hasn't completed). Fall back
    // to the workspace's primary task's repo — for github
    // workspaces this is the same `"owner/repo"` string the
    // project record carries, so once polling registers the
    // project the bucket label is identical and no row jumps.
    if let Some(repo) = w.primary_task().and_then(|t| t.repo.clone())
        && !repo.is_empty()
    {
        return repo;
    }
    // Orphan: no project_key AND no upstream task. The old
    // "(sandbox)" bucket retired in Stage 4 — those workspaces
    // land here too. A future migration can lift them into a
    // local Project by name.
    NO_REPO.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone, Utc};
    use pilot_core::{
        CiStatus, ReviewStatus, Task, TaskId, TaskRole, TaskState, Workspace, WorkspaceKey,
    };

    fn fixed_time() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 4, 1, 12, 0, 0).unwrap()
    }

    fn workspace_with_task(key_str: &str, repo: Option<&str>, minutes_old: i64) -> Workspace {
        let task = Task {
            id: TaskId {
                source: "github".into(),
                key: format!("owner/{}#1", repo.unwrap_or("repo")),
            },
            title: "x".into(),
            body: None,
            state: TaskState::Open,
            role: TaskRole::Author,
            ci: CiStatus::None,
            review: ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: "x".into(),
            repo: repo.map(String::from),
            branch: Some("main".into()),
            base_branch: None,
            updated_at: fixed_time() - Duration::minutes(minutes_old),
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: pilot_core::Mergeable::Mergeable,
            is_behind_base: false,
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            closes_issues: vec![],
        };
        let mut ws = Workspace::from_task(task, fixed_time());
        ws.key = WorkspaceKey(key_str.into());
        ws
    }

    fn inputs<'a>(
        workspaces: &'a HashMap<SessionKey, Workspace>,
        _subscribed: &'a BTreeSet<String>,
        collapsed: &'a BTreeSet<String>,
        attention: &'a pilot_config::AttentionConfig,
        asking: &'a HashSet<SessionKey>,
        projects: &'a BTreeMap<ProjectKey, Project>,
    ) -> ComputeInputs<'a> {
        ComputeInputs {
            workspaces,
            mailbox: Mailbox::Inbox,
            role_filter: RoleFilter::All,
            sort_mode: SortMode::Recent,
            show_inactive_in_inbox: false,
            projects,
            collapsed_repos: collapsed,
            attention,
            agents_asking: asking,
            now: fixed_time(),
        }
    }

    /// Empty workspace map + no subscribed repos = no rows.
    #[test]
    fn empty_inputs_produce_empty_visible() {
        let ws = HashMap::new();
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = pilot_config::AttentionConfig::default();
        let asking = HashSet::new();
        let projects = BTreeMap::new();
        let out = compute_visible(inputs(&ws, &sub, &col, &att, &asking, &projects));
        assert!(out.visible.is_empty());
        assert!(out.summaries.is_empty());
    }

    /// One workspace under one repo: header + workspace row.
    #[test]
    fn single_workspace_emits_header_then_row() {
        let mut ws = HashMap::new();
        let w = workspace_with_task("k1", Some("owner/r"), 10);
        ws.insert(SessionKey::from(&w.key), w);
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = pilot_config::AttentionConfig::default();
        let asking = HashSet::new();
        let projects = BTreeMap::new();
        let out = compute_visible(inputs(&ws, &sub, &col, &att, &asking, &projects));
        assert_eq!(out.visible.len(), 2);
        assert!(matches!(out.visible[0], VisibleRow::RepoHeader(_)));
        assert!(matches!(out.visible[1], VisibleRow::Workspace(_)));
    }

    /// Workspaces in different repos: one header each, alphabetical
    /// repo order (BTreeMap), workspaces under each.
    #[test]
    fn multiple_repos_grouped_and_alphabetized() {
        let mut ws = HashMap::new();
        let a = workspace_with_task("ka", Some("owner/a"), 10);
        let b = workspace_with_task("kb", Some("owner/b"), 10);
        ws.insert(SessionKey::from(&a.key), a);
        ws.insert(SessionKey::from(&b.key), b);
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = pilot_config::AttentionConfig::default();
        let asking = HashSet::new();
        let projects = BTreeMap::new();
        let out = compute_visible(inputs(&ws, &sub, &col, &att, &asking, &projects));
        // header(a) + ws + header(b) + ws.
        assert_eq!(out.visible.len(), 4);
        if let VisibleRow::RepoHeader(name) = &out.visible[0] {
            assert_eq!(name, "owner/a");
        } else {
            panic!("expected RepoHeader, got {:?}", out.visible[0]);
        }
        if let VisibleRow::RepoHeader(name) = &out.visible[2] {
            assert_eq!(name, "owner/b");
        } else {
            panic!("expected RepoHeader, got {:?}", out.visible[2]);
        }
    }

    /// Collapsed repo: header only, workspace rows under it are
    /// suppressed.
    #[test]
    fn collapsed_repo_emits_header_only() {
        let mut ws = HashMap::new();
        let w = workspace_with_task("k1", Some("owner/r"), 10);
        ws.insert(SessionKey::from(&w.key), w);
        let sub = BTreeSet::new();
        let mut col = BTreeSet::new();
        col.insert("owner/r".to_string());
        let att = pilot_config::AttentionConfig::default();
        let asking = HashSet::new();
        let projects = BTreeMap::new();
        let out = compute_visible(inputs(&ws, &sub, &col, &att, &asking, &projects));
        assert_eq!(out.visible.len(), 1);
        assert!(matches!(out.visible[0], VisibleRow::RepoHeader(_)));
        // Summary still counts the active workspace.
        assert_eq!(out.summaries.get("owner/r").unwrap().active, 1);
    }

    /// Workspace with project_key set lands under its project's
    /// display-name header, NOT the legacy `task.repo`-derived one.
    /// Drives the post-Stage-4 grouping invariant.
    #[test]
    fn workspaces_group_under_project_display_name() {
        let mut ws = HashMap::new();
        let mut w = workspace_with_task("k1", Some("owner/r"), 10);
        let pk = ProjectKey::github("acme", "tool");
        w.project_key = Some(pk.clone());
        ws.insert(SessionKey::from(&w.key), w);
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = pilot_config::AttentionConfig::default();
        let asking = HashSet::new();
        let mut projects = BTreeMap::new();
        projects.insert(
            pk.clone(),
            Project::new(pk, "acme/tool", chrono::Utc::now()),
        );
        let out = compute_visible(inputs(&ws, &sub, &col, &att, &asking, &projects));
        assert!(
            out.summaries.contains_key("acme/tool"),
            "header should be the project's display name"
        );
        assert!(!out.summaries.contains_key("owner/r"));
    }

    /// Project with no workspace yields a header in Inbox
    /// (so the user can see "I'm subscribed but nothing's in flight").
    #[test]
    fn empty_project_emits_header_in_inbox() {
        let ws = HashMap::new();
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = pilot_config::AttentionConfig::default();
        let asking = HashSet::new();
        let mut projects = BTreeMap::new();
        let pk = ProjectKey::github("owner", "empty");
        projects.insert(
            pk.clone(),
            Project::new(pk, "owner/empty", chrono::Utc::now()),
        );
        let out = compute_visible(inputs(&ws, &sub, &col, &att, &asking, &projects));
        assert_eq!(out.visible.len(), 1);
        assert!(matches!(&out.visible[0], VisibleRow::RepoHeader(name) if name == "owner/empty"));
    }

    /// Same setup, but Inactive mailbox: the empty project header
    /// is NOT shown (alternate view, not a subscription).
    #[test]
    fn empty_project_skipped_in_inactive() {
        let ws = HashMap::new();
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = pilot_config::AttentionConfig::default();
        let asking = HashSet::new();
        let mut projects = BTreeMap::new();
        let pk = ProjectKey::github("owner", "empty");
        projects.insert(
            pk.clone(),
            Project::new(pk, "owner/empty", chrono::Utc::now()),
        );
        let mut i = inputs(&ws, &sub, &col, &att, &asking, &projects);
        i.mailbox = Mailbox::Inactive;
        let out = compute_visible(i);
        assert!(out.visible.is_empty());
    }

    /// Two workspaces in same repo: sorted by updated_at desc.
    #[test]
    fn same_repo_workspaces_sorted_by_updated_at_desc() {
        let mut ws = HashMap::new();
        // `older` was updated 60 min ago, `newer` was updated 10 min ago.
        let older = workspace_with_task("k_older", Some("owner/r"), 60);
        let newer = workspace_with_task("k_newer", Some("owner/r"), 10);
        ws.insert(SessionKey::from(&older.key), older);
        ws.insert(SessionKey::from(&newer.key), newer);
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = pilot_config::AttentionConfig::default();
        let asking = HashSet::new();
        let projects = BTreeMap::new();
        let out = compute_visible(inputs(&ws, &sub, &col, &att, &asking, &projects));
        // [header, newer, older].
        let keys: Vec<&str> = out
            .visible
            .iter()
            .filter_map(|r| match r {
                VisibleRow::Workspace(k) => Some(k.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(keys, vec!["k_newer", "k_older"]);
    }

    /// A workspace with no primary task (no .repo set) lands under
    /// `(no repo)`.
    #[test]
    fn workspace_with_no_repo_grouped_under_no_repo() {
        let mut ws = HashMap::new();
        let w = workspace_with_task("k1", None, 10);
        ws.insert(SessionKey::from(&w.key), w);
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = pilot_config::AttentionConfig::default();
        let asking = HashSet::new();
        let projects = BTreeMap::new();
        let out = compute_visible(inputs(&ws, &sub, &col, &att, &asking, &projects));
        assert!(out.summaries.contains_key("(no repo)"));
    }

    /// Active count in the summary matches the number of visible
    /// workspaces under the repo, regardless of collapse state.
    #[test]
    fn summary_active_counts_all_workspaces_even_when_collapsed() {
        let mut ws = HashMap::new();
        for i in 0..3 {
            let mut w = workspace_with_task(&format!("k{i}"), Some("owner/r"), 10 + i);
            w.key = WorkspaceKey(format!("k{i}"));
            ws.insert(SessionKey::from(&w.key), w);
        }
        let sub = BTreeSet::new();
        let mut col = BTreeSet::new();
        col.insert("owner/r".to_string());
        let att = pilot_config::AttentionConfig::default();
        let asking = HashSet::new();
        let projects = BTreeMap::new();
        let out = compute_visible(inputs(&ws, &sub, &col, &att, &asking, &projects));
        assert_eq!(out.summaries.get("owner/r").unwrap().active, 3);
    }
}
