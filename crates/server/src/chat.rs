//! Provider-agnostic chat integration.
//!
//! Pilot uses external chat systems (Slack today; Discord / Matrix /
//! IRC planned) as a "second display" for the inbox — a place where
//! agent events surface, and from which the user can drive the
//! agent without being at the keyboard.
//!
//! ## Channel granularity — one channel per (session, agent), or one
//! thread per (session, agent) inside the anchor channel
//!
//! A pilot workspace can host multiple sessions (= worktrees on
//! disk), each running one or more agents. Two layouts are
//! supported, selected by the provider via [`TerminalTarget`]:
//!
//! - **Dedicated channel** — each `(session_id, agent)` gets its own
//!   chat channel, named `<workspace>-<session-short>-<agent>`:
//!
//!   ```text
//!   #github-acme-widget-186-a3f1c277-claude
//!   #github-acme-widget-186-a3f1c277-codex     (codex in the same session)
//!   #github-acme-widget-186-9e22d100-claude   (a second worktree's claude)
//!   ```
//!
//! - **Thread in anchor channel** — when the provider opts out of
//!   per-(session, agent) channels (Slack with
//!   `per_workspace_channels: false`), every agent terminal gets one
//!   anchor message in the configured anchor channel
//!   (`#pilot` by default); subsequent notifications are posted as
//!   thread replies and inbound thread replies route back to the
//!   anchoring terminal. This keeps a quiet Slack while preserving
//!   Asking → reply round-trips.
//!
//! Either way, inbound `@pilot yes` (or a thread reply) is
//! unambiguous: the channel id alone identifies the terminal in
//! dedicated mode; in thread mode the `thread_ts` identifies it.
//! The older workspace-keyed model couldn't distinguish two agents
//! in the same workspace and silently routed everything to the
//! most-recently-spawned terminal.
//!
//! ### Toggling
//!
//! Switching `per_workspace_channels` requires a restart. The
//! provider stashes the mode at boot and the per-terminal route
//! map is populated as terminals spawn; flipping mid-run would
//! leave existing terminals on their original surface (channels)
//! while new ones switch to the other (threads), which is
//! confusing — easier to require a restart than to migrate.
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
    /// User said something in a channel pilot can see. `thread_ts` is
    /// set when the message came from inside a thread — the
    /// dispatcher uses it to route to the (session, agent) anchored
    /// by that thread (thread-fallback mode).
    Message {
        channel: String,
        user: String,
        text: String,
        ts: String,
        thread_ts: Option<String>,
    },
    /// Provider asked the client to reconnect. Dispatcher ignores;
    /// the provider's own retry loop owns reconnection.
    Disconnected { reason: String },
}

/// Where a freshly-spawned (session, agent) terminal should surface in
/// chat. Returned by [`ChatProvider::terminal_target`]; the
/// dispatcher uses the variant to decide whether to create a
/// dedicated channel or anchor a thread in an existing one.
#[derive(Debug, Clone)]
pub enum TerminalTarget {
    /// Create (or look up) a channel by name and post every
    /// notification at the top level of that channel. Inbound routes
    /// by `channel_id`.
    DedicatedChannel(String),
    /// Use this existing channel id and anchor a thread for the
    /// terminal. The dispatcher posts the header message via
    /// [`ChatProvider::post`], reads the returned `thread_ts`, and
    /// posts subsequent notifications via
    /// [`ChatProvider::post_to_thread`]. Inbound routes by
    /// `thread_ts`.
    AnchorThread(String),
    /// No chat surface for this terminal — the dispatcher silently
    /// drops outbound events and never tries to ensure a channel.
    /// (Named `NoSurface` rather than `None` so `match` arms don't
    /// shadow [`Option::None`] at a glance.)
    NoSurface,
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

    /// Pick the chat surface for a freshly-spawned (workspace,
    /// session_id, agent_id) terminal. See [`TerminalTarget`].
    fn terminal_target(
        &self,
        workspace_key: &str,
        session_id: &str,
        agent_id: &str,
    ) -> TerminalTarget;

