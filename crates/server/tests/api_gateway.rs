pub use lazybox_server::metrics;
pub use lazybox_server::pty;
pub use lazybox_server::{Server, ServerConfig, dispatch_command};

#[allow(dead_code)]
#[path = "../src/api_gateway.rs"]
mod api_gateway;

use api_gateway::{
    CommandResponse, DesktopCommand, DesktopEvent, DesktopTerminalSnapshot, GatewayOptions,
    HealthResponse, JsonClientFrame, JsonServerFrame, ProtocolResponse,
    UnsupportedFingerprintResponse, UnsupportedProtocolResponse, WorkspacesResponse,
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
        DesktopCommand::FocusWorkspace { .. } => "FocusWorkspace",
        DesktopCommand::MarkRead { .. } => "MarkRead",
        DesktopCommand::PostReply { .. } => "PostReply",
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
        DesktopEvent::ProviderError { .. } => "ProviderError",
        DesktopEvent::CommandRejected { .. } => "CommandRejected",
        DesktopEvent::PollCompleted { .. } => "PollCompleted",
        DesktopEvent::PollProgress { .. } => "PollProgress",
        DesktopEvent::WorktreeProgress { .. } => "WorktreeProgress",
    }
}

#[test]
fn desktop_compatibility_fixture_is_current() {
    let session_key = lazybox_core::SessionKey::from("github:o/r#42");
    let workspace = desktop_contract_workspace();
    let commands = vec![
        DesktopCommand::SpawnAgent {
            session_key: session_key.clone(),
            agent: "codex".into(),
        },
        DesktopCommand::SpawnShell {
            session_key: session_key.clone(),
        },
        DesktopCommand::FocusWorkspace {
            session_key: session_key.clone(),
        },
        DesktopCommand::MarkRead {
            session_key: session_key.clone(),
        },
        DesktopCommand::PostReply {
            session_key: session_key.clone(),
            body: "Ready for another look.".into(),
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
    ];
    let command_tags = commands
        .iter()
        .map(desktop_command_tag)
        .collect::<std::collections::BTreeSet<_>>();
    let event_tags = events
        .iter()
        .map(desktop_event_tag)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(command_tags.len(), 6);
    assert_eq!(event_tags.len(), 12);
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
async fn unsupported_protocol_fingerprint_is_rejected_clearly() {
    let request = Request::builder()
        .method(Method::GET)
        .uri("/v1/protocol")
        .header(
            api_gateway::PROTOCOL_FINGERPRINT_HEADER,
            api_gateway::DESKTOP_PROTOCOL_FINGERPRINT.wrapping_add(1),
        )
        .body(Full::new(Bytes::new()))
        .unwrap();

    let response = api_gateway::handle_request(
        ServerConfig::in_memory(),
        GatewayOptions::default(),
        request,
    )
    .await;

    assert_eq!(response.status(), StatusCode::UPGRADE_REQUIRED);
    let payload: UnsupportedFingerprintResponse = read_json(response).await;
    assert_eq!(payload.supported, api_gateway::DESKTOP_PROTOCOL_FINGERPRINT);
    assert!(
        payload
            .error
            .contains("unsupported lazybox protocol fingerprint")
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
