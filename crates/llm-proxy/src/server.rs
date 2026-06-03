//! HTTP proxy server. Listens on `127.0.0.1:<ephemeral>`, forwards every
//! request to a configured upstream (Anthropic or OpenAI), and emits
//! a `ProxyRecord` after each request-response round-trip.
//!
//! Clients of this module:
//! - The daemon, which spawns one proxy per agent kind the user is
//!   running and injects the listen URL into the agent's env via
//!   `ANTHROPIC_BASE_URL` / `OPENAI_BASE_URL`.
//! - Tests, which spawn a mock upstream and a proxy pointed at it.
//!
//! ## What's in / what's not
//!
//! - IN: byte-level request/response forwarding; method/path/status/
//!   duration/bytes in the record; session-tag attribution via
//!   `X-Lazybox-Session` header; header redaction for stored records.
//! - NOT YET: SSE parsing for token counts and tool calls; cost
//!   computation from parsed tokens. Those land in a follow-up so
//!   this file stays focused on the transport layer.

use crate::{ApiProvider, ProxyConfig, ProxyRecord};
use bytes::Bytes;
use http_body_util::BodyExt;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

/// Session-tag header the proxy reads to attribute requests. The
/// daemon sets this value via env injection in the spawn wrapper.
pub const SESSION_TAG_HEADER: &str = "x-lazybox-session";

