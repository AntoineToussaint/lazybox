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

    // An undecodable row is surfaced, never skipped: it could itself be a
    // workspace holding this record, so dropping it would turn "a worker is on
    // this" into "nobody is" — or present one match as the whole answer.
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
                unreadable.join(", "),
            ),
        });
    }

    let runtimes = crate::spawn_handler::agent_runtime_snapshot(config).await;
    let held_here = crate::working_claims::locally_held_claims(config);
    let now = Utc::now();

    let mut workspaces: Vec<WorkspaceStatus> = Vec::with_capacity(matches.len());
    for workspace in matches {
        workspaces.push(workspace_status(config, workspace, id, &runtimes, &held_here, now).await);
    }
    // Stable order so repeated queries read the same way.
    workspaces.sort_by(|a, b| a.key.as_str().cmp(b.key.as_str()));

    // Archiving tombstones the key of the workspace actually archived
    // (`workspace::archive_workspace_key`), so this finds a record archived
    // under its *own* standalone key. A record that had already folded into
    // another row is tombstoned under that row's key instead, and the fold link
    // died with the deleted row — so there is nothing left to resolve it back.
    // Such a record reports `NoWorkspace`, whose wording is careful to claim
    // only that this daemon holds no workspace for it, never that nobody
    // worked on it. `archived_after_a_fold_is_reported_as_no_workspace` pins
    // the boundary so `Archived` is not read as exhaustive.
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
        unreadable_workspaces: unreadable,
    })
}

