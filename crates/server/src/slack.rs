//! Slack daemon glue. Single long-running task that:
//!
//! 1. **Boots** — `auth.test`, list channels, post a bootstrap line
//!    in the anchor channel. Caches `channel_name → channel_id`.
//! 2. **Outbound** — subscribes to the daemon's broadcast bus and
//!    fans events to Slack:
//!    - `WorkspaceUpserted`: ensure a per-workspace channel exists
//!      (auto-create on first sight) and post the PR / issue
//!      description.
//!    - `AgentState::Asking`: claude/codex/cursor is waiting on
//!      the user (a yes/no prompt, or "done — what next?"). Grab
//!      the recent PTY output and post it to the channel so the
//!      user can answer from their phone.
//!    - `TerminalSpawned` / `TerminalExited`: track
//!      `workspace_key → primary_terminal_id` so inbound replies
//!      have something to write to.
//! 3. **Inbound** — runs the [`SocketModeClient`] receiver and
//!    routes incoming `app_mention` / `message` events back into
//!    the workspace's primary agent terminal via
//!    `IpcCommand::Write { terminal_id, bytes: text + "\r" }`.

use crate::ServerConfig;
use pilot_config::SlackConfig;
use pilot_ipc::{AgentState, Event, TerminalId};
use pilot_slack::api::{Client as ApiClient, Message, channel_name_for_workspace};
use pilot_slack::socket::{InboundEvent, SocketModeClient};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// State shared between outbound (bus → Slack) and inbound (Slack →
/// PTY) halves. Built once at startup; both halves take `Arc` clones.
#[derive(Default)]
struct SlackState {
    /// `channel_name → channel_id` lookup. Populated at boot via
    /// `conversations.list` + updated when pilot creates a new
    /// channel.
    name_to_id: HashMap<String, String>,
    /// Reverse: `channel_id → workspace_key`. Built when pilot
    /// resolves a channel for a workspace; used by inbound to
    /// route a Slack message back to the right session.
    channel_to_workspace: HashMap<String, String>,
    /// `workspace_key → primary agent terminal_id`. Updated on
    /// `TerminalSpawned` / `TerminalExited`. Pilot writes inbound
    /// replies here. Falls back to any terminal in the workspace
    /// if no agent is tracked yet.
    workspace_to_terminal: HashMap<String, TerminalId>,
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
    let state = Arc::new(Mutex::new(SlackState::default()));

    // ── Boot ──────────────────────────────────────────────────────
    let auth = api.auth_test().await?;
    tracing::info!(
        team = %auth.team,
        user = %auth.user,
        "slack: connected"
    );

    // Page through up to ~1000 channels (one cursor follow-up is
    // enough for any plausible workspace). The reverse map of
    // channel_id → workspace_key starts empty; pilot fills it as
    // it resolves workspaces.
    let listing = api.conversations_list(1000).await?;
    {
        let mut s = state.lock().await;
        for c in &listing.channels {
            s.name_to_id.insert(c.name.clone(), c.id.clone());
        }
        tracing::info!(
            channels = s.name_to_id.len(),
            "slack: prefetched channel listing"
        );
    }

