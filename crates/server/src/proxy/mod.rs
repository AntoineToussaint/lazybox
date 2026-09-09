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

/// kv key holding the loopback ports metered agents have recently been
/// baked with, most-recent first. Reclaimed on restart so a metered agent
/// that survived it keeps resolving its `ANTHROPIC_BASE_URL` /
/// `OPENAI_BASE_URL` instead of retry-looping on a dead port.
///
/// A list rather than a single cell because it has **multiple writers**: the
/// standalone-daemon pid guard only skips the embedded *socket* bind, so
/// `lazybox server start` plus a `lazybox` TUI launch runs two daemons
/// against one store, each binding a proxy of its own. A cell let the second
/// daemon's (individually correct) fallback erase the first's live port, and
/// the first then restored the stale value on its next restart — #1616.
///
/// Appending instead of overwriting also removes the need to know *which*
/// daemon a remembered port belongs to. A restarted daemon cannot recognise
/// its own previous incarnation — role, pid file, and start order are all
/// guesses that invert when the other daemon stops first — so it doesn't
/// guess: it claims the first port in the list that will still bind. The
/// ports a living peer is serving are exactly the ones that refuse, which
/// makes the kernel, not a heuristic, the arbiter of ownership.
const PORTS_KV_KEY: &str = "proxy:ports";

/// The pre-#1616 single-port key, read once to seed the list so metered
/// agents spawned before the upgrade keep reaching the proxy across it.
const LEGACY_PORT_KV_KEY: &str = "proxy:port";

/// How many ports to remember. A daemon only takes a new one when its
/// previous port is contended at startup, so this spans far more restarts
/// than a metered agent survives.
const PORT_HISTORY: usize = 8;

/// Bind the proxy's loopback listener, **reclaiming a port a previous daemon
/// baked into live agents** when one is still free, and record whatever was
/// bound for the next restart.
///
/// The port is baked into every metered agent's `*_BASE_URL` environment at
/// spawn, and a lazybox restart keeps those agent processes alive (session
/// recovery) — so an ephemeral port per daemon left every surviving metered
/// agent dialing a dead port: "connection refused", retry loop, session
/// stuck until respawn. Mirrors the MCP endpoint, which bakes its URL the
/// same way and solved this the same way (#1420).
pub(crate) async fn bind_listener(config: &crate::ServerConfig) -> Option<(TcpListener, u16)> {
    let known = restore_ports(config).await;
    let bound = claim_known_port(&known).or_else(|| {
        if !known.is_empty() {
            tracing::warn!(
                ?known,
                "every remembered proxy port is taken — agents that survived the restart keep \
                 dialing theirs until respawn; binding a fresh port"
            );
        }
        bind_fresh()
    })?;
    let (listener, port) = bound;
    persist_port(config, port).await;
    Some((listener, port))
}

/// Take the most recent remembered port that still binds. A port that
/// refuses is being served by a peer daemon this store shares — its agents
/// depend on it, so it is skipped rather than contended for.
fn claim_known_port(known: &[u16]) -> Option<(TcpListener, u16)> {
    known
        .iter()
        .copied()
        .find_map(|port| match crate::mcp::bind_loopback_port(port) {
            Ok(listener) => Some((listener, port)),
            Err(error) => {
                tracing::debug!(port, %error, "remembered proxy port is held; trying the next");
                None
            }
        })
}

fn bind_fresh() -> Option<(TcpListener, u16)> {
    let listener = match crate::mcp::bind_loopback_port(0) {
        Ok(listener) => listener,
        Err(error) => {
            tracing::warn!("metering proxy failed to bind: {error}");
            return None;
        }
    };
    match listener.local_addr() {
        Ok(addr) => Some((listener, addr.port())),
        Err(error) => {
            tracing::warn!("metering proxy local_addr failed: {error}");
            None
        }
    }
}

async fn persist_port(config: &crate::ServerConfig, port: u16) {
    if let Err(error) = crate::store_blocking(&config.store, move |store| {
        // Re-read rather than reuse the list loaded before the bind: two
        // daemons starting together would otherwise each write back a list
        // missing the other's port, and a port dropped from the list is one
        // its daemon can no longer reclaim for its live agents.
        let mut ports = vec![port];
        ports.extend(read_ports(store).into_iter().filter(|known| *known != port));
        ports.truncate(PORT_HISTORY);
        let value = ports
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(",");
        store.set_kv(PORTS_KV_KEY, &value)
    })
    .await
    {
        tracing::warn!("metering proxy: persist port: {error}");
    }
}

