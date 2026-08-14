//! The metering reverse-proxy (#1062, #1109).
//!
//! Agents point their `ANTHROPIC_BASE_URL` / `OPENAI_BASE_URL` at a
//! per-agent URL on this loopback proxy; it forwards each request to the
//! real upstream verbatim, streams the response straight back, and tees
//! that stream through the `usage_parse` module to recover the token
//! counts. The
//! recovered usage is handed to a [`UsageSink`] — the daemon wires that to
//! `Event::AgentSessionUsage`, which is the *only* usage source for
//! interactive terminal agents (they drive the real CLI in a PTY and emit
//! no structured `AgentUsage`) and the one that finally captures Codex
//! (#1109).
//!
//! Attribution rides the URL path: the injected base URL carries
//! `/<provider>/<agent-id>`, so the proxy knows which agent a request
//! belongs to and which upstream to forward it to without inspecting the
//! body. The proxy is otherwise transparent — it copies auth headers
//! through untouched and never buffers a streaming response, so the
//! agent's own credentials and incremental output are unaffected.

mod usage_parse;

use std::convert::Infallible;
use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{HeaderMap, HeaderName};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use lazybox_agents::LlmProvider;
use lazybox_ipc::AgentUsage;
use tokio::net::{TcpListener, TcpStream};

pub use usage_parse::UsageAccumulator;

/// The loopback port the running proxy bound, published once at startup so
/// the spawn path can point agents at it. Process-global because there is
/// exactly one proxy per daemon and the spawn code loads config on demand
/// (it has no runtime handle to thread the port through).
static PROXY_PORT: std::sync::OnceLock<u16> = std::sync::OnceLock::new();

/// Record the port the proxy bound. Idempotent — a second call is ignored.
pub fn set_port(port: u16) {
    let _ = PROXY_PORT.set(port);
}

/// The proxy's port once it is serving, or `None` before it starts (or
/// when metering is disabled and it never does).
pub fn port() -> Option<u16> {
    PROXY_PORT.get().copied()
}

/// Callback invoked once per upstream response that carried usage, with
/// the agent id parsed from the request path.
pub type UsageSink = Arc<dyn Fn(&str, AgentUsage) + Send + Sync>;

type BoxErr = Box<dyn std::error::Error + Send + Sync>;
type ProxyBody = BoxBody<Bytes, BoxErr>;

/// The real upstream base URLs each provider forwards to. Ordinarily the
/// provider defaults, but a configured `agent.llm_gateway_url` chains
/// through here so the proxy meters traffic that still terminates at the
/// user's own gateway.
#[derive(Debug, Clone)]
pub struct Upstreams {
    pub anthropic: String,
    pub openai: String,
}

impl Default for Upstreams {
    fn default() -> Self {
        Self {
            anthropic: "https://api.anthropic.com".to_string(),
            openai: "https://api.openai.com".to_string(),
        }
    }
}

impl Upstreams {
    fn base_for(&self, provider: &str) -> Option<&str> {
        match provider {
            "anthropic" => Some(&self.anthropic),
            "openai" => Some(&self.openai),
            _ => None,
        }
    }
}

/// The URL-path segment naming a provider on the proxy. Kept in one place
/// so the injected base URL and the request-path parse can't drift.
fn provider_segment(provider: LlmProvider) -> &'static str {
    match provider {
        LlmProvider::Anthropic => "anthropic",
        LlmProvider::OpenAI => "openai",
    }
}

/// The base URL to hand an agent so its traffic routes through the proxy
/// and is attributed to `agent_id`. The agent CLI appends its own path
/// (`/v1/messages`, `/v1/responses`, …) to this.
pub fn injected_base_url(port: u16, provider: LlmProvider, agent_id: &str) -> String {
    format!(
        "http://127.0.0.1:{port}/{}/{}",
        provider_segment(provider),
        agent_id
    )
}

struct ProxyState {
    client: reqwest::Client,
    upstreams: Upstreams,
    sink: UsageSink,
}

