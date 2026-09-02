//! MCP coordination server — Phase 0 read loop (#1420).
//!
//! A daemon-hosted [Model Context Protocol](https://modelcontextprotocol.io)
//! server that lets one agent session discover and read another, across
//! repos, without the manual `/status` Session-ID dance. Phase 0 exposes
//! three read-only tools over what the daemon already tracks:
//!
//! - `whoami` — the calling session's identity.
//! - `list_sessions` — every live agent session.
//! - `read_session` — a cleaned tail of another session's output.
//!
//! Identity is **implicit from the connection**: each spawned agent carries a
//! per-session bearer token (minted at spawn, see the spawn wiring in a later
//! phase) that the daemon maps back to its [`SessionKey`] via
//! [`TokenRegistry`]. A tool reads that bearer from the request `Parts` rmcp
//! stashes in its [`RequestContext`], so no tool takes a "who am I" argument.
//!
//! The transport is streamable HTTP, served on a loopback listener via
//! `rmcp`'s tower `StreamableHttpService` wrapped onto the existing hyper
//! stack — no axum listener of our own. Loopback-only mirrors the JSON API
//! gateway's trust boundary; every tool additionally requires a *registered*
//! session token, so an unauthenticated caller on the loopback port gets
//! nothing.

use std::collections::HashMap;
use std::sync::Arc;

use lazybox_core::SessionKey;
use parking_lot::RwLock;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo};
use rmcp::service::RequestContext;
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, schemars, tool, tool_handler, tool_router,
};

use crate::ServerConfig;
use crate::api_gateway;

/// Maps a per-session bearer token to the [`SessionKey`] of the agent that
/// owns it. A token is registered when a session spawns and forgotten when it
/// ends, so a live token resolving to a key is proof the caller is that
/// session. Cheap-clone `Arc` interior so the registry can be shared with the
/// spawn path.
#[derive(Debug, Default)]
pub struct TokenRegistry {
    inner: RwLock<HashMap<String, SessionKey>>,
}

impl TokenRegistry {
    /// Bind `token` to `key`, replacing any prior binding for that token.
    pub fn register(&self, token: impl Into<String>, key: SessionKey) {
        self.inner.write().insert(token.into(), key);
    }

    /// Drop a token (session ended / respawned with a fresh token).
    pub fn forget(&self, token: &str) {
        self.inner.write().remove(token);
    }

    /// Drop every token bound to `key`. Called before minting a fresh token
    /// for a respawn so a session never accumulates stale tokens.
    pub fn forget_session(&self, key: &SessionKey) {
        self.inner.write().retain(|_, bound| bound != key);
    }

    /// Resolve a token to its session, if still registered.
    pub fn resolve(&self, token: &str) -> Option<SessionKey> {
        self.inner.read().get(token).cloned()
    }

    /// Snapshot the live `token → session-key-string` bindings for
    /// persistence, so a reattached agent's baked bearer survives a daemon
    /// restart (#1420).
    pub fn snapshot(&self) -> HashMap<String, String> {
        self.inner
            .read()
            .iter()
            .map(|(token, key)| (token.clone(), key.as_str().to_string()))
            .collect()
    }

    /// Merge restored `token → session` bindings into the live map without
    /// dropping any minted since boot. Used once at startup to rehydrate the
    /// registry from the persisted snapshot.
    pub fn restore_from(&self, entries: impl IntoIterator<Item = (String, SessionKey)>) {
        let mut inner = self.inner.write();
        for (token, key) in entries {
            inner.insert(token, key);
        }
    }

    /// Number of live tokens — for diagnostics/tests.
    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    /// Whether any token is registered.
    pub fn is_empty(&self) -> bool {
        self.inner.read().is_empty()
    }
}

/// Per-process MCP coordination state, held by [`ServerConfig::mcp`]: the
/// token → session registry plus the endpoint URL the listener binds. Both
/// the listener (resolving a tool caller) and the spawn path (registering a
/// token, writing the agent's config) read it through the shared `Arc`.
#[derive(Debug, Default)]
pub struct McpRuntime {
    tokens: TokenRegistry,
    /// Base URL of the MCP endpoint (`http://127.0.0.1:PORT/`) once
    /// [`start`] has bound the listener; `None` before then.
    endpoint: RwLock<Option<String>>,
}

