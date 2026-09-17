//! Answer "are we working on `owner/repo#151`?" from the daemon's own live
//! state (#1785).
//!
//! The facts this joins already exist — they were just never joined by a
//! *record* reference, which is why answering the question took SQLite
//! archaeology in the session that prompted the issue. Three sources:
//!
//! - the persisted workspaces, matched on [`Workspace::hierarchy_task_ids`] so
//!   an issue still resolves through the PR workspace it folded into;
//! - [`crate::spawn_handler::agent_runtime_snapshot`], the live terminal
//!   registry — the only evidence that an agent turn is actually executing;
//! - the record's own `lazybox:w:` labels and this daemon's claim rows, which
//!   together say whether a claim is held *here* or by a worker we cannot see.
//!
//! Strictly read-only. It never materializes a workspace from the provider the
//! way [`crate::workspace::attach::attach_to_record`] does, never touches a
//! claim, and never spawns: asking what a worker is doing must not start one.

use crate::ServerConfig;
use chrono::Utc;
use lazybox_core::{Task, TaskId, Workspace};
use lazybox_ipc::task_status::{
    AgentFacts, BlockerFacts, ClaimFacts, ClaimHolder, SessionFacts, TASK_STATUS_SCHEMA_VERSION,
    TaskRefInfo, TaskStatusError, TaskStatusReport, TrackerFacts, WorkspaceStatus, derive_verdict,
};
use std::collections::HashSet;

/// Assemble the report for `id`.
///
/// `Err` is reserved for "the daemon could not establish status" — never for a
/// record nobody is working on, which is a perfectly good report with a
/// [`lazybox_ipc::task_status::WorkState::NoWorkspace`] verdict. Conflating the
/// two is how a caller ends up reporting "no worker" when it actually failed to
/// look.
pub async fn report(
    config: &ServerConfig,
    id: &TaskId,
) -> Result<TaskStatusReport, TaskStatusError> {
    let store = config.store.clone();
    let records = tokio::task::spawn_blocking(move || store.list_workspaces())
        .await
        .map_err(|error| TaskStatusError::Unavailable {
            detail: format!("workspace scan task failed: {error}"),
        })?
        .map_err(|error| TaskStatusError::Unavailable {
            detail: format!("read workspaces: {error}"),
        })?;

    // An undecodable row is reported, never skipped: silently dropping it could
    // turn "a worker is on this" into "nobody is".
    let mut unreadable = Vec::new();
    let mut matches = Vec::new();
    for record in records {
        let Some(json) = record.workspace_json else {
            unreadable.push(record.key);
            continue;
        };
        match Workspace::decode_persisted(&json) {
            Ok(workspace) => {
                if workspace.hierarchy_task_ids().any(|task| task == id) {
                    matches.push(workspace);
                }
            }
            Err(_) => unreadable.push(record.key),
        }
    }
    if !unreadable.is_empty() && matches.is_empty() {
        return Err(TaskStatusError::Unavailable {
            detail: format!(
                "{} workspace row(s) could not be decoded, so this record cannot be ruled out: {}",
                unreadable.len(),
                unreadable.join(", ")
            ),
        });
    }

    let runtimes = crate::spawn_handler::agent_runtime_snapshot(config).await;
    let held_here = crate::working_claims::locally_held_labels(config);
    let now = Utc::now();

    let mut workspaces: Vec<WorkspaceStatus> = matches
        .into_iter()
        .map(|workspace| workspace_status(config, workspace, id, &runtimes, &held_here, now))
        .collect();
    // Stable order so repeated queries read the same way.
    workspaces.sort_by(|a, b| a.key.as_str().cmp(b.key.as_str()));

    let archived = workspaces.is_empty() && {
        let key = lazybox_core::workspace_key_for_id(id);
        crate::workspace::load_archived_set(config).contains(&key)
    };

    Ok(TaskStatusReport {
        schema_version: TASK_STATUS_SCHEMA_VERSION,
        task: TaskRefInfo {
            id: id.clone(),
            repo: lazybox_core::task_ref::github_repo_of(id).map(str::to_string),
            number: id.number(),
        },
        observed_at: now,
        verdict: derive_verdict(&workspaces, archived),
        workspaces,
    })
}

