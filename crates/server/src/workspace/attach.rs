//! Attach work to the workspace a tracker record already has (#1586).
//!
//! A GitHub issue / PR or a Linear ticket gets exactly one workspace from the
//! poll, and every surface that starts work on it — the `spawn_worker` MCP
//! tool, `lazybox workspace create --issue`, the gateway — must land in *that*
//! row. A second, named workspace beside the record splits the branch, the
//! activity, the cost, the claim and the epic graph across two rows the fleet
//! cannot reconcile.
//!
//! Two entry points:
//!
//! - [`attach_to_record`] resolves a [`TaskId`] to its workspace, running a
//!   single-item sync first when the poll has not reached the record yet (an
//!   issue filed seconds ago).
//! - [`resolve_named_create`] is the guard on the *anchor-less* path: a bare
//!   name under a repo-scoped project is either redirected onto the open task
//!   it names or refused, unless the caller declared it scratch.

use crate::ServerConfig;
use lazybox_core::{ProjectKey, Task, TaskId, TaskState, Workspace, WorkspaceKey};

/// Why a record could not be attached to. Every variant is caller-facing text:
/// an agent or a CLI user reads it and has to know what to do next.
#[derive(Debug, thiserror::Error)]
pub enum AttachError {
    #[error("no GitHub credentials — cannot look up {0}")]
    NoGitHubClient(TaskId),
    #[error("look up {task}: {message}")]
    Lookup { task: TaskId, message: String },
    #[error(
        "{0} has no lazybox workspace and could not be fetched — check the reference, and that \
         the record is visible to your token"
    )]
    Unresolved(TaskId),
}

/// The workspace holding `anchor`, materializing it from the provider when the
/// poll has not created it yet.
///
/// The two-step (scan, then sync, then scan again) is what makes
/// "file the issue, then work in its workspace" usable inside one agent turn:
/// the issue is seconds old, no poll tick has run, and without the sync the
/// caller would see "no workspace" and reach for a named one.
pub async fn attach_to_record(
    config: &ServerConfig,
    anchor: &TaskId,
) -> Result<WorkspaceKey, AttachError> {
    if let Some(key) = workspace_for_task(config, anchor) {
        return Ok(key);
    }
    materialize(config, anchor).await?;
    workspace_for_task(config, anchor).ok_or_else(|| AttachError::Unresolved(anchor.clone()))
}

/// The workspace whose tasks include `id`. Matches on the whole hierarchy, not
/// the primary task, so an issue that has since acquired a PR still resolves
/// under its own id — the issue→PR fold puts both on one row.
pub fn workspace_for_task(config: &ServerConfig, id: &TaskId) -> Option<WorkspaceKey> {
    load_workspaces(config)
        .into_iter()
        .find(|ws| ws.hierarchy_task_ids().any(|task| task == id))
        .map(|ws| ws.key)
}

/// Single-item fetch + upsert for a record the poll has not seen. GitHub only:
/// a Linear ticket's workspace is created by the Linear poll, and a targeted
/// fetch there needs the node id the identifier alone does not carry.
async fn materialize(config: &ServerConfig, anchor: &TaskId) -> Result<(), AttachError> {
    let Some(slug) = lazybox_core::task_ref::github_repo_of(anchor) else {
        return Ok(());
    };
    let Some((owner, repo)) = slug.split_once('/') else {
        return Ok(());
    };
    let Some(number) = anchor.number() else {
        return Ok(());
    };
    let client = crate::polling::resolve_gh_client_result(config)
        .await
        .map_err(|_| AttachError::NoGitHubClient(anchor.clone()))?;

    // `owner/repo#N` does not say whether N is an issue or a PR — GitHub
    // numbers them from one sequence — so try the issue connection first and
    // fall back to the PR one, which is what the number turns out to be for a
    // reference to work already in review.
    let fetched = match client
        .fetch_single_issue_interactive(owner, repo, number)
        .await
    {
        Ok(Some(task)) => Some(task),
        Ok(None) => client
            .fetch_single_pr_interactive(owner, repo, number)
            .await
            .map_err(|e| AttachError::Lookup {
                task: anchor.clone(),
                message: e.to_string(),
            })?,
        Err(e) => {
            return Err(AttachError::Lookup {
                task: anchor.clone(),
                message: e.to_string(),
            });
        }
    };
    if let Some(task) = fetched {
        crate::polling::upsert(config, task).await;
    }
    Ok(())
}

/// What an anchor-less `CreateWorkspace` should actually do.
#[derive(Debug, PartialEq, Eq)]
pub enum NamedCreate {
    /// No repo scope to violate (a local project) or the caller declared
    /// scratch — create the named workspace as asked.
    Create,
    /// The name resolves to an open task in this project's repo. Its workspace
    /// is the target; `notice` explains the redirect to the caller.
    Attach { anchor: TaskId, notice: String },
    /// A repo-scoped named create with nothing to attach to. `message` carries
    /// the rule and the way to comply.
    Refuse { message: String },
}

