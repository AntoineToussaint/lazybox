//! Slack Web API client — `chat.postMessage`, `conversations.*`,
//! `auth.test`. Just the calls lazybox needs; not a full SDK.
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
    base: String,
}

impl Client {
    /// Build a client from a bot token (`xoxb-...`). The token is
    /// held in memory; redact via the wrapper's `Debug` impl below
    /// so accidental tracing doesn't leak it.
    pub fn new(bot_token: impl Into<String>) -> Self {
        Self {
            http: crate::http_client(),
            bot_token: bot_token.into(),
            base: SLACK_API.to_string(),
        }
    }

    /// `auth.test` — round-trip the bot token to confirm it works
    /// and to read back the bot's display name. Called once at
    /// startup so a misconfigured token surfaces immediately.
    pub async fn auth_test(&self) -> Result<AuthTestResponse, SlackError> {
        self.call::<_, AuthTestResponse>("auth.test", &()).await
    }

    /// `conversations.list` — fetch up to `limit` channels the bot
    /// can see. Lazybox uses this once at startup to build the
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

    /// Paginated `conversations.list`. Used by `lazybox slack prune`,
    /// which needs to walk the whole workspace (including archived
    /// channels — so it can skip ones already archived) and which
    /// can't rely on a single 1000-channel page. `cursor` is the
    /// `response_metadata.next_cursor` from the previous call; pass
    /// an empty string for the first page.
    pub async fn conversations_list_page(
        &self,
        limit: u32,
        cursor: &str,
        include_archived: bool,
    ) -> Result<ConversationsListResponse, SlackError> {
        #[derive(Serialize)]
        struct Args<'a> {
            limit: u32,
            exclude_archived: bool,
            types: &'static str,
            #[serde(skip_serializing_if = "str::is_empty")]
            cursor: &'a str,
        }
        self.call::<_, ConversationsListResponse>(
            "conversations.list",
            &Args {
                limit,
                exclude_archived: !include_archived,
                types: "public_channel,private_channel",
                cursor,
            },
        )
        .await
    }

    /// Walk every page of `conversations.list` and collect all
    /// non-archived channels the bot can see. The boot cache seed and
    /// the `name_taken` recovery path both need the full workspace,
    /// not just the first 1000-channel page.
    pub async fn conversations_list_all(&self) -> Result<Vec<Channel>, SlackError> {
        let mut out = Vec::new();
        let mut cursor = String::new();
        loop {
            let resp = self.conversations_list_page(1000, &cursor, false).await?;
            out.extend(resp.channels);
            if resp.response_metadata.next_cursor.is_empty() {
                break;
            }
            cursor = resp.response_metadata.next_cursor;
        }
        Ok(out)
    }

    /// `conversations.archive` — archive a channel by id. Slack
    /// doesn't expose a delete-channel endpoint; archiving is the
    /// closest lazybox can get. Idempotent-ish: re-archiving a channel
    /// that's already archived returns `already_archived`, which
    /// callers should treat as success.
    pub async fn conversations_archive(&self, channel_id: &str) -> Result<(), SlackError> {
        #[derive(Serialize)]
        struct Args<'a> {
            channel: &'a str,
        }
        // `conversations.archive` returns just `{ok: true}` on
        // success — no typed body to deserialize, so accept the
        // empty envelope through `serde_json::Value`.
        self.call::<_, serde_json::Value>(
            "conversations.archive",
            &Args {
                channel: channel_id,
            },
        )
        .await
        .map(|_| ())
    }

    /// `conversations.create` — create a public channel by name.
    /// Returns the channel id; lazybox stashes that for the
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
    /// `thread_ts` optional for threading (used by the per-workspace
    /// thread strategy).
    pub async fn post_message(&self, m: &Message) -> Result<PostMessageResponse, SlackError> {
        self.call::<_, PostMessageResponse>("chat.postMessage", m)
            .await
    }

    /// `conversations.join` — self-join a public channel by id. Used
    /// by `lazybox slack init` so the bot lands in `#lazybox` without
    /// requiring a manual `/invite @lazybox` from the user.
    ///
    /// Idempotent: Slack returns `ok: true` even when the bot is
    /// already a member (with `already_in_channel: true` in the
    /// payload, which we don't bother surfacing).
    pub async fn conversations_join(
        &self,
        channel_id: &str,
    ) -> Result<ConversationsJoinResponse, SlackError> {
        #[derive(Serialize)]
        struct Args<'a> {
            channel: &'a str,
        }
        self.call::<_, ConversationsJoinResponse>(
            "conversations.join",
            &Args {
                channel: channel_id,
            },
        )
        .await
    }

    /// Point the client at a different API base. Test-only: lets a
    /// canned local server stand in for `slack.com`.
    #[cfg(test)]
    fn with_base(mut self, base: impl Into<String>) -> Self {
        self.base = base.into();
        self
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
        let url = format!("{}/{endpoint}", self.base);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.bot_token)
            .json(body)
            .send()
            .await?;
        // HTTP status gates the parse: Slack's 429 (and proxy-level
        // 5xx pages) carry non-JSON bodies that used to surface as
        // an opaque `json:` error. 429 maps to the typed rate-limit
        // error with the Retry-After hint when present.
        let status = resp.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok());
            return Err(SlackError::rate_limited(retry_after));
        }
        let raw = resp.text().await?;
        if !status.is_success() {
            return Err(SlackError::Api(format!(
                "{endpoint}: http {}",
                status.as_u16()
            )));
        }
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
    /// Unix timestamp (seconds) the channel was created. Used by
    /// `lazybox slack prune --older-than` to age-out stale per-(session,
    /// agent) channels. Slack always sends this; the `default` keeps
    /// the deserializer permissive against legacy fixtures / mock
    /// payloads that omit it.
    #[serde(default)]
    pub created: i64,
}

