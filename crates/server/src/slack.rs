//! Slack adapter. Implements [`crate::chat::ChatProvider`] over the
//! `pilot-slack` crate's HTTP + Socket Mode clients.
//!
//! Boot flow:
//!
//! 1. **Auth** — `auth.test` round-trips the bot token + reads the
//!    bot's user id (used to strip self-mentions on inbound).
//! 2. **Channel cache** — `conversations.list` once at startup builds
//!    a `channel_name → channel_id` map so subsequent
//!    `ensure_workspace_channel` calls avoid a network round-trip
//!    when the channel already exists.
//! 3. **Anchor hello** — post a one-shot "pilot online" line in
//!    `slack.anchor_channel` so the user sees the bot came up.
//! 4. **Drive** — spawn one task that selects on the bus (outbound)
//!    and the Socket Mode inbound stream, delegating both to
//!    [`crate::chat`].

use crate::ServerConfig;
use crate::chat::{self, ChatError, ChatInbound, ChatProvider, RouterState};
use pilot_config::SlackConfig;
use pilot_slack::api::{Client as ApiClient, Message, channel_name_for_terminal};
use pilot_slack::socket::{InboundEvent, SocketModeClient};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Slack-specific provider state. Wraps the API client + a name→id
/// channel cache + the resolved bot identity.
pub struct SlackProvider {
    api: ApiClient,
    cfg: SlackConfig,
    bot_user_id: String,
    /// `channel_name → channel_id` cache. Populated at boot from
    /// `conversations.list`; updated when pilot creates a new
    /// channel.
    name_to_id: Mutex<HashMap<String, String>>,
}

impl SlackProvider {
    pub fn new(
        api: ApiClient,
        cfg: SlackConfig,
        bot_user_id: String,
        seed: HashMap<String, String>,
    ) -> Self {
        Self {
            api,
            cfg,
            bot_user_id,
            name_to_id: Mutex::new(seed),
        }
    }
}

impl ChatProvider for SlackProvider {
    fn id(&self) -> &'static str {
        "slack"
    }

    fn strip_self_mention<'a>(&self, text: &'a str) -> &'a str {
        let prefix = format!("<@{}>", self.bot_user_id);
        text.strip_prefix(prefix.as_str())
            .map(str::trim_start)
            .unwrap_or(text)
    }

    fn post<'a>(
        &'a self,
        channel: &'a str,
        body: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), ChatError>> + Send + 'a>> {
        Box::pin(async move {
            self.api
                .post_message(&Message::new(channel.to_string(), body.to_string()))
                .await
                .map(|_| ())
                .map_err(|e| ChatError::Provider(e.to_string()))
        })
    }

    fn channel_name(
        &self,
        workspace_key: &str,
        session_id: &str,
        agent_id: &str,
    ) -> Option<String> {
        // `per_workspace_channels: false` means "don't auto-create
        // per-(session, agent) channels" — outbound notifications
        // are dropped. The user wanted everything in `#pilot` in
        // that mode; today we just stay silent there.
        if !self.cfg.per_workspace_channels {
            return None;
        }
        Some(channel_name_for_terminal(
            workspace_key,
            session_id,
            agent_id,
            &self.cfg.channel_prefix,
        ))
    }

    fn ensure_channel<'a>(
        &'a self,
        name: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, ChatError>> + Send + 'a>> {
        Box::pin(async move {
            if let Some(id) = self.name_to_id.lock().await.get(name).cloned() {
                return Ok(id);
            }
            match self.api.conversations_create(name).await {
                Ok(resp) => {
                    let id = resp.channel.id.clone();
                    self.name_to_id
                        .lock()
                        .await
                        .insert(name.to_string(), id.clone());
                    tracing::info!(channel = %resp.channel.name, "slack: created channel");
                    Ok(id)
                }
                Err(pilot_slack::SlackError::Api(ref e)) if e == "name_taken" => {
                    // Race: someone (us, in a prior session?) made
                    // the channel between our list + create. Refetch
                    // and use the existing id.
                    tracing::debug!(name = %name, "slack: channel exists, looking up id");
                    match self.api.conversations_list(1000).await {
                        Ok(listing) => {
                            let mut s = self.name_to_id.lock().await;
                            for c in &listing.channels {
                                s.insert(c.name.clone(), c.id.clone());
                            }
                            s.get(name).cloned().ok_or_else(|| {
                                ChatError::Provider(format!(
                                    "name_taken but channel `{name}` not in refreshed listing"
                                ))
                            })
                        }
                        Err(e) => Err(ChatError::Provider(e.to_string())),
                    }
                }
                Err(e) => Err(ChatError::Provider(e.to_string())),
            }
        })
    }
}

/// Spawn the Slack task. Returns immediately; the task drives
/// itself in the background. If config is missing tokens, this is
/// a no-op (caller doesn't need to check).
pub fn spawn(config: ServerConfig, slack: SlackConfig) -> Option<tokio::task::JoinHandle<()>> {
    let bot_token = resolve_token(slack.bot_token.as_deref(), "SLACK_BOT_TOKEN")?;
    let app_token = resolve_token(slack.app_token.as_deref(), "SLACK_APP_TOKEN")?;
    Some(tokio::spawn(async move {
        if let Err(e) = run(config, slack, bot_token, app_token).await {
            tracing::error!("slack task exited with error: {e:?}");
        }
    }))
}