/// Start the metering proxy when `agent.metering_proxy` is on: bind a
/// loopback port, publish it for the spawn path, and serve until the
/// daemon exits, emitting `Event::AgentSessionUsage` for every metered
/// response. Returns the serving task, or `None` when metering is off or
/// the bind fails (in which case agents just reach the vendor directly —
/// metering is best-effort, never a spawn blocker).
pub async fn spawn(config: &crate::ServerConfig) -> Option<tokio::task::JoinHandle<()>> {
    let cfg = lazybox_config::Config::load().unwrap_or_default();
    if !cfg.agent.metering_proxy {
        return None;
    }

    let listener = match TcpListener::bind(("127.0.0.1", 0)).await {
        Ok(listener) => listener,
        Err(error) => {
            tracing::warn!("metering proxy failed to bind: {error}");
            return None;
        }
    };
    let port = match listener.local_addr() {
        Ok(addr) => addr.port(),
        Err(error) => {
            tracing::warn!("metering proxy local_addr failed: {error}");
            return None;
        }
    };
    set_port(port);

    // Chain through a configured gateway when one is set, so the proxy
    // meters traffic that still terminates at the user's own endpoint.
    let upstreams = match cfg.agent.gateway_url() {
        Some(url) => Upstreams {
            anthropic: url.to_string(),
            openai: url.to_string(),
        },
        None => Upstreams::default(),
    };

    let bus = config.bus.clone();
    let sink: UsageSink = Arc::new(move |agent_id: &str, usage| {
        let _ = bus.send(lazybox_ipc::Event::AgentSessionUsage {
            agent_id: agent_id.to_string(),
            usage,
        });
    });

    tracing::info!("metering proxy listening on 127.0.0.1:{port}");
    Some(tokio::spawn(serve(listener, upstreams, sink)))
}

/// Serve the proxy on an already-bound loopback listener until the process
/// exits. Errors on individual connections are logged, not fatal.
pub async fn serve(listener: TcpListener, upstreams: Upstreams, sink: UsageSink) {
    let state = Arc::new(ProxyState {
        client: reqwest::Client::new(),
        upstreams,
        sink,
    });
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(error) => {
                tracing::warn!("metering proxy accept failed: {error}");
                continue;
            }
        };
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(error) = serve_connection(state, stream).await {
                tracing::debug!("metering proxy connection ended: {error}");
            }
        });
    }
}

async fn serve_connection(state: Arc<ProxyState>, stream: TcpStream) -> Result<(), BoxErr> {
    let io = TokioIo::new(stream);
    hyper::server::conn::http1::Builder::new()
        .serve_connection(
            io,
            service_fn(move |request| {
                let state = state.clone();
                async move { Ok::<_, Infallible>(handle(state, request).await) }
            }),
        )
        .await?;
    Ok(())
}

/// Split a proxied request path into `(provider, agent_id, upstream_path)`.
/// The injected base URL contributes the leading `/<provider>/<agent>`;
/// everything after is the agent CLI's own path, forwarded unchanged.
fn split_path(path: &str) -> Option<(&str, &str, String)> {
    let rest = path.strip_prefix('/')?;
    let (provider, rest) = rest.split_once('/')?;
    if provider.is_empty() {
        return None;
    }
    let (agent, tail) = match rest.split_once('/') {
        Some((agent, tail)) => (agent, format!("/{tail}")),
        None => (rest, String::new()),
    };
    if agent.is_empty() {
        return None;
    }
    Some((provider, agent, tail))
}

/// Headers that describe a single hop and must not be forwarded across the
/// proxy, plus `host`/`content-length` which the outbound client sets
/// itself from the new request.
fn is_dropped_header(name: &HeaderName) -> bool {
    const DROP: &[&str] = &[
        "connection",
        "proxy-connection",
        "keep-alive",
        "transfer-encoding",
        "te",
        "trailer",
        "upgrade",
        "host",
        "content-length",
        // Not hop-by-hop, but stripped from the forwarded request so the
        // upstream returns an identity-encoded body. Without this, an
        // agent's `Accept-Encoding: gzip` would yield a compressed
        // response that streams back fine but is opaque to the usage
        // parser — usage would be silently lost (this crate builds reqwest
        // without its decompression features on purpose). Harmless on the
        // response side, where the header never appears.
        "accept-encoding",
    ];
    DROP.contains(&name.as_str())
}

fn forwarded_headers(src: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::with_capacity(src.len());
    for (name, value) in src {
        if !is_dropped_header(name) {
            out.append(name.clone(), value.clone());
        }
    }
    out
}

fn error_response(status: StatusCode, message: &str) -> Response<ProxyBody> {
    let body = Full::new(Bytes::from(message.to_owned()))
        .map_err(|never: Infallible| match never {})
        .boxed();
    Response::builder()
        .status(status)
        .body(body)
        .unwrap_or_else(|_| Response::new(empty_body()))
}

fn empty_body() -> ProxyBody {
    Full::new(Bytes::new())
        .map_err(|never: Infallible| match never {})
        .boxed()
}