impl McpRuntime {
    /// The token registry shared with the spawn path.
    pub fn tokens(&self) -> &TokenRegistry {
        &self.tokens
    }

    /// Record the bound endpoint URL (called once by [`start`]).
    pub fn set_endpoint(&self, url: String) {
        *self.endpoint.write() = Some(url);
    }

    /// The endpoint URL, if a listener has started.
    pub fn endpoint(&self) -> Option<String> {
        self.endpoint.read().clone()
    }
}

/// Extract a bearer token from an `Authorization` header value, tolerating the
/// scheme's canonical casing. Returns the raw token without the `Bearer `
/// prefix.
fn parse_bearer(header_value: &str) -> Option<&str> {
    let value = header_value.trim();
    let rest = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))?;
    let token = rest.trim();
    (!token.is_empty()).then_some(token)
}

/// Read the bearer token out of HTTP request parts. Split from
/// [`LazyboxMcp::bearer`] so the header→token path is unit-testable against a
/// real [`http::request::Parts`] without fabricating a `RequestContext` (whose
/// `Peer` is not constructible outside rmcp).
fn bearer_from_parts(parts: &http::request::Parts) -> Option<String> {
    let header = parts.headers.get(http::header::AUTHORIZATION)?;
    parse_bearer(header.to_str().ok()?).map(str::to_owned)
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ListSessionsArgs {
    /// Optional case-insensitive substring filter over workspace name or repo.
    #[serde(default)]
    filter: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ReadSessionArgs {
    /// Workspace key (also the target session's key string), as returned by
    /// `list_sessions`.
    workspace: String,
    /// Trailing lines of output to return (clamped to 1..=500; default 40).
    #[serde(default)]
    tail: Option<usize>,
}

/// The lazybox MCP handler. One instance per connection, all sharing the
/// process [`ServerConfig`] (Arc-backed, cheap to clone). Tool identity comes
/// from `config.mcp` (the [`McpRuntime`] token registry).
#[derive(Clone)]
pub struct LazyboxMcp {
    config: ServerConfig,
    tool_router: ToolRouter<LazyboxMcp>,
}

// The tool bodies are thin: they resolve the caller, then delegate to the
// `*_payload` inherent methods below, which take plain arguments so they are
// unit-testable without fabricating a `RequestContext`.
#[tool_router]
impl LazyboxMcp {
    pub fn new(config: ServerConfig) -> Self {
        Self {
            config,
            tool_router: Self::tool_router(),
        }
    }

    /// The bearer token on the current request, if any. rmcp's streamable-HTTP
    /// transport stashes the raw [`http::request::Parts`] in the tool
    /// `RequestContext` extensions (see its `tower` module), from which we read
    /// the `Authorization` header.
    fn bearer(ctx: &RequestContext<RoleServer>) -> Option<String> {
        bearer_from_parts(ctx.extensions.get::<http::request::Parts>()?)
    }

    /// Resolve the calling session, rejecting a missing or unknown token. Used
    /// by every tool so an unauthenticated loopback caller gets nothing.
    fn caller(&self, ctx: &RequestContext<RoleServer>) -> Result<SessionKey, McpError> {
        let token = Self::bearer(ctx)
            .ok_or_else(|| McpError::invalid_request("missing bearer token", None))?;
        self.config
            .mcp
            .tokens()
            .resolve(&token)
            .ok_or_else(|| McpError::invalid_request("unknown or expired session token", None))
    }

    /// Identity block for `key`, joining the live-agent snapshot when present.
    async fn whoami_payload(&self, key: &SessionKey) -> Result<serde_json::Value, McpError> {
        let resp = api_gateway::agents_response(&self.config)
            .await
            .map_err(|error| McpError::internal_error(format!("read agents: {error}"), None))?;
        let me = resp
            .agents
            .into_iter()
            .find(|agent| agent.workspace_key == key.as_str());
        Ok(serde_json::json!({
            "session_key": key.as_str(),
            "workspace_name": me.as_ref().map(|a| a.workspace_name.clone()),
            "repo": me.as_ref().and_then(|a| a.repo.clone()),
            "agent": me.as_ref().map(|a| a.agent.clone()),
            "state": me.as_ref().and_then(|a| a.state),
        }))
    }

    /// Every live agent session, optionally filtered by a case-insensitive
    /// substring over workspace name or repo.
    async fn list_sessions_payload(
        &self,
        filter: Option<&str>,
    ) -> Result<serde_json::Value, McpError> {
        let resp = api_gateway::agents_response(&self.config)
            .await
            .map_err(|error| McpError::internal_error(format!("read agents: {error}"), None))?;
        let needle = filter.map(str::to_lowercase);
        let sessions: Vec<_> = resp
            .agents
            .into_iter()
            .filter(|agent| match &needle {
                None => true,
                Some(needle) => {
                    agent.workspace_name.to_lowercase().contains(needle)
                        || agent
                            .repo
                            .as_deref()
                            .is_some_and(|repo| repo.to_lowercase().contains(needle))
                }
            })
            .map(|agent| {
                serde_json::json!({
                    "session_key": agent.workspace_key,
                    "workspace_name": agent.workspace_name,
                    "repo": agent.repo,
                    "agent": agent.agent,
                    "state": agent.state,
                    "last_prompt": agent.last_prompt,
                })
            })
            .collect();
        Ok(serde_json::json!({ "sessions": sessions }))
    }

    /// Cleaned tail of `workspace`'s running agent, or `None` when it has no
    /// live agent terminal.
    async fn read_session_text(&self, workspace: &str, tail: Option<usize>) -> Option<String> {
        let key = SessionKey::from(workspace);
        let terminal_id = self.config.terminal.running_agent_terminal(&key).await?;
        let max_lines = tail
            .unwrap_or(api_gateway::AGENT_OUTPUT_DEFAULT_LINES)
            .clamp(1, api_gateway::AGENT_OUTPUT_MAX_LINES);
        Some(
            crate::spawn_handler::agent_output_snapshot(&self.config, terminal_id, max_lines)
                .await
                .unwrap_or_default(),
        )
    }

    #[tool(
        description = "Identify the calling session: its session key, workspace, repo, and agent. Call this first to learn who you are before referencing other sessions."
    )]
    async fn whoami(&self, ctx: RequestContext<RoleServer>) -> Result<CallToolResult, McpError> {
        let key = self.caller(&ctx)?;
        Ok(json_result(self.whoami_payload(&key).await?))
    }

    #[tool(
        description = "List sibling agent sessions across all repos — workspace key, name, repo, agent, state, and its last prompt. The way to discover what other sessions exist and what each is working on."
    )]
    async fn list_sessions(
        &self,
        Parameters(args): Parameters<ListSessionsArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let _ = self.caller(&ctx)?;
        Ok(json_result(
            self.list_sessions_payload(args.filter.as_deref()).await?,
        ))
    }

    #[tool(
        description = "Read the recent terminal output of another session by its workspace key (from list_sessions). Returns a cleaned tail so you can see what that agent is doing right now."
    )]
    async fn read_session(
        &self,
        Parameters(args): Parameters<ReadSessionArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let _ = self.caller(&ctx)?;
        match self.read_session_text(&args.workspace, args.tail).await {
            Some(output) => Ok(CallToolResult::success(vec![ContentBlock::text(output)])),
            None => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "no running agent in workspace {}",
                args.workspace
            ))])),
        }
    }
}

