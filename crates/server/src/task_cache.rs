//! Serving the daemon's tracker-record cache to the sessions it spawns (#1799).
//!
//! The daemon fetches every issue and PR it shows — title, body, labels,
//! state, parent, comments — and re-reads them on each sweep. The agent it
//! then spawns on one of those records used to receive none of it, so its
//! first act was `gh issue view N` for text the daemon had just paid for,
//! and each subagent paid again. Five parallel sessions emptied the shared
//! 5,000/hour token budget in seven minutes, and the starved poller left the
//! inbox showing twenty issues open that had been closed for forty minutes.
//!
//! This module is the read side of the fix: it projects the cache into
//! [`TaskRecord`]s, writes this workspace's records into the worktree at
//! spawn as `.lazybox/task.json`, and backs the `task` / `get_issue` /
//! `list_issues` / `get_pr` MCP tools. Nothing here talks to a provider — a
//! miss is reported as a miss, never filled with a fetch, so serving an
//! agent can never cost the budget it exists to protect.

use std::collections::HashMap;
use std::path::Path;

use chrono::{DateTime, Utc};
use lazybox_core::{
    TASK_FILE_RELATIVE_PATH, Task, TaskRecord, WORKSPACE_RECORD_FILE_SCHEMA, Workspace,
    WorkspaceKey, WorkspaceRecordFile,
};

use crate::ServerConfig;

/// When each workspace's tasks last came off a provider, as
/// [`crate::registries::PollState`] recorded it. A workspace absent from the
/// map has not been polled since this daemon started, which reads as an
/// unknown cache age rather than a fabricated recent one.
pub(crate) type FetchTimes = HashMap<WorkspaceKey, DateTime<Utc>>;

/// Every workspace the daemon has cached, decoded. Rows whose JSON no
/// longer parses are skipped: a serving read must never be the thing that
/// surfaces a corrupt row, and the poller's own loader already reports them.
pub(crate) fn cached_workspaces(store: &dyn lazybox_store::Store) -> Vec<Workspace> {
    store
        .list_workspaces()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|record| record.workspace_json)
        .filter_map(|json| serde_json::from_str::<Workspace>(&json).ok())
        .collect()
}

/// Every task in `workspaces`, in no particular order.
pub(crate) fn tasks_of(workspaces: &[Workspace]) -> Vec<&Task> {
    workspaces
        .iter()
        .flat_map(|ws| {
            ws.pr
                .iter()
                .chain(ws.gh_issues.iter())
                .chain(ws.linear_issues.iter())
        })
        .collect()
}

/// Whether `task` is the record `repo`/`number` names. Matching on the id's
/// `owner/repo#N` key rather than [`Task::repo`] keeps a task that lost its
/// repo field addressable, and keeps `#7` in one repo from answering for
/// `#7` in another.
fn addresses(task: &Task, repo: &str, number: u64) -> bool {
    task.id.key.rsplit_once('#').is_some_and(|(owner_repo, n)| {
        owner_repo.eq_ignore_ascii_case(repo) && n.parse::<u64>() == Ok(number)
    })
}

/// The cached record for `repo#number`, with its sub-issues filled in.
/// `want_pr` selects which of an issue/PR pair sharing a number is meant —
/// GitHub numbers them in one sequence, so a repo has at most one of each.
pub(crate) fn find_record(
    workspaces: &[Workspace],
    fetched: &FetchTimes,
    repo: &str,
    number: u64,
    want_pr: bool,
) -> Option<TaskRecord> {
    let all = tasks_of(workspaces);
    let (task, fetched_at) = workspaces.iter().find_map(|ws| {
        let task = ws
            .pr
            .iter()
            .chain(ws.gh_issues.iter())
            .chain(ws.linear_issues.iter())
            .find(|task| task.is_pr() == want_pr && addresses(task, repo, number))?;
        Some((task, fetched.get(&ws.key).copied()))
    })?;
    Some(TaskRecord::of(task, fetched_at).with_sub_issues(all))
}

/// Whether `state` is what `wanted` names. Separators and case are ignored,
/// so an agent writing `in-progress`, `in_progress` or `InProgress` all reach
/// [`TaskState::InProgress`] — a filter that silently matched nothing would
/// read as "this repo has no such issues".
fn state_matches(state: lazybox_core::TaskState, wanted: &str) -> bool {
    let canonical = match state {
        lazybox_core::TaskState::Open => "open",
        lazybox_core::TaskState::InProgress => "inprogress",
        lazybox_core::TaskState::InReview => "inreview",
        lazybox_core::TaskState::Closed => "closed",
        lazybox_core::TaskState::Merged => "merged",
        lazybox_core::TaskState::Draft => "draft",
    };
    let wanted: String = wanted
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    wanted == canonical
}

