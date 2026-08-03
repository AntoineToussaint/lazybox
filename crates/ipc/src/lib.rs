//! Lazybox IPC — protocol between the TUI and the daemon.
//!
//! The daemon is the single source of truth for all state (sessions,
//! worktrees, PTYs, provider polling, persistence). The TUI issues
//! `Command`s and receives `Event`s.
//!
//! **Communication is abstracted behind `Client` / `Connection` traits.**
//! The common case — TUI and daemon living in one process — uses the
//! `channel` transport: a pair of tokio mpsc channels, zero
//! serialization, zero sockets. The remote case — TUI running on a
//! laptop connecting to a daemon on a workstation over SSH — uses the
//! `socket` transport: length-prefixed bincode over a Unix socket
//! (which SSH's `-L` forwards). Client code never branches on which.
//!
//! # Wire framing (socket transport only)
//!
//! Each message on the wire is `[u32 BE length][bincode payload]`.
//! Max frame size is `MAX_FRAME_BYTES` (64 MiB).

use lazybox_core::SessionKey;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub mod channel;
pub mod socket;
pub mod transport;

pub const MAX_FRAME_BYTES: u32 = 64 * 1024 * 1024;

/// Socket command frames have a much smaller legitimate shape than snapshot
/// events. A distinct ingress ceiling prevents one peer from filling bounded
/// command queues with dozens of 64 MiB payloads. 256 KiB still leaves ample
/// room for a large composed prompt while bounding every retained command.
pub const MAX_COMMAND_FRAME_BYTES: u32 = 256 * 1024;

/// Largest `bytes` payload a client should put in a single
/// [`Command::Write`]. Derived from [`MAX_COMMAND_FRAME_BYTES`] (half
/// of it) so the two limits can never drift apart: half the ingress
/// cap leaves generous headroom for the bincode envelope (enum
/// ordinal, terminal id, vec length) and the 4-byte frame prefix, so
/// a chunked write always frames comfortably under the daemon's
/// command-frame ceiling. Larger writes (a multi-hundred-KiB paste)
/// are split with [`Command::write_chunked`]; the socket client
/// additionally normalizes every outbound command through
/// [`Command::into_write_chunks`] right before framing, so no
/// emission site can produce a frame the daemon's bounded command
/// reader refuses.
pub const MAX_WRITE_CHUNK_BYTES: usize = (MAX_COMMAND_FRAME_BYTES / 2) as usize;

/// Magic prefix of the 8-byte connection preamble each side sends
/// before any frames (`PROTOCOL_MAGIC ++ PROTOCOL_FINGERPRINT as u32
/// LE`). Lets a peer distinguish "wire-incompatible lazybox" from "not
/// lazybox at all" before bincode ever touches the stream.
pub const PROTOCOL_MAGIC: [u8; 4] = *b"LZBX";

/// Wire-compatibility fingerprint, negotiated by the connection
/// handshake (`socket::client_handshake` / `socket::server_handshake`).
///
/// Derived at build time (`build.rs`) from a hash of every
/// wire-defining input — this crate's sources, `lazybox-core`'s, and
/// the workspace lockfile — never hand-maintained. bincode identifies
/// enum variants by ordinal and structs by field order, so any change
/// to the `Command` / `Event` encodings makes an old peer silently
/// misread every subsequent frame; two binaries agree on this value
/// only when built from identical wire sources, and anything else is
/// rejected at connect with a clear "restart the daemon" error. The
/// hash over-approximates on purpose: a comment edit in a wire crate
/// forces a restart, but a wire change can never ride under an
/// unchanged number (which hand-bumping allowed whenever two branches
/// picked the same next value).
pub const PROTOCOL_FINGERPRINT: u32 = parse_u32(env!("LAZYBOX_PROTOCOL_FINGERPRINT"));

/// Const decimal parser for the build-script-emitted fingerprint —
/// `env!` yields a `&str` and there is no const `str::parse` yet.
const fn parse_u32(s: &str) -> u32 {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut v: u32 = 0;
    while i < bytes.len() {
        v = v * 10 + (bytes[i] - b'0') as u32;
        i += 1;
    }
    v
}

/// This binary's build identity: the workspace version plus the git
/// short SHA captured at compile time (`build.rs`). Two binaries built
/// from the same commit share this string; a stale daemon and a fresh
/// client differ.
///
/// [`PROTOCOL_FINGERPRINT`] only changes when a wire-defining input
/// does, so two builds dozens of commits apart can share one
/// fingerprint and connect cleanly while behaving differently. The
/// handshake exchanges this string so the client can surface a
/// "restart the daemon" banner on a build skew the fingerprint can't
/// see.
pub const BUILD_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("LAZYBOX_BUILD_SHA"));

/// The build commit, suffix-free (no `-dirty`), or `"unknown"` when
/// built outside a git checkout. Distinct from the SHA baked into
/// [`BUILD_VERSION`] (which carries the dirty marker) because the
/// staleness guard feeds it to `git rev-list` as a revision, where the
/// suffix would make it unresolvable.
pub const BUILD_GIT_SHA: &str = env!("LAZYBOX_BUILD_GIT_SHA");

/// Absolute path of the git checkout this binary was built from, or
/// empty when built outside one (a release tarball). The staleness
/// guard resolves this checkout's current branch and tracking upstream
/// against [`BUILD_GIT_SHA`]; an empty value disables the check rather
/// than guessing.
pub const BUILD_SOURCE_DIR: &str = env!("LAZYBOX_BUILD_SOURCE_DIR");

/// Whether this binary is an installer-managed release build (cargo-dist,
/// which compiles with `--profile dist`) rather than a dev/source build
/// (`cargo run`, `cargo build`, `cargo test`). The outdated-build nudge
/// and its "update & restart" affordance only make sense for a binary an
/// installer can swap in place; a source build is updated with `git pull
/// && cargo build`, so the guard is gated on this and a source build is
/// instead tagged `(dev)` in the header. A build we can't confidently
/// attribute to the release flow is treated as dev.
pub const IS_RELEASE_BUILD: bool = matches!(env!("LAZYBOX_RELEASE_BUILD").as_bytes(), [b'1', ..]);

/// Stable id for a spawned terminal. Distinct from SessionKey because a
/// single session may hold multiple terminals (agent + shell + logs).
///
/// `Default = TerminalId(0)` backs `#[serde(default)]` on optional
/// terminal-id fields — `0` never corresponds to a real allocation (the
/// daemon's id allocator starts at 1), so it's a safe sentinel for "the
/// producer omitted the field." Note the default only ever applies on
/// self-describing transports (the JSON gateway): bincode is not
/// self-describing, so on the socket transport an absent field is a
/// decode error, never a default — mixed-version peers are rejected by
/// the wire-fingerprint handshake instead — adding even a trailing
/// field shifts [`PROTOCOL_FINGERPRINT`] automatically.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct TerminalId(pub u64);

/// Stable id for a structured agent runtime. This is intentionally
/// separate from `TerminalId`: a run may be structured-JSON only, terminal
/// only, or mirrored into both surfaces by higher layers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct AgentRunId(pub u64);

/// Opaque client-generated id that correlates a structured-run start
/// request with its success or failure event.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct AgentRunRequestId(pub String);

/// Runtime surface requested for an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub enum AgentRuntimeMode {
    /// Traditional PTY/terminal byte stream.
    Terminal,
    /// Provider-neutral structured JSON events, independent of PTY bytes.
    /// The wire name remains `StreamJson` for protocol compatibility.
    StreamJson,
}

/// Host access granted to an agent launch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub enum AgentRunAccess {
    /// Use the agent's normal provider policy.
    #[default]
    Default,
    /// Prevent the agent from changing the host and ignore user-provided
    /// extensions that could escape that boundary.
    ReadOnly,
}

/// What to launch inside a terminal.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub enum TerminalKind {
    /// A known agent by id (e.g. "claude", "codex"). The daemon looks
    /// up the `Agent` impl and computes argv.
    Agent(String),
    /// Plain shell — `config.shell.command`.
    Shell,
    /// Tail a file inside the worktree.
    LogTail { path: String },
}

/// `RunnerKind` is the user-facing vocabulary: every PTY child of a
/// session is a "runner", whether it runs an agent or a plain shell.
/// `TerminalKind` is the wire-historic name kept for back-compat.
/// They're the same type — pick whichever reads better at the call
/// site. New code should prefer `RunnerKind`.
pub type RunnerKind = TerminalKind;

/// `RunnerId` mirrors `TerminalId` for the same reason. Daemon-
/// allocated u64 — a session-local handle, not a global UUID.
pub type RunnerId = TerminalId;

impl TerminalKind {
    /// Whether at most one runner of this kind may exist in a single
    /// session. Singleton kinds (Agent variants — Claude, Codex,
    /// Cursor) toggle-or-focus on duplicate spawn requests; multi
    /// kinds (Shell) always spawn a new instance.
    pub fn is_singleton(&self) -> bool {
        matches!(self, TerminalKind::Agent(_))
    }

    /// Equality of "uniqueness identity". Two singleton kinds collide
    /// iff their agent ids match. Two shells never collide. LogTail
    /// collides on path.
    pub fn singleton_key(&self) -> Option<String> {
        match self {
            TerminalKind::Agent(id) => Some(format!("agent:{id}")),
            TerminalKind::LogTail { path } => Some(format!("logtail:{path}")),
            TerminalKind::Shell => None,
        }
    }
}

/// Where a submitted user prompt came from. Distinguishes text the
/// user typed by hand from a body expanded out of the `]]s` snippet
/// picker, so the per-session prompt history can tag snippet-sourced
/// entries (issue #523).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub enum PromptSource {
    /// The user typed (or pasted) the prompt directly.
    #[default]
    Typed,
    /// The prompt was expanded from a snippet. Carries the snippet's
    /// key and category so the history can name which snippet it was
    /// (`category` is empty when the snippet declares none).
    Snippet { key: String, category: String },
}

/// One prompt the user submitted to an agent terminal, retained in a
/// bounded per-session history (issue #523). The pinned "you ▸ …" recap
/// is just the most recent entry; `]]h` opens the full list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct UserPrompt {
    /// The submitted prompt text (already trimmed, newlines preserved).
    pub text: String,
    /// Unix-epoch milliseconds when lazybox accepted the submission.
    /// Generated once by the component that confirms delivery and stored
    /// verbatim so a single timestamp follows the entry through
    /// persistence and replay.
    pub timestamp_ms: u64,
    /// Whether this prompt was typed or expanded from a snippet.
    #[serde(default)]
    pub source: PromptSource,
}

/// What the agent's PTY is doing right now. Drives the side-panel
/// state slot (working spinner / "needs input" pill / done / idle /
/// exited) and the TerminalStack tab badge.
///
/// The states are mutually exclusive and share a single UI slot per
/// session. They're produced per-agent-kind by
/// [`Agent::detect_state`](../lazybox_agents/trait.Agent.html), the
/// agent's lifecycle hooks, and the PTY-exit path — each agent decides
/// how to recognise "working" / "input needed" from its own PTY output.
/// An agent with no opinion returns `None`, which consumers treat as
/// `Idle` (so an unknown agent never falsely reports `Working`).
///
/// The lifecycle is a real state machine (see
/// `lazybox_agents::AgentStateMachine`). The load-bearing rule: **once
/// an agent is `Working` it can only leave for `Done`, `InputNeeded`,
/// or `Exited`** — never back to `Idle`. A working agent that comes to
/// rest has *finished a turn* (`Done`); it hasn't reverted to the
/// never-worked `Idle`. That makes the "working spinner silently
/// vanishes to a blank pill" flap structurally impossible rather than
/// something the UI has to damp.
///
/// `InputNeeded` and `Done` are the two states where the user must
/// act, so they raise an alert (desktop notification + footer notice);
/// `Working`, `Idle`, and `Exited` are silent.
///
/// Variants are appended, never reordered: the socket transport
/// encodes this enum by bincode ordinal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub enum AgentState {
    /// Actively producing output / running a tool right now — the
    /// agent's status line shows a streaming spinner / pulser.
    Working,
    /// Paused waiting on the user at a structural prompt: a permission
    /// gate, chooser, or Y/N prompt (see issues #26 / #122). Freeform
    /// conversational asks are deliberately not flagged. → alert.
    InputNeeded,
    /// Idle with no active work: freshly launched, or sitting at a
    /// ready composer having never run a task. The safe default for
    /// any agent that can't tell. Silent — nothing to act on. An agent
    /// only sits here *before* its first turn; once it has worked it
    /// resolves to `Done`, not back to `Idle`.
    Idle,
    /// Finished its turn — the agent ran work and has now come to rest.
    /// Distinct from `Idle`, which never worked. → alert. Sticky: a
    /// subsequent idle reading keeps `Done` until the agent works again
    /// or asks for input.
    ///
    /// Reached two ways: a lifecycle hook (Claude's `Stop`), or — for
    /// hookless agents (Codex, Cursor) — the PTY state machine promoting
    /// a `Working`-agent that settles at a resting composer, since
    /// "came to rest after working" is the only finished-turn signal a
    /// screen-scrape can offer.
    Done,
    /// The agent's process ended — a clean exit or a crash (issue #356:
    /// the `w x` Codex that died and left a stuck "working" pill). The
    /// terminal, final state: the PTY is gone, so no reading can move
    /// off it. `code` is the process exit status when observable
    /// (`Some(0)` clean, `Some(n)` failed, `None` when the exit couldn't
    /// be read). Set by the PTY-exit teardown, cleared when a fresh
    /// agent is spawned into the same workspace.
    Exited { code: Option<i32> },
}

/// A normalized lifecycle hook fired by an agent, decoupled from the
/// agent's wire JSON. Claude Code emits these via configured hooks
/// (`Stop`, `Notification`, `PreToolUse`, …); lazybox injects a hook
/// command at spawn so the daemon receives deterministic state signals
/// instead of screen-scraping the PTY. The wire JSON → `HookEvent`
/// translation lives in `lazybox_agents::hook`; mapping `HookEvent` →
/// [`AgentState`] lives there too. This type is the IPC-stable shape
/// carried by [`Command::IngestHook`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct HookEvent {
    pub kind: HookEventKind,
    /// The agent's own session id (Claude's `session_id`). Informational
    /// — lazybox correlates by the backend key it baked into the hook
    /// command, not this — but captured for the structured-stream path.
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    /// Tool being invoked, for `PreToolUse` / `PostToolUse`.
    pub tool_name: Option<String>,
    /// Notification descriptor (`notification_type` or `message`), used
    /// to distinguish a permission/elicitation prompt from an idle one.
    pub notification: Option<String>,
}

/// The lifecycle point a [`HookEvent`] fired at. `Other` is the
/// catch-all for hook names lazybox doesn't map to a state transition.
///
/// `PermissionRequest`, `SubagentStart`, and `PostCompact` are events
/// Claude Code does not actually fire — nothing produces them anymore,
/// but the variants stay (in place) because the socket transport
/// encodes this enum by bincode ordinal. `UserPromptSubmit` is appended
/// at the end for the same reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub enum HookEventKind {
    SessionStart,
    SessionEnd,
    PreToolUse,
    PostToolUse,
    Notification,
    PermissionRequest,
    Stop,
    SubagentStart,
    SubagentStop,
    PreCompact,
    PostCompact,
    Other,
    UserPromptSubmit,
}

/// User input sent to a structured agent runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct AgentInputMessage {
    /// Human-readable user text.
    pub text: Option<String>,
    /// Raw JSON payload for runtimes that accept structured input.
    pub json: Option<String>,
}

/// Decision for a tool/permission request emitted by an agent runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub enum AgentApprovalDecision {
    Approve,
    Deny { reason: Option<String> },
}

/// Answer to a structured question from an agent runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct AgentQuestionAnswer {
    pub answer: String,
}

/// Token/cost usage reported by a structured agent runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct AgentUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    /// Cost in millionths of a USD. Integer wire value avoids float
    /// compatibility issues across languages.
    pub cost_usd_micros: Option<u64>,
}

/// Stable identity for the human or service account connected to a
/// Lazybox daemon. The current local daemon uses `local`; remote/multi-user
/// clients should authenticate into distinct principal ids.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct PrincipalId(String);

impl PrincipalId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn local() -> Self {
        Self("local".into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for PrincipalId {
    fn default() -> Self {
        Self::local()
    }
}

impl From<&str> for PrincipalId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for PrincipalId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl fmt::Debug for PrincipalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("PrincipalId").field(&self.0).finish()
    }
}

