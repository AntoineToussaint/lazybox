//! Provider-agnostic chat integration.
//!
//! Pilot uses external chat systems (Slack today; Discord / Matrix /
//! IRC planned) as a "second display" for the inbox — a place where
//! agent events surface, and from which the user can drive the
//! agent without being at the keyboard.
//!
//! ## Channel granularity — one channel per (session, agent)
//!
//! A pilot workspace can host multiple sessions (= worktrees on
//! disk), each running one or more agents. Each `(session_id, agent)`
//! pair gets its own chat channel, named
//! `<workspace>-<session-short>-<agent>`:
//!
//! ```text
//! #github-acme-widget-186-a3f1c277-claude
//! #github-acme-widget-186-a3f1c277-codex     (codex in the same session)
//! #github-acme-widget-186-9e22d100-claude   (a second worktree's claude)
//! ```
//!
//! This shape means inbound `@pilot yes` is unambiguous — the
//! channel uniquely identifies which agent's PTY to write to. The
//! older workspace-keyed model couldn't distinguish two agents in
//! the same workspace and silently routed everything to the
//! most-recently-spawned terminal.
//!
//! ## Provider abstraction
//!
//! The pieces split between *provider-specific* and *provider-
//! agnostic*:
//!
//! - **Provider-specific:** auth, wire protocol, channel creation,
//!   mention syntax. (Slack adapter: `crate::slack`.)
//! - **Provider-agnostic:** mapping channel_id ↔ terminal_id,
//!   parsing inbound commands, formatting the status reply, picking
//!   what to post for each bus event.
//!
//! This module owns the provider-agnostic half. Adapters plug in by
//! implementing [`ChatProvider`] and feeding normalized inbound
//! events into [`handle_inbound`] + bus events into
//! [`handle_bus_event`].

use crate::ServerConfig;
use pilot_ipc::{AgentState, Event, TerminalId, TerminalKind};
use pilot_store::Store;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// How long the agent must be quiet (no PTY bytes) before the
/// Asking state reads as `done — waiting for next task` rather than
/// `paused — input expected`. Heuristic: claude's status-line
/// redraws stop ~1-2 seconds after the last real output, so 3
/// seconds gives a comfortable margin without making short Y/N
/// prompts read as "done".
const DONE_QUIET_THRESHOLD: Duration = Duration::from_secs(3);

/// Limit on rows in the global status reply. Mostly there to keep
/// replies scannable on a phone.
const STATUS_GLOBAL_LIMIT: usize = 30;

/// Error type for the chat layer. Providers convert their own
/// error types into this so the dispatcher only deals with one
/// shape.
#[derive(Debug, thiserror::Error)]
pub enum ChatError {
    #[error("provider: {0}")]
    Provider(String),
}

/// One inbound message normalized across providers. The provider's
/// own event stream (slack `InboundEvent`, discord `Message`, …)
/// gets mapped into this before the dispatcher sees it.
#[derive(Debug, Clone)]
pub enum ChatInbound {
    /// Provider has come online. The dispatcher ignores it today —
    /// adapters can log "connected" themselves.
    Connected,
    /// User said something in a channel pilot can see.
    Message {
        channel: String,
        user: String,
        text: String,
        ts: String,
    },
    /// Provider asked the client to reconnect. Dispatcher ignores;
    /// the provider's own retry loop owns reconnection.
    Disconnected { reason: String },
}

/// What pilot needs from a chat backend.
///
/// Return-style follows the [`crate::backend::SessionBackend`]
/// pattern: `Pin<Box<dyn Future>>` rather than `async-trait`, so the
/// trait stays `dyn`-compatible without extra crates.
pub trait ChatProvider: Send + Sync {
    /// Stable id for logs / metrics. `"slack"`, `"discord"`, ...
    fn id(&self) -> &'static str;