/// Decide an anchor-less create. A named workspace under a repo is the shape
/// that produces a side row beside a tracker record, so it needs either a
/// record to attach to or an explicit `scratch` declaration; a local
/// (repo-less) project is scratch by construction and always allowed.
pub fn resolve_named_create(
    config: &ServerConfig,
    name: &str,
    project_key: &ProjectKey,
    scratch: bool,
) -> NamedCreate {
    if scratch || project_key.source_prefix() != "github" {
        return NamedCreate::Create;
    }
    if let Some(task) = open_task_matching(config, name, project_key) {
        let notice = format!(
            "attached to {} instead of creating a workspace beside it",
            task.id.key
        );
        return NamedCreate::Attach {
            anchor: task.id,
            notice,
        };
    }
    NamedCreate::Refuse {
        message: format!(
            "refusing to create the workspace \"{name}\" beside a tracked repo: a GitHub issue / \
             PR already gets exactly one workspace, and a second named row splits its branch, \
             activity, cost and epic graph. File the record and attach to it — `gh issue create \
             --repo <owner/repo> …` then `lazybox workspace create --issue <owner/repo#N>` — or \
             pass `--scratch` if this really is repo-less scratch."
        ),
    }
}

/// The open task in `project_key`'s repo that `name` names, if any. Matches a
/// `#N` / `N` reference or the task title after normalization, so the shapes
/// an agent naturally types for a workspace ("1586", "#1586", the issue title)
/// all land on the record instead of beside it.
fn open_task_matching(config: &ServerConfig, name: &str, project_key: &ProjectKey) -> Option<Task> {
    let needle = normalize(name);
    if needle.is_empty() {
        return None;
    }
    let number: Option<u64> = needle.trim_start_matches('#').parse().ok();
    load_workspaces(config)
        .into_iter()
        .filter(|ws| lazybox_core::workspace_project_key(ws).as_ref() == Some(project_key))
        .flat_map(|ws| {
            ws.pr
                .into_iter()
                .chain(ws.gh_issues)
                .chain(ws.linear_issues)
        })
        .filter(|task| !matches!(task.state, TaskState::Merged | TaskState::Closed))
        .find(|task| match number {
            Some(number) => task.id.number() == Some(number),
            None => normalize(&task.title) == needle,
        })
}

/// Lowercase, drop punctuation, collapse whitespace — so `Fix the parser!` and
/// `fix-the-parser` compare equal.
fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for ch in text.trim().chars() {
        if ch.is_alphanumeric() || ch == '#' {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.extend(ch.to_lowercase());
        } else {
            pending_space = true;
        }
    }
    out
}

