//! Provider-agnostic chat integration.
//!
//! Pilot uses external chat systems (Slack today; Discord / Matrix /
//! IRC planned) as a "second display" for the inbox — a place where
//! workspace and agent events surface, and from which the user can
//! drive the agent without being at the keyboard.
//!
//! The pieces of that integration split cleanly between
//! *provider-specific* and *provider-agnostic* code:
//!
//! - **Provider-specific:** auth (Slack uses bot + app tokens;
//!   Discord uses a single bot token; Matrix uses an access token),
//!   wire protocol (Slack Socket Mode WebSocket; Discord gateway;
//!   Matrix `/sync`), channel creation, mention syntax.
//! - **Provider-agnostic:** mapping `channel_id ↔ workspace_key`,
//!   parsing inbound commands like `status` / `ls`, formatting the
//!   reply, deciding whether an inbound message routes to the PTY or
//!   to the status formatter, picking the right channel for an
//!   outbound bus event.
//!
//! This module owns the provider-agnostic half. The Slack adapter
//! (`crate::slack`) plugs in by implementing [`ChatProvider`] and
//! delegating its inbound + bus-event loops to the dispatcher
//! functions here.
//!
//! ## Adding a new provider
//!
//! 1. Add a `pilot-<name>` crate with the wire types + a thin client
//!    (HTTP / WebSocket as needed).
//! 2. In `crate::<name>`, define a struct that implements
//!    [`ChatProvider`] (the post + ensure-channel + bot-id +
//!    strip-mention quartet).
//! 3. Spawn a task that drains the provider's inbound stream into
//!    normalized [`ChatInbound`]s and calls
//!    [`handle_inbound`] for each.
//! 4. Subscribe to the bus and call [`handle_bus_event`].

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

/// Limit on workspaces summarized in a global status reply.
/// Mostly there to keep replies scannable on a phone.
const STATUS_GLOBAL_LIMIT: usize = 15;

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
    /// Provider has come online — useful for "bot just (re)started"
    /// log lines. The dispatcher ignores it today.
    Connected,
    /// User said something in a channel pilot can see. `channel` is
    /// the provider's stable id (Slack channel id, Discord channel
    /// id). `user` is the speaker's id; pilot uses it only for
    /// logging today. `ts` is the provider's message timestamp,
    /// preserved so future thread-reply code can use it as an
    /// anchor.
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

/// Provider-side interface every chat backend implements. Three jobs:
///
/// 1. **Post** a message — outbound notifications.
/// 2. **Ensure a channel** exists for a workspace — pilot creates
///    one per workspace by default.
/// 3. **Strip self-mention** — `<@bot>` in slack, `<@!123>` in
///    discord. Pulled into the trait because the rest of the
///    dispatch logic shouldn't care which syntax the provider uses.
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

    /// Post a plain-text message to a channel id.
    fn post<'a>(
        &'a self,
        channel: &'a str,
        body: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), ChatError>> + Send + 'a>>;

    /// Find or create the channel that hosts `workspace_key`'s
    /// conversation. `Ok(None)` means the provider intentionally
    /// has no channel for it (e.g. `per_workspace_channels: false`
    /// in Slack, channel-not-allowed in Discord). The dispatcher
    /// caches the returned id so a stable call only hits the
    /// network once.
    fn ensure_workspace_channel<'a>(
        &'a self,
        workspace_key: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>, ChatError>> + Send + 'a>>;
}

