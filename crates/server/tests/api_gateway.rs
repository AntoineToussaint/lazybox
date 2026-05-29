pub use pilot_server::{Server, ServerConfig};

#[allow(dead_code)]
#[path = "../src/api_gateway.rs"]
mod api_gateway;

use api_gateway::{
    CommandResponse, GatewayOptions, HealthResponse, JsonClientFrame, JsonServerFrame,
    WorkspacesResponse,
};
use bytes::Bytes;
use chrono::Utc;
use http_body_util::{BodyExt, Full};
use hyper::header::{AUTHORIZATION, HeaderValue};
use hyper::{Method, Request, StatusCode};
use pilot_agents::{Agent, SpawnCtx};
use pilot_core::{CiStatus, ReviewStatus, Task, TaskId, TaskRole, TaskState, Workspace};
use pilot_ipc::{AgentInputMessage, AgentRuntimeMode, Command, Event};
use pilot_store::WorkspaceRecord;
use std::path::PathBuf;
use std::sync::Arc;

struct FakeStreamAgent {
    program: PathBuf,
}

impl Agent for FakeStreamAgent {
    fn id(&self) -> &'static str {
        "fake-api-stream"
    }

    fn display_name(&self) -> &'static str {
        "Fake API Stream"
    }

    fn spawn(&self, _ctx: &SpawnCtx) -> Vec<String> {
        vec![self.program.display().to_string()]
    }
}

#[cfg(unix)]
fn make_fake_claude_script(dir: &std::path::Path) -> PathBuf {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let script = dir.join("fake-api-claude-stream.sh");
    let mut file = std::fs::File::create(&script).unwrap();
    writeln!(
        file,
        r#"#!/bin/sh
if IFS= read -r _line; then
  printf '%s\n' '{{"type":"stream_event","event":{{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":"api-ok"}}}}}}'
  printf '%s\n' '{{"type":"result","subtype":"success","session_id":"api-session","result":"done"}}'
fi
"#
    )
    .unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();
    script
}

fn make_task(key: &str) -> Task {
    let (path, num) = key.rsplit_once('#').unwrap_or((key, "1"));
    Task {
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
    assert_eq!(payload.service, "pilot-api-gateway");
    assert!(payload.ok);
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
    assert_eq!(payload.workspaces[0].pr.as_ref().unwrap().id.key, "o/r#42");
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
#[cfg(unix)]
#[tokio::test]
async fn stream_route_can_start_structured_agent_run() {
    let temp = tempfile::tempdir().unwrap();
    let program = make_fake_claude_script(temp.path());
    let mut config = ServerConfig::in_memory();
    config
        .agents
        .register(Arc::new(FakeStreamAgent { program }));

    let command = Command::StartAgentRun {
        session_key: "api:stream".into(),
        session_id: None,
        agent: "fake-api-stream".into(),
        mode: AgentRuntimeMode::StreamJson,
        cwd: Some(temp.path().display().to_string()),
        initial_input: Some(AgentInputMessage {
            text: Some("hello".into()),
            json: None,
        }),
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