fn load_workspaces(config: &ServerConfig) -> Vec<Workspace> {
    config
        .store
        .list_workspaces()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|record| record.workspace_json)
        .filter_map(|json| Workspace::decode_persisted(&json).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ServerConfig;

    fn task(repo: &str, number: u64, title: &str, state: &str) -> Task {
        serde_json::from_value(serde_json::json!({
            "id": { "source": "github", "key": format!("{repo}#{number}") },
            "title": title,
            "body": null,
            "state": state,
            "role": "Author",
            "ci": "None",
            "review": "None",
            "checks": [],
            "unread_count": 0,
            "url": format!("https://github.com/{repo}/issues/{number}"),
            "repo": repo,
            "branch": null,
            "needs_reply": false,
            "last_commenter": null,
            "updated_at": chrono::Utc::now(),
        }))
        .expect("seeded task")
    }

    fn save(config: &ServerConfig, ws: &Workspace) {
        config
            .store
            .save_workspace(&lazybox_store::WorkspaceRecord {
                key: ws.key.as_str().to_string(),
                created_at: chrono::Utc::now(),
                workspace_json: Some(serde_json::to_string(ws).expect("serialize")),
            })
            .expect("save");
    }

    /// The row a poll would have built for one GitHub issue.
    fn seed_issue(config: &ServerConfig, key: &str, repo: &str, number: u64, title: &str) {
        let mut ws = Workspace::empty(WorkspaceKey::new(key), "branch", chrono::Utc::now());
        ws.project_key = Some(github_project(repo));
        ws.gh_issues.push(task(repo, number, title, "Open"));
        save(config, &ws);
    }

    fn github_project(repo: &str) -> ProjectKey {
        let (owner, name) = repo.split_once('/').expect("owner/repo");
        ProjectKey::github(owner, name)
    }

    fn gh(key: &str) -> TaskId {
        TaskId {
            source: "github".into(),
            key: key.into(),
        }
    }

    #[test]
    fn workspace_for_task_finds_the_row_by_any_of_its_ids() {
        let config = ServerConfig::in_memory();
        // The issue→PR fold puts both on one row; the issue must still resolve
        // under its own id once the PR became the headline task.
        let mut ws = Workspace::empty(WorkspaceKey::new("row"), "branch", chrono::Utc::now());
        ws.pr = Some(task("acme/widget", 8, "the PR", "Open"));
        ws.gh_issues
            .push(task("acme/widget", 7, "the issue", "Open"));
        save(&config, &ws);

        assert_eq!(
            workspace_for_task(&config, &gh("acme/widget#7")),
            Some(WorkspaceKey::new("row"))
        );
        assert_eq!(
            workspace_for_task(&config, &gh("acme/widget#8")),
            Some(WorkspaceKey::new("row"))
        );
        assert_eq!(workspace_for_task(&config, &gh("acme/widget#9")), None);
    }

    #[test]
    fn named_create_matching_an_open_issue_attaches() {
        let config = ServerConfig::in_memory();
        seed_issue(
            &config,
            "github-acme-widget-7",
            "acme/widget",
            7,
            "Fix the parser",
        );
        let project = github_project("acme/widget");

        // By number, with and without the `#`, and by title after
        // normalization — the shapes an agent naturally reaches for.
        for name in ["7", "#7", "Fix the parser", "fix-the-parser"] {
            match resolve_named_create(&config, name, &project, false) {
                NamedCreate::Attach { anchor, notice } => {
                    assert_eq!(anchor, gh("acme/widget#7"), "{name}");
                    assert!(notice.contains("acme/widget#7"), "{notice}");
                }
                other => panic!("{name:?} should attach, got {other:?}"),
            }
        }
    }

    #[test]
    fn named_create_ignores_a_closed_task_and_another_repo() {
        let config = ServerConfig::in_memory();
        seed_issue(
            &config,
            "github-acme-widget-7",
            "acme/widget",
            7,
            "Fix the parser",
        );
        let mut closed =
            Workspace::empty(WorkspaceKey::new("closed"), "branch", chrono::Utc::now());
        closed.project_key = Some(github_project("acme/widget"));
        closed
            .gh_issues
            .push(task("acme/widget", 8, "Old work", "Closed"));
        save(&config, &closed);

        // A closed record is not something to attach work to.
        assert!(matches!(
            resolve_named_create(&config, "Old work", &github_project("acme/widget"), false),
            NamedCreate::Refuse { .. }
        ));
        // …and a record in a different repo must not be matched by number.
        assert!(matches!(
            resolve_named_create(&config, "7", &github_project("other/repo"), false),
            NamedCreate::Refuse { .. }
        ));
    }

    #[test]
    fn named_create_under_a_repo_without_scratch_is_refused() {
        let config = ServerConfig::in_memory();
        let refusal = resolve_named_create(
            &config,
            "spike the cache",
            &github_project("acme/widget"),
            false,
        );
        match refusal {
            NamedCreate::Refuse { message } => {
                // The refusal has to carry the rule AND the way to comply, or
                // the caller just retries with a different name.
                assert!(message.contains("gh issue create"), "{message}");
                assert!(message.contains("--scratch"), "{message}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn scratch_and_local_projects_still_create() {
        let config = ServerConfig::in_memory();
        assert_eq!(
            resolve_named_create(
                &config,
                "spike the cache",
                &github_project("acme/widget"),
                true
            ),
            NamedCreate::Create
        );
        // A repo-less project has no record for a named row to shadow.
        assert_eq!(
            resolve_named_create(&config, "notes", &ProjectKey::local("scratch"), false),
            NamedCreate::Create
        );
    }

    #[tokio::test]
    async fn attach_to_record_returns_the_existing_row() {
        let config = ServerConfig::in_memory();
        seed_issue(
            &config,
            "github-acme-widget-7",
            "acme/widget",
            7,
            "Fix the parser",
        );
        let key = attach_to_record(&config, &gh("acme/widget#7"))
            .await
            .expect("the seeded row resolves");
        assert_eq!(key.as_str(), "github-acme-widget-7");
    }

    #[tokio::test]
    async fn attach_to_record_reports_an_unresolvable_linear_ticket() {
        // No workspace and no GitHub materialization path: the caller must be
        // told, not handed a fresh row.
        let config = ServerConfig::in_memory();
        let err = attach_to_record(
            &config,
            &TaskId {
                source: "linear".into(),
                key: "ENG-45".into(),
            },
        )
        .await
        .expect_err("nothing to attach to");
        assert!(err.to_string().contains("ENG-45"), "{err}");
    }
}
