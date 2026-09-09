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

mod quota_parse;
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
use lazybox_ipc::{AgentUsage, ProviderQuota};
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

/// kv key holding the last loopback port, reused on restart so a metered
/// agent that survived the restart keeps resolving its baked
/// `ANTHROPIC_BASE_URL` / `OPENAI_BASE_URL`.
const PORT_KV_KEY: &str = "proxy:port";

/// Bind the proxy's loopback listener, **reusing the port persisted by the
/// previous daemon** when it is free (falling back to an ephemeral one with a
/// warning), and persist whatever was bound for the next restart.
///
/// The port is baked into every metered agent's `*_BASE_URL` environment at
/// spawn, and a lazybox restart keeps those agent processes alive (session
/// recovery) — so an ephemeral port per daemon left every surviving metered
/// agent dialing a dead port: "connection refused", retry loop, session
/// stuck until respawn. Mirrors the MCP endpoint, which bakes its URL the
/// same way and solved this the same way (#1420).
pub(crate) async fn bind_listener(config: &crate::ServerConfig) -> Option<(TcpListener, u16)> {
    let prior = restore_port(config).await;
    let listener = match crate::mcp::bind_loopback(prior) {
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
    persist_port(config, port).await;
    Some((listener, port))
}

async fn persist_port(config: &crate::ServerConfig, port: u16) {
    let value = port.to_string();
    if let Err(error) = crate::store_blocking(&config.store, move |store| {
        store.set_kv(PORT_KV_KEY, &value)
    })
    .await
    {
        tracing::warn!("metering proxy: persist port: {error}");
    }
}

async fn restore_port(config: &crate::ServerConfig) -> Option<u16> {
    match crate::store_blocking(&config.store, |store| store.get_kv(PORT_KV_KEY)).await {
        Ok(Some(raw)) => raw.trim().parse().ok(),
        _ => None,
    }
}

/// Callback invoked once per upstream response that carried usage, with the
/// agent id and session key parsed from the request path (session is `""`
/// when the spawn opted in without a resolvable key).
pub type UsageSink = Arc<dyn Fn(&str, &str, AgentUsage) + Send + Sync>;

/// Callback invoked when a response carried provider plan-quota (Anthropic's
/// unified rate-limit headers), with the agent id and session key from the
/// request path — the "can I keep working?" signal, distinct from
/// [`UsageSink`]'s token counts.
pub type QuotaSink = Arc<dyn Fn(&str, &str, ProviderQuota) + Send + Sync>;

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
    /// The ChatGPT-subscription backend Codex talks to when logged in with a
    /// ChatGPT account rather than an API key. A request under the `openai`
    /// segment carrying a `chatgpt-account-id` header routes here instead of
    /// `openai`, and is metered count-only ($0) — a subscription is a flat fee.
    pub chatgpt: String,
}

impl Default for Upstreams {
    fn default() -> Self {
        Self {
            anthropic: "https://api.anthropic.com".to_string(),
            // Codex's custom-provider `base_url` appends `/responses` (not
            // `/v1/responses`), so the `/v1` lives here on the upstream.
            openai: "https://api.openai.com/v1".to_string(),
            chatgpt: "https://chatgpt.com/backend-api/codex".to_string(),
        }
    }
}

/// A resolved upstream: where to forward, and whether to meter count-only.
struct Route<'a> {
    base: &'a str,
    count_only: bool,
}