impl fmt::Display for PrincipalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Public, non-secret credential metadata that clients may receive in
/// snapshots or events. Secret material is deliberately not represented
/// here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct ProviderCredentialMetadata {
    pub principal_id: PrincipalId,
    pub provider_id: String,
    pub source: String,
    pub scopes: Vec<String>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Secret-bearing credential bootstrap payload. Custom `Debug` keeps
/// daemon command tracing from printing provider tokens.
#[derive(Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct ProviderCredentialInput {
    pub provider_id: String,
    pub token: String,
    pub source: String,
    pub scopes: Vec<String>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl fmt::Debug for ProviderCredentialInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderCredentialInput")
            .field("provider_id", &self.provider_id)
            .field("token", &"[REDACTED]")
            .field("source", &self.source)
            .field("scopes", &self.scopes)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// Spawn parameters carried alongside `Command::InjectPrompt` so the
/// daemon can fall back to creating a fresh terminal when the cached
/// `terminal_id` no longer exists (agent died between the user's `w`
/// press and the command arriving). Carries every spawn field that is
/// not already supplied by `InjectPrompt` itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct SpawnFallback {
    pub session_key: SessionKey,
    #[serde(default)]
    pub session_id: Option<lazybox_core::SessionId>,
    #[serde(default)]
    pub client_request_id: Option<String>,
    pub kind: TerminalKind,
    pub cwd: Option<String>,
    #[serde(default)]
    pub model_alias: Option<String>,
    #[serde(default)]
    pub access: AgentRunAccess,
}

/// One row of `Event::WorktreesInspected`. Mirrors
/// `lazybox_git_ops::WorktreeInspection` as a wire-friendly value type
/// (no `SystemTime`, no library-specific enum). `reasons` carries the
/// short tags from `OrphanReason::tag()` so clients can render
/// without needing to depend on `lazybox-git-ops`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct WorktreeInspectionDto {
    pub path: std::path::PathBuf,
    pub bare_path: Option<std::path::PathBuf>,
    pub branch: Option<String>,
    pub session_id: Option<String>,
    pub reasons: Vec<String>,
    pub size_bytes: u64,
    /// Most-recent mtime in the worktree, as seconds since the Unix
    /// epoch. `None` when the directory is gone (prunable entries).
    pub last_modified_unix: Option<u64>,
    pub has_uncommitted_changes: bool,
    pub has_unpushed_commits: bool,
    pub is_safe_to_delete: bool,
}

/// Wire-friendly projection of a combined staged/unstaged worktree diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct WorkspaceDiffDto {
    pub status: Vec<String>,
    pub stat: Vec<String>,
    pub files: Vec<DiffFileDto>,
    pub truncated: bool,
}

