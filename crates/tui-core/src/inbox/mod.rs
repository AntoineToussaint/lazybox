//! Client-free inbox view logic: grouping, sort, filter, and search
//! over `lazybox-core`/`lazybox-ipc` domain types. No render context —
//! both the ratatui TUI and the desktop client build their sidebar
//! from [`compute_visible`], so the two clients cannot drift.
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

mod attention;
mod filter;
mod model;

pub use attention::{
    AttentionSignal, INACTIVE_GRACE, attention_gate, mailbox_membership,
    workspace_attention_signals, workspace_needs_attention,
};
pub use filter::{Filter, FilterAxis, FilterCtx, FilterEntry, FilterMenuItem, FilterSet};
pub use model::{
    Mailbox, RepoSummary, SearchState, SortMode, VisibleRow, WorkspaceKind, role_rank,
};

use lazybox_core::{Project, ProjectKey, SessionKey, Task, Workspace};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Output of `compute_visible`. Held together because the
/// summaries are derived during the same pass that builds the
/// row list — re-deriving them would duplicate the grouping.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct ComputeOutcome {
    pub visible: Vec<VisibleRow>,
    pub summaries: BTreeMap<String, RepoSummary>,
}

/// Inputs to the visible-rows pass. Borrowed so the function
/// doesn't take ownership; lifetime threads from the caller.
///
/// This is the client-free entry point's single argument bundle: a
/// non-TUI caller (the desktop's `src-tauri`) builds the same
/// workspace map + agent-state map + [`FilterSet`] + [`SortMode`] +
/// optional [`SearchState`] its gateway connection exposes and gets
/// back a [`ComputeOutcome`].
pub struct ComputeInputs<'a> {
    pub workspaces: &'a HashMap<SessionKey, Workspace>,
    pub mailbox: Mailbox,
    /// Composable predicate filter layered on top of the mailbox. An
    /// empty set is the no-op identity. See [`FilterSet::accepts`].
    pub filters: &'a FilterSet,
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
    /// Repo group names the user pinned to the top, in pin order.
    /// Pinned groups render first (in this order); every other group
    /// keeps the algorithmic (alphabetical) order. A name here that
    /// has no matching group this pass is simply ignored.
    pub pinned_repos: &'a [String],
    /// Workspace keys the user has starred ("focused"), in focus order.
    /// A visible starred workspace is lifted out of its repo group into
    /// the synthetic `★ Focused` section rendered first, in this order.
    /// A key here with no matching visible workspace this pass is simply
    /// ignored. The manual, per-workspace counterpart to [`Self::pinned_repos`].
    pub focused_workspaces: &'a [SessionKey],
    pub attention: &'a lazybox_config::AttentionConfig,
    pub agents: &'a HashMap<SessionKey, lazybox_ipc::AgentState>,
    pub now: chrono::DateTime<chrono::Utc>,
    /// Free-text search, or `None`. When `Some` with a non-empty
    /// query, a scoped search (`scope: Some`) keeps only that
    /// project's fuzzy-matching rows while other projects pass through
    /// untouched; a global search (`scope: None`) keeps only
    /// fuzzy-matching rows across every project. See [`search_matches`].
    pub search: Option<&'a SearchState>,
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
        .filter(|(_, w)| {
            input.filters.accepts(&FilterCtx {
                w,
                agents: input.agents,
            })
        })
        // Free-text search. A scoped search (`scope: Some`) filters
        // only the matching project's rows and leaves every other
        // project fully visible; a global search (`scope: None`)
        // filters every repo group at once.
        .filter(|(_, w)| match input.search {
            Some(s) if !s.query.is_empty() => match &s.scope {
                None => search_matches(&s.query, w),
                Some(scope) if group_label(w, input.projects, input.workspaces) == *scope => {
                    search_matches(&s.query, w)
                }
                Some(_) => true,
            },
            _ => true,
        })
        .collect();

    // Step 1b: lift the starred ("focused") workspaces out of the
    // filtered set. A visible starred workspace is gathered into the
    // synthetic `★ Focused` section — rendered first, in focus order —
    // regardless of which repo it belongs to, and is NOT re-listed
    // under its repo group. A star naming a workspace not visible in
    // the current mailbox/filter this pass is simply skipped, exactly
    // as a pin naming an absent repo is (Step 4b). Focus is the manual,
    // per-workspace counterpart to the repo pin.
    let focused_set: BTreeSet<&str> = input
        .focused_workspaces
        .iter()
        .map(|k| k.as_str())
        .collect();
    let filtered_by_key: BTreeMap<&str, (&SessionKey, &Workspace)> = filtered
        .iter()
        .map(|(k, w)| (k.as_str(), (*k, *w)))
        .collect();
    let focused_rows: Vec<(&SessionKey, &Workspace)> = input
        .focused_workspaces
        .iter()
        .filter_map(|key| filtered_by_key.get(key.as_str()).copied())
        .collect();

    // Step 2: bucket the non-focused workspaces by project. A
    // workspace's parent project is looked up via
    // `lazybox_core::workspace_project_key` → resolved through the
    // daemon's project table to get the display name. Workspaces with
    // no project_key (back-compat reads of pre-Stage-1 records OR
    // orphans whose task.repo failed to derive) land under `(no repo)`.
    let mut by_repo: BTreeMap<String, Vec<(&SessionKey, &Workspace)>> = BTreeMap::new();
    for (k, w) in &filtered {
        if focused_set.contains(k.as_str()) {
            continue;
        }
        by_repo
            .entry(group_label(w, input.projects, input.workspaces))
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
    // subscriptions), and omitted while a filter is active — a
    // narrowed view listing every subscribed repo as an empty header
    // buries the few matches behind a wall of chrome, so under a
    // filter only repos with matching workspaces get a header. A
    // *global* search (`scope: None`, the desktop's global `/`) narrows
    // the same way, so it suppresses them too; the TUI's scoped search
    // leaves other projects untouched, so it keeps their headers.
    let global_search = input
        .search
        .is_some_and(|s| !s.query.is_empty() && s.scope.is_none());
    let mut all_repos: BTreeSet<String> = by_repo.keys().cloned().collect();
    if input.mailbox == Mailbox::Inbox && input.filters.is_empty() && !global_search {
        all_repos.extend(
            input
                .projects
                .values()
                .map(|p| project_label(p, input.workspaces)),
        );
    }

    // Step 4b: order the groups. Pinned groups lead, in the user's pin
    // order (skipping pins with no group this pass); the rest follow in
    // the algorithmic `BTreeSet` (alphabetical) order. Pinning overrides
    // the default order only for the pinned repos and leaves everything
    // else untouched — the "pinned > algorithmic" compose rule (#760).
    let mut ordered_repos: Vec<String> = Vec::with_capacity(all_repos.len());
    for pinned in input.pinned_repos {
        if all_repos.remove(pinned) {
            ordered_repos.push(pinned.clone());
        }
    }
    ordered_repos.extend(all_repos);

    // Step 5: emit headers + workspace rows + session sub-rows.
    let mut visible: Vec<VisibleRow> = Vec::with_capacity(filtered.len() + ordered_repos.len() + 4);
    let mut summaries: BTreeMap<String, RepoSummary> = BTreeMap::new();

    // Step 5a: the `★ Focused` section, first and above every repo. Only
    // emitted when at least one starred workspace is visible this pass.
    // Session sub-rows follow the same 2+-sessions rule as repo rows;
    // no KindHeader split — the section is a flat, cross-repo shortlist.
    if !focused_rows.is_empty() {
        visible.push(VisibleRow::FocusedHeader);
        for (k, w) in &focused_rows {
            visible.push(VisibleRow::Workspace((*k).clone()));
            if w.session_count() >= 2 {
                let mut sessions: Vec<&lazybox_core::WorkspaceSession> =
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

    for repo in &ordered_repos {
        visible.push(VisibleRow::RepoHeader(repo.clone()));
        let mut summary = RepoSummary::default();
        if let Some(rows) = by_repo.get(repo) {
            summary.active = rows.len();
            for (_, w) in rows {
                if workspace_needs_attention(w, input.attention, input.agents) {
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
                        let mut sessions: Vec<&lazybox_core::WorkspaceSession> =
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

fn github_task_repo(w: &Workspace, project_key: &ProjectKey) -> Option<String> {
    if project_key.source_prefix() != "github" {
        return None;
    }
    let task = w.primary_task()?;
    let repo = task.repo.as_deref()?.trim();
    (!repo.is_empty() && lazybox_core::project_key_for_task(task).as_ref() == Some(project_key))
        .then(|| repo.to_string())
}

pub fn project_label(project: &Project, workspaces: &HashMap<SessionKey, Workspace>) -> String {
    workspaces
        .values()
        .filter(|w| lazybox_core::workspace_project_key(w).as_ref() == Some(&project.key))
        .find_map(|w| github_task_repo(w, &project.key))
        .unwrap_or_else(|| project.display_name())
}

pub fn group_label(
    w: &Workspace,
    projects: &BTreeMap<ProjectKey, Project>,
    workspaces: &HashMap<SessionKey, Workspace>,
) -> String {
    // A matching GitHub task carries the owner/repo boundary that the
    // flat project key loses. Otherwise use the project record label so
    // moved/local workspaces still group under their chosen project.
    if let Some(pk) = lazybox_core::workspace_project_key(w) {
        if let Some(repo) = github_task_repo(w, &pk) {
            return repo;
        }
        if let Some(p) = projects.get(&pk) {
            return project_label(p, workspaces);
        }
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

/// Convert `Task.id.key` (e.g. `"owner/repo#1234"`) to the trailing
/// integer. Returns `None` when there's no `#`-suffix or the suffix
/// isn't a number — those cases shouldn't happen for GitHub-derived
/// tasks today, but we don't want to panic if Linear or a custom
/// provider lands here. A pure dependency of both [`search_matches`]
/// and the sidebar row renderer (`task_label::pr_number_color` colors
/// the number the TUI side keeps).
pub fn pr_number(task: &Task) -> Option<u64> {
    let key = task.id.key.as_str();
    let (_, num) = key.rsplit_once('#')?;
    num.parse().ok()
}

/// Does `query` match `w`? Matches when the query is a case-insensitive
/// fuzzy (subsequence) match on the workspace's displayed title, OR a
/// substring match on any of its searchable metadata: PR/issue number,
/// repo, labels, or requested reviewers / assignees. A leading `#` on
/// the query is ignored so both `100` and `#100` find issue #100. An
/// empty query (after trimming) matches everything — callers guard
/// against that, but it keeps the function total.
pub fn search_matches(query: &str, w: &Workspace) -> bool {
    let q = query.trim().trim_start_matches('#').to_lowercase();
    if q.is_empty() {
        return true;
    }
    let task = w.primary_task();
    if let Some(n) = task.and_then(pr_number)
        && n.to_string().contains(&q)
    {
        return true;
    }
    if let Some(t) = task {
        // Substring matches on metadata: repo, labels, and the people
        // requested on the task (reviewers / assignees).
        if t.repo
            .as_deref()
            .is_some_and(|r| r.to_lowercase().contains(&q))
            || t.labels.iter().any(|l| l.name.to_lowercase().contains(&q))
            || t.reviewers.iter().any(|r| r.to_lowercase().contains(&q))
            || t.assignees.iter().any(|a| a.to_lowercase().contains(&q))
        {
            return true;
        }
    }
    // Same title the workspace row renders: task title, else the
    // workspace's own name.
    let title = task
        .map(|t| t.title.as_str())
        .unwrap_or_else(|| w.name.as_str());
    is_subsequence(&title.to_lowercase(), &q)
}

/// True when every char of `needle` appears in `haystack` in order
/// (not necessarily contiguous) — the fzf-style loose match.
fn is_subsequence(haystack: &str, needle: &str) -> bool {
    let mut hay = haystack.chars();
    needle.chars().all(|nc| hay.any(|hc| hc == nc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone, Utc};
    use lazybox_core::{
        CiStatus, ReviewStatus, Task, TaskId, TaskRole, TaskState, Workspace, WorkspaceKey,
    };

    fn fixed_time() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 4, 1, 12, 0, 0).unwrap()
    }

    fn workspace_with_task(key_str: &str, repo: Option<&str>, minutes_old: i64) -> Workspace {
        let task = Task {
            author: String::new(),
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
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Mergeable,
            is_behind_base: false,
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            kind: None,
            closes_issues: vec![],
            priority: None,
            state_label: None,
        };
        let mut ws = Workspace::from_task(task, fixed_time());
        ws.key = WorkspaceKey(key_str.into());
        ws
    }

    fn inputs<'a>(
        workspaces: &'a HashMap<SessionKey, Workspace>,
        _subscribed: &'a BTreeSet<String>,
        collapsed: &'a BTreeSet<String>,
        attention: &'a lazybox_config::AttentionConfig,
        asking: &'a HashMap<SessionKey, lazybox_ipc::AgentState>,
        projects: &'a BTreeMap<ProjectKey, Project>,
    ) -> ComputeInputs<'a> {
        static NO_FILTERS: FilterSet = FilterSet::new();
        ComputeInputs {
            workspaces,
            mailbox: Mailbox::Inbox,
            filters: &NO_FILTERS,
            sort_mode: SortMode::Recent,
            show_inactive_in_inbox: false,
            projects,
            collapsed_repos: collapsed,
            pinned_repos: &[],
            focused_workspaces: &[],
            attention,
            agents: asking,
            now: fixed_time(),
            search: None,
        }
    }

    /// Empty workspace map + no subscribed repos = no rows.
    #[test]
    fn empty_inputs_produce_empty_visible() {
        let ws = HashMap::new();
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
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
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
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
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
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

    /// A pinned repo floats to the top ahead of the alphabetical
    /// order; unpinned repos keep their relative (alphabetical) order
    /// below it (#760).
    #[test]
    fn pinned_repo_floats_above_alphabetical_order() {
        let mut ws = HashMap::new();
        for (k, repo) in [("ka", "owner/a"), ("kb", "owner/b"), ("kc", "owner/c")] {
            let w = workspace_with_task(k, Some(repo), 10);
            ws.insert(SessionKey::from(&w.key), w);
        }
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();
        let pins = vec!["owner/c".to_string()];
        let mut inp = inputs(&ws, &sub, &col, &att, &asking, &projects);
        inp.pinned_repos = &pins;
        let out = compute_visible(inp);
        let headers: Vec<&str> = out
            .visible
            .iter()
            .filter_map(|r| match r {
                VisibleRow::RepoHeader(name) => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(headers, ["owner/c", "owner/a", "owner/b"]);
    }

    /// Multiple pins render first in pin order (not alphabetical), and
    /// a pin naming a repo with no rows this pass is skipped without
    /// disturbing the rest.
    #[test]
    fn multiple_pins_keep_pin_order_and_skip_absent() {
        let mut ws = HashMap::new();
        for (k, repo) in [("ka", "owner/a"), ("kb", "owner/b"), ("kc", "owner/c")] {
            let w = workspace_with_task(k, Some(repo), 10);
            ws.insert(SessionKey::from(&w.key), w);
        }
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();
        // Pin c then a; "owner/z" has no group and must be ignored.
        let pins = vec![
            "owner/c".to_string(),
            "owner/z".to_string(),
            "owner/a".to_string(),
        ];
        let mut inp = inputs(&ws, &sub, &col, &att, &asking, &projects);
        inp.pinned_repos = &pins;
        let out = compute_visible(inp);
        let headers: Vec<&str> = out
            .visible
            .iter()
            .filter_map(|r| match r {
                VisibleRow::RepoHeader(name) => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(headers, ["owner/c", "owner/a", "owner/b"]);
    }

    /// A starred workspace is lifted into the `★ Focused` section at the
    /// very top — a `FocusedHeader` first, the starred row under it — and
    /// is NOT re-listed under its repo group.
    #[test]
    fn focused_workspace_lifts_into_top_section() {
        let mut ws = HashMap::new();
        for (k, repo) in [("ka", "owner/a"), ("kb", "owner/b")] {
            let w = workspace_with_task(k, Some(repo), 10);
            ws.insert(SessionKey::from(&w.key), w);
        }
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();
        let focus = vec![SessionKey::from("kb")];
        let mut inp = inputs(&ws, &sub, &col, &att, &asking, &projects);
        inp.focused_workspaces = &focus;
        let out = compute_visible(inp);

        // First two rows: the synthetic header then the starred row.
        assert!(matches!(out.visible[0], VisibleRow::FocusedHeader));
        assert!(matches!(&out.visible[1], VisibleRow::Workspace(k) if k.as_str() == "kb"));

        // `kb` appears exactly once (lifted, not duplicated); its repo
        // header still renders but with no workspace under it.
        let kb_rows = out
            .visible
            .iter()
            .filter(|r| matches!(r, VisibleRow::Workspace(k) if k.as_str() == "kb"))
            .count();
        assert_eq!(kb_rows, 1);
        // The non-focused `ka` stays in its repo group.
        let ka_after_a_header = out.visible.windows(2).any(|w| {
            matches!(&w[0], VisibleRow::RepoHeader(n) if n == "owner/a")
                && matches!(&w[1], VisibleRow::Workspace(k) if k.as_str() == "ka")
        });
        assert!(ka_after_a_header, "ka stays under owner/a");
    }

    /// The Focused section renders its rows in focus (Vec) order, and a
    /// star naming a workspace not visible this pass is skipped — the
    /// per-workspace parallel of the pin's skip-absent behavior.
    #[test]
    fn focused_section_keeps_focus_order_and_skips_absent() {
        let mut ws = HashMap::new();
        for (k, repo) in [("ka", "owner/a"), ("kb", "owner/b"), ("kc", "owner/c")] {
            let w = workspace_with_task(k, Some(repo), 10);
            ws.insert(SessionKey::from(&w.key), w);
        }
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();
        // Focus kc then ka; "kz" has no workspace and must be skipped.
        let focus = vec![
            SessionKey::from("kc"),
            SessionKey::from("kz"),
            SessionKey::from("ka"),
        ];
        let mut inp = inputs(&ws, &sub, &col, &att, &asking, &projects);
        inp.focused_workspaces = &focus;
        let out = compute_visible(inp);

        // Rows under the FocusedHeader, up to the first RepoHeader.
        let focused: Vec<&str> = out
            .visible
            .iter()
            .skip_while(|r| !matches!(r, VisibleRow::FocusedHeader))
            .skip(1)
            .take_while(|r| matches!(r, VisibleRow::Workspace(_)))
            .filter_map(|r| match r {
                VisibleRow::Workspace(k) => Some(k.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(focused, ["kc", "ka"]);
    }

    /// A star naming a workspace filtered out by the current mailbox is
    /// not surfaced — the Focused section respects the same filtered set
    /// the repo groups do (parity with the pin, which only reorders what
    /// the view already shows). With no visible starred rows, no
    /// `FocusedHeader` is emitted.
    #[test]
    fn focused_section_respects_mailbox_filter() {
        let mut ws = HashMap::new();
        let w = workspace_with_task("ka", Some("owner/a"), 10);
        ws.insert(SessionKey::from(&w.key), w);
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();
        let focus = vec![SessionKey::from("ka")];
        // Snoozed mailbox: the (non-snoozed) workspace isn't a member, so
        // even though it's starred it doesn't appear anywhere.
        let mut inp = inputs(&ws, &sub, &col, &att, &asking, &projects);
        inp.mailbox = Mailbox::Snoozed;
        inp.focused_workspaces = &focus;
        let out = compute_visible(inp);
        assert!(
            !out.visible
                .iter()
                .any(|r| matches!(r, VisibleRow::FocusedHeader)),
            "no focused header when no starred row is visible"
        );
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
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
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
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
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

    #[test]
    fn github_workspace_repairs_lossy_project_header_from_task_repo() {
        let mut w = workspace_with_task("k1", Some("codefly-dev/warden-platform"), 10);
        let pk = ProjectKey::github("codefly-dev", "warden-platform");
        w.project_key = Some(pk.clone());
        let mut ws = HashMap::new();
        ws.insert(SessionKey::from(&w.key), w);
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let mut projects = BTreeMap::new();
        projects.insert(
            pk.clone(),
            Project::new(pk, "codefly/dev-warden-platform", chrono::Utc::now()),
        );

        let out = compute_visible(inputs(&ws, &sub, &col, &att, &asking, &projects));

        assert_eq!(out.visible.len(), 2);
        assert!(matches!(
            &out.visible[0],
            VisibleRow::RepoHeader(name) if name == "codefly-dev/warden-platform"
        ));
        assert!(!out.summaries.contains_key("codefly/dev-warden-platform"));
    }

    /// Project with no workspace yields a header in Inbox
    /// (so the user can see "I'm subscribed but nothing's in flight").
    #[test]
    fn empty_project_emits_header_in_inbox() {
        let ws = HashMap::new();
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
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

    /// With a filter active, an empty subscribed project does NOT emit
    /// a header — a narrowed view shouldn't list every repo as an
    /// empty header burying the matches (issue #443 review).
    #[test]
    fn active_filter_suppresses_empty_project_headers() {
        let ws = HashMap::new();
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let mut projects = BTreeMap::new();
        let pk = ProjectKey::github("owner", "empty");
        projects.insert(
            pk.clone(),
            Project::new(pk, "owner/empty", chrono::Utc::now()),
        );
        let mut filters = FilterSet::default();
        filters.toggle(Filter::Author);
        let mut i = inputs(&ws, &sub, &col, &att, &asking, &projects);
        i.filters = &filters;
        let out = compute_visible(i);
        assert!(
            out.visible.is_empty(),
            "empty project header suppressed under an active filter"
        );
    }

    /// A project whose stored `name` is the raw key (the legacy
    /// self-add fallback) still renders as `owner/repo`, and its
    /// workspaces group under that same prettified header.
    #[test]
    fn raw_key_project_renders_as_owner_slash_repo() {
        let pk = ProjectKey::github("AntoineToussaint", "lazybox");
        let mut w = workspace_with_task("k1", None, 10);
        w.project_key = Some(pk.clone());
        let mut ws = HashMap::new();
        ws.insert(SessionKey::from(&w.key), w);
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let mut projects = BTreeMap::new();
        // Stored with `name == key` — the bug this fix repairs.
        projects.insert(
            pk.clone(),
            Project::new(pk.clone(), pk.as_str(), chrono::Utc::now()),
        );
        let out = compute_visible(inputs(&ws, &sub, &col, &att, &asking, &projects));
        assert!(out.summaries.contains_key("AntoineToussaint/lazybox"));
        assert!(
            !out.summaries
                .contains_key("github-AntoineToussaint-lazybox")
        );
    }

    /// Same setup, but Inactive mailbox: the empty project header
    /// is NOT shown (alternate view, not a subscription).
    #[test]
    fn empty_project_skipped_in_inactive() {
        let ws = HashMap::new();
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
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
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
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
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
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
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();
        let out = compute_visible(inputs(&ws, &sub, &col, &att, &asking, &projects));
        assert_eq!(out.summaries.get("owner/r").unwrap().active, 3);
    }

    /// Build a workspace in `repo` with the given issue/PR `num` and
    /// display title — enough to exercise search matching.
    fn titled(key: &str, repo: &str, num: u64, title: &str) -> Workspace {
        let mut w = workspace_with_task(key, Some(repo), 10);
        w.key = WorkspaceKey(key.into());
        // `workspace_with_task` seeds a GitHub issue (no PR slot), so
        // the primary task lives in `gh_issues`.
        if let Some(t) = w.gh_issues.get_mut(0) {
            t.id.key = format!("{repo}#{num}");
            t.title = title.into();
        }
        w.name = title.into();
        w
    }

    fn search(scope: &str, query: &str) -> SearchState {
        SearchState {
            scope: Some(scope.into()),
            query: query.into(),
            editing: true,
        }
    }

    fn global_search(query: &str) -> SearchState {
        SearchState {
            scope: None,
            query: query.into(),
            editing: true,
        }
    }

    /// Query keeps fuzzy title matches in the scoped project, hides
    /// the rest.
    #[test]
    fn search_filters_scoped_project_by_title() {
        let mut ws = HashMap::new();
        for w in [
            titled("k1", "owner/r", 1, "Add search bar"),
            titled("k2", "owner/r", 2, "Fix flaky test"),
        ] {
            ws.insert(SessionKey::from(&w.key), w);
        }
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();
        let s = search("owner/r", "search");
        let mut i = inputs(&ws, &sub, &col, &att, &asking, &projects);
        i.search = Some(&s);
        let out = compute_visible(i);
        let keys: Vec<&str> = out
            .visible
            .iter()
            .filter_map(|r| match r {
                VisibleRow::Workspace(k) => Some(k.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(keys, vec!["k1"]);
        assert_eq!(out.summaries.get("owner/r").unwrap().active, 1);
    }

    /// A bare number matches the issue/PR number (with or without `#`).
    #[test]
    fn search_matches_on_number() {
        let mut ws = HashMap::new();
        for w in [
            titled("k1", "owner/r", 100, "Alpha"),
            titled("k2", "owner/r", 7, "Beta"),
        ] {
            ws.insert(SessionKey::from(&w.key), w);
        }
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();
        let s = search("owner/r", "#100");
        let mut i = inputs(&ws, &sub, &col, &att, &asking, &projects);
        i.search = Some(&s);
        let out = compute_visible(i);
        let keys: Vec<&str> = out
            .visible
            .iter()
            .filter_map(|r| match r {
                VisibleRow::Workspace(k) => Some(k.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(keys, vec!["k1"]);
    }

    /// The search only filters its scoped project — workspaces in
    /// other projects stay fully visible.
    #[test]
    fn search_leaves_other_projects_untouched() {
        let mut ws = HashMap::new();
        for w in [
            titled("k1", "owner/a", 1, "Add search"),
            titled("k2", "owner/a", 2, "Unrelated"),
            titled("k3", "owner/b", 3, "Unrelated"),
        ] {
            ws.insert(SessionKey::from(&w.key), w);
        }
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();
        let s = search("owner/a", "search");
        let mut i = inputs(&ws, &sub, &col, &att, &asking, &projects);
        i.search = Some(&s);
        let out = compute_visible(i);
        // owner/a is filtered to k1; owner/b keeps its row regardless.
        assert_eq!(out.summaries.get("owner/a").unwrap().active, 1);
        assert_eq!(out.summaries.get("owner/b").unwrap().active, 1);
    }

    /// A global search (`scope: None`) filters EVERY project at once —
    /// a matching row survives in each repo, non-matching rows drop.
    #[test]
    fn global_search_filters_all_projects() {
        let mut ws = HashMap::new();
        for w in [
            titled("k1", "owner/a", 1, "Add search"),
            titled("k2", "owner/a", 2, "Unrelated"),
            titled("k3", "owner/b", 3, "Search elsewhere"),
            titled("k4", "owner/b", 4, "Unrelated"),
        ] {
            ws.insert(SessionKey::from(&w.key), w);
        }
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();
        let s = global_search("search");
        let mut i = inputs(&ws, &sub, &col, &att, &asking, &projects);
        i.search = Some(&s);
        let out = compute_visible(i);
        let keys: Vec<&str> = out
            .visible
            .iter()
            .filter_map(|r| match r {
                VisibleRow::Workspace(k) => Some(k.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(keys, vec!["k1", "k3"]);
        assert_eq!(out.summaries.get("owner/a").unwrap().active, 1);
        assert_eq!(out.summaries.get("owner/b").unwrap().active, 1);
    }

    /// The killer case: a bare PR/issue number typed into the global
    /// box surfaces the match no matter which repo it lives in.
    #[test]
    fn global_search_jumps_to_number_across_repos() {
        let mut ws = HashMap::new();
        for w in [
            titled("k1", "owner/a", 100, "Alpha"),
            titled("k2", "owner/a", 7, "Beta"),
            titled("k3", "owner/b", 720, "Gamma"),
        ] {
            ws.insert(SessionKey::from(&w.key), w);
        }
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();
        let s = global_search("#720");
        let mut i = inputs(&ws, &sub, &col, &att, &asking, &projects);
        i.search = Some(&s);
        let out = compute_visible(i);
        let keys: Vec<&str> = out
            .visible
            .iter()
            .filter_map(|r| match r {
                VisibleRow::Workspace(k) => Some(k.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(keys, vec!["k3"]);
    }

    /// A global search suppresses empty subscribed-project headers (the
    /// desktop's global `/`), while a scoped search keeps them (the
    /// TUI's `/`).
    #[test]
    fn global_search_suppresses_empty_project_headers_scoped_keeps_them() {
        let ws = HashMap::new();
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let mut projects = BTreeMap::new();
        let pk = ProjectKey::github("owner", "empty");
        projects.insert(
            pk.clone(),
            Project::new(pk, "owner/empty", chrono::Utc::now()),
        );

        // Global (scope None): the empty repo header is gone.
        let global = global_search("anything");
        let mut i = inputs(&ws, &sub, &col, &att, &asking, &projects);
        i.search = Some(&global);
        assert!(compute_visible(i).visible.is_empty());

        // Scoped (named repo): unrelated empty projects still get a header.
        let scoped = search("some/other", "anything");
        let mut i = inputs(&ws, &sub, &col, &att, &asking, &projects);
        i.search = Some(&scoped);
        assert!(compute_visible(i).summaries.contains_key("owner/empty"));
    }

    /// An empty query is a no-op even when search state is present.
    #[test]
    fn empty_query_shows_full_tree() {
        let mut ws = HashMap::new();
        for w in [
            titled("k1", "owner/r", 1, "Alpha"),
            titled("k2", "owner/r", 2, "Beta"),
        ] {
            ws.insert(SessionKey::from(&w.key), w);
        }
        let sub = BTreeSet::new();
        let col = BTreeSet::new();
        let att = lazybox_config::AttentionConfig::default();
        let asking = HashMap::new();
        let projects = BTreeMap::new();
        let s = search("owner/r", "");
        let mut i = inputs(&ws, &sub, &col, &att, &asking, &projects);
        i.search = Some(&s);
        let out = compute_visible(i);
        assert_eq!(out.summaries.get("owner/r").unwrap().active, 2);
    }

    #[test]
    fn search_matches_is_case_insensitive_subsequence() {
        let w = titled("k1", "owner/r", 1, "Add Search Filter Bar");
        assert!(search_matches("sfb", &w)); // subsequence across words
        assert!(search_matches("FILTER", &w)); // case-insensitive
        assert!(!search_matches("zzz", &w));
    }

    #[test]
    fn search_matches_repo_labels_and_people() {
        let mut w = titled("k1", "octo/widget", 1, "Unrelated title");
        if let Some(t) = w.gh_issues.get_mut(0) {
            t.labels = vec![lazybox_core::Label {
                name: "bug".into(),
                color: String::new(),
            }];
            t.reviewers = vec!["alice".into()];
            t.assignees = vec!["bob".into()];
        }
        assert!(search_matches("widget", &w), "repo substring");
        assert!(search_matches("bug", &w), "label substring");
        assert!(search_matches("alice", &w), "reviewer substring");
        assert!(search_matches("bob", &w), "assignee substring");
        assert!(!search_matches("nobody", &w));
    }

    #[test]
    fn pr_number_extracts_trailing_int() {
        let w = titled("k1", "owner/r", 1234, "x");
        assert_eq!(pr_number(w.primary_task().unwrap()), Some(1234));
    }

    #[test]
    fn pr_number_returns_none_when_no_hash() {
        let mut w = workspace_with_task("k1", Some("owner/r"), 10);
        if let Some(t) = w.gh_issues.get_mut(0) {
            t.id.key = "plain-key".into();
        }
        assert_eq!(pr_number(w.primary_task().unwrap()), None);
    }
}

/// The desktop client (#732) consumes these view-model types as
/// generated TypeScript. Pin that each one derives `ts_rs::TS` and
/// produces a non-empty declaration, so a serde/shape change that
/// breaks the contract fails here rather than in the desktop build.
#[cfg(all(test, feature = "desktop-contract"))]
mod contract_tests {
    use super::*;
    use ts_rs::{Config, TS};

    #[test]
    fn view_model_types_have_typescript_declarations() {
        let cfg = Config::default();
        assert!(ComputeOutcome::decl(&cfg).contains("ComputeOutcome"));
        assert!(VisibleRow::decl(&cfg).contains("VisibleRow"));
        assert!(WorkspaceKind::decl(&cfg).contains("WorkspaceKind"));
        assert!(SortMode::decl(&cfg).contains("SortMode"));
        assert!(Filter::decl(&cfg).contains("Filter"));
        assert!(FilterAxis::decl(&cfg).contains("FilterAxis"));
        assert!(FilterMenuItem::decl(&cfg).contains("FilterMenuItem"));
        assert!(RepoSummary::decl(&cfg).contains("RepoSummary"));
    }
}