/// Shared state for one chat provider's dispatcher. Built once at
/// startup; the outbound (bus → chat) and inbound (chat → PTY /
/// status) halves take `Arc` clones.
#[derive(Default)]
pub struct RouterState {
    /// `channel_id → workspace_key`. Built when pilot resolves a
    /// channel for a workspace; used by inbound to route a chat
    /// message back to the right session.
    channel_to_workspace: HashMap<String, String>,
    /// `workspace_key → primary agent terminal_id`. Updated on
    /// `TerminalSpawned` / `TerminalExited`. Pilot writes inbound
    /// replies here. Empty entries mean "no agent yet — skip".
    workspace_to_terminal: HashMap<String, TerminalId>,
    /// `terminal_id → last instant we saw PTY output`. Updated on
    /// every `TerminalOutput` bus event. When `AgentState::Asking`
    /// fires the dispatcher reads this to label notifications as
    /// "paused" (recent output) vs "done" (quiet for a while).
    last_output_at: HashMap<TerminalId, Instant>,
    /// Workspaces we've already posted the "new workspace"
    /// notification for. The bus broadcasts `WorkspaceUpserted` on
    /// every read-state change too — without this set the dispatcher
    /// would re-post on every keystroke.
    posted_workspaces: std::collections::HashSet<String>,
}

impl RouterState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-seed the workspace → channel mapping with channels the
    /// provider already knows about at boot. Slack uses this after
    /// `conversations.list` so subsequent `ensure_workspace_channel`
    /// calls find the existing id without an HTTP roundtrip.
    pub fn record_channel(&mut self, channel_id: String, workspace_key: String) {
        self.channel_to_workspace.insert(channel_id, workspace_key);
    }
}

/// Inbound chat commands pilot responds to instead of forwarding to
/// the PTY. The set is small on purpose — pilot is an inbox, not a
/// chatbot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatCommand {
    /// Report state of sessions. In a workspace channel: that
    /// workspace's sessions + agent state. Anywhere else (anchor /
    /// untracked): a summary across every known workspace.
    Status,
}