/// Exact checkout whose local changes should be reviewed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub enum WorkspaceDiffTarget {
    Session(lazybox_core::SessionId),
    LinkedCheckout,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct DiffFileDto {
    pub old_path: Option<String>,
    pub path: String,
    pub headers: Vec<String>,
    pub hunks: Vec<DiffHunkDto>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct DiffHunkDto {
    pub header: String,
    pub old_start: u32,
    pub new_start: u32,
    pub lines: Vec<DiffLineDto>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct DiffLineDto {
    pub kind: DiffLineKindDto,
    pub text: String,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub enum DiffLineKindDto {
    Context,
    Addition,
    Deletion,
    Meta,
}

/// One row of `Event::CheckoutsDiscovered` — an on-disk git checkout
/// found by the dev-folder scan, mapped to its GitHub repo when the
/// origin allows. Wire-friendly projection of
/// `lazybox_git_ops::DiscoveredCheckout` plus the resolved `repo`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct DiscoveredCheckoutDto {
    /// Absolute path to the checkout's working directory.
    pub path: std::path::PathBuf,
    /// `owner/repo` resolved from the `origin` remote, or `None` when
    /// the checkout has no origin / a non-GitHub origin.
    pub repo: Option<String>,
    /// Checked-out branch, or `None` on a detached HEAD.
    pub branch: Option<String>,
    /// `git status --porcelain` reported at least one entry.
    pub has_uncommitted_changes: bool,
}

/// serde `default` for a JSON `bool` field that must default to `true`.
/// Bincode is not self-describing and cannot apply this to an older, shorter
/// command; socket peers with that shape are rejected by
/// [`PROTOCOL_FINGERPRINT`] negotiation.
fn default_true() -> bool {
    true
}

/// TUI → daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub enum Command {
    /// Start streaming events. Connection replies with `Event::Snapshot`
    /// then a live stream.
    Subscribe,
    /// Create a fresh `Session` (== fresh worktree folder) inside the
    /// workspace identified by `session_key` (this name on the wire
    /// holds the workspace key — see the SessionKey docs). The
    /// daemon allocates a new `SessionId`, sets up the worktree on
    /// disk, and emits `Event::SessionCreated`. The TUI uses this
    /// when the user explicitly wants a separate folder from any
    /// existing sessions.
    CreateSession {
        session_key: SessionKey,
        kind: TerminalKind,
        /// Optional friendly label. Defaults to the kind's name.
        label: Option<String>,
    },
    /// Spawn a terminal inside a session. `session_id == Some(id)`
    /// targets that specific session; `None` lets the daemon pick the
    /// workspace's default session (creating one on the fly when the
    /// workspace has no sessions yet). The session supplies the cwd
    /// (its worktree path). `cwd` may override that for ad-hoc spawns.
    Spawn {
        session_key: SessionKey,
        #[serde(default)]
        session_id: Option<lazybox_core::SessionId>,
        /// Client-generated correlation id echoed by the command outcome.
        #[serde(default)]
        client_request_id: Option<String>,
        kind: TerminalKind,
        cwd: Option<String>,
        /// Optional initial prompt the daemon should inject after the
        /// agent reaches its ready state. Drives the `f`-for-fix flow:
        /// sidebar/activity panes pre-build the agent instruction so
        /// the user doesn't have to retype it. Ignored for `Shell` —
        /// shells don't define `Agent::inject_prompt`.
        #[serde(default)]
        initial_prompt: Option<String>,
        /// Run this session on the repo's shared **main checkout**
        /// (the default branch, in one worktree reused across the
        /// repo) instead of provisioning an isolated per-task
        /// worktree. Riskier — changes land directly on the shared
        /// branch — so the client gates it behind a confirm. Ignored
        /// when `cwd` is overridden.
        #[serde(default)]
        on_main: bool,
        /// Model-tier alias the user picked via the `w S` / `a M`
        /// chords (`"S"`, `"M"`, `"L"`). The daemon resolves it against
        /// the target agent's tier menu and appends the tier's args to
        /// the spawn argv. `None` → the agent's configured-or-hardcoded
        /// default model.
        #[serde(default)]
        model_alias: Option<String>,
        /// Host access policy for the spawned agent. Shell and log
        /// terminals ignore this field.
        #[serde(default)]
        access: AgentRunAccess,
    },
    /// Cancel an in-flight `Spawn` for this workspace that is still
    /// provisioning its worktree (cold clone / fetch). The daemon
    /// aborts the provision — killing the underlying `git`/transport
    /// child so a stalled clone doesn't linger orphaned — and releases
    /// the in-flight singleton claim so a retry starts fresh. A no-op
    /// when nothing is in flight (the spawn already finished or
    /// failed), so the client can send it unconditionally on Esc.
    CancelSpawn {
        session_key: SessionKey,
    },
    Write {
        terminal_id: TerminalId,
        bytes: Vec<u8>,
        intent: TerminalInputIntent,
    },
    /// Append a prompt the user submitted to an agent terminal to that
    /// terminal's bounded per-session history (issue #523), so the
    /// pinned "you ▸ …" recap and the `]]h` history view survive a
    /// restart. The prompt is composed client-side from the *outgoing*
    /// keystroke stream (see `TerminalSlot::record_pty_bytes`), which the
    /// daemon never reconstructs, so the client reports each committed
    /// prompt here for the daemon to append against the terminal's
    /// backend key. Replayed back to clients via
    /// `TerminalSnapshot::prompt_history`.
    RecordUserMessage {
        terminal_id: TerminalId,
        prompt: UserPrompt,
    },
    /// Inject a prompt into an EXISTING agent terminal — same flow
    /// the daemon uses for Spawn's `initial_prompt`, but targeting
    /// a live terminal so `w fix CI` / `w address comments` reuses
    /// the user's running claude tab instead of spawning a second
    /// one. The daemon looks up the agent for `terminal_id` and
    /// asks the agent's PTY protocol for one atomic prompt write sequence,
    /// the same paste/settle/submit flow used at spawn time.
    ///
    /// `fallback_spawn` covers the race where the TUI cached the
    /// agent's terminal id, the agent died, `TerminalExited` is in
    /// flight but the user already pressed `w`. Without the fallback
    /// the daemon used to silently no-op and the prompt was lost.
    /// When set, an unknown / dead `terminal_id` is rewritten back
    /// into a Spawn with the carried parameters + `prompt` as the
    /// initial prompt.
    InjectPrompt {
        terminal_id: TerminalId,
        prompt: String,
        #[serde(default)]
        fallback_spawn: Option<SpawnFallback>,
        /// Whether to press Enter after pasting `prompt`. `true` for the
        /// `w` / snippet inject paths (paste + run). `false` for prompt
        /// *recall* (`]]r`): the recovered text is dropped into the
        /// composer for the user to edit and submit themselves, so the
        /// daemon skips the settle-gated submit keystroke.
        #[serde(default = "default_true")]
        submit: bool,
    },
    /// Persist the in-flight composer buffer (typed but not yet
    /// submitted) for an agent terminal so a half-typed prompt survives
    /// a lazybox restart. Sent whenever the buffer changes; an empty
    /// `buffer` clears the stored draft. The daemon keys it by backend
    /// session key and replays it via `TerminalSnapshot::composing_buffer`
    /// so a reconnecting or restarted client can recall it (`]]r`).
    RecordComposingBuffer {
        terminal_id: TerminalId,
        buffer: String,
    },
    Resize {
        terminal_id: TerminalId,
        cols: u16,
        rows: u16,
    },
    /// Defense-in-depth recovery request from a client that observed a
    /// terminal sequence gap below the daemon's normal drop/resync path.
    /// The daemon replies with `TerminalResync` only when an authoritative
    /// replay covers `required_seq`; otherwise it replies
    /// `TerminalResyncUnavailable` and the client retries on later output.
    RequestTerminalResync {
        terminal_id: TerminalId,
        required_seq: u64,
    },
    Close {
        terminal_id: TerminalId,
        /// Client-generated correlation id used to report a backend kill
        /// failure. Successful closes still complete with `TerminalExited`.
        #[serde(default)]
        client_request_id: Option<String>,
    },
    /// A lifecycle hook fired by an agent (Claude Code), forwarded by
    /// the `lazybox hook-ingest` helper the daemon injects at spawn. The
    /// daemon maps it to an [`AgentState`] transition — deterministic
    /// state, no PTY screen-scraping.
    ///
    /// Correlation is by `backend_key` — the stable backend session
    /// key (tmux session name) lazybox baked into the hook command at
    /// spawn. Backend keys survive daemon restarts, unlike
    /// `TerminalId`s, which restart at the process boundary while a
    /// tmux-backed agent keeps running with the old id in its settings
    /// file. `terminal_id` is the legacy correlation field from
    /// pre-backend-key settings files; the daemon drops hooks that
    /// carry only it (such a session just falls back to PTY
    /// detection). The field stays in this position — and
    /// `backend_key` is appended last — because the socket transport
    /// encodes commands with bincode, which is field-order sensitive.
    /// The `#[serde(default)]` below only takes effect on the JSON
    /// gateway; bincode is not self-describing and never applies
    /// defaults, so appending this field was a wire-format change
    /// protected by the wire-fingerprint handshake, not by the
    /// attribute.
    IngestHook {
        terminal_id: TerminalId,
        hook: HookEvent,
        #[serde(default)]
        backend_key: Option<String>,
    },
    Kill {
        session_key: SessionKey,
    },
    /// Answer to a `MergedPrRemovable` event (the user confirmed the
    /// "this PR merged — remove its workspace and worktree?" modal).
    /// Kills the workspace's sessions, force-deletes its backing
    /// worktree directories, then drops the row — the worktree
    /// deletion `Kill` deliberately skips. `force` is implied by the
    /// confirm modal having warned about any uncommitted/unpushed
    /// work, so the daemon always reaps the dirs here.
    RemoveMergedWorkspace {
        session_key: SessionKey,
    },
    /// Delete a Project: kill every workspace under it (which kills
    /// every backing terminal) then drop the Project record. The
    /// daemon broadcasts `WorkspaceRemoved` for each workspace then
    /// `ProjectRemoved` for the project so the sidebar can drop the
    /// rows in one batch. Destructive — gated by the unified
    /// ActionConfirm modal on the TUI side.
    DeleteProject {
        project_key: lazybox_core::ProjectKey,
    },
    /// Manually collapse an issue workspace into the PR workspace
    /// that closes it. Same end-state as the auto-detect path
    /// (`merge_closing_issue_workspaces`) but invoked by the user —
    /// bypasses the `rejected_merge` / `prompted_merge` dedupe
    /// state so a previously-dismissed prompt becomes actionable
    /// again. The daemon picks the target PR by scanning workspaces
    /// for one whose `closes_issues` includes this issue.
    CollapseIntoPr {
        issue_workspace_key: SessionKey,
    },
    MarkRead {
        session_key: SessionKey,
    },
    /// Hint to the daemon: the user is now looking at this
    /// workspace. The polling layer uses it to bump the workspace's
    /// repo to the front of the round-robin sync cursor so a
    /// comment landing on the visible PR shows up next cycle
    /// instead of waiting the rest of the rotation. No store
    /// mutation, no broadcast — pure scheduling hint.
    ///
    /// Sent on sidebar cursor moves alongside the existing
    /// `MarkRead`. The daemon silently ignores it if the workspace
    /// has no upstream repo (e.g. a locally-created pre-PR
    /// sandbox).
    FocusWorkspace {
        session_key: SessionKey,
    },
    /// A desktop-notification click asks every attached TUI to jump to
    /// the workspace that raised the banner. Kept separate from
    /// [`Command::FocusWorkspace`], which every ordinary cursor move
    /// emits as a polling hint and must not move other clients.
    ActivateWorkspace {
        session_key: SessionKey,
    },
    /// Mark exactly one activity row as read. The auto-mark-on-hover
    /// flow uses this so a brief glance at one comment doesn't flip
    /// the whole workspace's unread badge to zero. `index` is the
    /// activity slot in `Workspace.activity` after the daemon's
    /// `sort_activity` pass — the same view the TUI saw *when it
    /// resolved the row*. Because a poll can commit a new top-of-feed
    /// comment between that snapshot and this command landing (which
    /// shifts every older index), `fingerprint` carries the row's
    /// stable identity: the daemon resolves it against its CURRENT
    /// list (index serves as a position hint / twin disambiguator)
    /// and no-ops when the row is gone, instead of marking whatever
    /// now sits at `index`. `None` keeps the raw-index behavior for
    /// clients that don't track fingerprints (JSON API scripts).
    MarkActivityRead {
        session_key: SessionKey,
        index: usize,
        #[serde(default)]
        fingerprint: Option<lazybox_core::ActivityFingerprint>,
    },
    /// Reverse a previous `MarkActivityRead`. Bound to the `z` undo
    /// affordance.
    UnmarkActivityRead {
        session_key: SessionKey,
        index: usize,
    },
    /// Create a brand-new pre-PR workspace with a user-chosen name
    /// inside a specific Project. The daemon allocates a fresh
    /// `WorkspaceKey` (slug-based, with a numeric suffix on
    /// collision), stamps the workspace with `project_key`, persists,
    /// and broadcasts. Used by the sidebar's `n` key — which requires
    /// the cursor to be on a Project header (or a workspace under one)
    /// so `project_key` is always resolvable.
    CreateWorkspace {
        name: String,
        /// The Project this workspace lives under. The TUI resolves
        /// it from the sidebar cursor before sending; the daemon
        /// trusts the value (no project-exists check today — the
        /// `n` flow can't fire without a focused project, and a
        /// stale key just produces an orphan workspace).
        project_key: lazybox_core::ProjectKey,
        /// When `Some(agent_id)`, the daemon immediately spawns that
        /// agent (claude / codex / cursor / …) into the freshly
        /// created workspace, so the user lands in a live session
        /// instead of an empty row. `None` leaves the workspace bare.
        /// The TUI sets this to the configured default agent for both
        /// the `n` key and the global "start agent" shortcut.
        spawn_agent: Option<String>,
    },
    /// Create a brand-new local Project — a top-level container the
    /// sidebar groups workspaces under, like a github repo but with
    /// no upstream provider. Slugified to `local-<slug>`; idempotent
    /// on collision (re-opens the existing project, same shape as
    /// `polling::ensure_project_for_workspace` for provider
    /// projects). Bound to `x p` in the default keymap.
    CreateProject {
        name: String,
    },
    /// Update the per-session tile/tab layout (`SessionLayout`).
    /// Persisted so the user's split arrangement survives restart.
    /// `layout_json` carries the serialized `lazybox_core::SessionLayout`
    /// — a string here keeps the wire type free of a core dep without
    /// forcing the IPC crate into the workspace types.
    SetSessionLayout {
        session_key: SessionKey,
        session_id_raw: String,
        layout_json: String,
    },
    Snooze {
        session_key: SessionKey,
        until: chrono::DateTime<chrono::Utc>,
    },
    Unsnooze {
        session_key: SessionKey,
    },
    /// Set the workspace's client-side "auto-merge on green" arm. When
    /// armed, the TUI auto-fires `MergePr` once the workspace's own PR
    /// becomes merge-ready. The daemon just persists the flag on the
    /// `Workspace` (like `Snooze`) and re-broadcasts; the merge decision
    /// and dispatch stay client-side.
    SetAutoMergeOnGreen {
        session_key: SessionKey,
        enabled: bool,
    },
    /// Set the workspace's "track main" arm (issue #535). When enabled,
    /// the daemon's background sweep keeps this workspace's worktree
    /// fast-forwarded to `origin/<default>` while the tree is clean. The
    /// daemon persists the flag on the `Workspace` (like
    /// `SetAutoMergeOnGreen`), resolves and stores the base branch, and
    /// re-broadcasts.
    SetTrackMain {
        session_key: SessionKey,
        enabled: bool,
    },
    /// Set the per-session auto-fix arm for one [`lazybox_core::AutoFixKind`]
    /// on the workspace (issue #363). `Arm` overrides a label opt-out, `Disarm`
    /// forces auto-fix off for this workspace, `Default` follows the
    /// global config. The daemon persists it on the `Workspace`
    /// (like `SetAutoMergeOnGreen`) and re-broadcasts; the auto-fix
    /// dispatcher reads it back to gate the fix.
    SetAutoFixPolicy {
        session_key: SessionKey,
        kind: lazybox_core::AutoFixKind,
        arm: lazybox_core::PolicyArm,
    },
    /// Atomically set both auto-fix arms for the workspace. The direct
    /// one-key toggle uses one command so bounded transports cannot admit
    /// only half of the requested policy change.
    SetAutoFixPolicies {
        session_key: SessionKey,
        ci: lazybox_core::PolicyArm,
        conflict: lazybox_core::PolicyArm,
    },
    /// Post a top-level reply to the workspace's primary task. Today
    /// this maps to "create an issue/PR comment" on GitHub; future
    /// providers (Linear, etc.) wire their own send path. The daemon
    /// posts via the workspace's owning provider, then `Refresh`-es so
    /// the new comment lands in the activity feed on the next poll.
    PostReply {
        session_key: SessionKey,
        body: String,
    },
    Refresh,
    Shutdown,
    /// Answer to a `WorkspaceMergePending` event. When the daemon
    /// detects a PR that `closes` an issue whose workspace has live
    /// sessions, it stalls the merge and asks via the TUI. The TUI
    /// replies here: `accept=true` runs the merge (sessions move to
    /// the PR workspace, issue row disappears); `accept=false`
    /// leaves both rows visible and the stall is dropped — the
    /// merge won't be re-prompted for this issue until lazybox
    /// restarts.
    ConfirmMerge {
        issue_workspace_key: lazybox_core::WorkspaceKey,
        pr_workspace_key: lazybox_core::WorkspaceKey,
        accept: bool,
    },
    /// Manual "adopt": move all sessions from `source_workspace_key`
    /// into `target_workspace_key`. Driven by the sidebar's `x a`
    /// picker — useful when you started work on the wrong row and
    /// want to migrate the running agent without losing it. Unlike
    /// the issue→PR merge, the source workspace is NOT deleted; it
    /// just becomes a session-less tracking row the user can ignore
    /// or remove via `x x`.
    AdoptSessions {
        source_workspace_key: lazybox_core::WorkspaceKey,
        target_workspace_key: lazybox_core::WorkspaceKey,
    },
    /// Merge the workspace's PR via the provider. Fires from the
    /// sidebar's `g m` shortcut on a READY (approved + green
    /// CI) row. The daemon looks up the PR's `node_id` and calls
    /// the GraphQL `mergePullRequest` mutation. Method defaults
    /// to the repo's setting; future per-repo config can override.
    MergePr {
        workspace_key: lazybox_core::WorkspaceKey,
    },
    /// Close the workspace's GitHub issue upstream. Fires from the
    /// sidebar's `x c` shortcut on an issue-only workspace, after
    /// a confirm. GitHub has no non-admin "delete issue" via the API,
    /// so the daemon closes the issue (state `NOT_PLANNED`) via the
    /// GraphQL `closeIssue` mutation. The next poll picks up the
    /// closed state and offers the usual workspace-removal prompt.
    CloseIssue {
        workspace_key: lazybox_core::WorkspaceKey,
    },
    /// Delete or close the workspace's primary upstream item, resolved
    /// by kind: a PR is closed without merging (`closePullRequest`);
    /// an issue is hard-deleted (`deleteIssue`) when the token has the
    /// admin rights GitHub requires, degrading to a NOT_PLANNED close
    /// otherwise. Fires from the github leader's `g d` chord, after a
    /// confirm. The next poll's rescope sweep removes the vanished
    /// item's workspace from the inbox.
    DeleteOrClose {
        workspace_key: lazybox_core::WorkspaceKey,
    },
    /// Request reviews on the workspace's PR from the given GitHub
    /// logins. Adds to the existing reviewer set (no replacement).
    /// Only meaningful when the focused workspace's primary task is
    /// a PR — daemon resolves the PR's node ID from its stored task
    /// and calls the `requestReviews` GraphQL mutation. Logins
    /// flow through unchanged; the daemon resolves each to a node
    /// ID before issuing the mutation.
    RequestReviewers {
        workspace_key: lazybox_core::WorkspaceKey,
        logins: Vec<String>,
    },
    /// Add assignees to the workspace's PR or issue. Works on any
    /// `Assignable` (both PRs and issues). Same flow as
    /// `RequestReviewers` — daemon resolves logins → node IDs and
    /// calls the `addAssigneesToAssignable` mutation.
    AddAssignees {
        workspace_key: lazybox_core::WorkspaceKey,
        logins: Vec<String>,
    },
    /// Replace the assignee set on the workspace's PR / issue.
    /// Daemon diffs against the currently-persisted assignees and
    /// fires `addAssigneesToAssignable` + `removeAssigneesFromAssignable`
    /// as needed so this is a single atomic-feeling "set to exactly
    /// this list" operation from the TUI's POV. Empty list clears
    /// every assignee.
    SetAssignees {
        workspace_key: lazybox_core::WorkspaceKey,
        logins: Vec<String>,
    },
    /// Replace the label set on the workspace's PR / issue. Daemon
    /// diffs against the currently-persisted labels and fires
    /// `addLabelsToLabelable` + `removeLabelsFromLabelable` as
    /// needed. Empty list clears every label. Names are matched
    /// against the repo's full label set; names that don't exist on
    /// the repo are silently dropped.
    SetLabels {
        workspace_key: lazybox_core::WorkspaceKey,
        names: Vec<String>,
    },
    /// Ask the daemon to fetch the repository's full label set for
    /// the workspace's PR / issue and broadcast it back via
    /// `Event::RepoLabels`. Used by the label picker on mount so
    /// the user can pick from every label the repo defines (not
    /// just the ones currently applied).
    FetchRepoLabels {
        workspace_key: lazybox_core::WorkspaceKey,
    },
    /// Admin command: walk every persisted workspace, drop sessions
    /// whose terminals aren't currently live, and remove the
    /// corresponding worktrees from disk. Used to reclaim disk
    /// space without losing the inbox (the PR / issue rows stay —
    /// only the working-tree directories + session records are
    /// torn down). Live terminals are skipped so a running claude
    /// agent isn't pulled out from under itself.
    ///
    /// Daemon broadcasts a `WorkspaceUpserted` for every workspace
    /// it touched so the sidebar shows the now-empty session list.
    /// A final `CleanWorktreesCompleted` lets the TUI surface
    /// "cleaned N worktrees, freed N GB" in the footer.
    CleanWorktrees,
    /// Walk `<state_root>/worktrees/` + every bare clone, classify
    /// each entry (orphan reasons, size, last modified, uncommitted /
    /// unpushed work), and reply with `Event::WorktreesInspected`.
    /// Read-only — no deletes happen until the TUI follows up with
    /// per-row `DeleteOrphanedWorktree` calls.
    InspectWorktrees,
    /// Read the focused checkout and reply
    /// with `Event::WorkspaceDiffInspected`.
    InspectWorkspaceDiff {
        workspace_key: lazybox_core::WorkspaceKey,
        target: WorkspaceDiffTarget,
    },
    /// Walk the configured dev roots (`scan.roots`, or `roots` when the
    /// user pointed the scan at an explicit folder) and reply with
    /// `Event::CheckoutsDiscovered` — every on-disk git clone found,
    /// mapped to its GitHub `owner/repo` where the origin allows.
    /// Read-only discovery; importing a checkout is the separate
    /// `ImportLocalCheckout` step. `roots` empty ⇒ use `scan.roots`.
    ScanCheckouts {
        #[serde(default)]
        roots: Vec<std::path::PathBuf>,
    },
    /// Import a discovered on-disk checkout as a **linked (no-worktree)
    /// workspace**: lazybox points at `path` directly and every session
    /// spawns there, on its current branch, with no worktree provisioned
    /// and no bare clone. The daemon re-describes `path` to derive the
    /// repo (`origin` → `owner/repo`) and current branch, creates the
    /// workspace under that repo's project, and emits `WorkspaceUpserted`.
    /// `spawn_agent = Some(id)` immediately starts that agent in the
    /// checkout; `None` just creates the tracking workspace.
    ImportLocalCheckout {
        path: std::path::PathBuf,
        #[serde(default)]
        spawn_agent: Option<String>,
    },
    /// Delete a single worktree the inspector flagged.
    ///
    /// `force = false` makes the daemon re-check safety on its side
    /// (uncommitted / unpushed / locked) and refuse if anything looks
    /// risky. `force = true` overrides that gate — only sent after an
    /// explicit user confirm for a row with local work. Reply arrives
    /// as `Event::OrphanedWorktreeDeleted`.
    DeleteOrphanedWorktree {
        path: std::path::PathBuf,
        #[serde(default)]
        force: bool,
    },
    /// Lazy-fetch the workspace's PR-detail activity (review-thread
    /// comments). The inbox-scan query intentionally skips
    /// `reviewThreads` for cost reasons; this command back-fills
    /// them when the user opens the workspace's activity pane. The
    /// daemon merges the result into the workspace's activity list
    /// and broadcasts an updated `WorkspaceUpserted`. No-op when
    /// the workspace has no PR (issues don't have review threads).
    FetchPrDetails {
        workspace_key: lazybox_core::WorkspaceKey,
    },
    /// Targeted re-poll of a single workspace's own GitHub entities (its
    /// PR and any linked issues) instead of the global `Refresh` sweep —
    /// the "sync this" action. The daemon deep-fetches each entity by
    /// `(owner, repo, number)` and upserts the result, so read markers
    /// and state update just for that row. Cheap next to a full sweep;
    /// no-op for a workspace with no GitHub PR/issue.
    SyncWorkspace {
        workspace_key: lazybox_core::WorkspaceKey,
    },
    /// Start an agent runtime using a structured protocol surface. This
    /// does not replace `Spawn`; terminal clients can keep using PTY
    /// bytes while structured clients subscribe to run events.
    StartAgentRun {
        request_id: AgentRunRequestId,
        session_key: SessionKey,
        #[serde(default)]
        session_id: Option<lazybox_core::SessionId>,
        /// Resolve the run from this live terminal's daemon-owned session
        /// instead of trusting a client-side session hint.
        #[serde(default)]
        source_terminal_id: Option<TerminalId>,
        agent: String,
        mode: AgentRuntimeMode,
        cwd: Option<String>,
        initial_input: Option<AgentInputMessage>,
        /// Resume the most recent provider conversation in the resolved
        /// cwd instead of starting with empty context.
        #[serde(default)]
        resume_latest: bool,
        #[serde(default)]
        access: AgentRunAccess,
    },
    SendAgentInput {
        run_id: AgentRunId,
        message: AgentInputMessage,
    },
    InterruptAgentRun {
        run_id: AgentRunId,
    },
    DecideAgentApproval {
        run_id: AgentRunId,
        request_id: String,
        decision: AgentApprovalDecision,
    },
    AnswerAgentQuestion {
        run_id: AgentRunId,
        question_id: String,
        answer: AgentQuestionAnswer,
    },
    /// Store/update a provider credential for one Lazybox principal.
    /// This is the bootstrap path for local desktop clients; future
    /// API connection auth can make `principal_id` implicit.
    UpsertProviderCredential {
        principal_id: PrincipalId,
        credential: ProviderCredentialInput,
    },
    RemoveProviderCredential {
        principal_id: PrincipalId,
        provider_id: String,
    },
    ListProviderCredentials {
        principal_id: PrincipalId,
    },
    /// Explicit "no" to a `MergedPrRemovable` prompt: keep the
    /// workspace and its worktree. Pins the workspace in the daemon's
    /// removal-prompt memory so the level-triggered re-emit stops
    /// asking for the rest of the session. An Esc dismissal sends
    /// nothing — the daemon re-prompts after its reprompt interval,
    /// same semantics as the issue→PR merge modal.
    ///
    /// Appended last: bincode identifies variants by ordinal, so this
    /// position keeps the change mechanical.
    KeepMergedWorkspace {
        session_key: SessionKey,
    },
    /// Ask the daemon for the terminal's deep scrollback, rebuilt from
    /// the backend's own history (tmux `capture-pane`) rather than the
    /// in-memory replay ring. Sent when the user scrolls a live
    /// terminal up into local scrollback: a full-screen agent's
    /// in-place redraws leave almost nothing in the client's
    /// libghostty scrollback, while tmux has been retaining
    /// `history-limit` lines the whole time — the same history the
    /// restart/reattach path already seeds from. The daemon replies on
    /// this connection with [`Event::TerminalScrollback`]; backends
    /// without a history source (raw PTY) reply nothing.
    ///
    /// Appended last: bincode identifies variants by ordinal, so this
    /// position keeps the change mechanical.
    FetchScrollback {
        terminal_id: TerminalId,
    },
    /// Re-run the out-of-band agent-CLI version check now and report
    /// via `Event::AgentCliUpdatesChecked` (with `manual: true`).
    /// Appended last — after `FetchScrollback`, which shipped at
    /// protocol 12; see `KeepMergedWorkspace`.
    CheckAgentCliUpdates,
    /// Update every enabled agent CLI through its lazybox-managed
    /// channel — the sanctioned replacement for the in-session
    /// self-updaters lazybox suppresses at spawn. Runs detached from
    /// any session PTY; each agent's outcome arrives as an
    /// `Event::AgentCliUpdateFinished`. Appended last; see
    /// `KeepMergedWorkspace`.
    UpdateAgentClis,
    /// Persist the workspace's free-form local note (issue #458). The
    /// note is a lazybox-only scratchpad — never synced to a provider.
    /// The daemon loads the workspace, replaces its `notes`, and
    /// re-broadcasts `WorkspaceUpserted` (like `Snooze`), so no
    /// dedicated read command is needed — the note rides the existing
    /// workspace snapshot. Appended last; see `KeepMergedWorkspace`.
    SetNotes {
        session_key: SessionKey,
        notes: String,
    },
    /// Update the workspace's PR branch against its base — the "Update
    /// branch" button on github.com. Fires from the sidebar's `g u`
    /// shortcut on a `BEHIND` row (and the bulk `Shift-U` fan-out). The
    /// daemon looks up the PR's `node_id` and calls the GraphQL
    /// `updatePullRequestBranch` mutation, then wakes the poll so the
    /// `BEHIND` tag clears. Appended last (bincode is ordinal-sensitive).
    UpdateBranch {
        workspace_key: lazybox_core::WorkspaceKey,
    },
    /// Deliver a snippet to a live terminal and record its histories only
    /// after the backend accepts the write (and, for an agent composer,
    /// submission is confirmed). The daemon derives the workspace from
    /// `terminal_id`, preventing a client from attributing a delivery to a
    /// different workspace. Success is reported through
    /// [`Event::SnippetDelivered`]; failure through
    /// [`Event::TerminalInputRejected`]. Appended last (bincode is
    /// ordinal-sensitive).
    DeliverSnippet {
        terminal_id: TerminalId,
        snippet_key: String,
        category: String,
        body: String,
    },
    /// Mark an update target as dismissed so the startup update modal
    /// stops re-appearing for it (issue #548). `target` is the opaque
    /// `source:<sha>` / `release:<version>` key the build guard derives;
    /// a newer target is never covered by an older dismissal. The daemon
    /// persists the dismissed set and replays it via `Event::Snapshot`'s
    /// `dismissed_updates`, so dismissing on one client (in-process or
    /// `--connect`) sticks for all of them instead of writing a
    /// client-local `state.db`. Appended last (bincode is
    /// ordinal-sensitive).
    SetUpdateDismissal {
        target: String,
    },
    /// Resume the exact agent conversation represented by a frozen exited
    /// pane. The daemon retains the launch metadata for the terminal id and
    /// uses the provider's exact-session resume builder.
    ResumeAgent {
        terminal_id: TerminalId,
    },
    /// Confirm provider-owned interactive authentication for one blocked
    /// agent terminal.
    ReauthenticateAgent {
        terminal_id: TerminalId,
        switch_account: bool,
    },
    /// Cancel only the authentication subprocess created for this terminal.
    CancelAgentReauthentication {
        terminal_id: TerminalId,
    },
    /// Rename a workspace's display name (issue #744). The daemon loads
    /// the workspace, replaces its `name`, and re-broadcasts
    /// `WorkspaceUpserted` (like `SetNotes`), so attached TUIs update the
    /// sidebar label. Only the display name changes — the workspace key
    /// and any session worktrees are left untouched. Appended last
    /// (bincode is ordinal-sensitive).
    RenameWorkspace {
        session_key: SessionKey,
        name: String,
    },
}

impl Command {
    /// Build the [`Command::Write`]s for `bytes`, split into chunks of
    /// at most [`MAX_WRITE_CHUNK_BYTES`] so each command frames under
    /// the daemon's [`MAX_COMMAND_FRAME_BYTES`] ingress cap (one
    /// unchunked >256 KiB paste used to kill a `--connect` session:
    /// the oversized frame tripped the daemon's bounded command
    /// reader).
    ///
    /// Chunking happens at raw byte boundaries: PTY input is a byte
    /// stream, so splitting mid-UTF-8 or mid-escape-sequence is fine —
    /// the PTY reassembles the exact same stream. Ordering is
    /// preserved because every chunk travels the same ordered command
    /// channel as the original write would have.
    pub fn write_chunked(
        terminal_id: TerminalId,
        bytes: Vec<u8>,
        intent: TerminalInputIntent,
    ) -> Vec<Command> {
        if bytes.len() <= MAX_WRITE_CHUNK_BYTES {
            // Common case: no split, no copy.
            return vec![Command::Write {
                terminal_id,
                bytes,
                intent,
            }];
        }
        let chunks: Vec<_> = bytes.chunks(MAX_WRITE_CHUNK_BYTES).collect();
        let last = chunks.len().saturating_sub(1);
        chunks
            .into_iter()
            .enumerate()
            .map(|(index, chunk)| Command::Write {
                terminal_id,
                bytes: chunk.to_vec(),
                intent: if index == last {
                    intent
                } else {
                    TerminalInputIntent::Compose
                },
            })
            .collect()
    }

    /// Normalize this command for the wire: an oversized
    /// [`Command::Write`] becomes several in-order chunked writes (see
    /// [`Command::write_chunked`]); every other command passes through
    /// untouched. The socket client's outbound path runs each command
    /// through this so no single write can exceed the daemon's
    /// command-frame cap, regardless of which call site emitted it.
    pub fn into_write_chunks(self) -> Vec<Command> {
        match self {
            Command::Write {
                terminal_id,
                bytes,
                intent,
            } => Command::write_chunked(terminal_id, bytes, intent),
            other => vec![other],
        }
    }
}

/// Semantic origin of bytes written to a terminal.
///
/// Agent-state detection uses this at the I/O boundary: `Submit` is
/// authoritative evidence that a turn may start, while `View` covers pointer
/// traffic that can repaint a full-screen terminal without asking the agent
/// to work. `Compose` changes the in-flight line without committing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub enum TerminalInputIntent {
    Compose,
    Submit,
    View,
}

