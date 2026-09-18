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
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use lazybox_core::{
    ARTIFACT_SPOOL_RELATIVE_PATH, Activity, RECORD_CONTENT_WARNING, TASK_FILE_RELATIVE_PATH, Task,
    TaskRecord, WORKSPACE_RECORD_FILE_SCHEMA, Workspace, WorkspaceKey, WorkspaceRecordFile,
};
use lazybox_store::StoreError;

use crate::ServerConfig;

/// Per-call half of [`write_atomically`]'s temp-file name — see its docs.
static TMP_NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// When each workspace's tasks last came off a provider, as
/// [`crate::registries::PollState`] recorded it. A workspace absent from the
/// map has not been polled since this daemon started, which reads as an
/// unknown cache age rather than a fabricated recent one.
pub(crate) type FetchTimes = HashMap<WorkspaceKey, DateTime<Utc>>;

/// Cached workspaces whose stored JSON mentions any of `needles`, decoded.
///
/// The prefilter is what keeps a serving read cheap. Decoding *every* row
/// means parsing each workspace's whole activity feed — up to
/// [`lazybox_core::MAX_ACTIVITY_ITEMS`] entries apiece — and an agent can
/// call these tools in a loop, so a full decode per call is work the fleet
/// pays for repeatedly. A task's id key appears verbatim in the JSON of any
/// row holding it (as its own task, or as another task's `parent` /
/// `closes_issues` edge), so a substring match is a conservative superset:
/// it can only admit extra rows, never hide a matching one, and the exact
/// comparisons downstream reject the extras. Matching is case-insensitive
/// because repo case is not stable between config and the GitHub API, which
/// is the same reason [`addresses`] compares case-insensitively.
///
/// An empty `needles` decodes everything — the honest fallback for a caller
/// that cannot name what it is looking for.
///
/// Rows whose JSON no longer parses are skipped: a serving read must never
/// be the thing that surfaces a corrupt row, and the poller's own loader
/// already reports them. A *store* failure, by contrast, is returned — see
/// [`cached_workspaces`].
pub(crate) fn workspaces_matching(
    store: &dyn lazybox_store::Store,
    needles: &[String],
) -> Result<Vec<Workspace>, StoreError> {
    let lowered: Vec<String> = needles.iter().map(|n| n.to_lowercase()).collect();
    Ok(store
        .list_workspaces()?
        .into_iter()
        .filter_map(|record| record.workspace_json)
        .filter(|json| {
            if lowered.is_empty() {
                return true;
            }
            let haystack = json.to_lowercase();
            lowered.iter().any(|needle| haystack.contains(needle))
        })
        .filter_map(|json| serde_json::from_str::<Workspace>(&json).ok())
        .collect())
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
    let (task, comments, fetched_at) = workspaces.iter().find_map(|ws| {
        let task = ws
            .pr
            .iter()
            .chain(ws.gh_issues.iter())
            .chain(ws.linear_issues.iter())
            .find(|task| task.is_pr() == want_pr && addresses(task, repo, number))?;
        Some((task, ws.activity.as_slice(), fetched.get(&ws.key).copied()))
    })?;
    Some(TaskRecord::of(task, comments, fetched_at).with_sub_issues(all))
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

/// Cached issue records in `repo`, newest-updated first, each shrunk to
/// list scale by [`TaskRecord::into_summary`]. `state` filters on the
/// canonical [`lazybox_core::TaskState`] name, separator- and
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
                .map(move |task| (task, ws.activity.as_slice(), fetched.get(&ws.key).copied()))
        })
        .filter(|(task, _, _)| {
            task.repo
                .as_deref()
                .is_some_and(|r| r.eq_ignore_ascii_case(repo))
                || task
                    .id
                    .key
                    .rsplit_once('#')
                    .is_some_and(|(owner_repo, _)| owner_repo.eq_ignore_ascii_case(repo))
        })
        .filter(|(task, _, _)| state.is_none_or(|wanted| state_matches(task.state, wanted)))
        .map(|(task, comments, fetched_at)| TaskRecord::of(task, comments, fetched_at))
        .collect();
    records.sort_by_key(|record| std::cmp::Reverse(record.updated_at));
    records.truncate(limit);
    for record in &mut records {
        record.sub_issues = lazybox_core::sub_issue_ids(&record.id, all.iter().copied());
    }
    // A survey is for picking, not for reading: full bodies at list scale
    // are the context blowout this tool exists to prevent.
    records.into_iter().map(TaskRecord::into_summary).collect()
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
    let comments: &[Activity] = &ws.activity;
    let record = |task: &Task| {
        TaskRecord::of(task, comments, fetched_at).with_sub_issues(all.iter().copied())
    };
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
        content_warning: RECORD_CONTENT_WARNING.to_string(),
    }
}