/// Parse a message body into a [`ChatCommand`] if it looks like one.
/// Returns `None` when the message is regular agent input.
///
/// Matched keywords (case-insensitive, leading token only):
/// `status`, `state`, `ls`, `list`. The token has to lead — "what's
/// the status" does NOT match — so plain chat doesn't accidentally
/// trigger a status reply. Tokens like `ls -la` still route to
/// status (everything after the keyword is ignored today).
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
/// agent input (we forward to the workspace's PTY).
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
    let workspace_key = state
        .lock()
        .await
        .channel_to_workspace
        .get(&channel)
        .cloned();
    // Query commands short-circuit before PTY-forward: `status` in
    // the anchor channel (no workspace mapping) still produces a
    // reply.
    if let Some(cmd) = parse_command(&text) {
        let body = build_status_reply(server, state, workspace_key.as_deref(), cmd).await;
        if let Err(e) = provider.post(&channel, &body).await {
            tracing::warn!(provider = provider.id(), channel = %channel, "{}: status post failed: {e}", provider.id());
        }
        return;
    }
    let Some(workspace_key) = workspace_key else {
        tracing::debug!(
            provider = provider.id(),
            channel = %channel,
            "chat: inbound in untracked channel — ignoring"
        );
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
            provider = provider.id(),
            workspace = %workspace_key,
            "chat: inbound message but no agent terminal — skipping"
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
            workspace = %workspace_key,
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
        Event::WorkspaceUpserted(ws) => {
            // Only post on the first time we see a workspace — the
            // bus broadcasts on every read-state change too.
            let workspace_key = ws.key.as_str().to_string();
            {
                let mut s = state.lock().await;
                if s.posted_workspaces.contains(&workspace_key) {
                    return;
                }
                s.posted_workspaces.insert(workspace_key.clone());
            }
            let Some(channel_id) = resolve_channel(provider, state, &workspace_key).await else {
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
            if let Err(e) = provider.post(&channel_id, &body).await {
                tracing::warn!(provider = provider.id(), "chat: post failed: {e}");
            }
        }
        Event::AgentState {
            session_key,
            state: agent_state,
        } => {
            // Only post on Asking transitions — Active is the
            // default and the channel would drown in "now streaming"
            // messages otherwise.
            if agent_state != AgentState::Asking {
                return;
            }
            let workspace_key = session_key.as_str().to_string();
            let Some(channel_id) = resolve_channel(provider, state, &workspace_key).await else {
                return;
            };
            let (terminal_id, quiet_for) = {
                let s = state.lock().await;
                let tid = s.workspace_to_terminal.get(&workspace_key).copied();
                let quiet = tid
                    .and_then(|t| s.last_output_at.get(&t).copied())
                    .map(|t| t.elapsed());
                (tid, quiet)
            };
            let Some(terminal_id) = terminal_id else {
                let _ = provider
                    .post(&channel_id, "⏸ agent is waiting on input")
                    .await;
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
        Event::TerminalSpawned {
            terminal_id,
            session_key,
            kind,
            ..
        } => {
            if !matches!(kind, TerminalKind::Agent(_)) {
                return;
            }
            let mut s = state.lock().await;
            s.workspace_to_terminal
                .insert(session_key.as_str().to_string(), terminal_id);
            // Seed last-output-at so a brand-new terminal that goes
            // straight into Asking on its first prompt doesn't get
            // labelled `done` (no recorded output ever → elapsed
            // calculation falls through to `paused`, the right
            // default).
            s.last_output_at.insert(terminal_id, Instant::now());
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
            s.workspace_to_terminal.retain(|_, tid| *tid != terminal_id);
            s.last_output_at.remove(&terminal_id);
        }
        _ => {}
    }
}

/// Resolve the channel for a workspace, caching the
/// channel → workspace reverse map. Returns `None` when the
/// provider has no channel for it (config disabled, error).
async fn resolve_channel(
    provider: &dyn ChatProvider,
    state: &Arc<Mutex<RouterState>>,
    workspace_key: &str,
) -> Option<String> {
    match provider.ensure_workspace_channel(workspace_key).await {
        Ok(Some(id)) => {
            state
                .lock()
                .await
                .channel_to_workspace
                .insert(id.clone(), workspace_key.to_string());
            Some(id)
        }
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(
                provider = provider.id(),
                workspace = %workspace_key,
                "chat: ensure_workspace_channel failed: {e}"
            );
            None
        }
    }
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
/// drop blank lines, and return up to the last 30 non-empty lines.
/// Used as the "context" block in chat "waiting on input" messages
/// so the user can see what the agent is asking.
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
            // Strip CR — terminals use \r for cursor reset between
            // status-line updates; in chat it just produces blank
            // line noise.
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

/// Build the body of a status reply. Pure read of the daemon's
/// existing state — no provider I/O happens here.
async fn build_status_reply(
    server: &ServerConfig,
    state: &Arc<Mutex<RouterState>>,
    workspace_key: Option<&str>,
    cmd: ChatCommand,
) -> String {
    let ChatCommand::Status = cmd;
    let workspaces = load_workspaces_for_status(&*server.store);
    let agent_states_snapshot: HashMap<TerminalId, AgentState> = {
        let m = server.agent_states.lock().await;
        m.clone()
    };
    let workspace_to_terminal: HashMap<String, TerminalId> = {
        let s = state.lock().await;
        s.workspace_to_terminal.clone()
    };
    let terminal_meta: HashMap<TerminalId, (pilot_core::SessionKey, TerminalKind)> = {
        let m = server.terminal_meta.lock().await;
        m.clone()
    };
    format_status_reply(
        workspace_key,
        &workspaces,
        &agent_states_snapshot,
        &workspace_to_terminal,
        &terminal_meta,
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
    workspace_key: Option<&str>,
    workspaces: &[pilot_core::Workspace],
    agent_states: &HashMap<TerminalId, AgentState>,
    workspace_to_terminal: &HashMap<String, TerminalId>,
    terminal_meta: &HashMap<TerminalId, (pilot_core::SessionKey, TerminalKind)>,
) -> String {
    if let Some(key) = workspace_key {
        let Some(ws) = workspaces.iter().find(|w| w.key.as_str() == key) else {
            return format!("❓ no workspace tracked for `{key}`");
        };
        return format_workspace_status(ws, agent_states, workspace_to_terminal, terminal_meta);
    }
    if workspaces.is_empty() {
        return "📭 no workspaces in the inbox yet".to_string();
    }
    let mut lines: Vec<String> = Vec::with_capacity(workspaces.len() + 2);
    lines.push(format!("📋 *{} workspace(s)*", workspaces.len()));
    for ws in workspaces.iter().take(STATUS_GLOBAL_LIMIT) {
        lines.push(format_workspace_one_liner(
            ws,
            agent_states,
            workspace_to_terminal,
            terminal_meta,
        ));
    }
    if workspaces.len() > STATUS_GLOBAL_LIMIT {
        lines.push(format!(
            "… and {} more",
            workspaces.len() - STATUS_GLOBAL_LIMIT
        ));
    }
    lines.join("\n")
}

fn format_workspace_one_liner(
    ws: &pilot_core::Workspace,
    agent_states: &HashMap<TerminalId, AgentState>,
    workspace_to_terminal: &HashMap<String, TerminalId>,
    terminal_meta: &HashMap<TerminalId, (pilot_core::SessionKey, TerminalKind)>,
) -> String {
    let icon = workspace_icon(ws, agent_states, workspace_to_terminal);
    let name = ws.name.as_str();
    let session_count = ws.sessions.len();
    let agents = agent_kinds_for_workspace(ws, workspace_to_terminal, terminal_meta);
    let agent_suffix = if agents.is_empty() {
        String::new()
    } else {
        format!(" · {}", agents.join(", "))
    };
    format!("{icon} `{name}` · {session_count} session(s){agent_suffix}")
}

fn format_workspace_status(
    ws: &pilot_core::Workspace,
    agent_states: &HashMap<TerminalId, AgentState>,
    workspace_to_terminal: &HashMap<String, TerminalId>,
    terminal_meta: &HashMap<TerminalId, (pilot_core::SessionKey, TerminalKind)>,
) -> String {
    let mut out = String::new();
    let icon = workspace_icon(ws, agent_states, workspace_to_terminal);
    out.push_str(&format!("{icon} *{}*\n", ws.name));
    if let Some(task) = ws.primary_task() {
        out.push_str(&format!("<{}>\n", task.url));
    }
    let agents = agent_kinds_for_workspace(ws, workspace_to_terminal, terminal_meta);
    if !agents.is_empty() {
        out.push_str(&format!("agents: {}\n", agents.join(", ")));
    }
    let tid = workspace_to_terminal.get(ws.key.as_str()).copied();
    let agent_state = tid.and_then(|t| agent_states.get(&t).copied());
    let state_label = match agent_state {
        Some(AgentState::Asking) => "⏸ asking",
        Some(AgentState::Active) => "▶ active",
        None => "—",
    };
    out.push_str(&format!("state: {state_label}\n"));
    out.push_str(&format!("sessions: {}", ws.sessions.len()));
    if ws.unread_count() > 0 {
        out.push_str(&format!(" · {} unread", ws.unread_count()));
    }
    out
}

fn workspace_icon(
    ws: &pilot_core::Workspace,
    agent_states: &HashMap<TerminalId, AgentState>,
    workspace_to_terminal: &HashMap<String, TerminalId>,
) -> &'static str {
    let Some(tid) = workspace_to_terminal.get(ws.key.as_str()).copied() else {
        return "·";
    };
    match agent_states.get(&tid).copied() {
        Some(AgentState::Asking) => "⏸",
        Some(AgentState::Active) => "▶",
        None => "·",
    }
}

fn agent_kinds_for_workspace(
    ws: &pilot_core::Workspace,
    workspace_to_terminal: &HashMap<String, TerminalId>,
    terminal_meta: &HashMap<TerminalId, (pilot_core::SessionKey, TerminalKind)>,
) -> Vec<String> {
    let Some(tid) = workspace_to_terminal.get(ws.key.as_str()).copied() else {
        return vec![];
    };
    match terminal_meta.get(&tid) {
        Some((_, TerminalKind::Agent(id))) => vec![id.clone()],
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use pilot_core::{SessionKey, Workspace, WorkspaceKey};

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
        // "status" appears, but not leading — chat shouldn't trigger.
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
        // Trailing `?` shouldn't defeat the match — common in chat.
        assert_eq!(parse_command("status?"), Some(ChatCommand::Status));
    }

    fn make_workspace(key: &str, name: &str) -> Workspace {
        Workspace::empty(WorkspaceKey::new(key), "main", Utc::now()).tap_name(name)
    }

    // helper extension so the empty-workspace test fixture sets a
    // readable name without rebuilding the literal `Workspace`
    // every time
    trait WorkspaceTestExt {
        fn tap_name(self, name: &str) -> Self;
    }
    impl WorkspaceTestExt for Workspace {
        fn tap_name(mut self, name: &str) -> Self {
            self.name = name.to_string();
            self
        }
    }

    #[test]
    fn format_status_reply_global_lists_workspaces() {
        let ws_a = make_workspace("a", "Alpha");
        let ws_b = make_workspace("b", "Beta");
        let reply = format_status_reply(
            None,
            &[ws_a, ws_b],
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert!(reply.contains("2 workspace(s)"));
        assert!(reply.contains("Alpha"));
        assert!(reply.contains("Beta"));
    }

    #[test]
    fn format_status_reply_global_caps_at_limit() {
        let many: Vec<Workspace> = (0..(STATUS_GLOBAL_LIMIT + 5))
            .map(|i| make_workspace(&format!("k{i}"), &format!("Name{i}")))
            .collect();
        let reply = format_status_reply(
            None,
            &many,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert!(reply.contains("… and 5 more"));
    }

    #[test]
    fn format_status_reply_global_handles_empty_inbox() {
        let reply =
            format_status_reply(None, &[], &HashMap::new(), &HashMap::new(), &HashMap::new());
        assert_eq!(reply, "📭 no workspaces in the inbox yet");
    }

    #[test]
    fn format_status_reply_per_workspace_renders_state() {
        let mut ws = make_workspace("a", "Alpha");
        ws.name = "Alpha workspace".into();
        let mut workspace_to_terminal = HashMap::new();
        workspace_to_terminal.insert("a".to_string(), TerminalId(7));
        let mut agent_states = HashMap::new();
        agent_states.insert(TerminalId(7), AgentState::Asking);
        let mut terminal_meta = HashMap::new();
        terminal_meta.insert(
            TerminalId(7),
            (SessionKey::new("a"), TerminalKind::Agent("claude".into())),
        );

        let reply = format_status_reply(
            Some("a"),
            &[ws],
            &agent_states,
            &workspace_to_terminal,
            &terminal_meta,
        );
        assert!(reply.starts_with("⏸ *Alpha workspace*"), "{}", reply);
        assert!(reply.contains("agents: claude"));
        assert!(reply.contains("state: ⏸ asking"));
    }

    #[test]
    fn format_status_reply_per_workspace_missing_key_reports_so() {
        let reply = format_status_reply(
            Some("nope"),
            &[],
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert!(reply.contains("nope"));
        assert!(reply.contains("no workspace"));
    }

    #[test]
    fn workspace_icon_reflects_agent_state() {
        let ws = make_workspace("a", "A");
        let mut workspace_to_terminal = HashMap::new();
        workspace_to_terminal.insert("a".to_string(), TerminalId(1));
        // Untracked terminal → `·`
        assert_eq!(
            workspace_icon(&ws, &HashMap::new(), &workspace_to_terminal),
            "·"
        );
        // Active → `▶`
        let mut active = HashMap::new();
        active.insert(TerminalId(1), AgentState::Active);
        assert_eq!(workspace_icon(&ws, &active, &workspace_to_terminal), "▶");
        // Asking → `⏸`
        let mut asking = HashMap::new();
        asking.insert(TerminalId(1), AgentState::Asking);
        assert_eq!(workspace_icon(&ws, &asking, &workspace_to_terminal), "⏸");
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