/// The terminal state a removable workspace's primary task reached,
/// so the confirm modal can word its prompt correctly: a PR is
/// "merged", an issue is "closed". Both dispatch the same
/// `Command::RemoveMergedWorkspace` on "yes".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub enum RemovableTerminalState {
    Merged,
    Closed,
}

/// Connection → TUI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub enum Event {
    // ── Hierarchy reminder ────────────────────────────────────────
    //
    // Repo (string `"owner/name"` from the task's provider)
    //  └── Workspace (one unit-of-work; `Workspace`)
    //       └── Session (= one folder worktree; runtime state)
    //            └── Terminal (= one PTY rooted in that folder)
    //
    // Snapshot carries Workspace rows; Sessions and Terminals are
    // recovered separately so a client reconnecting mid-flight can
    // re-bind to its running agents. `WorkspaceUpserted` /
    // `WorkspaceRemoved` are the fan-out events; `TerminalSpawned` /
    // `TerminalOutput` / `TerminalExited` track the bottom layer.
    /// Initial snapshot reply to `Subscribe`. Sent once before the
    /// live stream starts so the client has a baseline. The row model
    /// is `Workspace` — one per worktree, holding the linked PR +
    /// issues; every component reads from the workspace directly and
    /// projects to a primary task via `workspace.primary_task()`.
    Snapshot {
        workspaces: Vec<lazybox_core::Workspace>,
        terminals: Vec<TerminalSnapshot>,
        /// Top-level Projects the daemon knows about. Sidebar
        /// headers render from here so empty projects (no workspaces
        /// yet) still appear. The `#[serde(default)]` only matters on
        /// self-describing transports (the JSON gateway); it does NOT
        /// make older daemons wire-compatible over the socket —
        /// bincode never applies defaults, so adding this field was a
        /// wire-format change guarded by the wire-fingerprint
        /// handshake; future trailing fields shift the fingerprint
        /// the same way.
        #[serde(default)]
        projects: Vec<lazybox_core::Project>,
        /// Global most-recently-used snippet keys, newest first, capped
        /// daemon-side (#548). Every client seeds its picker "Recent"
        /// group from this so the MRU is shared across in-process and
        /// `--connect` clients. Trailing `#[serde(default)]` field — same
        /// wire-fingerprint caveat as `projects` above.
        #[serde(default)]
        recent_snippets: Vec<String>,
        /// Update targets the user has dismissed (`source:<sha>` /
        /// `release:<version>`), so the startup update modal stays hidden
        /// for them across clients and restarts (#548). Trailing
        /// `#[serde(default)]` field — same caveat as `projects`.
        #[serde(default)]
        dismissed_updates: Vec<String>,
    },
    /// Authenticated user's login per provider source ("github" →
    /// "AntoineToussaint", etc.). Emitted once after the daemon's
    /// gh/linear client(s) initialize; the TUI uses these logins
    /// to render activity authored by the current user as `@me`
    /// instead of the bare login. Empty vec is valid — providers
    /// that don't have an identified user just omit themselves.
    ViewerIdentities {
        logins: Vec<(String, String)>,
    },
    /// The daemon's authoritative auto-fix policy config: the global
    /// enable switch and the label opt-out set. Emitted once per
    /// subscribe, as the last of the post-`Snapshot` pushes (after
    /// `ViewerIdentities`), so the policies menu (`g p`, issue #363)
    /// reflects what the **daemon** would actually do — not what the
    /// client's own local config happens to say. This matters in
    /// `--connect` remote mode, where the client reads a different config
    /// file than the daemon; without it an armed workspace could show a
    /// lit glyph while the remote daemon has auto-fix globally off
    /// (tracker #512). The per-session arm and the PR's labels already
    /// arrive authoritatively (on the `Workspace` / its `Task`); this
    /// supplies the remaining global inputs the client can't otherwise
    /// know.
    AutoFixPolicyConfig {
        enabled: bool,
        opt_out_labels: Vec<String>,
    },
    /// The daemon's effective command for newly spawned plain shells.
    /// Emitted once per subscribe so a remote client reports the shell
    /// selected by the host that owns the PTYs and reads the config.
    ShellCommandConfig {
        command: String,
        configured: bool,
    },
    /// A workspace was created or updated.
    /// Boxed because Workspace is several KB once activity is
    /// populated; keeping the `Event` enum slim avoids worst-case
    /// async-channel overhead.
    WorkspaceUpserted(Box<lazybox_core::Workspace>),
    WorkspaceRemoved(lazybox_core::WorkspaceKey),
    /// A project (top-level container — github repo, linear team,
    /// or local) was registered or updated. Sidebar headers render
    /// from these. Emitted both on initial snapshot replay AND when
    /// polling discovers a new repo/team.
    ProjectUpserted(Box<lazybox_core::Project>),
    /// A project was removed (user deleted a local project, or all
    /// scope/filter rules that pointed at it were dropped).
    ProjectRemoved(lazybox_core::ProjectKey),
    /// Daemon detected that this workspace fell out of scope (the
    /// user removed its repo, narrowed the filter so its task no
    /// longer matches, etc.) AND it has running terminals. The
    /// daemon does NOT auto-delete; instead it sends this event so
    /// the TUI can ask the user whether to kill the running sessions
    /// or keep the workspace anyway. The TUI responds with either
    /// `Command::Kill { session_key }` (drop everything) or by doing
    /// nothing (workspace stays out-of-scope but visible).
    WorkspaceOutOfScope {
        workspace_key: lazybox_core::WorkspaceKey,
        /// Compact identifier for the confirm modal — `owner/repo#N`
        /// for PRs/issues, the workspace key otherwise.
        label: String,
        /// Primary task title (PR/issue subject) when known. The TUI
        /// renders it inline so the user can recognize which work is
        /// about to be killed without memorizing PR numbers.
        #[serde(default)]
        title: Option<String>,
        /// Number of live terminals (Claude/codex/shell/…) the user
        /// would lose if they confirm removal.
        active_terminal_count: usize,
    },
    /// Daemon detected a PR that closes an issue and wants to merge
    /// the issue's workspace into the PR's — BUT the issue has live
    /// sessions. The TUI asks the user; reply via
    /// `Command::ConfirmMerge`. Without live sessions the daemon
    /// merges silently and emits `WorkspaceMerged` instead.
    WorkspaceMergePending {
        issue_workspace_key: lazybox_core::WorkspaceKey,
        pr_workspace_key: lazybox_core::WorkspaceKey,
        /// Compact `owner/repo#N` form for both sides — the TUI renders
        /// them in the confirm modal so the user can recognize what's
        /// about to fold without memorizing keys.
        issue_label: String,
        pr_label: String,
        /// Number of live terminals on the issue's side. The reason
        /// we're prompting in the first place; the modal text quotes
        /// it back so the user knows what they'd be moving.
        active_terminal_count: usize,
    },
    /// Daemon collapsed an issue workspace into its closing PR. Sent
    /// for both the silent (no live sessions) and the
    /// post-confirm paths. The TUI flashes a footer notice so the
    /// inbox-row disappearance isn't surprising.
    WorkspaceMerged {
        issue_workspace_key: lazybox_core::WorkspaceKey,
        pr_workspace_key: lazybox_core::WorkspaceKey,
        issue_label: String,
        pr_label: String,
    },
    /// The PR for `workspace_key` was just merged via `Command::MergePr`.
    /// The local Task still reads `Open` until the next poll catches
    /// up, so the TUI flashes a footer notice so the keypress doesn't
    /// look like a no-op.
    PrMerged {
        workspace_key: lazybox_core::WorkspaceKey,
        pr_label: String,
    },
    /// `Command::MergePr` failed at the GitHub merge API — the user
    /// pressed `g m` and the merge did NOT happen. Distinct from a
    /// generic retryable `ProviderError`: the TUI surfaces this as a
    /// prominent, persistent error naming the reason (not mergeable,
    /// branch protection, required checks, review required, conflict,
    /// permissions) rather than a self-fading flash. The PR stays
    /// Open/actionable.
    PrMergeFailed {
        workspace_key: lazybox_core::WorkspaceKey,
        pr_label: String,
        reason: String,
    },
    /// The issue for `workspace_key` was just closed via
    /// `Command::CloseIssue`. The local Task still reads `Open` until
    /// the next poll catches up, so the TUI flashes a footer notice so
    /// the keypress doesn't look like a no-op. The workspace-removal
    /// prompt follows from the daemon's open→closed detection on the
    /// next poll (which `handle_close_issue` wakes).
    IssueClosed {
        workspace_key: lazybox_core::WorkspaceKey,
        issue_label: String,
    },
    /// `Command::CloseIssue` failed at the GitHub API — the user
    /// pressed `x c` and the close did NOT happen. Surfaced as a
    /// prominent, persistent error naming the reason (e.g. missing
    /// permissions), mirroring `PrMergeFailed`. The issue stays
    /// Open/actionable.
    IssueCloseFailed {
        workspace_key: lazybox_core::WorkspaceKey,
        issue_label: String,
        reason: String,
    },
    /// The PR for `workspace_key` was closed (without merging) via
    /// `Command::DeleteOrClose`. Same "flash a notice now, poll
    /// reconciles later" contract as [`Event::PrMerged`].
    PrClosed {
        workspace_key: lazybox_core::WorkspaceKey,
        pr_label: String,
    },
    /// The issue for `workspace_key` was removed upstream via
    /// `Command::DeleteOrClose` — hard-deleted when the token had
    /// admin rights, otherwise (`fell_back_to_close`) closed as
    /// NOT_PLANNED because GitHub refused the delete. The TUI names
    /// the degradation so the user knows the issue still exists.
    IssueDeleted {
        workspace_key: lazybox_core::WorkspaceKey,
        issue_label: String,
        fell_back_to_close: bool,
    },
    /// `Command::DeleteOrClose` failed at the GitHub API — nothing was
    /// deleted or closed (for an issue, even the close fallback
    /// failed). Surfaced as a prominent, persistent error naming the
    /// reason, mirroring `PrMergeFailed` / `IssueCloseFailed`.
    DeleteOrCloseFailed {
        workspace_key: lazybox_core::WorkspaceKey,
        label: String,
        reason: String,
    },
    /// A workspace's primary task reached a terminal state (a PR
    /// merged, or an issue closed) and its workspace is a candidate for
    /// removal. Level-triggered: emitted on the open→terminal flip and
    /// then re-emitted by the daemon's per-tick reprompt scan for as
    /// long as the workspace keeps sessions and the user hasn't
    /// answered — so a broadcast lost to lag, a reconnect, or a daemon
    /// restart can't strand the workspace unprompted. The daemon has
    /// inspected the backing worktree(s); the TUI prompts the user —
    /// reusing the removal-confirm modal — and replies with
    /// `Command::RemoveMergedWorkspace` on yes or
    /// `Command::KeepMergedWorkspace` on no (which stops the
    /// re-prompts). An Esc dismissal sends nothing and the prompt
    /// self-heals after the reprompt interval.
    ///
    /// Suppressed when `worktree.auto_cleanup_merged` is enabled — that
    /// opt-in path reaps safe worktrees silently instead.
    MergedPrRemovable {
        workspace_key: lazybox_core::WorkspaceKey,
        /// Compact `owner/repo#N` form for the confirm modal copy.
        label: String,
        /// Whether the task merged (PR) or closed (issue), so the modal
        /// copy words its prompt correctly.
        terminal_state: RemovableTerminalState,
        /// Live terminals that removal would kill. Quoted back so the
        /// user knows what they'd lose.
        active_terminal_count: usize,
        /// Any backing worktree has uncommitted or unpushed work. The
        /// modal warns when set; removal force-deletes regardless.
        has_local_work: bool,
    },
    /// A pending workspace-removal prompt for `workspace_key` no longer
    /// applies and any outstanding `MergedPrRemovable` modal should be
    /// dismissed. Emitted when a closed issue reopens before its removal
    /// was acted on (issue #552): the workspace is alive again, so a
    /// stale "remove closed issue?" prompt must not linger. No-op for
    /// clients that never mounted a matching prompt.
    RemovalCancelled {
        workspace_key: lazybox_core::WorkspaceKey,
    },
    /// Response to `Command::FetchRepoLabels`. Carries the
    /// repository's full label set (name + color) so the TUI's
    /// label picker can populate without round-tripping a
    /// request/response correlation id — the picker is keyed by
    /// `workspace_key` so the receiver knows which mount to fill.
    RepoLabels {
        workspace_key: lazybox_core::WorkspaceKey,
        labels: Vec<lazybox_core::Label>,
    },
    /// A new session (= folder worktree) was provisioned inside its
    /// workspace. Sent in response to `Command::CreateSession` and
    /// also when the daemon auto-creates a session for a workspace-
    /// scoped Spawn. Sidebar uses this to expand the workspace row
    /// into session sub-rows once the count crosses 1.
    SessionCreated(Box<lazybox_core::WorkspaceSession>),
    /// Progress signal during first-time worktree provisioning (cold
    /// clone / fetch / `git worktree add` / mounts / scripts). Emitted
    /// only on the provisioning path — an instant resume of an existing
    /// worktree sends none, so the TUI's progress modal never flashes
    /// for the fast path. `session_key` ties the events to the spawn
    /// the user just triggered; the matching `TerminalSpawned` *queues*
    /// the dismiss rather than tearing the modal down on the spot, so a
    /// fast provision still walks every step before the modal closes.
    WorktreeProgress {
        session_key: SessionKey,
        step: WorktreeStep,
        status: WorktreeStepStatus,
        /// Whether the local user asked for this spawn or the daemon
        /// started it autonomously — routes the client between the
        /// progress modal and a quiet footer notice (issue #645).
        #[serde(default)]
        origin: SpawnOrigin,
    },
    /// A session ended (process exited and the worktree was reaped,
    /// OR the user explicitly killed it). Includes the workspace
    /// key so consumers can look up which row to update without
    /// parsing the session id.
    SessionEnded {
        workspace_key: lazybox_core::WorkspaceKey,
        session_id: lazybox_core::SessionId,
    },
    TerminalSpawned {
        terminal_id: TerminalId,
        session_key: SessionKey,
        kind: TerminalKind,
        /// Launched in no-permission / bypass mode (autonomous
        /// session running unattended). Drives the session UI's
        /// "no-perms" indicator. Always `false` for interactive and
        /// shell terminals.
        #[serde(default)]
        no_permission: bool,
        /// Running on the repo's shared main checkout rather than an
        /// isolated worktree. Drives the terminal's "main" badge so
        /// it's obvious the session sits on the shared branch.
        #[serde(default)]
        on_main: bool,
        /// Display label of the model tier the session was launched
        /// with (`"Haiku"`, `"Sonnet"`, `"Opus"`), when the user picked
        /// one via a tier chord. Drives the terminal tab's tier badge.
        /// `None` for a default-model / shell spawn (no badge).
        #[serde(default)]
        model_label: Option<String>,
    },
    TerminalOutput {
        terminal_id: TerminalId,
        bytes: Vec<u8>,
        /// First monotonic per-terminal chunk sequence represented by
        /// `bytes`. Normally equal to `seq`; the TUI drain may coalesce a
        /// contiguous run and preserve this lower bound.
        first_seq: u64,
        /// Last monotonic per-terminal chunk sequence represented by
        /// `bytes`. Together with `first_seq`, lets the consumer detect
        /// gaps even after adjacent chunks are coalesced.
        seq: u64,
    },
    /// Re-establish a terminal's grid from the daemon-side replay ring
    /// after the bounded event channel dropped one or more
    /// `TerminalOutput` chunks for it (see [`channel`] / the server's
    /// per-connection forwarder). Dropping raw bytes mid-stream
    /// corrupts the libghostty-vt parser, so the forwarder never lets a
    /// partial stream through: it drops the coalescable output, then
    /// emits exactly one `TerminalResync` carrying the full ring. The
    /// consumer resets its parser and re-feeds `replay`, which
    /// reconstructs the correct screen without the dropped bytes. `seq`
    /// is the ring's last sequence — the consumer adopts it so the
    /// resumed live stream (all `seq` strictly greater) applies exactly
    /// once.
    TerminalResync {
        terminal_id: TerminalId,
        replay: Vec<u8>,
        seq: u64,
    },
    /// Recovery could not currently produce a complete replay covering
    /// the observed gap. The consumer must preserve its last coherent
    /// grid, discard live output, and retry later—never clear the parser
    /// or treat this as sequence coverage.
    TerminalResyncUnavailable {
        terminal_id: TerminalId,
    },
    TerminalExited {
        terminal_id: TerminalId,
        exit_code: Option<i32>,
        /// Cleaned tail of an *agent* terminal's last PTY output, so the
        /// frozen "exited" pane can show *why* it died instead of a blank
        /// screen (issue #368). `None` for shells, forced removals,
        /// recovered sessions, or when the agent produced no output. The
        /// client decides whether to surface it (a dead-on-arrival pane
        /// paints it; a normal crash keeps its own last screen).
        last_output: Option<String>,
    },
    /// Daemon-driven "focus this existing terminal instead of
    /// spawning a duplicate". Fired by the singleton guard in
    /// `handle_spawn` when a `Spawn { Agent(id) }` lands and a
    /// matching agent is already running. The TUI moves the active
    /// tab + focuses the terminal stack.
    TerminalFocusRequested {
        terminal_id: TerminalId,
    },
    /// A one-shot request from a clicked desktop notification. Each
    /// attached TUI resolves the key against its current sidebar and
    /// follows the same workspace-jump path as the fuzzy picker.
    WorkspaceFocusRequested {
        session_key: SessionKey,
    },
    /// Every terminal keyed to `from` now belongs to `to`. Broadcast
    /// when the daemon rebadges terminals during an issue→PR collapse
    /// (or a manual adopt) so live TUI clients re-point their terminal
    /// slots BEFORE the `WorkspaceRemoved` for `from` arrives — without
    /// this the terminal stack drops the moved terminals on removal and
    /// the session vanishes from view until the next daemon restart.
    TerminalsRebadged {
        from: SessionKey,
        to: SessionKey,
    },
    AgentState {
        /// Workspace the asking agent lives in. Kept for backwards
        /// compatibility with TUI consumers that index by workspace
        /// (sidebar "needs input" badge); the per-terminal id below is
        /// what new consumers (e.g. the chat dispatcher's
        /// channel-per-(session, agent) routing) should key off.
        session_key: SessionKey,
        /// Which terminal flipped state. A workspace with two agents
        /// running (Claude + Codex) emits two distinct `AgentState`
        /// events; previously the wire carried only `session_key` and
        /// consumers couldn't tell them apart. The `#[serde(default)]`
        /// (→ `TerminalId(0)`) only applies on the JSON gateway, where
        /// an older producer can omit the field; over the socket,
        /// bincode never applies defaults — adding this field was a
        /// wire-format change guarded by the wire-fingerprint
        /// handshake, and mismatched peers are rejected at connect,
        /// not papered over.
        #[serde(default)]
        terminal_id: TerminalId,
        state: AgentState,
    },
    ProviderError {
        source: String,
        /// Terse one-line summary for the status bar.
        message: String,
        /// Full diagnostic for the error modal / dev tools. Empty
        /// if the producer didn't classify the error (legacy path).
        #[serde(default)]
        detail: String,
        /// Severity. See [`ProviderErrorKind`] — the daemon writes
        /// `kind.as_str()` here. Empty (uncategorized) is treated
        /// as `Permanent` for safety.
        #[serde(default)]
        kind: String,
    },
    /// GitHub polling is intentionally paused until its observed
    /// rate-limit window resets. This is a wait state, not a failed
    /// or in-flight sync: clients should stop any poll spinner and
    /// show the budget/reset details.
    GithubRateLimitWait {
        remaining: u32,
        limit: u32,
        reset_at: chrono::DateTime<chrono::Utc>,
    },
    /// Emitted at the end of every successful poll cycle, even when
    /// no tasks matched. The TUI uses this to distinguish "polling
    /// hasn't run yet" from "polling ran and found nothing matching
    /// the user's filter." Drives the polling-modal's empty-state
    /// dismiss path.
    PollCompleted {
        source: String,
        /// Number of tasks the source's filter kept post-fetch.
        count: usize,
    },
    /// Granular progress signal during a poll cycle. Drives the
    /// polling modal's "what is lazybox doing right now" indicator
    /// (e.g. "Querying PRs in acme/widget…", "Got 5 PRs,
    /// fetching reviews…"). Also great for debugging — every
    /// progress step shows up in the log.
    PollProgress {
        source: String,
        /// Short, user-facing description of the current step.
        message: String,
    },
    Notification {
        title: String,
        body: String,
    },
    /// `Command::CleanWorktrees` finished. `removed` is the number of
    /// worktrees actually torn down (skipping ones with live
    /// terminals); `skipped` is the count of sessions that were left
    /// alone because their terminal was still attached. The TUI uses
    /// this to surface a `cleaned N worktrees · M kept (active)`
    /// footer notice.
    CleanWorktreesCompleted {
        removed: usize,
        skipped: usize,
    },
    /// `Command::InspectWorktrees` finished. `inspections` is the
    /// full report — both flagged orphans and healthy entries — in
    /// path-sorted order. Drives the in-app inspector modal.
    WorktreesInspected {
        inspections: Vec<WorktreeInspectionDto>,
    },
    /// `Command::InspectWorkspaceDiff` finished. `diff` is absent when
    /// the workspace/target disappeared or git could not read it.
    WorkspaceDiffInspected {
        workspace_key: lazybox_core::WorkspaceKey,
        target: WorkspaceDiffTarget,
        /// Live agent terminals rooted in the inspected checkout.
        agent_terminal_ids: Vec<TerminalId>,
        diff: Option<WorkspaceDiffDto>,
        error: Option<String>,
    },
    /// `Command::ScanCheckouts` finished. `checkouts` is every on-disk
    /// git clone found under the dev roots, mapped to its GitHub repo
    /// where possible, in path-sorted order. Drives the import picker.
    /// Empty when nothing was found (or the roots were unset).
    CheckoutsDiscovered {
        checkouts: Vec<DiscoveredCheckoutDto>,
    },
    /// `Command::DeleteOrphanedWorktree` finished. `ok = false` means
    /// the daemon refused (safety gate) or the underlying `git
    /// worktree remove` failed; `error` carries the human-readable
    /// reason in that case.
    OrphanedWorktreeDeleted {
        path: std::path::PathBuf,
        ok: bool,
        #[serde(default)]
        error: Option<String>,
    },
    AgentRunStarted {
        request_id: AgentRunRequestId,
        run_id: AgentRunId,
        session_key: SessionKey,
        #[serde(default)]
        session_id: Option<lazybox_core::SessionId>,
        agent: String,
        mode: AgentRuntimeMode,
    },
    AgentRunStartFailed {
        request_id: AgentRunRequestId,
        message: String,
    },
    /// Lossless raw provider JSONL object text from the runtime.
    AgentRawJson {
        run_id: AgentRunId,
        json: String,
    },
    AgentDebug {
        run_id: AgentRunId,
        message: String,
    },
    AgentAssistantTextDelta {
        run_id: AgentRunId,
        delta: String,
    },
    AgentToolCallStarted {
        run_id: AgentRunId,
        call_id: String,
        name: String,
        input_json: Option<String>,
    },
    AgentToolCallDelta {
        run_id: AgentRunId,
        call_id: String,
        delta_json: String,
    },
    AgentToolCallFinished {
        run_id: AgentRunId,
        call_id: String,
        output_json: Option<String>,
        error: Option<String>,
    },
    AgentPermissionRequest {
        run_id: AgentRunId,
        request_id: String,
        tool_name: String,
        input_json: Option<String>,
        reason: Option<String>,
    },
    AgentUserQuestion {
        run_id: AgentRunId,
        question_id: String,
        prompt: String,
        choices: Vec<String>,
        allow_freeform: bool,
    },
    AgentUsage {
        run_id: AgentRunId,
        usage: AgentUsage,
    },
    AgentTurnFinished {
        run_id: AgentRunId,
        result: Option<String>,
        session_id: Option<String>,
        error: Option<String>,
    },
    AgentRunFinished {
        run_id: AgentRunId,
        exit_code: Option<i32>,
        error: Option<String>,
    },
    ProviderCredentialUpdated {
        principal_id: PrincipalId,
        provider_id: String,
        metadata: ProviderCredentialMetadata,
    },
    ProviderCredentialRemoved {
        principal_id: PrincipalId,
        provider_id: String,
    },
    ProviderCredentialsListed {
        principal_id: PrincipalId,
        credentials: Vec<ProviderCredentialMetadata>,
    },
    /// Terminal-local input failed or was deliberately rejected before the
    /// daemon could prove delivery. This is not a provider/sync failure:
    /// clients surface it as a retryable terminal notice without poisoning
    /// provider polling state or sync history.
    TerminalInputRejected {
        terminal_id: TerminalId,
        message: String,
    },
    /// A daemon queue reached its explicit admission limit before accepting
    /// this command. The command was not executed; retry is safe after the
    /// named subsystem catches up.
    CommandRejected {
        command: String,
        message: String,
    },
    /// Reply to [`Command::FetchScrollback`]: the terminal's history as
    /// the backend retains it (tmux `capture-pane -e -J -S -<limit>`,
    /// normalized like the restart-recovery seed). Unlike
    /// [`Event::TerminalResync`] — whose `replay` is the raw ring
    /// stream and therefore carries the inner program's escape
    /// sequences — this payload is content-only: the consumer re-feeds
    /// it for DEEP scrollback but must preserve terminal modes (mouse
    /// tracking, DECCKM, …) across the rebuild itself, because a
    /// capture never re-asserts them. `seq` is the ring's high-water
    /// mark at capture time; live chunks at or below it are covered by
    /// the capture.
    ///
    /// Appended last (bincode ordinal compatibility, see
    /// [`PROTOCOL_FINGERPRINT`]).
    TerminalScrollback {
        terminal_id: TerminalId,
        replay: Vec<u8>,
        seq: u64,
    },
    /// Result of an out-of-band agent-CLI version check (scheduled, or
    /// `Command::CheckAgentCliUpdates`). One status per enabled agent
    /// that advertises an update channel. `manual` distinguishes a
    /// user-triggered check — always worth a footer summary — from the
    /// scheduled sweep, which clients only surface when something is
    /// available or failed. Appended last — after `TerminalScrollback`,
    /// which shipped at protocol 12; see `KeepMergedWorkspace`.
    AgentCliUpdatesChecked {
        statuses: Vec<AgentCliUpdateStatus>,
        manual: bool,
    },
    /// One agent's lazybox-managed CLI update finished. `message`
    /// carries the actionable failure detail when `ok` is false.
    AgentCliUpdateFinished {
        agent_id: String,
        display_name: String,
        ok: bool,
        installed_before: Option<String>,
        installed_after: Option<String>,
        message: String,
    },
    /// Recovered interactive agents whose persisted PTY launch contract
    /// predates the running daemon's requirement. The ids are guaranteed to
    /// be a subset of the [`Event::Snapshot`] sent by the same Subscribe
    /// response, so a client never warns about a terminal it was not given.
    /// The processes remain attached; clients ask the user to close and reopen
    /// them.
    ///
    /// Appended last for bincode ordinal compatibility; see
    /// [`PROTOCOL_FINGERPRINT`].
    RecoveredTerminalsRequireRestart {
        terminal_ids: Vec<TerminalId>,
    },
    /// The PR branch for `workspace_key` was just updated against its
    /// base via `Command::UpdateBranch`. The local Task still reflects
    /// the stale `mergeStateStatus` until the next poll, so the TUI
    /// flashes a footer notice so the keypress doesn't look like a no-op.
    /// Appended last; see `AgentCliUpdateFinished`.
    BranchUpdated {
        workspace_key: lazybox_core::WorkspaceKey,
        pr_label: String,
    },
    /// `Command::UpdateBranch` failed at the GitHub API — the update did
    /// NOT happen. Surfaced as a prominent, persistent error naming the
    /// reason (conflict, permissions), mirroring `PrMergeFailed`. The PR
    /// stays actionable. Appended last.
    BranchUpdateFailed {
        workspace_key: lazybox_core::WorkspaceKey,
        pr_label: String,
        reason: String,
    },
    /// A [`Command::DeliverSnippet`] reached the terminal and its durable
    /// histories were updated. `prompt` is present for agent terminals,
    /// whose recap/history display submitted prompts; shell deliveries
    /// have no prompt history. Appended last.
    SnippetDelivered {
        terminal_id: TerminalId,
        session_key: SessionKey,
        snippet_key: String,
        prompt: Option<UserPrompt>,
    },
    /// A client-correlated command reached its durable success boundary.
    /// Appended last for bincode ordinal compatibility.
    CommandCompleted {
        client_request_id: String,
    },
    /// A client-correlated command failed before reaching its success
    /// boundary. The command had no further effect after this event.
    /// Appended last for bincode ordinal compatibility.
    CommandFailed {
        client_request_id: String,
        message: String,
    },
    /// A built-in agent adapter confidently classified its recent PTY
    /// output as a provider-account authentication failure.
    AgentAuthRequired {
        terminal_id: TerminalId,
        agent_id: String,
        display_name: String,
        reason: String,
        other_session_count: usize,
        /// The agent isolates its login per session (Codex → a private
        /// `CODEX_HOME`), so re-auth rewrites only this session's
        /// credential and leaves the rest of the fleet on their own
        /// logins — no machine-wide cascade to warn about.
        credentials_isolated: bool,
    },
    /// Non-secret orchestration progress. Provider PTY bytes continue over
    /// the terminal stream and are never copied into this message.
    AgentAuthProgress {
        recovery_terminal_id: TerminalId,
        terminal_id: TerminalId,
        phase: AgentAuthPhase,
    },
    /// Terminal outcome for the recovery attempt. `error` is a bounded,
    /// scrubbed status assembled from process exit state, never provider
    /// output.
    AgentAuthFinished {
        recovery_terminal_id: TerminalId,
        terminal_id: TerminalId,
        display_name: String,
        success: bool,
        error: Option<String>,
    },
    /// The provider has no exact session id for this resume and will use its
    /// cwd-based fallback in a shared checkout.
    AgentResumeFallback {
        terminal_id: TerminalId,
        display_name: String,
    },
    /// A daemon-owned terminal replaced a recoverable pane. Unlike matching
    /// by workspace/agent, this relation remains unambiguous when isolated and
    /// on-main terminals for the same agent coexist.
    TerminalReplaced {
        old_terminal_id: TerminalId,
        terminal_id: TerminalId,
        session_key: SessionKey,
        kind: TerminalKind,
        no_permission: bool,
        on_main: bool,
        model_label: Option<String>,
        authenticating: bool,
    },
    /// Provider-owned authentication PTY bytes sent only to the connection
    /// currently attached to the recovery flow. They never enter the daemon's
    /// process-wide broadcast bus.
    AgentAuthOutput {
        terminal_id: TerminalId,
        bytes: Vec<u8>,
        first_seq: u64,
        seq: u64,
    },
    /// Connection-private authoritative replay for an authentication PTY,
    /// used when ownership moves to a reconnecting client.
    AgentAuthReplay {
        terminal_id: TerminalId,
        replay: Vec<u8>,
        seq: u64,
    },
    /// The model + reasoning effort a live agent terminal is running has
    /// changed. Sourced from the daemon's PTY detection (Codex prints
    /// `<model> <effort>` in its composer footer), so it stays accurate
    /// when the user switches model *inside* the agent — the spawn-time
    /// tier label can't. `model_label` is the same compact display string
    /// (`"gpt-5.5 · xhigh"`) the spawn tier uses, so the sidebar model
    /// badge and the terminal tab badge share one field. Broadcast only on
    /// a change, and folded back into the reconnect snapshot's
    /// `model_label` so a re-subscribing client doesn't lose it.
    TerminalModelChanged {
        session_key: SessionKey,
        terminal_id: TerminalId,
        model_label: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub enum AgentAuthPhase {
    LoggingOut,
    LoginInteractive,
    Resuming,
}

/// Installed-vs-latest reading for one agent CLI, produced by the
/// daemon's out-of-band update check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct AgentCliUpdateStatus {
    pub agent_id: String,
    pub display_name: String,
    /// Parsed installed version, `None` when the CLI is missing or its
    /// version command failed (see `error`).
    pub installed: Option<String>,
    /// Latest released version, `None` when the agent has no queryable
    /// registry or the lookup failed.
    pub latest: Option<String>,
    /// `latest` is known and strictly newer than `installed`.
    pub update_available: bool,
    /// Human-readable failure detail from either probe.
    pub error: Option<String>,
    /// The user opted this agent into `agents.<id>.auto_update`, so a
    /// scheduled sweep applies an available update itself — clients
    /// word the availability notice as "auto-updating" instead of
    /// telling the user to trigger it manually. The `#[serde(default)]`
    /// only matters on the JSON gateway; over the socket this field
    /// rides the same wire-fingerprint shift as the event itself.
    #[serde(default)]
    pub auto_update: bool,
}