    /// Strip a leading self-mention from inbound text. Provider-
    /// specific because the mention syntax differs (slack
    /// `<@Uxxx>`, discord `<@!123>`, matrix `@bot:server`).
    fn strip_self_mention<'a>(&self, text: &'a str) -> &'a str;

    /// Compute the channel name the provider will use for a given
    /// (workspace, session_id, agent_id) tuple. Returning `None`
    /// means "no per-terminal channel for this provider" — e.g.
    /// Slack with `per_workspace_channels: false`, where everything
    /// routes through the anchor channel instead. When `None`, the
    /// dispatcher silently drops outbound events for this terminal
    /// and never creates a channel.
    fn channel_name(&self, workspace_key: &str, session_id: &str, agent_id: &str)
    -> Option<String>;

    /// Post a plain-text message to a channel id.
    fn post<'a>(
        &'a self,
        channel: &'a str,
        body: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), ChatError>> + Send + 'a>>;

    /// Find or create the channel with the given name. Returns the
    /// provider's stable channel id. `name_taken` should be
    /// transparent (provider returns the existing id).
    fn ensure_channel<'a>(
        &'a self,
        name: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, ChatError>> + Send + 'a>>;
}

/// Shared state for one chat provider's dispatcher. Built once at
/// startup; the outbound (bus → chat) and inbound (chat → PTY /
/// status) halves take `Arc` clones.
#[derive(Default)]
pub struct RouterState {
    /// `terminal_id → channel id`. One entry per agent terminal that
    /// has a chat channel. Populated on `TerminalSpawned(Agent)` and
    /// cleared on `TerminalExited`.
    terminal_to_channel: HashMap<TerminalId, String>,
    /// Reverse map for inbound routing. Same lifetime as
    /// `terminal_to_channel`.
    channel_to_terminal: HashMap<String, TerminalId>,
    /// `terminal_id → last instant we saw PTY output`. Updated on
    /// every `TerminalOutput` bus event. When `AgentState::Asking`
    /// fires the dispatcher reads this to label notifications as
    /// "paused" (recent output) vs "done" (quiet for a while).
    last_output_at: HashMap<TerminalId, Instant>,
}

impl RouterState {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Inbound chat commands pilot responds to instead of forwarding to
/// the PTY. The set is small on purpose — pilot is an inbox, not a
/// chatbot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatCommand {
    /// Report state of sessions. In a per-terminal channel: that
    /// agent's state + recent activity. In a channel pilot doesn't
    /// route to a terminal: a summary across every tracked agent.
    Status,
}

/// Parse a message body into a [`ChatCommand`] if it looks like one.
/// Returns `None` when the message is regular agent input.
///
/// Matched keywords (case-insensitive, leading token only):
/// `status`, `state`, `ls`, `list`. The token has to lead — "what's
/// the status" does NOT match — so plain chat doesn't accidentally
/// trigger a status reply. Trailing punctuation is tolerated
/// (`status?` matches); trailing args are ignored (`ls -la` matches).
pub fn parse_command(text: &str) -> Option<ChatCommand> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let first = trimmed
        .split_whitespace()
        .next()?
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_ascii_lowercase();
    match first.as_str() {
        "status" | "state" | "ls" | "list" => Some(ChatCommand::Status),
        _ => None,
    }
}

/// Top-level inbound handler. Provider-specific code maps its own
/// wire event into [`ChatInbound`] and calls this. Either the
/// message is a [`ChatCommand`] (we reply via the provider) or it's
/// agent input (we forward to the channel's terminal).
pub async fn handle_inbound(
    provider: &dyn ChatProvider,
    server: &ServerConfig,
    state: &Arc<Mutex<RouterState>>,
    msg: ChatInbound,
) {
    let (channel, raw_text) = match msg {
        ChatInbound::Message { channel, text, .. } => (channel, text),
        ChatInbound::Connected | ChatInbound::Disconnected { .. } => return,
    };
    let text = provider.strip_self_mention(&raw_text).trim().to_string();
    if text.is_empty() {
        return;
    }
    let terminal_id = state
        .lock()
        .await
        .channel_to_terminal
        .get(&channel)
        .copied();
    // Query commands short-circuit before the PTY-forward: `status`
    // in an unmapped channel (e.g. anchor / DMs) still gets a reply.
    if let Some(cmd) = parse_command(&text) {
        let body = build_status_reply(server, state, terminal_id, cmd).await;
        if let Err(e) = provider.post(&channel, &body).await {
            tracing::warn!(
                provider = provider.id(),
                channel = %channel,
                "chat: status post failed: {e}"
            );
        }
        return;
    }
    let Some(terminal_id) = terminal_id else {
        tracing::debug!(
            provider = provider.id(),
            channel = %channel,
            "chat: inbound in untracked channel — ignoring"
        );
        return;
    };
    // Multi-line messages need bracket-paste sequences (`ESC[200~
    // ... ESC[201~`) — without them claude's terminal interprets
    // each embedded newline as a submit, sending the message
    // line-by-line and triggering a separate inference per line.
    // The submit-cr lives outside the paste markers so claude
    // actually dispatches the assembled prompt.
    let bytes = encode_for_pty(&text);
    let backend_key = {
        let terminals = server.terminals.lock().await;
        terminals.get(&terminal_id).cloned()
    };
    if let Some(key) = backend_key
        && let Err(e) = server.backend.write(&key, &bytes).await
    {
        tracing::warn!(
            provider = provider.id(),
            ?terminal_id,
            "chat: backend.write failed: {e:?}"
        );
    } else {
        tracing::info!(
            provider = provider.id(),
            ?terminal_id,
            "chat: routed inbound message to agent"
        );
    }
}