    /// Post a plain-text message to a channel id. Returns the
    /// provider's message id (Slack `ts`, Discord snowflake, …) — the
    /// dispatcher uses it as `thread_ts` when anchoring a thread.
    /// Providers without a stable message id may return an empty
    /// string; threading will then degrade to channel-level routing.
    fn post<'a>(
        &'a self,
        channel: &'a str,
        body: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, ChatError>> + Send + 'a>>;

    /// Post a plain-text message as a reply in a thread. `thread_ts`
    /// is the anchor message id returned by an earlier `post`.
    fn post_to_thread<'a>(
        &'a self,
        channel: &'a str,
        thread_ts: &'a str,
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
    /// has a chat surface (dedicated channel *or* anchor thread).
    /// Populated on `TerminalSpawned(Agent)` and cleared on
    /// `TerminalExited`.
    terminal_to_channel: HashMap<TerminalId, String>,
    /// Reverse map for inbound routing in **dedicated-channel** mode.
    /// Not populated for thread-anchored terminals — multiple
    /// terminals can share one channel id in thread mode, so the
    /// `channel_id → terminal_id` lookup would clobber. Thread-mode
    /// terminals are reachable only via [`Self::thread_to_terminal`].
    channel_to_terminal: HashMap<String, TerminalId>,
    /// `terminal_id → thread_ts`. Set when the terminal was anchored
    /// in a thread (see [`TerminalTarget::AnchorThread`]); absent for
    /// terminals on dedicated channels. The dispatcher reads this to
    /// decide between [`ChatProvider::post`] and
    /// [`ChatProvider::post_to_thread`] for outbound notifications.
    terminal_to_thread: HashMap<TerminalId, String>,
    /// Reverse of `terminal_to_thread`. Inbound thread replies route
    /// through here first — falling back to `channel_to_terminal`
    /// only if the message wasn't in a known thread.
    thread_to_terminal: HashMap<String, TerminalId>,
    /// `terminal_id → last instant we saw PTY output`. Updated on
    /// every `TerminalOutput` bus event. When `AgentState::InputNeeded`
    /// fires the dispatcher reads this to label notifications as
    /// "paused" (recent output) vs "done" (quiet for a while).
    last_output_at: HashMap<TerminalId, Instant>,
}

impl RouterState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Test helper — record a terminal as living on a dedicated
    /// channel. Asserting tests use this so they don't reach into
    /// the internal map layout.
    #[cfg(test)]
    fn record_dedicated(&mut self, terminal_id: TerminalId, channel_id: &str) {
        self.terminal_to_channel
            .insert(terminal_id, channel_id.to_string());
        self.channel_to_terminal
            .insert(channel_id.to_string(), terminal_id);
    }

    /// Test helper — record a terminal as living in a thread inside
    /// a (shared) anchor channel.
    #[cfg(test)]
    fn record_thread(&mut self, terminal_id: TerminalId, channel_id: &str, thread_ts: &str) {
        self.terminal_to_channel
            .insert(terminal_id, channel_id.to_string());
        self.terminal_to_thread
            .insert(terminal_id, thread_ts.to_string());
        self.thread_to_terminal
            .insert(thread_ts.to_string(), terminal_id);
    }
}

/// Decide which terminal an inbound message routes to, given a
/// snapshot of the router state. Pure so it can be unit-tested
/// without the dispatcher's async + ServerConfig surface.
///
/// Order matters: thread first, channel as a fallback. An unknown
/// `thread_ts` falls through to the channel lookup so a user starting
/// a fresh thread inside a tracked channel still reaches the right
/// terminal (dedicated mode), and `status` from an unknown thread in
/// the anchor channel still gets a global reply (handler caller
/// short-circuits on the command before requiring `Some(tid)`).
pub(crate) fn route_inbound(
    state: &RouterState,
    channel: &str,
    thread_ts: Option<&str>,
) -> Option<TerminalId> {
    thread_ts
        .and_then(|t| state.thread_to_terminal.get(t).copied())
        .or_else(|| state.channel_to_terminal.get(channel).copied())
}

