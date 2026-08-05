pub use lazybox_server::metrics;
pub use lazybox_server::polling;
pub use lazybox_server::pty;
pub use lazybox_server::spawn_handler;
pub use lazybox_server::{Server, ServerConfig, dispatch_command};

#[allow(dead_code)]
#[path = "../src/api_gateway.rs"]
mod api_gateway;

use api_gateway::{
    AgentTaskKind, AgentsResponse, CommandResponse, DesktopCommand, DesktopEvent, DesktopInfo,
    DesktopTerminalSnapshot, GatewayOptions, HealthResponse, JsonClientFrame, JsonServerFrame,
    ProtocolResponse, UnsupportedProtocolResponse, WorkspacesResponse,
};
use bytes::Bytes;
use chrono::{TimeZone, Utc};
use http_body_util::{BodyExt, Full};
use hyper::header::{AUTHORIZATION, HeaderValue};
use hyper::{Method, Request, StatusCode};
use lazybox_agents::{Agent, SpawnCtx, StructuredAgentProtocol};
use lazybox_core::{CiStatus, ReviewStatus, Task, TaskId, TaskRole, TaskState, Workspace};
use lazybox_ipc::{
    AgentInputMessage, AgentRunRequestId, AgentRuntimeMode, AgentState, Command, Event, TerminalId,
    TerminalInputIntent, TerminalKind, WorktreeStep, WorktreeStepStatus,
};
use lazybox_server::ServerError;
use lazybox_server::agent_stream::{AgentStreamConfig, AgentStreamIo, AgentStreamSpawner};
use lazybox_store::{MemoryStore, WorkspaceRecord};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

struct FakeStreamAgent;

impl Agent for FakeStreamAgent {
    fn id(&self) -> &'static str {
        "fake-api-stream"
    }

    fn display_name(&self) -> &'static str {
        "Fake API Stream"
    }

    fn structured_protocol(&self) -> Option<StructuredAgentProtocol> {
        Some(StructuredAgentProtocol::ClaudeStreamJson)
    }

    fn spawn(&self, _ctx: &SpawnCtx) -> Vec<String> {
        vec!["fake-api-claude".into()]
    }
}

struct FakePtyAgent;

impl Agent for FakePtyAgent {
    fn id(&self) -> &'static str {
        "fake-api-pty"
    }

    fn display_name(&self) -> &'static str {
        "Fake API PTY"
    }

    fn spawn(&self, _ctx: &SpawnCtx) -> Vec<String> {
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "sleep 1; printf '__LB_SIZE__'; stty size; printf '__LB_BEGIN__\\n'; \
             i=0; while [ \"$i\" -lt 800 ]; do \
             printf '__LB_OUTPUT__%04d xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\\n' \"$i\"; \
             i=$((i + 1)); sleep 0.005; done; printf '__LB_END__\\n'; \
             IFS= read -r line; printf '__LB_INPUT__%s\\n' \"$line\"; sleep 30"
                .into(),
        ]
    }
}

/// Mocks the structured-run process at the [`AgentStreamSpawner`]
/// boundary (CONTRIBUTING rule #5): in-memory pipes, no real `claude`
/// or shell. Mirrors the shell script's "read one input line, then
/// emit the canned stream-json and close" sequence.
struct FakeStreamSpawner {
    script: &'static str,
}

impl AgentStreamSpawner for FakeStreamSpawner {
    fn spawn<'a>(
        &'a self,
        _config: AgentStreamConfig,
    ) -> Pin<Box<dyn Future<Output = Result<AgentStreamIo, ServerError>> + Send + 'a>> {
        let script = self.script;
        Box::pin(async move {
            let (driver_stdin, fake_in) = tokio::io::duplex(4096);
            let (mut fake_out, driver_stdout) = tokio::io::duplex(4096);
            tokio::spawn(async move {
                let mut input = BufReader::new(fake_in).lines();
                let _ = input.next_line().await;
                let _ = fake_out.write_all(script.as_bytes()).await;
                // Dropping `fake_out` here closes the driver's stdout (EOF).
            });
            Ok(AgentStreamIo {
                stdin: Box::pin(driver_stdin),
                stdout: Box::pin(driver_stdout),
                wait: Box::pin(async { Some(0) }),
            })
        })
    }
}

const FAKE_API_STREAM_SCRIPT: &str = concat!(
    r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"api-ok"}}}"#,
    "\n",
    r#"{"type":"result","subtype":"success","session_id":"api-session","result":"done"}"#,
    "\n",
);

fn make_task(key: &str) -> Task {
    let (path, num) = key.rsplit_once('#').unwrap_or((key, "1"));
    Task {
        author: String::new(),
        id: TaskId {
            source: "github".into(),
            key: key.into(),
        },
        title: format!("PR {key}"),
        body: None,
        state: TaskState::Open,
        role: TaskRole::Reviewer,
        ci: CiStatus::Success,
        review: ReviewStatus::Pending,
        checks: vec![],
        unread_count: 0,
        url: format!("https://github.com/{path}/pull/{num}"),
        repo: Some("o/r".into()),
        branch: None,
        base_branch: None,
        updated_at: Utc::now(),
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
    }
}

fn desktop_contract_workspace() -> Workspace {
    let timestamp = Utc
        .timestamp_opt(1_700_000_000, 0)
        .single()
        .expect("fixed fixture timestamp");
    let mut task = make_task("o/r#42");
    task.updated_at = timestamp;
    Workspace::from_task(task, timestamp)
}

fn desktop_command_tag(command: &DesktopCommand) -> &'static str {
    match command {
        DesktopCommand::SpawnAgent { .. } => "SpawnAgent",
        DesktopCommand::SpawnShell { .. } => "SpawnShell",
        DesktopCommand::CreateWorkspace { .. } => "CreateWorkspace",
        DesktopCommand::FocusWorkspace { .. } => "FocusWorkspace",
        DesktopCommand::MarkRead { .. } => "MarkRead",
        DesktopCommand::RenameWorkspace { .. } => "RenameWorkspace",
        DesktopCommand::PostReply { .. } => "PostReply",
        DesktopCommand::MergePr { .. } => "MergePr",
        DesktopCommand::UpdateBranch { .. } => "UpdateBranch",
        DesktopCommand::Archive { .. } => "Archive",
        DesktopCommand::CloseIssue { .. } => "CloseIssue",
        DesktopCommand::DeleteOrClose { .. } => "DeleteOrClose",
        DesktopCommand::DeliverSnippet { .. } => "DeliverSnippet",
        DesktopCommand::SetAutoMergeOnGreen { .. } => "SetAutoMergeOnGreen",
        DesktopCommand::SetTrackMain { .. } => "SetTrackMain",
        DesktopCommand::SetAutoFixPolicies { .. } => "SetAutoFixPolicies",
        DesktopCommand::Snooze { .. } => "Snooze",
        DesktopCommand::Unsnooze { .. } => "Unsnooze",
        DesktopCommand::SyncWorkspace { .. } => "SyncWorkspace",
        DesktopCommand::SetNotes { .. } => "SetNotes",
        DesktopCommand::InspectWorkspaceDiff { .. } => "InspectWorkspaceDiff",
        DesktopCommand::Refresh => "Refresh",
    }
}

fn desktop_event_tag(event: &DesktopEvent) -> &'static str {
    match event {
        DesktopEvent::Snapshot { .. } => "Snapshot",
        DesktopEvent::WorkspaceUpserted(_) => "WorkspaceUpserted",
        DesktopEvent::WorkspaceRemoved(_) => "WorkspaceRemoved",
        DesktopEvent::TerminalSpawned { .. } => "TerminalSpawned",
        DesktopEvent::TerminalExited { .. } => "TerminalExited",
        DesktopEvent::TerminalFocusRequested { .. } => "TerminalFocusRequested",
        DesktopEvent::AgentState { .. } => "AgentState",
        DesktopEvent::SnippetDelivered { .. } => "SnippetDelivered",
        DesktopEvent::ProviderError { .. } => "ProviderError",
        DesktopEvent::CommandRejected { .. } => "CommandRejected",
        DesktopEvent::PollCompleted { .. } => "PollCompleted",
        DesktopEvent::PollProgress { .. } => "PollProgress",
        DesktopEvent::WorktreeProgress { .. } => "WorktreeProgress",
        DesktopEvent::WorkspaceActionOutcome { .. } => "WorkspaceActionOutcome",
        DesktopEvent::WorkspaceDiffInspected { .. } => "WorkspaceDiffInspected",
    }
}

#[test]
fn desktop_info_projects_the_daemons_own_agents_and_repositories() {
    // An unconfigured daemon still offers a spawnable set: the built-in
    // trio with the real `cursor-agent` registry id (not `cursor`), and
    // `claude` as the default.
    let mut config = lazybox_config::Config::default();
    let info: DesktopInfo = api_gateway::build_desktop_info(&config);
    assert!(info.agents.contains(&"claude".to_string()));
    assert!(info.agents.contains(&"codex".to_string()));
    assert!(info.agents.contains(&"cursor-agent".to_string()));
    assert_eq!(info.default_agent, "claude");
    assert!(info.repositories.is_empty());

    // A configured daemon reports its enabled agents, its default, and its
    // concrete `owner/repo` scopes — a whole-org scope is not a spawn
    // target and is excluded.
    config.setup.agents = ["codex".to_string()].into_iter().collect();
    config.setup.default_agent = Some("codex".to_string());
    config.setup.scopes.insert(
        "github".into(),
        ["github:acme/widget".into(), "github:whole-org".into()]
            .into_iter()
            .collect(),
    );
    let info = api_gateway::build_desktop_info(&config);
    assert_eq!(info.default_agent, "codex");
    assert!(info.agents.contains(&"codex".to_string()));
    assert_eq!(info.repositories.len(), 1);
    assert_eq!(info.repositories[0].label, "acme/widget");
    assert_eq!(
        info.repositories[0].project_key,
        lazybox_core::ProjectKey::github("acme", "widget")
    );
}

