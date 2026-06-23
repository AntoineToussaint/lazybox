//! Slack adapter. Implements [`crate::chat::ChatProvider`] over the
//! `lazybox-slack` crate's HTTP + Socket Mode clients.
//!
//! Boot flow:
//!
//! 1. **Auth** — `auth.test` round-trips the bot token + reads the
//!    bot's user id (used to strip self-mentions on inbound).
//! 2. **Channel cache** — `conversations.list` once at startup builds
//!    a `channel_name → channel_id` map so subsequent
//!    `ensure_workspace_channel` calls avoid a network round-trip
//!    when the channel already exists.
//! 3. **Anchor hello** — post a one-shot "lazybox online" line in
//!    `slack.anchor_channel` so the user sees the bot came up.
//! 4. **Drive** — spawn one task that selects on the bus (outbound)
//!    and the Socket Mode inbound stream, delegating both to
//!    [`crate::chat`].

use crate::ServerConfig;
use crate::chat::{self, ChatError, ChatInbound, ChatProvider, RouterState, TerminalTarget};
use lazybox_config::SlackConfig;
use lazybox_slack::api::{Client as ApiClient, Message, channel_name_for_terminal};
use lazybox_slack::socket::{InboundEvent, SocketModeClient};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::broadcast;

/// Provider-only slice of [`SlackConfig`] — just the routing knobs
/// the dispatcher needs. Built from the full config at construction
/// so the provider never holds the bot/app tokens after the
/// credential resolver is done with them.
#[derive(Debug, Clone)]
struct ProviderCfg {
    channel_prefix: String,
    per_workspace_channels: bool,
    /// Slack user ids allowed to drive agent PTYs from chat. Empty =
    /// allow everyone (the dispatcher logs a one-time warning at
    /// startup recommending it be set).
    allowed_users: Vec<String>,
}

/// Slack-specific provider state. Wraps the API client + a name→id
/// channel cache + the resolved bot identity.
pub struct SlackProvider {
    api: ApiClient,
    cfg: ProviderCfg,
    bot_user_id: String,
    /// `channel_name → channel_id` cache. Populated at boot from
    /// `conversations.list`; updated when lazybox creates a new
    /// channel. Plain `std::sync::Mutex` — guards never span an
    /// `.await`, so the async-aware variant would be overkill.
    name_to_id: std::sync::Mutex<HashMap<String, String>>,
    /// Pre-resolved id of `cfg.anchor_channel`. Set when the channel
    /// is visible to the bot at boot. Required for thread-fallback
    /// mode (`per_workspace_channels: false`); when missing, the
    /// provider degrades to "no chat surface" and logs a warning at
    /// startup so the user knows to `/invite @lazybox`.
    anchor_channel_id: Option<String>,
}