    // Anchor-channel hello. Best-effort — if the channel doesn't
    // exist yet the user needs to /invite the bot, so we post once
    // we find it AND log clearly when we don't.
    if let Some(anchor_id) = state.lock().await.name_to_id.get(&slack.anchor_channel).cloned() {
        let _ = api
            .post_message(&Message::new(
                anchor_id,
                format!(
                    "*pilot online* · connected as <@{}>. Mirroring {} project(s).",
                    auth.user_id, listing.channels.len()
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

    // ── Inbound socket ────────────────────────────────────────────
    let (mut inbound_rx, _socket_handle) = SocketModeClient::new(app_token).start();
    let bot_user_id = auth.user_id.clone();

    // ── Outbound bus ──────────────────────────────────────────────
    let mut bus_rx = config.bus.subscribe();

    // Drive both halves from one task — `select!` over the two
    // streams keeps state ownership single-threaded. State lives
    // behind an Arc<Mutex> so future workers (e.g. a separate
    // posting queue) can share it.
    loop {
        tokio::select! {
            biased;
            evt = bus_rx.recv() => {
                match evt {
                    Ok(e) => handle_bus_event(&api, &slack, &state, &config, e).await,
                    Err(_) => continue, // lagged — broadcast channel skipped events
                }
            }
            msg = inbound_rx.recv() => {
                let Some(msg) = msg else { break };
                handle_inbound(&config, &state, &bot_user_id, msg).await;
            }
        }
    }
    Ok(())
}

/// Bus → Slack fan-out. Each variant is one or two HTTP calls.
async fn handle_bus_event(
    api: &ApiClient,
    cfg: &SlackConfig,
    state: &Arc<Mutex<SlackState>>,
    server: &ServerConfig,
    event: Event,
) {
    match event {
        Event::WorkspaceUpserted(ws) => {
            // Only post on the first time we see a workspace — the
            // bus broadcasts on every read-state change too, and we
            // don't want to spam Slack with re-posts of the same PR
            // description.
            let workspace_key = ws.key.as_str().to_string();
            let already = state
                .lock()
                .await
                .channel_to_workspace
                .values()
                .any(|wk| wk == &workspace_key);
            if already {
                return;
            }
            let Some(channel_id) =
                ensure_channel_for_workspace(api, cfg, state, &workspace_key).await
            else {
                return;
            };
            let title = ws
                .primary_task()
                .map(|t| t.title.clone())
                .unwrap_or_else(|| ws.name.clone());
            let url = ws.primary_task().map(|t| t.url.clone());
            let body = match url {
                Some(u) => format!("📋 *{title}*\n<{u}>"),
                None => format!("📋 *{title}*"),
            };
            let _ = api.post_message(&Message::new(channel_id, body)).await;
        }
        Event::AgentState { session_key, state: agent_state } => {
            // Only post on Asking transitions — Active is the
            // default and Slack would drown in "now streaming"
            // messages otherwise.
            if agent_state != AgentState::Asking {
                return;
            }
            let workspace_key = session_key.as_str().to_string();
            let Some(channel_id) =
                ensure_channel_for_workspace(api, cfg, state, &workspace_key).await
            else {
                return;
            };
            let Some(terminal_id) = state
                .lock()
                .await
                .workspace_to_terminal
                .get(&workspace_key)
                .copied()
            else {
                // No agent terminal tracked yet — just post a heads-up.
                let _ = api
                    .post_message(&Message::new(
                        channel_id,
                        "⏸ agent is waiting on input".to_string(),
                    ))
                    .await;
                return;
            };
            // Pull recent PTY output for context. Last ~2 KB of the
            // ring buffer, stripped of ANSI + truncated to last 30
            // non-empty lines — enough to show the question without
            // dumping the whole conversation.
            let context = recent_terminal_text(server, terminal_id).await;
            let body = if context.is_empty() {
                "⏸ agent is waiting on input — reply in this channel to answer".to_string()
            } else {
                format!(
                    "⏸ *waiting on input* — reply in this channel to answer\n```\n{context}\n```"
                )
            };
            let _ = api.post_message(&Message::new(channel_id, body)).await;
        }
        Event::TerminalSpawned { terminal_id, session_key, kind, .. } => {
            // Track agent terminals only (shells aren't claude/codex/
            // cursor — sending Slack replies to a shell does the
            // wrong thing).
            if !matches!(kind, pilot_ipc::TerminalKind::Agent(_)) {
                return;
            }
            state
                .lock()
                .await
                .workspace_to_terminal
                .insert(session_key.as_str().to_string(), terminal_id);
        }
        Event::TerminalExited { terminal_id, .. } => {
            let mut s = state.lock().await;
            s.workspace_to_terminal
                .retain(|_, tid| *tid != terminal_id);
        }
        _ => {}
    }
}

/// Ensure pilot has a channel for `workspace_key`. Looks up the
/// cached name → id map first, creates the channel via
/// `conversations.create` on miss, handles the `name_taken` race
/// (someone else made the channel between our list + create) by
/// returning the existing id.
///
/// When `per_workspace_channels: false` is set in config, all
/// workspaces route to the anchor channel instead.
async fn ensure_channel_for_workspace(
    api: &ApiClient,
    cfg: &SlackConfig,
    state: &Arc<Mutex<SlackState>>,
    workspace_key: &str,
) -> Option<String> {
    if !cfg.per_workspace_channels {
        return state.lock().await.name_to_id.get(&cfg.anchor_channel).cloned();
    }
    let name = channel_name_for_workspace(workspace_key, &cfg.channel_prefix);
    if let Some(id) = state.lock().await.name_to_id.get(&name).cloned() {
        state
            .lock()
            .await
            .channel_to_workspace
            .insert(id.clone(), workspace_key.to_string());
        return Some(id);
    }
    match api.conversations_create(&name).await {
        Ok(resp) => {
            let id = resp.channel.id.clone();
            let mut s = state.lock().await;
            s.name_to_id.insert(name, id.clone());
            s.channel_to_workspace
                .insert(id.clone(), workspace_key.to_string());
            tracing::info!(channel = %resp.channel.name, "slack: created channel");
            Some(id)
        }
        Err(pilot_slack::SlackError::Api(ref e)) if e == "name_taken" => {
            // Race: someone (us, in a prior session?) made the
            // channel between our list call + this create. Refetch
            // the listing and use the existing id.
            tracing::debug!(name = %name, "slack: channel exists, looking up id");
            if let Ok(listing) = api.conversations_list(1000).await {
                let mut s = state.lock().await;
                for c in &listing.channels {
                    s.name_to_id.insert(c.name.clone(), c.id.clone());
                }
                if let Some(id) = s.name_to_id.get(&name).cloned() {
                    s.channel_to_workspace
                        .insert(id.clone(), workspace_key.to_string());
                    return Some(id);
                }
            }
            None
        }
        Err(e) => {
            tracing::warn!(name = %name, error = ?e, "slack: conversations.create failed");
            None
        }
    }
}

/// Pull the last ~2 KB of the terminal's ring buffer, strip ANSI,
/// drop trailing blank lines, and return up to the last 30
/// non-empty lines. Used as the "context" block in Slack
/// "waiting on input" messages so the user can see what claude is
/// asking.
async fn recent_terminal_text(server: &ServerConfig, terminal_id: TerminalId) -> String {
    let backend_key = {
        let terminals = server.terminals.lock().await;
        match terminals.get(&terminal_id) {
            Some(k) => k.clone(),
            None => return String::new(),
        }
    };
    let (mut raw, _seq) = match server.backend.snapshot(&backend_key).await {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(?terminal_id, "slack: backend.snapshot failed: {e:?}");
            return String::new();
        }
    };
    // Tail to ~2 KB so a fresh ring of 64KB doesn't post a wall of
    // text. The last bytes are the most recent output.
    const TAIL: usize = 2048;
    if raw.len() > TAIL {
        raw = raw[raw.len() - TAIL..].to_vec();
    }
    let text = String::from_utf8_lossy(&raw);
    // Strip ANSI escape sequences (CSI / OSC / SS3 / single-char) —
    // Slack renders them as garbage characters otherwise.
    let cleaned = strip_ansi(&text);
    // Last 30 non-empty lines.
    let lines: Vec<&str> = cleaned
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    let start = lines.len().saturating_sub(30);
    lines[start..].join("\n")
}

/// Strip ANSI escape sequences. Handles `ESC [ ... letter` CSI
/// sequences (the common case from claude / shells) plus single-
/// character ESC + char sequences. Leaves everything else
/// untouched. Pure UTF-8 in / out.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Read the introducer: [ for CSI, ] for OSC, P for DCS, ...
            match chars.next() {
                Some('[') => {
                    // CSI: skip until a letter terminator (0x40..=0x7e).
                    for c2 in chars.by_ref() {
                        if c2.is_ascii_alphabetic() || c2 == '~' {
                            break;
                        }
                    }
                }
                Some(']') => {
                    // OSC: skip until BEL (0x07) or ESC \.
                    let mut prev = ' ';
                    for c2 in chars.by_ref() {
                        if c2 == '\x07' || (prev == '\x1b' && c2 == '\\') {
                            break;
                        }
                        prev = c2;
                    }
                }
                Some(_) => {} // Two-char escape — already consumed.
                None => break,
            }
        } else if c == '\r' {
            // Strip CR — terminals use \r for cursor reset between
            // status-line updates; in Slack it just produces blank
            // line noise.
        } else {
            out.push(c);
        }
    }
    out
}

