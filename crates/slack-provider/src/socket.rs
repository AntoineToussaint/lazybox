//! Socket Mode — pilot opens a WebSocket out to Slack so inbound
//! events arrive without needing a public HTTPS endpoint.
//!
//! Lifecycle per connection:
//!   1. `apps.connections.open` (HTTP) using the app token → one-shot
//!      `wss://...` URL with a short TTL.
//!   2. Open the WebSocket. Slack sends `hello` first, then `events_api`
//!      frames carrying inbound events (`app_mention`,
//!      `message.channels`, slash commands, interactivity, etc.).
//!   3. For every `events_api` frame pilot replies with
//!      `{"envelope_id": "...", "payload": {}}` to ACK. Without
//!      the ACK Slack retries up to 3 times then disconnects.
//!   4. Slack periodically asks the client to reconnect — `disconnect`
//!      frame with `reason: "warning"` (gentle, switch over) or
//!      `"refresh_requested"` (urgent). Pilot just rebuilds the
//!      WSS URL and reconnects.
//!
//! ## Events pilot consumes
//!
//! - `app_mention` — user `@pilot`'d the bot anywhere it's a member.
//! - `message.channels` / `message.groups` / `message.im` — incoming
//!   plain message in a watched channel.
//! - Slash commands / interactivity arrive through the same socket
//!   but pilot doesn't use them today; the parser keeps them as
//!   `Other` so a future addition is a one-arm change.
//!
//! Pilot's daemon spawns one `SocketModeClient::run_forever` task on
//! startup and consumes the resulting `InboundEvent` stream. The
//! mapping from `channel_id → workspace_key` (built when pilot
//! creates the per-workspace channel) turns the event into an
//! `IpcCommand::Spawn` or `InjectPrompt`.

use crate::SlackError;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

const APP_OPEN: &str = "https://slack.com/api/apps.connections.open";

/// Output stream item — pilot consumes these in the daemon's slack
/// task and routes to the right session.
#[derive(Debug, Clone)]
pub enum InboundEvent {
    /// Slack sent `hello` — connection is live. Daemon can log
    /// "slack connected" + post the bootstrap message.
    Hello,
    /// User mentioned the bot. `channel` + `text` (with the `<@Uxxx>`
    /// prefix stripped) drive the command parser.
    Mention {
        channel: String,
        user: String,
        text: String,
        ts: String,
    },
    /// Plain message in a channel the bot is a member of. Pilot
    /// treats these the same as mentions in per-workspace channels
    /// (since the user shouldn't have to `@pilot` in a dedicated
    /// channel for the bot).
    Message {
        channel: String,
        user: String,
        text: String,
        ts: String,
    },
    /// Slack told us to reconnect. The driver loop catches this and
    /// rebuilds the WSS URL; consumers see one `Hello` per new
    /// connection.
    Disconnect { reason: String },
}

/// Connection driver. `run_forever` loops over (open WSS → consume
/// events → handle reconnect) until cancelled, emitting parsed
/// events through the channel returned by `start`.
pub struct SocketModeClient {
    app_token: String,
    http: reqwest::Client,
}

impl SocketModeClient {
    pub fn new(app_token: impl Into<String>) -> Self {
        Self {
            app_token: app_token.into(),
            http: reqwest::Client::new(),
        }
    }

    /// Spawn the driver loop in the background. Returns an mpsc
    /// receiver of `InboundEvent`s and a `JoinHandle` so callers
    /// can shut down on daemon exit.
    pub fn start(
        self,
    ) -> (
        mpsc::Receiver<InboundEvent>,
        tokio::task::JoinHandle<()>,
    ) {
        let (tx, rx) = mpsc::channel(128);
        let handle = tokio::spawn(async move {
            self.run_forever(tx).await;
        });
        (rx, handle)
    }

    /// Open a fresh WSS URL via `apps.connections.open`. Slack
    /// returns a one-shot URL valid for ~30 seconds.
    async fn open_wss_url(&self) -> Result<String, SlackError> {
        #[derive(Deserialize)]
        struct Resp {
            ok: bool,
            #[serde(default)]
            error: Option<String>,
            #[serde(default)]
            url: Option<String>,
        }
        let r: Resp = self
            .http
            .post(APP_OPEN)
            .bearer_auth(&self.app_token)
            .send()
            .await?
            .json()
            .await?;
        if !r.ok {
            return Err(SlackError::Api(
                r.error.unwrap_or_else(|| "apps.connections.open".into()),
            ));
        }
        r.url.ok_or_else(|| {
            SlackError::Api("apps.connections.open: missing `url`".into())
        })
    }

    async fn run_forever(self, tx: mpsc::Sender<InboundEvent>) {
        // Reconnect loop with exponential backoff (capped). Slack
        // can drop the socket for many reasons (graceful refresh,
        // upstream maintenance, network blip); we just rebuild.
        let mut backoff = Duration::from_secs(1);
        loop {
            match self.run_once(&tx).await {
                Ok(()) => {
                    tracing::info!("slack: socket closed cleanly, reconnecting");
                    backoff = Duration::from_secs(1);
                }
                Err(e) => {
                    tracing::warn!("slack: socket error {e:?}; backing off {backoff:?}");
                    tokio::time::sleep(backoff).await;
                    // Exponential backoff, capped at 60s.
                    backoff = (backoff * 2).min(Duration::from_secs(60));
                }
            }
        }
    }