/// The git *common* directory for the checkout at `worktree`, where
/// `info/exclude` lives.
///
/// Resolved from the on-disk layout rather than by running `git`: this is on
/// the spawn path, and `worktree_dir_ready` already reads the same pointer
/// file directly. `.git` is either a directory (a full clone) or a file
/// `gitdir: <path>`; when that path sits under `worktrees/<name>` — a linked
/// worktree — the common dir is two levels up, and otherwise (a submodule's
/// `modules/<name>`) it is the path itself.
///
/// Note that git does NOT honor a per-worktree `info/exclude`; only the
/// common one is read, which is why this resolves the common dir and not the
/// worktree's own git dir.
fn git_common_dir(worktree: &Path) -> Option<PathBuf> {
    let dot_git = worktree.join(".git");
    let meta = std::fs::metadata(&dot_git).ok()?;
    if meta.is_dir() {
        return Some(dot_git);
    }
    let pointer = std::fs::read_to_string(&dot_git).ok()?;
    let target = pointer.trim().strip_prefix("gitdir:")?.trim();
    let target = if Path::new(target).is_absolute() {
        PathBuf::from(target)
    } else {
        worktree.join(target)
    };
    match target.parent().and_then(Path::file_name) {
        Some(parent) if parent == "worktrees" => target
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf),
        _ => Some(target),
    }
}

/// Hide lazybox's own two paths inside `<worktree>/.lazybox/` from git by
/// naming them in the checkout's `info/exclude`, idempotently: the record
/// file [`TASK_FILE_RELATIVE_PATH`] and the artifact spool
/// [`ARTIFACT_SPOOL_RELATIVE_PATH`] (#1822).
///
/// Both land in one pass because the spool is written by the *agent*, at a
/// time the daemon does not choose — so its exclusion has to be in place
/// before the session starts, not added the first time an artifact appears.
///
/// Deliberately NOT a `.gitignore` in `.lazybox/`. That directory belongs to
/// the repository, not to lazybox — `<repo>/.lazybox/snippets.yaml` is a
/// shipped feature (`lazybox_config::snippets`) that users commit. An ignore
/// file there containing `*` makes `git add .lazybox/snippets.yaml` fail with
/// "paths are ignored by one of your .gitignore files", and because the
/// ignore file ignores itself it never appears in `git status`, so nothing
/// points at lazybox as the cause. It would also land in the user's own
/// clone for a `linked_checkout` workspace, which lazybox otherwise promises
/// never to modify, and would clobber a tracked `.lazybox/.gitignore` on
/// every spawn.
///
/// `info/exclude` is the mechanism git provides for exactly this — local,
/// never committed — and naming the single path leaves the rest of
/// `.lazybox/` addable.
pub(crate) fn exclude_lazybox_paths(worktree: &Path) -> std::io::Result<()> {
    let Some(common) = git_common_dir(worktree) else {
        // Not a git checkout (a scratch directory, a vanished worktree).
        // Nothing to exclude from, and nothing to fail about.
        return Ok(());
    };
    let patterns = [
        format!("/{TASK_FILE_RELATIVE_PATH}"),
        format!("/{ARTIFACT_SPOOL_RELATIVE_PATH}/"),
    ];
    let exclude = common.join("info").join("exclude");
    let existing = std::fs::read_to_string(&exclude).unwrap_or_default();
    let missing: Vec<&String> = patterns
        .iter()
        .filter(|pattern| !existing.lines().any(|line| line.trim() == pattern.as_str()))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(exclude.parent().expect("info/exclude has a parent"))?;
    let mut body = existing;
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    for pattern in missing {
        body.push_str(pattern);
        body.push('\n');
    }
    write_atomically(&exclude, body.as_bytes())
}