/// Slack → PTY. Look up `channel → workspace → terminal`, strip
/// any `<@UBOT>` prefix the mention path added, write the body +
/// `\r` to the agent's stdin.
async fn handle_inbound(
    server: &ServerConfig,
    state: &Arc<Mutex<SlackState>>,
    bot_user_id: &str,
    msg: InboundEvent,
) {
    let (channel, raw_text) = match msg {
        InboundEvent::Mention { channel, text, .. }
        | InboundEvent::Message { channel, text, .. } => (channel, text),
        InboundEvent::Hello | InboundEvent::Disconnect { .. } => return,
    };
    let text = strip_bot_mention(&raw_text, bot_user_id).trim().to_string();
    if text.is_empty() {
        return;
    }
    let Some(workspace_key) = state.lock().await.channel_to_workspace.get(&channel).cloned()
    else {
        tracing::debug!(channel = %channel, "slack: inbound in untracked channel — ignoring");
        return;
    };
    let Some(terminal_id) = state
        .lock()
        .await
        .workspace_to_terminal
        .get(&workspace_key)
        .copied()
    else {
        tracing::warn!(
            workspace = %workspace_key,
            "slack: inbound message but no agent terminal — skipping"
        );
        return;
    };
    // Write the text + CR (claude's stream-json mode treats CR as
    // submit). For plain claude TUI this is the same — the prompt
    // line submits on Enter.
    let mut bytes = text.into_bytes();
    bytes.push(b'\r');
    let backend_key = {
        let terminals = server.terminals.lock().await;
        terminals.get(&terminal_id).cloned()
    };
    if let Some(key) = backend_key
        && let Err(e) = server.backend.write(&key, &bytes).await
    {
        tracing::warn!(?terminal_id, "slack: backend.write failed: {e:?}");
    } else {
        tracing::info!(
            workspace = %workspace_key,
            "slack: routed inbound message to agent"
        );
    }
}

