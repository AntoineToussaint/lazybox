pub use lazybox_server::metrics;
pub use lazybox_server::{Server, ServerConfig, dispatch_command};

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
use lazybox_agents::{Agent, SpawnCtx, StructuredAgentProtocol};
use lazybox_core::{CiStatus, ReviewStatus, Task, TaskId, TaskRole, TaskState, Workspace};
use lazybox_ipc::{AgentInputMessage, AgentRunRequestId, AgentRuntimeMode, Command, Event};
use lazybox_server::ServerError;
use lazybox_server::agent_stream::{AgentStreamConfig, AgentStreamIo, AgentStreamSpawner};
use lazybox_store::WorkspaceRecord;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

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
    }
}

async fn read_json<T: serde::de::DeserializeOwned>(
    response: hyper::Response<api_gateway::Body>,
) -> T {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn raw_http_request(port: u16, path: &str, token: Option<&str>) -> String {
    let authorization = token
        .map(|token| format!("Authorization: Bearer {token}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n{authorization}\r\n"
    );
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    response
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
async fn thin_client_shell_is_public_while_api_routes_stay_authenticated() {
    let request = Request::builder()
        .method(Method::GET)
        .uri("/")
        .body(Full::new(Bytes::new()))
        .unwrap();
    let options = GatewayOptions {
        bearer_token: Some("secret".into()),
        ..GatewayOptions::default()
    };

    let response = api_gateway::handle_request(ServerConfig::in_memory(), options, request).await;

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
    assert!(html.contains("lazybox remote"));
    assert!(html.contains("/v1/workspaces"));
    assert!(html.contains("Authorization: `Bearer ${token}`"));
}

#[tokio::test]
async fn wildcard_listener_serves_the_token_authenticated_thin_client_path() {
    let listener = TcpListener::bind("0.0.0.0:0").await.unwrap();
    let bind_addr = listener.local_addr().unwrap();
    assert!(!bind_addr.ip().is_loopback());
    let port = bind_addr.port();
    let options = GatewayOptions {
        bind_addr,
        bearer_token: Some("remote-secret".into()),
        ..GatewayOptions::default()
    };
    let server = tokio::spawn(api_gateway::serve_listener(
        ServerConfig::in_memory(),
        options,
        listener,
    ));

    let page = raw_http_request(port, "/", None).await;
    assert!(page.starts_with("HTTP/1.1 200 OK"));
    assert!(page.contains("lazybox remote"));

    let unauthorized = raw_http_request(port, "/v1/health", None).await;
    assert!(unauthorized.starts_with("HTTP/1.1 401 Unauthorized"));

    let authenticated = raw_http_request(port, "/v1/health", Some("remote-secret")).await;
    assert!(authenticated.starts_with("HTTP/1.1 200 OK"));
    assert!(authenticated.contains("\"service\":\"lazybox-api-gateway\""));

    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
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