/// Wrap a JSON value as a successful single-text tool result.
fn json_result(payload: serde_json::Value) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(
        serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string()),
    )])
}

// `router` defaults to `Self::tool_router()`, which rebuilds the router on
// every dispatch; point it at the instance we already built in `new` instead.
#[tool_handler(router = self.tool_router.clone())]
impl ServerHandler for LazyboxMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_instructions(
                "lazybox cross-agent coordination. Discover other sessions with \
                 list_sessions, learn your own identity with whoami, and read \
                 another session's recent output with read_session."
                    .to_string(),
            )
    }
}

/// kv key holding the JSON `token → session-key` map, so a reattached agent's
/// baked bearer keeps resolving across a daemon restart (#1420).
const TOKENS_KV_KEY: &str = "mcp:tokens";
/// kv key holding the last loopback port, reused on restart so a reattached
/// agent's baked endpoint URL still resolves.
const PORT_KV_KEY: &str = "mcp:port";

/// Restore persisted tokens, reuse the prior loopback port when free, record
/// the endpoint on `config.mcp`, and serve the MCP endpoint in a detached
/// task. Returns the bound address. Call once at daemon boot, before any agent
/// spawns so the spawn path sees the endpoint. Loopback-only mirrors the JSON
/// gateway's trust boundary.
///
/// Restoring tokens + reusing the port is what lets a session that survived
/// the restart (tmux) keep calling coordination tools: it baked
/// `http://127.0.0.1:PORT/` and a bearer into its MCP client at spawn and
/// cannot be handed new ones, so both must come back unchanged (#1420).
pub async fn start(config: ServerConfig) -> std::io::Result<std::net::SocketAddr> {
    restore_tokens(&config).await;
    let listener = bind_loopback(restore_port(&config).await)?;
    let addr = listener.local_addr()?;
    config.mcp.set_endpoint(format!("http://{addr}/"));
    persist_port(&config, addr.port()).await;
    tokio::spawn(async move {
        if let Err(error) = serve_listener(listener, config).await {
            tracing::warn!("mcp server exited: {error}");
        }
    });
    Ok(addr)
}