/// Top-level bus-event handler. Provider-specific code subscribes
/// to the broadcast bus and forwards every event here. Variants
/// the chat layer doesn't react to (file IO, polling) are a no-op.
pub async fn handle_bus_event(
    provider: &dyn ChatProvider,
    server: &ServerConfig,
    state: &Arc<Mutex<RouterState>>,
    event: Event,
) {
    match event {
        Event::TerminalSpawned {
            terminal_id,
            session_key,
            kind,
            ..
        } => {
            let TerminalKind::Agent(agent_id) = kind else {
                return;
            };
            // Look up the session id; without it we can't name the
            // channel uniquely per (session, agent).
            let session_id = server
                .terminal_sessions
                .lock()
                .await
                .get(&terminal_id)
                .copied();
            let Some(session_id) = session_id else {
                tracing::debug!(
                    ?terminal_id,
                    "chat: TerminalSpawned with no session — skipping channel create"
                );
                return;
            };
            let workspace_key = session_key.as_str().to_string();
            let Some(name) =
                provider.channel_name(&workspace_key, &session_id.to_string(), &agent_id)
            else {
                return;
            };
            let channel_id = match provider.ensure_channel(&name).await {
                Ok(id) => id,
                Err(e) => {
                    tracing::warn!(
                        provider = provider.id(),
                        name = %name,
                        "chat: ensure_channel failed: {e}"
                    );
                    return;
                }
            };
            {
                let mut s = state.lock().await;
                s.terminal_to_channel
                    .insert(terminal_id, channel_id.clone());
                s.channel_to_terminal
                    .insert(channel_id.clone(), terminal_id);
                // Seed last-output-at so a brand-new terminal that
                // goes straight into Asking on its first prompt
                // doesn't get labelled `done`.
                s.last_output_at.insert(terminal_id, Instant::now());
            }
            // Header so the channel isn't a wall of confusing
            // notifications — include the workspace title and the
            // agent so a phone reader can orient.
            let header = workspace_header(&*server.store, &workspace_key, &agent_id);
            if let Err(e) = provider.post(&channel_id, &header).await {
                tracing::warn!(provider = provider.id(), "chat: header post failed: {e}");
            }
        }
        Event::AgentState {
            terminal_id,
            state: agent_state,
            ..
        } => {
            if agent_state != AgentState::Asking {
                return;
            }
            let (channel_id, quiet_for) = {
                let s = state.lock().await;
                let ch = s.terminal_to_channel.get(&terminal_id).cloned();
                let quiet = s.last_output_at.get(&terminal_id).map(|t| t.elapsed());
                (ch, quiet)
            };
            let Some(channel_id) = channel_id else {
                // Terminal not tracked — probably spawned before the
                // chat task came up, or `channel_name` returned None.
                return;
            };
            let label = asking_label(quiet_for);
            let context = recent_terminal_text(server, terminal_id).await;
            let body = if context.is_empty() {
                format!("{label} · reply in this channel to answer")
            } else {
                format!("{label} · reply in this channel to answer\n```\n{context}\n```")
            };
            if let Err(e) = provider.post(&channel_id, &body).await {
                tracing::warn!(provider = provider.id(), "chat: post failed: {e}");
            }
        }
        Event::TerminalOutput { terminal_id, .. } => {
            state
                .lock()
                .await
                .last_output_at
                .insert(terminal_id, Instant::now());
        }
        Event::TerminalExited { terminal_id, .. } => {
            let mut s = state.lock().await;
            if let Some(ch) = s.terminal_to_channel.remove(&terminal_id) {
                s.channel_to_terminal.remove(&ch);
            }
            s.last_output_at.remove(&terminal_id);
        }
        _ => {}
    }
}