/// Env wins over YAML so credentials don't have to live on disk.
fn resolve_token(yaml: Option<&str>, env_key: &str) -> Option<String> {
    if let Ok(v) = std::env::var(env_key) {
        if !v.trim().is_empty() {
            return Some(v);
        }
    }
    yaml.filter(|s| !s.trim().is_empty()).map(str::to_string)
}

async fn run(
    config: ServerConfig,
    slack: SlackConfig,
    bot_token: String,
    app_token: String,
) -> Result<(), pilot_slack::SlackError> {
    let api = ApiClient::new(bot_token);

    // ── Boot ──────────────────────────────────────────────────────
    let auth = api.auth_test().await?;
    tracing::info!(team = %auth.team, user = %auth.user, "slack: connected");

    // Page through ~1000 channels. The provider seed below uses this
    // cache so subsequent ensure_workspace_channel calls don't HTTP.
    let listing = api.conversations_list(1000).await?;
    let mut seed = HashMap::new();
    for c in &listing.channels {
        seed.insert(c.name.clone(), c.id.clone());
    }
    tracing::info!(channels = seed.len(), "slack: prefetched channel listing");

    // Anchor-channel hello. Best-effort — if the channel isn't
    // visible the user needs to /invite the bot. Log clearly so
    // setup is debuggable.
    if let Some(anchor_id) = seed.get(&slack.anchor_channel).cloned() {
        let _ = api
            .post_message(&Message::new(
                anchor_id,
                format!(
                    "*pilot online* · connected as <@{}>. Mirroring {} project(s).",
                    auth.user_id,
                    listing.channels.len()
                ),
            ))
            .await;
    } else {
        tracing::warn!(
            anchor_channel = %slack.anchor_channel,
            "slack: anchor channel not visible; /invite @pilot to that channel \
             so it can post bootstrap messages",
        );
    }

    // ── Plumbing ──────────────────────────────────────────────────
    let provider: Arc<dyn ChatProvider> =
        Arc::new(SlackProvider::new(api, slack, auth.user_id.clone(), seed));
    let state = Arc::new(Mutex::new(RouterState::new()));

    // ── Inbound socket ────────────────────────────────────────────
    let (mut inbound_rx, _socket_handle) = SocketModeClient::new(app_token).start();

    // ── Outbound bus ──────────────────────────────────────────────
    let mut bus_rx = config.bus.subscribe();

    // Drive both halves from one task — `select!` over the two
    // streams keeps state ownership single-threaded.
    loop {
        tokio::select! {
            biased;
            evt = bus_rx.recv() => {
                match evt {
                    Ok(e) => chat::handle_bus_event(&*provider, &config, &state, e).await,
                    Err(_) => continue, // lagged — broadcast channel skipped events
                }
            }
            msg = inbound_rx.recv() => {
                let Some(msg) = msg else { break };
                if let Some(normalized) = map_inbound(msg) {
                    chat::handle_inbound(&*provider, &config, &state, normalized).await;
                }
            }
        }
    }
    Ok(())
}

/// Normalize Slack's wire-level inbound shape into the chat layer's
/// provider-agnostic [`ChatInbound`]. Hello → Connected; disconnect
/// → Disconnected; mention + message both → Message (the chat layer
/// doesn't care which Slack event delivered the text).
fn map_inbound(ev: InboundEvent) -> Option<ChatInbound> {
    match ev {
        InboundEvent::Hello => Some(ChatInbound::Connected),
        InboundEvent::Disconnect { reason } => Some(ChatInbound::Disconnected { reason }),
        InboundEvent::Mention {
            channel,
            user,
            text,
            ts,
        }
        | InboundEvent::Message {
            channel,
            user,
            text,
            ts,
        } => Some(ChatInbound::Message {
            channel,
            user,
            text,
            ts,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pilot_config::SlackConfig;

    fn provider() -> SlackProvider {
        SlackProvider::new(
            ApiClient::new("xoxb-test".to_string()),
            SlackConfig::default(),
            "UBOT".to_string(),
            HashMap::new(),
        )
    }

    #[test]
    fn strip_self_mention_removes_leading_mention() {
        let p = provider();
        assert_eq!(p.strip_self_mention("<@UBOT> hello"), "hello");
    }

    #[test]
    fn strip_self_mention_leaves_text_alone_if_no_mention() {
        let p = provider();
        assert_eq!(p.strip_self_mention("just text"), "just text");
    }

    #[test]
    fn map_inbound_hello_becomes_connected() {
        let out = map_inbound(InboundEvent::Hello).unwrap();
        assert!(matches!(out, ChatInbound::Connected));
    }

    #[test]
    fn map_inbound_disconnect_carries_reason() {
        let out = map_inbound(InboundEvent::Disconnect {
            reason: "refresh_requested".to_string(),
        })
        .unwrap();
        match out {
            ChatInbound::Disconnected { reason } => {
                assert_eq!(reason, "refresh_requested")
            }
            _ => panic!("expected Disconnected"),
        }
    }

    #[test]
    fn map_inbound_mention_and_message_normalize_to_message() {
        let mention = map_inbound(InboundEvent::Mention {
            channel: "C1".into(),
            user: "U1".into(),
            text: "hi".into(),
            ts: "1.0".into(),
        })
        .unwrap();
        let message = map_inbound(InboundEvent::Message {
            channel: "C1".into(),
            user: "U1".into(),
            text: "hi".into(),
            ts: "1.0".into(),
        })
        .unwrap();
        assert!(matches!(mention, ChatInbound::Message { .. }));
        assert!(matches!(message, ChatInbound::Message { .. }));
    }
}