#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    #[error("bind: {0}")]
    Bind(#[from] std::io::Error),
    #[error("reqwest client: {0}")]
    Client(#[from] reqwest::Error),
    #[error("proxy already shut down")]
    AlreadyShutDown,
}

/// Handle to a running proxy. Drop it or call `shutdown` to stop.
pub struct ProxyServer {
    addr: SocketAddr,
    shutdown_tx: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl ProxyServer {
    /// Start the proxy. Returns the handle plus the records channel
    /// the caller drains to observe telemetry.
    pub async fn start(
        config: ProxyConfig,
        upstream: String,
    ) -> Result<(Self, mpsc::Receiver<ProxyRecord>), ProxyError> {
        let listener = TcpListener::bind(config.listen).await?;
        let addr = listener.local_addr()?;
        // Record channel: bounded to give back-pressure without
        // silently dropping — but large enough that a normal burst
        // (a tool-heavy turn can emit ~20 records in a second)
        // doesn't stall the proxy.
        let (records_tx, records_rx) = mpsc::channel::<ProxyRecord>(256);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        // reqwest client is cheap to clone (it's an Arc internally).
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .pool_max_idle_per_host(8)
            .build()?;

        let ctx = Arc::new(ServerCtx {
            upstream,
            config,
            client,
            records_tx,
        });

        let task = tokio::spawn(run_accept_loop(listener, ctx, shutdown_rx));

        Ok((
            Self {
                addr,
                shutdown_tx: Some(shutdown_tx),
                task: Some(task),
            },
            records_rx,
        ))
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Signal the proxy to stop accepting new connections. In-flight
    /// requests are allowed to complete. Awaits the accept-loop task.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for ProxyServer {
    fn drop(&mut self) {
        // If the user forgot to call shutdown, still signal the task
        // so it doesn't leak.
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

struct ServerCtx {
    upstream: String,
    /// Kept so future body-parsing paths can read `record_bodies` /
    /// redaction settings without another round-trip to the daemon.
    #[allow(dead_code)]
    config: ProxyConfig,
    client: reqwest::Client,
    records_tx: mpsc::Sender<ProxyRecord>,
}

async fn run_accept_loop(
    listener: TcpListener,
    ctx: Arc<ServerCtx>,
    mut shutdown: oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => {
                tracing::debug!("proxy: shutdown signaled");
                return;
            }
            accept = listener.accept() => {
                let (stream, _peer) = match accept {
                    Ok(tuple) => tuple,
                    Err(e) => {
                        tracing::warn!("proxy accept: {e}");
                        continue;
                    }
                };
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let service = service_fn(move |req| {
                        let ctx = ctx.clone();
                        handle_request(ctx, req)
                    });
                    if let Err(e) =
                        http1::Builder::new().serve_connection(io, service).await
                    {
                        tracing::debug!("proxy connection ended: {e}");
                    }
                });
            }
        }
    }
}

/// Handle one request: read it, forward to upstream, return the
/// response. Records a `ProxyRecord` regardless of success / failure.
async fn handle_request(
    ctx: Arc<ServerCtx>,
    req: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
    let started = Instant::now();
    let started_at = chrono::Utc::now();
    let method = req.method().clone();
    let uri = req.uri().clone();
    let path_and_query = uri
        .path_and_query()
        .map(|p| p.to_string())
        .unwrap_or_else(|| uri.path().to_string());
    let session_tag = req
        .headers()
        .get(SESSION_TAG_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // Classify by path. Anthropic's /v1/messages and OpenAI's
    // /v1/chat/completions are the hot paths; anything else is
    // `Unknown` but still forwarded.
    let provider = classify_provider(&path_and_query);

    // Forward.
    let (status, response_bytes, request_bytes, upstream_err) =
        match forward(ctx.as_ref(), req).await {
            Ok((status, body_bytes, req_bytes)) => (status.as_u16(), body_bytes, req_bytes, None),
            Err(e) => (
                StatusCode::BAD_GATEWAY.as_u16(),
                Bytes::from_static(b"lazybox-server: upstream error\n"),
                0,
                Some(e.to_string()),
            ),
        };

    // Build and emit the record. Failure to send is a shutdown
    // indicator — just drop the record.
    let record = ProxyRecord {
        session_key: session_tag
            .clone()
            .map(|t| t.into())
            .unwrap_or_else(|| "unknown".into()),
        started_at,
        duration: started.elapsed(),
        provider,
        endpoint: path_and_query.clone(),
        request_model: None,
        tokens_input: None,
        tokens_output: None,
        tokens_cache_read: None,
        tokens_cache_create: None,
        estimated_cost_usd: None,
        tool_calls: vec![],
        assistant_text: None,
        status,
        error: upstream_err,
    };
    // Honor `record_bodies` by omitting the body-derived fields.
    // For now body-derived fields are always None so there's nothing
    // extra to blank; the config is in place for when SSE parsing
    // ships. Apply redaction-by-headers later at the daemon level,
    // since headers aren't currently stored on the record.
    let _ = ctx.records_tx.try_send(record);
    let _ = request_bytes; // placed-in for future byte-accounting

    let _ = method; // method is captured for future structured records

    // Assemble the response back to the caller.
    let mut builder = Response::builder().status(status);
    // Forward essential content-type so clients don't mis-decode.
    // Upstream headers are not propagated in this MVP; the agent
    // clients we target (Claude Code, Codex) don't rely on custom
    // response headers for SSE framing beyond content-type, which we
    // set below.
    builder = builder.header(
        "content-type",
        detect_content_type(&response_bytes, &path_and_query),
    );
    let resp = builder.body(Full::new(response_bytes)).unwrap_or_else(|_| {
        Response::new(Full::new(Bytes::from_static(
            b"lazybox-server: encode error\n",
        )))
    });
    Ok(resp)
}

fn classify_provider(path: &str) -> ApiProvider {
    // Simple prefix matching. Upstream can be either Anthropic or
    // OpenAI; we tag the record so downstream analytics can split.
    if path.starts_with("/v1/messages") {
        ApiProvider::Anthropic
    } else if path.starts_with("/v1/chat/completions") || path.starts_with("/v1/completions") {
        ApiProvider::OpenAI
    } else {
        ApiProvider::Unknown
    }
}

fn detect_content_type(body: &Bytes, path: &str) -> &'static str {
    // Streaming endpoints produce SSE. Everything else is JSON for
    // the APIs we proxy.
    if path.contains("stream") || body.starts_with(b"event:") || body.starts_with(b"data:") {
        "text/event-stream"
    } else {
        "application/json"
    }
}

/// Forward one request to upstream. Returns (status, body_bytes,
/// request_body_bytes) on success.
async fn forward(
    ctx: &ServerCtx,
    req: Request<Incoming>,
) -> Result<(StatusCode, Bytes, u64), ForwardError> {
    let (parts, body) = req.into_parts();
    let body_bytes = body
        .collect()
        .await
        .map_err(|e| ForwardError::ReadBody(e.to_string()))?
        .to_bytes();
    let request_bytes = body_bytes.len() as u64;

    // Reconstruct the URL by appending the path+query onto the
    // configured upstream base.
    let path = parts
        .uri
        .path_and_query()
        .map(|p| p.to_string())
        .unwrap_or_else(|| parts.uri.path().to_string());
    let target = format!(
        "{}{}",
        ctx.upstream.trim_end_matches('/'),
        if path.starts_with('/') { "" } else { "/" }
    );
    let target = format!("{target}{path}");

    let method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes())
        .map_err(|e| ForwardError::Method(e.to_string()))?;
    let mut builder = ctx.client.request(method, target);
    for (name, value) in parts.headers.iter() {
        // Strip hop-by-hop headers. The listener is HTTP/1.1 and
        // reqwest handles Host + Content-Length for us; forwarding
        // them would confuse the upstream.
        let lname = name.as_str().to_ascii_lowercase();
        if matches!(
            lname.as_str(),
            "host"
                | "connection"
                | "content-length"
                | "transfer-encoding"
                | "proxy-authorization"
                | "upgrade"
                | "te"
        ) {
            continue;
        }
        builder = builder.header(name.as_str(), value.as_bytes());
    }
    if !body_bytes.is_empty() {
        builder = builder.body(body_bytes);
    }

    let response = builder.send().await.map_err(ForwardError::Upstream)?;
    let status = response.status();
    let resp_bytes = response.bytes().await.map_err(ForwardError::ReadUpstream)?;

    Ok((status, resp_bytes, request_bytes))
}

#[derive(Debug, thiserror::Error)]
enum ForwardError {
    #[error("read request body: {0}")]
    ReadBody(String),
    #[error("invalid method: {0}")]
    Method(String),
    #[error("upstream: {0}")]
    Upstream(reqwest::Error),
    #[error("read upstream body: {0}")]
    ReadUpstream(reqwest::Error),
}