/// One-line header posted into a newly-created (session, agent)
/// channel. Pulls the workspace title from the store so phone
/// readers see what they're looking at without context-switching
/// to GitHub.
fn workspace_header(store: &dyn Store, workspace_key: &str, agent_id: &str) -> String {
    let title = store_workspace_title(store, workspace_key);
    match title {
        Some(t) => {
            format!("🤖 *{t}* · `{agent_id}` session\nreply in this channel to send to the agent")
        }
        None => format!("🤖 `{agent_id}` session in `{workspace_key}`"),
    }
}

/// Resolve a workspace title from the store. Returns `None` if the
/// store can't read or the workspace isn't there yet (race between
/// `TerminalSpawned` and the polling tick that inserts the row).
fn store_workspace_title(store: &dyn Store, workspace_key: &str) -> Option<String> {
    let records = store.list_workspaces().ok()?;
    let r = records.into_iter().find(|r| r.key == workspace_key)?;
    let json = r.workspace_json?;
    let ws: pilot_core::Workspace = serde_json::from_str(&json).ok()?;
    let title = ws
        .primary_task()
        .map(|t| t.title.clone())
        .unwrap_or(ws.name);
    Some(title)
}

/// Pick the label that fronts an Asking notification. `quiet_for` is
/// elapsed time since the last PTY output. `None` means the
/// terminal has no recorded output yet — that's `paused` territory
/// (a brand-new agent that prompts on first run).
fn asking_label(quiet_for: Option<Duration>) -> &'static str {
    match quiet_for {
        Some(d) if d >= DONE_QUIET_THRESHOLD => "✅ *done — waiting for next task*",
        _ => "⏸ *paused — input expected*",
    }
}

/// Pull the last ~2 KB of the terminal's ring buffer, strip ANSI,
/// drop blank lines, return up to the last 30 non-empty lines. Used
/// as the "context" block in chat "waiting on input" messages so the
/// user can see what the agent is asking.
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
            tracing::debug!(?terminal_id, "chat: backend.snapshot failed: {e:?}");
            return String::new();
        }
    };
    const TAIL: usize = 2048;
    if raw.len() > TAIL {
        raw = raw[raw.len() - TAIL..].to_vec();
    }
    let text = String::from_utf8_lossy(&raw);
    let cleaned = strip_ansi(&text);
    let lines: Vec<&str> = cleaned.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(30);
    lines[start..].join("\n")
}

/// Strip ANSI escape sequences. Handles `ESC [ ... letter` CSI
/// sequences (the common case from claude / shells), `ESC ] ... BEL`
/// OSC sequences, plus single-character `ESC + char` sequences.
/// Leaves everything else untouched. Pure UTF-8 in / out.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.next() {
                Some('[') => {
                    for c2 in chars.by_ref() {
                        if c2.is_ascii_alphabetic() || c2 == '~' {
                            break;
                        }
                    }
                }
                Some(']') => {
                    let mut prev = ' ';
                    for c2 in chars.by_ref() {
                        if c2 == '\x07' || (prev == '\x1b' && c2 == '\\') {
                            break;
                        }
                        prev = c2;
                    }
                }
                Some(_) => {}
                None => break,
            }
        } else if c == '\r' {
            // \r is used by status-line updaters; in non-terminal
            // contexts (chat) it produces blank lines.
        } else {
            out.push(c);
        }
    }
    out
}

/// Encode a chat reply for the agent's PTY. Single-line is raw text
/// plus a CR. Multi-line is wrapped in a bracket-paste pair
/// (`ESC[200~ ... ESC[201~`) plus a trailing CR — the same protocol
/// shells / editors use for terminal paste, which claude treats as
/// one logical input rather than per-line submits.
fn encode_for_pty(text: &str) -> Vec<u8> {
    if !text.contains('\n') {
        let mut bytes = text.as_bytes().to_vec();
        bytes.push(b'\r');
        return bytes;
    }
    let mut out: Vec<u8> = Vec::with_capacity(text.len() + 16);
    out.extend_from_slice(b"\x1b[200~");
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push(b'\r');
        }
        out.extend_from_slice(line.as_bytes());
    }
    out.extend_from_slice(b"\x1b[201~");
    out.push(b'\r');
    out
}