#[derive(Deserialize, Debug, Clone)]
pub struct ConversationsCreateResponse {
    pub channel: Channel,
}

#[derive(Deserialize, Debug, Clone)]
pub struct ConversationsJoinResponse {
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
    /// Optional thread anchor. Lazybox uses this when
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
/// hyphens, underscores, and periods. Lazybox turns
/// `"github-acme-widget-186"` → `"github-acme-widget-186"` (already
/// lowercase + safe characters) but defensively sluggifies in case
/// a future workspace key contains unexpected chars.
pub fn channel_name_for_workspace(workspace_key: &str, prefix: &str) -> String {
    sluggify(&format!("{prefix}{workspace_key}"))
}

/// Channel name for a specific (session, agent) pair. Format:
/// `<prefix><workspace>-<session8>-<agent>`. Each per-(session, agent)
/// channel is its own conversation so a workspace running Claude in
/// one worktree and Codex in another doesn't collide on `@lazybox`
/// inbound or status reports.
///
/// `session_id` is shortened to 8 hex chars — enough uniqueness for a
/// per-workspace handful of worktrees and short enough to leave room
/// inside Slack's 80-char limit. If two sessions in the same
/// workspace ever collide on the prefix the name_taken recovery path
/// reuses the first one's channel; not ideal, but the alternative
/// (full UUID = unreadable channel names) loses more.
pub fn channel_name_for_terminal(
    workspace_key: &str,
    session_id: &str,
    agent_id: &str,
    prefix: &str,
) -> String {
    let short = session_id
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .take(8)
        .collect::<String>();
    sluggify(&format!("{prefix}{workspace_key}-{short}-{agent_id}",))
}

/// Slug a string into a valid Slack channel name: lowercase letters,
/// digits, hyphens, underscores, periods. Caps at 80 chars and trims
/// trailing punctuation Slack rejects.
///
/// Names that overflow the cap get a deterministic 8-hex-char hash
/// suffix of the FULL slug in the last segment, so two long
/// (session, agent) pairs that share their first 80 chars can't
/// collide on the truncated name (which would silently route two
/// agents into one channel via the `name_taken` recovery path).
///
/// `pub` so `lazybox slack prune --workspace` can normalize the user's
/// input (e.g. `acme/widget#186`) the same way before matching it
/// against parsed channel names.
pub fn sluggify(input: &str) -> String {
    let mut s = String::with_capacity(input.len());
    for c in input.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
            s.push(c.to_ascii_lowercase());
        } else {
            s.push('-');
        }
    }
    if s.chars().count() > 80 {
        let digest = fnv1a_64(s.as_bytes());
        let mut head: String = s.chars().take(71).collect();
        while matches!(head.chars().last(), Some('-' | '.' | '_')) {
            head.pop();
        }
        s = format!("{head}-{:08x}", digest as u32);
    }
    while matches!(s.chars().last(), Some('-' | '.' | '_')) {
        s.pop();
    }
    s
}