fn workspace_status(
    config: &ServerConfig,
    workspace: Workspace,
    matched: &TaskId,
    runtimes: &[crate::spawn_handler::AgentTerminalRuntime],
    held_here: &HashSet<String>,
    now: chrono::DateTime<chrono::Utc>,
) -> WorkspaceStatus {
    let live: Vec<&crate::spawn_handler::AgentTerminalRuntime> = runtimes
        .iter()
        .filter(|runtime| runtime.session_key.as_str() == workspace.key.as_str())
        .collect();

    let matched_task = task_by_id(&workspace, matched);
    let headline = workspace.primary_task();

    WorkspaceStatus {
        key: workspace.key.clone(),
        name: workspace.name.clone(),
        matched: matched.clone(),
        tracker: headline.map(tracker_facts),
        // Only when the query matched something other than the headline —
        // otherwise the same record would be reported twice.
        matched_tracker: matched_task
            .filter(|task| headline.is_none_or(|head| head.id != task.id))
            .map(tracker_facts),
        role: workspace.effective_role(),
        claim: matched_task
            .map(|task| claim_facts(task, held_here, now))
            .unwrap_or_default(),
        blocker: crate::epics::load_declared(config, workspace.key.as_str())
            .ok()
            .flatten()
            .map(|blocker| BlockerFacts {
                reason: blocker.reason,
                kind: blocker.kind.as_str().to_string(),
                owner: format!("{:?}", blocker.owner).to_lowercase(),
                since: chrono::DateTime::from_timestamp_millis(blocker.since).unwrap_or(now),
            }),
        sessions: workspace
            .sessions
            .iter()
            .map(|session| SessionFacts {
                id: session.id.to_string(),
                name: session.name.clone(),
                state: session.state,
                created_at: session.created_at,
                last_output_at: session.last_output_at,
                has_live_agent: live
                    .iter()
                    .any(|runtime| runtime.session_id == Some(session.id)),
            })
            .collect(),
        agents: live
            .iter()
            .map(|runtime| AgentFacts {
                agent: runtime.agent_id.clone(),
                turn: runtime.agent_state,
                model: runtime.model_label.clone(),
                last_prompt_at: runtime.last_prompt.as_ref().and_then(|prompt| {
                    i64::try_from(prompt.timestamp_ms)
                        .ok()
                        .and_then(chrono::DateTime::from_timestamp_millis)
                }),
                on_main: runtime.on_main,
            })
            .collect(),
    }
}

fn task_by_id<'a>(workspace: &'a Workspace, id: &TaskId) -> Option<&'a Task> {
    workspace
        .pr
        .iter()
        .chain(workspace.gh_issues.iter())
        .chain(workspace.linear_issues.iter())
        .find(|task| &task.id == id)
}

fn tracker_facts(task: &Task) -> TrackerFacts {
    TrackerFacts {
        id: task.id.clone(),
        kind: task.kind.unwrap_or(if task.is_pr() {
            lazybox_core::TaskKind::Pr
        } else {
            lazybox_core::TaskKind::Issue
        }),
        title: task.title.clone(),
        url: task.url.clone(),
        state: task.state,
        ci: task.ci,
        review: task.review,
        updated_at: task.updated_at,
        closed_at: task.closed_at,
    }
}