async fn handle(state: Arc<ProxyState>, request: Request<Incoming>) -> Response<ProxyBody> {
    let (parts, body) = request.into_parts();

    let Some((provider, agent_id, upstream_path)) = split_path(parts.uri.path()) else {
        return error_response(StatusCode::NOT_FOUND, "proxy: malformed metering path");
    };
    let Some(base) = state.upstreams.base_for(provider) else {
        return error_response(StatusCode::NOT_FOUND, "proxy: unknown provider");
    };
    let agent_id = agent_id.to_string();

    let mut url = format!("{}{}", base.trim_end_matches('/'), upstream_path);
    if let Some(query) = parts.uri.query() {
        url.push('?');
        url.push_str(query);
    }

    let body_bytes = match body.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(_) => {
            return error_response(StatusCode::BAD_GATEWAY, "proxy: request body read failed");
        }
    };

    let upstream = state
        .client
        .request(parts.method.clone(), url.as_str())
        .headers(forwarded_headers(&parts.headers))
        .body(body_bytes)
        .send()
        .await;
    let upstream = match upstream {
        Ok(response) => response,
        Err(error) => {
            tracing::debug!("metering proxy upstream error for {url}: {error}");
            return error_response(StatusCode::BAD_GATEWAY, "proxy: upstream request failed");
        }
    };

    let status = upstream.status();
    let mut builder = Response::builder().status(status);
    if let Some(headers) = builder.headers_mut() {
        *headers = forwarded_headers(upstream.headers());
    }

    // Tee the response to the client while feeding the usage parser. The
    // sink fires exactly once, when the upstream stream ends cleanly —
    // partial/aborted responses (an error frame) never report a total.
    let sink = state.sink.clone();
    let stream = futures::stream::unfold(
        (
            upstream.bytes_stream(),
            UsageAccumulator::default(),
            Some((sink, agent_id)),
        ),
        |(mut bytes, mut acc, mut pending)| async move {
            match bytes.next().await {
                Some(Ok(chunk)) => {
                    acc.push(&chunk);
                    Some((Ok(Frame::data(chunk)), (bytes, acc, pending)))
                }
                Some(Err(error)) => {
                    pending = None;
                    Some((Err(BoxErr::from(error)), (bytes, acc, pending)))
                }
                None => {
                    if let Some((sink, agent_id)) = pending.take()
                        && let Some(usage) = acc.finish()
                    {
                        sink(&agent_id, usage);
                    }
                    None
                }
            }
        },
    );

    let body = BodyExt::boxed(StreamBody::new(stream));
    match builder.body(body) {
        Ok(response) => response,
        Err(_) => error_response(StatusCode::BAD_GATEWAY, "proxy: response build failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_path_extracts_provider_agent_and_tail() {
        let (provider, agent, tail) = split_path("/anthropic/claude/v1/messages").unwrap();
        assert_eq!(provider, "anthropic");
        assert_eq!(agent, "claude");
        assert_eq!(tail, "/v1/messages");
    }

    #[test]
    fn split_path_handles_a_bare_prefix() {
        let (provider, agent, tail) = split_path("/openai/codex").unwrap();
        assert_eq!(provider, "openai");
        assert_eq!(agent, "codex");
        assert_eq!(tail, "");
    }

    #[test]
    fn split_path_rejects_incomplete_prefixes() {
        assert!(split_path("/anthropic").is_none());
        assert!(split_path("/anthropic/").is_none());
        assert!(split_path("/").is_none());
    }

    #[test]
    fn injected_base_url_round_trips_through_split() {
        let url = injected_base_url(7777, LlmProvider::OpenAI, "codex");
        assert_eq!(url, "http://127.0.0.1:7777/openai/codex");
        let path = url.strip_prefix("http://127.0.0.1:7777").unwrap();
        let (provider, agent, _) = split_path(path).unwrap();
        assert_eq!((provider, agent), ("openai", "codex"));
    }

    #[test]
    fn hop_by_hop_headers_are_dropped_but_auth_survives() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer secret".parse().unwrap());
        headers.insert("host", "127.0.0.1".parse().unwrap());
        headers.insert("connection", "keep-alive".parse().unwrap());
        headers.insert("accept-encoding", "gzip, br".parse().unwrap());
        headers.insert("content-type", "application/json".parse().unwrap());
        let forwarded = forwarded_headers(&headers);
        assert!(forwarded.contains_key("authorization"));
        assert!(forwarded.contains_key("content-type"));
        assert!(!forwarded.contains_key("host"));
        assert!(!forwarded.contains_key("connection"));
        // Stripped so the upstream returns a body the usage parser can read.
        assert!(!forwarded.contains_key("accept-encoding"));
    }
}