/// Cached issue records in `repo`, newest-updated first. `state` filters on
/// the canonical [`lazybox_core::TaskState`] name, separator- and
/// case-insensitively; `None` returns every state.
pub(crate) fn list_issue_records(
    workspaces: &[Workspace],
    fetched: &FetchTimes,
    repo: &str,
    state: Option<&str>,
    limit: usize,
) -> Vec<TaskRecord> {
    let all = tasks_of(workspaces);
    let mut records: Vec<TaskRecord> = workspaces
        .iter()
        .flat_map(|ws| {
            ws.gh_issues
                .iter()
                .chain(ws.linear_issues.iter())
                .chain(ws.pr.iter())
                .filter(|task| !task.is_pr())
                .map(move |task| (task, fetched.get(&ws.key).copied()))
        })
        .filter(|(task, _)| {
            task.repo
                .as_deref()
                .is_some_and(|r| r.eq_ignore_ascii_case(repo))
                || task
                    .id
                    .key
                    .rsplit_once('#')
                    .is_some_and(|(owner_repo, _)| owner_repo.eq_ignore_ascii_case(repo))
        })
        .filter(|(task, _)| state.is_none_or(|wanted| state_matches(task.state, wanted)))
        .map(|(task, fetched_at)| TaskRecord::of(task, fetched_at))
        .collect();
    records.sort_by_key(|record| std::cmp::Reverse(record.updated_at));
    records.truncate(limit);
    for record in &mut records {
        record.sub_issues = lazybox_core::sub_issue_ids(&record.id, all.iter().copied());
    }
    records
}

/// This workspace's own records — the PR when there is one, else the first
/// linked issue, plus everything else linked to the row.
pub(crate) fn workspace_record_file(
    workspaces: &[Workspace],
    fetched: &FetchTimes,
    key: &WorkspaceKey,
) -> Option<WorkspaceRecordFile> {
    let ws = workspaces.iter().find(|ws| ws.key == *key)?;
    Some(record_file_for(
        ws,
        fetched.get(key).copied(),
        &tasks_of(workspaces),
    ))
}

/// Project one workspace into its record file. `all` is the full task set
/// used to resolve sub-issues; pass an empty slice to skip that resolution.
pub(crate) fn record_file_for(
    ws: &Workspace,
    fetched_at: Option<DateTime<Utc>>,
    all: &[&Task],
) -> WorkspaceRecordFile {
    let mut linked: Vec<&Task> = ws
        .pr
        .iter()
        .chain(ws.gh_issues.iter())
        .chain(ws.linear_issues.iter())
        .collect();
    // `primary_task` is the row's subject; the rest stay as `also_linked` in
    // the order the workspace holds them. Matched by identity rather than by
    // `TaskId`, so a workspace that somehow holds the same id twice still
    // splits at the task `primary_task` actually chose.
    let primary = ws
        .primary_task()
        .and_then(|primary| {
            linked
                .iter()
                .position(|task| std::ptr::eq::<Task>(*task, primary))
        })
        .map(|index| linked.remove(index));
    let record =
        |task: &Task| TaskRecord::of(task, fetched_at).with_sub_issues(all.iter().copied());
    WorkspaceRecordFile {
        schema: WORKSPACE_RECORD_FILE_SCHEMA,
        workspace: ws.key.as_str().to_string(),
        repo: ws
            .primary_task()
            .and_then(|task| task.repo.clone())
            .or_else(|| linked.first().and_then(|task| task.repo.clone())),
        branch: ws.branch.clone(),
        primary: primary.map(record),
        also_linked: linked.into_iter().map(record).collect(),
        written_at: Utc::now(),
    }
}

/// Write `file` into `worktree` as `.lazybox/task.json`, alongside a
/// `.gitignore` that hides the whole directory.
///
/// The ignore file is written every time rather than only on create: the
/// directory is lazybox's, and a record file that shows up in `git status`
/// would end up committed by the very agent it was written for. Errors are
/// returned rather than logged here so the spawn path can decide — a
/// read-only or vanished worktree must not fail a spawn.
pub(crate) fn write_record_file(
    worktree: &Path,
    file: &WorkspaceRecordFile,
) -> std::io::Result<()> {
    let path = worktree.join(TASK_FILE_RELATIVE_PATH);
    let dir = path
        .parent()
        .expect("TASK_FILE_RELATIVE_PATH always has a parent directory");
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join(".gitignore"), "*\n")?;
    let json = serde_json::to_string_pretty(file)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    std::fs::write(&path, json)
}