/// Resolve which upstream a request forwards to, and whether its tokens are
/// count-only. Codex uses a single `openai` segment for both auth modes; the
/// `chatgpt-account-id` request header — which Codex attaches only in
/// ChatGPT-subscription mode — is what splits them, so post-spawn logins are
/// handled without any spawn-time auth detection.
fn resolve_route<'a>(
    provider: &str,
    headers: &HeaderMap,
    upstreams: &'a Upstreams,
) -> Option<Route<'a>> {
    match provider {
        "anthropic" => Some(Route {
            base: &upstreams.anthropic,
            count_only: false,
        }),
        "openai" if headers.contains_key("chatgpt-account-id") => Some(Route {
            base: &upstreams.chatgpt,
            count_only: true,
        }),
        "openai" => Some(Route {
            base: &upstreams.openai,
            count_only: false,
        }),
        _ => None,
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
pub fn injected_base_url(
    port: u16,
    provider: LlmProvider,
    agent_id: &str,
    session: &str,
) -> String {
    format!(
        "http://127.0.0.1:{port}/{}/{}/{}",
        provider_segment(provider),
        agent_id,
        session,
    )
}

struct ProxyState {
    client: reqwest::Client,
    upstreams: Upstreams,
    sink: UsageSink,
    quota_sink: QuotaSink,
    /// Per-model price overrides (`agent.pricing`), layered over the built-in
    /// rate card when pricing a response's tokens.
    prices: usage_parse::PriceOverrides,
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

    let (listener, port) = bind_listener(config).await?;
    set_port(port);

    // Chain through a configured gateway when one is set, so the proxy
    // meters traffic that still terminates at the user's own endpoint.
    //
    // Path contract: the proxy forwards each provider's wire path appended to
    // its upstream, and the two agents append DIFFERENT prefixes — Claude adds
    // `/v1/messages`, Codex adds only `/responses` (no `/v1`; that is why the
    // vendor default bakes `/v1` onto `openai` but not `anthropic`). A single
    // `gateway_url` fronting both therefore receives `<url>/v1/messages` for
    // Claude and `<url>/responses` for Codex, so the gateway must accept BOTH
    // shapes. In particular, an OpenAI-compatible gateway that expects
    // `/v1/responses` needs the `/v1` included in the configured URL — the
    // proxy does not add it here (the vendor default's `/v1` is OpenAI's real
    // host path, not something to presume onto an arbitrary gateway, and many
    // gateway URLs already carry their own `/v1`).
    let upstreams = match cfg.agent.gateway_url() {
        Some(url) => Upstreams {
            anthropic: url.to_string(),
            openai: url.to_string(),
            // ChatGPT-subscription requests are authenticated by the user's
            // ChatGPT account session (`chatgpt-account-id` + an OAuth session
            // token) that ONLY chatgpt.com/backend-api/codex can validate — a
            // generic API gateway can't service them (it would 401/403). So
            // subscription traffic stays pinned to the vendor backend even when
            // a gateway is configured; the gateway only fronts API-key traffic.
            chatgpt: Upstreams::default().chatgpt,
        },
        None => Upstreams::default(),
    };

    let bus = config.bus.clone();
    let sink: UsageSink = Arc::new(move |agent_id: &str, session: &str, usage| {
        let _ = bus.send(lazybox_ipc::Event::AgentSessionUsage {
            agent_id: agent_id.to_string(),
            session_key: session_key_opt(session),
            usage,
        });
    });
    let quota_bus = config.bus.clone();
    let quota_sink: QuotaSink = Arc::new(move |agent_id: &str, session: &str, quota| {
        let _ = quota_bus.send(lazybox_ipc::Event::AgentProviderQuota {
            agent_id: agent_id.to_string(),
            session_key: session_key_opt(session),
            quota,
        });
    });

    let prices: usage_parse::PriceOverrides = Arc::new(cfg.agent.pricing.clone());

    tracing::info!("metering proxy listening on 127.0.0.1:{port}");
    Some(tokio::spawn(serve(
        listener, upstreams, sink, quota_sink, prices,
    )))
}

/// The session key parsed from a proxy path, as an `Option` — an empty
/// segment (a metered spawn with no resolvable key) becomes `None`.
fn session_key_opt(session: &str) -> Option<lazybox_core::SessionKey> {
    (!session.is_empty()).then(|| lazybox_core::SessionKey::new(session))
}

/// Serve the proxy on an already-bound loopback listener until the process
/// exits. Errors on individual connections are logged, not fatal.
pub async fn serve(
    listener: TcpListener,
    upstreams: Upstreams,
    sink: UsageSink,
    quota_sink: QuotaSink,
    prices: usage_parse::PriceOverrides,
) {
    let state = Arc::new(ProxyState {
        client: reqwest::Client::new(),
        upstreams,
        sink,
        quota_sink,
        prices,
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

/// Split a proxied request path into `(provider, agent_id, session, upstream_path)`.
/// The injected base URL contributes the leading `/<provider>/<agent>/<session>`
/// (`session` attributes usage to one workspace, #per-session); everything after
/// is the agent CLI's own path, forwarded unchanged. `session` may be empty
/// (a spawn that opted in without a resolvable key) but provider and agent
/// must be present.
fn split_path(path: &str) -> Option<(&str, &str, &str, String)> {
    let rest = path.strip_prefix('/')?;
    let (provider, rest) = rest.split_once('/')?;
    if provider.is_empty() {
        return None;
    }
    let (agent, rest) = rest.split_once('/')?;
    if agent.is_empty() {
        return None;
    }
    let (session, tail) = match rest.split_once('/') {
        Some((session, tail)) => (session, format!("/{tail}")),
        None => (rest, String::new()),
    };
    Some((provider, agent, session, tail))
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

    let Some((provider, agent_id, session, upstream_path)) = split_path(parts.uri.path()) else {
        return error_response(StatusCode::NOT_FOUND, "proxy: malformed metering path");
    };
    let Some(Route { base, count_only }) =
        resolve_route(provider, &parts.headers, &state.upstreams)
    else {
        return error_response(StatusCode::NOT_FOUND, "proxy: unknown provider");
    };
    let agent_id = agent_id.to_string();
    let session = session.to_string();

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

    // Plan-quota ("can I keep working?") rides Anthropic's unified
    // rate-limit *headers*, so it is read here — synchronously, before the
    // body streams — and attributed to the agent whose request surfaced it.
    // OpenAI/Codex doesn't expose it on headers, so this fires for Anthropic
    // only; Codex quota is sourced from its session log elsewhere.
    if provider == "anthropic" {
        let quota = quota_parse::parse_anthropic_headers(upstream.headers());
        if !quota.is_empty() {
            (state.quota_sink)(&agent_id, &session, quota);
        }
    }

    let mut builder = Response::builder().status(status);
    if let Some(headers) = builder.headers_mut() {
        *headers = forwarded_headers(upstream.headers());
    }

    // Tee the response to the client while feeding the usage parser. The
    // sink fires exactly once, when the upstream stream ends cleanly —
    // partial/aborted responses (an error frame) never report a total.
    //
    // Known small under-count (#1389): a response the client aborts, or that
    // errors mid-stream, drops the tokens it did consume. Capturing those
    // partials would mean pricing an incomplete `usage` block the upstream
    // never finalized (and often never sent) — deliberately not done, since a
    // wrong partial is worse than a missing one for a cost meter. The gap is
    // bounded to interrupted turns and documented rather than guessed.
    let sink = state.sink.clone();
    let accumulator = {
        let acc = UsageAccumulator::with_prices(state.prices.clone());
        if count_only { acc.counting_only() } else { acc }
    };
    let stream = futures::stream::unfold(
        (
            upstream.bytes_stream(),
            accumulator,
            Some((sink, agent_id, session)),
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
                    if let Some((sink, agent_id, session)) = pending.take()
                        && let Some(usage) = acc.finish()
                    {
                        sink(&agent_id, &session, usage);
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

    /// The restart case: the proxy's port is persisted, and a fresh daemon
    /// (fresh listener, same store) binds the SAME port back — so a metered
    /// agent that survived the restart with `…BASE_URL=http://127.0.0.1:PORT`
    /// baked in keeps reaching the proxy instead of "connection refused".
    /// When the port is genuinely taken, it falls back to a fresh one and
    /// persists that instead, so the next restart converges again.
    #[tokio::test]
    async fn bind_listener_reuses_the_persisted_port_across_a_restart() {
        let config = crate::ServerConfig::in_memory();

        // The freed port is only ours to reclaim if nothing else on the host
        // takes it in the window between the drop and the rebind — and a
        // just-released ephemeral port is exactly what the next ephemeral
        // bind anywhere on the box is handed. `bind_listener` answers a lost
        // race by falling back to a fresh port, which is correct behaviour
        // but indistinguishable here from "reuse is broken", so retry on a
        // fresh port instead of asserting we win the race.
        let mut reclaimed = None;
        for _ in 0..16 {
            // First daemon: ephemeral port, now persisted.
            let (first, port) = bind_listener(&config).await.expect("first bind");
            assert_eq!(restore_port(&config).await, Some(port));
            drop(first);

            // Second daemon on the same store: same port comes back.
            let (second, reused) = bind_listener(&config).await.expect("rebind");
            if reused == port {
                reclaimed = Some((second, port));
                break;
            }
        }
        let (held, port) = reclaimed.expect("the persisted port is reused after a restart");
        assert_eq!(restore_port(&config).await, Some(port));

        // Port still held (the prior daemon didn't release it) → fresh port,
        // persisted so the next restart converges on it.
        let (_third, fallback) = bind_listener(&config).await.expect("fallback bind");
        assert_ne!(fallback, port, "a held port falls back to a fresh one");
        assert_eq!(restore_port(&config).await, Some(fallback));
        drop(held);
    }

    #[test]
    fn split_path_extracts_provider_agent_session_and_tail() {
        let (provider, agent, session, tail) =
            split_path("/anthropic/claude/github-acme-widget-7/v1/messages").unwrap();
        assert_eq!(provider, "anthropic");
        assert_eq!(agent, "claude");
        assert_eq!(session, "github-acme-widget-7");
        assert_eq!(tail, "/v1/messages");
    }

    #[test]
    fn split_path_handles_a_bare_prefix() {
        let (provider, agent, session, tail) = split_path("/openai/codex/sess-1").unwrap();
        assert_eq!(provider, "openai");
        assert_eq!(agent, "codex");
        assert_eq!(session, "sess-1");
        assert_eq!(tail, "");
    }

    #[test]
    fn split_path_rejects_incomplete_prefixes() {
        // Provider and agent are mandatory; a session-less prefix is malformed.
        assert!(split_path("/anthropic").is_none());
        assert!(split_path("/anthropic/").is_none());
        assert!(split_path("/anthropic/claude").is_none());
        assert!(split_path("/").is_none());
    }

    #[test]
    fn injected_base_url_round_trips_through_split() {
        let url = injected_base_url(7777, LlmProvider::OpenAI, "codex", "github-acme-widget-7");
        assert_eq!(
            url,
            "http://127.0.0.1:7777/openai/codex/github-acme-widget-7"
        );
        let path = url.strip_prefix("http://127.0.0.1:7777").unwrap();
        let (provider, agent, session, _) = split_path(path).unwrap();
        assert_eq!(
            (provider, agent, session),
            ("openai", "codex", "github-acme-widget-7")
        );
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

    #[test]
    fn resolve_route_openai_splits_on_chatgpt_account_header() {
        let ups = Upstreams::default();

        // API-key Codex: no ChatGPT header → OpenAI, priced.
        let bare = HeaderMap::new();
        let r = resolve_route("openai", &bare, &ups).expect("route");
        assert_eq!(r.base, "https://api.openai.com/v1");
        assert!(!r.count_only, "api-key traffic is priced");

        // ChatGPT-subscription Codex: header present → ChatGPT backend, $0.
        let mut sub = HeaderMap::new();
        sub.insert("chatgpt-account-id", "acct-123".parse().unwrap());
        let r = resolve_route("openai", &sub, &ups).expect("route");
        assert_eq!(r.base, "https://chatgpt.com/backend-api/codex");
        assert!(r.count_only, "a subscription is a flat fee → count-only");
    }

    #[test]
    fn resolve_route_anthropic_is_always_priced_and_unknown_is_none() {
        let ups = Upstreams::default();
        let mut headers = HeaderMap::new();
        // Even with the header set, anthropic never routes to the ChatGPT
        // backend — the header only means something under the openai segment.
        headers.insert("chatgpt-account-id", "acct-123".parse().unwrap());
        let r = resolve_route("anthropic", &headers, &ups).expect("route");
        assert_eq!(r.base, "https://api.anthropic.com");
        assert!(!r.count_only);

        assert!(resolve_route("gemini", &headers, &ups).is_none());
    }

    #[test]
    fn openai_upstream_carries_v1_so_codex_responses_path_is_correct() {
        // Codex's custom provider appends `/responses` to base_url, and the
        // proxy forwards `{openai}/responses` — which must land at
        // `/v1/responses`, so the `/v1` has to live on the upstream default.
        assert_eq!(Upstreams::default().openai, "https://api.openai.com/v1");
    }
}
