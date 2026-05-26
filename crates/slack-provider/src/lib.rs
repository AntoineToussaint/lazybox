//! Slack provider — outbound notifications + inbound Socket Mode.
//!
//! Pilot uses Slack as a "second display" for the inbox: each
//! workspace gets a mirrored channel where workspace + agent events post,
//! and inbound `@pilot` mentions route back to claude sessions
//! running on the daemon. Designed for "pilot at home, control
//! from phone."
//!
//! ## Why a single crate instead of one for in / one for out
//!
//! Inbound and outbound share the same Slack API client, the
//! same token chain, the same channel-name slugifier, and the
//! same `channel_id ↔ workspace_key` mapping. Splitting them
//! would force a `pilot-slack-core` shared crate underneath —
//! pure ceremony for the same call graph. One crate, two
//! modules (`api` for HTTP, `socket` for WebSocket).
//!
//! ## Auth
//!
//! Two tokens:
//! - **Bot token** (`xoxb-...`) for HTTP API. Loaded via
//!   `pilot_auth::CredentialChain` from `SLACK_BOT_TOKEN` env or
//!   `~/.pilot/config.yaml::slack.bot_token`.
//! - **App token** (`xapp-...`) for Socket Mode WebSocket. Same
//!   chain, distinct key (`SLACK_APP_TOKEN`).
//!
//! See `docs/slack-setup.md` for the Slack-side app config.

pub mod api;
pub mod socket;

pub use api::{Client, Message, channel_name_for_workspace};
pub use socket::{InboundEvent, SocketModeClient};

use pilot_auth::{CredentialChain, EnvProvider};

/// Workspace-key prefix this provider owns. Mirrors `pilot_gh::SOURCE`
/// and `pilot_linear::SOURCE` so the daemon's mutation router can
/// dispatch on `<source>-<rest>` consistently.
///
/// Slack isn't a *task* source though — it's an io channel layered on
/// top of github/linear/local projects. So nothing actually keys
/// workspaces by `slack-...`. The constant exists for symmetry and to
/// label log lines / metrics.
pub const SOURCE: &str = "slack";

/// Credential chain for the bot token. Tried in order:
/// `SLACK_BOT_TOKEN` env, then the daemon's config-file resolver
/// (called separately, since pilot_auth doesn't read YAML).
pub fn bot_credential_chain() -> CredentialChain {
    CredentialChain::new().with(EnvProvider::new("SLACK_BOT_TOKEN"))
}

/// Credential chain for the Socket Mode app token. Same shape as
/// bot, keyed on `SLACK_APP_TOKEN`.
pub fn app_credential_chain() -> CredentialChain {
    CredentialChain::new().with(EnvProvider::new("SLACK_APP_TOKEN"))
}

/// Pilot-side errors from the Slack provider. Wraps HTTP + JSON +
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