#[test]
fn desktop_compatibility_fixture_is_current() {
    let session_key = lazybox_core::SessionKey::from("github:o/r#42");
    let workspace = desktop_contract_workspace();
    let commands = vec![
        DesktopCommand::SpawnAgent {
            session_key: session_key.clone(),
            agent: "codex".into(),
            model_alias: Some("L".into()),
            on_main: true,
        },
        DesktopCommand::SpawnShell {
            session_key: session_key.clone(),
            on_main: false,
        },
        DesktopCommand::CreateWorkspace {
            name: "first workspace".into(),
            project_key: lazybox_core::ProjectKey::github("o", "r"),
            agent: Some("codex".into()),
        },
        DesktopCommand::FocusWorkspace {
            session_key: session_key.clone(),
        },
        DesktopCommand::MarkRead {
            session_key: session_key.clone(),
        },
        DesktopCommand::RenameWorkspace {
            session_key: session_key.clone(),
            name: "renamed".into(),
        },
        DesktopCommand::PostReply {
            session_key: session_key.clone(),
            body: "Ready for another look.".into(),
        },
        DesktopCommand::MergePr {
            session_key: session_key.clone(),
        },
        DesktopCommand::UpdateBranch {
            session_key: session_key.clone(),
        },
        DesktopCommand::Archive {
            session_key: session_key.clone(),
        },
        DesktopCommand::CloseIssue {
            session_key: session_key.clone(),
        },
        DesktopCommand::DeleteOrClose {
            session_key: session_key.clone(),
        },
        DesktopCommand::DeliverSnippet {
            terminal_id: TerminalId(7),
            snippet_key: "rev".into(),
            category: "Review".into(),
            body: "Review the current diff.".into(),
        },
        DesktopCommand::SetAutoMergeOnGreen {
            session_key: session_key.clone(),
            enabled: true,
        },
        DesktopCommand::SetTrackMain {
            session_key: session_key.clone(),
            enabled: false,
        },
        DesktopCommand::SetAutoFixPolicies {
            session_key: session_key.clone(),
            ci: lazybox_core::PolicyArm::Arm,
            conflict: lazybox_core::PolicyArm::Disarm,
        },
        DesktopCommand::Snooze {
            session_key: session_key.clone(),
            until: Utc.with_ymd_and_hms(2026, 8, 5, 9, 0, 0).unwrap(),
        },
        DesktopCommand::Unsnooze {
            session_key: session_key.clone(),
        },
        DesktopCommand::SyncWorkspace {
            session_key: session_key.clone(),
        },
        DesktopCommand::SetNotes {
            session_key: session_key.clone(),
            notes: "Waiting on the flaky integration job.".into(),
        },
        DesktopCommand::InspectWorkspaceDiff {
            session_key: session_key.clone(),
            target: lazybox_ipc::WorkspaceDiffTarget::LinkedCheckout,
        },
        DesktopCommand::Refresh,
    ];
    let events = vec![
        DesktopEvent::Snapshot {
            workspaces: vec![workspace.clone()],
            terminals: vec![DesktopTerminalSnapshot {
                terminal_id: TerminalId(7),
                session_key: session_key.clone(),
                kind: TerminalKind::Agent("codex".into()),
                last_seq: 42,
                agent_state: Some(AgentState::Working),
            }],
            recent_snippets: vec!["rev".into()],
        },
        DesktopEvent::WorkspaceUpserted(Box::new(workspace)),
        DesktopEvent::WorkspaceRemoved(lazybox_core::WorkspaceKey("github:o/r#42".into())),
        DesktopEvent::TerminalSpawned {
            terminal_id: TerminalId(7),
            session_key: session_key.clone(),
            kind: TerminalKind::Shell,
        },
        DesktopEvent::TerminalExited {
            terminal_id: TerminalId(7),
            exit_code: Some(0),
            last_output: Some("done".into()),
        },
        DesktopEvent::TerminalFocusRequested {
            terminal_id: TerminalId(7),
        },
        DesktopEvent::AgentState {
            session_key: session_key.clone(),
            terminal_id: TerminalId(7),
            state: AgentState::InputNeeded,
        },
        DesktopEvent::SnippetDelivered {
            terminal_id: TerminalId(7),
            session_key: session_key.clone(),
            snippet_key: "rev".into(),
        },
        DesktopEvent::ProviderError {
            source: "github".into(),
            message: "unavailable".into(),
        },
        DesktopEvent::CommandRejected {
            command: "Refresh".into(),
            message: "busy".into(),
        },
        DesktopEvent::PollCompleted {
            source: "github".into(),
            count: 3,
        },
        DesktopEvent::PollProgress {
            source: "github".into(),
            message: "page 2".into(),
        },
        DesktopEvent::WorktreeProgress {
            session_key,
            step: WorktreeStep::Fetch,
            status: WorktreeStepStatus::Progress("50%".into()),
        },
        DesktopEvent::WorkspaceActionOutcome {
            workspace_key: lazybox_core::WorkspaceKey("github:o/r#42".into()),
            ok: false,
            message: "Merge of github:o/r#42 failed: not mergeable".into(),
        },
        DesktopEvent::WorkspaceDiffInspected {
            workspace_key: lazybox_core::WorkspaceKey("github:o/r#42".into()),
            diff: Some(lazybox_ipc::WorkspaceDiffDto {
                status: vec![" M src/main.rs".into()],
                stat: vec![" src/main.rs | 2 +-".into()],
                files: vec![lazybox_ipc::DiffFileDto {
                    old_path: None,
                    path: "src/main.rs".into(),
                    headers: vec!["diff --git a/src/main.rs b/src/main.rs".into()],
                    hunks: vec![lazybox_ipc::DiffHunkDto {
                        header: "@@ -1,2 +1,2 @@".into(),
                        old_start: 1,
                        new_start: 1,
                        lines: vec![lazybox_ipc::DiffLineDto {
                            kind: lazybox_ipc::DiffLineKindDto::Addition,
                            text: "+let x = 1;".into(),
                            old_line: None,
                            new_line: Some(1),
                        }],
                    }],
                }],
                truncated: false,
            }),
            error: None,
        },
    ];
    let command_tags = commands
        .iter()
        .map(desktop_command_tag)
        .collect::<std::collections::BTreeSet<_>>();
    let event_tags = events
        .iter()
        .map(desktop_event_tag)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(command_tags.len(), 22);
    assert_eq!(event_tags.len(), 15);
    let fixture = serde_json::json!({
        "protocol_version": api_gateway::DESKTOP_PROTOCOL_VERSION,
        "protocol_fingerprint": api_gateway::DESKTOP_PROTOCOL_FINGERPRINT,
        "commands": commands,
        "events": events,
    });
    let rendered = format!(
        "{}\n",
        serde_json::to_string_pretty(&fixture).expect("serialize compatibility fixture")
    );
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../apps/desktop/src/generated/compatibility.json");

    if std::env::var_os("UPDATE_DESKTOP_CONTRACT").is_some() {
        std::fs::write(&path, rendered).expect("write compatibility fixture");
        return;
    }

    let committed = std::fs::read_to_string(&path).expect(
        "desktop compatibility fixture is missing; run the contract generator and update it",
    );
    assert_eq!(
        committed, rendered,
        "desktop compatibility fixture is stale; rerun with UPDATE_DESKTOP_CONTRACT=1"
    );
}

#[test]
fn desktop_boundary_rejects_internal_commands_and_private_events() {
    let internal_command = serde_json::json!({
        "ListProviderCredentials": {
            "principal_id": "local"
        }
    });
    assert!(serde_json::from_value::<DesktopCommand>(internal_command).is_err());
    assert!(
        api_gateway::desktop_event(Event::ProviderCredentialsListed {
            principal_id: lazybox_ipc::PrincipalId::local(),
            credentials: Vec::new(),
        })
        .is_none()
    );
}

#[test]
fn desktop_boundary_forwards_workspace_mutation_outcomes() {
    let key = lazybox_core::WorkspaceKey("github:o/r#42".into());

    // A GitHub-rejected merge must reach the desktop as a failure notice —
    // not be dropped, which would leave the fired command looking like a
    // no-op (#816).
    let failed = api_gateway::desktop_event(Event::PrMergeFailed {
        workspace_key: key.clone(),
        pr_label: "o/r#42".into(),
        reason: "not mergeable".into(),
    });
    match failed {
        Some(DesktopEvent::WorkspaceActionOutcome {
            workspace_key,
            ok,
            message,
        }) => {
            assert_eq!(workspace_key, key);
            assert!(!ok);
            assert!(message.contains("not mergeable"), "message was {message:?}");
        }
        other => panic!("expected a failure outcome, got {other:?}"),
    }

    // A successful merge reports success.
    match api_gateway::desktop_event(Event::PrMerged {
        workspace_key: key.clone(),
        pr_label: "o/r#42".into(),
    }) {
        Some(DesktopEvent::WorkspaceActionOutcome { ok, .. }) => assert!(ok),
        other => panic!("expected a success outcome, got {other:?}"),
    }

    // The degraded delete (delete refused → closed) reports success but
    // names the degradation so the user knows the issue still exists.
    match api_gateway::desktop_event(Event::IssueDeleted {
        workspace_key: key.clone(),
        issue_label: "o/r#42".into(),
        fell_back_to_close: true,
    }) {
        Some(DesktopEvent::WorkspaceActionOutcome { ok, message, .. }) => {
            assert!(ok);
            assert!(message.contains("not-planned"), "message was {message:?}");
        }
        other => panic!("expected a degraded-delete outcome, got {other:?}"),
    }
}