/// Write this workspace's record file into `worktree`, best-effort. Called
/// on the spawn path, where a failure must never block the session: an
/// agent without the file falls back to `gh`, which is exactly the
/// pre-#1799 behavior.
pub(crate) async fn write_record_file_for_spawn(
    config: &ServerConfig,
    key: &WorkspaceKey,
    worktree: &Path,
) {
    let key = key.clone();
    let fetched = config.poll.tasks_fetched_snapshot();
    let file = crate::store_blocking(&config.store, move |store| {
        let workspaces = cached_workspaces(store);
        workspace_record_file(&workspaces, &fetched, &key)
    })
    .await;
    let Some(file) = file else {
        return;
    };
    if let Err(error) = write_record_file(worktree, &file) {
        tracing::warn!(
            worktree = %worktree.display(),
            %error,
            "task record: could not write {TASK_FILE_RELATIVE_PATH} — the session falls back to `gh`"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lazybox_core::{
        CiStatus, Label, Mergeable, ReviewStatus, TaskId, TaskKind, TaskRole, TaskState,
    };

    fn task(key: &str, title: &str, kind: TaskKind) -> Task {
        Task {
            id: TaskId {
                source: "github".into(),
                key: key.into(),
            },
            title: title.into(),
            body: Some(format!("body of {key}")),
            state: TaskState::Open,
            role: TaskRole::Author,
            ci: CiStatus::None,
            review: ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: format!("https://github.com/{}", key.replace('#', "/issues/")),
            repo: key.rsplit_once('#').map(|(repo, _)| repo.to_string()),
            branch: None,
            base_branch: None,
            updated_at: Utc::now(),
            created_at: None,
            closed_at: None,
            labels: vec![Label::new("bug")],
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
            kind: Some(kind),
            priority: None,
            state_label: None,
        }
    }

    fn issue_workspace(key: &str, task_key: &str) -> Workspace {
        let mut ws = Workspace::empty(WorkspaceKey::new(key), "main", Utc::now());
        ws.gh_issues = vec![task(task_key, task_key, TaskKind::Issue)];
        ws
    }

    /// Every workspace polled just now.
    fn all_fresh(workspaces: &[Workspace]) -> FetchTimes {
        workspaces
            .iter()
            .map(|ws| (ws.key.clone(), Utc::now()))
            .collect()
    }

    #[test]
    fn find_record_matches_on_the_repo_and_number_together() {
        let workspaces = vec![
            issue_workspace("a", "acme/widget#7"),
            issue_workspace("b", "acme/other#7"),
        ];
        let fetched = all_fresh(&workspaces);
        let found = find_record(&workspaces, &fetched, "acme/other", 7, false).expect("record");
        assert_eq!(found.id, "github:acme/other#7");
        // The same number in a different repo must never answer for this one —
        // an agent handed the wrong issue body acts on the wrong task.
        assert_eq!(
            find_record(&workspaces, &fetched, "acme/widget", 7, false)
                .expect("record")
                .id,
            "github:acme/widget#7"
        );
        assert!(find_record(&workspaces, &fetched, "acme/absent", 7, false).is_none());
    }

    #[test]
    fn find_record_separates_a_pr_from_an_issue_sharing_a_number() {
        // GitHub numbers issues and PRs in one sequence per repo, so `#7` can
        // be either. `get_issue` must not hand back a PR, or an agent reads a
        // diff summary as an issue body.
        let mut ws = Workspace::empty(WorkspaceKey::new("a"), "main", Utc::now());
        ws.pr = Some(task("acme/widget#7", "the PR", TaskKind::Pr));
        let mut other = issue_workspace("b", "acme/widget#7");
        other.gh_issues[0].title = "the issue".into();
        let workspaces = vec![ws, other];
        let fetched = all_fresh(&workspaces);

        assert_eq!(
            find_record(&workspaces, &fetched, "acme/widget", 7, true)
                .expect("pr")
                .title,
            "the PR"
        );
        assert_eq!(
            find_record(&workspaces, &fetched, "acme/widget", 7, false)
                .expect("issue")
                .title,
            "the issue"
        );
    }

    #[test]
    fn find_record_carries_the_workspaces_own_cache_age() {
        let ws = issue_workspace("a", "acme/widget#7");
        let stamped = "2026-09-17T12:00:00Z".parse().expect("ts");
        let fetched: FetchTimes = [(ws.key.clone(), stamped)].into_iter().collect();
        let found = find_record(&[ws], &fetched, "acme/widget", 7, false).expect("record");
        assert_eq!(found.fetched_at, Some(stamped));
    }

    #[test]
    fn an_unpolled_workspace_reports_an_unknown_cache_age() {
        // A daemon restarted since the last sweep has no fetch time for the
        // row. `None` is the honest answer: inventing "now" would tell an
        // agent a copy of unknown age is seconds old.
        let ws = issue_workspace("a", "acme/widget#7");
        let found =
            find_record(&[ws], &FetchTimes::new(), "acme/widget", 7, false).expect("record");
        assert_eq!(found.fetched_at, None);
    }

    #[test]
    fn list_issue_records_filters_by_repo_and_state_and_sorts_newest_first() {
        let mut old = issue_workspace("a", "acme/widget#1");
        old.gh_issues[0].updated_at = "2026-09-01T00:00:00Z".parse().expect("ts");
        let mut recent = issue_workspace("b", "acme/widget#2");
        recent.gh_issues[0].updated_at = "2026-09-16T00:00:00Z".parse().expect("ts");
        let mut closed = issue_workspace("c", "acme/widget#3");
        closed.gh_issues[0].state = TaskState::Closed;
        let elsewhere = issue_workspace("d", "other/repo#4");
        let workspaces = vec![old, recent, closed, elsewhere];
        let fetched = all_fresh(&workspaces);

        let all = list_issue_records(&workspaces, &fetched, "acme/widget", None, 50);
        assert_eq!(all.len(), 3, "the other repo must not leak in");

        let open = list_issue_records(&workspaces, &fetched, "acme/widget", Some("open"), 50);
        let ids: Vec<&str> = open.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["github:acme/widget#2", "github:acme/widget#1"]);
    }

    #[test]
    fn the_state_filter_ignores_separators_and_case() {
        let mut ws = issue_workspace("a", "acme/widget#1");
        ws.gh_issues[0].state = TaskState::InProgress;
        let workspaces = [ws];
        for spelling in ["in-progress", "in_progress", "InProgress", "IN PROGRESS"] {
            assert_eq!(
                list_issue_records(
                    &workspaces,
                    &FetchTimes::new(),
                    "acme/widget",
                    Some(spelling),
                    50
                )
                .len(),
                1,
                "`{spelling}` must reach InProgress — a filter that silently \
                 matches nothing reads as an empty repo"
            );
        }
        assert!(
            list_issue_records(
                &workspaces,
                &FetchTimes::new(),
                "acme/widget",
                Some("open"),
                50
            )
            .is_empty()
        );
    }

    #[test]
    fn list_issue_records_never_returns_a_pr() {
        // `list_issues` is what replaces a `gh issue list` fan-out; a PR in
        // that result would send an agent editing the wrong record.
        let mut ws = Workspace::empty(WorkspaceKey::new("a"), "main", Utc::now());
        ws.pr = Some(task("acme/widget#7", "a PR", TaskKind::Pr));
        ws.gh_issues = vec![task("acme/widget#8", "an issue", TaskKind::Issue)];
        let records = list_issue_records(&[ws], &FetchTimes::new(), "acme/widget", None, 50);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, "github:acme/widget#8");
    }

    #[test]
    fn list_issue_records_fills_sub_issues_after_truncation() {
        let mut parent = issue_workspace("a", "acme/widget#1");
        let mut child = issue_workspace("b", "acme/widget#2");
        child.gh_issues[0].parent = Some(parent.gh_issues[0].id.clone());
        parent.gh_issues[0].updated_at = "2026-09-16T00:00:00Z".parse().expect("ts");
        child.gh_issues[0].updated_at = "2026-09-01T00:00:00Z".parse().expect("ts");

        // Limit 1 keeps only the parent — the child is still the parent's
        // sub-issue, so resolving children before the truncate would be wrong
        // in the other direction too.
        let records =
            list_issue_records(&[parent, child], &FetchTimes::new(), "acme/widget", None, 1);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].sub_issues, vec!["github:acme/widget#2"]);
    }

    #[test]
    fn record_file_puts_the_pr_first_and_keeps_the_issues_it_closes() {
        let mut ws = Workspace::empty(WorkspaceKey::new("a"), "feat/x", Utc::now());
        ws.pr = Some(task("acme/widget#9", "the PR", TaskKind::Pr));
        ws.gh_issues = vec![task("acme/widget#7", "the issue", TaskKind::Issue)];

        let file = record_file_for(&ws, Some(Utc::now()), &[]);
        assert_eq!(file.schema, lazybox_core::WORKSPACE_RECORD_FILE_SCHEMA);
        assert_eq!(file.primary.as_ref().expect("primary").title, "the PR");
        assert_eq!(file.repo.as_deref(), Some("acme/widget"));
        assert_eq!(file.branch, "feat/x");
        // The closed issue carries the brief; dropping it would send the agent
        // back to `gh` for the very text this file exists to deliver.
        let also: Vec<&str> = file.also_linked.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(also, vec!["github:acme/widget#7"]);
    }

    #[test]
    fn record_file_for_a_repo_less_workspace_has_no_primary() {
        let ws = Workspace::empty(WorkspaceKey::new("scratch"), "main", Utc::now());
        let file = record_file_for(&ws, None, &[]);
        assert!(file.primary.is_none());
        assert!(file.also_linked.is_empty());
    }

    #[test]
    fn writing_the_record_file_also_hides_it_from_git() {
        // A record file the agent can see in `git status` is a record file the
        // agent will commit.
        let dir = tempfile::tempdir().expect("tempdir");
        let ws = issue_workspace("a", "acme/widget#7");
        let file = record_file_for(&ws, Some(Utc::now()), &[]);
        write_record_file(dir.path(), &file).expect("write");

        let written = std::fs::read_to_string(dir.path().join(TASK_FILE_RELATIVE_PATH))
            .expect("record file readable");
        let parsed: WorkspaceRecordFile = serde_json::from_str(&written).expect("valid json");
        assert_eq!(
            parsed.primary.expect("primary").body,
            "body of acme/widget#7"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".lazybox/.gitignore")).expect("gitignore"),
            "*\n"
        );
    }

    #[tokio::test]
    async fn spawn_writes_the_record_the_daemon_already_holds() {
        // End-to-end over the store: what the spawn path actually puts in the
        // worktree, from a workspace the poller persisted.
        let config = ServerConfig::in_memory();
        let ws = issue_workspace("github-acme-widget-7", "acme/widget#7");
        config
            .store
            .save_workspace(&lazybox_store::WorkspaceRecord {
                key: ws.key.as_str().to_string(),
                created_at: Utc::now(),
                workspace_json: Some(serde_json::to_string(&ws).expect("serialize")),
            })
            .expect("save workspace");
        config.poll.note_tasks_fetched(&ws.key);

        let dir = tempfile::tempdir().expect("tempdir");
        write_record_file_for_spawn(&config, &ws.key, dir.path()).await;

        let written = std::fs::read_to_string(dir.path().join(TASK_FILE_RELATIVE_PATH))
            .expect("record file written at spawn");
        let parsed: WorkspaceRecordFile = serde_json::from_str(&written).expect("valid json");
        let primary = parsed.primary.expect("primary");
        assert_eq!(primary.body, "body of acme/widget#7");
        assert!(
            primary.fetched_at.is_some(),
            "the record must carry the cache age the poller recorded"
        );
    }

    #[tokio::test]
    async fn spawn_into_an_unknown_workspace_writes_nothing() {
        // A sandbox or scratch spawn has no cached row. Writing an empty
        // record would be worse than writing none: an agent would read
        // "no issue" as fact.
        let config = ServerConfig::in_memory();
        let dir = tempfile::tempdir().expect("tempdir");
        write_record_file_for_spawn(&config, &WorkspaceKey::new("absent"), dir.path()).await;
        assert!(!dir.path().join(TASK_FILE_RELATIVE_PATH).exists());
    }

    #[test]
    fn writing_the_record_file_replaces_a_previous_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut ws = issue_workspace("a", "acme/widget#7");
        write_record_file(dir.path(), &record_file_for(&ws, None, &[])).expect("write");
        ws.gh_issues[0].title = "renamed upstream".into();
        write_record_file(dir.path(), &record_file_for(&ws, None, &[])).expect("rewrite");

        let written = std::fs::read_to_string(dir.path().join(TASK_FILE_RELATIVE_PATH))
            .expect("record file readable");
        let parsed: WorkspaceRecordFile = serde_json::from_str(&written).expect("valid json");
        assert_eq!(parsed.primary.expect("primary").title, "renamed upstream");
    }
}