/// Bind a loopback TCP listener, preferring `desired_port` and falling back to
/// an ephemeral port (with a warning) when it's unavailable. `SO_REUSEADDR`
/// lets the reused port bind through a prior socket's lingering `TIME_WAIT`.
fn bind_loopback(desired_port: Option<u16>) -> std::io::Result<tokio::net::TcpListener> {
    if let Some(port) = desired_port.filter(|port| *port != 0) {
        match bind_loopback_port(port) {
            Ok(listener) => return Ok(listener),
            Err(error) => tracing::warn!(
                port,
                %error,
                "mcp: reusing prior port failed — reattached agents lose coordination until respawn; binding a fresh port"
            ),
        }
    }
    bind_loopback_port(0)
}

fn bind_loopback_port(port: u16) -> std::io::Result<tokio::net::TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_reuse_address(true)?;
    let addr: std::net::SocketAddr = (std::net::Ipv4Addr::LOCALHOST, port).into();
    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    socket.set_nonblocking(true)?;
    tokio::net::TcpListener::from_std(socket.into())
}

/// Persist the live token map (best-effort — a failed write only costs
/// cross-restart coordination, never the spawn).
pub(crate) async fn persist_tokens(config: &ServerConfig) {
    let payload = match serde_json::to_string(&config.mcp.tokens().snapshot()) {
        Ok(payload) => payload,
        Err(error) => {
            tracing::warn!("mcp: serialize token map: {error}");
            return;
        }
    };
    if let Err(error) = crate::store_blocking(&config.store, move |store| {
        store.set_kv(TOKENS_KV_KEY, &payload)
    })
    .await
    {
        tracing::warn!("mcp: persist token map: {error}");
    }
}

/// Rehydrate the token registry from the persisted snapshot, keeping only
/// tokens whose owning agent session survived this restart. Dropping the rest
/// bounds the map across restarts and stops a reboot-orphaned bearer (whose
/// backend session is gone) from resolving.
async fn restore_tokens(config: &ServerConfig) {
    let raw = match crate::store_blocking(&config.store, |store| store.get_kv(TOKENS_KV_KEY)).await
    {
        Ok(Some(raw)) => raw,
        _ => return,
    };
    let persisted: HashMap<String, String> = match serde_json::from_str(&raw) {
        Ok(map) => map,
        Err(error) => {
            tracing::warn!("mcp: parse persisted token map: {error}");
            return;
        }
    };
    if persisted.is_empty() {
        return;
    }
    let survivors = surviving_agent_sessions(config).await;
    let kept: Vec<(String, SessionKey)> = persisted
        .into_iter()
        .filter_map(|(token, key)| {
            let key = SessionKey::from(key.as_str());
            survivors.contains(&key).then_some((token, key))
        })
        .collect();
    if !kept.is_empty() {
        config.mcp.tokens().restore_from(kept);
    }
    // Rewrite the persisted map so the dropped (dead) tokens don't reappear on
    // the next restart.
    persist_tokens(config).await;
}