impl SlackProvider {
    pub fn new(
        api: ApiClient,
        cfg: SlackConfig,
        bot_user_id: String,
        seed: HashMap<String, String>,
    ) -> Self {
        let anchor_channel_id = seed.get(&cfg.anchor_channel).cloned();
        Self {
            api,
            cfg: ProviderCfg {
                channel_prefix: cfg.channel_prefix,
                per_workspace_channels: cfg.per_workspace_channels,
                allowed_users: cfg.allowed_users,
            },
            bot_user_id,
            name_to_id: std::sync::Mutex::new(seed),
            anchor_channel_id,
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

    /// `slack.allowed_users` gate. Empty list = allow all (warned
    /// about once at startup in `run`); non-empty = only those user
    /// ids may drive an agent PTY. Read-only status queries are
    /// exempted upstream in the chat dispatcher.
    fn is_user_allowed(&self, user: &str) -> bool {
        self.cfg.allowed_users.is_empty() || self.cfg.allowed_users.iter().any(|u| u == user)
    }

    fn post<'a>(
        &'a self,
        channel: &'a str,
        body: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, ChatError>> + Send + 'a>> {
        Box::pin(async move {
            self.api
                .post_message(&Message::new(channel.to_string(), body.to_string()))
                .await
                .map(|resp| resp.ts)
                .map_err(|e| ChatError::Provider(e.to_string()))
        })
    }

    fn post_to_thread<'a>(
        &'a self,
        channel: &'a str,
        thread_ts: &'a str,
        body: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), ChatError>> + Send + 'a>> {
        Box::pin(async move {
            self.api
                .post_message(
                    &Message::new(channel.to_string(), body.to_string()).in_thread(thread_ts),
                )
                .await
                .map(|_| ())
                .map_err(|e| ChatError::Provider(e.to_string()))
        })
    }

    fn terminal_target(
        &self,
        workspace_key: &str,
        session_id: &str,
        agent_id: &str,
    ) -> TerminalTarget {
        if self.cfg.per_workspace_channels {
            return TerminalTarget::DedicatedChannel(channel_name_for_terminal(
                workspace_key,
                session_id,
                agent_id,
                &self.cfg.channel_prefix,
            ));
        }
        // `per_workspace_channels: false`: anchor a thread in the
        // shared `#lazybox`-style channel. If the bot can't see that
        // channel (no `/invite`) there's nowhere to post — drop the
        // surface rather than fail every notification with a 404.
        match self.anchor_channel_id.as_ref() {
            Some(id) => TerminalTarget::AnchorThread(id.clone()),
            None => TerminalTarget::NoSurface,
        }
    }

    fn ensure_channel<'a>(
        &'a self,
        name: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, ChatError>> + Send + 'a>> {
        Box::pin(async move {
            if let Some(id) = self
                .name_to_id
                .lock()
                .expect("slack name_to_id mutex poisoned")
                .get(name)
                .cloned()
            {
                return Ok(id);
            }
            match self.api.conversations_create(name).await {
                Ok(resp) => {
                    let id = resp.channel.id.clone();
                    self.name_to_id
                        .lock()
                        .expect("slack name_to_id mutex poisoned")
                        .insert(name.to_string(), id.clone());
                    tracing::info!(channel = %resp.channel.name, "slack: created channel");
                    Ok(id)
                }
                Err(lazybox_slack::SlackError::Api(ref e)) if e == "name_taken" => {
                    // Race: someone (us, in a prior session?) made
                    // the channel between our list + create. Refetch
                    // and use the existing id.
                    tracing::debug!(name = %name, "slack: channel exists, looking up id");
                    match self.api.conversations_list_all().await {
                        Ok(channels) => {
                            let mut s = self
                                .name_to_id
                                .lock()
                                .expect("slack name_to_id mutex poisoned");
                            for c in &channels {
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
) -> Result<(), lazybox_slack::SlackError> {
    let api = ApiClient::new(bot_token);

    // ── Boot ──────────────────────────────────────────────────────
    let auth = api.auth_test().await?;
    tracing::info!(team = %auth.team, user = %auth.user, "slack: connected");

    // Walk every page of the channel listing. The provider seed below
    // uses this cache so subsequent ensure_workspace_channel calls
    // don't HTTP; a workspace with >1000 channels must not miss any.
    let channels = api.conversations_list_all().await?;
    let mut seed = HashMap::new();
    for c in &channels {
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
                    "*lazybox online* · connected as <@{}>. Mirroring {} project(s).",
                    auth.user_id,
                    channels.len()
                ),
            ))
            .await;
    } else {
        tracing::warn!(
            anchor_channel = %slack.anchor_channel,
            "slack: anchor channel not visible; /invite @lazybox to that channel \
             so it can post bootstrap messages",
        );
    }

    // ── Plumbing ──────────────────────────────────────────────────
    if slack.allowed_users.is_empty() {
        tracing::warn!(
            "slack: `slack.allowed_users` is empty — every member of a routed \
             channel can drive the agent PTY. Set `slack.allowed_users: [U…]` \
             in config.yaml to restrict who may send agent input",
        );
    }
    let provider: Arc<dyn ChatProvider> =
        Arc::new(SlackProvider::new(api, slack, auth.user_id.clone(), seed));
    let state = Arc::new(tokio::sync::Mutex::new(RouterState::new()));

    // ── Inbound socket ────────────────────────────────────────────
    let (inbound_rx, _socket_handle) = SocketModeClient::new(app_token).start();

    // ── Outbound bus ──────────────────────────────────────────────
    let bus_rx = config.bus.subscribe();

    let bot_user_id = auth.user_id.clone();
    drive(provider, config, state, bus_rx, inbound_rx, &bot_user_id).await;
    Ok(())
}

/// Drive both halves from one task — `select!` over the two streams
/// keeps state ownership behind the shared `RouterState` mutex.
/// Returns when either stream is exhausted (bus closed or inbound
/// socket gone).
///
/// Events whose handling performs Slack HTTP (`TerminalSpawned`
/// channel creation + header post, `AgentState` notifications) run
/// on detached tasks so a slow `slack.com` round-trip can't stall
/// the select loop into broadcast lag. Trade-off, accepted and
/// deliberate: status-style posts may reorder relative to each
/// other, and a notification racing its own terminal's spawn may be
/// dropped (the router map isn't populated yet) — the lag-recovery
/// rescan plus the next notification self-heal both cases. Cheap
/// state-only bookkeeping (`TerminalOutput`, `TerminalExited`) stays
/// inline to preserve its ordering.
async fn drive(
    provider: Arc<dyn ChatProvider>,
    config: ServerConfig,
    state: Arc<tokio::sync::Mutex<RouterState>>,
    mut bus_rx: broadcast::Receiver<lazybox_ipc::Event>,
    mut inbound_rx: tokio::sync::mpsc::Receiver<InboundEvent>,
    bot_user_id: &str,
) {
    use lazybox_ipc::Event;
    loop {
        tokio::select! {
            biased;
            evt = bus_rx.recv() => {
                match evt {
                    Ok(e @ (Event::TerminalSpawned { .. } | Event::AgentState { .. })) => {
                        // HTTP-bearing handling — off the loop.
                        let provider = provider.clone();
                        let config = config.clone();
                        let state = state.clone();
                        tokio::spawn(async move {
                            chat::handle_bus_event(&*provider, &config, &state, e).await;
                        });
                    }
                    Ok(e) => chat::handle_bus_event(&*provider, &config, &state, e).await,
                    // Lagged: the broadcast channel dropped events. A
                    // dropped `TerminalSpawned` would orphan that
                    // terminal from chat forever — rescan the live
                    // terminal metadata and re-attach anything missed.
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(missed = n, "slack: bus lagged; rescanning live terminals");
                        chat::resync_spawned_terminals(&*provider, &config, &state).await;
                        continue;
                    }
                    // Closed: the bus is gone (shutdown) — stop the loop instead
                    // of spinning on an arm that will never block again.
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            msg = inbound_rx.recv() => {
                let Some(msg) = msg else { break };
                if let Some(normalized) = map_inbound(msg, bot_user_id) {
                    chat::handle_inbound(&*provider, &config, &state, normalized).await;
                }
            }
        }
    }
}

/// Normalize Slack's wire-level inbound shape into the chat layer's
/// provider-agnostic [`ChatInbound`]. Hello → Connected; disconnect
/// → Disconnected; mention + message both → Message (the chat layer
/// doesn't care which Slack event delivered the text).
///
/// Messages whose author is the bot itself are dropped — a second line
/// of defense behind `socket.rs`'s `bot_id` filter against lazybox's own
/// posts routing back into the agent.
fn map_inbound(ev: InboundEvent, bot_user_id: &str) -> Option<ChatInbound> {
    match ev {
        InboundEvent::Hello => Some(ChatInbound::Connected),
        InboundEvent::Disconnect { reason } => Some(ChatInbound::Disconnected { reason }),
        InboundEvent::Mention {
            channel,
            user,
            text,
            ts,
            thread_ts,
        }
        | InboundEvent::Message {
            channel,
            user,
            text,
            ts,
            thread_ts,
        } => {
            if user == bot_user_id {
                return None;
            }
            Some(ChatInbound::Message {
                channel,
                user,
                text,
                ts,
                thread_ts,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lazybox_config::SlackConfig;

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

    #[tokio::test]
    async fn drive_returns_when_bus_closes() {
        use lazybox_store::MemoryStore;

        let provider: Arc<dyn ChatProvider> = Arc::new(provider());
        let config = ServerConfig::with_store(Arc::new(MemoryStore::new()));
        let state = Arc::new(tokio::sync::Mutex::new(RouterState::new()));

        // A closed bus: drop the only sender so `recv()` yields `Closed`.
        let (bus_tx, bus_rx) = broadcast::channel::<lazybox_ipc::Event>(8);
        drop(bus_tx);

        // Keep the inbound sender alive so that arm never fires — the
        // bus arm (polled first under `biased`) is what must stop the
        // loop. Before the fix a `Closed` bus made the loop `continue`
        // and spin forever, so this timeout would elapse.
        let (_inbound_tx, inbound_rx) = tokio::sync::mpsc::channel(8);

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            drive(provider, config, state, bus_rx, inbound_rx, "UBOT"),
        )
        .await
        .expect("drive should return promptly once the bus closes");
    }

    #[test]
    fn is_user_allowed_empty_list_allows_everyone() {
        let p = provider();
        assert!(p.is_user_allowed("U-anyone"));
    }

    #[test]
    fn is_user_allowed_nonempty_list_restricts() {
        let p = SlackProvider::new(
            ApiClient::new("xoxb-test".to_string()),
            SlackConfig {
                allowed_users: vec!["U111".into(), "U222".into()],
                ..SlackConfig::default()
            },
            "UBOT".to_string(),
            HashMap::new(),
        );
        assert!(p.is_user_allowed("U111"));
        assert!(p.is_user_allowed("U222"));
        assert!(!p.is_user_allowed("U-intruder"));
    }

    #[test]
    fn map_inbound_hello_becomes_connected() {
        let out = map_inbound(InboundEvent::Hello, "UBOT").unwrap();
        assert!(matches!(out, ChatInbound::Connected));
    }

    #[test]
    fn map_inbound_disconnect_carries_reason() {
        let out = map_inbound(
            InboundEvent::Disconnect {
                reason: "refresh_requested".to_string(),
            },
            "UBOT",
        )
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
        let mention = map_inbound(
            InboundEvent::Mention {
                channel: "C1".into(),
                user: "U1".into(),
                text: "hi".into(),
                ts: "1.0".into(),
                thread_ts: None,
            },
            "UBOT",
        )
        .unwrap();
        let message = map_inbound(
            InboundEvent::Message {
                channel: "C1".into(),
                user: "U1".into(),
                text: "hi".into(),
                ts: "1.0".into(),
                thread_ts: None,
            },
            "UBOT",
        )
        .unwrap();
        assert!(matches!(mention, ChatInbound::Message { .. }));
        assert!(matches!(message, ChatInbound::Message { .. }));
    }

    #[test]
    fn map_inbound_drops_messages_authored_by_the_bot() {
        let out = map_inbound(
            InboundEvent::Message {
                channel: "C1".into(),
                user: "UBOT".into(),
                text: "needs input".into(),
                ts: "1.0".into(),
                thread_ts: None,
            },
            "UBOT",
        );
        assert!(out.is_none());
    }

    #[test]
    fn map_inbound_carries_thread_ts_through() {
        let out = map_inbound(
            InboundEvent::Message {
                channel: "C1".into(),
                user: "U1".into(),
                text: "yes".into(),
                ts: "2.0".into(),
                thread_ts: Some("1.0".into()),
            },
            "UBOT",
        )
        .unwrap();
        match out {
            ChatInbound::Message {
                thread_ts: Some(t), ..
            } => assert_eq!(t, "1.0"),
            other => panic!("expected threaded message, got {other:?}"),
        }
    }

    #[test]
    fn terminal_target_dedicated_when_per_workspace_channels() {
        let p = SlackProvider::new(
            ApiClient::new("xoxb-test".to_string()),
            SlackConfig {
                per_workspace_channels: true,
                ..SlackConfig::default()
            },
            "UBOT".to_string(),
            HashMap::new(),
        );
        let target = p.terminal_target("github-acme-widget-186", "a3f1c277-9abc", "claude");
        match target {
            TerminalTarget::DedicatedChannel(name) => {
                assert!(name.contains("github-acme-widget-186"));
                assert!(name.contains("claude"));
            }
            other => panic!("expected DedicatedChannel, got {other:?}"),
        }
    }

    #[test]
    fn terminal_target_anchor_thread_when_per_workspace_channels_false() {
        let mut seed = HashMap::new();
        seed.insert("lazybox".to_string(), "C-ANCHOR".to_string());
        let p = SlackProvider::new(
            ApiClient::new("xoxb-test".to_string()),
            SlackConfig {
                per_workspace_channels: false,
                anchor_channel: "lazybox".into(),
                ..SlackConfig::default()
            },
            "UBOT".to_string(),
            seed,
        );
        let target = p.terminal_target("ws", "sess", "claude");
        match target {
            TerminalTarget::AnchorThread(id) => assert_eq!(id, "C-ANCHOR"),
            other => panic!("expected AnchorThread, got {other:?}"),
        }
    }

    #[test]
    fn terminal_target_none_when_anchor_channel_not_visible() {
        let p = SlackProvider::new(
            ApiClient::new("xoxb-test".to_string()),
            SlackConfig {
                per_workspace_channels: false,
                anchor_channel: "lazybox".into(),
                ..SlackConfig::default()
            },
            "UBOT".to_string(),
            HashMap::new(),
        );
        let target = p.terminal_target("ws", "sess", "claude");
        assert!(matches!(target, TerminalTarget::NoSurface));
    }
}
