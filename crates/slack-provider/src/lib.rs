//! Slack provider — outbound notifications + inbound Socket Mode.
//!
//! Lazybox uses Slack as a "second display" for the inbox: each
//! workspace gets a mirrored channel where workspace + agent events post,
//! and inbound `@lazybox` mentions route back to claude sessions
//! running on the daemon. Designed for "lazybox at home, control
//! from phone."
//!
//! ## Why a single crate instead of one for in / one for out
//!
//! Inbound and outbound share the same Slack API client, the
//! same token chain, the same channel-name slugifier, and the
//! same `channel_id ↔ workspace_key` mapping. Splitting them
//! would force a `lazybox-slack-core` shared crate underneath —
//! pure ceremony for the same call graph. One crate, two
//! modules (`api` for HTTP, `socket` for WebSocket).
//!
//! ## Auth
//!
//! Two tokens:
//! - **Bot token** (`xoxb-...`) for HTTP API. Loaded via
//!   `lazybox_auth::CredentialChain` from `SLACK_BOT_TOKEN` env or
//!   `~/.lazybox/config.yaml::slack.bot_token`.
//! - **App token** (`xapp-...`) for Socket Mode WebSocket. Same
//!   chain, distinct key (`SLACK_APP_TOKEN`).
//!
//! See `docs/slack-setup.md` for the Slack-side app config.

pub mod api;
pub mod prune;
pub mod socket;

pub use api::{Client, Message, channel_name_for_workspace};
pub use socket::{InboundEvent, SocketModeClient};

use lazybox_auth::{CredentialChain, EnvProvider};

/// Workspace-key prefix this provider owns. Mirrors `lazybox_gh::SOURCE`
/// and `lazybox_linear::SOURCE` so the daemon's mutation router can
/// dispatch on `<source>-<rest>` consistently.
///
/// Slack isn't a *task* source though — it's an io channel layered on
/// top of github/linear/local projects. So nothing actually keys
/// workspaces by `slack-...`. The constant exists for symmetry and to
/// label log lines / metrics.
pub const SOURCE: &str = "slack";

/// Credential chain for the bot token. Tried in order:
/// `SLACK_BOT_TOKEN` env, then the daemon's config-file resolver
/// (called separately, since lazybox_auth doesn't read YAML).
pub fn bot_credential_chain() -> CredentialChain {
    CredentialChain::new().with(EnvProvider::new("SLACK_BOT_TOKEN"))
}

/// Credential chain for the Socket Mode app token. Same shape as
/// bot, keyed on `SLACK_APP_TOKEN`.
pub fn app_credential_chain() -> CredentialChain {
    CredentialChain::new().with(EnvProvider::new("SLACK_APP_TOKEN"))
}

/// Shared reqwest client construction for every Slack HTTP call.
/// Explicit timeouts: without them a black-holed connection to
/// `slack.com` parks the calling task forever (reqwest's default has
/// NO overall timeout). 30s total / 10s connect comfortably covers
/// Slack's slowest endpoints while keeping a dead network finite.
/// Builder failure (TLS init) falls back to the default client —
/// degraded but never panicking in a library crate.
pub(crate) fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_else(|e| {
            tracing::warn!("slack http client builder failed ({e}); using default client");
            reqwest::Client::new()
        })
}

/// Lazybox-side errors from the Slack provider. Wraps HTTP + JSON +
/// Slack's own structured error responses.
#[derive(Debug, thiserror::Error)]
pub enum SlackError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// Slack returns 200 OK with `{ok: false, error: "..."}`. Caller
    /// usually wants the human-readable error code (e.g.
    /// `channel_not_found`, `not_in_channel`, `rate_limited`) more
    /// than the HTTP status.
    #[error("slack: {0}")]
    Api(String),
    #[error("websocket: {0}")]
    WebSocket(String),
    #[error("config: {0}")]
    Config(String),
}
