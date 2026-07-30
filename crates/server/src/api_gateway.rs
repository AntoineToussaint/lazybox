//! Minimal JSON gateway for Lazybox.
//!
//! This module is intentionally isolated from `lib.rs` wiring. It uses
//! Hyper 1 for HTTP and exposes newline-delimited JSON frames so API
//! clients can drive the same server-owned IPC model as the TUI.

use crate::metrics::EventMetricsSnapshot;
use crate::{Server, ServerConfig};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, channel::Channel, combinators::UnsyncBoxBody};
use hyper::body::{Body as HttpBody, Incoming};
use hyper::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use lazybox_ipc::{
    COMMAND_CHANNEL_CAPACITY, Command, Connection, EVENT_CHANNEL_CAPACITY, Event, TerminalId,
    event_forward_channel,
};
use lazybox_store::StoreError;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::fmt::Display;
use std::mem::size_of;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, mpsc};

pub type Body = UnsyncBoxBody<Bytes, Infallible>;
pub const DESKTOP_PROTOCOL_VERSION: u32 = 1;
pub const PROTOCOL_VERSION_HEADER: &str = "x-lazybox-protocol-version";
pub const PROTOCOL_FINGERPRINT_HEADER: &str = "x-lazybox-protocol-fingerprint";
pub const TERMINAL_BINARY_CONTENT_TYPE: &str = "application/vnd.lazybox.terminal.v1";
pub const TERMINAL_FRAME_LENGTH_OFFSET: usize = 0;
pub const TERMINAL_FRAME_LENGTH_PREFIX_BYTES: usize = 4;
pub const TERMINAL_SERVER_FRAME_HEADER_BYTES: usize = 25;
pub const TERMINAL_SERVER_FRAME_KIND_OFFSET: usize = TERMINAL_FRAME_LENGTH_PREFIX_BYTES;
pub const TERMINAL_SERVER_FRAME_TERMINAL_ID_OFFSET: usize =
    TERMINAL_SERVER_FRAME_KIND_OFFSET + size_of::<u8>();
pub const TERMINAL_SERVER_FRAME_FIRST_SEQ_OFFSET: usize =
    TERMINAL_SERVER_FRAME_TERMINAL_ID_OFFSET + size_of::<u64>();
pub const TERMINAL_SERVER_FRAME_LAST_SEQ_OFFSET: usize =
    TERMINAL_SERVER_FRAME_FIRST_SEQ_OFFSET + size_of::<u64>();
pub const TERMINAL_SERVER_FRAME_PAYLOAD_OFFSET: usize =
    TERMINAL_FRAME_LENGTH_PREFIX_BYTES + TERMINAL_SERVER_FRAME_HEADER_BYTES;
pub const MAX_TERMINAL_BINARY_FRAME_BYTES: usize =
    crate::pty::REPLAY_RING_BYTES + TERMINAL_SERVER_FRAME_HEADER_BYTES;
pub const TERMINAL_SERVER_FRAME_SNAPSHOT: u8 = 1;
pub const TERMINAL_SERVER_FRAME_OUTPUT: u8 = 2;
pub const TERMINAL_SERVER_FRAME_RESYNC: u8 = 3;
pub const TERMINAL_SERVER_FRAME_SCROLLBACK: u8 = 4;
pub const TERMINAL_SERVER_FRAME_RESYNC_UNAVAILABLE: u8 = 5;
pub const TERMINAL_CLIENT_COMMAND_WRITE: u8 = 1;
pub const TERMINAL_CLIENT_COMMAND_RESIZE: u8 = 2;
pub const TERMINAL_CLIENT_COMMAND_RESYNC: u8 = 3;
pub const TERMINAL_CLIENT_COMMAND_CLOSE: u8 = 4;
pub const TERMINAL_CLIENT_COMMAND_FETCH_SCROLLBACK: u8 = 5;
pub const TERMINAL_CLIENT_BODY_KIND_OFFSET: usize = 0;
pub const TERMINAL_CLIENT_BODY_TERMINAL_ID_OFFSET: usize =
    TERMINAL_CLIENT_BODY_KIND_OFFSET + size_of::<u8>();
pub const TERMINAL_CLIENT_FRAME_HEADER_BYTES: usize = 9;
pub const TERMINAL_CLIENT_BODY_PAYLOAD_OFFSET: usize = TERMINAL_CLIENT_FRAME_HEADER_BYTES;
pub const TERMINAL_CLIENT_FRAME_KIND_OFFSET: usize =
    TERMINAL_FRAME_LENGTH_PREFIX_BYTES + TERMINAL_CLIENT_BODY_KIND_OFFSET;
pub const TERMINAL_CLIENT_FRAME_TERMINAL_ID_OFFSET: usize =
    TERMINAL_FRAME_LENGTH_PREFIX_BYTES + TERMINAL_CLIENT_BODY_TERMINAL_ID_OFFSET;
pub const TERMINAL_CLIENT_FRAME_PAYLOAD_OFFSET: usize =
    TERMINAL_FRAME_LENGTH_PREFIX_BYTES + TERMINAL_CLIENT_BODY_PAYLOAD_OFFSET;
pub const TERMINAL_RESIZE_PAYLOAD_BYTES: usize = 4;
pub const TERMINAL_RESIZE_COLS_OFFSET: usize = 0;
pub const TERMINAL_RESIZE_ROWS_OFFSET: usize = size_of::<u16>();
pub const TERMINAL_RESYNC_PAYLOAD_BYTES: usize = size_of::<u64>();
pub const TERMINAL_RESYNC_REQUIRED_SEQ_OFFSET: usize = 0;
pub const TERMINAL_WRITE_INTENT_OFFSET: usize = 0;
pub const TERMINAL_WRITE_BYTES_OFFSET: usize = size_of::<u8>();
pub const TERMINAL_WRITE_INTENT_COMPOSE: u8 = 0;
pub const TERMINAL_WRITE_INTENT_SUBMIT: u8 = 1;
pub const TERMINAL_WRITE_INTENT_VIEW: u8 = 2;
pub const DESKTOP_TERMINAL_STREAM_ITEM_RESET: u8 = 0;
pub const DESKTOP_TERMINAL_STREAM_ITEM_DATA: u8 = 1;

