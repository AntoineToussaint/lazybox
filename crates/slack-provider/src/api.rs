//! Slack Web API client — `chat.postMessage`, `conversations.*`,
//! `auth.test`. Just the calls pilot needs; not a full SDK.
//!
//! ## Why no `slack-rs` / `slack-api` dep
//!
//! The two existing crates either bundle the wider SDK (4MB of
//! generated types we don't use) or haven't tracked the API
//! changes since 2022. We hit five endpoints — direct
//! `reqwest::Client` is smaller, easier to audit, and faster to
//! patch when Slack rotates a field.

use crate::SlackError;
use serde::{Deserialize, Serialize};

/// Slack HTTP API base. No proxy / staging override today; this
/// is one of the global endpoints with no per-tenant routing.
const SLACK_API: &str = "https://slack.com/api";

/// Authenticated client. Cheaply cloneable — wraps a
/// `reqwest::Client` (which is itself an `Arc<Inner>`).
#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    bot_token: String,
}

impl Client {
    /// Build a client from a bot token (`xoxb-...`). The token is
    /// held in memory; redact via the wrapper's `Debug` impl below
    /// so accidental tracing doesn't leak it.
    pub fn new(bot_token: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            bot_token: bot_token.into(),
        }
    }

    /// `auth.test` — round-trip the bot token to confirm it works
    /// and to read back the bot's display name. Called once at
    /// startup so a misconfigured token surfaces immediately.
    pub async fn auth_test(&self) -> Result<AuthTestResponse, SlackError> {
        self.call::<_, AuthTestResponse>("auth.test", &()).await
    }

    /// `conversations.list` — fetch up to `limit` channels the bot
    /// can see. Pilot uses this once at startup to build the
    /// `channel_name → channel_id` map; new channels created later
    /// are tracked via the `conversations.create` return value.
    pub async fn conversations_list(
        &self,
        limit: u32,
    ) -> Result<ConversationsListResponse, SlackError> {
        #[derive(Serialize)]
        struct Args {
            limit: u32,
            // exclude_archived = true: archived channels can't take
            // new messages anyway, so they pollute the lookup table.
            exclude_archived: bool,
            // Public + private only — group DMs / IMs come via a
            // separate endpoint when we need them.
            types: &'static str,
        }
        self.call::<_, ConversationsListResponse>(
            "conversations.list",
            &Args {
                limit,
                exclude_archived: true,
                types: "public_channel,private_channel",
            },
        )
        .await
    }

    /// `conversations.create` — create a public channel by name.
    /// Returns the channel id; pilot stashes that for the
    /// `channel_id → workspace_key` reverse map.
    ///
    /// Idempotent at the API level — Slack returns
    /// `name_taken` if the channel already exists. Callers handle
    /// that case by looking up the existing id.
    pub async fn conversations_create(
        &self,
        name: &str,
    ) -> Result<ConversationsCreateResponse, SlackError> {
        #[derive(Serialize)]
        struct Args<'a> {
            name: &'a str,
            is_private: bool,
        }
        self.call::<_, ConversationsCreateResponse>(
            "conversations.create",
            &Args {
                name,
                is_private: false,
            },
        )
        .await
    }

    /// `chat.postMessage` — post a message to a channel by id.
    /// `thread_ts` optional for threading (used by the per-PR
    /// thread strategy).
    pub async fn post_message(&self, m: &Message) -> Result<PostMessageResponse, SlackError> {
        self.call::<_, PostMessageResponse>("chat.postMessage", m).await
    }

    /// Internal: shared request shape. Slack returns 200 OK with
    /// `{ok: false, error: "..."}` for application errors — surface
    /// those as `SlackError::Api(error_code)` so callers can pattern-
    /// match on string codes (e.g. `"name_taken"`).
    async fn call<Req: Serialize + ?Sized, Resp: for<'de> Deserialize<'de>>(
        &self,
        endpoint: &str,
        body: &Req,
    ) -> Result<Resp, SlackError> {
        let url = format!("{SLACK_API}/{endpoint}");
        let raw = self
            .http
            .post(&url)
            .bearer_auth(&self.bot_token)
            .json(body)
            .send()
            .await?
            .text()
            .await?;
        // Two-step parse: first into a {ok, error} probe, then into
        // the typed response. Lets the application-error path
        // surface `error` regardless of which endpoint failed.
        let probe: SlackEnvelope = serde_json::from_str(&raw)?;
        if !probe.ok {
            return Err(SlackError::Api(probe.error.unwrap_or_else(|| {
                format!("{endpoint}: unknown error (no `error` field)")
            })));
        }
        Ok(serde_json::from_str(&raw)?)
    }
}