#[test]
fn desktop_inspect_diff_command_maps_to_ipc() {
    let session_key: lazybox_core::SessionKey = "github:o/r#42".into();
    let command = DesktopCommand::InspectWorkspaceDiff {
        session_key: session_key.clone(),
        target: lazybox_ipc::WorkspaceDiffTarget::LinkedCheckout,
    };
    match command.into_correlated(Some("req-1".into())) {
        lazybox_ipc::Command::InspectWorkspaceDiff {
            workspace_key,
            target,
        } => {
            assert_eq!(workspace_key.as_str(), session_key.as_str());
            assert_eq!(target, lazybox_ipc::WorkspaceDiffTarget::LinkedCheckout);
        }
        other => panic!("expected InspectWorkspaceDiff, got {other:?}"),
    }
}

#[test]
fn desktop_boundary_forwards_workspace_diff() {
    let key = lazybox_core::WorkspaceKey("github:o/r#42".into());
    let dto = lazybox_ipc::WorkspaceDiffDto {
        status: vec![" M src/main.rs".into()],
        stat: vec![" src/main.rs | 1 +".into()],
        files: Vec::new(),
        truncated: false,
    };

    // A successful inspection carries the diff and drops the internal-only
    // `agent_terminal_ids` / `target` the desktop reader doesn't use.
    match api_gateway::desktop_event(Event::WorkspaceDiffInspected {
        workspace_key: key.clone(),
        target: lazybox_ipc::WorkspaceDiffTarget::LinkedCheckout,
        agent_terminal_ids: vec![lazybox_ipc::TerminalId(3)],
        diff: Some(dto.clone()),
        error: None,
    }) {
        Some(DesktopEvent::WorkspaceDiffInspected {
            workspace_key,
            diff,
            error,
        }) => {
            assert_eq!(workspace_key, key);
            assert_eq!(diff, Some(dto));
            assert!(error.is_none());
        }
        other => panic!("expected a diff event, got {other:?}"),
    }

    // A read failure forwards the error with no diff.
    match api_gateway::desktop_event(Event::WorkspaceDiffInspected {
        workspace_key: key.clone(),
        target: lazybox_ipc::WorkspaceDiffTarget::LinkedCheckout,
        agent_terminal_ids: Vec::new(),
        diff: None,
        error: Some("not a git repository".into()),
    }) {
        Some(DesktopEvent::WorkspaceDiffInspected { diff, error, .. }) => {
            assert!(diff.is_none());
            assert_eq!(error.as_deref(), Some("not a git repository"));
        }
        other => panic!("expected a diff error event, got {other:?}"),
    }
}