/// Severity classification for `Event::ProviderError`. The TUI uses
/// this to decide whether to auto-mount an error modal or just flash
/// a footer notice. Stored on the wire as the plain string returned
/// by `as_str` so existing TUI matches keep working.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderErrorKind {
    /// Network blip, rate limit, transient API failure that the daemon
    /// is auto-retrying. Self-healing, so the TUI keeps it quiet — a
    /// non-alarming sync status, never a red banner (#730).
    Retryable,
    /// A retryable transient whose retries are exhausted: the daemon
    /// kept failing across successive cycles, so sync is genuinely
    /// stuck and now actionable. The terse message names the failure
    /// class (a stuck sync) and never blames the token; the precise
    /// cause (a 502, a dropped connection) rides the event's diagnostic
    /// field. A throttle never reaches this state — it stays a quiet
    /// [`Retryable`](Self::Retryable) backoff (#772). The TUI surfaces
    /// this as a real error (#730).
    Exhausted,
    /// Credentials failed to resolve / unauthorized. The TUI walks
    /// the user through re-auth.
    Auth,
    /// Genuine misconfiguration or upstream invariant violation.
    /// Surfaces an error modal.
    Permanent,
}

impl ProviderErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Retryable => "retryable",
            Self::Exhausted => "exhausted",
            Self::Auth => "auth",
            Self::Permanent => "permanent",
        }
    }

    /// Parse the wire `kind` string back into the classification.
    /// Inverse of [`as_str`](Self::as_str) — the one place these strings
    /// are matched, so consumers reason over the enum instead of raw
    /// literals. An empty (uncategorized/legacy) or unrecognized value
    /// maps to [`Permanent`](Self::Permanent) — the safe default, since
    /// an unknown failure is surfaced rather than silently swallowed.
    pub fn from_wire(kind: &str) -> Self {
        match kind {
            "retryable" => Self::Retryable,
            "exhausted" => Self::Exhausted,
            "auth" => Self::Auth,
            _ => Self::Permanent,
        }
    }

    /// Whether this failure needs the user to act on it *now* — the flag
    /// that decides the footer surface (#730). A [`Retryable`](Self::Retryable)
    /// transient the daemon is still auto-retrying is self-healing and
    /// stays a quiet status; everything else (retries [`Exhausted`](Self::Exhausted),
    /// [`Auth`](Self::Auth), [`Permanent`](Self::Permanent)) is actionable
    /// and earns a real error surface.
    pub fn is_actionable(self) -> bool {
        !matches!(self, Self::Retryable)
    }
}