/// Session keys of agent sessions the backend still hosts after a restart.
async fn surviving_agent_sessions(config: &ServerConfig) -> std::collections::HashSet<SessionKey> {
    let keys = config.backend.list().await.unwrap_or_default();
    let mut sessions = std::collections::HashSet::new();
    for key in keys {
        if let Some((session_key, kind)) =
            crate::spawn_handler::load_terminal_meta(config, &key).await
            && matches!(kind, lazybox_ipc::TerminalKind::Agent(_))
        {
            sessions.insert(session_key);
        }
    }
    sessions
}

async fn persist_port(config: &ServerConfig, port: u16) {
    let value = port.to_string();
    if let Err(error) = crate::store_blocking(&config.store, move |store| {
        store.set_kv(PORT_KV_KEY, &value)
    })
    .await
    {
        tracing::warn!("mcp: persist port: {error}");
    }
}

async fn restore_port(config: &ServerConfig) -> Option<u16> {
    match crate::store_blocking(&config.store, |store| store.get_kv(PORT_KV_KEY)).await {
        Ok(Some(raw)) => raw.trim().parse().ok(),
        _ => None,
    }
}

/// Serve the MCP endpoint on an already-bound loopback listener, accepting
/// connections until the listener errors. Mirrors the JSON gateway's
/// per-connection spawn model.
pub async fn serve_listener(
    listener: tokio::net::TcpListener,
    config: ServerConfig,
) -> std::io::Result<()> {
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use hyper_util::server::conn::auto::Builder;
    use hyper_util::service::TowerToHyperService;
    use rmcp::transport::streamable_http_server::StreamableHttpService;
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;

    let factory_config = config.clone();
    let service = TowerToHyperService::new(StreamableHttpService::new(
        move || Ok(LazyboxMcp::new(factory_config.clone())),
        Arc::new(LocalSessionManager::default()),
        Default::default(),
    ));

    loop {
        let (stream, _) = listener.accept().await?;
        let io = TokioIo::new(stream);
        let service = service.clone();
        tokio::spawn(async move {
            if let Err(error) = Builder::new(TokioExecutor::default())
                .serve_connection(io, service)
                .await
            {
                tracing::debug!("mcp connection closed: {error}");
            }
        });
    }
}

/// Prepare a spawning agent to reach the coordination MCP: mint a fresh
/// per-session bearer, (re)register it, write the agent's MCP config file, and
/// return its path for the spawn argv (`--mcp-config`). Returns `None` when no
/// listener has started or the agent doesn't accept an injected MCP config, so
/// the spawn is unchanged.
///
/// A fresh token per spawn (with the prior one cleared) means a respawn's old
/// bearer stops resolving — the map holds one live token per session.
///
/// Mutates only the in-memory registry; the caller must
/// `persist_tokens` afterwards (from its async context) so the binding
/// survives a daemon restart.
pub fn provision_for_spawn(
    config: &ServerConfig,
    session_key: &SessionKey,
    agent: &dyn lazybox_agents::Agent,
) -> Option<std::path::PathBuf> {
    if !agent.supports_mcp_config() {
        return None;
    }
    let endpoint = config.mcp.endpoint()?;
    config.mcp.tokens().forget_session(session_key);
    let token = uuid::Uuid::new_v4().to_string();
    config
        .mcp
        .tokens()
        .register(token.clone(), session_key.clone());
    match write_mcp_config(session_key, &endpoint, &token) {
        Ok(path) => Some(path),
        Err(error) => {
            tracing::warn!(
                "mcp: could not write config for {}: {error}",
                session_key.as_str()
            );
            config.mcp.tokens().forget(&token);
            None
        }
    }
}

/// Tear down a session's coordination state when its last agent terminal ends:
/// forget its bearer token(s), delete its on-disk MCP config, and persist the
/// shrunken map. Without this a dead session's token resolves for the daemon's
/// whole lifetime — letting a terminated agent keep reading every live sibling
/// — and its bearer file lingers on disk (#1420).
pub async fn deprovision_session(config: &ServerConfig, session_key: &SessionKey) {
    config.mcp.tokens().forget_session(session_key);
    let _ = std::fs::remove_file(mcp_config_path(session_key));
    persist_tokens(config).await;
}

/// Directory holding per-session MCP config files. Each embeds a bearer token,
/// so it is created private to the owner (0700) — other local users must not
/// traverse in.
fn mcp_config_dir() -> std::path::PathBuf {
    crate::lifecycle::runtime_dir().join("mcp")
}