/// Post `body` either as a thread reply (when `thread_ts` is `Some`)
/// or as a fresh top-level message. Drops the message id from the
/// `post` path — callers that need the id (thread anchoring) call
/// `provider.post` directly.
async fn post_into(
    provider: &dyn ChatProvider,
    channel: &str,
    thread_ts: Option<&str>,
    body: &str,
) -> Result<(), ChatError> {
    match thread_ts {
        Some(t) => provider.post_to_thread(channel, t, body).await,
        None => provider.post(channel, body).await.map(|_| ()),
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
    let (channel, raw_text, in_thread_ts) = match msg {
        ChatInbound::Message {
            channel,
            text,
            thread_ts,
            ..
        } => (channel, text, thread_ts),
        ChatInbound::Connected | ChatInbound::Disconnected { .. } => return,
    };
    let text = provider.strip_self_mention(&raw_text).trim();
    if text.is_empty() {
        return;
    }
    // Routing order: thread first (anchor mode), then channel
    // (dedicated mode). A `thread_ts` we don't know about falls
    // through to the channel lookup — could be a brand-new thread the
    // user started in the anchor channel, in which case we have
    // nothing to write to. The channel-level fallback also lets the
    // status command work from inside a thread we don't track.
    let terminal_id = {
        let s = state.lock().await;
        route_inbound(&s, &channel, in_thread_ts.as_deref())
    };
    // Query commands short-circuit before the PTY-forward: `status`
    // in an unmapped channel (e.g. anchor / DMs) still gets a reply.
    // Status replies post in the same thread as the query so the
    // anchor channel doesn't fill with top-level status lines.
    if let Some(cmd) = parse_command(text) {
        let body = build_status_reply(server, state, terminal_id, cmd).await;
        if let Err(e) = post_into(provider, &channel, in_thread_ts.as_deref(), &body).await {
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
            thread_ts = ?in_thread_ts,
            "chat: inbound in untracked channel/thread — ignoring"
        );
        return;
    };
    // Multi-line messages need bracket-paste sequences (`ESC[200~
    // ... ESC[201~`) — without them claude's terminal interprets
    // each embedded newline as a submit, sending the message
    // line-by-line and triggering a separate inference per line.
    // The submit-cr lives outside the paste markers so claude
    // actually dispatches the assembled prompt.
    let bytes = encode_for_pty(text);
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
            // Look up the session id; without it we can't pick a
            // unique chat surface per (session, agent).
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
            let target =
                provider.terminal_target(&workspace_key, &session_id.to_string(), &agent_id);
            on_terminal_spawned(
                provider,
                &*server.store,
                state,
                terminal_id,
                &workspace_key,
                &agent_id,
                target,
            )
            .await;
        }
        Event::AgentState {
            terminal_id,
            state: agent_state,
            ..
        } => {
            if agent_state != AgentState::InputNeeded {
                return;
            }
            let (channel_id, thread_ts, quiet_for) = {
                let s = state.lock().await;
                let ch = s.terminal_to_channel.get(&terminal_id).cloned();
                let th = s.terminal_to_thread.get(&terminal_id).cloned();
                let quiet = s.last_output_at.get(&terminal_id).map(|t| t.elapsed());
                (ch, th, quiet)
            };
            let Some(channel_id) = channel_id else {
                // Terminal not tracked — probably spawned before the
                // chat task came up, or `terminal_target` returned
                // `None`.
                return;
            };
            let label = asking_label(quiet_for);
            let context = recent_terminal_text(server, terminal_id).await;
            // Surface-aware reply hint: in thread mode the user
            // replies in *this thread*; in dedicated mode they reply
            // in the channel.
            let reply_hint = if thread_ts.is_some() {
                "reply in this thread to answer"
            } else {
                "reply in this channel to answer"
            };
            let body = if context.is_empty() {
                format!("{label} · {reply_hint}")
            } else {
                format!("{label} · {reply_hint}\n```\n{context}\n```")
            };
            if let Err(e) = post_into(provider, &channel_id, thread_ts.as_deref(), &body).await {
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
                // Only clear the reverse map if it still points at
                // this terminal — defensive against the (unlikely)
                // case of a stale entry.
                if matches!(s.channel_to_terminal.get(&ch), Some(t) if *t == terminal_id) {
                    s.channel_to_terminal.remove(&ch);
                }
            }
            if let Some(t) = s.terminal_to_thread.remove(&terminal_id) {
                s.thread_to_terminal.remove(&t);
            }
            s.last_output_at.remove(&terminal_id);
        }
        _ => {}
    }
}

