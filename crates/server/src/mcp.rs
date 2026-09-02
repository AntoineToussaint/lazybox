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
//! `rmcp`'s tower [`StreamableHttpService`] wrapped onto the existing hyper
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

    /// Resolve a token to its session, if still registered.
    pub fn resolve(&self, token: &str) -> Option<SessionKey> {
        self.inner.read().get(token).cloned()
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

/// Shared state behind every MCP connection: the daemon handle plus the token
/// registry. Cloned per session by the service factory; both fields are
/// cheap-clone (Arc-backed).
#[derive(Clone)]
pub struct McpState {
    pub config: ServerConfig,
    pub tokens: Arc<TokenRegistry>,
}

impl McpState {
    pub fn new(config: ServerConfig, tokens: Arc<TokenRegistry>) -> Self {
        Self { config, tokens }
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

/// The lazybox MCP handler. One instance per connection, all sharing
/// [`McpState`].
#[derive(Clone)]
pub struct LazyboxMcp {
    state: McpState,
    tool_router: ToolRouter<LazyboxMcp>,
}

#[tool_router]
impl LazyboxMcp {
    pub fn new(state: McpState) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
        }
    }

    /// The bearer token on the current request, if any.
    fn bearer(ctx: &RequestContext<RoleServer>) -> Option<String> {
        let parts = ctx.extensions.get::<http::request::Parts>()?;
        let header = parts.headers.get(http::header::AUTHORIZATION)?;
        parse_bearer(header.to_str().ok()?).map(str::to_owned)
    }

    /// Resolve the calling session, rejecting a missing or unknown token. Used
    /// by every tool so an unauthenticated loopback caller gets nothing.
    fn caller(&self, ctx: &RequestContext<RoleServer>) -> Result<SessionKey, McpError> {
        let token = Self::bearer(ctx)
            .ok_or_else(|| McpError::invalid_request("missing bearer token", None))?;
        self.state
            .tokens
            .resolve(&token)
            .ok_or_else(|| McpError::invalid_request("unknown or expired session token", None))
    }

    #[tool(
        description = "Identify the calling session: its session key, workspace, repo, and agent. Call this first to learn who you are before referencing other sessions."
    )]
    async fn whoami(&self, ctx: RequestContext<RoleServer>) -> Result<CallToolResult, McpError> {
        let key = self.caller(&ctx)?;
        let resp = api_gateway::agents_response(&self.state.config)
            .await
            .map_err(|error| McpError::internal_error(format!("read agents: {error}"), None))?;
        let me = resp
            .agents
            .into_iter()
            .find(|agent| agent.workspace_key == key.as_str());
        let payload = serde_json::json!({
            "session_key": key.as_str(),
            "workspace_name": me.as_ref().map(|a| a.workspace_name.clone()),
            "repo": me.as_ref().and_then(|a| a.repo.clone()),
            "agent": me.as_ref().map(|a| a.agent.clone()),
            "state": me.as_ref().and_then(|a| a.state),
        });
        Ok(CallToolResult::success(vec![ContentBlock::text(
            serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string()),
        )]))
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
        let resp = api_gateway::agents_response(&self.state.config)
            .await
            .map_err(|error| McpError::internal_error(format!("read agents: {error}"), None))?;
        let needle = args.filter.map(|f| f.to_lowercase());
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
        let payload = serde_json::json!({ "sessions": sessions });
        Ok(CallToolResult::success(vec![ContentBlock::text(
            serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string()),
        )]))
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
        let key = SessionKey::from(args.workspace.as_str());
        let Some(terminal_id) = self
            .state
            .config
            .terminal
            .running_agent_terminal(&key)
            .await
        else {
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "no running agent in workspace {}",
                args.workspace
            ))]));
        };
        let max_lines = args
            .tail
            .unwrap_or(api_gateway::AGENT_OUTPUT_DEFAULT_LINES)
            .clamp(1, api_gateway::AGENT_OUTPUT_MAX_LINES);
        let output =
            crate::spawn_handler::agent_output_snapshot(&self.state.config, terminal_id, max_lines)
                .await
                .unwrap_or_default();
        Ok(CallToolResult::success(vec![ContentBlock::text(output)]))
    }
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

/// Serve the MCP endpoint on an already-bound loopback listener, accepting
/// connections until the listener errors. Mirrors the JSON gateway's
/// per-connection spawn model.
pub async fn serve_listener(
    listener: tokio::net::TcpListener,
    state: McpState,
) -> std::io::Result<()> {
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use hyper_util::server::conn::auto::Builder;
    use hyper_util::service::TowerToHyperService;
    use rmcp::transport::streamable_http_server::StreamableHttpService;
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;

    let factory_state = state.clone();
    let service = TowerToHyperService::new(StreamableHttpService::new(
        move || Ok(LazyboxMcp::new(factory_state.clone())),
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