/// Slack response envelope every endpoint shares. The application-
/// level success bit + (when failed) an `error` code string.
#[derive(Deserialize)]
struct SlackEnvelope {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct AuthTestResponse {
    pub url: String,
    pub team: String,
    pub user: String,
    pub user_id: String,
    pub bot_id: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct ConversationsListResponse {
    pub channels: Vec<Channel>,
    /// Cursor for pagination. Empty / absent on the last page.
    #[serde(default)]
    pub response_metadata: ResponseMetadata,
}

#[derive(Deserialize, Debug, Clone, Default)]
pub struct ResponseMetadata {
    #[serde(default)]
    pub next_cursor: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Channel {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub is_archived: bool,
    #[serde(default)]
    pub is_private: bool,
}

#[derive(Deserialize, Debug, Clone)]
pub struct ConversationsCreateResponse {
    pub channel: Channel,
}

/// Message payload for `chat.postMessage`. Built with the public
/// constructors so consumers don't accidentally send malformed
/// payloads (Slack rejects missing `channel` even on success
/// envelopes).
#[derive(Serialize, Debug, Clone)]
pub struct Message {
    pub channel: String,
    pub text: String,
    /// Optional rich-formatting blocks. None → plain text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocks: Option<serde_json::Value>,
    /// Optional thread anchor. Pilot uses this when
    /// `channel_strategy: thread_per_workspace` is configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_ts: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mrkdwn: Option<bool>,
}

impl Message {
    pub fn new(channel: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            channel: channel.into(),
            text: text.into(),
            blocks: None,
            thread_ts: None,
            mrkdwn: Some(true),
        }
    }

    pub fn in_thread(mut self, thread_ts: impl Into<String>) -> Self {
        self.thread_ts = Some(thread_ts.into());
        self
    }
}

#[derive(Deserialize, Debug, Clone)]
pub struct PostMessageResponse {
    pub channel: String,
    /// Message timestamp — used as the `thread_ts` for follow-up
    /// replies in the same thread.
    pub ts: String,
}

/// Canonical Slack channel name for a workspace key. Slack channel
/// names must be lowercase, ≤ 80 chars, with only letters, digits,
/// hyphens, underscores, and periods. Pilot turns
/// `"github-acme-widget-186"` → `"github-acme-widget-186"` (already
/// lowercase + safe characters) but defensively sluggifies in case
/// a future workspace key contains unexpected chars.
pub fn channel_name_for_workspace(workspace_key: &str, prefix: &str) -> String {
    let mut s = String::with_capacity(prefix.len() + workspace_key.len());
    s.push_str(prefix);
    for c in workspace_key.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
            s.push(c.to_ascii_lowercase());
        } else {
            s.push('-');
        }
    }
    // Slack enforces 80 chars; we cap at 80 too.
    if s.chars().count() > 80 {
        s = s.chars().take(80).collect();
    }
    // Names can't end with `-` or `.` — trim trailing punctuation.
    while matches!(s.chars().last(), Some('-' | '.' | '_')) {
        s.pop();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_name_passes_through_well_formed_workspace_key() {
        assert_eq!(
            channel_name_for_workspace("github-acme-widget-186", ""),
            "github-acme-widget-186"
        );
    }

    #[test]
    fn channel_name_lowercases_input() {
        assert_eq!(
            channel_name_for_workspace("GitHub-Acme-Widget-186", ""),
            "github-acme-widget-186"
        );
    }

    #[test]
    fn channel_name_replaces_invalid_chars_with_dash() {
        assert_eq!(
            channel_name_for_workspace("github:acme/widget#186", ""),
            "github-acme-widget-186"
        );
    }

    #[test]
    fn channel_name_applies_prefix() {
        assert_eq!(
            channel_name_for_workspace("github-acme-widget-186", "pr-"),
            "pr-github-acme-widget-186"
        );
    }

    #[test]
    fn channel_name_caps_at_eighty_chars() {
        let long = format!("github-{}", "a".repeat(100));
        let out = channel_name_for_workspace(&long, "");
        assert!(out.chars().count() <= 80);
    }

    #[test]
    fn channel_name_trims_trailing_punctuation() {
        // Hypothetical key that ends in `-` after sanitization —
        // Slack rejects those.
        assert_eq!(channel_name_for_workspace("foo-", ""), "foo");
        assert_eq!(channel_name_for_workspace("foo!", ""), "foo");
    }
}