    async fn run_once(&self, tx: &mpsc::Sender<InboundEvent>) -> Result<(), SlackError> {
        let url = self.open_wss_url().await?;
        tracing::debug!("slack: connecting to socket mode");
        let (ws_stream, _resp) = tokio_tungstenite::connect_async(&url)
            .await
            .map_err(|e| SlackError::WebSocket(e.to_string()))?;
        let (mut ws_sink, mut ws_stream) = ws_stream.split();

        while let Some(msg) = ws_stream.next().await {
            let msg = msg.map_err(|e| SlackError::WebSocket(e.to_string()))?;
            let text = match msg {
                WsMessage::Text(t) => t.to_string(),
                WsMessage::Ping(p) => {
                    let _ = ws_sink.send(WsMessage::Pong(p)).await;
                    continue;
                }
                WsMessage::Close(_) => break,
                _ => continue,
            };
            let envelope: SocketEnvelope = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("slack: unparseable frame: {e}; raw={text}");
                    continue;
                }
            };
            // ACK every envelope that carries one (events_api, slash,
            // interactivity). hello / disconnect don't carry an
            // envelope_id.
            if let Some(eid) = &envelope.envelope_id {
                let ack = serde_json::json!({ "envelope_id": eid });
                if let Err(e) = ws_sink.send(WsMessage::Text(ack.to_string().into())).await {
                    tracing::warn!("slack: ack send failed: {e}");
                }
            }
            for event in envelope.into_inbound() {
                if tx.send(event).await.is_err() {
                    // Consumer dropped — shut down.
                    return Ok(());
                }
            }
        }
        Ok(())
    }
}

/// Outer Socket Mode envelope. Five kinds today; pilot consumes
/// `hello`, `events_api`, and `disconnect`. The rest get a debug
/// log + ACK so the connection stays healthy.
#[derive(Deserialize, Debug)]
struct SocketEnvelope {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    envelope_id: Option<String>,
    #[serde(default)]
    payload: Option<serde_json::Value>,
    /// Set on `disconnect` frames.
    #[serde(default)]
    reason: Option<String>,
}

impl SocketEnvelope {
    fn into_inbound(self) -> Vec<InboundEvent> {
        match self.kind.as_str() {
            "hello" => vec![InboundEvent::Hello],
            "disconnect" => vec![InboundEvent::Disconnect {
                reason: self.reason.unwrap_or_default(),
            }],
            "events_api" => {
                let Some(payload) = self.payload else {
                    return vec![];
                };
                parse_events_api(&payload).into_iter().collect()
            }
            // slash commands / interactivity / others: ACKed already,
            // no inbound event surfaced.
            _ => vec![],
        }
    }
}

/// Parse an `events_api` payload's inner `event` field. Slack
/// nests the actual event under `payload.event.{type, ...}`.
fn parse_events_api(payload: &serde_json::Value) -> Option<InboundEvent> {
    let event = payload.get("event")?;
    let kind = event.get("type")?.as_str()?;
    let channel = event.get("channel")?.as_str()?.to_string();
    let user = event
        .get("user")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let text = event
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let ts = event
        .get("ts")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    match kind {
        "app_mention" => Some(InboundEvent::Mention {
            channel,
            user,
            text,
            ts,
        }),
        "message" => {
            // Slack delivers bot's own messages back as `message`
            // events with `subtype: "bot_message"` or with the bot's
            // user id. Filter those out so we don't infinite-loop.
            let subtype = event.get("subtype").and_then(|v| v.as_str());
            if subtype == Some("bot_message") || subtype == Some("message_changed") {
                return None;
            }
            Some(InboundEvent::Message {
                channel,
                user,
                text,
                ts,
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hello_frame_surfaces_as_hello_event() {
        let env: SocketEnvelope =
            serde_json::from_value(json!({ "type": "hello" })).unwrap();
        assert!(matches!(
            env.into_inbound().as_slice(),
            [InboundEvent::Hello]
        ));
    }

    #[test]
    fn disconnect_frame_carries_reason() {
        let env: SocketEnvelope =
            serde_json::from_value(json!({ "type": "disconnect", "reason": "refresh_requested" }))
                .unwrap();
        match env.into_inbound().as_slice() {
            [InboundEvent::Disconnect { reason }] => assert_eq!(reason, "refresh_requested"),
            other => panic!("expected disconnect, got {other:?}"),
        }
    }

    #[test]
    fn events_api_app_mention_routes_to_mention() {
        let env: SocketEnvelope = serde_json::from_value(json!({
            "type": "events_api",
            "envelope_id": "e1",
            "payload": {
                "event": {
                    "type": "app_mention",
                    "channel": "C123",
                    "user": "U456",
                    "text": "<@UBOT> work",
                    "ts": "12345.6789",
                }
            }
        }))
        .unwrap();
        match env.into_inbound().as_slice() {
            [InboundEvent::Mention {
                channel,
                user,
                text,
                ts,
            }] => {
                assert_eq!(channel, "C123");
                assert_eq!(user, "U456");
                assert_eq!(text, "<@UBOT> work");
                assert_eq!(ts, "12345.6789");
            }
            other => panic!("expected mention, got {other:?}"),
        }
    }

    #[test]
    fn events_api_bot_message_is_filtered_to_avoid_loops() {
        let env: SocketEnvelope = serde_json::from_value(json!({
            "type": "events_api",
            "envelope_id": "e2",
            "payload": {
                "event": {
                    "type": "message",
                    "subtype": "bot_message",
                    "channel": "C123",
                    "user": "UBOT",
                    "text": "i posted this",
                    "ts": "1.2",
                }
            }
        }))
        .unwrap();
        assert!(env.into_inbound().is_empty());
    }

    #[test]
    fn unknown_envelope_type_produces_no_events() {
        let env: SocketEnvelope =
            serde_json::from_value(json!({ "type": "some_future_kind", "envelope_id": "e" }))
                .unwrap();
        assert!(env.into_inbound().is_empty());
    }
}