pub const DESKTOP_PROTOCOL_FINGERPRINT: u32 = desktop_protocol_fingerprint();

const fn desktop_protocol_fingerprint() -> u32 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let ipc = lazybox_ipc::PROTOCOL_FINGERPRINT.to_le_bytes();
    let mut index = 0;
    while index < ipc.len() {
        hash ^= ipc[index] as u64;
        hash = hash.wrapping_mul(0x100_0000_01b3);
        index += 1;
    }
    let source = include_bytes!("api_gateway.rs");
    index = 0;
    while index < source.len() {
        hash ^= source[index] as u64;
        hash = hash.wrapping_mul(0x100_0000_01b3);
        index += 1;
    }
    (hash ^ (hash >> 32)) as u32
}

const API_CLIENT_HTML: &str = include_str!("api_client.html");

#[derive(Debug, Clone)]
pub struct GatewayOptions {
    pub bind_addr: SocketAddr,
    pub bearer_token: Option<String>,
    /// Hard cap on simultaneously served HTTP connections, including
    /// long-lived event streams.
    pub max_connections: usize,
    /// Maximum wall time for a one-shot command handler.
    pub command_timeout: Duration,
}

impl Default for GatewayOptions {
    fn default() -> Self {
        Self {
            bind_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            bearer_token: None,
            max_connections: 32,
            command_timeout: Duration::from_secs(5 * 60),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    #[error("http server error: {0}")]
    Hyper(#[from] hyper::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("store error: {0}")]
    Store(String),
    #[error("refusing plaintext API listener on non-loopback address {0}")]
    NonLoopback(SocketAddr),
}

impl From<StoreError> for GatewayError {
    fn from(value: StoreError) -> Self {
        Self::Store(value.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct HealthResponse {
    pub ok: bool,
    pub service: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct ProtocolResponse {
    pub protocol_version: u32,
    pub protocol_fingerprint: u32,
    pub build_version: String,
    pub terminal_transport: String,
    pub max_terminal_frame_bytes: usize,
    pub max_terminal_write_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct UnsupportedProtocolResponse {
    pub error: String,
    pub requested: String,
    pub supported: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct UnsupportedFingerprintResponse {
    pub error: String,
    pub requested: String,
    pub supported: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct WorkspacesResponse {
    pub workspaces: Vec<lazybox_core::Workspace>,
    /// Persisted rows that were preserved but could not be decoded.
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct CommandResponse {
    pub ok: bool,
    /// Set only after the daemon-side handler has returned.
    pub completed: bool,
    /// Connection-scoped outcomes emitted directly by the command handler.
    pub events: Vec<Event>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
#[serde(tag = "type", content = "payload")]
pub enum JsonClientFrame {
    Command(Command),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
#[serde(tag = "type", content = "payload")]
pub enum JsonServerFrame {
    Event(Event),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct DesktopInfo {
    pub protocol_version: u32,
    pub max_terminal_frame_bytes: usize,
    pub max_terminal_write_bytes: usize,
    pub agents: Vec<String>,
    pub default_agent: String,
    pub setup_completed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub enum DesktopCommand {
    SpawnAgent {
        session_key: lazybox_core::SessionKey,
        agent: String,
    },
    SpawnShell {
        session_key: lazybox_core::SessionKey,
    },
    CreateWorkspace {
        name: String,
        project_key: lazybox_core::ProjectKey,
        agent: Option<String>,
    },
    MarkRead {
        session_key: lazybox_core::SessionKey,
    },
    PostReply {
        session_key: lazybox_core::SessionKey,
        body: String,
    },
    FocusWorkspace {
        session_key: lazybox_core::SessionKey,
    },
    Refresh,
}

impl From<DesktopCommand> for Command {
    fn from(command: DesktopCommand) -> Self {
        match command {
            DesktopCommand::SpawnAgent { session_key, agent } => Command::Spawn {
                session_key,
                session_id: None,
                client_request_id: None,
                kind: lazybox_ipc::TerminalKind::Agent(agent),
                cwd: None,
                initial_prompt: None,
                on_main: false,
                model_alias: None,
                access: lazybox_ipc::AgentRunAccess::Default,
            },
            DesktopCommand::SpawnShell { session_key } => Command::Spawn {
                session_key,
                session_id: None,
                client_request_id: None,
                kind: lazybox_ipc::TerminalKind::Shell,
                cwd: None,
                initial_prompt: None,
                on_main: false,
                model_alias: None,
                access: lazybox_ipc::AgentRunAccess::Default,
            },
            DesktopCommand::CreateWorkspace {
                name,
                project_key,
                agent,
            } => Command::CreateWorkspace {
                name,
                project_key,
                spawn_agent: agent,
            },
            DesktopCommand::MarkRead { session_key } => Command::MarkRead { session_key },
            DesktopCommand::PostReply { session_key, body } => {
                Command::PostReply { session_key, body }
            }
            DesktopCommand::FocusWorkspace { session_key } => {
                Command::FocusWorkspace { session_key }
            }
            DesktopCommand::Refresh => Command::Refresh,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct DesktopTerminalSnapshot {
    pub terminal_id: TerminalId,
    pub session_key: lazybox_core::SessionKey,
    pub kind: lazybox_ipc::TerminalKind,
    pub last_seq: u64,
    pub agent_state: Option<lazybox_ipc::AgentState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub enum DesktopEvent {
    Snapshot {
        workspaces: Vec<lazybox_core::Workspace>,
        terminals: Vec<DesktopTerminalSnapshot>,
    },
    WorkspaceUpserted(Box<lazybox_core::Workspace>),
    WorkspaceRemoved(lazybox_core::WorkspaceKey),
    TerminalSpawned {
        terminal_id: TerminalId,
        session_key: lazybox_core::SessionKey,
        kind: lazybox_ipc::TerminalKind,
    },
    TerminalExited {
        terminal_id: TerminalId,
        exit_code: Option<i32>,
        last_output: Option<String>,
    },
    TerminalFocusRequested {
        terminal_id: TerminalId,
    },
    AgentState {
        session_key: lazybox_core::SessionKey,
        terminal_id: TerminalId,
        state: lazybox_ipc::AgentState,
    },
    ProviderError {
        source: String,
        message: String,
    },
    CommandRejected {
        command: String,
        message: String,
    },
    PollCompleted {
        source: String,
        count: usize,
    },
    PollProgress {
        source: String,
        message: String,
    },
    WorktreeProgress {
        session_key: lazybox_core::SessionKey,
        step: lazybox_ipc::WorktreeStep,
        status: lazybox_ipc::WorktreeStepStatus,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
#[serde(tag = "type", content = "payload")]
pub enum DesktopStreamMessage {
    Connected,
    Disconnected { message: String },
    Frame(Box<DesktopEvent>),
}

pub fn desktop_event(event: Event) -> Option<DesktopEvent> {
    match event {
        Event::Snapshot {
            workspaces,
            terminals,
            ..
        } => Some(DesktopEvent::Snapshot {
            workspaces,
            terminals: terminals
                .into_iter()
                .map(|terminal| DesktopTerminalSnapshot {
                    terminal_id: terminal.terminal_id,
                    session_key: terminal.session_key,
                    kind: terminal.kind,
                    last_seq: terminal.last_seq,
                    agent_state: terminal.agent_state,
                })
                .collect(),
        }),
        Event::WorkspaceUpserted(workspace) => Some(DesktopEvent::WorkspaceUpserted(workspace)),
        Event::WorkspaceRemoved(key) => Some(DesktopEvent::WorkspaceRemoved(key)),
        Event::TerminalSpawned {
            terminal_id,
            session_key,
            kind,
            ..
        } => Some(DesktopEvent::TerminalSpawned {
            terminal_id,
            session_key,
            kind,
        }),
        Event::TerminalExited {
            terminal_id,
            exit_code,
            last_output,
        } => Some(DesktopEvent::TerminalExited {
            terminal_id,
            exit_code,
            last_output,
        }),
        Event::TerminalFocusRequested { terminal_id } => {
            Some(DesktopEvent::TerminalFocusRequested { terminal_id })
        }
        Event::AgentState {
            session_key,
            terminal_id,
            state,
        } => Some(DesktopEvent::AgentState {
            session_key,
            terminal_id,
            state,
        }),
        Event::ProviderError {
            source, message, ..
        } => Some(DesktopEvent::ProviderError { source, message }),
        Event::CommandRejected { command, message } => {
            Some(DesktopEvent::CommandRejected { command, message })
        }
        Event::PollCompleted { source, count } => {
            Some(DesktopEvent::PollCompleted { source, count })
        }
        Event::PollProgress { source, message } => {
            Some(DesktopEvent::PollProgress { source, message })
        }
        Event::WorktreeProgress {
            session_key,
            step,
            status,
            ..
        } => Some(DesktopEvent::WorktreeProgress {
            session_key,
            step,
            status,
        }),
        _ => None,
    }
}

pub struct LocalIpcBridge {
    pub command_tx: mpsc::Sender<Command>,
    /// Bounded — fed by the same drop-and-resync forwarder
    /// ([`crate::event_forward`]) the channel/socket transports use, so
    /// a stalled `/v1/events` client can never buffer the event
    /// firehose unboundedly: output is dropped and re-synced from the
    /// ring, lifecycle events queue losslessly behind the cap.
    pub event_rx: mpsc::Receiver<Event>,
}

pub fn check_bearer_token(
    authorization: Option<&HeaderValue>,
    expected_token: Option<&str>,
) -> bool {
    let Some(expected_token) = expected_token else {
        return true;
    };
    let Some(value) = authorization.and_then(|value| value.to_str().ok()) else {
        return false;
    };
    let Some(token) = value.strip_prefix("Bearer ") else {
        return false;
    };
    constant_time_eq(token.as_bytes(), expected_token.as_bytes())
}

/// Constant-time byte comparison for the bearer token: fold the XOR of
/// every byte pair instead of short-circuiting on the first mismatch,
/// so response timing doesn't leak how much of the token matched. The
/// length check still exits early — length is not a secret.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn health_response() -> HealthResponse {
    HealthResponse {
        ok: true,
        service: "lazybox-api-gateway".into(),
    }
}

pub fn protocol_response() -> ProtocolResponse {
    ProtocolResponse {
        protocol_version: DESKTOP_PROTOCOL_VERSION,
        protocol_fingerprint: DESKTOP_PROTOCOL_FINGERPRINT,
        build_version: lazybox_ipc::BUILD_VERSION.to_string(),
        terminal_transport: TERMINAL_BINARY_CONTENT_TYPE.to_string(),
        max_terminal_frame_bytes: MAX_TERMINAL_BINARY_FRAME_BYTES,
        max_terminal_write_bytes: lazybox_ipc::MAX_WRITE_CHUNK_BYTES,
    }
}

pub fn metrics_response(config: &ServerConfig) -> EventMetricsSnapshot {
    config.event_metrics.snapshot()
}

/// Full workspace scan + deserialize on `spawn_blocking` (issue #34's
/// convention): the synchronous rusqlite scan can pin a runtime
/// worker for up to the 5s busy_timeout when another process
/// contends on the DB, which on the gateway's runtime would stall
/// unrelated requests.
pub async fn workspaces_response(
    config: &ServerConfig,
) -> Result<WorkspacesResponse, GatewayError> {
    let store = config.store.clone();
    let records = tokio::task::spawn_blocking(move || store.list_workspaces())
        .await
        .map_err(|error| {
            lazybox_store::StoreError::Backend(format!("workspace scan task failed: {error}"))
        })??;
    let mut workspaces = Vec::new();
    let mut warnings = Vec::new();
    for record in records {
        match record.workspace_json {
            Some(json) => match serde_json::from_str::<lazybox_core::Workspace>(&json) {
                Ok(workspace) => workspaces.push(workspace),
                Err(error) => {
                    tracing::warn!(
                        "api gateway: preserving unreadable workspace {}: {error}",
                        record.key
                    );
                    warnings.push(format!("workspace {}: {error}", record.key));
                }
            },
            None => {
                warnings.push(format!("workspace {}: missing JSON payload", record.key));
            }
        }
    }
    Ok(WorkspacesResponse {
        workspaces,
        warnings,
    })
}

/// Create a local IPC bridge backed by the existing `Server::serve`
/// connection model. API handlers feed decoded `JsonClientFrame`
/// commands into `command_tx` and serialize `event_rx` values as
/// `JsonServerFrame::Event`.
///
/// Wired like the channel/socket transports: the serve loop writes the
/// bounded raw staging stream, and `Server::serve` spawns the drop-and-resync
/// forwarder ([`crate::event_forward::forward_events`]) that bridges it
/// into the bounded `event_rx`. The resync semantics translate directly
/// to ndjson — a slow consumer sees `TerminalResync` frames instead of
/// every output chunk, and the daemon's memory stays bounded.
pub fn spawn_local_bridge(config: ServerConfig) -> LocalIpcBridge {
    let (client_to_server_tx, client_to_server_rx) = mpsc::channel(COMMAND_CHANNEL_CAPACITY);
    let (client_tx, client_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
    let (raw_tx, forward) = event_forward_channel(client_tx);
    let conn = Connection::with_forward(raw_tx, client_to_server_rx, forward);
    tokio::spawn(async move {
        if let Err(error) = Server::new(config).serve(conn).await {
            tracing::warn!("api gateway ipc bridge closed: {error}");
        }
    });
    LocalIpcBridge {
        command_tx: client_to_server_tx,
        event_rx: client_rx,
    }
}

pub async fn serve(config: ServerConfig, options: GatewayOptions) -> Result<(), GatewayError> {
    ensure_loopback(options.bind_addr)?;
    let listener = TcpListener::bind(options.bind_addr).await?;
    serve_listener(config, options, listener).await
}

pub async fn serve_listener(
    config: ServerConfig,
    options: GatewayOptions,
    listener: TcpListener,
) -> Result<(), GatewayError> {
    ensure_loopback(listener.local_addr()?)?;
    let connection_limit = Arc::new(Semaphore::new(options.max_connections.max(1)));
    loop {
        let permit = connection_limit
            .clone()
            .acquire_owned()
            .await
            .expect("API connection semaphore is never closed");
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            // A transient accept error (e.g. EMFILE under fd pressure)
            // must not tear down the whole listener — log and keep
            // serving, matching the Unix-socket service loop.
            Err(error) => {
                drop(permit);
                tracing::warn!("api gateway accept failed: {error}");
                continue;
            }
        };
        let config = config.clone();
        let options = options.clone();
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(error) = serve_connection(config, options, stream).await {
                tracing::warn!("api gateway connection failed: {error}");
            }
        });
    }
}

fn ensure_loopback(address: SocketAddr) -> Result<(), GatewayError> {
    if address.ip().is_loopback() {
        Ok(())
    } else {
        Err(GatewayError::NonLoopback(address))
    }
}

async fn serve_connection(
    config: ServerConfig,
    options: GatewayOptions,
    stream: TcpStream,
) -> Result<(), GatewayError> {
    let io = TokioIo::new(stream);
    hyper::server::conn::http1::Builder::new()
        .serve_connection(
            io,
            service_fn(move |request| {
                let config = config.clone();
                let options = options.clone();
                async move { Ok::<_, Infallible>(handle_request(config, options, request).await) }
            }),
        )
        .await?;
    Ok(())
}

pub async fn handle_request<B>(
    config: ServerConfig,
    options: GatewayOptions,
    request: Request<B>,
) -> Response<Body>
where
    B: HttpBody<Data = Bytes> + Send + Unpin + 'static,
    B::Error: Display + Send + Sync + 'static,
{
    if request.method() == Method::GET && request.uri().path() == "/" {
        return api_client_response();
    }

    if !check_bearer_token(
        request.headers().get(AUTHORIZATION),
        options.bearer_token.as_deref(),
    ) {
        return json_response(
            StatusCode::UNAUTHORIZED,
            &serde_json::json!({ "error": "unauthorized" }),
        );
    }

    if let Some(requested) = request.headers().get(PROTOCOL_VERSION_HEADER)
        && requested.as_bytes() != DESKTOP_PROTOCOL_VERSION.to_string().as_bytes()
    {
        return json_response(
            StatusCode::UPGRADE_REQUIRED,
            &UnsupportedProtocolResponse {
                error: format!(
                    "unsupported lazybox protocol version {}; this daemon supports version {}",
                    requested.to_str().unwrap_or("<non-UTF-8>"),
                    DESKTOP_PROTOCOL_VERSION
                ),
                requested: requested.to_str().unwrap_or("<non-UTF-8>").to_string(),
                supported: DESKTOP_PROTOCOL_VERSION,
            },
        );
    }
    if let Some(requested) = request.headers().get(PROTOCOL_FINGERPRINT_HEADER)
        && requested.as_bytes() != DESKTOP_PROTOCOL_FINGERPRINT.to_string().as_bytes()
    {
        return json_response(
            StatusCode::UPGRADE_REQUIRED,
            &UnsupportedFingerprintResponse {
                error: format!(
                    "unsupported lazybox protocol fingerprint {}; this daemon supports {}",
                    requested.to_str().unwrap_or("<non-UTF-8>"),
                    DESKTOP_PROTOCOL_FINGERPRINT
                ),
                requested: requested.to_str().unwrap_or("<non-UTF-8>").to_string(),
                supported: DESKTOP_PROTOCOL_FINGERPRINT,
            },
        );
    }

    match (request.method(), request.uri().path()) {
        (&Method::GET, "/v1/health") => json_response(StatusCode::OK, &health_response()),
        (&Method::GET, "/v1/protocol") => json_response(StatusCode::OK, &protocol_response()),
        (&Method::GET, "/v1/metrics") => json_response(StatusCode::OK, &metrics_response(&config)),
        (&Method::GET, "/v1/workspaces") => match workspaces_response(&config).await {
            Ok(payload) => json_response(StatusCode::OK, &payload),
            Err(error) => json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &serde_json::json!({ "error": error.to_string() }),
            ),
        },
        (&Method::GET, "/v1/events") => stream_events_response(config),
        (&Method::POST, "/v1/commands") => {
            command_response(config, &options, request.into_body()).await
        }
        (&Method::POST, "/v1/stream") => stream_command_response(config, request.into_body()),
        (&Method::POST, "/v1/terminal") => terminal_stream_response(config, request.into_body()),
        _ => json_response(
            StatusCode::NOT_FOUND,
            &serde_json::json!({ "error": "not found" }),
        ),
    }
}

async fn command_response<B>(
    config: ServerConfig,
    options: &GatewayOptions,
    body: B,
) -> Response<Body>
where
    B: HttpBody<Data = Bytes> + Send + Unpin + 'static,
    B::Error: Display + Send + Sync + 'static,
{
    let bytes = match collect_command_body(body).await {
        Ok(bytes) => bytes,
        Err(error) => {
            return json_response(error.status, &serde_json::json!({ "error": error.message }));
        }
    };
    let command = match decode_command_frame(&bytes) {
        Ok(command) => command,
        Err(error) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                &serde_json::json!({ "error": format!("decode command frame: {error}") }),
            );
        }
    };
    if matches!(command, Command::Subscribe | Command::Shutdown) {
        return json_response(
            StatusCode::BAD_REQUEST,
            &serde_json::json!({
                "error": "Subscribe and Shutdown are not valid one-shot API commands"
            }),
        );
    }
    if is_binary_terminal_command(&command) {
        return json_response(
            StatusCode::BAD_REQUEST,
            &serde_json::json!({
                "error": "terminal input, resize, resync, close, and scrollback commands must use /v1/terminal"
            }),
        );
    }

    // Execute and await the handler itself. Previously this endpoint returned
    // 200 as soon as an unbounded channel accepted the command, then dropped
    // the bridge; a slow mutation could be abandoned after the success reply.
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let event_tx = lazybox_ipc::EventSender::from_unbounded(event_tx);
    let mut task = tokio::spawn(async move {
        crate::dispatch_command(&config, &event_tx, command).await;
    });
    match tokio::time::timeout(options.command_timeout, &mut task).await {
        Ok(Ok(())) => {
            let mut events = Vec::new();
            while let Ok(event) = event_rx.try_recv() {
                events.push(event);
            }
            json_response(
                StatusCode::OK,
                &CommandResponse {
                    ok: true,
                    completed: true,
                    events,
                },
            )
        }
        Ok(Err(error)) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &serde_json::json!({ "error": format!("command handler failed: {error}") }),
        ),
        Err(_) => {
            task.abort();
            json_response(
                StatusCode::GATEWAY_TIMEOUT,
                &serde_json::json!({ "error": "command handler timed out" }),
            )
        }
    }
}

const MAX_COMMAND_BODY_BYTES: usize = lazybox_ipc::MAX_COMMAND_FRAME_BYTES as usize;

struct BodyReadError {
    status: StatusCode,
    message: String,
}

async fn collect_command_body<B>(mut body: B) -> Result<Bytes, BodyReadError>
where
    B: HttpBody<Data = Bytes> + Send + Unpin + 'static,
    B::Error: Display + Send + Sync + 'static,
{
    let mut bytes = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|error| BodyReadError {
            status: StatusCode::BAD_REQUEST,
            message: format!("read request body: {error}"),
        })?;
        let Ok(data) = frame.into_data() else {
            continue;
        };
        if bytes.len().saturating_add(data.len()) > MAX_COMMAND_BODY_BYTES {
            return Err(BodyReadError {
                status: StatusCode::PAYLOAD_TOO_LARGE,
                message: format!("command body exceeds the {MAX_COMMAND_BODY_BYTES}-byte limit"),
            });
        }
        bytes.extend_from_slice(&data);
    }
    Ok(Bytes::from(bytes))
}

fn stream_events_response(config: ServerConfig) -> Response<Body> {
    let bridge = spawn_local_bridge(config);
    let keepalive_tx = bridge.command_tx.clone();
    let _ = bridge.command_tx.try_send(Command::Subscribe);
    ndjson_event_response(bridge.event_rx, Some(keepalive_tx))
}

fn stream_command_response<B>(config: ServerConfig, body: B) -> Response<Body>
where
    B: HttpBody<Data = Bytes> + Send + Unpin + 'static,
    B::Error: Display + Send + Sync + 'static,
{
    let bridge = spawn_local_bridge(config);
    let command_tx = bridge.command_tx.clone();
    tokio::spawn(async move {
        pump_ndjson_commands(body, command_tx).await;
    });
    ndjson_event_response(bridge.event_rx, Some(bridge.command_tx))
}

fn ndjson_event_response(
    mut event_rx: mpsc::Receiver<Event>,
    keepalive_tx: Option<mpsc::Sender<Command>>,
) -> Response<Body> {
    let (mut tx, body) = Channel::<Bytes, Infallible>::new(32);
    tokio::spawn(async move {
        let _keepalive_tx = keepalive_tx;
        while let Some(event) = event_rx.recv().await {
            let Some(event) = control_event(event) else {
                continue;
            };
            let frame = JsonServerFrame::Event(event);
            let mut bytes = match serde_json::to_vec(&frame) {
                Ok(bytes) => bytes,
                Err(error) => {
                    tracing::warn!("api gateway: serialize event frame: {error}");
                    continue;
                }
            };
            bytes.push(b'\n');
            if tx.send_data(Bytes::from(bytes)).await.is_err() {
                break;
            }
        }
    });
    response_with_body(StatusCode::OK, "application/x-ndjson", body.boxed_unsync())
}

pub(crate) fn control_event(mut event: Event) -> Option<Event> {
    match &mut event {
        Event::Snapshot { terminals, .. } => {
            for terminal in terminals {
                terminal.replay.clear();
                terminal.replay_available = false;
            }
            Some(event)
        }
        Event::TerminalOutput { .. }
        | Event::TerminalResync { .. }
        | Event::TerminalScrollback { .. } => None,
        _ => Some(event),
    }
}

fn terminal_stream_response<B>(config: ServerConfig, body: B) -> Response<Body>
where
    B: HttpBody<Data = Bytes> + Send + Unpin + 'static,
    B::Error: Display + Send + Sync + 'static,
{
    let bridge = spawn_local_bridge(config);
    let command_tx = bridge.command_tx.clone();
    let _ = command_tx.try_send(Command::Subscribe);
    tokio::spawn(async move {
        pump_terminal_commands(body, command_tx).await;
    });
    binary_terminal_response(bridge.event_rx, bridge.command_tx)
}

fn binary_terminal_response(
    mut event_rx: mpsc::Receiver<Event>,
    keepalive_tx: mpsc::Sender<Command>,
) -> Response<Body> {
    let (mut tx, body) = Channel::<Bytes, Infallible>::new(32);
    tokio::spawn(async move {
        let _keepalive_tx = keepalive_tx;
        while let Some(event) = event_rx.recv().await {
            for frame in encode_terminal_event(&event) {
                if tx.send_data(Bytes::from(frame)).await.is_err() {
                    return;
                }
            }
        }
    });
    response_with_body(
        StatusCode::OK,
        TERMINAL_BINARY_CONTENT_TYPE,
        body.boxed_unsync(),
    )
}

pub fn encode_terminal_event(event: &Event) -> Vec<Vec<u8>> {
    match event {
        Event::Snapshot { terminals, .. } => terminals
            .iter()
            .filter(|terminal| terminal.replay_available)
            .map(|terminal| {
                encode_terminal_server_frame(
                    TERMINAL_SERVER_FRAME_SNAPSHOT,
                    terminal.terminal_id,
                    0,
                    terminal.last_seq,
                    &terminal.replay,
                )
            })
            .collect(),
        Event::TerminalOutput {
            terminal_id,
            bytes,
            first_seq,
            seq,
        } => vec![encode_terminal_server_frame(
            TERMINAL_SERVER_FRAME_OUTPUT,
            *terminal_id,
            *first_seq,
            *seq,
            bytes,
        )],
        Event::TerminalResync {
            terminal_id,
            replay,
            seq,
        } => vec![encode_terminal_server_frame(
            TERMINAL_SERVER_FRAME_RESYNC,
            *terminal_id,
            0,
            *seq,
            replay,
        )],
        Event::TerminalScrollback {
            terminal_id,
            replay,
            seq,
        } => vec![encode_terminal_server_frame(
            TERMINAL_SERVER_FRAME_SCROLLBACK,
            *terminal_id,
            0,
            *seq,
            replay,
        )],
        Event::TerminalResyncUnavailable { terminal_id } => {
            vec![encode_terminal_server_frame(
                TERMINAL_SERVER_FRAME_RESYNC_UNAVAILABLE,
                *terminal_id,
                0,
                0,
                &[],
            )]
        }
        _ => Vec::new(),
    }
}

fn encode_terminal_server_frame(
    kind: u8,
    terminal_id: TerminalId,
    first_seq: u64,
    seq: u64,
    payload: &[u8],
) -> Vec<u8> {
    let body_len = TERMINAL_SERVER_FRAME_HEADER_BYTES + payload.len();
    let mut frame = Vec::with_capacity(TERMINAL_FRAME_LENGTH_PREFIX_BYTES + body_len);
    frame.extend_from_slice(&(body_len as u32).to_be_bytes());
    frame.push(kind);
    frame.extend_from_slice(&terminal_id.0.to_be_bytes());
    frame.extend_from_slice(&first_seq.to_be_bytes());
    frame.extend_from_slice(&seq.to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

pub fn encode_terminal_command(command: &Command) -> Option<Vec<u8>> {
    let (kind, terminal_id, tail) = match command {
        Command::Write {
            terminal_id,
            bytes,
            intent,
        } => {
            let mut tail = Vec::with_capacity(TERMINAL_WRITE_BYTES_OFFSET + bytes.len());
            tail.push(match intent {
                lazybox_ipc::TerminalInputIntent::Compose => TERMINAL_WRITE_INTENT_COMPOSE,
                lazybox_ipc::TerminalInputIntent::Submit => TERMINAL_WRITE_INTENT_SUBMIT,
                lazybox_ipc::TerminalInputIntent::View => TERMINAL_WRITE_INTENT_VIEW,
            });
            tail.extend_from_slice(bytes);
            (TERMINAL_CLIENT_COMMAND_WRITE, *terminal_id, tail)
        }
        Command::Resize {
            terminal_id,
            cols,
            rows,
        } => {
            let mut tail = Vec::with_capacity(TERMINAL_RESIZE_PAYLOAD_BYTES);
            tail.extend_from_slice(&cols.to_be_bytes());
            tail.extend_from_slice(&rows.to_be_bytes());
            (TERMINAL_CLIENT_COMMAND_RESIZE, *terminal_id, tail)
        }
        Command::RequestTerminalResync {
            terminal_id,
            required_seq,
        } => (
            TERMINAL_CLIENT_COMMAND_RESYNC,
            *terminal_id,
            required_seq.to_be_bytes().to_vec(),
        ),
        Command::Close {
            terminal_id,
            client_request_id: None,
        } => (TERMINAL_CLIENT_COMMAND_CLOSE, *terminal_id, Vec::new()),
        Command::FetchScrollback { terminal_id } => (
            TERMINAL_CLIENT_COMMAND_FETCH_SCROLLBACK,
            *terminal_id,
            Vec::new(),
        ),
        _ => return None,
    };
    let body_len = TERMINAL_CLIENT_FRAME_HEADER_BYTES + tail.len();
    let mut frame = Vec::with_capacity(TERMINAL_FRAME_LENGTH_PREFIX_BYTES + body_len);
    frame.extend_from_slice(&(body_len as u32).to_be_bytes());
    frame.push(kind);
    frame.extend_from_slice(&terminal_id.0.to_be_bytes());
    frame.extend_from_slice(&tail);
    Some(frame)
}

pub(crate) async fn pump_terminal_commands<B>(mut body: B, command_tx: mpsc::Sender<Command>)
where
    B: HttpBody<Data = Bytes> + Send + Unpin + 'static,
    B::Error: Display + Send + Sync + 'static,
{
    let mut buffer = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = match frame {
            Ok(frame) => frame,
            Err(error) => {
                tracing::warn!("api gateway: read terminal command stream: {error}");
                return;
            }
        };
        let Ok(data) = frame.into_data() else {
            continue;
        };
        let mut remaining = data.as_ref();
        loop {
            if buffer.len() < TERMINAL_FRAME_LENGTH_PREFIX_BYTES {
                let take = (TERMINAL_FRAME_LENGTH_PREFIX_BYTES - buffer.len()).min(remaining.len());
                buffer.extend_from_slice(&remaining[..take]);
                remaining = &remaining[take..];
                if buffer.len() < TERMINAL_FRAME_LENGTH_PREFIX_BYTES {
                    break;
                }
            }
            let body_len = u32::from_be_bytes(
                buffer[..TERMINAL_FRAME_LENGTH_PREFIX_BYTES]
                    .try_into()
                    .expect("four-byte length"),
            ) as usize;
            if body_len > lazybox_ipc::MAX_COMMAND_FRAME_BYTES as usize {
                tracing::warn!("api gateway: terminal command frame exceeded its limit");
                return;
            }
            let frame_len = TERMINAL_FRAME_LENGTH_PREFIX_BYTES + body_len;
            let take = (frame_len - buffer.len()).min(remaining.len());
            buffer.extend_from_slice(&remaining[..take]);
            remaining = &remaining[take..];
            if buffer.len() < frame_len {
                break;
            }
            match decode_terminal_command(&buffer[TERMINAL_FRAME_LENGTH_PREFIX_BYTES..frame_len]) {
                Ok(command) => {
                    if command_tx.send(command).await.is_err() {
                        return;
                    }
                }
                Err(error) => {
                    tracing::warn!("api gateway: decode terminal command: {error}");
                    return;
                }
            }
            buffer.clear();
            if remaining.is_empty() {
                break;
            }
        }
    }
    if !buffer.is_empty() {
        tracing::warn!("api gateway: terminal command stream ended with an incomplete frame");
    }
}

pub(crate) fn decode_terminal_command(body: &[u8]) -> Result<Command, &'static str> {
    if body.len() < TERMINAL_CLIENT_FRAME_HEADER_BYTES {
        return Err("frame is shorter than its header");
    }
    let kind = body[TERMINAL_CLIENT_BODY_KIND_OFFSET];
    let terminal_id = TerminalId(u64::from_be_bytes(
        body[TERMINAL_CLIENT_BODY_TERMINAL_ID_OFFSET..TERMINAL_CLIENT_BODY_PAYLOAD_OFFSET]
            .try_into()
            .expect("eight-byte terminal id"),
    ));
    let tail = &body[TERMINAL_CLIENT_BODY_PAYLOAD_OFFSET..];
    match kind {
        TERMINAL_CLIENT_COMMAND_WRITE
            if (TERMINAL_WRITE_BYTES_OFFSET
                ..=lazybox_ipc::MAX_WRITE_CHUNK_BYTES + TERMINAL_WRITE_BYTES_OFFSET)
                .contains(&tail.len()) =>
        {
            let intent = match tail[TERMINAL_WRITE_INTENT_OFFSET] {
                TERMINAL_WRITE_INTENT_COMPOSE => lazybox_ipc::TerminalInputIntent::Compose,
                TERMINAL_WRITE_INTENT_SUBMIT => lazybox_ipc::TerminalInputIntent::Submit,
                TERMINAL_WRITE_INTENT_VIEW => lazybox_ipc::TerminalInputIntent::View,
                _ => return Err("write intent is invalid"),
            };
            Ok(Command::Write {
                terminal_id,
                bytes: tail[TERMINAL_WRITE_BYTES_OFFSET..].to_vec(),
                intent,
            })
        }
        TERMINAL_CLIENT_COMMAND_WRITE if tail.is_empty() => Err("write intent is missing"),
        TERMINAL_CLIENT_COMMAND_WRITE => Err("write payload exceeds its limit"),
        TERMINAL_CLIENT_COMMAND_RESIZE if tail.len() == TERMINAL_RESIZE_PAYLOAD_BYTES => {
            Ok(Command::Resize {
                terminal_id,
                cols: u16::from_be_bytes(
                    tail[TERMINAL_RESIZE_COLS_OFFSET..TERMINAL_RESIZE_ROWS_OFFSET]
                        .try_into()
                        .expect("two-byte cols"),
                ),
                rows: u16::from_be_bytes(
                    tail[TERMINAL_RESIZE_ROWS_OFFSET..TERMINAL_RESIZE_PAYLOAD_BYTES]
                        .try_into()
                        .expect("two-byte rows"),
                ),
            })
        }
        TERMINAL_CLIENT_COMMAND_RESYNC if tail.len() == TERMINAL_RESYNC_PAYLOAD_BYTES => {
            Ok(Command::RequestTerminalResync {
                terminal_id,
                required_seq: u64::from_be_bytes(
                    tail[TERMINAL_RESYNC_REQUIRED_SEQ_OFFSET..]
                        .try_into()
                        .expect("eight-byte required sequence"),
                ),
            })
        }
        TERMINAL_CLIENT_COMMAND_CLOSE if tail.is_empty() => Ok(Command::Close {
            terminal_id,
            client_request_id: None,
        }),
        TERMINAL_CLIENT_COMMAND_FETCH_SCROLLBACK if tail.is_empty() => {
            Ok(Command::FetchScrollback { terminal_id })
        }
        _ => Err("unknown terminal command or invalid payload length"),
    }
}

/// Ceiling on one ndjson command line. A client that streams an
/// unterminated line would otherwise grow `buffer` without bound; no
/// legitimate `Command` comes anywhere near this.
const MAX_COMMAND_LINE_BYTES: usize = lazybox_ipc::MAX_COMMAND_FRAME_BYTES as usize;
/// Bound the amount of work one duplex request can enqueue before reconnecting.
const MAX_STREAM_COMMANDS: usize = 256;

pub(crate) async fn pump_ndjson_commands<B>(mut body: B, command_tx: mpsc::Sender<Command>)
where
    B: HttpBody<Data = Bytes> + Send + Unpin + 'static,
    B::Error: Display + Send + Sync + 'static,
{
    let mut buffer = Vec::new();
    let mut command_lines_seen = 0usize;
    while let Some(frame) = body.frame().await {
        let frame = match frame {
            Ok(frame) => frame,
            Err(error) => {
                tracing::warn!("api gateway: read stream command frame: {error}");
                return;
            }
        };
        let Ok(data) = frame.into_data() else {
            continue;
        };
        // Consume one line segment at a time instead of copying the whole
        // HTTP frame into `buffer`. One giant frame containing many small,
        // valid lines remains valid, while one giant unterminated line is
        // rejected before a second giant allocation is made.
        let mut remaining = data.as_ref();
        while let Some(pos) = remaining.iter().position(|byte| *byte == b'\n') {
            let (piece, rest) = remaining.split_at(pos + 1);
            if buffer.len().saturating_add(piece.len()) > MAX_COMMAND_LINE_BYTES {
                tracing::warn!(
                    buffered = buffer.len(),
                    incoming = piece.len(),
                    "api gateway: ndjson command line exceeded {MAX_COMMAND_LINE_BYTES} bytes — dropping connection",
                );
                let _ = command_tx.send(Command::Shutdown).await;
                return;
            }
            buffer.extend_from_slice(piece);
            if !trim_ascii(&buffer).is_empty() {
                command_lines_seen += 1;
                send_command_line(&buffer, &command_tx).await;
            }
            buffer.clear();
            // Count malformed non-empty lines too. Otherwise a hostile peer
            // could evade the work cap by streaming invalid JSON forever.
            if command_lines_seen >= MAX_STREAM_COMMANDS {
                tracing::warn!(
                    "api gateway: stream reached {MAX_STREAM_COMMANDS} commands — reconnect required"
                );
                let _ = command_tx.send(Command::Shutdown).await;
                return;
            }
            remaining = rest;
        }
        if buffer.len().saturating_add(remaining.len()) > MAX_COMMAND_LINE_BYTES {
            tracing::warn!(
                buffered = buffer.len(),
                incoming = remaining.len(),
                "api gateway: ndjson command line exceeded {MAX_COMMAND_LINE_BYTES} bytes — dropping connection",
            );
            let _ = command_tx.send(Command::Shutdown).await;
            return;
        }
        buffer.extend_from_slice(remaining);
    }
    if !buffer.iter().all(u8::is_ascii_whitespace) {
        send_command_line(&buffer, &command_tx).await;
    }
}

async fn send_command_line(line: &[u8], command_tx: &mpsc::Sender<Command>) {
    let trimmed = trim_ascii(line);
    if trimmed.is_empty() {
        return;
    }
    match decode_command_frame(trimmed) {
        Ok(command) if is_binary_terminal_command(&command) => {
            tracing::warn!(
                "api gateway: terminal command rejected from JSON stream; use /v1/terminal"
            );
        }
        Ok(command) => {
            if command_tx.send(command).await.is_err() {
                tracing::warn!("api gateway: command stream closed");
            }
        }
        Err(error) => {
            tracing::warn!("api gateway: decode command stream line: {error}");
        }
    }
}

fn is_binary_terminal_command(command: &Command) -> bool {
    matches!(
        command,
        Command::Write { .. }
            | Command::Resize { .. }
            | Command::RequestTerminalResync { .. }
            | Command::Close { .. }
            | Command::FetchScrollback { .. }
    )
}

fn decode_command_frame(bytes: &[u8]) -> serde_json::Result<Command> {
    let value: serde_json::Value = serde_json::from_slice(bytes)?;
    if let Ok(frame) = serde_json::from_value::<JsonClientFrame>(value.clone()) {
        match frame {
            JsonClientFrame::Command(command) => return Ok(command),
        }
    }
    serde_json::from_value::<Command>(value)
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map(|idx| idx + 1)
        .unwrap_or(start);
    &bytes[start..end]
}

fn json_response<T: Serialize + ?Sized>(status: StatusCode, payload: &T) -> Response<Body> {
    match serde_json::to_vec(payload) {
        Ok(bytes) => response_with_body(
            status,
            "application/json",
            Full::new(Bytes::from(bytes)).boxed_unsync(),
        ),
        Err(error) => response_with_body(
            StatusCode::INTERNAL_SERVER_ERROR,
            "application/json",
            Full::new(Bytes::from(format!(
                "{{\"error\":\"json serialization failed: {error}\"}}"
            )))
            .boxed_unsync(),
        ),
    }
}

fn api_client_response() -> Response<Body> {
    let mut response = response_with_body(
        StatusCode::OK,
        "text/html; charset=utf-8",
        Full::new(Bytes::from_static(API_CLIENT_HTML.as_bytes())).boxed_unsync(),
    );
    let headers = response.headers_mut();
    headers.insert(
        "cache-control",
        HeaderValue::from_static("no-store, max-age=0"),
    );
    headers.insert(
        "content-security-policy",
        HeaderValue::from_static(
            "default-src 'none'; connect-src 'self'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
        ),
    );
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response
}

fn response_with_body(
    status: StatusCode,
    content_type: &'static str,
    body: Body,
) -> Response<Body> {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}

#[allow(dead_code)]
fn _assert_http_body(_: Incoming) {}
