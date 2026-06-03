//! Wire types for LLM-proxy telemetry. These cross the IPC boundary
//! as `Event::ProxyRecord`, which is why they live in the protocol
//! crate rather than in `lazybox-llm-proxy`. The proxy crate re-exports
//! them so existing call sites (`lazybox_llm_proxy::ProxyRecord`) keep
//! compiling without churn.
//!
//! Moved here to break the previous inverted dependency
//! (`lazybox-ipc → lazybox-llm-proxy`). Wire types belong at the bottom
//! of the dependency graph so any consumer (TUI, JSON API gateway,
//! future remote agents) can read records without dragging in hyper
//! / reqwest.

use chrono::{DateTime, Utc};
use lazybox_core::SessionKey;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Which upstream API this record corresponds to. Extensible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApiProvider {
    Anthropic,
    OpenAI,
    /// Fallback for hosts lazybox doesn't recognize. We still forward the
    /// request and record coarse metadata (url, status, duration) but
    /// can't parse provider-specific token counts.
    Unknown,
}

/// One request/response pair through the proxy. Written to SQLite by
/// the daemon and surfaced to the TUI as part of the session dashboard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyRecord {
    pub session_key: SessionKey,
    pub started_at: DateTime<Utc>,
    #[serde(with = "humantime_serde")]
    pub duration: Duration,
    pub provider: ApiProvider,
    pub endpoint: String,
    pub request_model: Option<String>,
    pub tokens_input: Option<u64>,
    pub tokens_output: Option<u64>,
    pub tokens_cache_read: Option<u64>,
    pub tokens_cache_create: Option<u64>,
    pub estimated_cost_usd: Option<f64>,
    pub tool_calls: Vec<ToolCall>,
    /// Full assistant text if `record_bodies` is enabled. Populates
    /// session search ("find where I asked about the config migration").
    pub assistant_text: Option<String>,
    pub status: u16,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub name: String,
    /// Short, truncated, human-readable. Not the raw JSON — we keep
    /// that off the hot path of serialization.
    pub args_summary: String,
    pub result_size_bytes: Option<u64>,
    #[serde(with = "humantime_serde", default)]
    pub duration: Option<Duration>,
}