/// FNV-1a 64-bit. Stable across processes and Rust releases (unlike
/// `DefaultHasher`), which matters because channel names persist on
/// the Slack side across lazybox restarts. No new deps.
fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Spin up a tiny TCP server that emulates a two-page
    /// `conversations.list`: the first request (no `cursor` in the
    /// body) gets a `next_cursor`, the follow-up request (which carries
    /// the cursor) gets an empty one. Hand-rolled rather than pulling in
    /// `wiremock` for a single stateless rule. Returns the `http://addr`
    /// base to feed into [`Client::with_base`].
    async fn spawn_paginating_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(x) => x,
                    Err(_) => continue,
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    // The cursor is only serialized once it's non-empty,
                    // so its presence in the body marks the second page.
                    let body = if req.contains("\"cursor\"") {
                        r#"{"ok":true,"channels":[{"id":"C2","name":"second"}],"response_metadata":{"next_cursor":""}}"#
                    } else {
                        r#"{"ok":true,"channels":[{"id":"C1","name":"first"}],"response_metadata":{"next_cursor":"PAGE2"}}"#
                    };
                    let response = format!(
                        "HTTP/1.1 200 OK\r\n\
                         Content-Type: application/json\r\n\
                         Content-Length: {}\r\n\
                         Connection: close\r\n\r\n{body}",
                        body.len(),
                    );
                    let _ = sock.write_all(response.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn conversations_list_all_walks_every_page() {
        let base = spawn_paginating_server().await;
        let client = Client::new("xoxb-test").with_base(base);
        let channels = client.conversations_list_all().await.unwrap();
        let names: Vec<&str> = channels.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["first", "second"]);
    }

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

    #[test]
    fn channel_name_for_terminal_includes_session_and_agent() {
        let n = channel_name_for_terminal(
            "github-acme-widget-186",
            "a3f1c277-9abc-4d51-8f01-deadbeef0001",
            "claude",
            "",
        );
        assert_eq!(n, "github-acme-widget-186-a3f1c277-claude");
    }

    #[test]
    fn channel_name_for_terminal_applies_prefix() {
        let n =
            channel_name_for_terminal("github-acme-widget-186", "a3f1c277-9abc", "codex", "pr-");
        assert_eq!(n, "pr-github-acme-widget-186-a3f1c277-codex");
    }

    #[test]
    fn channel_name_for_terminal_caps_at_eighty_chars() {
        let long = format!("github-{}", "a".repeat(100));
        let n = channel_name_for_terminal(&long, "deadbeef", "claude", "");
        assert!(n.chars().count() <= 80);
    }

    /// One-shot raw HTTP server returning a fixed status line +
    /// headers + body. Lets the client-side status handling be
    /// tested without slack.com.
    async fn spawn_status_server(status_line: &'static str, headers: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(x) => x,
                    Err(_) => continue,
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let _ = sock.read(&mut buf).await;
                    let body = "rate limited";
                    let response = format!(
                        "HTTP/1.1 {status_line}\r\n{headers}Content-Type: text/plain\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len(),
                    );
                    let _ = sock.write_all(response.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        format!("http://{addr}")
    }

    /// HTTP 429 used to fall through to the JSON parse and surface as
    /// an opaque `json:` error. It must map to the typed rate-limit
    /// error, carrying the Retry-After hint.
    #[tokio::test]
    async fn http_429_maps_to_rate_limited_with_retry_after() {
        let base = spawn_status_server("429 Too Many Requests", "Retry-After: 7\r\n").await;
        let client = Client::new("xoxb-test").with_base(base);
        let err = client.auth_test().await.expect_err("429 must error");
        assert!(err.is_rate_limited(), "got: {err}");
        assert_eq!(err.retry_after_secs(), Some(7));
    }

    #[tokio::test]
    async fn http_429_without_retry_after_still_rate_limited() {
        let base = spawn_status_server("429 Too Many Requests", "").await;
        let client = Client::new("xoxb-test").with_base(base);
        let err = client.auth_test().await.expect_err("429 must error");
        assert!(err.is_rate_limited());
        assert_eq!(err.retry_after_secs(), None);
    }

    /// Non-JSON 5xx bodies (proxy error pages) surface as a typed
    /// API error naming the status, not a `json:` parse failure.
    #[tokio::test]
    async fn http_5xx_surfaces_status_not_json_error() {
        let base = spawn_status_server("502 Bad Gateway", "").await;
        let client = Client::new("xoxb-test").with_base(base);
        let err = client.auth_test().await.expect_err("502 must error");
        match err {
            SlackError::Api(code) => assert!(code.contains("http 502"), "got: {code}"),
            other => panic!("expected Api error, got {other:?}"),
        }
    }

    #[test]
    fn rate_limited_constructor_round_trips() {
        let e = SlackError::rate_limited(Some(30));
        assert!(e.is_rate_limited());
        assert_eq!(e.retry_after_secs(), Some(30));
        let e = SlackError::rate_limited(None);
        assert!(e.is_rate_limited());
        assert_eq!(e.retry_after_secs(), None);
        // Slack's own application code counts too.
        assert!(SlackError::Api("ratelimited".into()).is_rate_limited());
        assert!(!SlackError::Api("name_taken".into()).is_rate_limited());
    }

    /// Two long (session, agent) pairs sharing their first 80 chars
    /// used to truncate to the SAME channel name — the `name_taken`
    /// recovery then silently routed both agents into one channel.
    /// The hash suffix keeps them distinct (and deterministic).
    #[test]
    fn long_names_get_distinct_hash_suffixes() {
        let shared = format!("github-{}", "a".repeat(100));
        // Distinct sessions in the same long-named workspace: the
        // 8-char session shorts differ but sit past the 80-char cut.
        let one = channel_name_for_terminal(&shared, "11111111-aaaa", "claude", "");
        let two = channel_name_for_terminal(&shared, "22222222-bbbb", "claude", "");
        assert_ne!(one, two, "shared 80-char prefix must not collide");
        for n in [&one, &two] {
            assert!(n.chars().count() <= 80, "{n} exceeds slack's cap");
            assert!(
                n.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "-_.".contains(c)),
                "{n} has invalid chars"
            );
            assert!(!n.ends_with(['-', '.', '_']));
        }
        // Deterministic across calls — the name must be findable on
        // the next lazybox boot.
        assert_eq!(
            one,
            channel_name_for_terminal(&shared, "11111111-aaaa", "claude", "")
        );
    }

    #[test]
    fn short_names_keep_legacy_shape_without_hash() {
        // Under the cap nothing changes — existing channels keep
        // matching their stored names.
        assert_eq!(
            channel_name_for_workspace("github-acme-widget-186", ""),
            "github-acme-widget-186"
        );
    }

    #[test]
    fn channel_name_for_terminal_uses_only_hex_from_session_id() {
        // UUIDs come dash-separated; the slug helper would already
        // dash them out, but `channel_name_for_terminal` shortens by
        // filtering hex digits first so the truncated form looks like
        // `a3f1c277` and not `a3f1c27`. Pin that contract.
        let n =
            channel_name_for_terminal("ws", "a3f1c277-9abc-4d51-8f01-deadbeef0001", "claude", "");
        assert!(n.contains("a3f1c277"));
    }
}