/// Split the record's claim labels into live and lapsed, marking each with
/// whether *this* daemon is the one renewing it. An active claim nothing local
/// accounts for is the honest shape of "held by a worker we cannot observe".
fn claim_facts(
    task: &Task,
    held_here: &HashSet<String>,
    now: chrono::DateTime<chrono::Utc>,
) -> ClaimFacts {
    let mut facts = ClaimFacts {
        unqualified: task.has_label(lazybox_core::WORKING_LABEL_NAME),
        ..ClaimFacts::default()
    };
    for claim in task.qualified_working_claims() {
        let holder = ClaimHolder {
            device: claim.device.clone(),
            session: claim.session.clone(),
            expires_at: claim.expires_at,
            verified_locally: held_here.contains(&claim.label),
        };
        if claim.is_active_at(now) {
            facts.active.push(holder);
        } else {
            facts.expired.push(holder);
        }
    }
    facts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::SessionBackend as _;
    use lazybox_ipc::task_status::WorkState;

    fn task(repo: &str, number: u64, kind: lazybox_core::TaskKind) -> Task {
        let path = if matches!(kind, lazybox_core::TaskKind::Pr) {
            "pull"
        } else {
            "issues"
        };
        serde_json::from_value(serde_json::json!({
            "id": { "source": "github", "key": format!("{repo}#{number}") },
            "title": format!("task {number}"),
            "body": null,
            "state": "Open",
            "role": "Author",
            "ci": "None",
            "review": "None",
            "checks": [],
            "unread_count": 0,
            "url": format!("https://github.com/{repo}/{path}/{number}"),
            "repo": repo,
            "branch": null,
            "kind": kind,
            "needs_reply": false,
            "last_commenter": null,
            "updated_at": chrono::Utc::now(),
        }))
        .expect("fixture task")
    }

    fn id(repo: &str, number: u64) -> TaskId {
        TaskId {
            source: lazybox_core::GITHUB_SOURCE.to_string(),
            key: format!("{repo}#{number}"),
        }
    }

    fn save(config: &ServerConfig, workspace: &Workspace) {
        config
            .store
            .save_workspace(&lazybox_store::WorkspaceRecord {
                key: workspace.key.as_str().to_string(),
                created_at: Utc::now(),
                workspace_json: Some(serde_json::to_string(workspace).expect("encode")),
            })
            .expect("save");
    }

    #[tokio::test]
    async fn an_unknown_record_reports_no_workspace_rather_than_an_error() {
        let config = ServerConfig::in_memory();
        let report = report(&config, &id("o/r", 151)).await.expect("report");
        assert_eq!(report.verdict.state, WorkState::NoWorkspace);
        assert!(report.workspaces.is_empty());
        assert_eq!(report.schema_version, TASK_STATUS_SCHEMA_VERSION);
        assert_eq!(report.task.repo.as_deref(), Some("o/r"));
        assert_eq!(report.task.number, Some(151));
    }

    /// The #1785 regression: after the issue→PR fold the workspace is keyed by
    /// the PR, but a query for the *issue* must still find it — and must show
    /// the PR it produced.
    #[tokio::test]
    async fn an_issue_resolves_through_the_pr_workspace_it_folded_into() {
        let config = ServerConfig::in_memory();
        let mut workspace = Workspace::from_task(
            task("obin-ai/core-solutions", 187, lazybox_core::TaskKind::Pr),
            Utc::now(),
        );
        workspace.attach_task(task(
            "obin-ai/core-solutions",
            151,
            lazybox_core::TaskKind::Issue,
        ));
        save(&config, &workspace);

        let report = report(&config, &id("obin-ai/core-solutions", 151))
            .await
            .expect("report");
        assert_eq!(report.workspaces.len(), 1, "{:?}", report.workspaces);
        let found = &report.workspaces[0];
        assert_eq!(found.key.as_str(), "github-obin-ai-core-solutions-187");
        assert_eq!(
            found.matched,
            id("obin-ai/core-solutions", 151),
            "the report records which id the query matched"
        );
        assert_eq!(
            found.tracker.as_ref().expect("headline").id,
            id("obin-ai/core-solutions", 187),
            "the headline record is the PR"
        );
        assert_eq!(
            found.matched_tracker.as_ref().expect("matched").id,
            id("obin-ai/core-solutions", 151),
            "the issue's own lifecycle is reported alongside its PR's"
        );
    }

    #[tokio::test]
    async fn the_pr_side_of_the_same_row_answers_too() {
        let config = ServerConfig::in_memory();
        let mut workspace = Workspace::from_task(
            task("obin-ai/core-solutions", 187, lazybox_core::TaskKind::Pr),
            Utc::now(),
        );
        workspace.attach_task(task(
            "obin-ai/core-solutions",
            151,
            lazybox_core::TaskKind::Issue,
        ));
        save(&config, &workspace);

        let report = report(&config, &id("obin-ai/core-solutions", 187))
            .await
            .expect("report");
        let found = &report.workspaces[0];
        assert_eq!(found.key.as_str(), "github-obin-ai-core-solutions-187");
        assert!(
            found.matched_tracker.is_none(),
            "the headline record is not reported twice"
        );
    }

    #[tokio::test]
    async fn a_workspace_with_no_agent_is_not_started_not_missing() {
        let config = ServerConfig::in_memory();
        let workspace =
            Workspace::from_task(task("o/r", 7, lazybox_core::TaskKind::Issue), Utc::now());
        save(&config, &workspace);

        let report = report(&config, &id("o/r", 7)).await.expect("report");
        assert_eq!(report.verdict.state, WorkState::NotStarted);
        assert_eq!(report.workspaces.len(), 1);
        assert!(report.workspaces[0].agents.is_empty());
    }

    #[tokio::test]
    async fn an_archived_record_says_so_instead_of_unknown() {
        let config = ServerConfig::in_memory();
        let anchor = id("o/r", 9);
        assert!(crate::workspace::archive_workspace_key(
            &config,
            &lazybox_core::workspace_key_for_id(&anchor),
        ));

        let report = report(&config, &anchor).await.expect("report");
        assert_eq!(report.verdict.state, WorkState::Archived);
    }

    /// A claim label with no local claim row is a claim this daemon is not
    /// renewing — reported as held elsewhere, never as a running worker.
    #[tokio::test]
    async fn a_remote_claim_is_not_reported_as_a_local_worker() {
        let config = ServerConfig::in_memory();
        let mut anchor_task = task("o/r", 11, lazybox_core::TaskKind::Issue);
        let expires = Utc::now() + chrono::Duration::minutes(30);
        let label = lazybox_core::qualified_working_claim_label(
            "ffffffffffffffffffffffffffffffff",
            uuid::Uuid::new_v4(),
            expires,
        )
        .expect("label");
        anchor_task.labels.push(lazybox_core::Label::new(label));
        let workspace = Workspace::from_task(anchor_task, Utc::now());
        save(&config, &workspace);

        let report = report(&config, &id("o/r", 11)).await.expect("report");
        assert_eq!(report.verdict.state, WorkState::ClaimedElsewhere);
        let claim = &report.workspaces[0].claim;
        assert_eq!(claim.active.len(), 1);
        assert!(
            !claim.active[0].verified_locally,
            "no local claim row accounts for it"
        );
    }

    #[tokio::test]
    async fn an_expired_claim_is_kept_apart_from_a_live_one() {
        let config = ServerConfig::in_memory();
        let mut anchor_task = task("o/r", 12, lazybox_core::TaskKind::Issue);
        let label = lazybox_core::qualified_working_claim_label(
            "ffffffffffffffffffffffffffffffff",
            uuid::Uuid::new_v4(),
            Utc::now() - chrono::Duration::hours(2),
        )
        .expect("label");
        anchor_task.labels.push(lazybox_core::Label::new(label));
        save(&config, &Workspace::from_task(anchor_task, Utc::now()));

        let report = report(&config, &id("o/r", 12)).await.expect("report");
        let claim = &report.workspaces[0].claim;
        assert!(claim.active.is_empty());
        assert_eq!(claim.expired.len(), 1);
        assert_eq!(report.verdict.state, WorkState::AgentExited);
    }

    #[tokio::test]
    async fn a_declared_blocker_is_reported_with_its_age() {
        let config = ServerConfig::in_memory();
        let workspace =
            Workspace::from_task(task("o/r", 13, lazybox_core::TaskKind::Issue), Utc::now());
        save(&config, &workspace);
        crate::epics::persist_declared(
            &config,
            &crate::epics::DeclaredBlocker {
                workspace: workspace.key.clone(),
                reason: "waiting on the API contract".into(),
                kind: lazybox_ipc::BlockerKind::Contract,
                owner: lazybox_ipc::BlockerOwner::Operator,
                since: 1_700_000_000_000,
            },
        )
        .expect("persist");

        let report = report(&config, &id("o/r", 13)).await.expect("report");
        let blocker = report.workspaces[0].blocker.as_ref().expect("blocker");
        assert_eq!(blocker.reason, "waiting on the API contract");
        assert_eq!(blocker.kind, "contract");
        assert_eq!(blocker.since.timestamp_millis(), 1_700_000_000_000);
    }

    /// Two workspaces genuinely holding the record are both reported. The issue
    /// is explicit that a lookup must not pick an arbitrary first match.
    #[tokio::test]
    async fn every_matching_workspace_is_reported_not_just_the_first() {
        let config = ServerConfig::in_memory();
        let anchor = id("o/r", 21);
        let mut first =
            Workspace::from_task(task("o/r", 21, lazybox_core::TaskKind::Issue), Utc::now());
        first.key = lazybox_core::WorkspaceKey::new("github-o-r-21");
        save(&config, &first);
        let mut second =
            Workspace::from_task(task("o/r", 30, lazybox_core::TaskKind::Pr), Utc::now());
        second.attach_task(task("o/r", 21, lazybox_core::TaskKind::Issue));
        save(&config, &second);

        let report = report(&config, &anchor).await.expect("report");
        assert_eq!(report.workspaces.len(), 2, "{:?}", report.workspaces);
        assert_eq!(report.workspaces[0].key.as_str(), "github-o-r-21");
        assert_eq!(report.workspaces[1].key.as_str(), "github-o-r-30");
    }

    /// Register a live agent terminal in `workspace`, as a spawn would.
    async fn live_agent(
        config: &ServerConfig,
        mock: &crate::backend::MockBackend,
        key: &lazybox_core::WorkspaceKey,
        terminal_id: lazybox_ipc::TerminalId,
        state: lazybox_ipc::AgentState,
    ) {
        let session_key = lazybox_core::SessionKey::from(key);
        let backend_key = mock
            .spawn(&["claude".into()], None, &[], session_key.as_str())
            .await
            .expect("spawn mock terminal");
        config
            .terminal
            .register_terminal(
                terminal_id,
                backend_key,
                session_key,
                lazybox_ipc::TerminalKind::Agent("claude".into()),
            )
            .await;
        config
            .terminal
            .record_agent_state_generation(terminal_id, terminal_id.0)
            .await;
        config.terminal.record_agent_state(terminal_id, state).await;
    }

    /// The end-to-end shape the issue asks for: a worker running on the issue
    /// reads as `Working`; once its turn ends — having opened a PR that does
    /// not close the issue — the same query reads as `TurnEnded` with the issue
    /// still open. At no point does the report claim the task is finished.
    #[tokio::test]
    async fn a_worker_reads_as_working_then_turn_ended_with_the_issue_still_open() {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        let anchor = id("obin-ai/core-solutions", 151);
        let mut workspace = Workspace::from_task(
            task("obin-ai/core-solutions", 151, lazybox_core::TaskKind::Issue),
            Utc::now(),
        );
        workspace.key = lazybox_core::WorkspaceKey::new("github-obin-ai-core-solutions-151");
        save(&config, &workspace);
        live_agent(
            &config,
            &mock,
            &workspace.key,
            lazybox_ipc::TerminalId(1),
            lazybox_ipc::AgentState::Working,
        )
        .await;

        let working = report(&config, &anchor).await.expect("report");
        assert_eq!(working.verdict.state, WorkState::Working);
        assert!(working.verdict.state.is_currently_working());

        // The worker opens a PR and comes to rest. The PR joins the row; the
        // issue stays open because the PR does not close it.
        let mut with_pr = workspace.clone();
        with_pr.attach_task(task(
            "obin-ai/core-solutions",
            187,
            lazybox_core::TaskKind::Pr,
        ));
        save(&config, &with_pr);
        config
            .terminal
            .record_agent_state_generation(lazybox_ipc::TerminalId(1), 99)
            .await;
        config
            .terminal
            .record_agent_state(lazybox_ipc::TerminalId(1), lazybox_ipc::AgentState::Done)
            .await;

        let ended = report(&config, &anchor).await.expect("report");
        assert_eq!(ended.verdict.state, WorkState::TurnEnded);
        assert!(
            !ended.verdict.state.is_currently_working(),
            "a finished turn is not a running worker"
        );
        let found = &ended.workspaces[0];
        assert_eq!(
            found.tracker.as_ref().expect("headline").id,
            id("obin-ai/core-solutions", 187),
            "the PR is now the headline record"
        );
        assert_eq!(
            found.matched_tracker.as_ref().expect("matched").state,
            lazybox_core::TaskState::Open,
            "the issue the caller asked about is still open"
        );
    }

    /// Status is a read. It must not create the workspace the way `attach` does.
    #[tokio::test]
    async fn a_lookup_does_not_create_a_workspace() {
        let config = ServerConfig::in_memory();
        let before = config.store.list_workspaces().expect("list").len();
        let _ = report(&config, &id("o/r", 404)).await.expect("report");
        let after = config.store.list_workspaces().expect("list").len();
        assert_eq!(before, after, "a status lookup created a workspace row");
    }
}