/// Strip a leading `<@Uxxx>` mention of the bot — Slack prepends
/// these on `app_mention` events, and they're noise when forwarding
/// to claude's stdin.
fn strip_bot_mention(text: &str, bot_user_id: &str) -> String {
    let prefix = format!("<@{bot_user_id}>");
    text.strip_prefix(&prefix)
        .map(str::trim_start)
        .unwrap_or(text)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ansi_drops_csi_sequences() {
        let s = "\x1b[31mhello\x1b[0m world";
        assert_eq!(strip_ansi(s), "hello world");
    }

    #[test]
    fn strip_ansi_drops_osc_until_bel() {
        let s = "\x1b]0;title\x07after";
        assert_eq!(strip_ansi(s), "after");
    }

    #[test]
    fn strip_ansi_drops_cr() {
        // \r is used by status-line updaters; in non-terminal
        // contexts (Slack) it produces blank lines.
        assert_eq!(strip_ansi("a\rb"), "ab");
    }

    #[test]
    fn strip_bot_mention_removes_leading_mention() {
        assert_eq!(strip_bot_mention("<@UBOT> hello", "UBOT"), "hello");
    }

    #[test]
    fn strip_bot_mention_leaves_text_alone_if_no_mention() {
        assert_eq!(strip_bot_mention("just text", "UBOT"), "just text");
    }
}