async fn restore_ports(config: &crate::ServerConfig) -> Vec<u16> {
    crate::store_blocking(&config.store, read_ports).await
}

fn read_ports(store: &dyn lazybox_store::Store) -> Vec<u16> {
    if let Ok(Some(raw)) = store.get_kv(PORTS_KV_KEY) {
        return parse_ports(&raw);
    }
    match store.get_kv(LEGACY_PORT_KV_KEY) {
        Ok(Some(raw)) => parse_ports(&raw),
        _ => Vec::new(),
    }
}

fn parse_ports(raw: &str) -> Vec<u16> {
    raw.split(',')
        .filter_map(|port| port.trim().parse().ok())
        .filter(|port| *port != 0)
        .collect()
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

    /// A fresh install has nothing to reclaim: it takes an ephemeral port and
    /// remembers it, so the next restart has something to come back to.
    #[tokio::test]
    async fn a_fresh_install_binds_and_remembers_an_ephemeral_port() {
        let config = crate::ServerConfig::in_memory();
        let (_listener, port) = bind_listener(&config).await.expect("first bind");
        assert_eq!(restore_ports(&config).await, vec![port]);
    }

    /// The restart case (#1530): a fresh daemon on the same store binds the
    /// remembered port back, so a metered agent that survived the restart
    /// with `…BASE_URL=http://127.0.0.1:PORT` baked in keeps reaching the
    /// proxy instead of "connection refused". When that port is genuinely
    /// taken, it falls back to a fresh one and remembers it *beside* the one
    /// still in use rather than replacing it.
    #[tokio::test]
    async fn a_restart_reclaims_the_remembered_port_and_a_held_one_falls_back() {
        for attempt in 0..8 {
            let config = crate::ServerConfig::in_memory();
            let remembered = free_static_ports(22000 + attempt * 100, 1)[0];
            config
                .store
                .set_kv(PORTS_KV_KEY, &remembered.to_string())
                .expect("seed history");

            let (held, reused) = bind_listener(&config).await.expect("rebind");
            if reused != remembered {
                // Something on this host took the port in the window where it
                // was released; try a different one rather than assert we win
                // that race (#1602). A lucky draw cannot make a broken wiring
                // pass here the way it could for an ephemeral port: a
                // `bind_listener` that stopped reading the history binds an
                // ephemeral port, never one of these candidates.
                continue;
            }
            assert_eq!(
                restore_ports(&config).await,
                vec![remembered],
                "the reused port must be re-recorded so the next restart converges"
            );

            // Still held (a peer daemon never released it) → fresh port,
            // recorded in front of the one whose agents still depend on it.
            let (_next, fallback) = bind_listener(&config).await.expect("fallback bind");
            assert_ne!(
                fallback, remembered,
                "a held port falls back to a fresh one"
            );
            assert_eq!(restore_ports(&config).await, vec![fallback, remembered]);
            drop(held);
            return;
        }
        panic!(
            "the remembered port was never reclaimed — either bind_listener \
             stopped reading the history, or every candidate port was taken \
             by another process on this host"
        );
    }

    /// #1616, and the sequence that a per-daemon *slot* scheme got wrong: two
    /// daemons share a store, and whichever restarts first must come back on
    /// the port ITS live metered agents were baked with — including the
    /// second daemon restarting after the first has stopped, where any
    /// role-derived identity flips and hands it the other daemon's port.
    ///
    /// Nothing here identifies a daemon. Each simply claims the most recent
    /// remembered port that still binds, and a port a living peer is serving
    /// refuses to bind — so the kernel resolves ownership.
    #[tokio::test]
    async fn each_daemon_reclaims_its_own_port_when_the_other_restarts() {
        for attempt in 0..8 {
            let config = crate::ServerConfig::in_memory();
            // Static ports: an ephemeral one can be handed to a sibling test
            // in the window where this test releases it to simulate a
            // restart. A candidate lost anyway is retried, not asserted on.
            let ports = free_static_ports(23000 + attempt * 100, 2);
            config
                .store
                .set_kv(PORTS_KV_KEY, &format!("{},{}", ports[0], ports[1]))
                .expect("seed history");

            // Daemon A claims the front of the history; its agents are baked
            // with `a_port`. B starts beside it: A holds that port, so B
            // takes the next.
            let (a, a_port) = bind_listener(&config).await.expect("daemon A binds");
            let (b, b_port) = bind_listener(&config).await.expect("daemon B binds");
            if (a_port, b_port) != (ports[0], ports[1]) {
                continue;
            }
            assert_eq!(
                restore_ports(&config).await,
                vec![b_port, a_port],
                "a peer's bind appends to the history instead of erasing it"
            );

            // A stops, B restarts: B must reclaim `b_port`, NOT the port A's
            // agents are still baked with, even though `a_port` is free now.
            drop(a);
            drop(b);
            let (b_again, b_reclaimed) = bind_listener(&config).await.expect("B restarts");
            assert_eq!(b_reclaimed, b_port, "B comes back on its own port");

            // A restarts too and finds its own port waiting behind B's.
            let (_a_again, a_reclaimed) = bind_listener(&config).await.expect("A restarts");
            assert_eq!(a_reclaimed, a_port, "A comes back on its own port");
            drop(b_again);
            return;
        }
        panic!("every candidate port pair was taken by another process on this host");
    }

    /// The upgrade itself must not strand agents: a store written by the
    /// pre-#1616 code carries a single `proxy:port`, and the metered agents
    /// alive across the upgrade are baked with it. Seed the history from it
    /// so the first run of the new code reclaims it rather than binding fresh.
    #[tokio::test]
    async fn the_pre_1616_single_port_key_seeds_the_history() {
        for attempt in 0..8 {
            let config = crate::ServerConfig::in_memory();
            let legacy = free_static_ports(21000 + attempt * 100, 1)[0];
            config
                .store
                .set_kv(LEGACY_PORT_KV_KEY, &legacy.to_string())
                .expect("seed the legacy key");

            let (_listener, bound) = bind_listener(&config).await.expect("bind");
            if bound != legacy {
                continue;
            }
            assert_eq!(
                restore_ports(&config).await,
                vec![legacy],
                "the pre-upgrade port is carried into the history"
            );
            return;
        }
        panic!("every candidate port was taken by another process on this host");
    }

    /// Free ports from `base`, outside every platform's ephemeral range so a
    /// sibling test's ephemeral bind can never be handed one between a
    /// release here and the reclaim under test. Each caller scans its own
    /// range, so two tests running in parallel cannot pick the same port.
    /// The probes are held until all are found, then released together, so
    /// the ports are distinct.
    fn free_static_ports(base: u16, count: usize) -> Vec<u16> {
        let mut held = Vec::new();
        for port in base..base + 100 {
            if let Ok(listener) = crate::mcp::bind_loopback_port(port) {
                held.push((listener, port));
            }
            if held.len() == count {
                return held.iter().map(|(_, port)| *port).collect();
            }
        }
        panic!("no {count} free static ports from {base}");
    }

    #[test]
    fn the_port_history_is_bounded_and_ignores_junk() {
        assert_eq!(parse_ports("7777, 8888,9999"), vec![7777, 8888, 9999]);
        // A truncated / half-written value contributes what it can rather
        // than throwing away ports that live agents still depend on.
        assert_eq!(parse_ports("7777,,not-a-port,0,8888"), vec![7777, 8888]);
        assert!(parse_ports("").is_empty());
    }

    /// The history is capped, and re-binding a port already in it moves that
    /// port to the front instead of appending a duplicate — otherwise a few
    /// restarts would push a live agent's port off the end of the list.
    #[tokio::test]
    async fn rebinding_a_known_port_promotes_it_without_growing_the_history() {
        let config = crate::ServerConfig::in_memory();
        let history: Vec<String> = (1..=PORT_HISTORY + 2)
            .map(|n| (40000 + n as u16).to_string())
            .collect();
        config
            .store
            .set_kv(PORTS_KV_KEY, &history.join(","))
            .expect("seed history");

        let (_listener, bound) = bind_listener(&config).await.expect("bind");
        let stored = restore_ports(&config).await;
        assert_eq!(stored.first().copied(), Some(bound));
        assert_eq!(
            stored.iter().filter(|port| **port == bound).count(),
            1,
            "the reclaimed port is promoted, not duplicated"
        );
        assert!(stored.len() <= PORT_HISTORY);
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