/// Path of `session_key`'s MCP config file (the bearer lives inside).
fn mcp_config_path(session_key: &SessionKey) -> std::path::PathBuf {
    mcp_config_dir().join(format!("{}.json", sanitize_key(session_key.as_str())))
}

/// Write a Claude-style `.mcp.json` pointing at the daemon endpoint with the
/// session's bearer, under `<runtime>/mcp/<session>.json`. Overwritten on each
/// respawn (the file name is per session, the token inside is fresh).
///
/// The file carries a bearer secret, so it is written 0600 in a 0700 dir —
/// the same posture the gateway token file gets (`local_gateway::write_discovery`).
/// A world-readable config would let any local user on a shared box lift the
/// token and read every session's terminal over loopback.
fn write_mcp_config(
    session_key: &SessionKey,
    endpoint: &str,
    token: &str,
) -> std::io::Result<std::path::PathBuf> {
    create_private_dir(&mcp_config_dir())?;
    let path = mcp_config_path(session_key);
    let doc = serde_json::json!({
        "mcpServers": {
            "lazybox": {
                "type": "http",
                "url": endpoint,
                "headers": { "Authorization": format!("Bearer {token}") },
            }
        }
    });
    write_private_file(&path, &serde_json::to_vec_pretty(&doc)?)?;
    Ok(path)
}

/// Create `dir` (and parents) private to the owner (0700), mirroring
/// [`crate::lifecycle::ensure_runtime_dir`]. The bearer files inside must not
/// be reachable by other local users.
fn create_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        // DirBuilder's mode is filtered through the umask, and an already-
        // existing dir keeps its old mode; pin it to exactly 0700.
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir)?;
    Ok(())
}