// ── Status reply ──────────────────────────────────────────────────

/// Build the status reply for a chat command. `terminal_id` is
/// `Some` when the command came from a channel pilot routes to a
/// specific agent — that channel reports just that agent. `None`
/// means the channel isn't routed (anchor / DM / random) — return
/// the global summary across every tracked terminal.
async fn build_status_reply(
    server: &ServerConfig,
    state: &Arc<Mutex<RouterState>>,
    terminal_id: Option<TerminalId>,
    cmd: ChatCommand,
) -> String {
    let ChatCommand::Status = cmd;
    let agent_states_snapshot: HashMap<TerminalId, AgentState> = {
        let m = server.agent_states.lock().await;
        m.clone()
    };
    let terminal_to_channel: HashMap<TerminalId, String> = {
        let s = state.lock().await;
        s.terminal_to_channel.clone()
    };
    let terminal_meta: HashMap<TerminalId, (pilot_core::SessionKey, TerminalKind)> = {
        let m = server.terminal_meta.lock().await;
        m.clone()
    };
    let terminal_sessions: HashMap<TerminalId, pilot_core::SessionId> = {
        let m = server.terminal_sessions.lock().await;
        m.clone()
    };
    let workspaces = load_workspaces_for_status(&*server.store);
    let workspace_titles: HashMap<String, String> = workspaces
        .iter()
        .map(|w| {
            let title = w
                .primary_task()
                .map(|t| t.title.clone())
                .unwrap_or_else(|| w.name.clone());
            (w.key.as_str().to_string(), title)
        })
        .collect();
    format_status_reply(
        terminal_id,
        &agent_states_snapshot,
        &terminal_to_channel,
        &terminal_meta,
        &terminal_sessions,
        &workspace_titles,
    )
}

/// Pull the deserialized workspaces from the store. Mirrors
/// `lib::load_workspaces` but inlined here because that helper is
/// private; duplicating five lines is cheaper than re-exporting.
fn load_workspaces_for_status(store: &dyn Store) -> Vec<pilot_core::Workspace> {
    let records = match store.list_workspaces() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("chat status: list_workspaces failed: {e}");
            return vec![];
        }
    };
    records
        .into_iter()
        .filter_map(|r| serde_json::from_str(r.workspace_json.as_deref()?).ok())
        .collect()
}

/// Pure formatter — given the inbox state, render the status text.
/// Pure so tests can drive it without standing up a Store or a
/// running provider.
pub fn format_status_reply(
    terminal_id: Option<TerminalId>,
    agent_states: &HashMap<TerminalId, AgentState>,
    terminal_to_channel: &HashMap<TerminalId, String>,
    terminal_meta: &HashMap<TerminalId, (pilot_core::SessionKey, TerminalKind)>,
    terminal_sessions: &HashMap<TerminalId, pilot_core::SessionId>,
    workspace_titles: &HashMap<String, String>,
) -> String {
    if let Some(tid) = terminal_id {
        return format_terminal_status(
            tid,
            agent_states,
            terminal_meta,
            terminal_sessions,
            workspace_titles,
        );
    }
    let tracked: Vec<TerminalId> = terminal_to_channel.keys().copied().collect();
    if tracked.is_empty() {
        return "📭 no agent sessions tracked yet".to_string();
    }
    let mut rows: Vec<(String, String)> = tracked
        .iter()
        .filter_map(|tid| {
            let (workspace_key, kind) = terminal_meta.get(tid)?;
            let agent = match kind {
                TerminalKind::Agent(id) => id.clone(),
                _ => return None,
            };
            let ws_key = workspace_key.as_str().to_string();
            Some((
                ws_key.clone(),
                format_one_liner(*tid, &ws_key, &agent, agent_states, workspace_titles),
            ))
        })
        .collect();
    // Group adjacent rows from the same workspace so the reader
    // sees PR's agents next to each other.
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    let mut lines: Vec<String> = Vec::with_capacity(rows.len() + 2);
    lines.push(format!("📋 *{} agent session(s)*", rows.len()));
    for (_, line) in rows.iter().take(STATUS_GLOBAL_LIMIT) {
        lines.push(line.clone());
    }
    if rows.len() > STATUS_GLOBAL_LIMIT {
        lines.push(format!("… and {} more", rows.len() - STATUS_GLOBAL_LIMIT));
    }
    lines.join("\n")
}