async fn workspace_status(
    config: &ServerConfig,
    workspace: Workspace,
    matched: &TaskId,
    runtimes: &[crate::spawn_handler::AgentTerminalRuntime],
    held_here: &HashSet<(String, String)>,
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
        claim: match matched_task {
            Some(task) => claim_facts(config, task, held_here, now).await,
            None => Default::default(),
        },
        blocker: crate::epics::load_declared(config, workspace.key.as_str())
            .ok()
            .flatten()
            .map(|blocker| BlockerFacts {
                reason: blocker.reason,
                kind: blocker.kind.as_str().to_string(),
                owner: format!("{:?}", blocker.owner).to_lowercase(),
                since: chrono::DateTime::from_timestamp_millis(blocker.since),
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

/// Split the record's claims into live and lapsed, marking each with whether
/// *this* daemon is the one renewing it. An active claim nothing local
/// accounts for is the honest shape of "held by a worker we cannot observe".
///
/// This is the **decision point** that pays for the claim's identity (#1922).
/// Presence is the `working` label and arrives free in the poll payload, but
/// the label alone names no holder — so when it is attached, this resolves the
/// holder from the sticky claim comment: one `Cold` REST read, and only for a
/// record that actually carries the label. Never on a poll tick.
///
/// Two reasons the fetch is skipped, each a deliberate saving rather than an
/// omission:
///
/// - A legacy `lazybox:w:` label already carries its own holder and expiry in
///   its name, so there is nothing left to look up.
/// - A lease this box is itself renewing is already known locally, and the
///   local record is the better evidence anyway: a renewal that landed here
///   and was refused upstream leaves the two sides one expiry apart (#1870).
async fn claim_facts(
    config: &ServerConfig,
    task: &Task,
    held_here: &HashSet<(String, String)>,
    now: chrono::DateTime<chrono::Utc>,
) -> ClaimFacts {
    let mut facts = ClaimFacts::default();
    for claim in task.qualified_working_claims() {
        let holder = ClaimHolder {
            device: claim.device.clone(),
            session: claim.session.clone(),
            expires_at: claim.expires_at,
            verified_locally: held_here.contains(&(claim.device.clone(), claim.session.clone())),
            agent: None,
            model: None,
            started_at: None,
            workspace: None,
        };
        if claim.is_active_at(now) {
            facts.active.push(holder);
        } else {
            facts.expired.push(holder);
        }
    }
    if !task.has_stable_working_claim() {
        return facts;
    }
    match stable_claim_holder(config, task, held_here).await {
        Some(holder) => {
            if holder.expires_at > now {
                facts.active.push(holder);
            } else {
                facts.expired.push(holder);
            }
        }
        // The label stands with nothing of ours behind it — or nothing we
        // could read. Reported as unbacked, never resolved into a holder:
        // `ClaimFacts::unbacked_label` documents why that distinction has to
        // survive all the way to the caller.
        None => facts.unbacked_label = true,
    }
    facts
}

/// Resolve the stable label's holder from lazybox's own claim comment.
async fn stable_claim_holder(
    config: &ServerConfig,
    task: &Task,
    held_here: &HashSet<(String, String)>,
) -> Option<ClaimHolder> {
    let repo = task.repo.as_deref()?;
    let client = crate::polling::resolve_gh_client_result(config)
        .await
        .ok()?;
    let note = client
        .read_working_claim_note(&task.id, repo)
        .await
        .inspect_err(|error| {
            tracing::debug!(
                task = %task.id,
                %error,
                "could not read the claim comment; reporting the label as unbacked"
            );
        })
        .ok()??;
    // A released note is a record of finished work, not a claim — surface it
    // as lapsed by handing back its own release time as the expiry, so the
    // verdict never reads "claimed elsewhere" for work the holder said is over.
    let expires_at = note.released_at.unwrap_or(note.expires_at);
    Some(ClaimHolder {
        verified_locally: held_here.contains(&(note.device.clone(), note.session.clone())),
        device: note.device,
        session: note.session,
        expires_at,
        agent: note.agent,
        model: note.model,
        started_at: Some(note.started_at),
        workspace: note.workspace,
    })
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

    /// A canned comment list served over one connection per request, with
    /// every request recorded — enough to drive the claim-comment read.
    async fn spawn_comment_server(
        bodies: Vec<&'static str>,
        requests: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let mut served = 0usize;
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    continue;
                };
                let body = bodies[served.min(bodies.len() - 1)];
                served += 1;
                let requests = requests.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let read = sock.read(&mut buf).await.unwrap_or(0);
                    requests
                        .lock()
                        .expect("record")
                        .push(String::from_utf8_lossy(&buf[..read]).into_owned());
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len(),
                    );
                    let _ = sock.write_all(response.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        format!("http://{addr}")
    }

    fn comment_json(id: u64, login: &str, body: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "node_id": "IC_1",
            "url": "https://api.github.test/c",
            "html_url": "https://api.github.test/c",
            "body": body,
            "user": {
                "login": login,
                "id": 1,
                "node_id": "U_1",
                "avatar_url": "https://example.invalid/a",
                "gravatar_id": "",
                "url": "https://example.invalid/u",
                "html_url": "https://example.invalid/u",
                "followers_url": "https://example.invalid/u",
                "following_url": "https://example.invalid/u",
                "gists_url": "https://example.invalid/u",
                "starred_url": "https://example.invalid/u",
                "subscriptions_url": "https://example.invalid/u",
                "organizations_url": "https://example.invalid/u",
                "repos_url": "https://example.invalid/u",
                "events_url": "https://example.invalid/u",
                "received_events_url": "https://example.invalid/u",
                "type": "User",
                "site_admin": false,
                "name": null,
                "patch_url": null,
            },
            "created_at": "2026-08-18T12:00:00Z",
        })
    }

    fn remote_claim_note() -> lazybox_core::WorkingClaimNote {
        let mut note = lazybox_core::WorkingClaimNote::new(
            "fedcba9876543210fedc",
            "aaaaaaaaaa",
            Utc::now() - chrono::Duration::hours(2),
            Utc::now() + chrono::Duration::minutes(30),
        );
        note.agent = Some("codex".into());
        note.model = Some("GPT-5".into());
        note.workspace = Some("github-o-r-77".into());
        note
    }

    /// Seed a workspace whose record carries the stable `working` label, with
    /// `bodies` standing in for what GitHub returns for its comments.
    async fn claimed_workspace(
        bodies: Vec<&'static str>,
    ) -> (ServerConfig, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let config = ServerConfig::in_memory();
        let mut claimed = task("o/r", 77, lazybox_core::TaskKind::Issue);
        claimed
            .labels
            .push(lazybox_core::Label::new(lazybox_core::WORKING_LABEL_NAME));
        save(&config, &Workspace::from_task(claimed, Utc::now()));
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let base_uri = spawn_comment_server(bodies, requests.clone()).await;
        config.poll.cache_gh_client(
            lazybox_gh::GhClient::stub_with_base_uri_for_tests(&base_uri).expect("stub client"),
        );
        (config, requests)
    }

    /// The decision-point read (#1922). The `working` label is presence and
    /// arrives free in the poll payload; the holder comes from lazybox's own
    /// claim comment, fetched here and nowhere on the poll path.
    #[tokio::test]
    async fn a_stable_claim_resolves_its_holder_from_the_claim_comment() {
        let body = Box::leak(
            serde_json::json!([comment_json(
                9,
                lazybox_gh::GhClient::stub_login_for_tests(),
                &remote_claim_note().render()
            )])
            .to_string()
            .into_boxed_str(),
        );
        let (config, requests) = claimed_workspace(vec![body]).await;

        let report = report(&config, &id("o/r", 77)).await.expect("report");
        let claim = &report.workspaces[0].claim;
        assert!(
            !claim.unbacked_label,
            "a claim lazybox authored is backed, not unbacked"
        );
        assert_eq!(claim.active.len(), 1, "{claim:?}");
        let held = &claim.active[0];
        assert_eq!(held.device, "fedcba9876543210fedc");
        assert_eq!(held.agent.as_deref(), Some("codex"));
        assert_eq!(held.model.as_deref(), Some("GPT-5"));
        assert_eq!(held.workspace.as_deref(), Some("github-o-r-77"));
        assert!(held.started_at.is_some(), "the comment dates the claim");
        assert!(
            !held.verified_locally,
            "no local claim row accounts for this lease"
        );
        assert_eq!(
            report.verdict.state,
            WorkState::ClaimedElsewhere,
            "{:?}",
            report.verdict
        );
        // The cost of the whole answer.
        assert_eq!(
            requests.lock().expect("record").len(),
            1,
            "one comment read, and only because the label was there"
        );
    }

    /// The #1600 property at the layer that acts on it. Only writers can
    /// label; anyone can comment. A perfectly-formed claim note from any
    /// other login must not resolve into a holder — otherwise a drive-by
    /// comment invents a worker, and `task_status` reports work nobody is
    /// doing.
    #[tokio::test]
    async fn a_forged_claim_comment_leaves_the_label_unbacked() {
        let body = Box::leak(
            serde_json::json!([comment_json(
                9,
                "someone-else",
                &remote_claim_note().render()
            )])
            .to_string()
            .into_boxed_str(),
        );
        let (config, _requests) = claimed_workspace(vec![body]).await;

        let report = report(&config, &id("o/r", 77)).await.expect("report");
        let claim = &report.workspaces[0].claim;
        assert!(
            claim.active.is_empty() && claim.expired.is_empty(),
            "a forged note must produce NO holder: {claim:?}"
        );
        assert!(
            claim.unbacked_label,
            "the label stands, so it is reported — as unbacked"
        );
        assert_eq!(report.verdict.state, WorkState::Unknown);
        assert!(
            report
                .verdict
                .reason
                .contains("no lazybox-authored claim comment"),
            "{}",
            report.verdict.reason
        );
    }

    /// A released note is a record of finished work. Reporting it as an active
    /// claim would say "claimed elsewhere" about a task the holder has
    /// explicitly let go.
    #[tokio::test]
    async fn a_released_claim_comment_reads_as_lapsed_not_as_held() {
        let mut note = remote_claim_note();
        note.released_at = Some(Utc::now() - chrono::Duration::minutes(5));
        let body = Box::leak(
            serde_json::json!([comment_json(
                9,
                lazybox_gh::GhClient::stub_login_for_tests(),
                &note.render()
            )])
            .to_string()
            .into_boxed_str(),
        );
        let (config, _requests) = claimed_workspace(vec![body]).await;

        let report = report(&config, &id("o/r", 77)).await.expect("report");
        let claim = &report.workspaces[0].claim;
        assert!(claim.active.is_empty(), "{claim:?}");
        assert_eq!(claim.expired.len(), 1, "{claim:?}");
        assert_ne!(report.verdict.state, WorkState::ClaimedElsewhere);
    }

    /// The saving that keeps the read off the hot path: a record with no
    /// `working` label costs nothing, and a legacy label already carries its
    /// own holder in its name so there is nothing left to look up.
    #[tokio::test]
    async fn an_unclaimed_or_legacy_claimed_record_never_reaches_github() {
        let config = ServerConfig::in_memory();
        let mut legacy = task("o/r", 78, lazybox_core::TaskKind::Issue);
        let label = lazybox_core::qualified_working_claim_label(
            "0123456789abcdef0123456789abcdef",
            uuid::Uuid::from_u128(7),
            Utc::now() + chrono::Duration::minutes(30),
        )
        .expect("a well-formed box id yields a label");
        legacy.labels.push(lazybox_core::Label::new(&label));
        save(&config, &Workspace::from_task(legacy, Utc::now()));
        save(
            &config,
            &Workspace::from_task(task("o/r", 79, lazybox_core::TaskKind::Issue), Utc::now()),
        );
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let base_uri = spawn_comment_server(vec!["[]"], requests.clone()).await;
        config.poll.cache_gh_client(
            lazybox_gh::GhClient::stub_with_base_uri_for_tests(&base_uri).expect("stub client"),
        );

        let legacy_report = report(&config, &id("o/r", 78)).await.expect("report");
        assert_eq!(legacy_report.workspaces[0].claim.active.len(), 1);
        assert!(!legacy_report.workspaces[0].claim.unbacked_label);
        let plain = report(&config, &id("o/r", 79)).await.expect("report");
        assert!(plain.workspaces[0].claim.is_empty());

        assert!(
            requests.lock().expect("record").is_empty(),
            "neither shape may spend a GitHub request: {:?}",
            requests.lock().expect("record")
        );
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

    /// Known boundary of the `Archived` verdict: once an issue has folded into
    /// its PR workspace, archiving tombstones the *PR* key, and the deleted
    /// issue row took the link with it — so a query for the issue cannot tell
    /// "archived" from "never seen". This pins that it degrades to
    /// `NoWorkspace` (whose reason claims only that this daemon holds no
    /// workspace) rather than silently claiming the work never happened.
    #[tokio::test]
    async fn archived_after_a_fold_is_reported_as_no_workspace() {
        let config = ServerConfig::in_memory();
        let issue = id("o/r", 151);
        // The user archived the folded row, which is keyed by the PR.
        assert!(crate::workspace::archive_workspace_key(
            &config,
            &lazybox_core::workspace_key_for_id(&id("o/r", 187)),
        ));

        let report = report(&config, &issue).await.expect("report");
        assert_eq!(
            report.verdict.state,
            WorkState::NoWorkspace,
            "the fold link is gone with the row, so Archived is not recoverable here"
        );
        assert!(
            report
                .verdict
                .reason
                .contains("no workspace on this daemon"),
            "the reason must not claim nobody ever worked on it: {}",
            report.verdict.reason
        );
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
        assert_eq!(
            blocker
                .since
                .expect("a readable timestamp")
                .timestamp_millis(),
            1_700_000_000_000
        );
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

    /// An undecodable row alongside a good match must not vanish: it could be a
    /// second workspace holding this record, which would make the answer
    /// partial while reading as complete.
    #[tokio::test]
    async fn an_undecodable_row_is_surfaced_even_when_another_workspace_matched() {
        let config = ServerConfig::in_memory();
        let workspace =
            Workspace::from_task(task("o/r", 41, lazybox_core::TaskKind::Issue), Utc::now());
        save(&config, &workspace);
        config
            .store
            .save_workspace(&lazybox_store::WorkspaceRecord {
                key: "github-o-r-corrupt".into(),
                created_at: Utc::now(),
                workspace_json: Some("{ not json".into()),
            })
            .expect("save");

        let report = report(&config, &id("o/r", 41)).await.expect("report");
        assert_eq!(report.workspaces.len(), 1, "the good row still answers");
        assert_eq!(
            report.unreadable_workspaces,
            vec!["github-o-r-corrupt".to_string()],
            "the undecodable row rides the report instead of being dropped"
        );
    }

    /// A blocker whose stored timestamp cannot be read must report an unknown
    /// age, never the current time — that would make a stale blocker look fresh.
    #[tokio::test]
    async fn an_unreadable_blocker_timestamp_reports_no_age_rather_than_now() {
        let config = ServerConfig::in_memory();
        let workspace =
            Workspace::from_task(task("o/r", 42, lazybox_core::TaskKind::Issue), Utc::now());
        save(&config, &workspace);
        crate::epics::persist_declared(
            &config,
            &crate::epics::DeclaredBlocker {
                workspace: workspace.key.clone(),
                reason: "waiting".into(),
                kind: lazybox_ipc::BlockerKind::Decision,
                owner: lazybox_ipc::BlockerOwner::Operator,
                since: i64::MAX,
            },
        )
        .expect("persist");

        let report = report(&config, &id("o/r", 42)).await.expect("report");
        let blocker = report.workspaces[0].blocker.as_ref().expect("blocker");
        assert!(
            blocker.since.is_none(),
            "an unreadable timestamp must not be substituted with now: {:?}",
            blocker.since
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