async fn read_json<T: serde::de::DeserializeOwned>(
    response: hyper::Response<api_gateway::Body>,
) -> T {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[test]
fn bearer_token_helper_accepts_matching_token() {
    let header = HeaderValue::from_static("Bearer secret");
    assert!(api_gateway::check_bearer_token(
        Some(&header),
        Some("secret")
    ));
}

#[test]
fn bearer_token_helper_rejects_missing_or_wrong_token() {
    let header = HeaderValue::from_static("Bearer wrong");
    assert!(!api_gateway::check_bearer_token(None, Some("secret")));
    assert!(!api_gateway::check_bearer_token(
        Some(&header),
        Some("secret")
    ));
    assert!(!api_gateway::check_bearer_token(
        Some(&HeaderValue::from_static("secret")),
        Some("secret")
    ));
}

#[test]
fn bearer_token_helper_allows_requests_when_token_is_not_configured() {
    assert!(api_gateway::check_bearer_token(None, None));
}

#[tokio::test]
async fn local_browser_shell_is_public_while_api_routes_stay_authenticated() {
    let request = Request::builder()
        .method(Method::GET)
        .uri("/")
        .body(Full::new(Bytes::new()))
        .unwrap();
    let options = GatewayOptions {
        bearer_token: Some("secret".into()),
        ..GatewayOptions::default()
    };

    let response =
        api_gateway::handle_request(ServerConfig::in_memory(), options.clone(), request).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["content-type"],
        "text/html; charset=utf-8"
    );
    assert_eq!(response.headers()["cache-control"], "no-store, max-age=0");
    assert!(
        response.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("connect-src 'self'")
    );
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let html = std::str::from_utf8(&body).unwrap();
    assert!(html.contains("lazybox browser"));
    assert!(html.contains("/v1/workspaces"));
    assert!(html.contains("Authorization: `Bearer ${token}`"));

    let request = Request::builder()
        .method(Method::GET)
        .uri("/v1/health")
        .body(Full::new(Bytes::new()))
        .unwrap();
    let response = api_gateway::handle_request(ServerConfig::in_memory(), options, request).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn health_route_returns_json() {
    let request = Request::builder()
        .method(Method::GET)
        .uri("/v1/health")
        .body(Full::new(Bytes::new()))
        .unwrap();

    let response = api_gateway::handle_request(
        ServerConfig::in_memory(),
        GatewayOptions::default(),
        request,
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let payload: HealthResponse = read_json(response).await;
    assert_eq!(payload.service, "lazybox-api-gateway");
    assert!(payload.ok);
}

#[tokio::test]
async fn reusable_gateway_rejects_every_non_loopback_listener() {
    let options = GatewayOptions {
        bind_addr: "0.0.0.0:0".parse().expect("wildcard address"),
        ..GatewayOptions::default()
    };
    let error = api_gateway::serve(ServerConfig::in_memory(), options)
        .await
        .expect_err("public gateway entry point must reject wildcard binds");
    assert!(matches!(error, api_gateway::GatewayError::NonLoopback(_)));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:0")
        .await
        .expect("bind wildcard listener");
    let error = api_gateway::serve_listener(
        ServerConfig::in_memory(),
        GatewayOptions::default(),
        listener,
    )
    .await
    .expect_err("pre-bound wildcard listener must also be rejected");
    assert!(matches!(error, api_gateway::GatewayError::NonLoopback(_)));
}
#[tokio::test]
async fn metrics_route_returns_event_counters() {
    let config = ServerConfig::in_memory();
    // Drive the counters off zero so the route reflects live state.
    config.event_metrics.record_output_dropped();
    config.event_metrics.record_resync();
    config.event_metrics.record_bus_lagged(7);
    config.event_metrics.record_hot_sync_latency(2_000);
    config.event_metrics.record_cold_sync_latency(120_000);

    let request = Request::builder()
        .method(Method::GET)
        .uri("/v1/metrics")
        .body(Full::new(Bytes::new()))
        .unwrap();

    let response = api_gateway::handle_request(config, GatewayOptions::default(), request).await;

    assert_eq!(response.status(), StatusCode::OK);
    let payload: metrics::EventMetricsSnapshot = read_json(response).await;
    assert_eq!(payload.terminal_output_dropped, 1);
    assert_eq!(payload.terminal_resyncs, 1);
    assert_eq!(payload.bus_lagged_events, 7);
    assert_eq!(payload.bus_lag_recoveries, 0);
    assert_eq!(payload.hot_sync_samples, 1);
    assert_eq!(payload.hot_sync_p50_ms, Some(2_000));
    assert_eq!(payload.hot_sync_p95_ms, Some(2_000));
    assert_eq!(payload.cold_sync_samples, 1);
    assert_eq!(payload.cold_sync_p50_ms, Some(120_000));
    assert_eq!(payload.cold_sync_p95_ms, Some(120_000));
}

#[tokio::test]
async fn protocol_route_discovers_the_versioned_binary_boundary() {
    let request = Request::builder()
        .method(Method::GET)
        .uri("/v1/protocol")
        .body(Full::new(Bytes::new()))
        .unwrap();

    let response = api_gateway::handle_request(
        ServerConfig::in_memory(),
        GatewayOptions::default(),
        request,
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let payload: ProtocolResponse = read_json(response).await;
    assert_eq!(
        payload.protocol_version,
        api_gateway::DESKTOP_PROTOCOL_VERSION
    );
    assert_eq!(
        payload.protocol_fingerprint,
        api_gateway::DESKTOP_PROTOCOL_FINGERPRINT
    );
    assert_eq!(
        payload.terminal_transport,
        api_gateway::TERMINAL_BINARY_CONTENT_TYPE
    );
    assert_eq!(
        payload.max_terminal_frame_bytes,
        api_gateway::MAX_TERMINAL_BINARY_FRAME_BYTES
    );
    assert_eq!(
        payload.max_terminal_write_bytes,
        lazybox_ipc::MAX_WRITE_CHUNK_BYTES
    );
}

#[tokio::test]
async fn unsupported_protocol_header_is_rejected_clearly() {
    let request = Request::builder()
        .method(Method::GET)
        .uri("/v1/workspaces")
        .header(api_gateway::PROTOCOL_VERSION_HEADER, "999")
        .body(Full::new(Bytes::new()))
        .unwrap();

    let response = api_gateway::handle_request(
        ServerConfig::in_memory(),
        GatewayOptions::default(),
        request,
    )
    .await;

    assert_eq!(response.status(), StatusCode::UPGRADE_REQUIRED);
    let payload: UnsupportedProtocolResponse = read_json(response).await;
    assert_eq!(payload.requested, "999");
    assert_eq!(payload.supported, api_gateway::DESKTOP_PROTOCOL_VERSION);
    assert!(
        payload
            .error
            .contains("unsupported lazybox protocol version")
    );
}

#[tokio::test]
async fn matching_protocol_version_serves_regardless_of_build_fingerprint() {
    // The fingerprint over-approximates the wire contract (a Cargo.lock
    // bump flips it), so it is advisory only (#815): a request carrying a
    // compatible protocol version is served even when the two builds differ,
    // instead of the old per-request 426.
    let request = Request::builder()
        .method(Method::GET)
        .uri("/v1/protocol")
        .header(
            api_gateway::PROTOCOL_VERSION_HEADER,
            api_gateway::DESKTOP_PROTOCOL_VERSION,
        )
        .body(Full::new(Bytes::new()))
        .unwrap();

    let response = api_gateway::handle_request(
        ServerConfig::in_memory(),
        GatewayOptions::default(),
        request,
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let payload: ProtocolResponse = read_json(response).await;
    assert_eq!(
        payload.protocol_fingerprint,
        api_gateway::DESKTOP_PROTOCOL_FINGERPRINT
    );
}

#[test]
fn terminal_binary_frames_preserve_raw_bytes_and_sequence_metadata() {
    let event = Event::TerminalOutput {
        terminal_id: lazybox_ipc::TerminalId(7),
        bytes: vec![0, 1, 2, 0xff],
        first_seq: 11,
        seq: 13,
    };

    let frames = api_gateway::encode_terminal_event(&event);

    assert_eq!(frames.len(), 1);
    let frame = &frames[0];
    assert_eq!(
        u32::from_be_bytes(frame[..4].try_into().unwrap()) as usize,
        frame.len() - 4
    );
    assert_eq!(frame[4], 2);
    assert_eq!(u64::from_be_bytes(frame[5..13].try_into().unwrap()), 7);
    assert_eq!(u64::from_be_bytes(frame[13..21].try_into().unwrap()), 11);
    assert_eq!(u64::from_be_bytes(frame[21..29].try_into().unwrap()), 13);
    assert_eq!(&frame[29..], &[0, 1, 2, 0xff]);
}

#[test]
fn binary_snapshot_omits_non_authoritative_replay() {
    let event = Event::Snapshot {
        workspaces: Vec::new(),
        terminals: vec![lazybox_ipc::TerminalSnapshot {
            terminal_id: TerminalId(7),
            session_key: "desktop:test".into(),
            kind: TerminalKind::Shell,
            replay: Vec::new(),
            last_seq: 4,
            replay_available: false,
            no_permission: false,
            on_main: false,
            model_label: None,
            prompt_history: Vec::new(),
            composing_buffer: None,
            agent_state: None,
            authenticating: false,
        }],
        projects: Vec::new(),
        recent_snippets: Vec::new(),
        dismissed_updates: Vec::new(),
    };

    assert!(api_gateway::encode_terminal_event(&event).is_empty());
}

#[test]
fn binary_stream_reports_an_unavailable_resync() {
    let frames = api_gateway::encode_terminal_event(&Event::TerminalResyncUnavailable {
        terminal_id: TerminalId(7),
    });

    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0][4], 5);
    assert_eq!(u64::from_be_bytes(frames[0][5..13].try_into().unwrap()), 7);
    assert_eq!(frames[0].len(), 29);
}

#[test]
fn every_terminal_command_round_trips_the_binary_codec() {
    let commands = [
        Command::Write {
            terminal_id: lazybox_ipc::TerminalId(2),
            bytes: vec![0, 27, 0xff],
            intent: TerminalInputIntent::Submit,
        },
        Command::Resize {
            terminal_id: lazybox_ipc::TerminalId(2),
            cols: 120,
            rows: 40,
        },
        Command::RequestTerminalResync {
            terminal_id: lazybox_ipc::TerminalId(2),
            required_seq: 44,
        },
        Command::Close {
            terminal_id: lazybox_ipc::TerminalId(2),
            client_request_id: None,
        },
        Command::FetchScrollback {
            terminal_id: lazybox_ipc::TerminalId(2),
        },
    ];

    for command in commands {
        let frame = api_gateway::encode_terminal_command(&command).expect("terminal command");
        let body_len = u32::from_be_bytes(frame[..4].try_into().unwrap()) as usize;
        let decoded = api_gateway::decode_terminal_command(&frame[4..4 + body_len])
            .expect("decode terminal command");
        assert_eq!(format!("{decoded:?}"), format!("{command:?}"));
    }
}

#[test]
fn binary_codec_rejects_a_correlated_close_it_cannot_preserve() {
    assert!(
        api_gateway::encode_terminal_command(&Command::Close {
            terminal_id: TerminalId(2),
            client_request_id: Some("close-1".into()),
        })
        .is_none()
    );
}

#[tokio::test]
async fn terminal_command_body_forwards_every_complete_binary_frame() {
    let expected = [
        Command::Resize {
            terminal_id: TerminalId(2),
            cols: 120,
            rows: 40,
        },
        Command::Write {
            terminal_id: TerminalId(2),
            bytes: b"hello\n".to_vec(),
            intent: TerminalInputIntent::Submit,
        },
    ];
    let bytes = expected
        .iter()
        .flat_map(|command| api_gateway::encode_terminal_command(command).unwrap())
        .collect::<Vec<_>>();
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);

    api_gateway::pump_terminal_commands(Full::new(Bytes::from(bytes)), tx).await;

    for expected in expected {
        let actual = rx.recv().await.expect("decoded terminal command");
        assert_eq!(format!("{actual:?}"), format!("{expected:?}"));
    }
}

#[tokio::test]
async fn terminal_command_body_accepts_coalesced_frames_over_the_buffer_limit() {
    let expected = [
        Command::Write {
            terminal_id: TerminalId(2),
            bytes: vec![b'a'; lazybox_ipc::MAX_WRITE_CHUNK_BYTES],
            intent: TerminalInputIntent::Compose,
        },
        Command::Write {
            terminal_id: TerminalId(2),
            bytes: vec![b'b'; lazybox_ipc::MAX_WRITE_CHUNK_BYTES],
            intent: TerminalInputIntent::Compose,
        },
    ];
    let bytes = expected
        .iter()
        .flat_map(|command| api_gateway::encode_terminal_command(command).unwrap())
        .collect::<Vec<_>>();
    assert!(bytes.len() > lazybox_ipc::MAX_COMMAND_FRAME_BYTES as usize);
    let (tx, mut rx) = tokio::sync::mpsc::channel(2);

    api_gateway::pump_terminal_commands(Full::new(Bytes::from(bytes)), tx).await;

    for expected in expected {
        let actual = rx.recv().await.expect("decoded terminal command");
        assert_eq!(format!("{actual:?}"), format!("{expected:?}"));
    }
}

#[test]
fn json_control_stream_never_serializes_terminal_byte_payloads() {
    assert!(
        api_gateway::control_event(Event::TerminalOutput {
            terminal_id: lazybox_ipc::TerminalId(1),
            bytes: vec![1, 2, 3],
            first_seq: 1,
            seq: 1,
        })
        .is_none()
    );
    assert!(
        api_gateway::control_event(Event::TerminalResync {
            terminal_id: lazybox_ipc::TerminalId(1),
            replay: vec![1, 2, 3],
            seq: 1,
        })
        .is_none()
    );
    let mut snapshot = api_gateway::control_event(Event::Snapshot {
        workspaces: Vec::new(),
        terminals: vec![lazybox_ipc::TerminalSnapshot {
            terminal_id: TerminalId(1),
            session_key: "desktop:test".into(),
            kind: TerminalKind::Shell,
            replay: vec![1, 2, 3],
            last_seq: 1,
            replay_available: true,
            no_permission: false,
            on_main: false,
            model_label: None,
            prompt_history: Vec::new(),
            composing_buffer: None,
            agent_state: None,
            authenticating: false,
        }],
        projects: Vec::new(),
        recent_snippets: Vec::new(),
        dismissed_updates: Vec::new(),
    })
    .expect("snapshot stays on the control stream");
    let Event::Snapshot { terminals, .. } = &mut snapshot else {
        panic!("expected snapshot");
    };
    assert!(terminals[0].replay.is_empty());
    assert!(!terminals[0].replay_available);
}

struct DecodedTerminalFrame {
    kind: u8,
    terminal_id: TerminalId,
    first_seq: u64,
    seq: u64,
    payload: Bytes,
}

async fn next_terminal_frame(body: &mut api_gateway::Body) -> DecodedTerminalFrame {
    let frame = tokio::time::timeout(std::time::Duration::from_secs(10), body.frame())
        .await
        .expect("terminal stream yields within the deadline")
        .expect("terminal stream remains open")
        .expect("terminal stream frame is valid")
        .into_data()
        .expect("terminal stream emits data frames");
    let body_len = u32::from_be_bytes(frame[..4].try_into().expect("frame length")) as usize;
    assert_eq!(frame.len(), body_len + 4);
    DecodedTerminalFrame {
        kind: frame[4],
        terminal_id: TerminalId(u64::from_be_bytes(
            frame[5..13].try_into().expect("terminal id"),
        )),
        first_seq: u64::from_be_bytes(frame[13..21].try_into().expect("first sequence")),
        seq: u64::from_be_bytes(frame[21..29].try_into().expect("sequence")),
        payload: frame.slice(29..),
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[tokio::test]
async fn desktop_runtime_real_pty_handles_backpressure_reconnect_replay_and_resync() {
    let mut config = ServerConfig::with_store(Arc::new(MemoryStore::new()));
    config.agents.register(Arc::new(FakePtyAgent));
    let client_runtime = lazybox_server::client_runtime::ClientRuntime::start(
        config.clone(),
        lazybox_server::client_runtime::ClientRuntimeOptions {
            poll_interval: std::time::Duration::from_secs(60),
            restore_persisted_sessions: false,
            slack: None,
        },
    )
    .await;
    let temp = tempfile::tempdir().expect("temporary working directory");
    let spawn = JsonClientFrame::Command(Command::Spawn {
        session_key: "desktop:real-pty".into(),
        session_id: None,
        client_request_id: None,
        kind: TerminalKind::Agent("fake-api-pty".into()),
        cwd: Some(temp.path().to_string_lossy().into_owned()),
        initial_prompt: None,
        on_main: false,
        model_alias: None,
        access: lazybox_ipc::AgentRunAccess::Default,
    });
    let request = Request::builder()
        .method(Method::POST)
        .uri("/v1/commands")
        .body(Full::new(Bytes::from(
            serde_json::to_vec(&spawn).expect("spawn command JSON"),
        )))
        .unwrap();
    let response =
        api_gateway::handle_request(config.clone(), GatewayOptions::default(), request).await;
    assert_eq!(response.status(), StatusCode::OK);

    let terminal_id = config
        .terminal
        .terminal_ids()
        .await
        .into_iter()
        .next()
        .expect("spawn registered a real PTY");
    let backend_key = config
        .terminal
        .backend_key_for(terminal_id)
        .await
        .expect("spawned PTY has a backend session");
    let request = Request::builder()
        .method(Method::POST)
        .uri("/v1/terminal")
        .body(Full::new(Bytes::new()))
        .unwrap();
    let response =
        api_gateway::handle_request(config.clone(), GatewayOptions::default(), request).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[hyper::header::CONTENT_TYPE],
        api_gateway::TERMINAL_BINARY_CONTENT_TYPE
    );
    let mut terminal_body = response.into_body();
    let initial = next_terminal_frame(&mut terminal_body).await;
    assert_eq!(initial.kind, 1);
    assert_eq!(initial.terminal_id, terminal_id);

    let mut commands = api_gateway::encode_terminal_command(&Command::Resize {
        terminal_id,
        cols: 93,
        rows: 37,
    })
    .expect("resize frame");
    commands.extend(
        api_gateway::encode_terminal_command(&Command::Write {
            terminal_id,
            bytes: b"desktop-input\n".to_vec(),
            intent: TerminalInputIntent::Submit,
        })
        .expect("write frame"),
    );
    let request = Request::builder()
        .method(Method::POST)
        .uri("/v1/terminal")
        .body(Full::new(Bytes::from(commands)))
        .unwrap();
    let response =
        api_gateway::handle_request(config.clone(), GatewayOptions::default(), request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let _command_stream = response.into_body();

    let output_snapshot = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let snapshot = config
                .backend
                .snapshot(&backend_key)
                .await
                .expect("snapshot real PTY");
            if snapshot.last_seq > 512
                && contains_bytes(&snapshot.replay, b"__LB_END__")
                && contains_bytes(&snapshot.replay, b"__LB_INPUT__desktop-input")
            {
                break snapshot;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("sustained PTY command completes");
    assert!(output_snapshot.last_seq > lazybox_ipc::EVENT_CHANNEL_CAPACITY as u64);

    let mut saw_size = false;
    let mut saw_begin = false;
    let mut saw_end = false;
    let mut saw_input = false;
    let mut saw_recovery = false;
    let mut last_output_seq = 0;
    let mut observed_tail = Vec::new();
    for _ in 0..700 {
        let frame = next_terminal_frame(&mut terminal_body).await;
        assert_eq!(frame.terminal_id, terminal_id);
        if frame.kind == 2 {
            assert!(frame.first_seq > last_output_seq);
            assert!(frame.first_seq <= frame.seq);
            last_output_seq = frame.seq;
        }
        observed_tail.extend_from_slice(&frame.payload);
        saw_size |= contains_bytes(&observed_tail, b"__LB_SIZE__37 93");
        saw_begin |= contains_bytes(&observed_tail, b"__LB_BEGIN__");
        saw_end |= contains_bytes(&observed_tail, b"__LB_END__");
        saw_input |= contains_bytes(&observed_tail, b"__LB_INPUT__desktop-input");
        if observed_tail.len() > 128 {
            observed_tail.drain(..observed_tail.len() - 128);
        }
        saw_recovery |= frame.kind == 1 || frame.kind == 3;
        if saw_size && saw_begin && saw_end && saw_input && saw_recovery {
            break;
        }
    }
    assert!(saw_size, "binary resize reached the real PTY");
    assert!(saw_begin, "the real PTY began sustained output");
    assert!(saw_end, "sustained PTY output completed");
    assert!(saw_input, "binary input reached the real PTY");
    assert!(
        saw_recovery,
        "slow-consumer output recovered with an authoritative replay"
    );

    drop(terminal_body);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let request = Request::builder()
        .method(Method::POST)
        .uri("/v1/terminal")
        .body(Full::new(Bytes::new()))
        .unwrap();
    let response =
        api_gateway::handle_request(config.clone(), GatewayOptions::default(), request).await;
    let mut reconnect_stream = response.into_body();
    let snapshot = next_terminal_frame(&mut reconnect_stream).await;
    assert_eq!(snapshot.kind, 1);
    assert_eq!(snapshot.terminal_id, terminal_id);
    assert!(contains_bytes(&snapshot.payload, b"__LB_END__"));

    let request = Request::builder()
        .method(Method::POST)
        .uri("/v1/terminal")
        .body(Full::new(Bytes::from(
            api_gateway::encode_terminal_command(&Command::RequestTerminalResync {
                terminal_id,
                required_seq: snapshot.seq,
            })
            .expect("resync frame"),
        )))
        .unwrap();
    let response =
        api_gateway::handle_request(config.clone(), GatewayOptions::default(), request).await;
    let mut resync_stream = response.into_body();
    let first = next_terminal_frame(&mut resync_stream).await;
    let resync = if first.kind == 3 {
        first
    } else {
        assert_eq!(first.kind, 1);
        next_terminal_frame(&mut resync_stream).await
    };
    assert_eq!(resync.kind, 3);
    assert_eq!(resync.terminal_id, terminal_id);
    assert!(resync.seq >= snapshot.seq);
    assert!(contains_bytes(&resync.payload, b"__LB_END__"));

    config
        .backend
        .kill(&backend_key)
        .await
        .expect("close real PTY");
    client_runtime.shutdown().await;
}

#[tokio::test]
async fn health_route_enforces_bearer_token_when_configured() {
    let request = Request::builder()
        .method(Method::GET)
        .uri("/v1/health")
        .body(Full::new(Bytes::new()))
        .unwrap();
    let options = GatewayOptions {
        bearer_token: Some("secret".into()),
        ..GatewayOptions::default()
    };

    let response = api_gateway::handle_request(ServerConfig::in_memory(), options, request).await;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
#[tokio::test]
async fn workspaces_route_returns_current_store_snapshot() {
    let config = ServerConfig::in_memory();
    let workspace = Workspace::from_task(make_task("o/r#42"), Utc::now());
    config
        .store
        .save_workspace(&WorkspaceRecord {
            key: workspace.key.as_str().to_string(),
            created_at: workspace.created_at,
            workspace_json: Some(serde_json::to_string(&workspace).unwrap()),
        })
        .unwrap();
    let request = Request::builder()
        .method(Method::GET)
        .uri("/v1/workspaces")
        .header(AUTHORIZATION, "Bearer secret")
        .body(Full::new(Bytes::new()))
        .unwrap();
    let options = GatewayOptions {
        bearer_token: Some("secret".into()),
        ..GatewayOptions::default()
    };

    let response = api_gateway::handle_request(config, options, request).await;

    assert_eq!(response.status(), StatusCode::OK);
    let payload: WorkspacesResponse = read_json(response).await;
    assert_eq!(payload.workspaces.len(), 1);
    assert!(payload.warnings.is_empty());
    assert_eq!(payload.workspaces[0].pr.as_ref().unwrap().id.key, "o/r#42");
}

#[tokio::test]
async fn workspaces_route_reports_and_preserves_unreadable_records() {
    let config = ServerConfig::in_memory();
    config
        .store
        .save_workspace(&WorkspaceRecord {
            key: "broken-workspace".into(),
            created_at: Utc::now(),
            workspace_json: Some("not valid workspace JSON".into()),
        })
        .unwrap();
    let observed = config.clone();
    let request = Request::builder()
        .method(Method::GET)
        .uri("/v1/workspaces")
        .body(Full::new(Bytes::new()))
        .unwrap();

    let response = api_gateway::handle_request(config, GatewayOptions::default(), request).await;

    assert_eq!(response.status(), StatusCode::OK);
    let payload: WorkspacesResponse = read_json(response).await;
    assert!(payload.workspaces.is_empty());
    assert_eq!(payload.warnings.len(), 1);
    assert!(payload.warnings[0].contains("broken-workspace"));
    assert_eq!(
        observed.store.list_workspaces().unwrap().len(),
        1,
        "recovery diagnostics must not delete the unreadable record"
    );
}
#[tokio::test]
async fn agents_route_projects_running_agents_and_omits_shells() {
    let config = ServerConfig::in_memory();
    let now = Utc::now();

    let mut workspace = Workspace::from_task(make_task("o/r#42"), now);
    let session = lazybox_core::WorkspaceSession::new(
        workspace.key.clone(),
        lazybox_core::SessionKind::Agent {
            agent_id: "claude".into(),
        },
        std::path::PathBuf::from("/tmp/agent-worktree"),
        now,
    );
    let session_id = session.id;
    workspace.sessions.push(session);
    let workspace_key = workspace.key.as_str().to_string();
    config
        .store
        .save_workspace(&WorkspaceRecord {
            key: workspace_key.clone(),
            created_at: workspace.created_at,
            workspace_json: Some(serde_json::to_string(&workspace).unwrap()),
        })
        .unwrap();

    // One live agent terminal joined to that workspace, plus a shell in
    // the same workspace that must not surface as an agent.
    let agent_terminal = TerminalId(7);
    config
        .terminal
        .register_terminal(
            agent_terminal,
            "backend:agent".into(),
            workspace_key.as_str().into(),
            TerminalKind::Agent("claude".into()),
        )
        .await;
    config
        .terminal
        .associate_session(agent_terminal, session_id)
        .await;
    config
        .terminal
        .record_agent_state(agent_terminal, AgentState::Working)
        .await;
    config
        .terminal
        .register_terminal(
            TerminalId(8),
            "backend:shell".into(),
            workspace_key.as_str().into(),
            TerminalKind::Shell,
        )
        .await;

    let request = Request::builder()
        .method(Method::GET)
        .uri("/v1/agents")
        .body(Full::new(Bytes::new()))
        .unwrap();
    let response = api_gateway::handle_request(config, GatewayOptions::default(), request).await;

    assert_eq!(response.status(), StatusCode::OK);
    let payload: AgentsResponse = read_json(response).await;
    assert_eq!(payload.agents.len(), 1, "shells are not agents");
    let agent = &payload.agents[0];
    assert_eq!(agent.agent, "claude");
    assert_eq!(agent.workspace_key, workspace_key);
    assert_eq!(agent.workspace_name, "PR o/r#42");
    assert_eq!(agent.state, Some(AgentState::Working));
    assert!(
        agent.session_started_at.is_some(),
        "session_started_at joins the workspace session"
    );
    assert!(agent.last_prompt.is_none());
    let task = agent.task.as_ref().expect("agent carries its task");
    assert_eq!(task.id, "github:o/r#42");
    assert_eq!(task.number, Some(42));
    assert!(matches!(task.kind, AgentTaskKind::Pr));
    assert_eq!(task.repo.as_deref(), Some("o/r"));
    assert_eq!(agent.repo.as_deref(), Some("o/r"));
}

#[tokio::test]
async fn agents_route_reports_repo_for_a_task_less_workspace() {
    let config = ServerConfig::in_memory();
    let now = Utc::now();

    // A hand-created workspace: no PR/issue, but it knows its repo via
    // the project key.
    let mut workspace = Workspace::empty(lazybox_core::WorkspaceKey::new("scratch"), "main", now);
    workspace.name = "Scratch".into();
    workspace.project_key = Some(lazybox_core::ProjectKey::github("owner", "repo"));
    let workspace_key = workspace.key.as_str().to_string();
    config
        .store
        .save_workspace(&WorkspaceRecord {
            key: workspace_key.clone(),
            created_at: workspace.created_at,
            workspace_json: Some(serde_json::to_string(&workspace).unwrap()),
        })
        .unwrap();
    config
        .terminal
        .register_terminal(
            TerminalId(3),
            "backend:scratch".into(),
            workspace_key.as_str().into(),
            TerminalKind::Agent("codex".into()),
        )
        .await;

    let request = Request::builder()
        .method(Method::GET)
        .uri("/v1/agents")
        .body(Full::new(Bytes::new()))
        .unwrap();
    let response = api_gateway::handle_request(config, GatewayOptions::default(), request).await;

    assert_eq!(response.status(), StatusCode::OK);
    let payload: AgentsResponse = read_json(response).await;
    assert_eq!(payload.agents.len(), 1);
    let agent = &payload.agents[0];
    assert!(agent.task.is_none(), "no PR/issue attached");
    assert_eq!(
        agent.repo.as_deref(),
        Some("owner/repo"),
        "repo falls back to the project key"
    );
}

#[tokio::test]
async fn agents_route_reports_an_empty_fleet() {
    let request = Request::builder()
        .method(Method::GET)
        .uri("/v1/agents")
        .body(Full::new(Bytes::new()))
        .unwrap();
    let response = api_gateway::handle_request(
        ServerConfig::in_memory(),
        GatewayOptions::default(),
        request,
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let payload: AgentsResponse = read_json(response).await;
    assert!(payload.agents.is_empty());
    assert!(payload.warnings.is_empty());
}

#[tokio::test]
async fn command_route_accepts_json_client_frame() {
    let frame = JsonClientFrame::Command(Command::Refresh);
    let request = Request::builder()
        .method(Method::POST)
        .uri("/v1/commands")
        .body(Full::new(Bytes::from(serde_json::to_vec(&frame).unwrap())))
        .unwrap();

    let response = api_gateway::handle_request(
        ServerConfig::in_memory(),
        GatewayOptions::default(),
        request,
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let payload: CommandResponse = read_json(response).await;
    assert!(payload.ok);
    assert!(payload.completed);
    assert!(payload.events.is_empty());
}

#[tokio::test]
async fn command_route_returns_connection_scoped_handler_events() {
    let frame = JsonClientFrame::Command(Command::ListProviderCredentials {
        principal_id: lazybox_ipc::PrincipalId::local(),
    });
    let request = Request::builder()
        .method(Method::POST)
        .uri("/v1/commands")
        .body(Full::new(Bytes::from(serde_json::to_vec(&frame).unwrap())))
        .unwrap();

    let response = api_gateway::handle_request(
        ServerConfig::in_memory(),
        GatewayOptions::default(),
        request,
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let payload: CommandResponse = read_json(response).await;
    assert!(matches!(
        payload.events.as_slice(),
        [Event::ProviderCredentialsListed { credentials, .. }] if credentials.is_empty()
    ));
}

#[tokio::test]
async fn command_route_returns_the_correlated_terminal_failure() {
    let frame = JsonClientFrame::Command(Command::Spawn {
        session_key: "desktop:missing-agent".into(),
        session_id: None,
        client_request_id: Some("desktop-request".into()),
        kind: TerminalKind::Agent("not-installed".into()),
        cwd: Some(std::env::temp_dir().to_string_lossy().into_owned()),
        initial_prompt: None,
        on_main: false,
        model_alias: None,
        access: lazybox_ipc::AgentRunAccess::Default,
    });
    let request = Request::builder()
        .method(Method::POST)
        .uri("/v1/commands")
        .body(Full::new(Bytes::from(serde_json::to_vec(&frame).unwrap())))
        .unwrap();

    let response = api_gateway::handle_request(
        ServerConfig::in_memory(),
        GatewayOptions::default(),
        request,
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let payload: CommandResponse = read_json(response).await;
    assert!(!payload.ok);
    assert!(payload.completed);
    assert_eq!(payload.error.as_deref(), Some("terminal was not spawned"));
}

#[tokio::test]
async fn command_route_returns_reply_failure_to_the_requesting_client() {
    let frame = JsonClientFrame::Command(Command::PostReply {
        session_key: "github:missing/repo#1".into(),
        body: "Ready for review".into(),
    });
    let request = Request::builder()
        .method(Method::POST)
        .uri("/v1/commands")
        .body(Full::new(Bytes::from(serde_json::to_vec(&frame).unwrap())))
        .unwrap();

    let response = api_gateway::handle_request(
        ServerConfig::in_memory(),
        GatewayOptions::default(),
        request,
    )
    .await;

    let payload: CommandResponse = read_json(response).await;
    assert!(!payload.ok);
    assert_eq!(payload.error.as_deref(), Some("workspace not found"));
}

#[tokio::test]
async fn command_route_verifies_mark_read_reached_durable_state() {
    let config = ServerConfig::in_memory();
    let mut workspace = Workspace::from_task(make_task("o/r#42"), Utc::now());
    workspace.activity.push(lazybox_core::Activity {
        author: "reviewer".into(),
        body: "Please update this.".into(),
        created_at: Utc::now(),
        kind: lazybox_core::ActivityKind::Comment,
        node_id: None,
        path: None,
        line: None,
        diff_hunk: None,
        thread_id: None,
    });
    config
        .store
        .save_workspace(&WorkspaceRecord {
            key: workspace.key.as_str().to_string(),
            created_at: workspace.created_at,
            workspace_json: Some(serde_json::to_string(&workspace).unwrap()),
        })
        .unwrap();
    let frame = JsonClientFrame::Command(Command::MarkRead {
        session_key: (&workspace.key).into(),
    });
    let request = Request::builder()
        .method(Method::POST)
        .uri("/v1/commands")
        .body(Full::new(Bytes::from(serde_json::to_vec(&frame).unwrap())))
        .unwrap();

    let response =
        api_gateway::handle_request(config.clone(), GatewayOptions::default(), request).await;

    let payload: CommandResponse = read_json(response).await;
    assert!(payload.ok, "{:?}", payload.error);
    let saved = config
        .store
        .get_workspace(&workspace.key)
        .unwrap()
        .and_then(|record| record.workspace_json)
        .and_then(|json| serde_json::from_str::<Workspace>(&json).ok())
        .unwrap();
    assert_eq!(saved.unread_count(), 0);
}

#[tokio::test]
async fn json_command_route_rejects_every_binary_terminal_command() {
    let commands = [
        Command::Write {
            terminal_id: TerminalId(1),
            bytes: vec![1],
            intent: TerminalInputIntent::Compose,
        },
        Command::Resize {
            terminal_id: TerminalId(1),
            cols: 80,
            rows: 24,
        },
        Command::RequestTerminalResync {
            terminal_id: TerminalId(1),
            required_seq: 1,
        },
        Command::Close {
            terminal_id: TerminalId(1),
            client_request_id: None,
        },
        Command::FetchScrollback {
            terminal_id: TerminalId(1),
        },
    ];

    for command in commands {
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/commands")
            .body(Full::new(Bytes::from(
                serde_json::to_vec(&JsonClientFrame::Command(command)).unwrap(),
            )))
            .unwrap();
        let response = api_gateway::handle_request(
            ServerConfig::in_memory(),
            GatewayOptions::default(),
            request,
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}

#[tokio::test]
async fn json_stream_drops_terminal_commands_and_forwards_control_commands() {
    let mut input = serde_json::to_vec(&JsonClientFrame::Command(Command::Write {
        terminal_id: TerminalId(1),
        bytes: vec![1, 2, 3],
        intent: TerminalInputIntent::Compose,
    }))
    .unwrap();
    input.push(b'\n');
    input.extend(serde_json::to_vec(&JsonClientFrame::Command(Command::Refresh)).unwrap());
    input.push(b'\n');
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);

    api_gateway::pump_ndjson_commands(Full::new(Bytes::from(input)), tx).await;

    assert!(matches!(rx.recv().await, Some(Command::Refresh)));
    assert!(rx.recv().await.is_none());
}

#[tokio::test]
async fn command_route_returns_only_after_handler_side_effect() {
    let config = ServerConfig::in_memory();
    let observed = config.clone();
    let frame = JsonClientFrame::Command(Command::CreateProject {
        name: "API project".into(),
    });
    let request = Request::builder()
        .method(Method::POST)
        .uri("/v1/commands")
        .body(Full::new(Bytes::from(serde_json::to_vec(&frame).unwrap())))
        .unwrap();

    let response = api_gateway::handle_request(config, GatewayOptions::default(), request).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        observed
            .store
            .get_project(&lazybox_core::ProjectKey::local("api-project"))
            .unwrap()
            .is_some(),
        "HTTP completion must mean the command handler's store write is visible"
    );
}

#[tokio::test]
async fn command_route_rejects_connection_control_commands() {
    for command in [Command::Subscribe, Command::Shutdown] {
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/commands")
            .body(Full::new(Bytes::from(
                serde_json::to_vec(&JsonClientFrame::Command(command)).unwrap(),
            )))
            .unwrap();
        let response = api_gateway::handle_request(
            ServerConfig::in_memory(),
            GatewayOptions::default(),
            request,
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}

#[tokio::test]
async fn command_route_rejects_oversized_body() {
    let request = Request::builder()
        .method(Method::POST)
        .uri("/v1/commands")
        .body(Full::new(Bytes::from(vec![b'x'; 1024 * 1024 + 1])))
        .unwrap();

    let response = api_gateway::handle_request(
        ServerConfig::in_memory(),
        GatewayOptions::default(),
        request,
    )
    .await;

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}
#[tokio::test]
async fn command_route_rejects_malformed_json() {
    let request = Request::builder()
        .method(Method::POST)
        .uri("/v1/commands")
        .body(Full::new(Bytes::from_static(b"not json")))
        .unwrap();

    let response = api_gateway::handle_request(
        ServerConfig::in_memory(),
        GatewayOptions::default(),
        request,
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}
#[tokio::test]
async fn events_route_streams_initial_snapshot_as_ndjson() {
    let request = Request::builder()
        .method(Method::GET)
        .uri("/v1/events")
        .body(Full::new(Bytes::new()))
        .unwrap();

    let response = api_gateway::handle_request(
        ServerConfig::in_memory(),
        GatewayOptions::default(),
        request,
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();
    let frame = tokio::time::timeout(std::time::Duration::from_secs(2), body.frame())
        .await
        .expect("stream yields a frame")
        .expect("body frame")
        .expect("frame ok");
    let data = frame.into_data().expect("data frame");
    let server_frame: JsonServerFrame = serde_json::from_slice(data.trim_ascii()).unwrap();
    match server_frame {
        JsonServerFrame::Event(Event::Snapshot {
            workspaces,
            terminals,
            ..
        }) => {
            assert!(workspaces.is_empty());
            assert!(terminals.is_empty());
        }
        other => panic!("expected Snapshot frame, got {other:?}"),
    }
}

#[tokio::test]
async fn subscribe_surfaces_unreadable_records_after_the_snapshot() {
    let config = ServerConfig::in_memory();
    config
        .store
        .save_workspace(&WorkspaceRecord {
            key: "broken-workspace".into(),
            created_at: Utc::now(),
            workspace_json: Some("not valid workspace JSON".into()),
        })
        .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let tx = lazybox_ipc::EventSender::from_unbounded(tx);

    dispatch_command(&config, &tx, Command::Subscribe).await;

    assert!(matches!(rx.recv().await, Some(Event::Snapshot { .. })));
    match rx.recv().await {
        Some(Event::ProviderError {
            source, message, ..
        }) => {
            assert_eq!(source, "storage");
            assert!(message.contains("preserved, not deleted"));
        }
        other => panic!("expected storage recovery warning, got {other:?}"),
    }
}

/// A subscribing client — notably a remote `--connect` one — must be
/// told which agents the daemon can spawn, so it offers the box's
/// agents rather than the hardcoded trio it falls back to. See #742.
#[tokio::test]
async fn subscribe_reports_the_daemons_spawnable_agents() {
    let config = ServerConfig::in_memory();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let tx = lazybox_ipc::EventSender::from_unbounded(tx);

    dispatch_command(&config, &tx, Command::Subscribe).await;

    let mut agents = None;
    while let Ok(event) = rx.try_recv() {
        if let Event::AgentAvailabilityConfig {
            agents: reported, ..
        } = event
        {
            agents = Some(reported);
            break;
        }
    }
    let agents = agents.expect("subscribe emits AgentAvailabilityConfig");
    assert!(
        !agents.is_empty(),
        "the daemon always reports a spawnable set (its config, or the trio fallback)",
    );
}

#[tokio::test]
async fn stream_route_accepts_ndjson_commands_and_streams_events() {
    let mut line = serde_json::to_vec(&JsonClientFrame::Command(Command::Subscribe)).unwrap();
    line.push(b'\n');
    let request = Request::builder()
        .method(Method::POST)
        .uri("/v1/stream")
        .body(Full::new(Bytes::from(line)))
        .unwrap();

    let response = api_gateway::handle_request(
        ServerConfig::in_memory(),
        GatewayOptions::default(),
        request,
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();
    let frame = tokio::time::timeout(std::time::Duration::from_secs(2), body.frame())
        .await
        .expect("stream yields a frame")
        .expect("body frame")
        .expect("frame ok");
    let data = frame.into_data().expect("data frame");
    let server_frame: JsonServerFrame = serde_json::from_slice(data.trim_ascii()).unwrap();
    assert!(matches!(
        server_frame,
        JsonServerFrame::Event(Event::Snapshot { .. })
    ));
}

#[tokio::test]
async fn oversized_stream_frame_preserves_complete_lines_before_the_bad_line() {
    let mut payload = serde_json::to_vec(&JsonClientFrame::Command(Command::Subscribe)).unwrap();
    payload.push(b'\n');
    payload.extend(std::iter::repeat_n(b'x', 1024 * 1024 + 1));
    let request = Request::builder()
        .method(Method::POST)
        .uri("/v1/stream")
        .body(Full::new(Bytes::from(payload)))
        .unwrap();

    let response = api_gateway::handle_request(
        ServerConfig::in_memory(),
        GatewayOptions::default(),
        request,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    // Admission is line-scoped, not HTTP-frame-scoped: the complete
    // Subscribe line ahead of the oversized unterminated line still runs.
    let mut body = response.into_body();
    let frame = tokio::time::timeout(std::time::Duration::from_secs(2), body.frame())
        .await
        .expect("snapshot is not discarded with the later bad line")
        .expect("snapshot frame")
        .expect("frame body");
    let data = frame.into_data().expect("data frame");
    let server_frame: JsonServerFrame = serde_json::from_slice(data.trim_ascii()).unwrap();
    assert!(matches!(
        server_frame,
        JsonServerFrame::Event(Event::Snapshot { .. })
    ));
}

#[tokio::test]
async fn stream_route_caps_malformed_nonempty_command_lines() {
    let request = Request::builder()
        .method(Method::POST)
        .uri("/v1/stream")
        .body(Full::new(Bytes::from("{}\n".repeat(256))))
        .unwrap();

    let response = api_gateway::handle_request(
        ServerConfig::in_memory(),
        GatewayOptions::default(),
        request,
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        response.into_body().collect(),
    )
    .await
    .expect("over-limit command stream closes promptly")
    .expect("stream body remains valid");
}

#[tokio::test]
async fn stream_route_can_start_structured_agent_run() {
    let mut config = ServerConfig::in_memory();
    config.agents.register(Arc::new(FakeStreamAgent));
    config.agent_stream_spawner = Arc::new(FakeStreamSpawner {
        script: FAKE_API_STREAM_SCRIPT,
    });

    let command = Command::StartAgentRun {
        request_id: AgentRunRequestId("api-stream-request".into()),
        session_key: "api:stream".into(),
        session_id: None,
        source_terminal_id: None,
        agent: "fake-api-stream".into(),
        mode: AgentRuntimeMode::StreamJson,
        cwd: None,
        initial_input: Some(AgentInputMessage {
            text: Some("hello".into()),
            json: None,
        }),
        resume_latest: false,
        access: lazybox_ipc::AgentRunAccess::Default,
    };
    let mut line = serde_json::to_vec(&JsonClientFrame::Command(command)).unwrap();
    line.push(b'\n');
    let request = Request::builder()
        .method(Method::POST)
        .uri("/v1/stream")
        .body(Full::new(Bytes::from(line)))
        .unwrap();

    let response = api_gateway::handle_request(config, GatewayOptions::default(), request).await;

    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();
    let mut saw_delta = false;
    let mut saw_turn_finished = false;
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), body.frame())
            .await
            .expect("stream yields a frame")
            .expect("body frame")
            .expect("frame ok");
        let data = frame.into_data().expect("data frame");
        let server_frame: JsonServerFrame = serde_json::from_slice(data.trim_ascii()).unwrap();
        match server_frame {
            JsonServerFrame::Event(Event::AgentAssistantTextDelta { delta, .. }) => {
                assert_eq!(delta, "api-ok");
                saw_delta = true;
            }
            JsonServerFrame::Event(Event::AgentTurnFinished {
                result,
                session_id,
                error,
                ..
            }) => {
                assert_eq!(result.as_deref(), Some("done"));
                assert_eq!(session_id.as_deref(), Some("api-session"));
                assert!(error.is_none());
                saw_turn_finished = true;
            }
            JsonServerFrame::Event(Event::AgentRunFinished {
                exit_code, error, ..
            }) => {
                assert_eq!(exit_code, Some(0));
                assert!(error.is_none());
                break;
            }
            JsonServerFrame::Event(Event::AgentRunStarted { .. } | Event::AgentRawJson { .. }) => {}
            other => panic!("unexpected frame: {other:?}"),
        }
    }
    assert!(saw_delta);
    assert!(saw_turn_finished);
}

// ── issue #773: workspace-addressed inject + read-output ──────────────

/// Spawn a mock-backed agent terminal for `workspace`, seed its deep
/// scrollback, and return the backend key the mock records writes under.
async fn register_mock_agent(
    config: &ServerConfig,
    mock: &lazybox_server::backend::MockBackend,
    terminal_id: TerminalId,
    workspace: &str,
    agent: &str,
) -> String {
    use lazybox_server::backend::SessionBackend;
    let key = mock
        .spawn(&[], None, &[], "issue-773")
        .await
        .expect("spawn mock terminal");
    config
        .terminal
        .register_terminal(
            terminal_id,
            key.clone(),
            lazybox_core::SessionKey::new(workspace),
            TerminalKind::Agent(agent.into()),
        )
        .await;
    key
}

fn json_post(uri: &str, body: serde_json::Value) -> Request<Full<Bytes>> {
    Request::builder()
        .method(Method::POST)
        .uri(uri)
        .body(Full::new(Bytes::from(serde_json::to_vec(&body).unwrap())))
        .unwrap()
}

#[tokio::test]
async fn inject_route_resolves_the_workspace_and_delivers_to_the_running_agent() {
    let (config, mock) = ServerConfig::in_memory_with_mock();
    let terminal_id = TerminalId(773);
    let key =
        register_mock_agent(&config, &mock, terminal_id, "github:owner/repo#7", "claude").await;

    let request = json_post(
        "/v1/agents/inject",
        serde_json::json!({ "workspace": "github:owner/repo#7", "text": "drive the fleet" }),
    );
    let response =
        api_gateway::handle_request(config.clone(), GatewayOptions::default(), request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let payload: api_gateway::InjectResponse = read_json(response).await;
    assert!(payload.accepted);
    assert_eq!(payload.terminal_id, terminal_id);
    assert_eq!(payload.workspace, "github:owner/repo#7");

    // The prompt reaches the agent's PTY through the settle-gated path.
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let joined: Vec<u8> = mock.writes_for(&key).await.concat();
            if String::from_utf8_lossy(&joined).contains("drive the fleet") {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("inject must deliver the prompt to the resolved agent terminal");
}

#[tokio::test]
async fn inject_route_rejects_empty_text_without_touching_the_agent() {
    let (config, mock) = ServerConfig::in_memory_with_mock();
    let key = register_mock_agent(
        &config,
        &mock,
        TerminalId(1),
        "github:owner/repo#7",
        "claude",
    )
    .await;

    let request = json_post(
        "/v1/agents/inject",
        serde_json::json!({ "workspace": "github:owner/repo#7", "text": "   " }),
    );
    let response =
        api_gateway::handle_request(config.clone(), GatewayOptions::default(), request).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(mock.writes_for(&key).await.is_empty());
}

#[tokio::test]
async fn inject_route_404s_a_workspace_with_no_running_agent() {
    let (config, mock) = ServerConfig::in_memory_with_mock();
    // A shell in the workspace is not an agent — inject must not target it.
    use lazybox_server::backend::SessionBackend;
    let key = mock.spawn(&[], None, &[], "shell").await.expect("spawn");
    config
        .terminal
        .register_terminal(
            TerminalId(1),
            key,
            lazybox_core::SessionKey::new("github:owner/repo#7"),
            TerminalKind::Shell,
        )
        .await;

    let request = json_post(
        "/v1/agents/inject",
        serde_json::json!({ "workspace": "github:owner/repo#7", "text": "hello" }),
    );
    let response = api_gateway::handle_request(config, GatewayOptions::default(), request).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn output_route_returns_the_cleaned_and_line_limited_tail() {
    let (config, mock) = ServerConfig::in_memory_with_mock();
    let terminal_id = TerminalId(773);
    let key =
        register_mock_agent(&config, &mock, terminal_id, "github:owner/repo#7", "claude").await;
    // ANSI colour, a blank line, and a progress-bar `\r` overwrite the
    // cleaner must resolve to the post-carriage-return content.
    mock.set_deep_scrollback(
        &key,
        "\x1b[32mline-one\x1b[0m\n\nprogress\rline-two\nline-three\n".as_bytes(),
    )
    .await;

    let request = json_post(
        "/v1/agents/output",
        serde_json::json!({ "workspace": "github:owner/repo#7", "tail": 2 }),
    );
    let response =
        api_gateway::handle_request(config.clone(), GatewayOptions::default(), request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let payload: api_gateway::AgentOutputResponse = read_json(response).await;
    assert_eq!(payload.terminal_id, terminal_id);
    assert_eq!(payload.lines, 2);
    assert_eq!(payload.output, "line-two\nline-three");
}

#[tokio::test]
async fn output_route_404s_an_unknown_workspace() {
    let (config, _mock) = ServerConfig::in_memory_with_mock();
    let request = json_post(
        "/v1/agents/output",
        serde_json::json!({ "workspace": "github:owner/repo#404" }),
    );
    let response = api_gateway::handle_request(config, GatewayOptions::default(), request).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn output_route_tails_a_large_scrollback_correctly() {
    // A megabyte-scale deep scrollback must still return the exact last
    // lines — the read only cleans a bounded trailing window, and that
    // window must not clip the tail it keeps.
    let (config, mock) = ServerConfig::in_memory_with_mock();
    let terminal_id = TerminalId(773);
    let key =
        register_mock_agent(&config, &mock, terminal_id, "github:owner/repo#7", "claude").await;
    let mut huge = String::new();
    for n in 0..100_000 {
        huge.push_str(&format!("scroll {n}\n"));
    }
    assert!(huge.len() > 1024 * 1024, "buffer must exceed the scan cap");
    mock.set_deep_scrollback(&key, huge.as_bytes()).await;

    let request = json_post(
        "/v1/agents/output",
        serde_json::json!({ "workspace": "github:owner/repo#7", "tail": 2 }),
    );
    let response =
        api_gateway::handle_request(config.clone(), GatewayOptions::default(), request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let payload: api_gateway::AgentOutputResponse = read_json(response).await;
    assert_eq!(payload.lines, 2);
    assert_eq!(payload.output, "scroll 99998\nscroll 99999");
}

#[tokio::test]
async fn output_route_times_out_a_wedged_backend_read() {
    let (config, mock) = ServerConfig::in_memory_with_mock();
    let terminal_id = TerminalId(773);
    let key =
        register_mock_agent(&config, &mock, terminal_id, "github:owner/repo#7", "claude").await;
    // No deep scrollback → the read falls back to `snapshot`, which we
    // wedge so it never resolves. A bounded handler must return 504
    // instead of pinning the connection open forever.
    mock.wedge_snapshot(&key).await;
    let options = GatewayOptions {
        command_timeout: std::time::Duration::from_millis(50),
        ..GatewayOptions::default()
    };

    let request = json_post(
        "/v1/agents/output",
        serde_json::json!({ "workspace": "github:owner/repo#7" }),
    );
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        api_gateway::handle_request(config, options, request),
    )
    .await
    .expect("handler must return, not hang, on a wedged backend");
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
}

#[tokio::test]
async fn inject_route_times_out_when_the_terminal_lock_is_held() {
    let (config, mock) = ServerConfig::in_memory_with_mock();
    let terminal_id = TerminalId(773);
    let key =
        register_mock_agent(&config, &mock, terminal_id, "github:owner/repo#7", "claude").await;
    // A wedged write pins the per-terminal interaction lock: the background
    // Write acquires `acquire_live`, then hangs in the backend, holding it.
    // Inject registration must wait on that lock, so the bounded handler
    // returns 504 instead of pinning the HTTP connection open indefinitely.
    mock.wedge_write(&key).await;
    let writer = {
        let config = config.clone();
        tokio::spawn(async move {
            spawn_handler::handle_write(&config, terminal_id, b"x", TerminalInputIntent::Submit)
                .await;
        })
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !mock.write_attempts().await.contains(&key) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the wedged write must take the interaction lock");

    let options = GatewayOptions {
        command_timeout: std::time::Duration::from_millis(50),
        ..GatewayOptions::default()
    };
    let request = json_post(
        "/v1/agents/inject",
        serde_json::json!({ "workspace": "github:owner/repo#7", "text": "drive the fleet" }),
    );
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        api_gateway::handle_request(config, options, request),
    )
    .await
    .expect("inject handler must return, not hang, while the lock is held");
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    writer.abort();
}