/// Handle a freshly-spawned agent terminal: ensure the chat surface
/// (channel or thread anchor), populate the route maps, post a
/// header. Extracted so the surface-strategy split stays linear
/// instead of nesting matches inside `handle_bus_event`.
pub(crate) async fn on_terminal_spawned(
    provider: &dyn ChatProvider,
    store: &dyn Store,
    state: &Arc<Mutex<RouterState>>,
    terminal_id: TerminalId,
    workspace_key: &str,
    agent_id: &str,
    target: TerminalTarget,
) {
    let header = workspace_header(store, workspace_key, agent_id);
    match target {
        TerminalTarget::NoSurface => {}
        TerminalTarget::DedicatedChannel(name) => {
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
            if let Err(e) = provider.post(&channel_id, &header).await {
                tracing::warn!(provider = provider.id(), "chat: header post failed: {e}");
            }
        }
        TerminalTarget::AnchorThread(channel_id) => {
            // Post the header first to obtain its ts — that ts becomes
            // the thread anchor for every subsequent notification and
            // for inbound routing.
            let thread_ts: Option<String> = match provider.post(&channel_id, &header).await {
                Ok(ts) if !ts.is_empty() => Some(ts),
                Ok(_) => {
                    tracing::warn!(
                        provider = provider.id(),
                        "chat: provider returned empty thread anchor id; falling back to channel-only routing"
                    );
                    None
                }
                Err(e) => {
                    tracing::warn!(provider = provider.id(), "chat: anchor post failed: {e}");
                    return;
                }
            };
            let mut s = state.lock().await;
            s.terminal_to_channel.insert(terminal_id, channel_id);
            if let Some(ts) = thread_ts {
                s.thread_to_terminal.insert(ts.clone(), terminal_id);
                s.terminal_to_thread.insert(terminal_id, ts);
            }
            s.last_output_at.insert(terminal_id, Instant::now());
        }
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
        Some(AgentState::InputNeeded) => "⏸ needs input",
        Some(AgentState::Working) => "▶ working",
        Some(AgentState::Idle) => "· idle",
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

/// Status icon for an agent terminal. `⏸` if it needs input, `▶` if
/// working, `·` if idle or untracked.
fn agent_icon(tid: TerminalId, agent_states: &HashMap<TerminalId, AgentState>) -> &'static str {
    match agent_states.get(&tid).copied() {
        Some(AgentState::InputNeeded) => "⏸",
        Some(AgentState::Working) => "▶",
        Some(AgentState::Idle) | None => "·",
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
        agent_states.insert(TerminalId(1), AgentState::Working);
        agent_states.insert(TerminalId(2), AgentState::InputNeeded);
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
        agent_states.insert(TerminalId(7), AgentState::InputNeeded);
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
        assert!(reply.contains("state: ⏸ needs input"), "{}", reply);
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
        let mut working = HashMap::new();
        working.insert(tid, AgentState::Working);
        assert_eq!(agent_icon(tid, &working), "▶");
        let mut asking = HashMap::new();
        asking.insert(tid, AgentState::InputNeeded);
        assert_eq!(agent_icon(tid, &asking), "⏸");
        let mut idle = HashMap::new();
        idle.insert(tid, AgentState::Idle);
        assert_eq!(agent_icon(tid, &idle), "·");
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

    // ── route_inbound ─────────────────────────────────────────────────

    #[test]
    fn route_inbound_picks_thread_match_first() {
        let mut state = RouterState::new();
        state.record_dedicated(TerminalId(1), "C1");
        state.record_thread(TerminalId(7), "C-anchor", "t-99");
        // Thread anchor wins over channel mapping — in thread-mode
        // both terminals share the channel id and only thread_ts
        // disambiguates.
        assert_eq!(
            route_inbound(&state, "C1", Some("t-99")),
            Some(TerminalId(7))
        );
    }

    #[test]
    fn route_inbound_falls_back_to_channel_when_thread_unknown() {
        let mut state = RouterState::new();
        state.record_dedicated(TerminalId(1), "C1");
        // Unknown thread_ts → fall through to channel lookup. Lets a
        // user start a brand-new thread in a tracked channel and
        // still reach the terminal in dedicated mode.
        assert_eq!(
            route_inbound(&state, "C1", Some("never-seen")),
            Some(TerminalId(1))
        );
    }

    #[test]
    fn route_inbound_no_thread_uses_channel() {
        let mut state = RouterState::new();
        state.record_dedicated(TerminalId(1), "C1");
        assert_eq!(route_inbound(&state, "C1", None), Some(TerminalId(1)));
    }

    #[test]
    fn route_inbound_unknown_channel_and_thread_returns_none() {
        let state = RouterState::new();
        assert_eq!(route_inbound(&state, "C-unknown", None), None);
        assert_eq!(route_inbound(&state, "C-unknown", Some("t")), None);
    }

    // ── on_terminal_spawned + mock provider ───────────────────────────

    /// Minimal in-memory provider that records calls and serves
    /// rigged responses. Lets us drive `on_terminal_spawned` without
    /// HTTP and assert on state-map population. Recorders are
    /// `std::sync::Mutex` — guards never span an `.await` so the
    /// async-aware tokio variant would be overkill.
    struct MockProvider {
        target: TerminalTarget,
        /// Posts go here as `(channel, body)`. `post()` returns the
        /// next ts from `anchor_ts` (popped in order) so a test can
        /// preset a known thread anchor.
        posts: std::sync::Mutex<Vec<(String, String)>>,
        thread_posts: std::sync::Mutex<Vec<(String, String, String)>>,
        anchor_ts: std::sync::Mutex<Vec<String>>,
        ensured: std::sync::Mutex<Vec<String>>,
    }

    impl MockProvider {
        fn new(target: TerminalTarget, anchor_ts: Vec<String>) -> Self {
            Self {
                target,
                posts: std::sync::Mutex::new(vec![]),
                thread_posts: std::sync::Mutex::new(vec![]),
                anchor_ts: std::sync::Mutex::new(anchor_ts),
                ensured: std::sync::Mutex::new(vec![]),
            }
        }
    }

    impl ChatProvider for MockProvider {
        fn id(&self) -> &'static str {
            "mock"
        }
        fn strip_self_mention<'a>(&self, t: &'a str) -> &'a str {
            t
        }
        fn terminal_target(&self, _: &str, _: &str, _: &str) -> TerminalTarget {
            self.target.clone()
        }
        fn post<'a>(
            &'a self,
            channel: &'a str,
            body: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<String, ChatError>> + Send + 'a>> {
            Box::pin(async move {
                self.posts
                    .lock()
                    .unwrap()
                    .push((channel.to_string(), body.to_string()));
                Ok(self
                    .anchor_ts
                    .lock()
                    .unwrap()
                    .pop()
                    .unwrap_or_else(|| "ts-default".to_string()))
            })
        }
        fn post_to_thread<'a>(
            &'a self,
            channel: &'a str,
            thread_ts: &'a str,
            body: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<(), ChatError>> + Send + 'a>> {
            Box::pin(async move {
                self.thread_posts.lock().unwrap().push((
                    channel.to_string(),
                    thread_ts.to_string(),
                    body.to_string(),
                ));
                Ok(())
            })
        }
        fn ensure_channel<'a>(
            &'a self,
            name: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<String, ChatError>> + Send + 'a>> {
            Box::pin(async move {
                self.ensured.lock().unwrap().push(name.to_string());
                Ok(format!("C-{name}"))
            })
        }
    }

    struct EmptyStore;
    impl Store for EmptyStore {}

    #[tokio::test]
    async fn on_terminal_spawned_dedicated_mode_populates_channel_maps() {
        let provider = MockProvider::new(
            TerminalTarget::DedicatedChannel("ws-a-claude".into()),
            vec![],
        );
        let state = Arc::new(Mutex::new(RouterState::new()));
        let store = EmptyStore;
        on_terminal_spawned(
            &provider,
            &store,
            &state,
            TerminalId(1),
            "ws-a",
            "claude",
            provider.target.clone(),
        )
        .await;
        let s = state.lock().await;
        assert_eq!(
            s.terminal_to_channel
                .get(&TerminalId(1))
                .map(String::as_str),
            Some("C-ws-a-claude")
        );
        assert_eq!(
            s.channel_to_terminal.get("C-ws-a-claude").copied(),
            Some(TerminalId(1))
        );
        assert!(
            s.terminal_to_thread.is_empty(),
            "dedicated mode should not anchor a thread"
        );
        assert!(s.thread_to_terminal.is_empty());
    }

    #[tokio::test]
    async fn on_terminal_spawned_anchor_thread_populates_thread_maps() {
        let provider = MockProvider::new(
            TerminalTarget::AnchorThread("C-ANCHOR".into()),
            vec!["1700000000.000100".into()],
        );
        let state = Arc::new(Mutex::new(RouterState::new()));
        let store = EmptyStore;
        on_terminal_spawned(
            &provider,
            &store,
            &state,
            TerminalId(42),
            "ws-a",
            "claude",
            provider.target.clone(),
        )
        .await;
        let s = state.lock().await;
        assert_eq!(
            s.terminal_to_channel
                .get(&TerminalId(42))
                .map(String::as_str),
            Some("C-ANCHOR")
        );
        assert_eq!(
            s.terminal_to_thread
                .get(&TerminalId(42))
                .map(String::as_str),
            Some("1700000000.000100")
        );
        assert_eq!(
            s.thread_to_terminal.get("1700000000.000100").copied(),
            Some(TerminalId(42))
        );
        // No channel_to_terminal entry: in thread mode multiple
        // terminals share the channel id, so the reverse map must
        // not be populated or one would clobber the other.
        assert!(s.channel_to_terminal.is_empty());
    }

    #[tokio::test]
    async fn on_terminal_spawned_anchor_thread_two_terminals_dont_clobber() {
        let provider = MockProvider::new(
            TerminalTarget::AnchorThread("C-ANCHOR".into()),
            // Stack order: first pop is the last pushed.
            vec!["ts-second".into(), "ts-first".into()],
        );
        let state = Arc::new(Mutex::new(RouterState::new()));
        let store = EmptyStore;
        on_terminal_spawned(
            &provider,
            &store,
            &state,
            TerminalId(1),
            "ws-a",
            "claude",
            provider.target.clone(),
        )
        .await;
        on_terminal_spawned(
            &provider,
            &store,
            &state,
            TerminalId(2),
            "ws-a",
            "codex",
            provider.target.clone(),
        )
        .await;
        let s = state.lock().await;
        assert_eq!(
            s.thread_to_terminal.get("ts-first").copied(),
            Some(TerminalId(1))
        );
        assert_eq!(
            s.thread_to_terminal.get("ts-second").copied(),
            Some(TerminalId(2))
        );
        // route_inbound recovers both terminals via their respective
        // threads — this is the property the bug fix is for.
        assert_eq!(
            route_inbound(&s, "C-ANCHOR", Some("ts-first")),
            Some(TerminalId(1))
        );
        assert_eq!(
            route_inbound(&s, "C-ANCHOR", Some("ts-second")),
            Some(TerminalId(2))
        );
    }

    #[tokio::test]
    async fn on_terminal_spawned_none_target_is_silent_noop() {
        let provider = MockProvider::new(TerminalTarget::NoSurface, vec![]);
        let state = Arc::new(Mutex::new(RouterState::new()));
        let store = EmptyStore;
        on_terminal_spawned(
            &provider,
            &store,
            &state,
            TerminalId(1),
            "ws-a",
            "claude",
            provider.target.clone(),
        )
        .await;
        let s = state.lock().await;
        assert!(s.terminal_to_channel.is_empty());
        assert!(s.terminal_to_thread.is_empty());
        // and no post was attempted
        assert!(provider.posts.lock().unwrap().is_empty());
        assert!(provider.ensured.lock().unwrap().is_empty());
    }
}