/// A single phase of first-time worktree provisioning, reported by
/// `Event::WorktreeProgress`. Ordered as the daemon runs them so the
/// TUI's progress checklist can render them top-to-bottom without
/// carrying ordering on the wire.
///
/// `Clone`/`Fetch`/`WorktreeAdd` are the sub-phases of what used to be a
/// single opaque `Checkout` step — split so the long cold-clone phase
/// animates with advancing sub-progress instead of one spinner that
/// jumps straight to done.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub enum WorktreeStep {
    /// The one-time bare clone (a blobless partial fetch) — the slow
    /// part on a brand-new repo. Skipped (instant) when a healthy bare
    /// clone is already cached.
    Clone,
    /// Refreshing the remote-tracking ref before branching off it.
    Fetch,
    /// `git worktree add` materializing the checkout on disk. From a
    /// blobless clone this includes downloading the checked-out files,
    /// making it the bulk of a cold provision's wall-clock.
    WorktreeAdd,
    /// Applying configured mounts + setup scripts to the fresh tree.
    Setup,
}

/// Who triggered the spawn a `WorktreeProgress` stream belongs to.
/// Drives how the client surfaces provisioning: an
/// [`Interactive`](Self::Interactive) spawn is one the local user just
/// asked for (a `w` / `a` / `x …` chord), so its checklist mounts as a
/// modal they're waiting on; an [`Autonomous`](Self::Autonomous) spawn
/// is background work the daemon started (a GitHub label, an `@lazybox`
/// mention, an auto-fix, or session recovery), so it reports as a
/// footer notice instead of stealing focus with a modal (issue #645).
/// The [`AutonomousTrigger`] it carries names *which* background source
/// fired, so the notice can read `(@lazybox)` / `(label)` / … A genuine
/// *failure* still surfaces the modal regardless of origin, since it
/// needs a decision.
///
/// This is orthogonal to `handle_spawn`'s `autonomous` flag, which
/// controls unattended (no-permission) launch mode — a user `w` "work
/// on this" runs unattended yet is `Interactive` here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub enum SpawnOrigin {
    /// The local user issued the spawn. Provisioning mounts the
    /// progress modal (today's behavior).
    #[default]
    Interactive,
    /// The daemon auto-spawned this as background work. Provisioning
    /// reports a footer notice tagged with the trigger, no modal.
    Autonomous(AutonomousTrigger),
}

/// Which autonomous source started a spawn — names the parenthetical
/// tag on the "starting agent…" footer notice (issue #645).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub enum AutonomousTrigger {
    /// An `@lazybox` mention in an issue/PR body or comment.
    Mention,
    /// A GitHub label whose presence maps to an auto-spawn.
    Label,
    /// The auto-fix scan acting on a CI-failing / conflicting PR.
    AutoFix,
    /// Startup session recovery re-provisioning a missing worktree.
    Restore,
}

impl AutonomousTrigger {
    /// Parenthetical tag for the footer notice — e.g. `@lazybox` renders
    /// as "starting agent on owner/repo#7 (@lazybox)".
    pub fn notice_tag(self) -> &'static str {
        match self {
            Self::Mention => "@lazybox",
            Self::Label => "label",
            Self::AutoFix => "auto-fix",
            Self::Restore => "restored",
        }
    }
}

/// State transition for a [`WorktreeStep`]. `Started`/`Done` advance
/// the checklist; `Failed` carries the error so the modal can surface
/// it instead of dismissing silently. `Warned` marks a step that
/// completed in a degraded way (e.g. the base-ref fetch failed and the
/// worktree branched off a possibly-stale local ref) — the step still
/// counts as done, but the modal surfaces the note instead of hiding it
/// in the log (issue #320). `Progress` is a live detail line for a step
/// already `Started` — e.g. the clone transfer's `Receiving objects:
/// 42% …` — updating the row's detail text without advancing the
/// checklist (issue #405).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub enum WorktreeStepStatus {
    Started,
    Done,
    Warned(String),
    Failed(String),
    Progress(String),
}

/// The `Failed` message the daemon broadcasts when a provision is
/// aborted by [`Command::CancelSpawn`]. `Failed` (not `Warned`) so any
/// client's checklist stops and its Esc-dismissal marker releases —
/// but clients match on this exact string to frame the notice as a
/// confirmation of the user's own cancel rather than an error.
pub const SPAWN_CANCELLED_NOTE: &str = "workspace setup cancelled";

/// How a worktree-provisioning failure can be recovered — derived
/// purely from the daemon's error text via [`Self::classify`], so the
/// wire stays a plain `Failed(String)` and every surface (the daemon
/// picking the right checklist row, the modal rendering an affordance)
/// reads the *same* classification instead of re-matching strings
/// independently.
///
/// Issue #557's audit found that every `Failed` variant dead-ended to
/// the identical `Esc dismiss` footer regardless of what actually
/// broke. This enum is that audit's failure-mode matrix made
/// machine-readable: one variant per recoverable class (Bn refs are the
/// issue's rows), each carrying a one-line [`Self::hint`] telling the
/// user what to do and whether re-pressing (`r`) can help
/// ([`Self::retryable`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeRecovery {
    /// Transient — a fetch/network hiccup or a ref lock. Re-pressing may
    /// just work once the blip clears. The safe default when nothing
    /// more specific matches but the text smells like network.
    Transient,
    /// B1: the branch is checked out in another *live* worktree; git
    /// refuses to co-opt it. Safe managed holders without a live owner are
    /// reclaimed by the daemon before this reaches the client, so the
    /// remaining cases require joining a real session or freeing an
    /// external checkout.
    BranchHeldLive,
    /// B1 managed-holder edge: the checkout has no live owner but holds
    /// local state, is locked, or could not be verified safely, so automatic
    /// reclaim deliberately stopped.
    BranchHeldManaged,
    /// A completed worktree exists at the intended session path but is on
    /// another branch. Reusing it would silently put the session on the
    /// wrong work; switching it automatically could discard local work.
    BranchMismatch,
    /// B3: a leftover directory holds real uncommitted work, so the
    /// provisioner refuses to reuse or overwrite it. Manual `mv` aside.
    DirtyLeftover,
    /// B5/B6: the head/base branch isn't on `origin` (fork PR, deleted
    /// head, or a base that no longer exists) and no PR-ref fallback
    /// resolved it. Re-syncing the PR/repo is the fix, not a bare retry.
    BranchMissing,
    /// B9: a blobless clone can't download file contents because the
    /// remote is unreachable. Reconnect, then retry.
    Offline,
    /// B10: the repo's default branch couldn't be resolved (no
    /// `origin/HEAD`, an unusual default, or offline).
    DefaultBranchUnresolved,
    /// B11: the repo string isn't `owner/name` — a config/source problem
    /// upstream of git that a retry can't fix.
    BadRepo,
    /// B14/B17: disk, permission, or generic I/O error (out of space,
    /// read-only mount, `git init` half-write). Retryable once the
    /// filesystem problem is cleared.
    Disk,
    /// Unclassified — surface the raw text and still offer a retry, since
    /// a failed provision persists no session and re-pressing is clean.
    Unknown,
}

impl WorktreeRecovery {
    /// Classify a provisioning error from its (already user-facing)
    /// message. Order matters: the most specific markers are matched
    /// first so a message that happens to contain a generic word
    /// (`origin`, `remote`) still lands on its precise class. Every
    /// marker below is an exact substring the daemon emits — see
    /// `crates/git-ops/src/lib.rs` and `spawn_handler.rs`.
    pub fn classify(message: &str) -> Self {
        if message.contains("automatic reclaim blocked") {
            return Self::BranchHeldManaged;
        }
        if message.contains("not the requested branch") {
            return Self::BranchMismatch;
        }
        // B1 — the collision refusal names the holding worktree.
        if message.contains("another live worktree") {
            return Self::BranchHeldLive;
        }
        // B3 — the only message that tells the user to move a dir aside.
        if message.contains("move the directory aside") {
            return Self::DirtyLeftover;
        }
        // B11 — malformed repo string, caught before any git runs.
        if message.contains("is not owner/name") {
            return Self::BadRepo;
        }
        // B10 — default-branch resolution gave up.
        if message.contains("could not resolve default branch") {
            return Self::DefaultBranchUnresolved;
        }
        // B9 — blobless clone needs the network for blobs.
        if message.contains("could not download file contents") {
            return Self::Offline;
        }
        // B5/B6 — head or base ref absent locally and on origin.
        if message.contains("not found locally or on origin") {
            return Self::BranchMissing;
        }
        // B14/B17 — filesystem-class failures. `I/O error:` is
        // `GitError::Io`'s Display prefix; the rest are OS strings that
        // survive into the wrapped message.
        if message.contains("I/O error")
            || message.contains("No space left")
            || message.contains("Permission denied")
            || message.contains("Read-only file system")
        {
            return Self::Disk;
        }
        // Network transients that a retry can clear on its own.
        if message.contains("could not read from remote")
            || message.contains("Could not resolve host")
            || message.contains("Connection refused")
            || message.contains("Connection reset")
            || message.contains("Temporary failure in name resolution")
            || message.contains("timed out")
        {
            return Self::Transient;
        }
        Self::Unknown
    }