fn format_one_liner(
    tid: TerminalId,
    workspace_key: &str,
    agent_id: &str,
    agent_states: &HashMap<TerminalId, AgentState>,
    workspace_titles: &HashMap<String, String>,
) -> String {
    let icon = agent_icon(tid, agent_states);
    let title = workspace_titles
        .get(workspace_key)
        .cloned()
        .unwrap_or_else(|| workspace_key.to_string());
    format!("{icon} `{title}` · {agent_id}")
}

fn format_terminal_status(
    tid: TerminalId,
    agent_states: &HashMap<TerminalId, AgentState>,
    terminal_meta: &HashMap<TerminalId, (pilot_core::SessionKey, TerminalKind)>,
    terminal_sessions: &HashMap<TerminalId, pilot_core::SessionId>,
    workspace_titles: &HashMap<String, String>,
) -> String {
    let Some((workspace_key, kind)) = terminal_meta.get(&tid) else {
        return "❓ this channel's agent is no longer tracked".to_string();
    };
    let agent_id = match kind {
        TerminalKind::Agent(id) => id.as_str(),
        _ => "(non-agent)",
    };
    let ws_key = workspace_key.as_str();
    let title = workspace_titles
        .get(ws_key)
        .cloned()
        .unwrap_or_else(|| ws_key.to_string());
    let icon = agent_icon(tid, agent_states);
    let state_label = match agent_states.get(&tid).copied() {
        Some(AgentState::Asking) => "⏸ asking",
        Some(AgentState::Active) => "▶ active",
        None => "—",
    };
    let session_short: String = terminal_sessions
        .get(&tid)
        .map(|s| s.to_string().chars().take(8).collect())
        .unwrap_or_else(|| "—".to_string());
    format!(
        "{icon} *{title}*\nagent: `{agent_id}`\nsession: `{session_short}`\nstate: {state_label}"
    )
}