/// Replace `path`'s contents in one step: write a sibling temp file, then
/// rename over it.
///
/// A plain `fs::write` truncates before it writes, so a reader that opens
/// the file in that window sees a partial one. A respawn reuses the same
/// worktree path, so the reader racing the writer is an agent reading the
/// record while a second spawn rewrites it — and truncated JSON parses as
/// nothing rather than as an obvious error. Mirrors the tmp+rename the
/// config writer already uses (`lazybox_config`).
///
/// The temp name carries a per-process, per-call nonce because one of the
/// targets is **not** per-worktree: every worktree of a repo shares one
/// `info/exclude`, so a fixed `.exclude.tmp` beside it is a shared name two
/// concurrent spawns both write and both rename. The second rename then
/// fails `ENOENT` (the first already moved the file) and the spawn logs a
/// write failure for a file it wrote correctly.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let tmp = dir.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "lazybox".into()),
        std::process::id(),
        TMP_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    ));
    std::fs::write(&tmp, bytes)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::remove_file(&tmp);
            Err(error)
        }
    }
}

/// Write `file` into `worktree` as `.lazybox/task.json`, excluded from git.
///
/// Errors are returned rather than logged here so the spawn path can decide
/// — a read-only or vanished worktree must not fail a spawn.
pub(crate) fn write_record_file(
    worktree: &Path,
    file: &WorkspaceRecordFile,
) -> std::io::Result<()> {
    let path = worktree.join(TASK_FILE_RELATIVE_PATH);
    let dir = path
        .parent()
        .expect("TASK_FILE_RELATIVE_PATH always has a parent directory");
    std::fs::create_dir_all(dir)?;
    exclude_lazybox_paths(worktree)?;
    let json = serde_json::to_string_pretty(file)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    write_atomically(&path, json.as_bytes())
}

