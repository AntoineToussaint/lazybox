//! LLM proxy — transparent pass-through that records structured
//! telemetry from agent API traffic.
//!
//! **Rationale.** Scraping PTY frames for "what did the agent just do"
//! works for coarse state (working vs asking) but falls apart for
//! anything structured: tokens, tool calls, cost, assistant text for
//! search. The agent is already speaking a well-defined JSON protocol
//! to Anthropic / OpenAI. Proxying that protocol gives us reliable,
//! structured data for free.
//!
//! **Boundaries.** The proxy is deliberately read-only. It forwards
//! requests byte-for-byte (modulo adding our session tag header) and
//! forwards responses byte-for-byte. It never rewrites model names,
//! never swaps providers, never modifies tool results. Observability
//! only — so users can trust the agent is doing exactly what it
//! claims.
//!
//! **Trust.** Binds `127.0.0.1:<ephemeral>` only. Never accepts remote
//! connections. User's API key lives in the agent's env and hits the
//! proxy in the `Authorization` header; the proxy forwards it verbatim
//! and doesn't log auth headers.
//!
//! This module currently ships the types that the daemon and SQLite
//! layer persist. The hyper server that binds the port and forwards
//! upstream traffic lands together with the daemon integration — see
//! tasks #68 and #69 in the roadmap.

pub mod pricing;
pub mod server;

pub use server::{ProxyError, ProxyServer, SESSION_TAG_HEADER};

// Wire types live in `lazybox_ipc::proxy` so the protocol crate stays
// at the bottom of the dependency graph. Re-export them here so
// existing call sites (`lazybox_llm_proxy::ProxyRecord` etc.) keep
// working without churn.
pub use lazybox_ipc::{ApiProvider, ProxyRecord, ToolCall};

/// Per-spawn proxy configuration handed to the agent via env vars.
///
/// When the daemon spawns a wrapped agent command, it allocates one of
/// these, binds a fresh port if needed, and sets ANTHROPIC_BASE_URL /
/// OPENAI_BASE_URL in the child env. Each terminal gets its own
/// `session_tag`; the proxy uses that to attribute requests to the
/// right lazybox session without exposing lazybox internals to the agent.
#[derive(Debug, Clone)]
pub struct ProxyCtx {
    pub anthropic_url: Option<String>,
    pub openai_url: Option<String>,
    /// Header value the proxy recognizes. The daemon injects it as
    /// `X-Lazybox-Session: <tag>` into every agent request (via the
    /// upstream envvar trick when possible, or a header-rewriting
    /// layer if needed).
    pub session_tag: String,
}

/// Shared configuration for the daemon's proxy server. One proxy
/// instance can serve many agents (keyed by `session_tag`).
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    pub listen: std::net::SocketAddr,
    /// Record raw assistant text + tool arg/result payloads. True by
    /// default; flip off for privacy-sensitive workflows.
    pub record_bodies: bool,
    /// Header names (lowercased) stripped from stored records.
    pub redact_headers: Vec<String>,
    /// JSON paths stripped from stored records. Simple dotted paths;
    /// not a full JSONPath engine (we don't need one).
    pub redact_json_paths: Vec<String>,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:0".parse().expect("static addr parses"),
            record_bodies: true,
            redact_headers: vec!["authorization".into(), "x-api-key".into(), "cookie".into()],
            redact_json_paths: vec![],
        }
    }
}

// `ProxyServer` lives in `server.rs` — re-exported above.