/// Status icon for an agent terminal. `⏸` if asking, `▶` if
/// active, `·` if untracked.
fn agent_icon(tid: TerminalId, agent_states: &HashMap<TerminalId, AgentState>) -> &'static str {
    match agent_states.get(&tid).copied() {
        Some(AgentState::Asking) => "⏸",
        Some(AgentState::Active) => "▶",
        None => "·",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pilot_core::{SessionId, SessionKey};

    fn ws_titles(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn parse_command_matches_lead_status_keyword() {
        assert_eq!(parse_command("status"), Some(ChatCommand::Status));
        assert_eq!(parse_command(" status please "), Some(ChatCommand::Status));
        assert_eq!(parse_command("STATUS"), Some(ChatCommand::Status));
        assert_eq!(parse_command("state"), Some(ChatCommand::Status));
        assert_eq!(parse_command("ls"), Some(ChatCommand::Status));
        assert_eq!(parse_command("list"), Some(ChatCommand::Status));
        assert_eq!(parse_command("ls -la"), Some(ChatCommand::Status));
    }

    #[test]
    fn parse_command_ignores_non_leading_keywords() {
        assert_eq!(parse_command("what is the status"), None);
        assert_eq!(parse_command("please list it"), None);
    }

    #[test]
    fn parse_command_returns_none_for_normal_text() {
        assert_eq!(parse_command("yes"), None);
        assert_eq!(parse_command(""), None);
        assert_eq!(parse_command("   "), None);
        assert_eq!(parse_command("hello world"), None);
    }

    #[test]
    fn parse_command_strips_punctuation_on_keyword() {
        assert_eq!(parse_command("status?"), Some(ChatCommand::Status));
    }

    #[test]
    fn format_status_reply_global_lists_terminals_grouped_by_workspace() {
        let mut agent_states = HashMap::new();
        agent_states.insert(TerminalId(1), AgentState::Active);
        agent_states.insert(TerminalId(2), AgentState::Asking);
        let mut terminal_to_channel = HashMap::new();
        terminal_to_channel.insert(TerminalId(1), "C1".to_string());
        terminal_to_channel.insert(TerminalId(2), "C2".to_string());
        let mut terminal_meta = HashMap::new();
        terminal_meta.insert(
            TerminalId(1),
            (
                SessionKey::new("ws-a"),
                TerminalKind::Agent("claude".into()),
            ),
        );
        terminal_meta.insert(
            TerminalId(2),
            (SessionKey::new("ws-b"), TerminalKind::Agent("codex".into())),
        );
        let terminal_sessions = HashMap::new();
        let titles = ws_titles(&[("ws-a", "Alpha"), ("ws-b", "Beta")]);

        let reply = format_status_reply(
            None,
            &agent_states,
            &terminal_to_channel,
            &terminal_meta,
            &terminal_sessions,
            &titles,
        );
        assert!(reply.contains("2 agent session(s)"), "{}", reply);
        assert!(reply.contains("Alpha"));
        assert!(reply.contains("Beta"));
        assert!(reply.contains("claude"));
        assert!(reply.contains("codex"));
    }

    #[test]
    fn format_status_reply_global_empty_when_nothing_tracked() {
        let reply = format_status_reply(
            None,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(reply, "📭 no agent sessions tracked yet");
    }

    #[test]
    fn format_status_reply_per_terminal_includes_workspace_agent_session_state() {
        let mut agent_states = HashMap::new();
        agent_states.insert(TerminalId(7), AgentState::Asking);
        let mut terminal_meta = HashMap::new();
        terminal_meta.insert(
            TerminalId(7),
            (
                SessionKey::new("github-acme-widget-186"),
                TerminalKind::Agent("claude".into()),
            ),
        );
        let mut terminal_sessions = HashMap::new();
        let session_id =
            SessionId(uuid::Uuid::parse_str("a3f1c277-9abc-4d51-8f01-deadbeef0001").unwrap());
        terminal_sessions.insert(TerminalId(7), session_id);
        let titles = ws_titles(&[("github-acme-widget-186", "Fix the date picker")]);

        let reply = format_status_reply(
            Some(TerminalId(7)),
            &agent_states,
            &HashMap::new(),
            &terminal_meta,
            &terminal_sessions,
            &titles,
        );
        assert!(reply.starts_with("⏸ *Fix the date picker*"), "{}", reply);
        assert!(reply.contains("agent: `claude`"), "{}", reply);
        assert!(reply.contains("session: `a3f1c277"), "{}", reply);
        assert!(reply.contains("state: ⏸ asking"), "{}", reply);
    }

    #[test]
    fn format_status_reply_per_terminal_unknown_id_reports_so() {
        let reply = format_status_reply(
            Some(TerminalId(999)),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert!(reply.contains("no longer tracked"));
    }

    #[test]
    fn agent_icon_reflects_state() {
        let tid = TerminalId(1);
        assert_eq!(agent_icon(tid, &HashMap::new()), "·");
        let mut active = HashMap::new();
        active.insert(tid, AgentState::Active);
        assert_eq!(agent_icon(tid, &active), "▶");
        let mut asking = HashMap::new();
        asking.insert(tid, AgentState::Asking);
        assert_eq!(agent_icon(tid, &asking), "⏸");
    }

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
        assert_eq!(strip_ansi("a\rb"), "ab");
    }

    #[test]
    fn encode_for_pty_single_line_appends_cr() {
        assert_eq!(encode_for_pty("yes"), b"yes\r");
    }

    #[test]
    fn encode_for_pty_multi_line_wraps_in_bracket_paste() {
        let out = encode_for_pty("line one\nline two");
        assert_eq!(out, b"\x1b[200~line one\rline two\x1b[201~\r");
    }

    #[test]
    fn encode_for_pty_preserves_blank_lines_between_content() {
        let out = encode_for_pty("a\n\nb");
        assert_eq!(out, b"\x1b[200~a\r\rb\x1b[201~\r");
    }

    #[test]
    fn asking_label_recent_output_is_paused() {
        let recent = Duration::from_millis(500);
        assert_eq!(asking_label(Some(recent)), "⏸ *paused — input expected*");
    }

    #[test]
    fn asking_label_stale_output_is_done() {
        let stale = DONE_QUIET_THRESHOLD + Duration::from_secs(1);
        assert_eq!(
            asking_label(Some(stale)),
            "✅ *done — waiting for next task*"
        );
    }

    #[test]
    fn asking_label_no_recorded_output_defaults_to_paused() {
        assert_eq!(asking_label(None), "⏸ *paused — input expected*");
    }
}
