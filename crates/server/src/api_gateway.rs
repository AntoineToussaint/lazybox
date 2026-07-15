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
use lazybox_ipc::{Command, Connection, EVENT_CHANNEL_CAPACITY, Event, EventForward};
use lazybox_store::StoreError;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::fmt::Display;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, mpsc};

pub type Body = UnsyncBoxBody<Bytes, Infallible>;

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
            max_connections: 64,
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
}

impl From<StoreError> for GatewayError {
    fn from(value: StoreError) -> Self {
        Self::Store(value.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthResponse {
    pub ok: bool,
    pub service: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspacesResponse {
    pub workspaces: Vec<lazybox_core::Workspace>,
    /// Persisted rows that were preserved but could not be decoded.
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandResponse {
    pub ok: bool,
    /// Set only after the daemon-side handler has returned.
    pub completed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum JsonClientFrame {
    Command(Command),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum JsonServerFrame {
    Event(Event),
}

pub struct LocalIpcBridge {
    pub command_tx: mpsc::UnboundedSender<Command>,
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
/// raw unbounded stream, and `Server::serve` spawns the drop-and-resync
/// forwarder ([`crate::event_forward::forward_events`]) that bridges it
/// into the bounded `event_rx`. The resync semantics translate directly
/// to ndjson — a slow consumer sees `TerminalResync` frames instead of
/// every output chunk, and the daemon's memory stays bounded.
pub fn spawn_local_bridge(config: ServerConfig) -> LocalIpcBridge {
    let (client_to_server_tx, client_to_server_rx) = mpsc::unbounded_channel();
    let (raw_tx, raw_rx) = mpsc::unbounded_channel();
    let (client_tx, client_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
    let conn = Connection::with_forward(
        raw_tx,
        client_to_server_rx,
        EventForward { raw_rx, client_tx },
    );
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
    let listener = TcpListener::bind(options.bind_addr).await?;
    serve_listener(config, options, listener).await
}

pub async fn serve_listener(
    config: ServerConfig,
    options: GatewayOptions,
    listener: TcpListener,
) -> Result<(), GatewayError> {
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
    if !check_bearer_token(
        request.headers().get(AUTHORIZATION),
        options.bearer_token.as_deref(),
    ) {
        return json_response(
            StatusCode::UNAUTHORIZED,
            &serde_json::json!({ "error": "unauthorized" }),
        );
    }

    match (request.method(), request.uri().path()) {
        (&Method::GET, "/v1/health") => json_response(StatusCode::OK, &health_response()),
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

    // Execute and await the handler itself. Previously this endpoint returned
    // 200 as soon as an unbounded channel accepted the command, then dropped
    // the bridge; a slow mutation could be abandoned after the success reply.
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let mut task = tokio::spawn(async move {
        crate::dispatch_command(&config, &event_tx, command).await;
    });
    match tokio::time::timeout(options.command_timeout, &mut task).await {
        Ok(Ok(())) => json_response(
            StatusCode::OK,
            &CommandResponse {
                ok: true,
                completed: true,
            },
        ),
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

const MAX_COMMAND_BODY_BYTES: usize = 1024 * 1024;

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
    let _ = bridge.command_tx.send(Command::Subscribe);
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
    keepalive_tx: Option<mpsc::UnboundedSender<Command>>,
) -> Response<Body> {
    let (mut tx, body) = Channel::<Bytes, Infallible>::new(32);
    tokio::spawn(async move {
        let _keepalive_tx = keepalive_tx;
        while let Some(event) = event_rx.recv().await {
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

/// Ceiling on one ndjson command line. A client that streams an
/// unterminated line would otherwise grow `buffer` without bound; no
/// legitimate `Command` comes anywhere near this.
const MAX_COMMAND_LINE_BYTES: usize = 1024 * 1024;
/// Bound the amount of work one duplex request can enqueue before reconnecting.
const MAX_STREAM_COMMANDS: usize = 256;

async fn pump_ndjson_commands<B>(mut body: B, command_tx: mpsc::UnboundedSender<Command>)
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
        buffer.extend_from_slice(&data);
        while let Some(pos) = buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = buffer.drain(..=pos).collect();
            if trim_ascii(&line).is_empty() {
                continue;
            }
            command_lines_seen += 1;
            send_command_line(&line, &command_tx);
            // Count malformed non-empty lines too. Otherwise a hostile peer
            // could evade the work cap by streaming invalid JSON forever.
            if command_lines_seen >= MAX_STREAM_COMMANDS {
                tracing::warn!(
                    "api gateway: stream reached {MAX_STREAM_COMMANDS} commands — reconnect required"
                );
                let _ = command_tx.send(Command::Shutdown);
                return;
            }
        }
        if buffer.len() > MAX_COMMAND_LINE_BYTES {
            // Drop the whole connection, not just the line: a peer
            // sending megabytes without a newline is broken or hostile,
            // and resynchronizing mid-stream would misparse whatever
            // follows. `Shutdown` ends this bridge's serve loop, which
            // closes the event stream and tears the connection down.
            tracing::warn!(
                buffered = buffer.len(),
                "api gateway: ndjson command line exceeded {MAX_COMMAND_LINE_BYTES} bytes — dropping connection",
            );
            let _ = command_tx.send(Command::Shutdown);
            return;
        }
    }
    if !buffer.iter().all(u8::is_ascii_whitespace) {
        send_command_line(&buffer, &command_tx);
    }
}

fn send_command_line(line: &[u8], command_tx: &mpsc::UnboundedSender<Command>) {
    let trimmed = trim_ascii(line);
    if trimmed.is_empty() {
        return;
    }
    match decode_command_frame(trimmed) {
        Ok(command) => {
            if command_tx.send(command).is_err() {
                tracing::warn!("api gateway: command stream closed");
            }
        }
        Err(error) => {
            tracing::warn!("api gateway: decode command stream line: {error}");
        }
    }
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