/// This workspace's record file, loaded without decoding the whole store.
///
/// Two narrow reads instead of one broad one: the row itself by key, then
/// only the rows whose JSON mentions one of its task ids — the conservative
/// superset that can contain a sub-issue. Both the `task` tool and the spawn
/// writer go through here, so neither pays a full-store decode.
pub(crate) fn record_file_for_workspace(
    store: &dyn lazybox_store::Store,
    fetched: &FetchTimes,
    key: &WorkspaceKey,
) -> Result<Option<WorkspaceRecordFile>, StoreError> {
    let Some(json) = store.get_workspace(key)?.and_then(|r| r.workspace_json) else {
        return Ok(None);
    };
    let Ok(ws) = serde_json::from_str::<Workspace>(&json) else {
        return Ok(None);
    };
    let needles: Vec<String> = ws
        .pr
        .iter()
        .chain(ws.gh_issues.iter())
        .chain(ws.linear_issues.iter())
        .map(|task| task.id.key.clone())
        .collect();
    let related = if needles.is_empty() {
        Vec::new()
    } else {
        workspaces_matching(store, &needles)?
    };
    Ok(Some(record_file_for(
        &ws,
        fetched.get(key).copied(),
        &tasks_of(&related),
    )))
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
    let loaded = crate::store_blocking(&config.store, move |store| {
        record_file_for_workspace(store, &fetched, &key)
    })
    .await;
    let file = match loaded {
        Ok(Some(file)) => file,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(
                %error,
                "task record: could not read the workspace — the session falls back to `gh`"
            );
            return;
        }
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
        ActivityKind, CiStatus, Label, Mergeable, ReviewStatus, TaskId, TaskKind, TaskRole,
        TaskState,
    };

    fn comment(n: usize) -> Activity {
        Activity {
            author: format!("a{n}"),
            body: format!("c{n}"),
            created_at: Utc::now(),
            kind: ActivityKind::Comment,
            node_id: Some(format!("node{n}")),
            path: None,
            line: None,
            diff_hunk: None,
            thread_id: None,
        }
    }

    fn save(config: &ServerConfig, ws: &Workspace) {
        config
            .store
            .save_workspace(&lazybox_store::WorkspaceRecord {
                key: ws.key.as_str().to_string(),
                created_at: Utc::now(),
                workspace_json: Some(serde_json::to_string(ws).expect("serialize")),
            })
            .expect("save workspace");
    }

    /// A store whose reads always fail — the boundary behind F7. Every
    /// `Store` method has a default, so overriding the two the cache reads
    /// is enough.
    #[derive(Debug)]
    struct FailingStore;

    impl lazybox_store::Store for FailingStore {
        fn list_workspaces(&self) -> Result<Vec<lazybox_store::WorkspaceRecord>, StoreError> {
            Err(StoreError::Backend("disk is on fire".into()))
        }

        fn get_workspace(
            &self,
            _key: &WorkspaceKey,
        ) -> Result<Option<lazybox_store::WorkspaceRecord>, StoreError> {
            Err(StoreError::Backend("disk is on fire".into()))
        }
    }

    /// A real git checkout — `.gitignore` semantics are the boundary under
    /// test in the exclusion cases, so they are exercised against git itself.
    fn git_checkout() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .expect("git runs");
            assert!(out.status.success(), "git {args:?} failed: {out:?}");
        };
        run(&["init", "-q", "."]);
        dir
    }

    fn git_ignores(dir: &Path, relative: &str) -> bool {
        std::process::Command::new("git")
            .args(["check-ignore", "-q", relative])
            .current_dir(dir)
            .status()
            .expect("git runs")
            .success()
    }

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

    /// #1799 review, F1. The first cut hid the record with a `.gitignore`
    /// containing `*` inside `.lazybox/` — a directory the *repository*
    /// owns: `<repo>/.lazybox/snippets.yaml` is a shipped lazybox feature
    /// users commit. That ignore made `git add .lazybox/snippets.yaml` fail,
    /// and because the ignore file ignores itself it never showed in
    /// `git status`, so nothing pointed at lazybox as the cause.
    #[test]
    fn the_record_is_hidden_from_git_without_shadowing_the_repos_own_lazybox_dir() {
        let dir = git_checkout();
        let ws = issue_workspace("a", "acme/widget#7");
        write_record_file(dir.path(), &record_file_for(&ws, Some(Utc::now()), &[])).expect("write");

        assert!(
            git_ignores(dir.path(), TASK_FILE_RELATIVE_PATH),
            "the record file must not show up in `git status`"
        );
        assert!(
            !dir.path().join(".lazybox/.gitignore").exists(),
            "a `.gitignore` in the repo's own `.lazybox/` is what broke `git add`"
        );

        // The repo's own file in the same directory must stay addable.
        std::fs::write(dir.path().join(".lazybox/snippets.yaml"), "x: 1\n").expect("write");
        assert!(
            !git_ignores(dir.path(), ".lazybox/snippets.yaml"),
            "the repo's committed lazybox config must remain addable"
        );
        let added = std::process::Command::new("git")
            .args(["add", ".lazybox/snippets.yaml"])
            .current_dir(dir.path())
            .status()
            .expect("git runs");
        assert!(
            added.success(),
            "`git add` of the repo's own file must succeed"
        );
    }

    #[test]
    fn excluding_the_record_is_idempotent_and_preserves_existing_excludes() {
        let dir = git_checkout();
        let exclude = dir.path().join(".git/info/exclude");
        std::fs::create_dir_all(exclude.parent().expect("parent")).expect("mkdir");
        std::fs::write(&exclude, "# mine\n*.swp\n").expect("seed");

        let ws = issue_workspace("a", "acme/widget#7");
        for _ in 0..3 {
            write_record_file(dir.path(), &record_file_for(&ws, None, &[])).expect("write");
        }

        let body = std::fs::read_to_string(&exclude).expect("read");
        assert!(
            body.contains("*.swp"),
            "a user's own excludes must survive: {body}"
        );
        assert_eq!(
            body.lines()
                .filter(|l| l.trim() == format!("/{TASK_FILE_RELATIVE_PATH}"))
                .count(),
            1,
            "re-spawning must not append the pattern again: {body}"
        );
    }

    #[test]
    fn the_artifact_spool_is_excluded_alongside_the_record() {
        // #1822: the spool is written by the *agent*, whenever it likes, so
        // its exclusion has to be in place before the session starts. Every
        // artifact would otherwise dirty the worktree and trip the
        // dirty-worktree delete refusal.
        let dir = git_checkout();
        let ws = issue_workspace("a", "acme/widget#7");
        write_record_file(dir.path(), &record_file_for(&ws, None, &[])).expect("write");

        std::fs::create_dir_all(dir.path().join(ARTIFACT_SPOOL_RELATIVE_PATH)).expect("mkdir");
        std::fs::write(
            dir.path()
                .join(ARTIFACT_SPOOL_RELATIVE_PATH)
                .join("plan.md"),
            "# Plan\n",
        )
        .expect("spool a file");

        let status = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(dir.path())
            .output()
            .expect("git runs");
        let out = String::from_utf8_lossy(&status.stdout);
        assert!(
            !out.contains(".lazybox"),
            "neither the record nor the spool may dirty the worktree: {out}"
        );
    }

    #[test]
    fn a_second_worktree_of_the_same_repo_can_exclude_concurrently() {
        // Every worktree of a repo shares ONE `info/exclude` in the git
        // common dir, so two spawns racing there both read-modify-write the
        // same file. With a fixed temp name beside it the second rename hit
        // `ENOENT` — the first had already moved the file — and the spawn
        // logged a write failure for a file it had written correctly.
        let dir = git_checkout();
        let worktree = dir.path().to_path_buf();
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let worktree = worktree.clone();
                std::thread::spawn(move || exclude_lazybox_paths(&worktree))
            })
            .collect();
        for handle in handles {
            handle.join().expect("thread").expect("exclude succeeds");
        }

        let body = std::fs::read_to_string(dir.path().join(".git/info/exclude")).expect("read");
        for pattern in [
            format!("/{TASK_FILE_RELATIVE_PATH}"),
            format!("/{ARTIFACT_SPOOL_RELATIVE_PATH}/"),
        ] {
            assert_eq!(
                body.lines().filter(|l| l.trim() == pattern).count(),
                1,
                "`{pattern}` must appear exactly once: {body}"
            );
        }
    }

    #[test]
    fn writing_into_a_non_git_directory_still_writes_the_record() {
        // A scratch or sandbox worktree may not be a git checkout at all.
        // There is nothing to exclude from, and that is not a failure.
        let dir = tempfile::tempdir().expect("tempdir");
        let ws = issue_workspace("a", "acme/widget#7");
        write_record_file(dir.path(), &record_file_for(&ws, None, &[])).expect("write");
        assert!(dir.path().join(TASK_FILE_RELATIVE_PATH).exists());
    }

    #[test]
    fn a_rewrite_leaves_no_partial_file_or_temp_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut ws = issue_workspace("a", "acme/widget#7");
        write_record_file(dir.path(), &record_file_for(&ws, None, &[])).expect("write");
        ws.gh_issues[0].title = "renamed upstream".into();
        write_record_file(dir.path(), &record_file_for(&ws, None, &[])).expect("rewrite");

        let written = std::fs::read_to_string(dir.path().join(TASK_FILE_RELATIVE_PATH))
            .expect("record file readable");
        let parsed: WorkspaceRecordFile = serde_json::from_str(&written).expect("valid json");
        assert_eq!(parsed.primary.expect("primary").title, "renamed upstream");
        // tmp+rename, not truncate-then-write: a reader can never see a
        // half-written record, and nothing is left lying around.
        let strays: Vec<_> = std::fs::read_dir(dir.path().join(".lazybox"))
            .expect("dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("tmp"))
            .collect();
        assert!(strays.is_empty(), "temp files left behind: {strays:?}");
    }

    #[tokio::test]
    async fn spawn_writes_the_record_the_daemon_already_holds() {
        // End-to-end over the store: what the spawn path actually puts in the
        // worktree, from a workspace the poller persisted.
        let config = ServerConfig::in_memory();
        let mut ws = issue_workspace("github-acme-widget-7", "acme/widget#7");
        // What a repo sweep leaves on the task (`comments(last: 1)`) versus
        // the durable feed the workspace accumulates.
        ws.gh_issues[0].recent_activity = vec![comment(29)];
        ws.activity = (0..30).map(comment).collect();
        save(&config, &ws);
        config.poll.note_tasks_fetched(&ws.key);

        let dir = tempfile::tempdir().expect("tempdir");
        write_record_file_for_spawn(&config, &ws.key, dir.path()).await;

        let written = std::fs::read_to_string(dir.path().join(TASK_FILE_RELATIVE_PATH))
            .expect("record file written at spawn");
        let parsed: WorkspaceRecordFile = serde_json::from_str(&written).expect("valid json");
        assert!(
            !parsed.content_warning.is_empty(),
            "the file must carry the untrusted-text framing (#1799 review, F4)"
        );
        let primary = parsed.primary.expect("primary");
        assert_eq!(primary.body, "body of acme/widget#7");
        assert!(
            primary.fetched_at.is_some(),
            "the record must carry the cache age the poller recorded"
        );
        // #1799 review, F2: reading the polled task's own activity gave one
        // comment and reported the thread complete.
        assert_eq!(primary.comments.len(), lazybox_core::RECORD_COMMENT_LIMIT);
        assert_eq!(primary.comments_omitted, 10);
    }

    /// The store-backed loader narrows to the target row plus the rows that
    /// could hold its children (#1799 review, F6). The narrowing must not
    /// lose a sub-issue that lives in another workspace.
    #[tokio::test]
    async fn the_narrowed_load_still_finds_a_sub_issue_in_another_workspace() {
        let config = ServerConfig::in_memory();
        let parent = issue_workspace("p", "acme/widget#1");
        let mut child = issue_workspace("c", "acme/widget#2");
        child.gh_issues[0].parent = Some(parent.gh_issues[0].id.clone());
        let unrelated = issue_workspace("u", "other/repo#3");
        save(&config, &parent);
        save(&config, &child);
        save(&config, &unrelated);

        let file = crate::store_blocking(&config.store, {
            let key = parent.key.clone();
            move |store| record_file_for_workspace(store, &FetchTimes::new(), &key)
        })
        .await
        .expect("store read")
        .expect("record file");
        assert_eq!(
            file.primary.expect("primary").sub_issues,
            vec!["github:acme/widget#2"],
            "narrowing by task id must still admit the row holding the child"
        );
    }

    /// Repo case is not stable between config and the GitHub API, so the
    /// prefilter matches case-insensitively — a case-sensitive `contains`
    /// would hide a record that `addresses` would have matched.
    #[tokio::test]
    async fn the_prefilter_matches_regardless_of_repo_case() {
        let config = ServerConfig::in_memory();
        save(&config, &issue_workspace("a", "Acme/Widget#7"));

        let found = crate::store_blocking(&config.store, move |store| {
            let needle = "acme/widget#7".to_string();
            workspaces_matching(store, std::slice::from_ref(&needle))
        })
        .await
        .expect("store read");
        assert_eq!(found.len(), 1, "a case difference must not hide the row");
    }

    /// #1799 review, F2, at the `get_issue` seam.
    #[test]
    fn find_record_carries_the_workspaces_durable_comment_feed() {
        let mut ws = issue_workspace("a", "acme/widget#7");
        ws.gh_issues[0].recent_activity = vec![comment(29)];
        ws.activity = (0..30).map(comment).collect();
        let found =
            find_record(&[ws], &FetchTimes::new(), "acme/widget", 7, false).expect("record");
        assert_eq!(found.comments.len(), lazybox_core::RECORD_COMMENT_LIMIT);
        assert_eq!(found.comments_omitted, 10);
    }

    /// #1799 review, F7. A store failure flattened to an empty result reads
    /// as "lazybox has never polled this record" — a fact an agent acts on,
    /// standing in for "lazybox could not look".
    #[tokio::test]
    async fn a_store_failure_is_an_error_not_an_empty_cache() {
        let error = crate::store_blocking(
            &(std::sync::Arc::new(FailingStore) as std::sync::Arc<dyn lazybox_store::Store>),
            |store| workspaces_matching(store, &[]),
        )
        .await
        .expect_err("a failing store must not read as an empty cache");
        assert!(format!("{error}").contains("disk is on fire"));

        let read = crate::store_blocking(
            &(std::sync::Arc::new(FailingStore) as std::sync::Arc<dyn lazybox_store::Store>),
            |store| record_file_for_workspace(store, &FetchTimes::new(), &WorkspaceKey::new("a")),
        )
        .await;
        assert!(read.is_err());
    }

    /// The spawn path must survive a store failure without writing a record
    /// that would claim the workspace has no tracker record.
    #[tokio::test]
    async fn spawn_writes_nothing_when_the_store_fails() {
        let config = ServerConfig::with_store(std::sync::Arc::new(FailingStore));
        let dir = tempfile::tempdir().expect("tempdir");
        write_record_file_for_spawn(&config, &WorkspaceKey::new("a"), dir.path()).await;
        assert!(!dir.path().join(TASK_FILE_RELATIVE_PATH).exists());
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
}