    /// The checklist row a failure of this class belongs to, so the ✗
    /// lands on the phase that actually aborted instead of always
    /// "Cloning" (issue #557, acceptance #2). Pre-git failures
    /// ([`Self::BadRepo`]) and the ref-resolution/fetch phase point at
    /// the "Preparing worktree" row; the collision, dirty-dir, offline
    /// blob-download, and disk classes surface on the `git worktree add`
    /// row where they're raised.
    pub fn failed_step(&self) -> WorktreeStep {
        match self {
            Self::BadRepo
            | Self::DefaultBranchUnresolved
            | Self::BranchMissing
            | Self::Transient => WorktreeStep::Fetch,
            Self::BranchHeldLive
            | Self::BranchHeldManaged
            | Self::BranchMismatch
            | Self::DirtyLeftover
            | Self::Offline
            | Self::Disk => WorktreeStep::WorktreeAdd,
            // Unknown: keep it on whatever row the display is showing —
            // the modal's `fail_current` treats `Fetch` as "use the
            // current frontier", so an unclassified failure doesn't jump.
            Self::Unknown => WorktreeStep::Fetch,
        }
    }

    /// Whether re-pressing (`r` in the modal / a fresh `w`) can succeed
    /// *without* the user first doing something out-of-band. `false` for
    /// the classes that need a manual fix (free the holder, move a dir,
    /// re-sync the branch, fix the repo string).
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Self::Transient
                | Self::Offline
                | Self::DefaultBranchUnresolved
                | Self::Disk
                | Self::Unknown
        )
    }

    /// One-line, imperative recovery guidance shown under the error in
    /// the modal. Always names a concrete next step; the modal separately
    /// renders only the affordances valid for this class.
    pub fn hint(&self) -> &'static str {
        match self {
            Self::Transient => "Looks transient — press r to retry.",
            Self::BranchHeldLive => {
                "Press Esc; join the live session holding this branch, or free the \
                 external checkout."
            }
            Self::BranchHeldManaged => {
                "The managed holder has local state, is locked, or could not be verified. \
                 Preserve or remove it, then start again."
            }
            Self::BranchMismatch => {
                "This worktree is on another branch. Preserve or switch it, then start again."
            }
            Self::DirtyLeftover => {
                "A leftover folder holds uncommitted work. Move it aside, then start again."
            }
            Self::BranchMissing => {
                "The branch isn't on origin (fork or deleted head). Re-sync the PR \
                 (g s), then start again."
            }
            Self::Offline => "Can't download files while offline. Reconnect, then press r.",
            Self::DefaultBranchUnresolved => {
                "Couldn't find the repo's default branch. Check connectivity or the \
                 repo, then press r."
            }
            Self::BadRepo => {
                "This workspace's repo isn't in owner/name form — fix its source \
                 config; a retry won't help."
            }
            Self::Disk => "Disk or permission error. Free space or fix permissions, then press r.",
            Self::Unknown => "Press r to retry, or Esc to dismiss.",
        }
    }
}

impl Event {
    /// Build a `ProviderError` event with the given source / message
    /// / kind. Replaces ~28 hand-rolled struct literals that all
    /// carried the same `detail: String::new()` shape. Use the
    /// kind-specific shortcuts below when classification is clear at
    /// the call site.
    pub fn provider_error(
        source: &str,
        message: impl Into<String>,
        kind: ProviderErrorKind,
    ) -> Self {
        Self::ProviderError {
            source: source.to_string(),
            message: message.into(),
            detail: String::new(),
            kind: kind.as_str().to_string(),
        }
    }

    /// Shorthand for a retryable provider error. Use for the
    /// transient cases — network, rate limit, "GitHub said please
    /// try again." The TUI doesn't escalate these to a modal.
    pub fn provider_error_retryable(source: &str, message: impl Into<String>) -> Self {
        Self::provider_error(source, message, ProviderErrorKind::Retryable)
    }

    /// Shorthand for a permanent provider error. Use for the genuine
    /// "this won't fix itself by waiting" cases — bad config, missing
    /// repo, malformed task. The TUI auto-mounts a modal.
    pub fn provider_error_permanent(source: &str, message: impl Into<String>) -> Self {
        Self::provider_error(source, message, ProviderErrorKind::Permanent)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct TerminalSnapshot {
    pub terminal_id: TerminalId,
    pub session_key: SessionKey,
    pub kind: TerminalKind,
    /// Recent PTY output (daemon-side ring buffer). The client feeds
    /// this into its libghostty-vt to reconstruct the screen.
    pub replay: Vec<u8>,
    pub last_seq: u64,
    /// Whether `replay` is a VT-safe reset seed the client can feed into a
    /// ground-state parser to authoritatively initialize the screen. True for
    /// both a complete ring and a wrapped one: the daemon's `replay_snapshot`
    /// starts on a clean line boundary even after the ring overwrites its
    /// prefix (`ReplayRing::replay_snapshot_into`), so a wrapped ring is a
    /// correct, if shorter-history, reset. False ONLY when there is no
    /// authoritative replay to adopt — a backend snapshot failure/timeout, or
    /// a replay dropped by the snapshot byte budget; the client then preserves
    /// its last coherent screen and requests a resync.
    #[serde(default)]
    pub replay_available: bool,
    /// Launched in no-permission / bypass mode. Lets a reconnecting
    /// client re-render the "no-perms" indicator without waiting for
    /// a fresh `TerminalSpawned`.
    #[serde(default)]
    pub no_permission: bool,
    /// Running on the repo's shared main checkout. Lets a reconnecting
    /// client re-render the "main" badge without a fresh
    /// `TerminalSpawned`.
    #[serde(default)]
    pub on_main: bool,
    /// Model tier label the session was launched with. Lets a
    /// reconnecting client re-render the tier badge without a fresh
    /// `TerminalSpawned`. `None` for default-model / shell terminals.
    #[serde(default)]
    pub model_label: Option<String>,
    /// Bounded per-session history of prompts the user submitted to this
    /// terminal, oldest-first (Agent-only; empty for shells and for
    /// agents that haven't received a prompt yet). Persisted daemon-side
    /// from `Command::RecordUserMessage` (issue #523) so a reconnecting
    /// client can restore both the pinned "you ▸ …" recap (the last
    /// entry) and the `]]h` history view — the ring-buffer `replay` only
    /// carries PTY *output*, never the input we composed, so neither can
    /// be reconstructed from it.
    #[serde(default)]
    pub prompt_history: Vec<UserPrompt>,
    /// In-flight composer buffer (typed but not yet submitted) for this
    /// agent terminal, persisted daemon-side from
    /// `Command::RecordComposingBuffer`. Restored into the client's
    /// composing state so a restart can recall the half-typed prompt
    /// via `]]r`. `None` for shells and agents with no pending draft.
    #[serde(default)]
    pub composing_buffer: Option<String>,
    /// Authoritative lifecycle state for an agent terminal at the same
    /// baseline as this replay. `None` for shells and for a fresh agent that
    /// has not committed its first state yet.
    #[serde(default)]
    pub agent_state: Option<AgentState>,
    /// This terminal is the provider-owned interactive login process
    /// for a recoverable agent conversation.
    #[serde(default)]
    pub authenticating: bool,
}

// ── Transport abstraction ──────────────────────────────────────────────

use tokio::sync::{mpsc, watch};

/// Capacity of the bounded daemon→client event channel. This is the
/// hard memory ceiling on inbound events: the channel never holds more
/// than this many `Event`s no matter how fast the daemon produces
/// `TerminalOutput`. When it fills, the per-connection forwarder drops
/// the coalescable output for the affected terminal and re-syncs its
/// grid from the daemon ring (see [`Event::TerminalResync`]).
///
/// Sized well above the run loop's per-tick drain cap
/// (`MAX_EVENTS_PER_TICK` = 256) so ordinary single-frame bursts ride
/// through without ever tripping the drop path — overflow only kicks in
/// when the consumer is genuinely, sustainedly behind.
pub const EVENT_CHANNEL_CAPACITY: usize = 512;

/// Hard ceiling on each connection's daemon-side staging queue before the
/// drop/resync forwarder. This queue exists so the serve loop never blocks on
/// a slow client; making it bounded means a client that also stops consuming
/// structured/lifecycle events is disconnected instead of growing daemon
/// memory forever.
pub const RAW_EVENT_CHANNEL_CAPACITY: usize = 1024;

/// Commands buffered on either side of an IPC connection. A full queue
/// back-pressures socket reads and makes synchronous client sends fail loudly;
/// it never allocates an unbounded command backlog behind a wedged peer.
pub const COMMAND_CHANNEL_CAPACITY: usize = 32;

/// Shared overload signal between [`EventSender`] and [`EventForward`].
pub struct EventIngressHealth {
    overloaded: AtomicBool,
    notify: tokio::sync::Notify,
}

impl EventIngressHealth {
    fn new() -> Self {
        Self {
            overloaded: AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
        }
    }

    pub fn is_overloaded(&self) -> bool {
        self.overloaded.load(Ordering::Acquire)
    }

    pub async fn overloaded(&self) {
        if self.is_overloaded() {
            return;
        }
        self.notify.notified().await;
    }

    fn trip(&self) {
        if !self.overloaded.swap(true, Ordering::AcqRel) {
            // One forwarder waits on this signal. `notify_one` stores a
            // permit when it wins the tiny check→wait race; `notify_waiters`
            // would lose that notification and strand an overloaded stream.
            self.notify.notify_one();
        }
    }
}

/// Non-blocking, bounded event admission owned by one daemon connection.
/// A full raw queue trips the connection overload signal; the forwarder then
/// closes the client rather than silently losing a lifecycle event.
#[derive(Clone)]
pub enum EventSender {
    Bounded {
        tx: mpsc::Sender<Event>,
        health: Arc<EventIngressHealth>,
    },
    /// Explicit compatibility path for short-lived internal consumers that
    /// prove their own finite production bound. Network and TUI transports
    /// never construct this variant.
    Unbounded(mpsc::UnboundedSender<Event>),
}

impl EventSender {
    // The error returns the rejected Event so callers can classify/recover it.
    // Boxing would allocate on the overload path and complicate every sender;
    // successful admission remains the overwhelmingly hot path.
    #[allow(clippy::result_large_err)]
    pub fn send(&self, event: Event) -> Result<(), mpsc::error::TrySendError<Event>> {
        match self {
            Self::Bounded { tx, health } => match tx.try_send(event) {
                Ok(()) => Ok(()),
                Err(error @ mpsc::error::TrySendError::Full(_)) => {
                    health.trip();
                    Err(error)
                }
                Err(error @ mpsc::error::TrySendError::Closed(_)) => Err(error),
            },
            Self::Unbounded(tx) => tx
                .send(event)
                .map_err(|error| mpsc::error::TrySendError::Closed(error.0)),
        }
    }

    /// Wrap an existing unbounded sender for a finite, non-transport bridge.
    /// Prefer [`event_forward_channel`] for every long-lived connection.
    pub fn from_unbounded(tx: mpsc::UnboundedSender<Event>) -> Self {
        Self::Unbounded(tx)
    }

    pub async fn closed(&self) {
        match self {
            Self::Bounded { tx, .. } => tx.closed().await,
            Self::Unbounded(tx) => tx.closed().await,
        }
    }

    pub fn is_closed(&self) -> bool {
        match self {
            Self::Bounded { tx, .. } => tx.is_closed(),
            Self::Unbounded(tx) => tx.is_closed(),
        }
    }
}

/// The plumbing a per-connection event forwarder owns: it drains
/// `raw_rx` (everything the serve loop emits for this client) and
/// writes into the bounded `client_tx`, applying drop-and-resync to
/// `TerminalOutput` so the bounded channel can't grow without bound.
/// Constructed by the transports ([`channel::pair`], [`socket`]) and
/// handed to the server's `serve`, which spawns the forwarder with the
/// daemon config it needs to fetch ring replays.
pub struct EventForward {
    /// Bounded staging stream the serve loop writes to (`Connection::tx`).
    pub raw_rx: mpsc::Receiver<Event>,
    /// Bounded sink the client reads from (`Client::rx`, possibly via a
    /// socket). The forwarder's `try_send`/`reserve` against this is
    /// what enforces the memory ceiling.
    pub client_tx: mpsc::Sender<Event>,
    /// Trips when raw admission overflows. The forwarder must disconnect the
    /// client because the rejected event may be non-recoverable lifecycle
    /// state and continuing would manufacture a silently inconsistent view.
    pub health: Arc<EventIngressHealth>,
}

/// Wire one bounded raw event ingress to a client-facing event channel.
pub fn event_forward_channel(client_tx: mpsc::Sender<Event>) -> (EventSender, EventForward) {
    let (tx, raw_rx) = mpsc::channel(RAW_EVENT_CHANNEL_CAPACITY);
    let health = Arc::new(EventIngressHealth::new());
    (
        EventSender::Bounded {
            tx,
            health: health.clone(),
        },
        EventForward {
            raw_rx,
            client_tx,
            health,
        },
    )
}

/// A live connection to the daemon. Owned by the TUI.
///
/// Local (in-process) daemons hand back a `Client` whose `send`/`recv`
/// are tokio channel operations — no serialization at all. Remote
/// daemons hand back a `Client` whose internals read and write frames
/// over a socket. The TUI doesn't see the difference.
pub struct Client {
    tx: ClientCommandSender,
    /// Inbound daemon events. Bounded — see [`EVENT_CHANNEL_CAPACITY`].
    /// Pub so the realm orchestrator can `try_recv` non-blocking from a
    /// sync main loop. (Old async loop uses `Client::recv` instead.)
    pub rx: mpsc::Receiver<Event>,
    /// Live connection state, present only for a self-healing transport
    /// (`socket::connect_reconnecting`). `None` for the in-process
    /// channel and one-shot socket clients, which never drop mid-session
    /// — those always read as [`ConnectionStatus::Connected`].
    connection: Option<watch::Receiver<ConnectionState>>,
}

/// Whether the client currently has a live link to the daemon. A
/// self-healing transport flips to [`Reconnecting`](Self::Reconnecting)
/// while it re-dials so the UI can say so instead of freezing silently;
/// giving up entirely closes the event stream instead (surfaced as the
/// usual disconnect banner), so there is no terminal variant here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionStatus {
    Connected,
    Reconnecting,
}

/// A snapshot of the transport's link to the daemon, published on every
/// (re)connect and drop by the reconnect supervisor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionState {
    pub status: ConnectionStatus,
    /// Build version of the daemon on the *current* connection. Refreshed
    /// on every reconnect so a daemon restarted to a different build is
    /// reflected instead of stuck at the first handshake's value.
    pub daemon_build: String,
}

enum ClientCommandSender {
    Unbounded(mpsc::UnboundedSender<Command>),
    Bounded(mpsc::Sender<Command>),
}

impl Client {
    /// Compatibility constructor for finite test/internal producers that own
    /// an existing unbounded command channel. Real transports use bounded
    /// admission via [`Self::from_bounded_channels`].
    pub fn from_channels(tx: mpsc::UnboundedSender<Command>, rx: mpsc::Receiver<Event>) -> Self {
        Self {
            tx: ClientCommandSender::Unbounded(tx),
            rx,
            connection: None,
        }
    }

    /// Build a client whose synchronous sends use bounded admission. Every
    /// production transport uses this so a peer that stops reading cannot
    /// make the local process retain commands without limit.
    pub fn from_bounded_channels(tx: mpsc::Sender<Command>, rx: mpsc::Receiver<Event>) -> Self {
        Self {
            tx: ClientCommandSender::Bounded(tx),
            rx,
            connection: None,
        }
    }