/// Write `bytes` to `path` owner-only (0600). The `mode` on `OpenOptions`
/// applies only when the file is *created*, so an existing file (e.g. one an
/// older lazybox wrote 0644) is re-pinned to 0600 after the write.
fn write_private_file(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Filesystem-safe rendering of a session key (which carries `:`, `/`, `#`).
fn sanitize_key(key: &str) -> String {
    key.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::SessionBackend;

    #[test]
    fn parse_bearer_strips_scheme_and_whitespace() {
        assert_eq!(parse_bearer("Bearer abc123"), Some("abc123"));
        assert_eq!(parse_bearer("bearer abc123"), Some("abc123"));
        assert_eq!(parse_bearer("  Bearer   abc123  "), Some("abc123"));
    }

    #[test]
    fn parse_bearer_rejects_non_bearer_and_empty() {
        assert_eq!(parse_bearer("Basic abc123"), None);
        assert_eq!(parse_bearer("abc123"), None);
        assert_eq!(parse_bearer("Bearer "), None);
        assert_eq!(parse_bearer(""), None);
    }

    #[test]
    fn token_registry_round_trips() {
        let reg = TokenRegistry::default();
        assert!(reg.is_empty());
        let key = SessionKey::from("github:owner/repo#1");
        reg.register("tok-1", key.clone());
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.resolve("tok-1"), Some(key));
        assert_eq!(reg.resolve("missing"), None);
    }

    #[test]
    fn token_registry_forget_and_replace() {
        let reg = TokenRegistry::default();
        reg.register("tok", SessionKey::from("a"));
        reg.register("tok", SessionKey::from("b"));
        assert_eq!(reg.resolve("tok"), Some(SessionKey::from("b")));
        reg.forget("tok");
        assert_eq!(reg.resolve("tok"), None);
        assert!(reg.is_empty());
    }

    #[test]
    fn token_registry_forget_session_clears_every_token_for_a_key() {
        let reg = TokenRegistry::default();
        let key = SessionKey::from("s");
        reg.register("t1", key.clone());
        reg.register("t2", key.clone());
        reg.register("other", SessionKey::from("s2"));
        reg.forget_session(&key);
        assert_eq!(reg.resolve("t1"), None);
        assert_eq!(reg.resolve("t2"), None);
        assert_eq!(reg.resolve("other"), Some(SessionKey::from("s2")));
    }

    #[test]
    fn sanitize_key_is_filesystem_safe() {
        assert_eq!(sanitize_key("github:owner/repo#1"), "github_owner_repo_1");
    }

    #[tokio::test]
    async fn start_binds_loopback_and_records_endpoint() {
        let config = ServerConfig::in_memory();
        assert!(config.mcp.endpoint().is_none());
        let addr = start(config.clone()).await.expect("mcp listener binds");
        assert!(addr.ip().is_loopback());
        let endpoint = config.mcp.endpoint().expect("endpoint recorded");
        assert!(endpoint.contains(&addr.port().to_string()), "{endpoint}");
    }

    #[test]
    fn provision_needs_endpoint_and_a_supporting_agent() {
        let config = ServerConfig::in_memory();
        let key = SessionKey::from("test:mcp-provision-guard");
        let claude = config.agents.get("claude").expect("claude builtin");

        // No endpoint yet → nothing provisioned, no token minted.
        assert!(provision_for_spawn(&config, &key, claude.as_ref()).is_none());
        assert!(config.mcp.tokens().is_empty());

        // A non-Claude builtin never gets an injected config, endpoint or not.
        config.mcp.set_endpoint("http://127.0.0.1:9/".into());
        if let Some(codex) = config.agents.get("codex") {
            assert!(!codex.supports_mcp_config());
            assert!(provision_for_spawn(&config, &key, codex.as_ref()).is_none());
        }
        assert!(config.mcp.tokens().is_empty());
    }

    #[test]
    fn provision_writes_config_registers_token_and_respawn_replaces_it() {
        let config = ServerConfig::in_memory();
        config.mcp.set_endpoint("http://127.0.0.1:54321/".into());
        let key = SessionKey::from("test:mcp-provision-write");
        let claude = config.agents.get("claude").expect("claude builtin");

        let path = provision_for_spawn(&config, &key, claude.as_ref()).expect("provisioned");
        assert!(path.exists());
        let body = std::fs::read_to_string(&path).expect("config readable");
        assert!(body.contains("http://127.0.0.1:54321/"), "{body}");
        assert!(body.contains("\"lazybox\""), "{body}");
        assert!(body.contains("Bearer "), "{body}");
        assert_eq!(config.mcp.tokens().len(), 1);
        // The bearer is a secret: file 0600, dir 0700 — a world-readable config
        // would let any local user lift the token and read every session.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file_mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(file_mode & 0o777, 0o600, "bearer config must be 0600");
            let dir_mode = std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(dir_mode & 0o777, 0o700, "mcp dir must be 0700");
        }

        // A respawn mints a fresh token and clears the old one — one live
        // token per session, so a stale bearer stops resolving.
        provision_for_spawn(&config, &key, claude.as_ref()).expect("re-provisioned");
        assert_eq!(config.mcp.tokens().len(), 1);

        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn whoami_payload_reports_the_key_even_with_no_live_agent() {
        let handler = LazyboxMcp::new(ServerConfig::in_memory());
        let key = SessionKey::from("github:acme/widget#7");
        let payload = handler.whoami_payload(&key).await.expect("payload");
        assert_eq!(payload["session_key"], "github:acme/widget#7");
        assert!(payload["agent"].is_null());
    }

    #[tokio::test]
    async fn list_sessions_payload_is_empty_without_running_agents() {
        let handler = LazyboxMcp::new(ServerConfig::in_memory());
        let payload = handler.list_sessions_payload(None).await.expect("payload");
        assert_eq!(payload["sessions"].as_array().map(Vec::len), Some(0));
    }

    #[tokio::test]
    async fn read_session_is_none_for_a_workspace_with_no_agent() {
        let handler = LazyboxMcp::new(ServerConfig::in_memory());
        assert!(
            handler
                .read_session_text("test:absent", None)
                .await
                .is_none()
        );
    }

    #[test]
    fn bearer_from_parts_reads_the_authorization_header() {
        // Exercises the exact type rmcp stashes in the tool RequestContext
        // (`http::request::Parts`) — the seam `LazyboxMcp::bearer` reads.
        let (with_bearer, _) = http::Request::builder()
            .header(http::header::AUTHORIZATION, "Bearer tok-xyz")
            .body(())
            .expect("request builds")
            .into_parts();
        assert_eq!(bearer_from_parts(&with_bearer).as_deref(), Some("tok-xyz"));

        let (no_auth, _) = http::Request::builder().body(()).unwrap().into_parts();
        assert_eq!(bearer_from_parts(&no_auth), None);

        let (basic, _) = http::Request::builder()
            .header(http::header::AUTHORIZATION, "Basic zzz")
            .body(())
            .unwrap()
            .into_parts();
        assert_eq!(bearer_from_parts(&basic), None);
    }

    #[tokio::test]
    async fn deprovision_revokes_token_and_removes_config() {
        let config = ServerConfig::in_memory();
        config.mcp.set_endpoint("http://127.0.0.1:12345/".into());
        let key = SessionKey::from("test:mcp-deprovision");
        let claude = config.agents.get("claude").expect("claude builtin");

        let path = provision_for_spawn(&config, &key, claude.as_ref()).expect("provisioned");
        assert!(path.exists());
        assert_eq!(config.mcp.tokens().len(), 1);

        // Ending the session's last agent terminal must revoke the bearer and
        // delete the file — a dead session's token can't keep resolving.
        deprovision_session(&config, &key).await;
        assert!(config.mcp.tokens().is_empty());
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn tokens_snapshot_round_trips_through_restore_from() {
        let reg = TokenRegistry::default();
        reg.register("t1", SessionKey::from("a"));
        reg.register("t2", SessionKey::from("b"));
        let snapshot = reg.snapshot();

        let restored = TokenRegistry::default();
        restored.restore_from(
            snapshot
                .into_iter()
                .map(|(token, key)| (token, SessionKey::from(key.as_str()))),
        );
        assert_eq!(restored.resolve("t1"), Some(SessionKey::from("a")));
        assert_eq!(restored.resolve("t2"), Some(SessionKey::from("b")));
    }

    #[tokio::test]
    async fn restore_tokens_keeps_survivors_and_drops_dead_sessions() {
        let (config, mock) = ServerConfig::in_memory_with_mock();
        // One agent backend session survives the "restart".
        let backend_key = mock
            .spawn(&[], None, &[], "survivor")
            .await
            .expect("spawn mock session");
        let live = SessionKey::from("test:mcp-survivor");
        let meta = serde_json::to_string(&(
            live.as_str().to_string(),
            lazybox_ipc::TerminalKind::Agent("claude".to_string()),
        ))
        .unwrap();
        config
            .store
            .set_kv(&format!("terminal:{backend_key}"), &meta)
            .unwrap();

        // Persist a map holding the survivor's token plus a dead session's.
        config.mcp.tokens().register("live-tok", live.clone());
        config
            .mcp
            .tokens()
            .register("dead-tok", SessionKey::from("test:mcp-dead"));
        persist_tokens(&config).await;

        // Simulate a fresh daemon: the in-memory map is empty until restore.
        config.mcp.tokens().forget("live-tok");
        config.mcp.tokens().forget("dead-tok");
        assert!(config.mcp.tokens().is_empty());

        restore_tokens(&config).await;
        assert_eq!(config.mcp.tokens().resolve("live-tok"), Some(live));
        assert_eq!(
            config.mcp.tokens().resolve("dead-tok"),
            None,
            "a session with no surviving backend must not be restored"
        );
    }

    #[tokio::test]
    async fn bind_loopback_reuses_a_freed_port_and_falls_back_when_taken() {
        // A freed port rebinds (the restart case: old daemon gone), so a
        // reattached agent's baked `http://127.0.0.1:PORT/` keeps resolving.
        let first = bind_loopback(None).expect("ephemeral bind");
        let port = first.local_addr().unwrap().port();
        drop(first);
        let reused = bind_loopback(Some(port)).expect("reuse freed port");
        assert_eq!(reused.local_addr().unwrap().port(), port);

        // A still-held port can't be reused: fall back to a fresh one rather
        // than fail to start.
        let fallback = bind_loopback(Some(port)).expect("fallback binds");
        assert_ne!(fallback.local_addr().unwrap().port(), port);
    }

    #[tokio::test]
    async fn port_persist_round_trips_and_start_records_it() {
        let config = ServerConfig::in_memory();
        assert!(restore_port(&config).await.is_none());
        let addr = start(config.clone()).await.expect("listener binds");
        assert_eq!(
            restore_port(&config).await,
            Some(addr.port()),
            "the bound port must be persisted for reuse on the next start"
        );
    }
}