    /// Attach a reconnect supervisor's connection-state watch (see
    /// [`crate::socket::connect_reconnecting`]).
    pub fn with_connection_status(mut self, connection: watch::Receiver<ConnectionState>) -> Self {
        self.connection = Some(connection);
        self
    }

    /// Current link state. `Connected` for transports without a
    /// reconnect supervisor (in-process channel, one-shot socket).
    pub fn connection_status(&self) -> ConnectionStatus {
        self.connection
            .as_ref()
            .map(|rx| rx.borrow().status)
            .unwrap_or(ConnectionStatus::Connected)
    }

    /// Build version reported by the daemon on the current connection, if
    /// this client tracks connection state. Refreshed across reconnects.
    pub fn daemon_build(&self) -> Option<String> {
        self.connection
            .as_ref()
            .map(|rx| rx.borrow().daemon_build.clone())
    }

    // The Err variant carries a full Command (144 bytes); boxing
    // just to placate clippy would burden every successful send for
    // a path that only fires when the daemon has shut down.
    #[allow(clippy::result_large_err)]
    pub fn send(&self, cmd: Command) -> Result<(), mpsc::error::TrySendError<Command>> {
        match &self.tx {
            ClientCommandSender::Unbounded(tx) => tx
                .send(cmd)
                .map_err(|error| mpsc::error::TrySendError::Closed(error.0)),
            ClientCommandSender::Bounded(tx) => tx.try_send(cmd),
        }
    }

    pub async fn recv(&mut self) -> Option<Event> {
        self.rx.recv().await
    }
}

/// The server-side of a connection. One per connected client.
///
/// A daemon's main loop holds many `Connection`s. Events the daemon
/// wants to send go on `tx` (bounded, non-blocking admission); `rx` receives
/// commands from that specific client. The serve loop never blocks on `tx`.
///
/// When `forward` is `Some`, the raw `tx` stream does not reach the
/// client directly — a forwarder (spawned by the server) drains it into
/// a bounded client channel, applying drop-and-resync to high-volume
/// `TerminalOutput`. Every production transport ([`channel::pair`],
/// [`socket`], and the JSON gateway) sets it. `None` remains only for
/// explicitly finite internal harnesses built with
/// [`Connection::from_channels`].
pub struct Connection {
    pub tx: EventSender,
    pub rx: CommandReceiver,
    forward: Option<EventForward>,
}

pub enum CommandReceiver {
    Unbounded(mpsc::UnboundedReceiver<Command>),
    Bounded(mpsc::Receiver<Command>),
}

impl CommandReceiver {
    pub async fn recv(&mut self) -> Option<Command> {
        match self {
            Self::Unbounded(rx) => rx.recv().await,
            Self::Bounded(rx) => rx.recv().await,
        }
    }

    pub fn try_recv(&mut self) -> Result<Command, mpsc::error::TryRecvError> {
        match self {
            Self::Unbounded(rx) => rx.try_recv(),
            Self::Bounded(rx) => rx.try_recv(),
        }
    }
}

impl From<mpsc::UnboundedReceiver<Command>> for CommandReceiver {
    fn from(rx: mpsc::UnboundedReceiver<Command>) -> Self {
        Self::Unbounded(rx)
    }
}

impl From<mpsc::Receiver<Command>> for CommandReceiver {
    fn from(rx: mpsc::Receiver<Command>) -> Self {
        Self::Bounded(rx)
    }
}

impl Connection {
    /// Build a `Connection` with no forwarder. Reserved for finite internal
    /// dispatch harnesses whose event sender owns its own production bound;
    /// every long-lived transport uses [`Self::with_forward`].
    pub fn from_channels(tx: EventSender, rx: impl Into<CommandReceiver>) -> Self {
        Self {
            tx,
            rx: rx.into(),
            forward: None,
        }
    }

    /// Build a `Connection` whose raw `tx` stream is bridged to the
    /// client through `forward`. The server spawns the forwarder from
    /// `take_forward`.
    pub fn with_forward(
        tx: EventSender,
        rx: impl Into<CommandReceiver>,
        forward: EventForward,
    ) -> Self {
        Self {
            tx,
            rx: rx.into(),
            forward: Some(forward),
        }
    }

    /// Take the forwarder plumbing, if any. The server calls this once
    /// at the start of `serve` and spawns the drop-and-resync task.
    pub fn take_forward(&mut self) -> Option<EventForward> {
        self.forward.take()
    }
}

#[cfg(test)]
mod worktree_recovery_tests {
    use super::*;

    /// Each failure class is recognized from the exact daemon message,
    /// lands on the checklist row that actually aborted, and carries a
    /// hint naming a concrete next step (issue #557).
    #[test]
    fn classifies_every_failure_mode_from_its_daemon_message() {
        // (message-substring the daemon emits, expected class, expected row)
        let cases: &[(&str, WorktreeRecovery, WorktreeStep)] = &[
            (
                "branch 'feat' is already checked out at /tmp/w — refusing to take it \
                 from another live worktree; remove that worktree",
                WorktreeRecovery::BranchHeldLive,
                WorktreeStep::WorktreeAdd,
            ),
            (
                "branch 'feat' is held by the non-live managed worktree at /tmp/w — \
                 automatic reclaim blocked because the checkout contains ignored local files",
                WorktreeRecovery::BranchHeldManaged,
                WorktreeStep::WorktreeAdd,
            ),
            (
                "worktree /tmp/w is checked out on branch 'old', not the requested branch \
                 'feat' — refusing to reuse it",
                WorktreeRecovery::BranchMismatch,
                WorktreeStep::WorktreeAdd,
            ),
            (
                "/tmp/w exists but is not a worktree of /bare and holds uncommitted \
                 work — refusing to reuse or overwrite it; move the directory aside and retry",
                WorktreeRecovery::DirtyLeftover,
                WorktreeStep::WorktreeAdd,
            ),
            (
                "checkout_at: branch 'feat' not found locally or on origin",
                WorktreeRecovery::BranchMissing,
                WorktreeStep::Fetch,
            ),
            (
                "checkout_new_branch_at: base branch 'develop' not found locally or on origin",
                WorktreeRecovery::BranchMissing,
                WorktreeStep::Fetch,
            ),
            (
                "could not download file contents from origin — worktrees from a \
                 blobless clone need the remote reachable",
                WorktreeRecovery::Offline,
                WorktreeStep::WorktreeAdd,
            ),
            (
                "default_branch: could not resolve default branch for acme/widget",
                WorktreeRecovery::DefaultBranchUnresolved,
                WorktreeStep::Fetch,
            ),
            (
                "repo 'not-a-slug' is not owner/name",
                WorktreeRecovery::BadRepo,
                WorktreeStep::Fetch,
            ),
            (
                "init_standalone_at: I/O error: No space left on device (os error 28)",
                WorktreeRecovery::Disk,
                WorktreeStep::WorktreeAdd,
            ),
            (
                "checkout_at: I/O error: Permission denied (os error 13)",
                WorktreeRecovery::Disk,
                WorktreeStep::WorktreeAdd,
            ),
            (
                "fatal: could not read from remote repository",
                WorktreeRecovery::Transient,
                WorktreeStep::Fetch,
            ),
            (
                "something the classifier has never seen",
                WorktreeRecovery::Unknown,
                WorktreeStep::Fetch,
            ),
        ];
        for (msg, class, row) in cases {
            let got = WorktreeRecovery::classify(msg);
            assert_eq!(got, *class, "classify({msg:?})");
            assert_eq!(got.failed_step(), *row, "failed_step for {msg:?}");
            assert!(
                !got.hint().is_empty(),
                "hint for {class:?} must not be empty"
            );
        }
    }

    /// The disk marker must beat the network markers: a message that is
    /// filesystem-class ("No space left") but also mentions origin must
    /// not be mis-sorted as a transient network blip.
    #[test]
    fn specific_markers_beat_generic_ones() {
        assert_eq!(
            WorktreeRecovery::classify(
                "checkout_at: I/O error: No space left on device writing to origin"
            ),
            WorktreeRecovery::Disk,
        );
        // A fork-PR miss mentions "remote" in git's phrasing but is a
        // missing-branch case, not a bare transient.
        assert_eq!(
            WorktreeRecovery::classify(
                "branch 'feat' not found locally or on origin, and its pull-request \
                 head (refs/pull/9/head) could not be fetched from the remote"
            ),
            WorktreeRecovery::BranchMissing,
        );
    }

    /// Manual-fix classes don't advertise a bare retry as sufficient;
    /// the environment/transient classes do.
    #[test]
    fn retryability_splits_manual_from_transient() {
        for c in [
            WorktreeRecovery::BranchHeldLive,
            WorktreeRecovery::BranchHeldManaged,
            WorktreeRecovery::BranchMismatch,
            WorktreeRecovery::DirtyLeftover,
            WorktreeRecovery::BranchMissing,
            WorktreeRecovery::BadRepo,
        ] {
            assert!(!c.retryable(), "{c:?} needs a manual fix first");
        }
        for c in [
            WorktreeRecovery::Transient,
            WorktreeRecovery::Offline,
            WorktreeRecovery::DefaultBranchUnresolved,
            WorktreeRecovery::Disk,
            WorktreeRecovery::Unknown,
        ] {
            assert!(c.retryable(), "{c:?} is retryable on its own");
        }
        assert!(
            WorktreeRecovery::BranchHeldLive
                .hint()
                .contains("join the live session")
        );
        assert!(
            !WorktreeRecovery::BranchHeldLive.hint().contains("x a"),
            "session adoption cannot release a branch checkout"
        );
    }
}

#[cfg(test)]
mod transport_admission_tests {
    use super::*;
    use std::time::Duration;

    fn notice(index: usize) -> Event {
        Event::Notification {
            title: "bounded".into(),
            body: index.to_string(),
        }
    }

    #[tokio::test]
    async fn raw_event_ingress_has_a_hard_cap_and_trips_health() {
        let (client_tx, _client_rx) = mpsc::channel(1);
        let (sender, forward) = event_forward_channel(client_tx);
        for index in 0..RAW_EVENT_CHANNEL_CAPACITY {
            sender.send(notice(index)).expect("within raw capacity");
        }
        assert!(matches!(
            sender.send(notice(RAW_EVENT_CHANNEL_CAPACITY)),
            Err(mpsc::error::TrySendError::Full(_))
        ));
        assert!(forward.health.is_overloaded());
        tokio::time::timeout(Duration::from_millis(10), forward.health.overloaded())
            .await
            .expect("overload signal remains observable");

        drop(forward);
        tokio::time::timeout(Duration::from_millis(10), sender.closed())
            .await
            .expect("sender observes dropped ingress");
    }

    #[test]
    fn bounded_client_commands_fail_loudly_at_capacity() {
        let (command_tx, _command_rx) = mpsc::channel(1);
        let (_event_tx, event_rx) = mpsc::channel(1);
        let client = Client::from_bounded_channels(command_tx, event_rx);

        client.send(Command::Subscribe).expect("first command");
        assert!(matches!(
            client.send(Command::Refresh),
            Err(mpsc::error::TrySendError::Full(Command::Refresh))
        ));
    }

    #[tokio::test]
    async fn command_receiver_preserves_both_transport_shapes() {
        let (unbounded_tx, unbounded_rx) = mpsc::unbounded_channel();
        let mut unbounded = CommandReceiver::from(unbounded_rx);
        unbounded_tx.send(Command::Subscribe).expect("open");
        assert!(matches!(unbounded.recv().await, Some(Command::Subscribe)));

        let (bounded_tx, bounded_rx) = mpsc::channel(1);
        let mut bounded = CommandReceiver::from(bounded_rx);
        bounded_tx.try_send(Command::Refresh).expect("capacity");
        assert!(matches!(bounded.try_recv(), Ok(Command::Refresh)));
    }

    #[tokio::test]
    async fn explicit_unbounded_event_sender_reports_receiver_close() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sender = EventSender::from_unbounded(tx);
        sender.send(notice(1)).expect("open");
        assert!(matches!(rx.recv().await, Some(Event::Notification { .. })));
        drop(rx);
        assert!(matches!(
            sender.send(notice(2)),
            Err(mpsc::error::TrySendError::Closed(_))
        ));
    }

    /// A small write must pass through `write_chunked` as a single
    /// untouched command; an over-cap payload must split into in-order
    /// chunks, each under [`MAX_WRITE_CHUNK_BYTES`] (and therefore
    /// framing under [`MAX_COMMAND_FRAME_BYTES`]), concatenating
    /// byte-identically to the original.
    #[test]
    fn write_chunked_splits_at_the_cap_and_preserves_bytes() {
        // Below the cap: exactly one command, identical bytes.
        let small = vec![7u8; 64];
        match Command::write_chunked(TerminalId(1), small.clone(), TerminalInputIntent::Compose)
            .as_slice()
        {
            [
                Command::Write {
                    terminal_id,
                    bytes,
                    intent,
                },
            ] => {
                assert_eq!(*terminal_id, TerminalId(1));
                assert_eq!(*bytes, small);
                assert_eq!(*intent, TerminalInputIntent::Compose);
            }
            other => panic!("small write must stay a single command: {other:?}"),
        }

        // Above the cap: split, ordered, byte-identical. Non-uniform
        // pattern so a dropped/reordered chunk cannot go unnoticed.
        let original: Vec<u8> = (0..(MAX_WRITE_CHUNK_BYTES * 2 + 999))
            .map(|i| (i % 251) as u8)
            .collect();
        let chunks =
            Command::write_chunked(TerminalId(9), original.clone(), TerminalInputIntent::Submit);
        assert_eq!(chunks.len(), 3, "2·cap + tail = three chunks");
        let mut reassembled = Vec::new();
        for chunk in &chunks {
            match chunk {
                Command::Write {
                    terminal_id,
                    bytes,
                    intent,
                } => {
                    assert_eq!(*terminal_id, TerminalId(9));
                    assert!(bytes.len() <= MAX_WRITE_CHUNK_BYTES);
                    let expected = if reassembled.len() + bytes.len() == original.len() {
                        TerminalInputIntent::Submit
                    } else {
                        TerminalInputIntent::Compose
                    };
                    assert_eq!(*intent, expected);
                    reassembled.extend_from_slice(bytes);
                }
                other => panic!("chunking must only ever emit Write: {other:?}"),
            }
        }
        assert_eq!(reassembled, original);

        // Wire normalization: a non-Write passes through untouched.
        assert!(matches!(
            Command::Subscribe.into_write_chunks().as_slice(),
            [Command::Subscribe]
        ));
    }

    /// `from_wire` round-trips every `as_str`, unknown/empty wire values
    /// default to the actionable `Permanent`, and `is_actionable` marks
    /// exactly the self-healing `Retryable` transient as quiet (#730).
    #[test]
    fn provider_error_kind_wire_roundtrip_and_actionability() {
        for kind in [
            ProviderErrorKind::Retryable,
            ProviderErrorKind::Exhausted,
            ProviderErrorKind::Auth,
            ProviderErrorKind::Permanent,
        ] {
            assert_eq!(ProviderErrorKind::from_wire(kind.as_str()), kind);
        }
        // Legacy/uncategorized and unrecognized values are surfaced, not
        // swallowed.
        assert_eq!(
            ProviderErrorKind::from_wire(""),
            ProviderErrorKind::Permanent
        );
        assert_eq!(
            ProviderErrorKind::from_wire("garbage"),
            ProviderErrorKind::Permanent
        );

        assert!(!ProviderErrorKind::Retryable.is_actionable());
        assert!(ProviderErrorKind::Exhausted.is_actionable());
        assert!(ProviderErrorKind::Auth.is_actionable());
        assert!(ProviderErrorKind::Permanent.is_actionable());
        // An unknown kind is treated as actionable (via the Permanent
        // fallback) so a producer/consumer version skew never hides a
        // failure behind the quiet path.
        assert!(ProviderErrorKind::from_wire("future-kind").is_actionable());
    }

    /// The const decimal parser must agree with std's on the actual
    /// build-script output — a silent mis-parse would put a wrong
    /// fingerprint in every preamble.
    #[test]
    fn fingerprint_const_parser_matches_std() {
        let emitted = env!("LAZYBOX_PROTOCOL_FINGERPRINT");
        assert_eq!(
            PROTOCOL_FINGERPRINT,
            emitted
                .parse::<u32>()
                .expect("build.rs emits a decimal u32"),
        );
    }
}
